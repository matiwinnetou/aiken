use std::{
    collections::{HashMap, HashSet},
    ops::Deref,
    sync::Arc,
    vec,
};

use indexmap::IndexMap;
use itertools::Itertools;
use uplc::{
    ast::{
        builder::{
            self, apply_wrap, choose_list, constr_index_exposer, delayed_choose_list,
            delayed_if_else, if_else, repeat_tail_list, CONSTR_FIELDS_EXPOSER, CONSTR_GET_FIELD,
        },
        Constant as UplcConstant, Name, NamedDeBruijn, Program, Term, Type as UplcType,
    },
    builtins::DefaultFunction,
    machine::cost_model::ExBudget,
    parser::interner::Interner,
};

use crate::{
    air::Air,
    ast::{
        ArgName, AssignmentKind, BinOp, Clause, Pattern, Span, TypedArg, TypedDataType,
        TypedFunction, UnOp,
    },
    builder::{
        check_when_pattern_needs, constants_ir, convert_constants_to_data, convert_data_to_type,
        convert_type_to_data, get_common_ancestor, get_generics_and_type, handle_func_deps_ir,
        handle_recursion_ir, list_access_to_uplc, monomorphize, rearrange_clauses,
        wrap_validator_args, ClauseProperties, DataTypeKey, FuncComponents, FunctionAccessKey,
    },
    expr::TypedExpr,
    tipo::{
        self, ModuleValueConstructor, PatternConstructor, Type, TypeInfo, ValueConstructor,
        ValueConstructorVariant,
    },
    IdGenerator,
};

pub struct CodeGenerator<'a> {
    defined_functions: HashMap<FunctionAccessKey, ()>,
    functions: &'a HashMap<FunctionAccessKey, &'a TypedFunction>,
    // type_aliases: &'a HashMap<(String, String), &'a TypeAlias<Arc<tipo::Type>>>,
    data_types: &'a HashMap<DataTypeKey, &'a TypedDataType>,
    module_types: &'a HashMap<String, TypeInfo>,
    id_gen: IdGenerator,
    needs_field_access: bool,
    zero_arg_functions: HashMap<FunctionAccessKey, Vec<Air>>,
}

impl<'a> CodeGenerator<'a> {
    pub fn new(
        functions: &'a HashMap<FunctionAccessKey, &'a TypedFunction>,
        // type_aliases: &'a HashMap<(String, String), &'a TypeAlias<Arc<tipo::Type>>>,
        data_types: &'a HashMap<DataTypeKey, &'a TypedDataType>,
        module_types: &'a HashMap<String, TypeInfo>,
    ) -> Self {
        CodeGenerator {
            defined_functions: HashMap::new(),
            functions,
            // type_aliases,
            data_types,
            module_types,
            id_gen: IdGenerator::new(),
            needs_field_access: false,
            zero_arg_functions: HashMap::new(),
        }
    }

    pub fn generate(
        &mut self,
        body: TypedExpr,
        arguments: Vec<TypedArg>,
        wrap_as_validator: bool,
    ) -> Program<Name> {
        let mut ir_stack = vec![];
        let scope = vec![self.id_gen.next()];

        self.build_ir(&body, &mut ir_stack, scope);

        self.define_ir(&mut ir_stack);

        let mut term = self.uplc_code_gen(&mut ir_stack);

        if self.needs_field_access {
            term = builder::constr_get_field(term);

            term = builder::constr_fields_exposer(term);
        }

        // Wrap the validator body if ifThenElse term unit error
        term = if wrap_as_validator {
            builder::final_wrapper(term)
        } else {
            term
        };

        term = wrap_validator_args(term, arguments);

        let mut program = Program {
            version: (1, 0, 0),
            term,
        };

        let mut interner = Interner::new();

        interner.program(&mut program);

        program
    }

    pub(crate) fn build_ir(&mut self, body: &TypedExpr, ir_stack: &mut Vec<Air>, scope: Vec<u64>) {
        match body {
            TypedExpr::Int { value, .. } => ir_stack.push(Air::Int {
                scope,
                value: value.to_string(),
            }),
            TypedExpr::String { value, .. } => ir_stack.push(Air::String {
                scope,
                value: value.to_string(),
            }),
            TypedExpr::ByteArray { bytes, .. } => ir_stack.push(Air::ByteArray {
                scope,
                bytes: bytes.to_vec(),
            }),
            TypedExpr::Pipeline { expressions, .. } | TypedExpr::Sequence { expressions, .. } => {
                for (index, expr) in expressions.iter().enumerate() {
                    if index == 0 {
                        self.build_ir(expr, ir_stack, scope.clone());
                    } else {
                        let mut branch_scope = scope.clone();
                        branch_scope.push(self.id_gen.next());
                        self.build_ir(expr, ir_stack, branch_scope);
                    }
                }
            }
            TypedExpr::Var {
                constructor, name, ..
            } => match &constructor.variant {
                ValueConstructorVariant::ModuleConstant { literal, .. } => {
                    constants_ir(literal, ir_stack, scope);
                }
                ValueConstructorVariant::ModuleFn {
                    builtin: Some(builtin),
                    ..
                } => {
                    ir_stack.push(Air::Builtin {
                        scope,
                        func: *builtin,
                        tipo: constructor.tipo.clone(),
                    });
                }
                _ => {
                    ir_stack.push(Air::Var {
                        scope,
                        constructor: constructor.clone(),
                        name: name.clone(),
                        variant_name: String::new(),
                    });
                }
            },
            TypedExpr::Fn { args, body, .. } => {
                let mut func_body = vec![];
                let mut func_scope = scope.clone();
                func_scope.push(self.id_gen.next());
                self.build_ir(body, &mut func_body, func_scope);
                let mut arg_names = vec![];
                for arg in args {
                    let name = arg.arg_name.get_variable_name().unwrap_or("_").to_string();

                    arg_names.push(name);
                }

                ir_stack.push(Air::Fn {
                    scope,
                    params: arg_names,
                });

                ir_stack.append(&mut func_body);
            }
            TypedExpr::List {
                elements,
                tail,
                tipo,
                ..
            } => {
                ir_stack.push(Air::List {
                    scope: scope.clone(),
                    count: elements.len(),
                    tipo: tipo.clone(),
                    tail: tail.is_some(),
                });

                for element in elements {
                    let mut scope = scope.clone();
                    scope.push(self.id_gen.next());
                    self.build_ir(element, ir_stack, scope.clone())
                }

                if let Some(tail) = tail {
                    let mut scope = scope;
                    scope.push(self.id_gen.next());

                    self.build_ir(tail, ir_stack, scope);
                }
            }
            TypedExpr::Call { fun, args, .. } => {
                ir_stack.push(Air::Call {
                    scope: scope.clone(),
                    count: args.len(),
                });
                let mut scope_fun = scope.clone();
                scope_fun.push(self.id_gen.next());
                self.build_ir(fun, ir_stack, scope_fun);

                for arg in args {
                    let mut scope = scope.clone();
                    scope.push(self.id_gen.next());
                    self.build_ir(&arg.value, ir_stack, scope);
                }
            }
            TypedExpr::BinOp {
                name, left, right, ..
            } => {
                ir_stack.push(Air::BinOp {
                    scope: scope.clone(),
                    name: *name,
                    count: 2,
                    tipo: left.tipo(),
                });
                let mut scope_left = scope.clone();
                scope_left.push(self.id_gen.next());

                let mut scope_right = scope;
                scope_right.push(self.id_gen.next());

                self.build_ir(left, ir_stack, scope_left);
                self.build_ir(right, ir_stack, scope_right);
            }
            TypedExpr::Assignment {
                value,
                pattern,
                kind,
                tipo,
                ..
            } => {
                let mut define_vec: Vec<Air> = vec![];
                let mut value_vec: Vec<Air> = vec![];
                let mut pattern_vec: Vec<Air> = vec![];

                let mut value_scope = scope.clone();
                value_scope.push(self.id_gen.next());

                self.build_ir(value, &mut value_vec, value_scope);

                self.assignment_ir(
                    pattern,
                    &mut pattern_vec,
                    &mut value_vec,
                    tipo,
                    *kind,
                    scope,
                );

                ir_stack.append(&mut define_vec);
                ir_stack.append(&mut pattern_vec);
            }
            TypedExpr::When {
                subjects, clauses, ..
            } => {
                let subject_name = format!("__subject_name_{}", self.id_gen.next());
                let constr_var = format!("__constr_name_{}", self.id_gen.next());

                // assuming one subject at the moment
                let subject = subjects[0].clone();

                let clauses = if matches!(clauses[0].pattern[0], Pattern::List { .. }) {
                    rearrange_clauses(clauses.clone())
                } else {
                    clauses.clone()
                };

                if let Some((last_clause, clauses)) = clauses.split_last() {
                    let mut pattern_vec = vec![];

                    let mut clause_properties = ClauseProperties::init(
                        &subject.tipo(),
                        constr_var.clone(),
                        subject_name.clone(),
                    );

                    self.handle_each_clause(
                        &mut pattern_vec,
                        &mut clause_properties,
                        clauses,
                        &subject.tipo(),
                        scope.clone(),
                    );

                    let last_pattern = &last_clause.pattern[0];

                    let mut final_scope = scope.clone();

                    final_scope.push(self.id_gen.next());

                    pattern_vec.push(Air::Finally {
                        scope: final_scope.clone(),
                    });

                    let mut final_clause_vec = vec![];

                    self.build_ir(
                        &last_clause.then,
                        &mut final_clause_vec,
                        final_scope.clone(),
                    );

                    self.when_ir(
                        last_pattern,
                        &mut pattern_vec,
                        &mut final_clause_vec,
                        &subject.tipo(),
                        &mut clause_properties,
                        final_scope,
                    );

                    if *clause_properties.needs_constr_var() {
                        ir_stack.push(Air::Lam {
                            scope: scope.clone(),
                            name: constr_var.clone(),
                        });

                        self.build_ir(&subject, ir_stack, scope.clone());

                        ir_stack.push(Air::When {
                            scope: scope.clone(),
                            subject_name,
                            tipo: subject.tipo(),
                        });

                        let mut scope = scope;
                        scope.push(self.id_gen.next());

                        ir_stack.push(Air::Var {
                            scope,
                            constructor: ValueConstructor::public(
                                subject.tipo(),
                                ValueConstructorVariant::LocalVariable {
                                    location: Span::empty(),
                                },
                            ),
                            name: constr_var,
                            variant_name: String::new(),
                        })
                    } else {
                        ir_stack.push(Air::When {
                            scope: scope.clone(),
                            subject_name,
                            tipo: subject.tipo(),
                        });

                        let mut scope = scope;
                        scope.push(self.id_gen.next());

                        self.build_ir(&subject, ir_stack, scope);
                    }

                    ir_stack.append(&mut pattern_vec);
                };
            }
            TypedExpr::If {
                branches,
                final_else,
                ..
            } => {
                let mut if_ir = vec![];

                for (index, branch) in branches.iter().enumerate() {
                    let mut branch_scope = scope.clone();
                    branch_scope.push(self.id_gen.next());

                    if index == 0 {
                        if_ir.push(Air::If {
                            scope: scope.clone(),
                        });
                    } else {
                        if_ir.push(Air::If {
                            scope: branch_scope.clone(),
                        });
                    }
                    self.build_ir(&branch.condition, &mut if_ir, branch_scope.clone());
                    self.build_ir(&branch.body, &mut if_ir, branch_scope);
                }

                let mut branch_scope = scope;
                branch_scope.push(self.id_gen.next());

                self.build_ir(final_else, &mut if_ir, branch_scope);

                ir_stack.append(&mut if_ir);
            }
            TypedExpr::RecordAccess {
                record,
                index,
                tipo,
                ..
            } => {
                self.needs_field_access = true;

                ir_stack.push(Air::RecordAccess {
                    scope: scope.clone(),
                    index: *index,
                    tipo: tipo.clone(),
                });

                self.build_ir(record, ir_stack, scope);
            }
            TypedExpr::ModuleSelect {
                constructor,
                module_name,
                tipo,
                ..
            } => match constructor {
                ModuleValueConstructor::Record { .. } => todo!(),
                ModuleValueConstructor::Fn { name, module, .. } => {
                    let func = self.functions.get(&FunctionAccessKey {
                        module_name: module_name.clone(),
                        function_name: name.clone(),
                        variant_name: String::new(),
                    });

                    if let Some(func) = func {
                        ir_stack.push(Air::Var {
                            scope,
                            constructor: ValueConstructor::public(
                                tipo.clone(),
                                ValueConstructorVariant::ModuleFn {
                                    name: name.clone(),
                                    field_map: None,
                                    module: module.clone(),
                                    arity: func.arguments.len(),
                                    location: Span::empty(),
                                    builtin: None,
                                },
                            ),
                            name: format!("{module}_{name}"),
                            variant_name: String::new(),
                        });
                    } else {
                        let type_info = self.module_types.get(module_name).unwrap();
                        let value = type_info.values.get(name).unwrap();
                        match &value.variant {
                            ValueConstructorVariant::ModuleFn { builtin, .. } => {
                                let builtin = builtin.unwrap();

                                ir_stack.push(Air::Builtin {
                                    func: builtin,
                                    scope,
                                    tipo: tipo.clone(),
                                });
                            }
                            _ => unreachable!(),
                        }
                    }
                }
                ModuleValueConstructor::Constant { literal, .. } => {
                    constants_ir(literal, ir_stack, scope);
                }
            },
            TypedExpr::Todo { label, tipo, .. } => {
                ir_stack.push(Air::Todo {
                    scope,
                    label: label.clone(),
                    tipo: tipo.clone(),
                });
            }
            TypedExpr::RecordUpdate { .. } => todo!(),
            TypedExpr::UnOp { value, op, .. } => {
                ir_stack.push(Air::UnOp {
                    scope: scope.clone(),
                    op: *op,
                });

                self.build_ir(value, ir_stack, scope);
            }
            TypedExpr::Tuple { elems, tipo, .. } => {
                ir_stack.push(Air::Tuple {
                    scope: scope.clone(),
                    tipo: tipo.clone(),
                    count: elems.len(),
                });

                let mut elems_air = vec![];

                for elem in elems {
                    let mut scope = scope.clone();
                    scope.push(self.id_gen.next());
                    self.build_ir(elem, &mut elems_air, scope);
                }

                ir_stack.append(&mut elems_air);
            }
            TypedExpr::Trace {
                tipo, then, text, ..
            } => {
                let mut scope = scope;

                ir_stack.push(Air::Trace {
                    text: text.clone(),
                    tipo: tipo.clone(),
                    scope: scope.clone(),
                });

                scope.push(self.id_gen.next());

                self.build_ir(then, ir_stack, scope);
            }

            TypedExpr::TupleIndex { index, tuple, .. } => {
                ir_stack.push(Air::TupleIndex {
                    scope: scope.clone(),
                    tipo: tuple.tipo(),
                    index: *index,
                });

                self.build_ir(tuple, ir_stack, scope);
            }

            TypedExpr::ErrorTerm { tipo, label, .. } => {
                ir_stack.push(Air::ErrorTerm {
                    scope,
                    tipo: tipo.clone(),
                    label: label.clone(),
                });
            }
        }
    }

    fn handle_each_clause(
        &mut self,
        ir_stack: &mut Vec<Air>,
        clause_properties: &mut ClauseProperties,
        clauses: &[Clause<TypedExpr, PatternConstructor, Arc<Type>, String>],
        subject_type: &Arc<Type>,
        scope: Vec<u64>,
    ) {
        for (index, clause) in clauses.iter().enumerate() {
            // scope per clause is different
            let mut scope = scope.clone();
            scope.push(self.id_gen.next());

            // holds when clause pattern Air
            let mut clause_subject_vec = vec![];
            let mut clause_then_vec = vec![];

            // reset complex clause setting per clause back to default
            *clause_properties.is_complex_clause() = false;

            self.build_ir(&clause.then, &mut clause_then_vec, scope.clone());

            match clause_properties {
                ClauseProperties::ConstrClause {
                    original_subject_name,
                    ..
                } => {
                    let subject_name = original_subject_name.clone();
                    self.when_ir(
                        &clause.pattern[0],
                        &mut clause_subject_vec,
                        &mut clause_then_vec,
                        subject_type,
                        clause_properties,
                        scope.clone(),
                    );

                    ir_stack.push(Air::Clause {
                        scope,
                        tipo: subject_type.clone(),
                        complex_clause: *clause_properties.is_complex_clause(),
                        subject_name,
                    });
                }
                ClauseProperties::ListClause {
                    original_subject_name,
                    current_index,
                    ..
                } => {
                    let current_clause_index = *current_index;

                    let subject_name = if current_clause_index == 0 {
                        original_subject_name.clone()
                    } else {
                        format!("__tail_{}", current_clause_index - 1)
                    };

                    self.when_ir(
                        &clause.pattern[0],
                        &mut clause_subject_vec,
                        &mut clause_then_vec,
                        subject_type,
                        clause_properties,
                        scope.clone(),
                    );

                    let next_tail = if index == clauses.len() - 1 {
                        None
                    } else {
                        Some(format!("__tail_{}", current_clause_index))
                    };

                    ir_stack.push(Air::ListClause {
                        scope,
                        tipo: subject_type.clone(),
                        tail_name: subject_name,
                        next_tail_name: next_tail,
                        complex_clause: *clause_properties.is_complex_clause(),
                        inverse: false,
                    });

                    match clause_properties {
                        ClauseProperties::ListClause { current_index, .. } => {
                            *current_index += 1;
                        }
                        _ => unreachable!(),
                    }
                }
                ClauseProperties::TupleClause {
                    original_subject_name,
                    defined_tuple_indices,
                    ..
                } => {
                    let prev_defined_tuple_indices = defined_tuple_indices.clone();
                    let subject_name = original_subject_name.clone();

                    self.when_ir(
                        &clause.pattern[0],
                        &mut clause_subject_vec,
                        &mut clause_then_vec,
                        subject_type,
                        clause_properties,
                        scope.clone(),
                    );
                    let current_defined_tuple_indices = match clause_properties {
                        ClauseProperties::TupleClause {
                            defined_tuple_indices,
                            ..
                        } => defined_tuple_indices.clone(),
                        _ => unreachable!(),
                    };

                    let indices_to_define = current_defined_tuple_indices
                        .difference(&prev_defined_tuple_indices)
                        .cloned()
                        .collect();

                    ir_stack.push(Air::TupleClause {
                        scope,
                        tipo: subject_type.clone(),
                        indices: indices_to_define,
                        predefined_indices: prev_defined_tuple_indices,
                        subject_name,
                        count: subject_type.get_inner_types().len(),
                        complex_clause: *clause_properties.is_complex_clause(),
                    });
                }
            }

            ir_stack.append(&mut clause_subject_vec);
        }
    }

    fn when_ir(
        &mut self,
        pattern: &Pattern<tipo::PatternConstructor, Arc<tipo::Type>>,
        pattern_vec: &mut Vec<Air>,
        values: &mut Vec<Air>,
        tipo: &Type,
        clause_properties: &mut ClauseProperties,
        scope: Vec<u64>,
    ) {
        match pattern {
            Pattern::Int { value, .. } => {
                pattern_vec.push(Air::Int {
                    scope,
                    value: value.clone(),
                });

                pattern_vec.append(values);
            }
            Pattern::String { .. } => todo!(),
            Pattern::Var { name, .. } => {
                pattern_vec.push(Air::Discard {
                    scope: scope.clone(),
                });
                pattern_vec.push(Air::Lam {
                    scope: scope.clone(),
                    name: name.clone(),
                });

                pattern_vec.push(Air::Var {
                    scope,
                    constructor: ValueConstructor::public(
                        tipo.clone().into(),
                        ValueConstructorVariant::LocalVariable {
                            location: Span::empty(),
                        },
                    ),
                    name: clause_properties.original_subject_name().clone(),
                    variant_name: String::new(),
                });
                pattern_vec.append(values);
            }
            Pattern::VarUsage { .. } => todo!(),
            Pattern::Assign { name, pattern, .. } => {
                let mut new_vec = vec![];
                new_vec.push(Air::Lam {
                    scope: scope.clone(),
                    name: name.clone(),
                });
                new_vec.push(Air::Var {
                    scope: scope.clone(),
                    constructor: ValueConstructor::public(
                        tipo.clone().into(),
                        ValueConstructorVariant::LocalVariable {
                            location: Span::empty(),
                        },
                    ),
                    name: clause_properties.original_subject_name().clone(),
                    variant_name: String::new(),
                });

                new_vec.append(values);

                // pattern_vec.push(value)
                self.when_ir(
                    pattern,
                    pattern_vec,
                    &mut new_vec,
                    tipo,
                    clause_properties,
                    scope,
                );
            }
            Pattern::Discard { .. } => {
                pattern_vec.push(Air::Discard { scope });
                pattern_vec.append(values);
            }
            Pattern::List { elements, tail, .. } => {
                for element in elements {
                    check_when_pattern_needs(element, clause_properties);
                }

                if let Some(tail) = tail {
                    check_when_pattern_needs(tail, clause_properties);
                }
                *clause_properties.needs_constr_var() = false;

                pattern_vec.push(Air::Discard {
                    scope: scope.clone(),
                });

                self.when_recursive_ir(
                    pattern,
                    pattern_vec,
                    values,
                    clause_properties,
                    tipo,
                    scope,
                );
            }
            Pattern::Constructor {
                arguments,
                name: constr_name,
                ..
            } => {
                let mut temp_clause_properties = clause_properties.clone();
                *temp_clause_properties.needs_constr_var() = false;

                for arg in arguments {
                    check_when_pattern_needs(&arg.value, &mut temp_clause_properties);
                }

                // find data type definition
                let data_type_key = match tipo {
                    Type::Fn { ret, .. } => match ret.as_ref() {
                        Type::App { module, name, .. } => DataTypeKey {
                            module_name: module.clone(),
                            defined_type: name.clone(),
                        },
                        _ => unreachable!(),
                    },
                    Type::App { module, name, .. } => DataTypeKey {
                        module_name: module.clone(),
                        defined_type: name.clone(),
                    },
                    _ => unreachable!(),
                };

                let data_type = self.data_types.get(&data_type_key).unwrap();

                let (index, _) = data_type
                    .constructors
                    .iter()
                    .enumerate()
                    .find(|(_, dt)| &dt.name == constr_name)
                    .unwrap();

                let mut new_vec = vec![Air::Var {
                    constructor: ValueConstructor::public(
                        tipo.clone().into(),
                        ValueConstructorVariant::LocalVariable {
                            location: Span::empty(),
                        },
                    ),
                    name: temp_clause_properties.clause_var_name().clone(),
                    scope: scope.clone(),
                    variant_name: String::new(),
                }];

                // if only one constructor, no need to check
                if data_type.constructors.len() > 1 {
                    // push constructor Index
                    pattern_vec.push(Air::Int {
                        value: index.to_string(),
                        scope: scope.clone(),
                    });
                }

                if *temp_clause_properties.needs_constr_var() {
                    self.when_recursive_ir(
                        pattern,
                        pattern_vec,
                        &mut new_vec,
                        clause_properties,
                        tipo,
                        scope,
                    );
                } else {
                    self.when_recursive_ir(
                        pattern,
                        pattern_vec,
                        &mut vec![],
                        clause_properties,
                        tipo,
                        scope,
                    );
                }

                pattern_vec.append(values);

                // unify clause properties
                *clause_properties.is_complex_clause() = *clause_properties.is_complex_clause()
                    || *temp_clause_properties.is_complex_clause();

                *clause_properties.needs_constr_var() = *clause_properties.needs_constr_var()
                    || *temp_clause_properties.needs_constr_var();
            }
            Pattern::Tuple { elems, .. } => {
                for elem in elems {
                    check_when_pattern_needs(elem, clause_properties);
                }
                *clause_properties.needs_constr_var() = false;

                self.when_recursive_ir(
                    pattern,
                    pattern_vec,
                    &mut vec![],
                    clause_properties,
                    tipo,
                    scope,
                );

                pattern_vec.append(values);
            }
        }
    }

    fn when_recursive_ir(
        &mut self,
        pattern: &Pattern<tipo::PatternConstructor, Arc<tipo::Type>>,
        pattern_vec: &mut Vec<Air>,
        values: &mut Vec<Air>,
        clause_properties: &mut ClauseProperties,
        tipo: &Type,
        scope: Vec<u64>,
    ) {
        match pattern {
            Pattern::Int { .. } => todo!(),
            Pattern::String { .. } => todo!(),
            Pattern::Var { .. } => todo!(),
            Pattern::VarUsage { .. } => todo!(),
            Pattern::Assign { .. } => todo!(),
            Pattern::Discard { .. } => {
                pattern_vec.push(Air::Discard { scope });

                pattern_vec.append(values);
            }
            Pattern::List { elements, tail, .. } => {
                let mut names = vec![];
                let mut nested_pattern = vec![];
                let items_type = &tipo.get_inner_types()[0];
                // let mut nested_pattern = vec![];
                for element in elements {
                    let name = self.nested_pattern_ir_and_label(
                        element,
                        &mut nested_pattern,
                        items_type,
                        scope.clone(),
                    );

                    names.push(name.unwrap_or_else(|| "_".to_string()))
                }

                let mut tail_name = String::new();

                if let Some(tail) = tail {
                    match &**tail {
                        Pattern::Var { name, .. } => {
                            tail_name = name.clone();
                        }
                        Pattern::Discard { .. } => {}
                        _ => todo!(),
                    }
                }

                let tail_head_names = names
                    .iter()
                    .enumerate()
                    .filter(|(_, name)| *name != &"_".to_string())
                    .map(|(index, name)| {
                        if index == 0 {
                            (
                                clause_properties.original_subject_name().clone(),
                                name.clone(),
                            )
                        } else {
                            (format!("__tail_{}", index - 1), name.clone())
                        }
                    })
                    .collect_vec();

                if tail.is_some() && !elements.is_empty() {
                    let tail_var = if elements.len() == 1 {
                        clause_properties.original_subject_name().clone()
                    } else {
                        format!("__tail_{}", elements.len() - 2)
                    };

                    pattern_vec.push(Air::ListExpose {
                        scope,
                        tipo: tipo.clone().into(),
                        tail_head_names,
                        tail: Some((tail_var, tail_name)),
                    });
                } else {
                    pattern_vec.push(Air::ListExpose {
                        scope,
                        tipo: tipo.clone().into(),
                        tail_head_names,
                        tail: None,
                    });
                }

                pattern_vec.append(&mut nested_pattern);
                pattern_vec.append(values);
            }
            Pattern::Constructor {
                is_record,
                name: constr_name,
                arguments,
                constructor,
                tipo,
                ..
            } => {
                let data_type_key = match tipo.as_ref() {
                    Type::Fn { ret, .. } => match &**ret {
                        Type::App { module, name, .. } => DataTypeKey {
                            module_name: module.clone(),
                            defined_type: name.clone(),
                        },
                        _ => unreachable!(),
                    },
                    Type::App { module, name, .. } => DataTypeKey {
                        module_name: module.clone(),
                        defined_type: name.clone(),
                    },
                    _ => unreachable!(),
                };

                let data_type = self.data_types.get(&data_type_key).unwrap();
                let (_, constructor_type) = data_type
                    .constructors
                    .iter()
                    .enumerate()
                    .find(|(_, dt)| &dt.name == constr_name)
                    .unwrap();
                let mut nested_pattern = vec![];
                if *is_record {
                    let field_map = match constructor {
                        tipo::PatternConstructor::Record { field_map, .. } => {
                            field_map.clone().unwrap()
                        }
                    };

                    let mut type_map: HashMap<String, Arc<Type>> = HashMap::new();

                    for (index, arg) in tipo.arg_types().unwrap().iter().enumerate() {
                        let label = constructor_type.arguments[index].label.clone().unwrap();
                        let field_type = arg.clone();

                        type_map.insert(label, field_type);
                    }

                    let arguments_index = arguments
                        .iter()
                        .filter_map(|item| {
                            let label = item.label.clone().unwrap_or_default();
                            let field_index = field_map
                                .fields
                                .get(&label)
                                .map(|(index, _)| index)
                                .unwrap_or(&0);
                            let var_name = self.nested_pattern_ir_and_label(
                                &item.value,
                                &mut nested_pattern,
                                type_map.get(&label).unwrap_or(
                                    &Type::App {
                                        public: true,
                                        module: "".to_string(),
                                        name: "Discard".to_string(),
                                        args: vec![],
                                    }
                                    .into(),
                                ),
                                scope.clone(),
                            );

                            var_name.map(|var_name| (label, var_name, *field_index))
                        })
                        .sorted_by(|item1, item2| item1.2.cmp(&item2.2))
                        .collect::<Vec<(String, String, usize)>>();

                    if !arguments_index.is_empty() {
                        pattern_vec.push(Air::FieldsExpose {
                            count: arguments_index.len() + 2,
                            indices: arguments_index
                                .iter()
                                .map(|(label, var_name, index)| {
                                    let field_type = type_map.get(label).unwrap();
                                    (*index, var_name.clone(), field_type.clone())
                                })
                                .collect_vec(),
                            scope,
                        });
                    }
                } else {
                    let mut type_map: HashMap<usize, Arc<Type>> = HashMap::new();

                    for (index, arg) in tipo.arg_types().unwrap().iter().enumerate() {
                        let field_type = arg.clone();

                        type_map.insert(index, field_type);
                    }

                    let arguments_index = arguments
                        .iter()
                        .enumerate()
                        .filter_map(|(index, item)| {
                            let var_name = self.nested_pattern_ir_and_label(
                                &item.value,
                                &mut nested_pattern,
                                type_map.get(&index).unwrap(),
                                scope.clone(),
                            );

                            var_name.map(|var_name| (var_name, index))
                        })
                        .collect::<Vec<(String, usize)>>();

                    if !arguments_index.is_empty() {
                        pattern_vec.push(Air::FieldsExpose {
                            count: arguments_index.len() + 2,
                            indices: arguments_index
                                .iter()
                                .map(|(name, index)| {
                                    let field_type = type_map.get(index).unwrap();

                                    (*index, name.clone(), field_type.clone())
                                })
                                .collect_vec(),
                            scope,
                        });
                    }
                }

                pattern_vec.append(values);
                pattern_vec.append(&mut nested_pattern);
            }
            Pattern::Tuple { elems, .. } => {
                let mut names = vec![];
                let mut nested_pattern = vec![];
                let items_type = &tipo.get_inner_types();

                for (index, element) in elems.iter().enumerate() {
                    let name = self.nested_pattern_ir_and_label(
                        element,
                        &mut nested_pattern,
                        &items_type[index],
                        scope.clone(),
                    );

                    names.push((name.unwrap_or_else(|| "_".to_string()), index))
                }
                let mut defined_indices = match clause_properties.clone() {
                    ClauseProperties::TupleClause {
                        defined_tuple_indices,
                        ..
                    } => defined_tuple_indices,
                    _ => unreachable!(),
                };

                let mut previous_defined_names = vec![];
                for (name, index) in names.clone() {
                    if let Some(defined_index) = defined_indices
                        .iter()
                        .find(|(defined_index, _)| *defined_index as usize == index)
                    {
                        previous_defined_names.push(defined_index.clone());
                    } else {
                        defined_indices.insert((index, name));
                    }
                }

                for (index, name) in previous_defined_names {
                    let new_name = names
                        .iter()
                        .find(|(_, current_index)| *current_index == index)
                        .map(|(new_name, _)| new_name)
                        .unwrap();

                    let pattern_type = &tipo.get_inner_types()[index];

                    pattern_vec.push(Air::Lam {
                        scope: scope.clone(),
                        name: new_name.clone(),
                    });
                    pattern_vec.push(Air::Var {
                        scope: scope.clone(),
                        constructor: ValueConstructor::public(
                            pattern_type.clone(),
                            ValueConstructorVariant::LocalVariable {
                                location: Span::empty(),
                            },
                        ),
                        name,
                        variant_name: String::new(),
                    });
                }

                match clause_properties {
                    ClauseProperties::TupleClause {
                        defined_tuple_indices,
                        ..
                    } => {
                        *defined_tuple_indices = defined_indices;
                    }
                    _ => unreachable!(),
                }

                pattern_vec.append(&mut nested_pattern);
                pattern_vec.append(values);
            }
        }
    }

    fn nested_pattern_ir_and_label(
        &mut self,
        pattern: &Pattern<tipo::PatternConstructor, Arc<Type>>,
        pattern_vec: &mut Vec<Air>,
        pattern_type: &Arc<Type>,
        scope: Vec<u64>,
    ) -> Option<String> {
        match pattern {
            Pattern::Var { name, .. } => Some(name.clone()),
            Pattern::Discard { .. } => None,
            a @ Pattern::List { elements, tail, .. } => {
                let item_name = format!("__list_item_id_{}", self.id_gen.next());
                let new_tail_name = "__list_tail".to_string();

                if elements.is_empty() {
                    pattern_vec.push(Air::ListClause {
                        scope: scope.clone(),
                        tipo: pattern_type.clone(),
                        tail_name: item_name.clone(),
                        next_tail_name: None,
                        complex_clause: false,
                        inverse: true,
                    });

                    pattern_vec.push(Air::Discard {
                        scope: scope.clone(),
                    });

                    pattern_vec.push(Air::Var {
                        scope,
                        constructor: ValueConstructor::public(
                            pattern_type.clone(),
                            ValueConstructorVariant::LocalVariable {
                                location: Span::empty(),
                            },
                        ),
                        name: "__other_clauses_delayed".to_string(),
                        variant_name: String::new(),
                    });
                } else {
                    for (index, _) in elements.iter().enumerate() {
                        let prev_tail_name = if index == 0 {
                            item_name.clone()
                        } else {
                            format!("{}_{}", new_tail_name, index - 1)
                        };

                        let mut clause_properties = ClauseProperties::ListClause {
                            clause_var_name: item_name.clone(),
                            needs_constr_var: false,
                            is_complex_clause: false,
                            original_subject_name: item_name.clone(),
                            current_index: index,
                        };

                        let tail_name = format!("{}_{}", new_tail_name, index);

                        if elements.len() - 1 == index {
                            if tail.is_some() {
                                let tail_name = match *tail.clone().unwrap() {
                                    Pattern::Var { name, .. } => name,
                                    Pattern::Discard { .. } => "_".to_string(),
                                    _ => unreachable!(),
                                };

                                pattern_vec.push(Air::ListClause {
                                    scope: scope.clone(),
                                    tipo: pattern_type.clone(),
                                    tail_name: prev_tail_name,
                                    next_tail_name: Some(tail_name),
                                    complex_clause: false,
                                    inverse: false,
                                });

                                pattern_vec.push(Air::Discard {
                                    scope: scope.clone(),
                                });

                                pattern_vec.push(Air::Var {
                                    scope: scope.clone(),
                                    constructor: ValueConstructor::public(
                                        pattern_type.clone(),
                                        ValueConstructorVariant::LocalVariable {
                                            location: Span::empty(),
                                        },
                                    ),
                                    name: "__other_clauses_delayed".to_string(),
                                    variant_name: "".to_string(),
                                });

                                self.when_ir(
                                    a,
                                    pattern_vec,
                                    &mut vec![],
                                    pattern_type,
                                    &mut clause_properties,
                                    scope.clone(),
                                );
                            } else {
                                pattern_vec.push(Air::ListClause {
                                    scope: scope.clone(),
                                    tipo: pattern_type.clone(),
                                    tail_name: prev_tail_name,
                                    next_tail_name: Some(tail_name.clone()),
                                    complex_clause: false,
                                    inverse: false,
                                });

                                pattern_vec.push(Air::Discard {
                                    scope: scope.clone(),
                                });

                                pattern_vec.push(Air::Var {
                                    scope: scope.clone(),
                                    constructor: ValueConstructor::public(
                                        pattern_type.clone(),
                                        ValueConstructorVariant::LocalVariable {
                                            location: Span::empty(),
                                        },
                                    ),
                                    name: "__other_clauses_delayed".to_string(),
                                    variant_name: String::new(),
                                });

                                pattern_vec.push(Air::ListClause {
                                    scope: scope.clone(),
                                    tipo: pattern_type.clone(),
                                    tail_name: tail_name.clone(),
                                    next_tail_name: None,
                                    complex_clause: false,
                                    inverse: true,
                                });

                                pattern_vec.push(Air::Discard {
                                    scope: scope.clone(),
                                });

                                pattern_vec.push(Air::Var {
                                    scope: scope.clone(),
                                    constructor: ValueConstructor::public(
                                        pattern_type.clone(),
                                        ValueConstructorVariant::LocalVariable {
                                            location: Span::empty(),
                                        },
                                    ),
                                    name: "__other_clauses_delayed".to_string(),
                                    variant_name: String::new(),
                                });

                                self.when_ir(
                                    a,
                                    pattern_vec,
                                    &mut vec![],
                                    pattern_type,
                                    &mut clause_properties,
                                    scope.clone(),
                                );
                            }
                        } else {
                            let tail_name = match *tail.clone().unwrap() {
                                Pattern::Var { name, .. } => name,
                                Pattern::Discard { .. } => "_".to_string(),
                                _ => unreachable!(),
                            };

                            pattern_vec.push(Air::ListClause {
                                scope: scope.clone(),
                                tipo: pattern_type.clone(),
                                tail_name: prev_tail_name,
                                next_tail_name: Some(tail_name),
                                complex_clause: false,
                                inverse: false,
                            });

                            pattern_vec.push(Air::Discard {
                                scope: scope.clone(),
                            });

                            pattern_vec.push(Air::Var {
                                scope: scope.clone(),
                                constructor: ValueConstructor::public(
                                    pattern_type.clone(),
                                    ValueConstructorVariant::LocalVariable {
                                        location: Span::empty(),
                                    },
                                ),
                                name: "__other_clauses_delayed".to_string(),
                                variant_name: "".to_string(),
                            });

                            self.when_ir(
                                a,
                                pattern_vec,
                                &mut vec![],
                                pattern_type,
                                &mut clause_properties,
                                scope.clone(),
                            );
                        };
                    }
                }

                // self.when_recursive_ir(a);
                Some(item_name)
            }
            a @ Pattern::Constructor {
                tipo,
                name: constr_name,
                ..
            } => {
                let id = self.id_gen.next();
                let constr_var_name = format!("{constr_name}_{id}");
                let data_type_key = match tipo.as_ref() {
                    Type::Fn { ret, .. } => match &**ret {
                        Type::App { module, name, .. } => DataTypeKey {
                            module_name: module.clone(),
                            defined_type: name.clone(),
                        },
                        _ => unreachable!(),
                    },
                    Type::App { module, name, .. } => DataTypeKey {
                        module_name: module.clone(),
                        defined_type: name.clone(),
                    },
                    _ => unreachable!(),
                };

                let data_type = self.data_types.get(&data_type_key).unwrap();

                if data_type.constructors.len() > 1 {
                    pattern_vec.push(Air::ClauseGuard {
                        scope: scope.clone(),
                        tipo: tipo.clone(),
                        subject_name: constr_var_name.clone(),
                    });
                }

                let mut clause_properties = ClauseProperties::ConstrClause {
                    clause_var_name: constr_var_name.clone(),
                    needs_constr_var: false,
                    is_complex_clause: false,
                    original_subject_name: constr_var_name.clone(),
                };

                self.when_ir(
                    a,
                    pattern_vec,
                    &mut vec![],
                    tipo,
                    &mut clause_properties,
                    scope,
                );

                Some(constr_var_name)
            }
            a @ Pattern::Tuple { elems, .. } => {
                let item_name = format!("__tuple_item_id_{}", self.id_gen.next());

                let mut clause_properties = ClauseProperties::TupleClause {
                    clause_var_name: item_name.clone(),
                    needs_constr_var: false,
                    is_complex_clause: false,
                    original_subject_name: item_name.clone(),
                    defined_tuple_indices: HashSet::new(),
                };

                let mut inner_pattern_vec = vec![];

                self.when_ir(
                    a,
                    &mut inner_pattern_vec,
                    &mut vec![],
                    pattern_type,
                    &mut clause_properties,
                    scope.clone(),
                );

                let defined_indices = match clause_properties.clone() {
                    ClauseProperties::TupleClause {
                        defined_tuple_indices,
                        ..
                    } => defined_tuple_indices,
                    _ => unreachable!(),
                };

                pattern_vec.push(Air::TupleClause {
                    scope,
                    tipo: pattern_type.clone(),
                    indices: defined_indices,
                    predefined_indices: HashSet::new(),
                    subject_name: clause_properties.original_subject_name().to_string(),
                    count: elems.len(),
                    complex_clause: false,
                });

                pattern_vec.append(&mut inner_pattern_vec);

                Some(item_name)
            }
            _ => todo!(),
        }
    }

    fn assignment_ir(
        &mut self,
        pattern: &Pattern<tipo::PatternConstructor, Arc<Type>>,
        pattern_vec: &mut Vec<Air>,
        value_vec: &mut Vec<Air>,
        tipo: &Type,
        kind: AssignmentKind,
        scope: Vec<u64>,
    ) {
        match pattern {
            Pattern::Int { .. } | Pattern::String { .. } => unreachable!(),
            Pattern::Var { name, .. } => {
                pattern_vec.push(Air::Assignment {
                    name: name.clone(),
                    kind,
                    scope,
                });

                pattern_vec.append(value_vec);
            }
            Pattern::VarUsage { .. } => todo!(),
            Pattern::Assign { .. } => todo!(),
            Pattern::Discard { .. } => {
                self.pattern_ir(pattern, pattern_vec, value_vec, tipo, scope)
            }
            list @ Pattern::List { .. } => {
                self.pattern_ir(list, pattern_vec, value_vec, tipo, scope);
            }
            Pattern::Constructor { .. } => {
                self.pattern_ir(pattern, pattern_vec, value_vec, tipo, scope);
            }
            Pattern::Tuple { .. } => {
                self.pattern_ir(pattern, pattern_vec, value_vec, tipo, scope);
            }
        }
    }

    fn pattern_ir(
        &mut self,
        pattern: &Pattern<tipo::PatternConstructor, Arc<tipo::Type>>,
        pattern_vec: &mut Vec<Air>,
        values: &mut Vec<Air>,
        tipo: &Type,
        scope: Vec<u64>,
    ) {
        match pattern {
            Pattern::Int { .. } => todo!(),
            Pattern::String { .. } => todo!(),
            Pattern::Var { .. } => todo!(),
            Pattern::VarUsage { .. } => todo!(),
            Pattern::Assign { .. } => todo!(),
            Pattern::Discard { .. } => {
                pattern_vec.push(Air::Discard { scope });

                pattern_vec.append(values);
            }
            Pattern::List { elements, tail, .. } => {
                let mut elements_vec = vec![];

                let mut names = vec![];
                for element in elements {
                    match element {
                        Pattern::Var { name, .. } => {
                            names.push(name.clone());
                        }
                        a @ Pattern::List { .. } => {
                            let mut var_vec = vec![];
                            let item_name = format!("list_item_id_{}", self.id_gen.next());
                            names.push(item_name.clone());
                            var_vec.push(Air::Var {
                                constructor: ValueConstructor::public(
                                    Type::App {
                                        public: true,
                                        module: String::new(),
                                        name: String::new(),
                                        args: vec![],
                                    }
                                    .into(),
                                    ValueConstructorVariant::LocalVariable {
                                        location: Span::empty(),
                                    },
                                ),
                                name: item_name,
                                scope: scope.clone(),
                                variant_name: String::new(),
                            });
                            self.pattern_ir(
                                a,
                                &mut elements_vec,
                                &mut var_vec,
                                &tipo.get_inner_types()[0],
                                scope.clone(),
                            );
                        }
                        _ => todo!(),
                    }
                }

                if let Some(tail) = tail {
                    match &**tail {
                        Pattern::Var { name, .. } => names.push(name.clone()),
                        Pattern::Discard { .. } => {}
                        _ => unreachable!(),
                    }
                }

                pattern_vec.push(Air::ListAccessor {
                    names,
                    tail: tail.is_some(),
                    scope,
                    tipo: tipo.clone().into(),
                });

                pattern_vec.append(values);
                pattern_vec.append(&mut elements_vec);
            }
            Pattern::Constructor {
                is_record,
                name: constr_name,
                arguments,
                constructor,
                tipo,
                ..
            } => {
                let data_type_key = match tipo.as_ref() {
                    Type::Fn { ret, .. } => match &**ret {
                        Type::App { module, name, .. } => DataTypeKey {
                            module_name: module.clone(),
                            defined_type: name.clone(),
                        },
                        _ => unreachable!(),
                    },
                    Type::App { module, name, .. } => DataTypeKey {
                        module_name: module.clone(),
                        defined_type: name.clone(),
                    },
                    _ => unreachable!(),
                };

                let data_type = self.data_types.get(&data_type_key).unwrap();
                let (_, constructor_type) = data_type
                    .constructors
                    .iter()
                    .enumerate()
                    .find(|(_, dt)| &dt.name == constr_name)
                    .unwrap();
                let mut nested_pattern = vec![];
                if *is_record {
                    let field_map = match constructor {
                        tipo::PatternConstructor::Record { field_map, .. } => {
                            field_map.clone().unwrap()
                        }
                    };

                    let mut type_map: HashMap<String, Arc<Type>> = HashMap::new();

                    for (index, arg) in tipo.arg_types().unwrap().iter().enumerate() {
                        let label = constructor_type.arguments[index].label.clone().unwrap();
                        let field_type = arg.clone();

                        type_map.insert(label, field_type);
                    }

                    let arguments_index = arguments
                        .iter()
                        .map(|item| {
                            let label = item.label.clone().unwrap_or_default();
                            let field_index =
                                field_map.fields.get(&label).map(|x| &x.0).unwrap_or(&0);
                            let (discard, var_name) = match &item.value {
                                Pattern::Var { name, .. } => (false, name.clone()),
                                Pattern::Discard { .. } => (true, "".to_string()),
                                Pattern::List { .. } => todo!(),
                                a @ Pattern::Constructor {
                                    tipo,
                                    name: constr_name,
                                    ..
                                } => {
                                    let id = self.id_gen.next();
                                    let constr_name = format!("{constr_name}_{id}");
                                    self.pattern_ir(
                                        a,
                                        &mut nested_pattern,
                                        &mut vec![Air::Var {
                                            scope: scope.clone(),
                                            constructor: ValueConstructor::public(
                                                tipo.clone(),
                                                ValueConstructorVariant::LocalVariable {
                                                    location: Span::empty(),
                                                },
                                            ),
                                            name: constr_name.clone(),
                                            variant_name: String::new(),
                                        }],
                                        tipo,
                                        scope.clone(),
                                    );

                                    (false, constr_name)
                                }
                                _ => todo!(),
                            };

                            (label, var_name, *field_index, discard)
                        })
                        .filter(|(_, _, _, discard)| !discard)
                        .sorted_by(|item1, item2| item1.2.cmp(&item2.2))
                        .collect::<Vec<(String, String, usize, bool)>>();

                    if !arguments_index.is_empty() {
                        pattern_vec.push(Air::FieldsExpose {
                            count: arguments_index.len() + 2,
                            indices: arguments_index
                                .iter()
                                .map(|(label, var_name, index, _)| {
                                    let field_type = type_map.get(label).unwrap();
                                    (*index, var_name.clone(), field_type.clone())
                                })
                                .collect_vec(),
                            scope,
                        });
                    }
                } else {
                    let mut type_map: HashMap<usize, Arc<Type>> = HashMap::new();

                    for (index, arg) in tipo.arg_types().unwrap().iter().enumerate() {
                        let field_type = arg.clone();

                        type_map.insert(index, field_type);
                    }

                    let arguments_index = arguments
                        .iter()
                        .enumerate()
                        .map(|(index, item)| {
                            let (discard, var_name) = match &item.value {
                                Pattern::Var { name, .. } => (false, name.clone()),
                                Pattern::Discard { .. } => (true, "".to_string()),
                                Pattern::List { .. } => todo!(),
                                a @ Pattern::Constructor {
                                    tipo,
                                    name: constr_name,
                                    ..
                                } => {
                                    let id = self.id_gen.next();
                                    let constr_name = format!("{constr_name}_{id}");
                                    self.pattern_ir(
                                        a,
                                        &mut nested_pattern,
                                        &mut vec![Air::Var {
                                            scope: scope.clone(),
                                            constructor: ValueConstructor::public(
                                                tipo.clone(),
                                                ValueConstructorVariant::LocalVariable {
                                                    location: Span::empty(),
                                                },
                                            ),
                                            name: constr_name.clone(),
                                            variant_name: String::new(),
                                        }],
                                        tipo,
                                        scope.clone(),
                                    );

                                    (false, constr_name)
                                }
                                _ => todo!(),
                            };

                            (var_name, index, discard)
                        })
                        .filter(|(_, _, discard)| !discard)
                        .collect::<Vec<(String, usize, bool)>>();

                    if !arguments_index.is_empty() {
                        pattern_vec.push(Air::FieldsExpose {
                            count: arguments_index.len() + 2,
                            indices: arguments_index
                                .iter()
                                .map(|(name, index, _)| {
                                    let field_type = type_map.get(index).unwrap();

                                    (*index, name.clone(), field_type.clone())
                                })
                                .collect_vec(),
                            scope,
                        });
                    }
                }

                pattern_vec.append(values);
                pattern_vec.append(&mut nested_pattern);
            }
            Pattern::Tuple { elems, .. } => {
                let mut elements_vec = vec![];

                let mut names = vec![];
                for element in elems {
                    match element {
                        Pattern::Var { name, .. } => {
                            names.push(name.clone());
                        }
                        a @ Pattern::List { .. } => {
                            let mut var_vec = vec![];
                            let item_name = format!("list_item_id_{}", self.id_gen.next());
                            names.push(item_name.clone());
                            var_vec.push(Air::Var {
                                constructor: ValueConstructor::public(
                                    Type::App {
                                        public: true,
                                        module: String::new(),
                                        name: String::new(),
                                        args: vec![],
                                    }
                                    .into(),
                                    ValueConstructorVariant::LocalVariable {
                                        location: Span::empty(),
                                    },
                                ),
                                name: item_name,
                                scope: scope.clone(),
                                variant_name: String::new(),
                            });
                            self.pattern_ir(
                                a,
                                &mut elements_vec,
                                &mut var_vec,
                                &tipo.get_inner_types()[0],
                                scope.clone(),
                            );
                        }
                        _ => todo!(),
                    }
                }
                pattern_vec.push(Air::TupleAccessor {
                    names,
                    scope,
                    tipo: tipo.clone().into(),
                });

                pattern_vec.append(values);
                pattern_vec.append(&mut elements_vec);
            }
        }
    }

    fn define_ir(&mut self, ir_stack: &mut Vec<Air>) {
        let mut func_components = IndexMap::new();
        let mut func_index_map = IndexMap::new();

        let recursion_func_map = IndexMap::new();

        self.define_recurse_ir(
            ir_stack,
            &mut func_components,
            &mut func_index_map,
            recursion_func_map,
        );

        let mut final_func_dep_ir = IndexMap::new();
        let mut zero_arg_defined_functions = HashMap::new();
        let mut to_be_defined = HashMap::new();

        let mut dependency_map = IndexMap::new();
        let mut dependency_vec = vec![];

        let mut func_keys = func_components.keys().cloned().collect_vec();

        // deal with function dependencies by sorting order in which we iter over them.
        while let Some(function) = func_keys.pop() {
            let funct_comp = func_components.get(&function).unwrap();
            if dependency_map.contains_key(&function) {
                dependency_map.shift_remove(&function);
            }
            dependency_map.insert(function, ());
            func_keys.extend(funct_comp.dependencies.clone().into_iter());
        }

        dependency_vec.extend(dependency_map.keys().cloned());

        for func in dependency_vec {
            if self.defined_functions.contains_key(&func) {
                continue;
            }
            let funt_comp = func_components.get(&func).unwrap();
            let func_scope = func_index_map.get(&func).unwrap();

            let mut dep_ir = vec![];

            if !funt_comp.args.is_empty() {
                // deal with function dependencies
                handle_func_deps_ir(
                    &mut dep_ir,
                    funt_comp,
                    &func_components,
                    &mut self.defined_functions,
                    &func_index_map,
                    func_scope,
                    &mut to_be_defined,
                );
                final_func_dep_ir.insert(func, dep_ir);
            } else {
                // since zero arg functions are run at compile time we need to pull all deps
                let mut defined_functions = HashMap::new();
                // deal with function dependencies in zero arg functions
                handle_func_deps_ir(
                    &mut dep_ir,
                    funt_comp,
                    &func_components,
                    &mut defined_functions,
                    &func_index_map,
                    func_scope,
                    &mut HashMap::new(),
                );

                let mut final_zero_arg_ir = dep_ir;
                final_zero_arg_ir.extend(funt_comp.ir.clone());
                self.zero_arg_functions.insert(func, final_zero_arg_ir);

                for (key, val) in defined_functions.into_iter() {
                    zero_arg_defined_functions.insert(key, val);
                }
            }
        }

        // handle functions that are used in zero arg funcs but also used by the validator
        // or a func used by the validator
        for (key, val) in zero_arg_defined_functions.into_iter() {
            if !to_be_defined.contains_key(&key) {
                self.defined_functions.insert(key, val);
            }
        }

        for (index, ir) in ir_stack.clone().into_iter().enumerate().rev() {
            {
                let temp_func_index_map = func_index_map.clone();
                let to_insert = temp_func_index_map
                    .into_iter()
                    .filter(|func| {
                        get_common_ancestor(&func.1, &ir.scope()) == ir.scope()
                            && !self.defined_functions.contains_key(&func.0)
                            && !self.zero_arg_functions.contains_key(&func.0)
                    })
                    .collect_vec();

                for (function_access_key, scopes) in to_insert.into_iter() {
                    func_index_map.remove(&function_access_key);

                    self.defined_functions
                        .insert(function_access_key.clone(), ());

                    let mut full_func_ir =
                        final_func_dep_ir.get(&function_access_key).unwrap().clone();

                    let func_comp = func_components.get(&function_access_key).unwrap().clone();

                    // zero arg functions are not recursive
                    if !func_comp.args.is_empty() {
                        let mut recursion_ir = vec![];
                        handle_recursion_ir(&function_access_key, &func_comp, &mut recursion_ir);

                        full_func_ir.push(Air::DefineFunc {
                            scope: scopes.clone(),
                            func_name: function_access_key.function_name.clone(),
                            module_name: function_access_key.module_name.clone(),
                            params: func_comp.args.clone(),
                            recursive: func_comp.recursive,
                            variant_name: function_access_key.variant_name.clone(),
                        });

                        full_func_ir.extend(recursion_ir);

                        for ir in full_func_ir.into_iter().rev() {
                            ir_stack.insert(index, ir);
                        }
                    } else {
                        full_func_ir.extend(func_comp.ir.clone());

                        self.zero_arg_functions
                            .insert(function_access_key, full_func_ir);
                    }
                }
            }
        }
    }

    fn define_recurse_ir(
        &mut self,
        ir_stack: &mut [Air],
        func_components: &mut IndexMap<FunctionAccessKey, FuncComponents>,
        func_index_map: &mut IndexMap<FunctionAccessKey, Vec<u64>>,
        mut recursion_func_map: IndexMap<FunctionAccessKey, ()>,
    ) {
        self.process_define_ir(ir_stack, func_components, func_index_map);

        let mut recursion_func_map_to_add = recursion_func_map.clone();

        for func_index in func_index_map.clone().iter() {
            let func = func_index.0;

            let function_components = func_components.get_mut(func).unwrap();
            let mut function_ir = function_components.ir.clone();
            let mut skip = false;

            for ir in function_ir.clone() {
                if let Air::Var {
                    constructor:
                        ValueConstructor {
                            variant:
                                ValueConstructorVariant::ModuleFn {
                                    name: func_name,
                                    module,
                                    ..
                                },
                            ..
                        },
                    variant_name,
                    ..
                } = ir
                {
                    if recursion_func_map.contains_key(&FunctionAccessKey {
                        module_name: module.clone(),
                        function_name: func_name.clone(),
                        variant_name: variant_name.clone(),
                    }) && func.clone()
                        == (FunctionAccessKey {
                            module_name: module.clone(),
                            function_name: func_name.clone(),
                            variant_name: variant_name.clone(),
                        })
                    {
                        skip = true;
                    } else if func.clone()
                        == (FunctionAccessKey {
                            module_name: module.clone(),
                            function_name: func_name.clone(),
                            variant_name: variant_name.clone(),
                        })
                    {
                        recursion_func_map_to_add.insert(
                            FunctionAccessKey {
                                module_name: module.clone(),
                                function_name: func_name.clone(),
                                variant_name: variant_name.clone(),
                            },
                            (),
                        );
                    }
                }
            }

            recursion_func_map = recursion_func_map_to_add.clone();
            if !skip {
                let mut inner_func_components = IndexMap::new();

                let mut inner_func_index_map = IndexMap::new();

                self.define_recurse_ir(
                    &mut function_ir,
                    &mut inner_func_components,
                    &mut inner_func_index_map,
                    recursion_func_map.clone(),
                );

                function_components.ir = function_ir;

                //now unify
                for item in inner_func_components {
                    if !func_components.contains_key(&item.0) {
                        func_components.insert(item.0, item.1);
                    }
                }

                for item in inner_func_index_map {
                    if let Some(entry) = func_index_map.get_mut(&item.0) {
                        *entry = get_common_ancestor(entry, &item.1);
                    } else {
                        func_index_map.insert(item.0, item.1);
                    }
                }
            }
        }
    }

    fn process_define_ir(
        &mut self,
        ir_stack: &mut [Air],
        func_components: &mut IndexMap<FunctionAccessKey, FuncComponents>,
        func_index_map: &mut IndexMap<FunctionAccessKey, Vec<u64>>,
    ) {
        let mut to_be_defined_map: IndexMap<FunctionAccessKey, Vec<u64>> = IndexMap::new();
        for (index, ir) in ir_stack.to_vec().iter().enumerate().rev() {
            match ir {
                Air::Var {
                    scope, constructor, ..
                } => {
                    if let ValueConstructorVariant::ModuleFn {
                        name,
                        module,
                        builtin,
                        ..
                    } = &constructor.variant
                    {
                        if builtin.is_none() {
                            let non_variant_function_key = FunctionAccessKey {
                                module_name: module.clone(),
                                function_name: name.clone(),
                                variant_name: String::new(),
                            };

                            let function = self.functions.get(&non_variant_function_key).unwrap();

                            let mut func_ir = vec![];

                            self.build_ir(&function.body, &mut func_ir, scope.to_vec());

                            let param_types = constructor.tipo.arg_types().unwrap();

                            let mut generics_type_map: HashMap<u64, Arc<Type>> = HashMap::new();

                            for (index, arg) in function.arguments.iter().enumerate() {
                                if arg.tipo.is_generic() {
                                    let mut map = generics_type_map.into_iter().collect_vec();
                                    map.append(&mut get_generics_and_type(
                                        &arg.tipo,
                                        &param_types[index],
                                    ));

                                    generics_type_map = map.into_iter().collect();
                                }
                            }

                            let (variant_name, func_ir) =
                                monomorphize(func_ir, generics_type_map, &constructor.tipo);

                            let function_key = FunctionAccessKey {
                                module_name: module.clone(),
                                function_name: non_variant_function_key.function_name,
                                variant_name: variant_name.clone(),
                            };

                            ir_stack[index] = Air::Var {
                                scope: scope.clone(),
                                constructor: constructor.clone(),
                                name: name.clone(),
                                variant_name: variant_name.clone(),
                            };

                            if let Some(scope_prev) = to_be_defined_map.get(&function_key) {
                                let new_scope = get_common_ancestor(scope, scope_prev);

                                to_be_defined_map.insert(function_key, new_scope);
                            } else if func_components.get(&function_key).is_some() {
                                to_be_defined_map.insert(function_key.clone(), scope.to_vec());
                            } else {
                                to_be_defined_map.insert(function_key.clone(), scope.to_vec());
                                let mut func_calls = HashMap::new();

                                for ir in func_ir.clone().into_iter() {
                                    if let Air::Var {
                                        constructor:
                                            ValueConstructor {
                                                variant:
                                                    ValueConstructorVariant::ModuleFn {
                                                        name: func_name,
                                                        module,
                                                        ..
                                                    },
                                                tipo,
                                                ..
                                            },
                                        ..
                                    } = ir
                                    {
                                        let current_func = FunctionAccessKey {
                                            module_name: module.clone(),
                                            function_name: func_name.clone(),
                                            variant_name: String::new(),
                                        };

                                        let current_func_as_variant = FunctionAccessKey {
                                            module_name: module.clone(),
                                            function_name: func_name.clone(),
                                            variant_name: variant_name.clone(),
                                        };

                                        let function = self.functions.get(&current_func);
                                        if function_key.clone() == current_func_as_variant {
                                            func_calls.insert(current_func_as_variant, ());
                                        } else if let (Some(function), Type::Fn { .. }) =
                                            (function, &*tipo)
                                        {
                                            let mut generics_type_map: HashMap<u64, Arc<Type>> =
                                                HashMap::new();

                                            let param_types = tipo.arg_types().unwrap();

                                            for (index, arg) in
                                                function.arguments.iter().enumerate()
                                            {
                                                if arg.tipo.is_generic() {
                                                    let mut map =
                                                        generics_type_map.into_iter().collect_vec();
                                                    map.append(&mut get_generics_and_type(
                                                        &arg.tipo,
                                                        &param_types[index],
                                                    ));

                                                    generics_type_map = map.into_iter().collect();
                                                }
                                            }

                                            let mut func_ir = vec![];

                                            self.build_ir(
                                                &function.body,
                                                &mut func_ir,
                                                scope.to_vec(),
                                            );

                                            let (variant_name, _) =
                                                monomorphize(func_ir, generics_type_map, &tipo);

                                            func_calls.insert(
                                                FunctionAccessKey {
                                                    module_name: current_func.module_name,
                                                    function_name: current_func.function_name,
                                                    variant_name,
                                                },
                                                (),
                                            );
                                        } else {
                                            func_calls.insert(current_func, ());
                                        }
                                    }
                                }

                                let mut args = vec![];

                                for arg in function.arguments.iter() {
                                    match &arg.arg_name {
                                        ArgName::Named { name, .. } => {
                                            args.push(name.clone());
                                        }
                                        _ => {
                                            args.push("_".to_string());
                                        }
                                    }
                                }

                                let recursive = if func_calls.get(&function_key).is_some() {
                                    func_calls.remove(&function_key);
                                    true
                                } else {
                                    false
                                };

                                func_components.insert(
                                    function_key,
                                    FuncComponents {
                                        ir: func_ir,
                                        dependencies: func_calls.keys().cloned().collect_vec(),
                                        recursive,
                                        args,
                                    },
                                );
                            }
                        }
                    }
                }
                a => {
                    let scope = a.scope();

                    for func in to_be_defined_map.clone().iter() {
                        if get_common_ancestor(&scope, func.1) == scope.to_vec() {
                            if let Some(index_scope) = func_index_map.get(func.0) {
                                if get_common_ancestor(index_scope, func.1) == scope.to_vec() {
                                    func_index_map.insert(func.0.clone(), scope.clone());
                                    to_be_defined_map.shift_remove(func.0);
                                } else {
                                    to_be_defined_map.insert(
                                        func.0.clone(),
                                        get_common_ancestor(index_scope, func.1),
                                    );
                                }
                            } else {
                                func_index_map.insert(func.0.clone(), scope.clone());
                                to_be_defined_map.shift_remove(func.0);
                            }
                        }
                    }
                }
            }
        }

        //Still to be defined
        for func in to_be_defined_map.clone().iter() {
            let index_scope = func_index_map.get(func.0).unwrap();
            func_index_map.insert(func.0.clone(), get_common_ancestor(func.1, index_scope));
        }
    }

    fn uplc_code_gen(&mut self, ir_stack: &mut Vec<Air>) -> Term<Name> {
        let mut arg_stack: Vec<Term<Name>> = vec![];

        while let Some(ir_element) = ir_stack.pop() {
            self.gen_uplc(ir_element, &mut arg_stack);
        }

        arg_stack[0].clone()
    }

    fn gen_uplc(&mut self, ir: Air, arg_stack: &mut Vec<Term<Name>>) {
        match ir {
            Air::Int { value, .. } => {
                let integer = value.parse().unwrap();

                let term = Term::Constant(UplcConstant::Integer(integer));

                arg_stack.push(term);
            }
            Air::String { value, .. } => {
                let term = Term::Constant(UplcConstant::String(value));

                arg_stack.push(term);
            }
            Air::ByteArray { bytes, .. } => {
                let term = Term::Constant(UplcConstant::ByteString(bytes));
                arg_stack.push(term);
            }
            Air::Var {
                name,
                constructor,
                variant_name,
                ..
            } => {
                match &constructor.variant {
                    ValueConstructorVariant::LocalVariable { .. } => {
                        arg_stack.push(Term::Var(Name {
                            text: name,
                            unique: 0.into(),
                        }))
                    }
                    ValueConstructorVariant::ModuleConstant { .. } => {
                        unreachable!()
                    }
                    ValueConstructorVariant::ModuleFn {
                        name: func_name,
                        module,
                        ..
                    } => {
                        let name = if (*func_name == name
                            || name == format!("{module}_{func_name}"))
                            && !module.is_empty()
                        {
                            format!("{module}_{func_name}{variant_name}")
                        } else {
                            format!("{func_name}{variant_name}")
                        };

                        arg_stack.push(Term::Var(Name {
                            text: name,
                            unique: 0.into(),
                        }));
                    }
                    ValueConstructorVariant::Record {
                        name: constr_name,
                        field_map,
                        arity,
                        ..
                    } => {
                        let data_type_key = match &*constructor.tipo {
                            Type::App { module, name, .. } => DataTypeKey {
                                module_name: module.to_string(),
                                defined_type: name.to_string(),
                            },
                            Type::Fn { ret, .. } => match ret.deref() {
                                Type::App { module, name, .. } => DataTypeKey {
                                    module_name: module.to_string(),
                                    defined_type: name.to_string(),
                                },
                                _ => unreachable!(),
                            },
                            Type::Var { .. } => todo!(),
                            Type::Tuple { .. } => todo!(),
                        };

                        if constructor.tipo.is_bool() {
                            arg_stack
                                .push(Term::Constant(UplcConstant::Bool(constr_name == "True")));
                        } else if constructor.tipo.is_void() {
                            arg_stack.push(Term::Constant(UplcConstant::Unit));
                        } else {
                            let data_type = self.data_types.get(&data_type_key).unwrap();

                            let (constr_index, _) = data_type
                                .constructors
                                .iter()
                                .enumerate()
                                .find(|(_, x)| x.name == *constr_name)
                                .unwrap();

                            let mut fields =
                                Term::Constant(UplcConstant::ProtoList(UplcType::Data, vec![]));

                            let tipo = constructor.tipo;

                            let args_type = tipo.arg_types().unwrap();

                            if let Some(field_map) = field_map.clone() {
                                for field in field_map
                                    .fields
                                    .iter()
                                    .sorted_by(|item1, item2| {
                                        let (a, _) = item1.1;
                                        let (b, _) = item2.1;
                                        a.cmp(b)
                                    })
                                    .zip(&args_type)
                                    .rev()
                                {
                                    // TODO revisit
                                    fields = Term::Apply {
                                        function: Term::Apply {
                                            function: Term::Builtin(DefaultFunction::MkCons)
                                                .force_wrap()
                                                .into(),
                                            argument: convert_type_to_data(
                                                Term::Var(Name {
                                                    text: field.0 .0.clone(),
                                                    unique: 0.into(),
                                                }),
                                                field.1,
                                            )
                                            .into(),
                                        }
                                        .into(),
                                        argument: fields.into(),
                                    };
                                }
                            } else {
                                for (index, arg) in args_type.iter().enumerate().take(*arity) {
                                    fields = Term::Apply {
                                        function: Term::Apply {
                                            function: Term::Builtin(DefaultFunction::MkCons)
                                                .force_wrap()
                                                .into(),
                                            argument: convert_type_to_data(
                                                Term::Var(Name {
                                                    text: format!("__arg_{}", index),
                                                    unique: 0.into(),
                                                }),
                                                arg,
                                            )
                                            .into(),
                                        }
                                        .into(),
                                        argument: fields.into(),
                                    };
                                }
                            }

                            let mut term = Term::Apply {
                                function: Term::Apply {
                                    function: Term::Builtin(DefaultFunction::ConstrData).into(),
                                    argument: Term::Constant(UplcConstant::Integer(
                                        constr_index.try_into().unwrap(),
                                    ))
                                    .into(),
                                }
                                .into(),
                                argument: fields.into(),
                            };

                            if let Some(field_map) = field_map {
                                for field in field_map
                                    .fields
                                    .iter()
                                    .sorted_by(|item1, item2| {
                                        let (a, _) = item1.1;
                                        let (b, _) = item2.1;
                                        a.cmp(b)
                                    })
                                    .rev()
                                {
                                    term = Term::Lambda {
                                        parameter_name: Name {
                                            text: field.0.clone(),
                                            unique: 0.into(),
                                        },
                                        body: term.into(),
                                    };
                                }
                            } else {
                                for (index, _) in args_type.iter().enumerate().take(*arity) {
                                    term = Term::Lambda {
                                        parameter_name: Name {
                                            text: format!("__arg_{}", index),
                                            unique: 0.into(),
                                        },
                                        body: term.into(),
                                    };
                                }
                            }

                            arg_stack.push(term);
                        }
                    }
                };
            }
            Air::Discard { .. } => {
                arg_stack.push(Term::Constant(UplcConstant::Unit));
            }
            Air::List {
                count, tipo, tail, ..
            } => {
                let mut args = vec![];

                for _ in 0..count {
                    let arg = arg_stack.pop().unwrap();
                    args.push(arg);
                }
                let mut constants = vec![];
                for arg in &args {
                    if let Term::Constant(c) = arg {
                        constants.push(c.clone())
                    }
                }

                let list_type = tipo.get_inner_types()[0].clone();

                if constants.len() == args.len() && !tail {
                    let list = if tipo.is_map() {
                        let mut convert_keys = vec![];
                        let mut convert_values = vec![];
                        for constant in constants {
                            match constant {
                                UplcConstant::ProtoPair(_, _, fst, snd) => {
                                    convert_keys.push(*fst);
                                    convert_values.push(*snd);
                                }
                                _ => unreachable!(),
                            }
                        }
                        convert_keys = convert_constants_to_data(convert_keys);
                        convert_values = convert_constants_to_data(convert_values);

                        Term::Constant(UplcConstant::ProtoList(
                            UplcType::Pair(UplcType::Data.into(), UplcType::Data.into()),
                            convert_keys
                                .into_iter()
                                .zip(convert_values.into_iter())
                                .map(|(key, value)| {
                                    UplcConstant::ProtoPair(
                                        UplcType::Data,
                                        UplcType::Data,
                                        key.into(),
                                        value.into(),
                                    )
                                })
                                .collect_vec(),
                        ))
                    } else {
                        Term::Constant(UplcConstant::ProtoList(
                            UplcType::Data,
                            convert_constants_to_data(constants),
                        ))
                    };

                    arg_stack.push(list);
                } else {
                    let mut term = if tail {
                        arg_stack.pop().unwrap()
                    } else if tipo.is_map() {
                        Term::Constant(UplcConstant::ProtoList(
                            UplcType::Pair(UplcType::Data.into(), UplcType::Data.into()),
                            vec![],
                        ))
                    } else {
                        Term::Constant(UplcConstant::ProtoList(UplcType::Data, vec![]))
                    };

                    for arg in args.into_iter().rev() {
                        let list_item = if tipo.is_map() {
                            arg
                        } else {
                            convert_type_to_data(arg, &list_type)
                        };
                        term = Term::Apply {
                            function: Term::Apply {
                                function: Term::Builtin(DefaultFunction::MkCons)
                                    .force_wrap()
                                    .into(),
                                argument: list_item.into(),
                            }
                            .into(),
                            argument: term.into(),
                        };
                    }
                    arg_stack.push(term);
                }
            }
            Air::ListAccessor {
                names, tail, tipo, ..
            } => {
                let value = arg_stack.pop().unwrap();
                let mut term = arg_stack.pop().unwrap();

                let mut id_list = vec![];

                for _ in 0..names.len() {
                    id_list.push(self.id_gen.next());
                }

                let current_index = 0;
                let (first_name, names) = names.split_first().unwrap();

                let list_id = self.id_gen.next();

                let head_list = if tipo.is_map() {
                    Term::Apply {
                        function: Term::Force(Term::Builtin(DefaultFunction::HeadList).into())
                            .into(),
                        argument: Term::Var(Name {
                            text: format!("__list_{}", list_id),
                            unique: 0.into(),
                        })
                        .into(),
                    }
                } else {
                    convert_data_to_type(
                        Term::Apply {
                            function: Term::Force(Term::Builtin(DefaultFunction::HeadList).into())
                                .into(),
                            argument: Term::Var(Name {
                                text: format!("__list_{}", list_id),
                                unique: 0.into(),
                            })
                            .into(),
                        },
                        &tipo.get_inner_types()[0],
                    )
                };

                term = Term::Apply {
                    function: Term::Lambda {
                        parameter_name: Name {
                            text: format!("__list_{}", list_id),
                            unique: 0.into(),
                        },
                        body: Term::Apply {
                            function: Term::Lambda {
                                parameter_name: Name {
                                    text: first_name.clone(),
                                    unique: 0.into(),
                                },
                                body: Term::Apply {
                                    function: list_access_to_uplc(
                                        names,
                                        &id_list,
                                        tail,
                                        current_index,
                                        term,
                                        &tipo,
                                    )
                                    .into(),
                                    argument: Term::Apply {
                                        function: Term::Builtin(DefaultFunction::TailList)
                                            .force_wrap()
                                            .into(),
                                        argument: Term::Var(Name {
                                            text: format!("__list_{}", list_id),
                                            unique: 0.into(),
                                        })
                                        .into(),
                                    }
                                    .into(),
                                }
                                .into(),
                            }
                            .into(),
                            argument: head_list.into(),
                        }
                        .into(),
                    }
                    .into(),
                    argument: value.into(),
                };

                arg_stack.push(term);
            }
            Air::ListExpose {
                tail_head_names,
                tail,
                tipo,
                ..
            } => {
                let mut term = arg_stack.pop().unwrap();

                if let Some((tail_var, tail_name)) = tail {
                    term = Term::Apply {
                        function: Term::Lambda {
                            parameter_name: Name {
                                text: tail_name,
                                unique: 0.into(),
                            },
                            body: term.into(),
                        }
                        .into(),
                        argument: Term::Apply {
                            function: Term::Builtin(DefaultFunction::TailList).force_wrap().into(),
                            argument: Term::Var(Name {
                                text: tail_var,
                                unique: 0.into(),
                            })
                            .into(),
                        }
                        .into(),
                    };
                }

                for (tail_var, head_name) in tail_head_names.into_iter().rev() {
                    let head_list = if tipo.is_map() {
                        Term::Apply {
                            function: Term::Force(Term::Builtin(DefaultFunction::HeadList).into())
                                .into(),
                            argument: Term::Var(Name {
                                text: tail_var,
                                unique: 0.into(),
                            })
                            .into(),
                        }
                    } else {
                        convert_data_to_type(
                            Term::Apply {
                                function: Term::Force(
                                    Term::Builtin(DefaultFunction::HeadList).into(),
                                )
                                .into(),
                                argument: Term::Var(Name {
                                    text: tail_var,
                                    unique: 0.into(),
                                })
                                .into(),
                            },
                            &tipo.get_inner_types()[0],
                        )
                    };
                    term = Term::Apply {
                        function: Term::Lambda {
                            parameter_name: Name {
                                text: head_name,
                                unique: 0.into(),
                            },
                            body: term.into(),
                        }
                        .into(),
                        argument: head_list.into(),
                    };
                }

                arg_stack.push(term);
            }
            Air::Fn { params, .. } => {
                let mut term = arg_stack.pop().unwrap();

                for param in params.iter().rev() {
                    term = Term::Lambda {
                        parameter_name: Name {
                            text: param.clone(),
                            unique: 0.into(),
                        },
                        body: term.into(),
                    };
                }

                arg_stack.push(term);
            }
            Air::Call { count, .. } => {
                if count >= 1 {
                    let mut term = arg_stack.pop().unwrap();

                    for _ in 0..count {
                        let arg = arg_stack.pop().unwrap();

                        term = Term::Apply {
                            function: term.into(),
                            argument: arg.into(),
                        };
                    }
                    arg_stack.push(term);
                } else {
                    let term = arg_stack.pop().unwrap();

                    let zero_arg_functions = self.zero_arg_functions.clone();

                    if let Term::Var(Name { text, .. }) = term {
                        for (
                            FunctionAccessKey {
                                module_name,
                                function_name,
                                variant_name,
                            },
                            ir,
                        ) in zero_arg_functions.into_iter()
                        {
                            let name_module =
                                format!("{module_name}_{function_name}{variant_name}");
                            let name = format!("{function_name}{variant_name}");
                            if text == name || text == name_module {
                                let mut term = self.uplc_code_gen(&mut ir.clone());
                                term = builder::constr_get_field(term);
                                term = builder::constr_fields_exposer(term);

                                let mut program: Program<Name> = Program {
                                    version: (1, 0, 0),
                                    term,
                                };

                                let mut interner = Interner::new();

                                interner.program(&mut program);

                                let eval_program: Program<NamedDeBruijn> =
                                    program.try_into().unwrap();

                                let evaluated_term: Term<NamedDeBruijn> =
                                    eval_program.eval(ExBudget::default()).0.unwrap();

                                arg_stack.push(evaluated_term.try_into().unwrap());
                            }
                        }
                    }
                }
            }
            Air::Builtin { func, tipo, .. } => match func {
                DefaultFunction::FstPair | DefaultFunction::SndPair | DefaultFunction::HeadList => {
                    let id = self.id_gen.next();
                    let mut term: Term<Name> = func.into();
                    for _ in 0..func.force_count() {
                        term = term.force_wrap();
                    }

                    term = Term::Apply {
                        function: term.into(),
                        argument: Term::Var(Name {
                            text: format!("__arg_{}", id),
                            unique: 0.into(),
                        })
                        .into(),
                    };

                    let inner_type = if matches!(func, DefaultFunction::SndPair) {
                        tipo.get_inner_types()[0].get_inner_types()[1].clone()
                    } else {
                        tipo.get_inner_types()[0].get_inner_types()[0].clone()
                    };

                    term = convert_data_to_type(term, &inner_type);
                    term = Term::Lambda {
                        parameter_name: Name {
                            text: format!("__arg_{}", id),
                            unique: 0.into(),
                        },
                        body: term.into(),
                    };

                    arg_stack.push(term);
                }
                DefaultFunction::MkCons => todo!(),
                DefaultFunction::MkPairData => todo!(),
                _ => {
                    let mut term = Term::Builtin(func);
                    for _ in 0..func.force_count() {
                        term = term.force_wrap();
                    }
                    arg_stack.push(term);
                }
            },
            Air::BinOp { name, tipo, .. } => {
                let left = arg_stack.pop().unwrap();
                let right = arg_stack.pop().unwrap();

                let default_builtin = if tipo.is_int() {
                    DefaultFunction::EqualsInteger
                } else if tipo.is_string() {
                    DefaultFunction::EqualsString
                } else if tipo.is_bytearray() {
                    DefaultFunction::EqualsByteString
                } else {
                    DefaultFunction::EqualsData
                };

                let term = match name {
                    BinOp::And => {
                        delayed_if_else(left, right, Term::Constant(UplcConstant::Bool(false)))
                    }
                    BinOp::Or => {
                        delayed_if_else(left, Term::Constant(UplcConstant::Bool(true)), right)
                    }

                    BinOp::Eq => {
                        if tipo.is_bool() {
                            let term = delayed_if_else(
                                left,
                                right.clone(),
                                if_else(
                                    right,
                                    Term::Constant(UplcConstant::Bool(false)),
                                    Term::Constant(UplcConstant::Bool(true)),
                                ),
                            );
                            arg_stack.push(term);
                            return;
                        } else if tipo.is_map() {
                            let term = Term::Apply {
                                function: Term::Apply {
                                    function: default_builtin.into(),
                                    argument: Term::Apply {
                                        function: DefaultFunction::MapData.into(),
                                        argument: left.into(),
                                    }
                                    .into(),
                                }
                                .into(),
                                argument: Term::Apply {
                                    function: DefaultFunction::MapData.into(),
                                    argument: right.into(),
                                }
                                .into(),
                            };
                            arg_stack.push(term);
                            return;
                        } else if tipo.is_tuple()
                            && matches!(tipo.clone().get_uplc_type(), UplcType::Pair(_, _))
                        {
                            let term = Term::Apply {
                                function: Term::Apply {
                                    function: default_builtin.into(),
                                    argument: Term::Apply {
                                        function: DefaultFunction::MapData.into(),
                                        argument: Term::Apply {
                                            function: Term::Apply {
                                                function: Term::Builtin(DefaultFunction::MkCons)
                                                    .force_wrap()
                                                    .into(),
                                                argument: left.into(),
                                            }
                                            .into(),
                                            argument: Term::Constant(UplcConstant::ProtoList(
                                                UplcType::Pair(
                                                    UplcType::Data.into(),
                                                    UplcType::Data.into(),
                                                ),
                                                vec![],
                                            ))
                                            .into(),
                                        }
                                        .into(),
                                    }
                                    .into(),
                                }
                                .into(),
                                argument: Term::Apply {
                                    function: DefaultFunction::MapData.into(),
                                    argument: Term::Apply {
                                        function: Term::Apply {
                                            function: Term::Builtin(DefaultFunction::MkCons)
                                                .force_wrap()
                                                .into(),
                                            argument: right.into(),
                                        }
                                        .into(),
                                        argument: Term::Constant(UplcConstant::ProtoList(
                                            UplcType::Pair(
                                                UplcType::Data.into(),
                                                UplcType::Data.into(),
                                            ),
                                            vec![],
                                        ))
                                        .into(),
                                    }
                                    .into(),
                                }
                                .into(),
                            };
                            arg_stack.push(term);
                            return;
                        } else if tipo.is_list() {
                            let term = Term::Apply {
                                function: Term::Apply {
                                    function: default_builtin.into(),
                                    argument: Term::Apply {
                                        function: DefaultFunction::ListData.into(),
                                        argument: left.into(),
                                    }
                                    .into(),
                                }
                                .into(),

                                argument: Term::Apply {
                                    function: DefaultFunction::ListData.into(),
                                    argument: right.into(),
                                }
                                .into(),
                            };
                            arg_stack.push(term);
                            return;
                        } else if tipo.is_void() {
                            arg_stack.push(Term::Constant(UplcConstant::Bool(true)));
                            return;
                        }

                        Term::Apply {
                            function: Term::Apply {
                                function: default_builtin.into(),
                                argument: left.into(),
                            }
                            .into(),
                            argument: right.into(),
                        }
                    }
                    BinOp::NotEq => {
                        if tipo.is_bool() {
                            let term = delayed_if_else(
                                left,
                                if_else(
                                    right.clone(),
                                    Term::Constant(UplcConstant::Bool(false)),
                                    Term::Constant(UplcConstant::Bool(true)),
                                ),
                                right,
                            );
                            arg_stack.push(term);
                            return;
                        } else if tipo.is_map() {
                            let term = Term::Apply {
                                function: Term::Apply {
                                    function: Term::Apply {
                                        function: Term::Builtin(DefaultFunction::IfThenElse)
                                            .force_wrap()
                                            .into(),
                                        argument: Term::Apply {
                                            function: Term::Apply {
                                                function: default_builtin.into(),
                                                argument: Term::Apply {
                                                    function: DefaultFunction::MapData.into(),
                                                    argument: left.into(),
                                                }
                                                .into(),
                                            }
                                            .into(),
                                            argument: Term::Apply {
                                                function: DefaultFunction::MapData.into(),
                                                argument: right.into(),
                                            }
                                            .into(),
                                        }
                                        .into(),
                                    }
                                    .into(),
                                    argument: Term::Constant(UplcConstant::Bool(false)).into(),
                                }
                                .into(),
                                argument: Term::Constant(UplcConstant::Bool(true)).into(),
                            };
                            arg_stack.push(term);
                            return;
                        } else if tipo.is_tuple()
                            && matches!(tipo.clone().get_uplc_type(), UplcType::Pair(_, _))
                        {
                            let mut term = Term::Apply {
                                function: Term::Apply {
                                    function: default_builtin.into(),
                                    argument: Term::Apply {
                                        function: DefaultFunction::MapData.into(),
                                        argument: Term::Apply {
                                            function: Term::Apply {
                                                function: Term::Builtin(DefaultFunction::MkCons)
                                                    .force_wrap()
                                                    .into(),
                                                argument: left.into(),
                                            }
                                            .into(),
                                            argument: Term::Constant(UplcConstant::ProtoList(
                                                UplcType::Pair(
                                                    UplcType::Data.into(),
                                                    UplcType::Data.into(),
                                                ),
                                                vec![],
                                            ))
                                            .into(),
                                        }
                                        .into(),
                                    }
                                    .into(),
                                }
                                .into(),
                                argument: Term::Apply {
                                    function: Term::Apply {
                                        function: Term::Builtin(DefaultFunction::MkCons)
                                            .force_wrap()
                                            .into(),
                                        argument: right.into(),
                                    }
                                    .into(),
                                    argument: Term::Constant(UplcConstant::ProtoList(
                                        UplcType::Pair(
                                            UplcType::Data.into(),
                                            UplcType::Data.into(),
                                        ),
                                        vec![],
                                    ))
                                    .into(),
                                }
                                .into(),
                            };

                            term = if_else(
                                term,
                                Term::Constant(UplcConstant::Bool(false)),
                                Term::Constant(UplcConstant::Bool(true)),
                            );
                            arg_stack.push(term);
                            return;
                        } else if tipo.is_list() {
                            let term = if_else(
                                Term::Apply {
                                    function: Term::Apply {
                                        function: default_builtin.into(),
                                        argument: Term::Apply {
                                            function: DefaultFunction::ListData.into(),
                                            argument: left.into(),
                                        }
                                        .into(),
                                    }
                                    .into(),
                                    argument: Term::Apply {
                                        function: default_builtin.into(),
                                        argument: Term::Apply {
                                            function: DefaultFunction::ListData.into(),
                                            argument: right.into(),
                                        }
                                        .into(),
                                    }
                                    .into(),
                                },
                                Term::Constant(UplcConstant::Bool(false)),
                                Term::Constant(UplcConstant::Bool(true)),
                            );

                            arg_stack.push(term);
                            return;
                        } else if tipo.is_void() {
                            arg_stack.push(Term::Constant(UplcConstant::Bool(false)));
                            return;
                        }

                        Term::Apply {
                            function: Term::Apply {
                                function: Term::Apply {
                                    function: Term::Builtin(DefaultFunction::IfThenElse)
                                        .force_wrap()
                                        .into(),
                                    argument: Term::Apply {
                                        function: Term::Apply {
                                            function: default_builtin.into(),
                                            argument: left.into(),
                                        }
                                        .into(),
                                        argument: right.into(),
                                    }
                                    .into(),
                                }
                                .into(),
                                argument: Term::Constant(UplcConstant::Bool(false)).into(),
                            }
                            .into(),
                            argument: Term::Constant(UplcConstant::Bool(true)).into(),
                        }
                    }
                    BinOp::LtInt => Term::Apply {
                        function: Term::Apply {
                            function: Term::Builtin(DefaultFunction::LessThanInteger).into(),
                            argument: left.into(),
                        }
                        .into(),
                        argument: right.into(),
                    },
                    BinOp::LtEqInt => Term::Apply {
                        function: Term::Apply {
                            function: Term::Builtin(DefaultFunction::LessThanEqualsInteger).into(),
                            argument: left.into(),
                        }
                        .into(),
                        argument: right.into(),
                    },
                    BinOp::GtEqInt => Term::Apply {
                        function: Term::Apply {
                            function: Term::Builtin(DefaultFunction::LessThanEqualsInteger).into(),
                            argument: right.into(),
                        }
                        .into(),
                        argument: left.into(),
                    },
                    BinOp::GtInt => Term::Apply {
                        function: Term::Apply {
                            function: Term::Builtin(DefaultFunction::LessThanInteger).into(),
                            argument: right.into(),
                        }
                        .into(),
                        argument: left.into(),
                    },
                    BinOp::AddInt => Term::Apply {
                        function: Term::Apply {
                            function: Term::Builtin(DefaultFunction::AddInteger).into(),
                            argument: left.into(),
                        }
                        .into(),
                        argument: right.into(),
                    },
                    BinOp::SubInt => Term::Apply {
                        function: Term::Apply {
                            function: Term::Builtin(DefaultFunction::SubtractInteger).into(),
                            argument: left.into(),
                        }
                        .into(),
                        argument: right.into(),
                    },
                    BinOp::MultInt => Term::Apply {
                        function: Term::Apply {
                            function: Term::Builtin(DefaultFunction::MultiplyInteger).into(),
                            argument: left.into(),
                        }
                        .into(),
                        argument: right.into(),
                    },
                    BinOp::DivInt => Term::Apply {
                        function: Term::Apply {
                            function: Term::Builtin(DefaultFunction::DivideInteger).into(),
                            argument: left.into(),
                        }
                        .into(),
                        argument: right.into(),
                    },
                    BinOp::ModInt => Term::Apply {
                        function: Term::Apply {
                            function: Term::Builtin(DefaultFunction::ModInteger).into(),
                            argument: left.into(),
                        }
                        .into(),
                        argument: right.into(),
                    },
                };
                arg_stack.push(term);
            }
            Air::Assignment { name, .. } => {
                let right_hand = arg_stack.pop().unwrap();
                let lam_body = arg_stack.pop().unwrap();

                let term = Term::Apply {
                    function: Term::Lambda {
                        parameter_name: Name {
                            text: name,
                            unique: 0.into(),
                        },
                        body: lam_body.into(),
                    }
                    .into(),
                    argument: right_hand.into(),
                };

                arg_stack.push(term);
            }
            Air::DefineFunc {
                func_name,
                params,
                recursive,
                module_name,
                variant_name,
                ..
            } => {
                let func_name = if module_name.is_empty() {
                    format!("{func_name}{variant_name}")
                } else {
                    format!("{module_name}_{func_name}{variant_name}")
                };
                let mut func_body = arg_stack.pop().unwrap();

                let mut term = arg_stack.pop().unwrap();

                for param in params.iter().rev() {
                    func_body = Term::Lambda {
                        parameter_name: Name {
                            text: param.clone(),
                            unique: 0.into(),
                        },
                        body: func_body.into(),
                    };
                }

                if !recursive {
                    term = Term::Apply {
                        function: Term::Lambda {
                            parameter_name: Name {
                                text: func_name,
                                unique: 0.into(),
                            },
                            body: term.into(),
                        }
                        .into(),
                        argument: func_body.into(),
                    };
                    arg_stack.push(term);
                } else {
                    func_body = Term::Lambda {
                        parameter_name: Name {
                            text: func_name.clone(),
                            unique: 0.into(),
                        },
                        body: func_body.into(),
                    };

                    term = Term::Apply {
                        function: Term::Lambda {
                            parameter_name: Name {
                                text: func_name.clone(),
                                unique: 0.into(),
                            },
                            body: Term::Apply {
                                function: Term::Lambda {
                                    parameter_name: Name {
                                        text: func_name.clone(),
                                        unique: 0.into(),
                                    },
                                    body: term.into(),
                                }
                                .into(),
                                argument: Term::Apply {
                                    function: Term::Var(Name {
                                        text: func_name.clone(),
                                        unique: 0.into(),
                                    })
                                    .into(),
                                    argument: Term::Var(Name {
                                        text: func_name,
                                        unique: 0.into(),
                                    })
                                    .into(),
                                }
                                .into(),
                            }
                            .into(),
                        }
                        .into(),
                        argument: func_body.into(),
                    };

                    arg_stack.push(term);
                }
            }
            Air::DefineConst { .. } => todo!(),
            Air::DefineConstrFields { .. } => todo!(),
            Air::DefineConstrFieldAccess { .. } => todo!(),
            Air::Lam { name, .. } => {
                let arg = arg_stack.pop().unwrap();

                let mut term = arg_stack.pop().unwrap();

                term = Term::Apply {
                    function: Term::Lambda {
                        parameter_name: Name {
                            text: name,
                            unique: 0.into(),
                        },
                        body: term.into(),
                    }
                    .into(),
                    argument: arg.into(),
                };
                arg_stack.push(term);
            }
            Air::When {
                subject_name, tipo, ..
            } => {
                let subject = arg_stack.pop().unwrap();

                let mut term = arg_stack.pop().unwrap();

                term = if tipo.is_int()
                    || tipo.is_bytearray()
                    || tipo.is_string()
                    || tipo.is_list()
                    || tipo.is_tuple()
                {
                    Term::Apply {
                        function: Term::Lambda {
                            parameter_name: Name {
                                text: subject_name,
                                unique: 0.into(),
                            },
                            body: term.into(),
                        }
                        .into(),
                        argument: subject.into(),
                    }
                } else {
                    Term::Apply {
                        function: Term::Lambda {
                            parameter_name: Name {
                                text: subject_name,
                                unique: 0.into(),
                            },
                            body: term.into(),
                        }
                        .into(),
                        argument: constr_index_exposer(subject).into(),
                    }
                };

                arg_stack.push(term);
            }
            Air::Clause {
                tipo,
                subject_name,
                complex_clause,
                ..
            } => {
                // clause to compare
                let clause = arg_stack.pop().unwrap();

                // the body to be run if the clause matches
                let body = arg_stack.pop().unwrap();

                // the next branch in the when expression
                let mut term = arg_stack.pop().unwrap();

                let checker = if tipo.is_int() {
                    Term::Apply {
                        function: DefaultFunction::EqualsInteger.into(),
                        argument: Term::Var(Name {
                            text: subject_name,
                            unique: 0.into(),
                        })
                        .into(),
                    }
                } else if tipo.is_bytearray() {
                    Term::Apply {
                        function: DefaultFunction::EqualsByteString.into(),
                        argument: Term::Var(Name {
                            text: subject_name,
                            unique: 0.into(),
                        })
                        .into(),
                    }
                } else if tipo.is_bool() {
                    todo!()
                } else if tipo.is_string() {
                    Term::Apply {
                        function: DefaultFunction::EqualsString.into(),
                        argument: Term::Var(Name {
                            text: subject_name,
                            unique: 0.into(),
                        })
                        .into(),
                    }
                } else if tipo.is_list() {
                    unreachable!()
                } else {
                    Term::Apply {
                        function: DefaultFunction::EqualsInteger.into(),
                        argument: Term::Var(Name {
                            text: subject_name,
                            unique: 0.into(),
                        })
                        .into(),
                    }
                };

                if complex_clause {
                    term = Term::Apply {
                        function: Term::Lambda {
                            parameter_name: Name {
                                text: "__other_clauses_delayed".to_string(),
                                unique: 0.into(),
                            },
                            body: if_else(
                                Term::Apply {
                                    function: checker.into(),
                                    argument: clause.into(),
                                },
                                Term::Delay(body.into()),
                                Term::Var(Name {
                                    text: "__other_clauses_delayed".to_string(),
                                    unique: 0.into(),
                                }),
                            )
                            .force_wrap()
                            .into(),
                        }
                        .into(),
                        argument: Term::Delay(term.into()).into(),
                    }
                    .force_wrap()
                } else {
                    term = delayed_if_else(
                        Term::Apply {
                            function: checker.into(),
                            argument: clause.into(),
                        },
                        body,
                        term,
                    );
                }

                arg_stack.push(term);
            }
            Air::ListClause {
                tail_name,
                next_tail_name,
                inverse,
                complex_clause,
                ..
            } => {
                // discard to pop off
                let _ = arg_stack.pop().unwrap();

                // the body to be run if the clause matches
                // the next branch in the when expression
                let (body, mut term) = if inverse {
                    let term = arg_stack.pop().unwrap();
                    let body = arg_stack.pop().unwrap();

                    (body, term)
                } else {
                    let body = arg_stack.pop().unwrap();
                    let term = arg_stack.pop().unwrap();

                    (body, term)
                };

                let arg = if let Some(next_tail_name) = next_tail_name {
                    Term::Apply {
                        function: Term::Lambda {
                            parameter_name: Name {
                                text: next_tail_name,
                                unique: 0.into(),
                            },
                            body: term.into(),
                        }
                        .into(),
                        argument: Term::Apply {
                            function: Term::Builtin(DefaultFunction::TailList).force_wrap().into(),
                            argument: Term::Var(Name {
                                text: tail_name.clone(),
                                unique: 0.into(),
                            })
                            .into(),
                        }
                        .into(),
                    }
                } else {
                    term
                };

                if complex_clause {
                    term = choose_list(
                        Term::Var(Name {
                            text: tail_name,
                            unique: 0.into(),
                        }),
                        Term::Delay(body.into()),
                        Term::Var(Name {
                            text: "__other_clauses_delayed".to_string(),
                            unique: 0.into(),
                        }),
                    )
                    .force_wrap();

                    term = Term::Apply {
                        function: Term::Lambda {
                            parameter_name: Name {
                                text: "__other_clauses_delayed".into(),
                                unique: 0.into(),
                            },
                            body: term.into(),
                        }
                        .into(),
                        argument: Term::Delay(arg.into()).into(),
                    };
                } else {
                    term = delayed_choose_list(
                        Term::Var(Name {
                            text: tail_name,
                            unique: 0.into(),
                        }),
                        body,
                        arg,
                    );
                }

                arg_stack.push(term);
            }
            Air::ClauseGuard {
                subject_name, tipo, ..
            } => {
                let condition = arg_stack.pop().unwrap();

                let then = arg_stack.pop().unwrap();

                let checker = if tipo.is_int() {
                    Term::Apply {
                        function: DefaultFunction::EqualsInteger.into(),
                        argument: Term::Var(Name {
                            text: subject_name,
                            unique: 0.into(),
                        })
                        .into(),
                    }
                } else if tipo.is_bytearray() {
                    Term::Apply {
                        function: DefaultFunction::EqualsByteString.into(),
                        argument: Term::Var(Name {
                            text: subject_name,
                            unique: 0.into(),
                        })
                        .into(),
                    }
                } else if tipo.is_bool() {
                    todo!()
                } else if tipo.is_string() {
                    Term::Apply {
                        function: DefaultFunction::EqualsString.into(),
                        argument: Term::Var(Name {
                            text: subject_name,
                            unique: 0.into(),
                        })
                        .into(),
                    }
                } else if tipo.is_list() {
                    todo!()
                } else {
                    Term::Apply {
                        function: DefaultFunction::EqualsInteger.into(),
                        argument: constr_index_exposer(Term::Var(Name {
                            text: subject_name,
                            unique: 0.into(),
                        }))
                        .into(),
                    }
                };

                let term = if_else(
                    Term::Apply {
                        function: checker.into(),
                        argument: condition.into(),
                    },
                    Term::Delay(then.into()),
                    Term::Var(Name {
                        text: "__other_clauses_delayed".to_string(),
                        unique: 0.into(),
                    }),
                )
                .force_wrap();

                arg_stack.push(term);
            }
            Air::Finally { .. } => {
                let _clause = arg_stack.pop().unwrap();
            }
            Air::If { .. } => {
                let condition = arg_stack.pop().unwrap();
                let then = arg_stack.pop().unwrap();
                let mut term = arg_stack.pop().unwrap();

                term = delayed_if_else(condition, then, term);

                arg_stack.push(term);
            }
            Air::Constr { .. } => todo!(),
            Air::Fields { .. } => todo!(),
            Air::RecordAccess { index, tipo, .. } => {
                let constr = arg_stack.pop().unwrap();

                let mut term = Term::Apply {
                    function: Term::Apply {
                        function: Term::Var(Name {
                            text: CONSTR_GET_FIELD.to_string(),
                            unique: 0.into(),
                        })
                        .into(),
                        argument: Term::Apply {
                            function: Term::Var(Name {
                                text: CONSTR_FIELDS_EXPOSER.to_string(),
                                unique: 0.into(),
                            })
                            .into(),
                            argument: constr.into(),
                        }
                        .into(),
                    }
                    .into(),
                    argument: Term::Constant(UplcConstant::Integer(index.into())).into(),
                };

                term = convert_data_to_type(term, &tipo);

                arg_stack.push(term);
            }
            Air::FieldsExpose { indices, .. } => {
                self.needs_field_access = true;

                let constr_var = arg_stack.pop().unwrap();
                let mut body = arg_stack.pop().unwrap();

                let mut indices = indices.into_iter().rev();
                let highest = indices.next().unwrap();
                let mut id_list = vec![];

                for _ in 0..highest.0 {
                    id_list.push(self.id_gen.next());
                }

                let constr_name_lam = format!("__constr_fields_{}", self.id_gen.next());
                let highest_loop_index = highest.0 as i32 - 1;
                let last_prev_tail = Term::Var(Name {
                    text: if highest_loop_index == -1 {
                        constr_name_lam.clone()
                    } else {
                        format!(
                            "__tail_{}_{}",
                            highest_loop_index, id_list[highest_loop_index as usize]
                        )
                    },
                    unique: 0.into(),
                });

                body = Term::Apply {
                    function: Term::Lambda {
                        parameter_name: Name {
                            text: highest.1,
                            unique: 0.into(),
                        },
                        body: body.into(),
                    }
                    .into(),
                    argument: convert_data_to_type(
                        Term::Apply {
                            function: Term::Builtin(DefaultFunction::HeadList).force_wrap().into(),
                            argument: last_prev_tail.into(),
                        },
                        &highest.2,
                    )
                    .into(),
                };

                let mut current_field = None;
                for index in (0..highest.0).rev() {
                    let current_tail_index = index;
                    let previous_tail_index = if index == 0 { 0 } else { index - 1 };
                    let current_tail_id = id_list[index];
                    let previous_tail_id = if index == 0 { 0 } else { id_list[index - 1] };
                    if current_field.is_none() {
                        current_field = indices.next();
                    }

                    let prev_tail = if index == 0 {
                        Term::Var(Name {
                            text: constr_name_lam.clone(),
                            unique: 0.into(),
                        })
                    } else {
                        Term::Var(Name {
                            text: format!("__tail_{previous_tail_index}_{previous_tail_id}"),
                            unique: 0.into(),
                        })
                    };

                    if let Some(ref field) = current_field {
                        if field.0 == index {
                            let unwrapper = convert_data_to_type(
                                Term::Apply {
                                    function: Term::Builtin(DefaultFunction::HeadList)
                                        .force_wrap()
                                        .into(),
                                    argument: prev_tail.clone().into(),
                                },
                                &field.2,
                            );

                            body = Term::Apply {
                                function: Term::Lambda {
                                    parameter_name: Name {
                                        text: field.1.clone(),
                                        unique: 0.into(),
                                    },
                                    body: Term::Apply {
                                        function: Term::Lambda {
                                            parameter_name: Name {
                                                text: format!(
                                                    "__tail_{current_tail_index}_{current_tail_id}"
                                                ),
                                                unique: 0.into(),
                                            },
                                            body: body.into(),
                                        }
                                        .into(),
                                        argument: Term::Apply {
                                            function: Term::Builtin(DefaultFunction::TailList)
                                                .force_wrap()
                                                .into(),
                                            argument: prev_tail.into(),
                                        }
                                        .into(),
                                    }
                                    .into(),
                                }
                                .into(),
                                argument: unwrapper.into(),
                            };

                            current_field = None;
                        } else {
                            body = Term::Apply {
                                function: Term::Lambda {
                                    parameter_name: Name {
                                        text: format!(
                                            "__tail_{current_tail_index}_{current_tail_id}"
                                        ),
                                        unique: 0.into(),
                                    },
                                    body: body.into(),
                                }
                                .into(),
                                argument: Term::Apply {
                                    function: Term::Builtin(DefaultFunction::TailList)
                                        .force_wrap()
                                        .into(),
                                    argument: prev_tail.into(),
                                }
                                .into(),
                            }
                        }
                    } else {
                        body = Term::Apply {
                            function: Term::Lambda {
                                parameter_name: Name {
                                    text: format!("__tail_{current_tail_index}_{current_tail_id}"),
                                    unique: 0.into(),
                                },
                                body: body.into(),
                            }
                            .into(),
                            argument: Term::Apply {
                                function: Term::Builtin(DefaultFunction::TailList)
                                    .force_wrap()
                                    .into(),
                                argument: prev_tail.into(),
                            }
                            .into(),
                        }
                    }
                }

                body = Term::Apply {
                    function: Term::Lambda {
                        parameter_name: Name {
                            text: constr_name_lam,
                            unique: 0.into(),
                        },
                        body: body.into(),
                    }
                    .into(),
                    argument: Term::Apply {
                        function: Term::Var(Name {
                            text: CONSTR_FIELDS_EXPOSER.to_string(),
                            unique: 0.into(),
                        })
                        .into(),
                        argument: constr_var.into(),
                    }
                    .into(),
                };

                arg_stack.push(body);
            }
            Air::Tuple { tipo, count, .. } => {
                let mut args = vec![];

                for _ in 0..count {
                    let arg = arg_stack.pop().unwrap();
                    args.push(arg);
                }
                let mut constants = vec![];
                for arg in &args {
                    if let Term::Constant(c) = arg {
                        constants.push(c.clone())
                    }
                }

                let tuple_sub_types = tipo.get_inner_types();

                if constants.len() == args.len() {
                    let data_constants = convert_constants_to_data(constants);

                    if count == 2 {
                        let term = Term::Constant(UplcConstant::ProtoPair(
                            UplcType::Data,
                            UplcType::Data,
                            data_constants[0].clone().into(),
                            data_constants[1].clone().into(),
                        ));
                        arg_stack.push(term);
                    } else {
                        let term =
                            Term::Constant(UplcConstant::ProtoList(UplcType::Data, data_constants));
                        arg_stack.push(term);
                    }
                } else if count == 2 {
                    let term = Term::Apply {
                        function: Term::Apply {
                            function: DefaultFunction::MkPairData.into(),
                            argument: convert_type_to_data(args[0].clone(), &tuple_sub_types[0])
                                .into(),
                        }
                        .into(),
                        argument: convert_type_to_data(args[1].clone(), &tuple_sub_types[1]).into(),
                    };
                    arg_stack.push(term);
                } else {
                    let mut term = Term::Constant(UplcConstant::ProtoList(UplcType::Data, vec![]));
                    for (arg, tipo) in args.into_iter().zip(tuple_sub_types.into_iter()).rev() {
                        term = Term::Apply {
                            function: Term::Apply {
                                function: Term::Builtin(DefaultFunction::MkCons)
                                    .force_wrap()
                                    .into(),
                                argument: convert_type_to_data(arg, &tipo).into(),
                            }
                            .into(),
                            argument: term.into(),
                        };
                    }
                    arg_stack.push(term);
                }
            }
            Air::Todo { label, .. } => {
                let term = Term::Apply {
                    function: Term::Apply {
                        function: Term::Builtin(DefaultFunction::Trace).force_wrap().into(),
                        argument: Term::Constant(UplcConstant::String(
                            label.unwrap_or_else(|| "aiken::todo".to_string()),
                        ))
                        .into(),
                    }
                    .into(),
                    argument: Term::Delay(Term::Error.into()).into(),
                }
                .force_wrap();

                arg_stack.push(term);
            }
            Air::Record { .. } => todo!(),
            Air::RecordUpdate { .. } => todo!(),
            Air::UnOp { op, .. } => {
                let value = arg_stack.pop().unwrap();

                let term = match op {
                    UnOp::Not => if_else(
                        value,
                        Term::Constant(UplcConstant::Bool(false)),
                        Term::Constant(UplcConstant::Bool(true)),
                    ),
                    UnOp::Negate => Term::Apply {
                        function: Term::Apply {
                            function: Term::Builtin(DefaultFunction::SubtractInteger).into(),
                            argument: Term::Constant(UplcConstant::Integer(0)).into(),
                        }
                        .into(),
                        argument: value.into(),
                    },
                };

                arg_stack.push(term);
            }
            Air::TupleIndex { tipo, index, .. } => {
                let mut term = arg_stack.pop().unwrap();

                if matches!(tipo.get_uplc_type(), UplcType::Pair(_, _)) {
                    if index == 0 {
                        term = convert_data_to_type(
                            apply_wrap(
                                Term::Builtin(DefaultFunction::FstPair)
                                    .force_wrap()
                                    .force_wrap(),
                                term,
                            ),
                            &tipo.get_inner_types()[0],
                        );
                    } else {
                        term = convert_data_to_type(
                            apply_wrap(
                                Term::Builtin(DefaultFunction::SndPair)
                                    .force_wrap()
                                    .force_wrap(),
                                term,
                            ),
                            &tipo.get_inner_types()[1],
                        );
                    }
                } else {
                    self.needs_field_access = true;
                    term = apply_wrap(
                        apply_wrap(
                            Term::Var(Name {
                                text: CONSTR_GET_FIELD.to_string(),
                                unique: 0.into(),
                            }),
                            term,
                        ),
                        Term::Constant(UplcConstant::Integer(index as i128)),
                    );
                }

                arg_stack.push(term);
            }
            Air::TupleAccessor { tipo, names, .. } => {
                let inner_types = tipo.get_inner_types();
                let value = arg_stack.pop().unwrap();
                let mut term = arg_stack.pop().unwrap();
                let list_id = self.id_gen.next();

                if names.len() == 2 {
                    term = Term::Apply {
                        function: Term::Lambda {
                            parameter_name: Name {
                                text: format!("__tuple_{}", list_id),
                                unique: 0.into(),
                            },
                            body: Term::Apply {
                                function: Term::Lambda {
                                    parameter_name: Name {
                                        text: names[0].clone(),
                                        unique: 0.into(),
                                    },
                                    body: Term::Apply {
                                        function: Term::Lambda {
                                            parameter_name: Name {
                                                text: names[1].clone(),
                                                unique: 0.into(),
                                            },
                                            body: term.into(),
                                        }
                                        .into(),
                                        argument: convert_data_to_type(
                                            Term::Apply {
                                                function: Term::Builtin(DefaultFunction::SndPair)
                                                    .force_wrap()
                                                    .force_wrap()
                                                    .into(),
                                                argument: Term::Var(Name {
                                                    text: format!("__tuple_{}", list_id),
                                                    unique: 0.into(),
                                                })
                                                .into(),
                                            },
                                            &inner_types[1],
                                        )
                                        .into(),
                                    }
                                    .into(),
                                }
                                .into(),
                                argument: convert_data_to_type(
                                    Term::Apply {
                                        function: Term::Builtin(DefaultFunction::FstPair)
                                            .force_wrap()
                                            .force_wrap()
                                            .into(),
                                        argument: Term::Var(Name {
                                            text: format!("__tuple_{}", list_id),
                                            unique: 0.into(),
                                        })
                                        .into(),
                                    },
                                    &inner_types[0],
                                )
                                .into(),
                            }
                            .into(),
                        }
                        .into(),
                        argument: value.into(),
                    };
                } else {
                    let mut id_list = vec![];

                    for _ in 0..names.len() {
                        id_list.push(self.id_gen.next());
                    }

                    let current_index = 0;
                    let (first_name, names) = names.split_first().unwrap();

                    let head_list = convert_data_to_type(
                        Term::Apply {
                            function: Term::Force(Term::Builtin(DefaultFunction::HeadList).into())
                                .into(),
                            argument: Term::Var(Name {
                                text: format!("__tuple_{}", list_id),
                                unique: 0.into(),
                            })
                            .into(),
                        },
                        &tipo.get_inner_types()[0],
                    );

                    term = Term::Apply {
                        function: Term::Lambda {
                            parameter_name: Name {
                                text: format!("__tuple_{}", list_id),
                                unique: 0.into(),
                            },
                            body: Term::Apply {
                                function: Term::Lambda {
                                    parameter_name: Name {
                                        text: first_name.clone(),
                                        unique: 0.into(),
                                    },
                                    body: Term::Apply {
                                        function: list_access_to_uplc(
                                            names,
                                            &id_list,
                                            false,
                                            current_index,
                                            term,
                                            &tipo,
                                        )
                                        .into(),
                                        argument: Term::Apply {
                                            function: Term::Force(
                                                Term::Builtin(DefaultFunction::TailList).into(),
                                            )
                                            .into(),
                                            argument: Term::Var(Name {
                                                text: format!("__tuple_{}", list_id),
                                                unique: 0.into(),
                                            })
                                            .into(),
                                        }
                                        .into(),
                                    }
                                    .into(),
                                }
                                .into(),
                                argument: head_list.into(),
                            }
                            .into(),
                        }
                        .into(),
                        argument: value.into(),
                    };
                }

                arg_stack.push(term);
            }
            Air::Trace { text, .. } => {
                let term = arg_stack.pop().unwrap();

                let term = Term::Apply {
                    function: Term::Apply {
                        function: Term::Builtin(DefaultFunction::Trace).force_wrap().into(),
                        argument: Term::Constant(UplcConstant::String(
                            text.unwrap_or_else(|| "aiken::trace".to_string()),
                        ))
                        .into(),
                    }
                    .into(),
                    argument: term.into(),
                };

                arg_stack.push(term);
            }
            Air::ErrorTerm { label, .. } => {
                if let Some(label) = label {
                    let term = Term::Apply {
                        function: Term::Apply {
                            function: Term::Builtin(DefaultFunction::Trace).force_wrap().into(),
                            argument: Term::Constant(UplcConstant::String(label)).into(),
                        }
                        .into(),
                        argument: Term::Delay(Term::Error.into()).into(),
                    }
                    .force_wrap();

                    arg_stack.push(term);
                } else {
                    arg_stack.push(Term::Error)
                }
            }
            Air::TupleClause {
                tipo,
                indices,
                subject_name,
                complex_clause,
                ..
            } => {
                let mut term = arg_stack.pop().unwrap();

                let tuple_types = tipo.get_inner_types();

                if tuple_types.len() == 2 {
                    for (index, name) in indices.iter() {
                        if *index == 0 {
                            term = apply_wrap(
                                Term::Lambda {
                                    parameter_name: Name {
                                        text: name.clone(),
                                        unique: 0.into(),
                                    },
                                    body: term.into(),
                                },
                                convert_data_to_type(
                                    apply_wrap(
                                        Term::Builtin(DefaultFunction::FstPair)
                                            .force_wrap()
                                            .force_wrap(),
                                        Term::Var(Name {
                                            text: subject_name.clone(),
                                            unique: 0.into(),
                                        }),
                                    ),
                                    &tuple_types[*index].clone(),
                                ),
                            );
                        } else {
                            term = apply_wrap(
                                Term::Lambda {
                                    parameter_name: Name {
                                        text: name.clone(),
                                        unique: 0.into(),
                                    },
                                    body: term.into(),
                                },
                                convert_data_to_type(
                                    apply_wrap(
                                        Term::Builtin(DefaultFunction::SndPair)
                                            .force_wrap()
                                            .force_wrap(),
                                        Term::Var(Name {
                                            text: subject_name.clone(),
                                            unique: 0.into(),
                                        }),
                                    ),
                                    &tuple_types[*index].clone(),
                                ),
                            );
                        }
                    }
                } else {
                    for (index, name) in indices.iter() {
                        term = apply_wrap(
                            Term::Lambda {
                                parameter_name: Name {
                                    text: name.clone(),
                                    unique: 0.into(),
                                },
                                body: term.into(),
                            },
                            convert_data_to_type(
                                apply_wrap(
                                    Term::Builtin(DefaultFunction::HeadList).force_wrap(),
                                    repeat_tail_list(
                                        Term::Var(Name {
                                            text: subject_name.clone(),
                                            unique: 0.into(),
                                        }),
                                        *index,
                                    ),
                                ),
                                &tuple_types[*index].clone(),
                            ),
                        );
                    }
                }

                if complex_clause {
                    let next_clause = arg_stack.pop().unwrap();

                    term = apply_wrap(
                        Term::Lambda {
                            parameter_name: Name {
                                text: "__other_clauses_delayed".to_string(),
                                unique: 0.into(),
                            },
                            body: term.into(),
                        },
                        Term::Delay(next_clause.into()),
                    )
                }
                arg_stack.push(term);
            }
        }
    }
}
