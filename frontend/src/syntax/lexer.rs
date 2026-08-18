#[allow(unused_imports)]
use crate::prelude::*;
use crate::syntax::span::Span;
use crate::syntax::token::{StringPart, Token, TokenKind};

pub struct Lexer<'a> {
    source: &'a str,
    chars: core::iter::Peekable<core::str::CharIndices<'a>>,
    pos: usize,
}

impl<'a> Lexer<'a> {
    pub fn new(source: &'a str) -> Self {
        Lexer {
            source,
            chars: source.char_indices().peekable(),
            pos: 0,
        }
    }

    pub fn tokenize(mut self) -> Vec<Token> {
        let mut tokens = Vec::new();
        loop {
            let tok = self.next_token();
            let is_eof = tok.kind == TokenKind::Eof;
            tokens.push(tok);
            if is_eof {
                break;
            }
        }
        tokens
    }

    fn next_token(&mut self) -> Token {
        let saw_newline = self.skip_whitespace_and_comments();

        let start = self.pos;

        // Emit newline token if we crossed a line boundary
        if saw_newline {
            return Token::new(TokenKind::Newline, Span::new(start as u32, start as u32));
        }

        let Some((_, ch)) = self.advance() else {
            return Token::new(TokenKind::Eof, Span::new(start as u32, start as u32));
        };

        let kind = match ch {
            // Single-char tokens
            '(' => TokenKind::LParen,
            ')' => TokenKind::RParen,
            '{' => TokenKind::LBrace,
            '}' => TokenKind::RBrace,
            '[' => TokenKind::LBracket,
            ']' => TokenKind::RBracket,
            ',' => TokenKind::Comma,
            ';' => TokenKind::Semi,
            '.' => {
                if self.peek_char() == Some('.') {
                    self.advance();
                    if self.peek_char() == Some('=') {
                        self.advance();
                        TokenKind::DotDotEq
                    } else {
                        TokenKind::DotDot
                    }
                } else {
                    TokenKind::Dot
                }
            }
            '+' => {
                if self.peek_char() == Some('+') {
                    self.advance();
                    TokenKind::PlusPlus
                } else {
                    TokenKind::Plus
                }
            }
            '*' => TokenKind::Star,
            '%' => {
                if self.peek_char() == Some('{') {
                    self.advance();
                    TokenKind::HashBrace
                } else {
                    TokenKind::Percent
                }
            }

            // Multi-char operators
            '-' => {
                if self.peek_char() == Some('>') {
                    self.advance();
                    TokenKind::Arrow
                } else {
                    TokenKind::Minus
                }
            }
            '=' => {
                if self.peek_char() == Some('=') {
                    self.advance();
                    TokenKind::EqEq
                } else if self.peek_char() == Some('>') {
                    self.advance();
                    TokenKind::FatArrow
                } else {
                    TokenKind::Eq
                }
            }
            '!' => {
                if self.peek_char() == Some('=') {
                    self.advance();
                    TokenKind::NotEq
                } else {
                    TokenKind::Not
                }
            }
            '<' => {
                if self.peek_char() == Some('=') {
                    self.advance();
                    TokenKind::LtEq
                } else if self.peek_char() == Some('<') {
                    self.advance();
                    TokenKind::LtLt
                } else {
                    TokenKind::Lt
                }
            }
            '>' => {
                if self.peek_char() == Some('=') {
                    self.advance();
                    TokenKind::GtEq
                } else if self.peek_char() == Some('>') {
                    self.advance();
                    TokenKind::GtGt
                } else {
                    TokenKind::Gt
                }
            }
            '&' => {
                if self.peek_char() == Some('&') {
                    self.advance();
                    TokenKind::And
                } else {
                    TokenKind::Error("Expected &&".into())
                }
            }
            '|' => {
                if self.peek_char() == Some('|') {
                    self.advance();
                    TokenKind::Or
                } else if self.peek_char() == Some('>') {
                    self.advance();
                    TokenKind::PipeRight
                } else {
                    TokenKind::Pipe
                }
            }
            ':' => {
                if self.peek_char() == Some(':') {
                    self.advance();
                    TokenKind::Colon2
                } else if self.peek_char().map(|c| c.is_alphabetic()).unwrap_or(false) {
                    // Atom literal :name
                    self.atom()
                } else {
                    TokenKind::Colon
                }
            }
            '/' => {
                // Comments handled in skip_whitespace_and_comments
                TokenKind::Slash
            }

            // String literal
            '"' => self.string(),

            // Char literal 'a' or '\n'
            '\'' => self.char_lit(),

            // Number
            '0'..='9' => self.number(ch),

            // Identifier or keyword
            'a'..='z' | '_' => self.identifier(start),
            'A'..='Z' => self.type_identifier(start),

            _ => TokenKind::Error(format!("Unexpected character: {}", ch)),
        };

        Token::new(kind, Span::new(start as u32, self.pos as u32))
    }

    fn advance(&mut self) -> Option<(usize, char)> {
        let next = self.chars.next();
        if let Some((pos, _)) = self.chars.peek() {
            self.pos = *pos;
        } else {
            self.pos = self.source.len();
        }
        next
    }

    fn peek_char(&mut self) -> Option<char> {
        self.chars.peek().map(|(_, ch)| *ch)
    }

    fn skip_whitespace_and_comments(&mut self) -> bool {
        let mut saw_newline = false;
        loop {
            match self.peek_char() {
                Some(' ' | '\t' | '\r') => {
                    self.advance();
                }
                Some('\n') => {
                    self.advance();
                    saw_newline = true;
                }
                Some('/') => {
                    // Look ahead for //
                    let mut temp = self.chars.clone();
                    temp.next(); // consume /
                    if temp.peek().map(|(_, c)| *c) == Some('/') {
                        // Line comment
                        self.advance(); // /
                        self.advance(); // /
                        while let Some(ch) = self.peek_char() {
                            if ch == '\n' {
                                break;
                            }
                            self.advance();
                        }
                    } else {
                        break;
                    }
                }
                _ => break,
            }
        }
        saw_newline
    }

    fn identifier(&mut self, start: usize) -> TokenKind {
        while let Some(ch) = self.peek_char() {
            if ch.is_alphanumeric() || ch == '_' {
                self.advance();
            } else {
                break;
            }
        }
        let text = &self.source[start..self.pos];
        TokenKind::keyword(text).unwrap_or_else(|| TokenKind::Ident(text.to_string()))
    }

    fn type_identifier(&mut self, start: usize) -> TokenKind {
        while let Some(ch) = self.peek_char() {
            if ch.is_alphanumeric() || ch == '_' {
                self.advance();
            } else {
                break;
            }
        }
        let text = &self.source[start..self.pos];
        TokenKind::TypeIdent(text.to_string())
    }

    fn atom(&mut self) -> TokenKind {
        let start = self.pos;
        while let Some(ch) = self.peek_char() {
            if ch.is_alphanumeric() || ch == '_' {
                self.advance();
            } else {
                break;
            }
        }
        let text = &self.source[start..self.pos];
        TokenKind::Atom(text.to_string())
    }

    fn char_lit(&mut self) -> TokenKind {
        let ch = match self.peek_char() {
            Some('\\') => {
                self.advance();
                match self.advance() {
                    Some((_, 'n')) => '\n',
                    Some((_, 't')) => '\t',
                    Some((_, 'r')) => '\r',
                    Some((_, '\\')) => '\\',
                    Some((_, '\'')) => '\'',
                    Some((_, '0')) => '\0',
                    Some((_, c)) => {
                        return TokenKind::Error(format!("Invalid char escape: \\{}", c));
                    }
                    None => return TokenKind::Error("Unterminated char literal".into()),
                }
            }
            Some(c) => {
                self.advance();
                c
            }
            None => return TokenKind::Error("Unterminated char literal".into()),
        };

        match self.advance() {
            Some((_, '\'')) => TokenKind::Char(ch),
            _ => TokenKind::Error("Expected closing ' for char literal".into()),
        }
    }

    fn string(&mut self) -> TokenKind {
        let mut parts: Vec<StringPart> = Vec::new();
        let mut current = String::new();
        let mut has_interpolation = false;

        loop {
            match self.peek_char() {
                Some('"') => {
                    self.advance();
                    break;
                }
                Some('\\') => {
                    self.advance();
                    match self.advance() {
                        Some((_, 'n')) => current.push('\n'),
                        Some((_, 't')) => current.push('\t'),
                        Some((_, 'r')) => current.push('\r'),
                        Some((_, '\\')) => current.push('\\'),
                        Some((_, '"')) => current.push('"'),
                        Some((_, '$')) => current.push('$'), // escape interpolation
                        Some((_, ch)) => {
                            return TokenKind::Error(format!("Invalid escape: \\{}", ch));
                        }
                        None => return TokenKind::Error("Unterminated string".into()),
                    }
                }
                Some('$') => {
                    // Check for ${
                    let mut temp = self.chars.clone();
                    temp.next(); // consume $
                    if temp.peek().map(|(_, c)| *c) == Some('{') {
                        // Found interpolation
                        has_interpolation = true;
                        if !current.is_empty() {
                            parts.push(StringPart::Literal(core::mem::take(&mut current)));
                        }
                        self.advance(); // $
                        self.advance(); // {

                        // Lex the expression until matching }
                        let expr_tokens = self.lex_interpolated_expr();
                        match expr_tokens {
                            Ok(tokens) => parts.push(StringPart::Expr(tokens)),
                            Err(e) => return TokenKind::Error(e),
                        }
                    } else {
                        self.advance();
                        current.push('$');
                    }
                }
                Some(_) => {
                    let (_, ch) = self.advance().unwrap();
                    current.push(ch);
                }
                None => return TokenKind::Error("Unterminated string".into()),
            }
        }

        if has_interpolation {
            if !current.is_empty() {
                parts.push(StringPart::Literal(current));
            }
            TokenKind::InterpolatedString(parts)
        } else {
            TokenKind::String(current)
        }
    }

    /// Lex tokens inside a string interpolation ${...}
    fn lex_interpolated_expr(&mut self) -> Result<Vec<Token>, String> {
        let mut tokens = Vec::new();
        let mut brace_depth = 1;

        loop {
            self.skip_whitespace_and_comments();
            let start = self.pos;

            match self.peek_char() {
                Some('{') => {
                    self.advance();
                    brace_depth += 1;
                    tokens.push(Token::new(
                        TokenKind::LBrace,
                        Span::new(start as u32, self.pos as u32),
                    ));
                }
                Some('}') => {
                    self.advance();
                    brace_depth -= 1;
                    if brace_depth == 0 {
                        break;
                    }
                    tokens.push(Token::new(
                        TokenKind::RBrace,
                        Span::new(start as u32, self.pos as u32),
                    ));
                }
                Some(ch) => {
                    // Lex a single token
                    let kind = match ch {
                        '(' => {
                            self.advance();
                            TokenKind::LParen
                        }
                        ')' => {
                            self.advance();
                            TokenKind::RParen
                        }
                        '[' => {
                            self.advance();
                            TokenKind::LBracket
                        }
                        ']' => {
                            self.advance();
                            TokenKind::RBracket
                        }
                        ',' => {
                            self.advance();
                            TokenKind::Comma
                        }
                        '.' => {
                            self.advance();
                            TokenKind::Dot
                        }
                        '+' => {
                            self.advance();
                            TokenKind::Plus
                        }
                        '-' => {
                            self.advance();
                            TokenKind::Minus
                        }
                        '*' => {
                            self.advance();
                            TokenKind::Star
                        }
                        '/' => {
                            self.advance();
                            TokenKind::Slash
                        }
                        '%' => {
                            self.advance();
                            TokenKind::Percent
                        }
                        ':' => {
                            self.advance();
                            if self.peek_char() == Some(':') {
                                self.advance();
                                TokenKind::Colon2
                            } else if self.peek_char().map(|c| c.is_alphabetic()).unwrap_or(false) {
                                self.atom()
                            } else {
                                TokenKind::Colon
                            }
                        }
                        '"' => {
                            self.advance();
                            self.string()
                        }
                        '0'..='9' => {
                            self.advance();
                            self.number(ch)
                        }
                        'a'..='z' | '_' => self.identifier(start),
                        'A'..='Z' => self.type_identifier(start),
                        _ => {
                            self.advance();
                            TokenKind::Error(format!("Unexpected char in interpolation: {}", ch))
                        }
                    };
                    tokens.push(Token::new(kind, Span::new(start as u32, self.pos as u32)));
                }
                None => return Err("Unterminated string interpolation".into()),
            }
        }

        Ok(tokens)
    }

    fn number(&mut self, first: char) -> TokenKind {
        if first == '0' && matches!(self.peek_char(), Some('x' | 'X')) {
            self.advance();
            return self.hex_number();
        }

        let mut num_str = first.to_string();
        let mut is_float = false;

        while let Some(ch) = self.peek_char() {
            if ch.is_ascii_digit() {
                num_str.push(ch);
                self.advance();
            } else if ch == '.' && !is_float {
                // Look ahead to ensure it's a decimal, not a method call
                let mut temp = self.chars.clone();
                temp.next();
                if temp
                    .peek()
                    .map(|(_, c)| c.is_ascii_digit())
                    .unwrap_or(false)
                {
                    is_float = true;
                    num_str.push(ch);
                    self.advance();
                } else {
                    break;
                }
            } else if ch == '_' {
                // Allow underscores in numbers: 1_000_000
                self.advance();
            } else {
                break;
            }
        }

        if is_float {
            match num_str.parse::<f64>() {
                Ok(f) => TokenKind::Float(f),
                Err(e) => TokenKind::Error(format!("Invalid float: {}", e)),
            }
        } else {
            match num_str.parse::<i64>() {
                Ok(n) => TokenKind::Int(n),
                Err(e) => TokenKind::Error(format!("Invalid integer: {}", e)),
            }
        }
    }

    fn hex_number(&mut self) -> TokenKind {
        let mut num_str = String::new();

        while let Some(ch) = self.peek_char() {
            if ch.is_ascii_hexdigit() {
                num_str.push(ch);
                self.advance();
            } else if ch == '_' {
                self.advance();
            } else {
                break;
            }
        }

        if num_str.is_empty() {
            return TokenKind::Error("Invalid hexadecimal integer".into());
        }

        match i64::from_str_radix(&num_str, 16) {
            Ok(n) => TokenKind::Int(n),
            Err(e) => TokenKind::Error(format!("Invalid hexadecimal integer: {}", e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_tokens() {
        let tokens = Lexer::new("fn main() { }").tokenize();
        assert_eq!(tokens[0].kind, TokenKind::Fn);
        assert_eq!(tokens[1].kind, TokenKind::Ident("main".into()));
        assert_eq!(tokens[2].kind, TokenKind::LParen);
        assert_eq!(tokens[3].kind, TokenKind::RParen);
        assert_eq!(tokens[4].kind, TokenKind::LBrace);
        assert_eq!(tokens[5].kind, TokenKind::RBrace);
        assert_eq!(tokens[6].kind, TokenKind::Eof);
    }

    #[test]
    fn test_atoms() {
        let tokens = Lexer::new(":ok :error").tokenize();
        assert_eq!(tokens[0].kind, TokenKind::Atom("ok".into()));
        assert_eq!(tokens[1].kind, TokenKind::Atom("error".into()));
    }

    #[test]
    fn test_numbers() {
        let tokens = Lexer::new("42 3.14 1_000").tokenize();
        assert_eq!(tokens[0].kind, TokenKind::Int(42));
        assert_eq!(tokens[1].kind, TokenKind::Float(3.14));
        assert_eq!(tokens[2].kind, TokenKind::Int(1000));
    }

    #[test]
    fn test_hex_integer() {
        let tokens = Lexer::new("0x44434241").tokenize();
        assert_eq!(tokens[0].kind, TokenKind::Int(0x4443_4241));
    }

    #[test]
    fn test_strings() {
        let tokens = Lexer::new(r#""hello" "world\n""#).tokenize();
        assert_eq!(tokens[0].kind, TokenKind::String("hello".into()));
        assert_eq!(tokens[1].kind, TokenKind::String("world\n".into()));
    }

    #[test]
    fn test_operators() {
        let tokens = Lexer::new("-> => == != <= >= && || ::").tokenize();
        assert_eq!(tokens[0].kind, TokenKind::Arrow);
        assert_eq!(tokens[1].kind, TokenKind::FatArrow);
        assert_eq!(tokens[2].kind, TokenKind::EqEq);
        assert_eq!(tokens[3].kind, TokenKind::NotEq);
        assert_eq!(tokens[4].kind, TokenKind::LtEq);
        assert_eq!(tokens[5].kind, TokenKind::GtEq);
        assert_eq!(tokens[6].kind, TokenKind::And);
        assert_eq!(tokens[7].kind, TokenKind::Or);
        assert_eq!(tokens[8].kind, TokenKind::Colon2);
    }

    #[test]
    fn test_comments() {
        let tokens = Lexer::new("fn // comment\nmain").tokenize();
        assert_eq!(tokens[0].kind, TokenKind::Fn);
        assert_eq!(tokens[1].kind, TokenKind::Newline);
        assert_eq!(tokens[2].kind, TokenKind::Ident("main".into()));
    }
}
