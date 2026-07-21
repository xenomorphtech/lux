use crate::syntax::span::Span;

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

impl Token {
    pub fn new(kind: TokenKind, span: Span) -> Self {
        Token { kind, span }
    }
}

/// Parts of an interpolated string
#[derive(Debug, Clone, PartialEq)]
pub enum StringPart {
    Literal(String),
    Expr(Vec<Token>), // tokens to be parsed as expression
}

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // Literals
    Int(i64),
    Float(f64),
    Char(char),
    String(String),
    InterpolatedString(Vec<StringPart>), // "hello ${name}!"
    Atom(String),                        // :name
    Bool(bool),

    // Identifiers
    Ident(String),     // lowercase start
    TypeIdent(String), // uppercase start

    // Keywords
    Fn,
    Let,
    If,
    Unless,
    Cond,
    Else,
    Match,
    Enum,
    Struct,
    Type,
    Mod,
    Use,
    Spawn,
    Send,
    Receive,
    After,  // after (for receive timeout)
    For,    // for (list comprehension)
    In,     // in (list comprehension)
    Try,    // try
    Catch,  // catch
    SelfKw, // self
    Extern,
    Return,
    Pub,

    // Operators
    Plus,      // +
    PlusPlus,  // ++ (list/string concat)
    Minus,     // -
    Star,      // *
    Slash,     // /
    Percent,   // %
    Eq,        // =
    EqEq,      // ==
    NotEq,     // !=
    Lt,        // <
    LtEq,      // <=
    LtLt,      // << (binary start)
    Gt,        // >
    GtEq,      // >=
    GtGt,      // >> (binary end)
    And,       // &&
    Or,        // ||
    Not,       // !
    Arrow,     // ->
    FatArrow,  // =>
    Pipe,      // |
    PipeRight, // |>
    HashBrace, // %{ (map literal start)
    Colon2,    // ::

    // Delimiters
    LParen,   // (
    RParen,   // )
    LBrace,   // {
    RBrace,   // }
    LBracket, // [
    RBracket, // ]
    Comma,    // ,
    Colon,    // :
    Semi,     // ;
    Dot,      // .
    DotDot,   // .. (range exclusive)
    DotDotEq, // ..= (range inclusive)

    // Special
    Eof,
    Error(String),
    Newline,
}

impl TokenKind {
    pub fn keyword(s: &str) -> Option<TokenKind> {
        match s {
            "fn" => Some(TokenKind::Fn),
            "let" => Some(TokenKind::Let),
            "if" => Some(TokenKind::If),
            "unless" => Some(TokenKind::Unless),
            "cond" => Some(TokenKind::Cond),
            "else" => Some(TokenKind::Else),
            "match" => Some(TokenKind::Match),
            "enum" => Some(TokenKind::Enum),
            "struct" => Some(TokenKind::Struct),
            "type" => Some(TokenKind::Type),
            "mod" => Some(TokenKind::Mod),
            "use" => Some(TokenKind::Use),
            "spawn" => Some(TokenKind::Spawn),
            "send" => Some(TokenKind::Send),
            "receive" => Some(TokenKind::Receive),
            "after" => Some(TokenKind::After),
            "for" => Some(TokenKind::For),
            "in" => Some(TokenKind::In),
            "try" => Some(TokenKind::Try),
            "catch" => Some(TokenKind::Catch),
            "self" => Some(TokenKind::SelfKw),
            "extern" => Some(TokenKind::Extern),
            "return" => Some(TokenKind::Return),
            "pub" => Some(TokenKind::Pub),
            "true" => Some(TokenKind::Bool(true)),
            "false" => Some(TokenKind::Bool(false)),
            _ => None,
        }
    }
}
