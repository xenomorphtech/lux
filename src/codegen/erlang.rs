/// Core Erlang AST representation

#[derive(Debug, Clone)]
pub struct CoreModule {
    pub name: String,
    pub exports: Vec<(String, usize)>, // (name, arity)
    pub functions: Vec<CoreFunDef>,
}

#[derive(Debug, Clone)]
pub struct CoreFunDef {
    pub name: String,
    pub arity: usize,
    pub params: Vec<String>,
    pub body: CoreExpr,
}

#[derive(Debug, Clone)]
pub struct CoreBinarySegment {
    pub value: CoreExpr,
    pub size: CoreBinarySize,
    pub kind: CoreBinaryKind,
}

#[derive(Debug, Clone)]
pub enum CoreBinarySize {
    Bits(u32),
    All,
}

#[derive(Debug, Clone, Copy)]
pub enum CoreBinaryKind {
    Integer,
    BigInteger,
    Binary,
    Utf8,
}

#[derive(Debug, Clone)]
pub struct CoreBinaryPatternSegment {
    pub pattern: CorePattern,
    pub size: CoreBinarySize,
    pub kind: CoreBinaryKind,
}

#[derive(Debug, Clone)]
pub enum CoreExpr {
    Lit(CoreLit),
    Var(String),
    Tuple(Vec<CoreExpr>),
    List(Vec<CoreExpr>, Box<CoreExpr>), // elements, tail
    Cons(Box<CoreExpr>, Box<CoreExpr>),
    Map(Vec<(CoreExpr, CoreExpr)>), // #{k => v, ...}
    Binary(Vec<CoreBinarySegment>), // <<E1, E2, ...>>

    // Local function reference: 'name'/arity
    LocalFunRef(String, usize),

    // Remote function reference: 'module':'name'/arity
    RemoteFunRef(String, String, usize),

    // Function application
    Apply(Box<CoreExpr>, Vec<CoreExpr>),
    Call(String, String, Vec<CoreExpr>), // module:function(args)

    // Let binding
    Let(Vec<(String, CoreExpr)>, Box<CoreExpr>),

    // Case expression (pattern matching)
    Case(Box<CoreExpr>, Vec<CoreClause>),

    // Receive - generates a letrec with primops
    Receive {
        clauses: Vec<CoreClause>,
        timeout: Option<(Box<CoreExpr>, Box<CoreExpr>)>, // (timeout_ms, timeout_body)
    },

    // Lambda
    Fun(Vec<String>, Box<CoreExpr>),

    // Primops
    Primop(String, Vec<CoreExpr>), // e.g., 'send', 'spawn'

    // Sequence
    Seq(Box<CoreExpr>, Box<CoreExpr>),

    // Try-catch
    Try {
        body: Box<CoreExpr>,
        vars: Vec<String>,
        handler: Box<CoreExpr>,
        evars: Vec<String>,
        catch: Box<CoreExpr>,
    },
}

#[derive(Debug, Clone)]
pub enum CoreLit {
    Int(i64),
    Float(f64),
    Atom(String),
    String(String),
    Nil,
}

#[derive(Debug, Clone)]
pub struct CoreClause {
    pub patterns: Vec<CorePattern>,
    pub guard: CoreExpr,
    pub body: CoreExpr,
}

#[derive(Debug, Clone)]
pub enum CorePattern {
    Lit(CoreLit),
    Var(String),
    Tuple(Vec<CorePattern>),
    Cons(Box<CorePattern>, Box<CorePattern>),
    Nil,
    Binary(Vec<CoreBinaryPatternSegment>),
    Alias(String, Box<CorePattern>),
}

impl CoreLit {
    pub fn atom(s: impl Into<String>) -> Self {
        CoreLit::Atom(s.into())
    }

    pub fn int(n: i64) -> Self {
        CoreLit::Int(n)
    }
}

impl CoreExpr {
    pub fn lit(l: CoreLit) -> Self {
        CoreExpr::Lit(l)
    }

    pub fn var(s: impl Into<String>) -> Self {
        CoreExpr::Var(s.into())
    }

    pub fn atom(s: impl Into<String>) -> Self {
        CoreExpr::Lit(CoreLit::Atom(s.into()))
    }

    pub fn call(module: impl Into<String>, func: impl Into<String>, args: Vec<CoreExpr>) -> Self {
        CoreExpr::Call(module.into(), func.into(), args)
    }
}
