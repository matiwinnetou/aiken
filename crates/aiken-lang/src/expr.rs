use std::sync::Arc;

use vec1::Vec1;

use crate::{
    ast::{
        Annotation, Arg, AssignmentKind, BinOp, CallArg, Clause, DefinitionLocation, IfBranch,
        Pattern, RecordUpdateSpread, Span, TodoKind, TypedRecordUpdateArg, UnOp,
        UntypedRecordUpdateArg,
    },
    builtins::void,
    tipo::{ModuleValueConstructor, PatternConstructor, Type, ValueConstructor},
};

#[derive(Debug, Clone)]
pub enum TypedExpr {
    Int {
        location: Span,
        tipo: Arc<Type>,
        value: String,
    },

    String {
        location: Span,
        tipo: Arc<Type>,
        value: String,
    },

    ByteArray {
        location: Span,
        tipo: Arc<Type>,
        bytes: Vec<u8>,
    },

    Sequence {
        location: Span,
        expressions: Vec<Self>,
    },

    /// A chain of pipe expressions.
    /// By this point the type checker has expanded it into a series of
    /// assignments and function calls, but we still have a Pipeline AST node as
    /// even though it is identical to `Sequence` we want to use different
    /// locations when showing it in error messages, etc.
    Pipeline {
        location: Span,
        expressions: Vec<Self>,
    },

    Var {
        location: Span,
        constructor: ValueConstructor,
        name: String,
    },

    Fn {
        location: Span,
        tipo: Arc<Type>,
        is_capture: bool,
        args: Vec<Arg<Arc<Type>>>,
        body: Box<Self>,
        return_annotation: Option<Annotation>,
    },

    List {
        location: Span,
        tipo: Arc<Type>,
        elements: Vec<Self>,
        tail: Option<Box<Self>>,
    },

    Call {
        location: Span,
        tipo: Arc<Type>,
        fun: Box<Self>,
        args: Vec<CallArg<Self>>,
    },

    BinOp {
        location: Span,
        tipo: Arc<Type>,
        name: BinOp,
        left: Box<Self>,
        right: Box<Self>,
    },

    Assignment {
        location: Span,
        tipo: Arc<Type>,
        value: Box<Self>,
        pattern: Pattern<PatternConstructor, Arc<Type>>,
        kind: AssignmentKind,
    },

    Trace {
        location: Span,
        tipo: Arc<Type>,
        then: Box<Self>,
        text: Option<String>,
    },

    When {
        location: Span,
        tipo: Arc<Type>,
        subjects: Vec<Self>,
        clauses: Vec<Clause<Self, PatternConstructor, Arc<Type>, String>>,
    },

    If {
        location: Span,
        branches: Vec1<IfBranch<Self>>,
        final_else: Box<Self>,
        tipo: Arc<Type>,
    },

    RecordAccess {
        location: Span,
        tipo: Arc<Type>,
        label: String,
        index: u64,
        record: Box<Self>,
    },

    ModuleSelect {
        location: Span,
        tipo: Arc<Type>,
        label: String,
        module_name: String,
        module_alias: String,
        constructor: ModuleValueConstructor,
    },

    Tuple {
        location: Span,
        tipo: Arc<Type>,
        elems: Vec<Self>,
    },

    TupleIndex {
        location: Span,
        tipo: Arc<Type>,
        index: usize,
        tuple: Box<Self>,
    },

    Todo {
        location: Span,
        label: Option<String>,
        tipo: Arc<Type>,
    },

    ErrorTerm {
        location: Span,
        tipo: Arc<Type>,
        label: Option<String>,
    },

    RecordUpdate {
        location: Span,
        tipo: Arc<Type>,
        spread: Box<Self>,
        args: Vec<TypedRecordUpdateArg>,
    },

    UnOp {
        location: Span,
        value: Box<Self>,
        tipo: Arc<Type>,
        op: UnOp,
    },
}

impl TypedExpr {
    pub fn tipo(&self) -> Arc<Type> {
        match self {
            Self::Var { constructor, .. } => constructor.tipo.clone(),
            Self::Trace { then, .. } => then.tipo(),
            Self::Fn { tipo, .. }
            | Self::Int { tipo, .. }
            | Self::Todo { tipo, .. }
            | Self::ErrorTerm { tipo, .. }
            | Self::When { tipo, .. }
            | Self::List { tipo, .. }
            | Self::Call { tipo, .. }
            | Self::If { tipo, .. }
            | Self::UnOp { tipo, .. }
            | Self::BinOp { tipo, .. }
            | Self::Tuple { tipo, .. }
            | Self::String { tipo, .. }
            | Self::ByteArray { tipo, .. }
            | Self::TupleIndex { tipo, .. }
            | Self::Assignment { tipo, .. }
            | Self::ModuleSelect { tipo, .. }
            | Self::RecordAccess { tipo, .. }
            | Self::RecordUpdate { tipo, .. } => tipo.clone(),
            Self::Pipeline { expressions, .. } | Self::Sequence { expressions, .. } => {
                expressions.last().map(TypedExpr::tipo).unwrap_or_else(void)
            }
        }
    }

    pub fn is_literal(&self) -> bool {
        matches!(
            self,
            Self::Int { .. }
                | Self::List { .. }
                | Self::Tuple { .. }
                | Self::String { .. }
                | Self::ByteArray { .. }
        )
    }

    /// Returns `true` if the typed expr is [`Assignment`].
    pub fn is_assignment(&self) -> bool {
        matches!(self, Self::Assignment { .. })
    }

    pub fn definition_location(&self) -> Option<DefinitionLocation<'_>> {
        match self {
            TypedExpr::Fn { .. }
            | TypedExpr::Int { .. }
            | TypedExpr::Trace { .. }
            | TypedExpr::List { .. }
            | TypedExpr::Call { .. }
            | TypedExpr::When { .. }
            | TypedExpr::Todo { .. }
            | TypedExpr::ErrorTerm { .. }
            | TypedExpr::BinOp { .. }
            | TypedExpr::Tuple { .. }
            | TypedExpr::UnOp { .. }
            | TypedExpr::String { .. }
            | TypedExpr::Sequence { .. }
            | TypedExpr::Pipeline { .. }
            | TypedExpr::ByteArray { .. }
            | TypedExpr::Assignment { .. }
            | TypedExpr::TupleIndex { .. }
            | TypedExpr::RecordAccess { .. } => None,
            TypedExpr::If { .. } => None,

            // TODO: test
            // TODO: definition
            TypedExpr::RecordUpdate { .. } => None,

            // TODO: test
            TypedExpr::ModuleSelect {
                module_name,
                constructor,
                ..
            } => Some(DefinitionLocation {
                module: Some(module_name.as_str()),
                span: constructor.location(),
            }),

            // TODO: test
            TypedExpr::Var { constructor, .. } => Some(constructor.definition_location()),
        }
    }

    pub fn type_defining_location(&self) -> Span {
        match self {
            Self::Fn { location, .. }
            | Self::Int { location, .. }
            | Self::Var { location, .. }
            | Self::Trace { location, .. }
            | Self::Todo { location, .. }
            | Self::ErrorTerm { location, .. }
            | Self::When { location, .. }
            | Self::Call { location, .. }
            | Self::List { location, .. }
            | Self::BinOp { location, .. }
            | Self::Tuple { location, .. }
            | Self::String { location, .. }
            | Self::UnOp { location, .. }
            | Self::Pipeline { location, .. }
            | Self::ByteArray { location, .. }
            | Self::Assignment { location, .. }
            | Self::TupleIndex { location, .. }
            | Self::ModuleSelect { location, .. }
            | Self::RecordAccess { location, .. }
            | Self::RecordUpdate { location, .. } => *location,

            Self::If { branches, .. } => branches.first().body.type_defining_location(),

            Self::Sequence {
                expressions,
                location,
                ..
            } => expressions
                .last()
                .map(TypedExpr::location)
                .unwrap_or(*location),
        }
    }

    pub fn location(&self) -> Span {
        match self {
            Self::Fn { location, .. }
            | Self::Int { location, .. }
            | Self::Trace { location, .. }
            | Self::Var { location, .. }
            | Self::Todo { location, .. }
            | Self::ErrorTerm { location, .. }
            | Self::When { location, .. }
            | Self::Call { location, .. }
            | Self::If { location, .. }
            | Self::List { location, .. }
            | Self::BinOp { location, .. }
            | Self::Tuple { location, .. }
            | Self::String { location, .. }
            | Self::UnOp { location, .. }
            | Self::Sequence { location, .. }
            | Self::Pipeline { location, .. }
            | Self::ByteArray { location, .. }
            | Self::Assignment { location, .. }
            | Self::TupleIndex { location, .. }
            | Self::ModuleSelect { location, .. }
            | Self::RecordAccess { location, .. }
            | Self::RecordUpdate { location, .. } => *location,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum UntypedExpr {
    Int {
        location: Span,
        value: String,
    },

    String {
        location: Span,
        value: String,
    },

    Sequence {
        location: Span,
        expressions: Vec<Self>,
    },

    Var {
        location: Span,
        name: String,
    },

    Fn {
        location: Span,
        is_capture: bool,
        arguments: Vec<Arg<()>>,
        body: Box<Self>,
        return_annotation: Option<Annotation>,
    },

    List {
        location: Span,
        elements: Vec<Self>,
        tail: Option<Box<Self>>,
    },

    Call {
        arguments: Vec<CallArg<Self>>,
        fun: Box<Self>,
        location: Span,
    },

    BinOp {
        location: Span,
        name: BinOp,
        left: Box<Self>,
        right: Box<Self>,
    },

    ByteArray {
        location: Span,
        bytes: Vec<u8>,
    },

    PipeLine {
        expressions: Vec1<Self>,
    },

    Assignment {
        location: Span,
        value: Box<Self>,
        pattern: Pattern<(), ()>,
        kind: AssignmentKind,
        annotation: Option<Annotation>,
    },

    Trace {
        location: Span,
        then: Box<Self>,
        text: Option<String>,
    },

    When {
        location: Span,
        subjects: Vec<Self>,
        clauses: Vec<Clause<Self, (), (), ()>>,
    },

    If {
        location: Span,
        branches: Vec1<IfBranch<Self>>,
        final_else: Box<Self>,
    },

    FieldAccess {
        location: Span,
        label: String,
        container: Box<Self>,
    },

    Tuple {
        location: Span,
        elems: Vec<Self>,
    },

    TupleIndex {
        location: Span,
        index: usize,
        tuple: Box<Self>,
    },

    Todo {
        kind: TodoKind,
        location: Span,
        label: Option<String>,
    },

    ErrorTerm {
        location: Span,
        label: Option<String>,
    },

    RecordUpdate {
        location: Span,
        constructor: Box<Self>,
        spread: RecordUpdateSpread,
        arguments: Vec<UntypedRecordUpdateArg>,
    },

    UnOp {
        op: UnOp,
        location: Span,
        value: Box<Self>,
    },
}

impl UntypedExpr {
    pub fn append_in_sequence(self, next: Self) -> Self {
        let location = Span {
            start: self.location().start,
            end: next.location().end,
        };

        match (self.clone(), next.clone()) {
            (
                Self::Sequence {
                    expressions: mut current_expressions,
                    ..
                },
                Self::Sequence {
                    expressions: mut next_expressions,
                    ..
                },
            ) => {
                current_expressions.append(&mut next_expressions);

                Self::Sequence {
                    location,
                    expressions: current_expressions,
                }
            }
            (
                _,
                Self::Sequence {
                    expressions: mut next_expressions,
                    ..
                },
            ) => {
                let mut current_expressions = vec![self];

                current_expressions.append(&mut next_expressions);

                Self::Sequence {
                    location,
                    expressions: current_expressions,
                }
            }

            (_, _) => Self::Sequence {
                location,
                expressions: vec![self, next],
            },
        }
    }

    pub fn location(&self) -> Span {
        match self {
            Self::PipeLine { expressions, .. } => expressions.last().location(),
            Self::Trace { then, .. } => then.location(),
            Self::Fn { location, .. }
            | Self::Var { location, .. }
            | Self::Int { location, .. }
            | Self::Todo { location, .. }
            | Self::ErrorTerm { location, .. }
            | Self::When { location, .. }
            | Self::Call { location, .. }
            | Self::List { location, .. }
            | Self::ByteArray { location, .. }
            | Self::BinOp { location, .. }
            | Self::Tuple { location, .. }
            | Self::String { location, .. }
            | Self::Assignment { location, .. }
            | Self::TupleIndex { location, .. }
            | Self::FieldAccess { location, .. }
            | Self::RecordUpdate { location, .. }
            | Self::UnOp { location, .. }
            | Self::If { location, .. } => *location,
            Self::Sequence {
                location,
                expressions,
                ..
            } => expressions.last().map(Self::location).unwrap_or(*location),
        }
    }

    pub fn start_byte_index(&self) -> usize {
        match self {
            Self::Sequence {
                expressions,
                location,
                ..
            } => expressions
                .first()
                .map(|e| e.start_byte_index())
                .unwrap_or(location.start),
            Self::PipeLine { expressions, .. } => expressions.first().start_byte_index(),
            Self::Trace { location, .. } | Self::Assignment { location, .. } => location.start,
            _ => self.location().start,
        }
    }

    pub fn binop_precedence(&self) -> u8 {
        match self {
            Self::BinOp { name, .. } => name.precedence(),
            Self::PipeLine { .. } => 5,
            _ => std::u8::MAX,
        }
    }

    pub fn is_simple_constant(&self) -> bool {
        matches!(
            self,
            Self::String { .. } | Self::Int { .. } | Self::ByteArray { .. }
        )
    }
}
