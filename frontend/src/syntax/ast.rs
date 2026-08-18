#[allow(unused_imports)]
use crate::prelude::*;
use crate::syntax::span::Span;

pub type Ident = String;

/// A complete module
#[derive(Debug, Clone)]
pub struct Module {
    pub name: Option<Ident>,
    pub items: Vec<Item>,
    pub span: Span,
}

/// Top-level items
#[derive(Debug, Clone)]
pub enum Item {
    Function(Function),
    TypeAlias(TypeAlias),
    Enum(EnumDef),
    Struct(StructDef),
    Extern(ExternBlock),
    Use(UseDecl),
}

/// Use declaration: use module or use module::{item1, item2}
#[derive(Debug, Clone)]
pub struct UseDecl {
    pub module: Ident,
    pub items: Option<Vec<Ident>>, // None = import all, Some = specific items
    pub span: Span,
}

/// Struct definition
#[derive(Debug, Clone)]
pub struct StructDef {
    pub name: Ident,
    pub type_params: Vec<Ident>,
    pub fields: Vec<StructField>,
    pub span: Span,
}

/// Struct field
#[derive(Debug, Clone)]
pub struct StructField {
    pub name: Ident,
    pub ty: TypeExpr,
    pub span: Span,
}

/// Function definition
#[derive(Debug, Clone)]
pub struct Function {
    pub name: Ident,
    pub type_params: Vec<Ident>,
    pub params: Vec<Param>,
    pub return_type: Option<TypeExpr>,
    pub body: Expr,
    pub span: Span,
}

/// Function parameter
#[derive(Debug, Clone)]
pub struct Param {
    pub name: Ident,
    pub ty: Option<TypeExpr>,
    pub span: Span,
}

/// Type alias: type Foo = Bar
#[derive(Debug, Clone)]
pub struct TypeAlias {
    pub name: Ident,
    pub type_params: Vec<Ident>,
    pub ty: TypeExpr,
    pub span: Span,
}

/// Enum definition
#[derive(Debug, Clone)]
pub struct EnumDef {
    pub name: Ident,
    pub type_params: Vec<Ident>,
    pub variants: Vec<Variant>,
    pub span: Span,
}

/// Enum variant
#[derive(Debug, Clone)]
pub struct Variant {
    pub name: Ident,
    pub fields: Vec<TypeExpr>,
    pub span: Span,
}

/// External function declarations
#[derive(Debug, Clone)]
pub struct ExternBlock {
    pub abi: String,
    pub decls: Vec<ExternFn>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ExternFn {
    pub module: Ident,
    pub name: Ident,
    pub type_params: Vec<Ident>,
    pub params: Vec<TypeExpr>,
    pub return_type: TypeExpr,
    pub span: Span,
}

/// Type expressions
#[derive(Debug, Clone)]
pub enum TypeExpr {
    /// Named type: Int, Option<T>
    Named(Ident, Vec<TypeExpr>, Span),
    /// Tuple: (A, B, C)
    Tuple(Vec<TypeExpr>, Span),
    /// Function: fn(A, B) -> C
    Function(Vec<TypeExpr>, Box<TypeExpr>, Span),
    /// Record: { x: Int, y: Int }
    Record(Vec<(Ident, TypeExpr)>, Span),
    /// Unit: ()
    Unit(Span),
}

impl TypeExpr {
    pub fn span(&self) -> Span {
        match self {
            TypeExpr::Named(_, _, s) => *s,
            TypeExpr::Tuple(_, s) => *s,
            TypeExpr::Function(_, _, s) => *s,
            TypeExpr::Record(_, s) => *s,
            TypeExpr::Unit(s) => *s,
        }
    }
}

/// Part of an interpolated string
#[derive(Debug, Clone)]
pub enum InterpolatedPart {
    Literal(String),
    Expr(Box<Expr>),
}

/// Generator in a list comprehension: pattern <- source
#[derive(Debug, Clone)]
pub struct Generator {
    pub pattern: Pattern,
    pub source: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum BinarySegmentSpecifier {
    Integer,
    BigInteger,
    Binary,
    Utf8,
}

#[derive(Debug, Clone)]
pub struct BitStringSegment {
    pub value: Expr,
    pub size: Option<u32>,
    pub specifier: BinarySegmentSpecifier,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct BitStringPatternSegment {
    pub value: Pattern,
    pub size: Option<u32>,
    pub specifier: BinarySegmentSpecifier,
    pub span: Span,
}

/// Expressions
#[derive(Debug, Clone)]
pub enum Expr {
    // Literals
    Int(i64, Span),
    Float(f64, Span),
    Char(char, Span),
    String(String, Span),
    InterpolatedString(Vec<InterpolatedPart>, Span), // "hello ${name}!"
    Bool(bool, Span),
    Atom(String, Span),
    Unit(Span),

    // Variable reference
    Var(Ident, Span),

    // Compound literals
    Tuple(Vec<Expr>, Span),
    List(Vec<Expr>, Option<Box<Expr>>, Span), // [a, b | tail]
    ListComp {
        expr: Box<Expr>,            // the expression to evaluate
        generators: Vec<Generator>, // for x in list, y in list2
        filters: Vec<Expr>,         // if conditions
        span: Span,
    },
    Range(Box<Expr>, Box<Expr>, bool, Span), // start..end or start..=end (bool is inclusive)
    Map(Vec<(Expr, Expr)>, Span),            // %{key => value, ...}
    Record(Vec<(Ident, Expr)>, Span),
    StructInit(Ident, Vec<(Ident, Expr)>, Span), // Point { x: 1, y: 2 }
    BitString(Vec<BitStringSegment>, Span),      // <<1, 2, 3>> or <<"hello">>

    // Operations
    Binary(Box<Expr>, BinOp, Box<Expr>, Span),
    Unary(UnaryOp, Box<Expr>, Span),

    // Control flow
    If(Box<Expr>, Box<Expr>, Option<Box<Expr>>, Span),
    Cond(Vec<(Expr, Expr)>, Span), // cond { cond1 => body1, cond2 => body2, ... }
    Match(Box<Expr>, Vec<MatchArm>, Span),
    Block(Vec<Stmt>, Option<Box<Expr>>, Span),

    // Functions
    Call(Box<Expr>, Vec<Expr>, Span),
    MethodCall(Box<Expr>, Ident, Vec<Expr>, Span),
    Lambda(Vec<Param>, Option<TypeExpr>, Box<Expr>, Span),

    // Field/variant access
    Field(Box<Expr>, Ident, Span),
    Index(Box<Expr>, Box<Expr>, Span), // expr[key] - map/list access
    Path(Vec<Ident>, Span),            // Foo::Bar::Baz

    // Process primitives
    Spawn(Box<Expr>, Span),
    Send(Box<Expr>, Box<Expr>, Span),
    Receive {
        arms: Vec<MatchArm>,
        timeout: Option<(Box<Expr>, Box<Expr>)>, // (timeout_ms, timeout_body)
        span: Span,
    },
    SelfPid(Span),

    // Return
    Return(Option<Box<Expr>>, Span),

    // Try/Catch
    Try {
        body: Box<Expr>,
        catch_arms: Vec<CatchArm>,
        span: Span,
    },
}

/// A catch clause: class:pattern => body
#[derive(Debug, Clone)]
pub struct CatchArm {
    pub class: Option<Ident>, // error, throw, exit (or None for any)
    pub pattern: Pattern,
    pub body: Expr,
    pub span: Span,
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Int(_, s) => *s,
            Expr::Float(_, s) => *s,
            Expr::Char(_, s) => *s,
            Expr::String(_, s) => *s,
            Expr::InterpolatedString(_, s) => *s,
            Expr::Bool(_, s) => *s,
            Expr::Atom(_, s) => *s,
            Expr::Unit(s) => *s,
            Expr::Var(_, s) => *s,
            Expr::Tuple(_, s) => *s,
            Expr::List(_, _, s) => *s,
            Expr::ListComp { span, .. } => *span,
            Expr::Range(_, _, _, s) => *s,
            Expr::Map(_, s) => *s,
            Expr::Record(_, s) => *s,
            Expr::StructInit(_, _, s) => *s,
            Expr::BitString(_, s) => *s,
            Expr::Binary(_, _, _, s) => *s,
            Expr::Unary(_, _, s) => *s,
            Expr::If(_, _, _, s) => *s,
            Expr::Cond(_, s) => *s,
            Expr::Match(_, _, s) => *s,
            Expr::Block(_, _, s) => *s,
            Expr::Call(_, _, s) => *s,
            Expr::MethodCall(_, _, _, s) => *s,
            Expr::Lambda(_, _, _, s) => *s,
            Expr::Field(_, _, s) => *s,
            Expr::Index(_, _, s) => *s,
            Expr::Path(_, s) => *s,
            Expr::Spawn(_, s) => *s,
            Expr::Send(_, _, s) => *s,
            Expr::Receive { span, .. } => *span,
            Expr::SelfPid(s) => *s,
            Expr::Return(_, s) => *s,
            Expr::Try { span, .. } => *span,
        }
    }
}

/// Binary operators
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Concat, // ++ (list/string concatenation)
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
    Pipe, // |>
}

/// Unary operators
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UnaryOp {
    Neg,
    Not,
}

/// Match arm
#[derive(Debug, Clone)]
pub struct MatchArm {
    pub pattern: Pattern,
    pub guard: Option<Box<Expr>>,
    pub body: Expr,
    pub span: Span,
}

/// Patterns for matching
#[derive(Debug, Clone)]
pub enum Pattern {
    Wildcard(Span),
    Var(Ident, Span),
    Int(i64, Span),
    Float(f64, Span),
    Char(char, Span),
    String(String, Span),
    Bool(bool, Span),
    Atom(String, Span),
    Tuple(Vec<Pattern>, Span),
    List(Vec<Pattern>, Option<Box<Pattern>>, Span),
    Constructor(Vec<Ident>, Vec<Pattern>, Span), // Option::Some(x)
    Record(Vec<(Ident, Pattern)>, Span),
    BitString(Vec<BitStringPatternSegment>, Span),
    Or(Box<Pattern>, Box<Pattern>, Span),
}

impl Pattern {
    pub fn span(&self) -> Span {
        match self {
            Pattern::Wildcard(s) => *s,
            Pattern::Var(_, s) => *s,
            Pattern::Int(_, s) => *s,
            Pattern::Float(_, s) => *s,
            Pattern::Char(_, s) => *s,
            Pattern::String(_, s) => *s,
            Pattern::Bool(_, s) => *s,
            Pattern::Atom(_, s) => *s,
            Pattern::Tuple(_, s) => *s,
            Pattern::List(_, _, s) => *s,
            Pattern::Constructor(_, _, s) => *s,
            Pattern::Record(_, s) => *s,
            Pattern::BitString(_, s) => *s,
            Pattern::Or(_, _, s) => *s,
        }
    }
}

/// Statements (only in blocks)
#[derive(Debug, Clone)]
pub enum Stmt {
    Let(Pattern, Option<TypeExpr>, Expr, Span),
    Expr(Expr),
}
