use crate::syntax::ast::{CatchArm, Generator, *};
use crate::syntax::span::Span;
use crate::syntax::token::{StringPart, Token, TokenKind};

#[derive(Debug, Clone, Copy)]
pub struct ParserOptions {
    pub allow_extern: bool,
}

impl Default for ParserOptions {
    fn default() -> Self {
        Self { allow_extern: true }
    }
}

pub struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    pending_type_gt: usize,
    /// True if we just skipped a newline
    after_newline: bool,
    options: ParserOptions,
}

pub type ParseResult<T> = Result<T, ParseError>;

#[derive(Debug, Clone)]
pub struct ParseError {
    pub message: String,
    pub span: Span,
}

impl ParseError {
    pub fn new(message: impl Into<String>, span: Span) -> Self {
        ParseError {
            message: message.into(),
            span,
        }
    }
}

impl Parser {
    pub fn new(tokens: Vec<Token>) -> Self {
        Self::new_with_options(tokens, ParserOptions::default())
    }

    pub fn new_with_options(tokens: Vec<Token>, options: ParserOptions) -> Self {
        Parser {
            tokens,
            pos: 0,
            pending_type_gt: 0,
            after_newline: false,
            options,
        }
    }

    /// Skip any newline tokens and track if we saw one
    fn skip_newlines(&mut self) {
        self.after_newline = false;
        while matches!(
            self.tokens.get(self.pos).map(|t| &t.kind),
            Some(TokenKind::Newline)
        ) {
            self.pos += 1;
            self.after_newline = true;
        }
    }

    pub fn parse_module(&mut self) -> ParseResult<Module> {
        let start = self.current_span();
        let name = if self.check(&TokenKind::Mod) {
            self.advance();
            let name = self.expect_ident()?;
            Some(name)
        } else {
            None
        };

        let mut items = Vec::new();
        while !self.is_at_end() {
            items.push(self.parse_item()?);
        }

        Ok(Module {
            name,
            items,
            span: start.merge(self.prev_span()),
        })
    }

    fn parse_item(&mut self) -> ParseResult<Item> {
        match self.peek() {
            TokenKind::Fn => self.parse_function().map(Item::Function),
            TokenKind::Type => self.parse_type_alias().map(Item::TypeAlias),
            TokenKind::Enum => self.parse_enum().map(Item::Enum),
            TokenKind::Struct => self.parse_struct().map(Item::Struct),
            TokenKind::Extern => {
                if !self.options.allow_extern {
                    Err(self.error("`external` declarations are disabled in sandbox mode"))
                } else {
                    self.parse_extern().map(Item::Extern)
                }
            }
            TokenKind::Use => self.parse_use().map(Item::Use),
            _ => Err(self.error("Expected fn, type, enum, struct, use, or extern")),
        }
    }

    fn parse_function(&mut self) -> ParseResult<Function> {
        let start = self.current_span();
        self.expect(&TokenKind::Fn)?;
        let name = self.expect_ident()?;

        // Optional type parameters <T, U>
        let type_params = self.parse_type_params()?;

        // Parameters
        self.expect(&TokenKind::LParen)?;
        let params = self.parse_params()?;
        self.expect(&TokenKind::RParen)?;

        // Optional return type
        let return_type = if self.check(&TokenKind::Arrow) {
            self.advance();
            Some(self.parse_type()?)
        } else {
            None
        };

        // Body
        let body = self.parse_block_expr()?;

        Ok(Function {
            name,
            type_params,
            params,
            return_type,
            body,
            span: start.merge(self.prev_span()),
        })
    }

    fn parse_type_params(&mut self) -> ParseResult<Vec<Ident>> {
        if !self.check(&TokenKind::Lt) {
            return Ok(Vec::new());
        }
        self.advance();

        let mut params = Vec::new();
        loop {
            params.push(self.expect_type_ident()?);
            if !self.check(&TokenKind::Comma) {
                break;
            }
            self.advance();
        }
        self.expect_type_gt()?;
        Ok(params)
    }

    fn parse_params(&mut self) -> ParseResult<Vec<Param>> {
        let mut params = Vec::new();
        if self.check(&TokenKind::RParen) {
            return Ok(params);
        }

        loop {
            let start = self.current_span();
            let name = self.expect_ident()?;
            let ty = if self.check(&TokenKind::Colon) {
                self.advance();
                Some(self.parse_type()?)
            } else {
                None
            };
            params.push(Param {
                name,
                ty,
                span: start.merge(self.prev_span()),
            });

            if !self.check(&TokenKind::Comma) {
                break;
            }
            self.advance();
        }
        Ok(params)
    }

    fn parse_type(&mut self) -> ParseResult<TypeExpr> {
        let start = self.current_span();

        // Function type: fn(A, B) -> C
        if self.check(&TokenKind::Fn) {
            self.advance();
            self.expect(&TokenKind::LParen)?;
            let mut param_types = Vec::new();
            if !self.check(&TokenKind::RParen) {
                loop {
                    param_types.push(self.parse_type()?);
                    if !self.check(&TokenKind::Comma) {
                        break;
                    }
                    self.advance();
                }
            }
            self.expect(&TokenKind::RParen)?;
            self.expect(&TokenKind::Arrow)?;
            let ret = self.parse_type()?;
            return Ok(TypeExpr::Function(
                param_types,
                Box::new(ret),
                start.merge(self.prev_span()),
            ));
        }

        // Tuple or unit: () or (A, B)
        if self.check(&TokenKind::LParen) {
            self.advance();
            if self.check(&TokenKind::RParen) {
                self.advance();
                return Ok(TypeExpr::Unit(start.merge(self.prev_span())));
            }
            let mut types = vec![self.parse_type()?];
            while self.check(&TokenKind::Comma) {
                self.advance();
                types.push(self.parse_type()?);
            }
            self.expect(&TokenKind::RParen)?;
            if types.len() == 1 {
                return Ok(types.pop().unwrap());
            }
            return Ok(TypeExpr::Tuple(types, start.merge(self.prev_span())));
        }

        // Record: { x: Int, y: Int }
        if self.check(&TokenKind::LBrace) {
            self.advance();
            let mut fields = Vec::new();
            if !self.check(&TokenKind::RBrace) {
                loop {
                    let name = self.expect_ident()?;
                    self.expect(&TokenKind::Colon)?;
                    let ty = self.parse_type()?;
                    fields.push((name, ty));
                    if !self.check(&TokenKind::Comma) {
                        break;
                    }
                    self.advance();
                }
            }
            self.expect(&TokenKind::RBrace)?;
            return Ok(TypeExpr::Record(fields, start.merge(self.prev_span())));
        }

        // List type: [T]
        if self.check(&TokenKind::LBracket) {
            self.advance();
            let elem_type = self.parse_type()?;
            self.expect(&TokenKind::RBracket)?;
            // Return as Named("List", [elem_type])
            return Ok(TypeExpr::Named(
                "List".to_string(),
                vec![elem_type],
                start.merge(self.prev_span()),
            ));
        }

        // Named type: Int, Option<T>
        let name = self.expect_type_ident_or_ident()?;
        let type_args = if self.check(&TokenKind::Lt) {
            self.advance();
            let mut args = vec![self.parse_type()?];
            while self.check(&TokenKind::Comma) {
                self.advance();
                args.push(self.parse_type()?);
            }
            self.expect_type_gt()?;
            args
        } else {
            Vec::new()
        };

        Ok(TypeExpr::Named(
            name,
            type_args,
            start.merge(self.prev_span()),
        ))
    }

    fn parse_type_alias(&mut self) -> ParseResult<TypeAlias> {
        let start = self.current_span();
        self.expect(&TokenKind::Type)?;
        let name = self.expect_type_ident()?;
        let type_params = self.parse_type_params()?;
        self.expect(&TokenKind::Eq)?;
        let ty = self.parse_type()?;

        Ok(TypeAlias {
            name,
            type_params,
            ty,
            span: start.merge(self.prev_span()),
        })
    }

    fn parse_enum(&mut self) -> ParseResult<EnumDef> {
        let start = self.current_span();
        self.expect(&TokenKind::Enum)?;
        let name = self.expect_type_ident()?;
        let type_params = self.parse_type_params()?;

        self.expect(&TokenKind::LBrace)?;
        let mut variants = Vec::new();
        while !self.check(&TokenKind::RBrace) {
            let var_start = self.current_span();
            let var_name = self.expect_type_ident()?;
            let fields = if self.check(&TokenKind::LParen) {
                self.advance();
                let mut fs = Vec::new();
                if !self.check(&TokenKind::RParen) {
                    loop {
                        fs.push(self.parse_type()?);
                        if !self.check(&TokenKind::Comma) {
                            break;
                        }
                        self.advance();
                    }
                }
                self.expect(&TokenKind::RParen)?;
                fs
            } else {
                Vec::new()
            };
            variants.push(Variant {
                name: var_name,
                fields,
                span: var_start.merge(self.prev_span()),
            });
            if !self.check(&TokenKind::Comma) {
                break;
            }
            self.advance();
        }
        self.expect(&TokenKind::RBrace)?;

        Ok(EnumDef {
            name,
            type_params,
            variants,
            span: start.merge(self.prev_span()),
        })
    }

    fn parse_use(&mut self) -> ParseResult<UseDecl> {
        let start = self.current_span();
        self.expect(&TokenKind::Use)?;

        // Module name (can be lowercase or uppercase)
        let module = self.expect_ident_or_type_ident()?;

        // Optional: ::{item1, item2}
        let items = if self.check(&TokenKind::Colon2) {
            self.advance();
            self.expect(&TokenKind::LBrace)?;
            let mut items = Vec::new();
            while !self.check(&TokenKind::RBrace) {
                items.push(self.expect_ident_or_type_ident()?);
                if !self.check(&TokenKind::Comma) {
                    break;
                }
                self.advance();
            }
            self.expect(&TokenKind::RBrace)?;
            Some(items)
        } else {
            None
        };

        Ok(UseDecl {
            module,
            items,
            span: start.merge(self.prev_span()),
        })
    }

    fn parse_struct(&mut self) -> ParseResult<StructDef> {
        let start = self.current_span();
        self.expect(&TokenKind::Struct)?;
        let name = self.expect_type_ident()?;
        let type_params = self.parse_type_params()?;

        self.expect(&TokenKind::LBrace)?;
        let mut fields = Vec::new();
        while !self.check(&TokenKind::RBrace) {
            let field_start = self.current_span();
            let field_name = self.expect_ident()?;
            self.expect(&TokenKind::Colon)?;
            let field_ty = self.parse_type()?;
            fields.push(StructField {
                name: field_name,
                ty: field_ty,
                span: field_start.merge(self.prev_span()),
            });
            if !self.check(&TokenKind::Comma) {
                break;
            }
            self.advance();
        }
        self.expect(&TokenKind::RBrace)?;

        Ok(StructDef {
            name,
            type_params,
            fields,
            span: start.merge(self.prev_span()),
        })
    }

    fn parse_extern(&mut self) -> ParseResult<ExternBlock> {
        let start = self.current_span();
        self.expect(&TokenKind::Extern)?;
        let abi = if let TokenKind::String(s) = self.peek() {
            let s = s.clone();
            self.advance();
            s
        } else {
            "erlang".to_string()
        };

        self.expect(&TokenKind::LBrace)?;
        let mut decls = Vec::new();
        while !self.check(&TokenKind::RBrace) {
            decls.push(self.parse_extern_fn()?);
        }
        self.expect(&TokenKind::RBrace)?;

        Ok(ExternBlock {
            abi,
            decls,
            span: start.merge(self.prev_span()),
        })
    }

    fn parse_extern_fn(&mut self) -> ParseResult<ExternFn> {
        let start = self.current_span();
        self.expect(&TokenKind::Fn)?;
        let module = self.expect_ident()?;
        // Accept either : or :: for module separator (:: preferred to avoid atom ambiguity)
        if self.check(&TokenKind::Colon2) {
            self.advance();
        } else {
            self.expect(&TokenKind::Colon)?;
        }
        let name = self.expect_ident()?;
        let type_params = self.parse_type_params()?;

        self.expect(&TokenKind::LParen)?;
        let mut params = Vec::new();
        if !self.check(&TokenKind::RParen) {
            loop {
                params.push(self.parse_type()?);
                if !self.check(&TokenKind::Comma) {
                    break;
                }
                self.advance();
            }
        }
        self.expect(&TokenKind::RParen)?;
        self.expect(&TokenKind::Arrow)?;
        let return_type = self.parse_type()?;

        Ok(ExternFn {
            module,
            name,
            type_params,
            params,
            return_type,
            span: start.merge(self.prev_span()),
        })
    }

    fn parse_block_expr(&mut self) -> ParseResult<Expr> {
        let start = self.current_span();
        self.expect(&TokenKind::LBrace)?;

        let mut stmts = Vec::new();
        let mut final_expr = None;

        while !self.check(&TokenKind::RBrace) {
            if self.check(&TokenKind::Let) {
                stmts.push(self.parse_let_stmt()?);
            } else {
                let expr = self.parse_expr()?;
                if self.check(&TokenKind::RBrace) {
                    final_expr = Some(Box::new(expr));
                } else {
                    stmts.push(Stmt::Expr(expr));
                }
            }
        }
        self.expect(&TokenKind::RBrace)?;

        Ok(Expr::Block(
            stmts,
            final_expr,
            start.merge(self.prev_span()),
        ))
    }

    fn parse_let_stmt(&mut self) -> ParseResult<Stmt> {
        let start = self.current_span();
        self.expect(&TokenKind::Let)?;
        let pattern = self.parse_pattern()?;
        let ty = if self.check(&TokenKind::Colon) {
            self.advance();
            Some(self.parse_type()?)
        } else {
            None
        };
        self.expect(&TokenKind::Eq)?;
        let expr = self.parse_expr()?;
        Ok(Stmt::Let(pattern, ty, expr, start.merge(self.prev_span())))
    }

    fn parse_expr(&mut self) -> ParseResult<Expr> {
        self.parse_pipe_expr()
    }

    fn parse_pipe_expr(&mut self) -> ParseResult<Expr> {
        let mut expr = self.parse_or_expr()?;
        while self.check(&TokenKind::PipeRight) {
            let start = expr.span();
            self.advance();
            let right = self.parse_or_expr()?;
            let span = start.merge(right.span());
            expr = Expr::Binary(Box::new(expr), BinOp::Pipe, Box::new(right), span);
        }
        Ok(expr)
    }

    fn parse_or_expr(&mut self) -> ParseResult<Expr> {
        let mut left = self.parse_and_expr()?;
        while self.check(&TokenKind::Or) {
            self.advance();
            let right = self.parse_and_expr()?;
            let span = left.span().merge(right.span());
            left = Expr::Binary(Box::new(left), BinOp::Or, Box::new(right), span);
        }
        Ok(left)
    }

    fn parse_and_expr(&mut self) -> ParseResult<Expr> {
        let mut left = self.parse_equality_expr()?;
        while self.check(&TokenKind::And) {
            self.advance();
            let right = self.parse_equality_expr()?;
            let span = left.span().merge(right.span());
            left = Expr::Binary(Box::new(left), BinOp::And, Box::new(right), span);
        }
        Ok(left)
    }

    fn parse_equality_expr(&mut self) -> ParseResult<Expr> {
        let mut left = self.parse_comparison_expr()?;
        loop {
            let op = match self.peek() {
                TokenKind::EqEq => BinOp::Eq,
                TokenKind::NotEq => BinOp::NotEq,
                _ => break,
            };
            self.advance();
            let right = self.parse_comparison_expr()?;
            let span = left.span().merge(right.span());
            left = Expr::Binary(Box::new(left), op, Box::new(right), span);
        }
        Ok(left)
    }

    fn parse_comparison_expr(&mut self) -> ParseResult<Expr> {
        let mut left = self.parse_range_expr()?;
        loop {
            let op = match self.peek() {
                TokenKind::Lt => BinOp::Lt,
                TokenKind::LtEq => BinOp::LtEq,
                TokenKind::Gt => BinOp::Gt,
                TokenKind::GtEq => BinOp::GtEq,
                _ => break,
            };
            self.advance();
            let right = self.parse_range_expr()?;
            let span = left.span().merge(right.span());
            left = Expr::Binary(Box::new(left), op, Box::new(right), span);
        }
        Ok(left)
    }

    fn parse_range_expr(&mut self) -> ParseResult<Expr> {
        let left = self.parse_additive_expr()?;

        // Check for range operators
        let inclusive = match self.peek() {
            TokenKind::DotDot => false,
            TokenKind::DotDotEq => true,
            _ => return Ok(left),
        };
        self.advance();
        let right = self.parse_additive_expr()?;
        let span = left.span().merge(right.span());
        Ok(Expr::Range(
            Box::new(left),
            Box::new(right),
            inclusive,
            span,
        ))
    }

    fn parse_additive_expr(&mut self) -> ParseResult<Expr> {
        let mut left = self.parse_multiplicative_expr()?;
        loop {
            let op = match self.peek() {
                TokenKind::Plus => BinOp::Add,
                TokenKind::PlusPlus => BinOp::Concat,
                TokenKind::Minus => BinOp::Sub,
                _ => break,
            };
            self.advance();
            let right = self.parse_multiplicative_expr()?;
            let span = left.span().merge(right.span());
            left = Expr::Binary(Box::new(left), op, Box::new(right), span);
        }
        Ok(left)
    }

    fn parse_multiplicative_expr(&mut self) -> ParseResult<Expr> {
        let mut left = self.parse_unary_expr()?;
        loop {
            let op = match self.peek() {
                TokenKind::Star => BinOp::Mul,
                TokenKind::Slash => BinOp::Div,
                TokenKind::Percent => BinOp::Mod,
                _ => break,
            };
            self.advance();
            let right = self.parse_unary_expr()?;
            let span = left.span().merge(right.span());
            left = Expr::Binary(Box::new(left), op, Box::new(right), span);
        }
        Ok(left)
    }

    fn parse_unary_expr(&mut self) -> ParseResult<Expr> {
        let start = self.current_span();
        match self.peek() {
            TokenKind::Minus => {
                self.advance();
                let expr = self.parse_unary_expr()?;
                let span = start.merge(expr.span());
                Ok(Expr::Unary(UnaryOp::Neg, Box::new(expr), span))
            }
            TokenKind::Not => {
                self.advance();
                let expr = self.parse_unary_expr()?;
                let span = start.merge(expr.span());
                Ok(Expr::Unary(UnaryOp::Not, Box::new(expr), span))
            }
            _ => self.parse_call_expr(),
        }
    }

    fn parse_call_expr(&mut self) -> ParseResult<Expr> {
        let mut expr = self.parse_primary_expr()?;

        loop {
            // Check for call continuation - don't continue across newlines
            // First check if next token is newline
            if self.check_no_skip(&TokenKind::Newline) {
                // Skip the newlines and mark that we saw them
                self.skip_newlines();
                // Don't continue call parsing if next is ( - that's a new expression
                if matches!(self.peek_no_skip(), TokenKind::LParen) {
                    break;
                }
            }

            if self.check(&TokenKind::LParen) {
                let start = expr.span();
                self.advance();
                let args = self.parse_args()?;
                self.expect(&TokenKind::RParen)?;
                expr = Expr::Call(Box::new(expr), args, start.merge(self.prev_span()));
            } else if self.check(&TokenKind::Dot) {
                let start = expr.span();
                self.advance();
                let field = self.expect_ident()?;
                if self.check(&TokenKind::LParen) {
                    self.advance();
                    let args = self.parse_args()?;
                    self.expect(&TokenKind::RParen)?;
                    expr = Expr::MethodCall(
                        Box::new(expr),
                        field,
                        args,
                        start.merge(self.prev_span()),
                    );
                } else {
                    expr = Expr::Field(Box::new(expr), field, start.merge(self.prev_span()));
                }
            } else if self.check(&TokenKind::Colon2) {
                // Path continuation: Foo::Bar
                if let Expr::Var(name, span) = expr {
                    let mut path = vec![name];
                    while self.check(&TokenKind::Colon2) {
                        self.advance();
                        path.push(self.expect_ident_or_type_ident()?);
                    }
                    expr = Expr::Path(path, span.merge(self.prev_span()));
                } else if let Expr::Path(mut path, span) = expr {
                    while self.check(&TokenKind::Colon2) {
                        self.advance();
                        path.push(self.expect_ident_or_type_ident()?);
                    }
                    expr = Expr::Path(path, span.merge(self.prev_span()));
                } else {
                    break;
                }
            } else if self.check(&TokenKind::LBracket) {
                // Index access: expr[key]
                let start = expr.span();
                self.advance();
                let index = self.parse_expr()?;
                self.expect(&TokenKind::RBracket)?;
                expr = Expr::Index(
                    Box::new(expr),
                    Box::new(index),
                    start.merge(self.prev_span()),
                );
            } else {
                break;
            }
        }
        Ok(expr)
    }

    fn parse_args(&mut self) -> ParseResult<Vec<Expr>> {
        let mut args = Vec::new();
        if self.check(&TokenKind::RParen) {
            return Ok(args);
        }
        loop {
            args.push(self.parse_expr()?);
            if !self.check(&TokenKind::Comma) {
                break;
            }
            self.advance();
        }
        Ok(args)
    }

    fn parse_primary_expr(&mut self) -> ParseResult<Expr> {
        let start = self.current_span();

        match self.peek().clone() {
            TokenKind::Int(n) => {
                self.advance();
                Ok(Expr::Int(n, start))
            }
            TokenKind::Float(f) => {
                self.advance();
                Ok(Expr::Float(f, start))
            }
            TokenKind::Char(c) => {
                self.advance();
                Ok(Expr::Char(c, start))
            }
            TokenKind::String(s) => {
                self.advance();
                Ok(Expr::String(s, start))
            }
            TokenKind::InterpolatedString(parts) => {
                self.advance();
                let ast_parts = self.parse_interpolated_parts(parts)?;
                Ok(Expr::InterpolatedString(
                    ast_parts,
                    start.merge(self.prev_span()),
                ))
            }
            TokenKind::Bool(b) => {
                self.advance();
                Ok(Expr::Bool(b, start))
            }
            TokenKind::Atom(a) => {
                self.advance();
                Ok(Expr::Atom(a, start))
            }
            TokenKind::Ident(name) => {
                self.advance();
                Ok(Expr::Var(name, start))
            }
            TokenKind::TypeIdent(name) => {
                self.advance();
                // Could be start of path, struct init, or just a type name
                if self.check(&TokenKind::Colon2) {
                    let mut path = vec![name];
                    while self.check(&TokenKind::Colon2) {
                        self.advance();
                        path.push(self.expect_ident_or_type_ident()?);
                    }
                    Ok(Expr::Path(path, start.merge(self.prev_span())))
                } else if self.check(&TokenKind::LBrace) {
                    // Struct initialization: Point { x: 1, y: 2 }
                    self.advance();
                    let mut fields = Vec::new();
                    while !self.check(&TokenKind::RBrace) {
                        let field_name = self.expect_ident()?;
                        self.expect(&TokenKind::Colon)?;
                        let field_value = self.parse_expr()?;
                        fields.push((field_name, field_value));
                        if !self.check(&TokenKind::Comma) {
                            break;
                        }
                        self.advance();
                    }
                    self.expect(&TokenKind::RBrace)?;
                    Ok(Expr::StructInit(
                        name,
                        fields,
                        start.merge(self.prev_span()),
                    ))
                } else {
                    Ok(Expr::Var(name, start))
                }
            }
            TokenKind::SelfKw => {
                self.advance();
                self.expect(&TokenKind::LParen)?;
                self.expect(&TokenKind::RParen)?;
                Ok(Expr::SelfPid(start.merge(self.prev_span())))
            }
            TokenKind::If => self.parse_if_expr(),
            TokenKind::Unless => self.parse_unless_expr(),
            TokenKind::Cond => self.parse_cond_expr(),
            TokenKind::Match => self.parse_match_expr(),
            TokenKind::LtLt => {
                // Binary/bitstring literal: <<1, 2, 3>> or <<"hello">>
                self.advance();
                let mut elements = Vec::new();
                if !self.check(&TokenKind::GtGt) {
                    loop {
                        elements.push(self.parse_bitstring_expr_segment()?);
                        if !self.check(&TokenKind::Comma) {
                            break;
                        }
                        self.advance();
                    }
                }
                self.expect(&TokenKind::GtGt)?;
                Ok(Expr::BitString(elements, start.merge(self.prev_span())))
            }
            TokenKind::Spawn => {
                self.advance();
                self.expect(&TokenKind::LParen)?;
                let func = self.parse_expr()?;
                self.expect(&TokenKind::RParen)?;
                Ok(Expr::Spawn(Box::new(func), start.merge(self.prev_span())))
            }
            TokenKind::Send => {
                self.advance();
                self.expect(&TokenKind::LParen)?;
                let pid = self.parse_expr()?;
                self.expect(&TokenKind::Comma)?;
                let msg = self.parse_expr()?;
                self.expect(&TokenKind::RParen)?;
                Ok(Expr::Send(
                    Box::new(pid),
                    Box::new(msg),
                    start.merge(self.prev_span()),
                ))
            }
            TokenKind::Receive => self.parse_receive_expr(),
            TokenKind::Return => {
                self.advance();
                let expr = if self.check(&TokenKind::RBrace) || self.check(&TokenKind::Eof) {
                    None
                } else {
                    Some(Box::new(self.parse_expr()?))
                };
                Ok(Expr::Return(expr, start.merge(self.prev_span())))
            }
            TokenKind::LParen => {
                self.advance();
                if self.check(&TokenKind::RParen) {
                    self.advance();
                    return Ok(Expr::Unit(start.merge(self.prev_span())));
                }
                let expr = self.parse_expr()?;
                if self.check(&TokenKind::Comma) {
                    // Tuple
                    let mut exprs = vec![expr];
                    while self.check(&TokenKind::Comma) {
                        self.advance();
                        if self.check(&TokenKind::RParen) {
                            break;
                        }
                        exprs.push(self.parse_expr()?);
                    }
                    self.expect(&TokenKind::RParen)?;
                    Ok(Expr::Tuple(exprs, start.merge(self.prev_span())))
                } else {
                    self.expect(&TokenKind::RParen)?;
                    Ok(expr)
                }
            }
            TokenKind::LBracket => {
                self.advance();
                if self.check(&TokenKind::RBracket) {
                    self.advance();
                    return Ok(Expr::List(Vec::new(), None, start.merge(self.prev_span())));
                }
                let first_expr = self.parse_expr()?;

                // Check for list comprehension: [expr for pattern in source]
                if self.check(&TokenKind::For) {
                    return self.parse_list_comp(first_expr, start);
                }

                // Regular list
                let mut exprs = vec![first_expr];
                while self.check(&TokenKind::Comma) {
                    self.advance();
                    if self.check(&TokenKind::RBracket) {
                        break;
                    }
                    if self.check(&TokenKind::Pipe) {
                        break;
                    }
                    exprs.push(self.parse_expr()?);
                }
                let tail = if self.check(&TokenKind::Pipe) {
                    self.advance();
                    Some(Box::new(self.parse_expr()?))
                } else {
                    None
                };
                self.expect(&TokenKind::RBracket)?;
                Ok(Expr::List(exprs, tail, start.merge(self.prev_span())))
            }
            TokenKind::LBrace => self.parse_block_expr(),
            TokenKind::HashBrace => self.parse_map_expr(),
            TokenKind::Try => self.parse_try_expr(),
            TokenKind::Pipe => self.parse_lambda(),
            TokenKind::Or => self.parse_empty_lambda(), // || for empty params
            _ => Err(self.error("Expected expression")),
        }
    }

    fn parse_cond_expr(&mut self) -> ParseResult<Expr> {
        let start = self.current_span();
        self.expect(&TokenKind::Cond)?;
        self.expect(&TokenKind::LBrace)?;

        let mut arms = Vec::new();
        while !self.check(&TokenKind::RBrace) {
            let cond = self.parse_expr()?;
            self.expect(&TokenKind::FatArrow)?;
            let body = self.parse_expr()?;
            arms.push((cond, body));

            // Allow comma or newline between arms
            if self.check(&TokenKind::Comma) {
                self.advance();
            }
        }

        self.expect(&TokenKind::RBrace)?;
        Ok(Expr::Cond(arms, start.merge(self.prev_span())))
    }

    fn parse_unless_expr(&mut self) -> ParseResult<Expr> {
        let start = self.current_span();
        self.expect(&TokenKind::Unless)?;
        let cond = self.parse_expr()?;
        // Wrap condition in Not
        let negated_cond = Expr::Unary(UnaryOp::Not, Box::new(cond), start);
        let then_branch = self.parse_block_expr()?;
        let else_branch = if self.check(&TokenKind::Else) {
            self.advance();
            Some(Box::new(self.parse_block_expr()?))
        } else {
            None
        };
        Ok(Expr::If(
            Box::new(negated_cond),
            Box::new(then_branch),
            else_branch,
            start.merge(self.prev_span()),
        ))
    }

    fn parse_if_expr(&mut self) -> ParseResult<Expr> {
        let start = self.current_span();
        self.expect(&TokenKind::If)?;
        let cond = self.parse_expr()?;
        let then_branch = self.parse_block_expr()?;
        let else_branch = if self.check(&TokenKind::Else) {
            self.advance();
            if self.check(&TokenKind::If) {
                Some(Box::new(self.parse_if_expr()?))
            } else {
                Some(Box::new(self.parse_block_expr()?))
            }
        } else {
            None
        };
        Ok(Expr::If(
            Box::new(cond),
            Box::new(then_branch),
            else_branch,
            start.merge(self.prev_span()),
        ))
    }

    fn parse_match_expr(&mut self) -> ParseResult<Expr> {
        let start = self.current_span();
        self.expect(&TokenKind::Match)?;
        let scrutinee = self.parse_expr()?;
        self.expect(&TokenKind::LBrace)?;
        let mut arms = Vec::new();
        while !self.check(&TokenKind::RBrace) {
            arms.push(self.parse_match_arm()?);
            if !self.check(&TokenKind::Comma) {
                break;
            }
            self.advance();
        }
        self.expect(&TokenKind::RBrace)?;
        Ok(Expr::Match(
            Box::new(scrutinee),
            arms,
            start.merge(self.prev_span()),
        ))
    }

    fn parse_receive_expr(&mut self) -> ParseResult<Expr> {
        let start = self.current_span();
        self.expect(&TokenKind::Receive)?;
        self.expect(&TokenKind::LBrace)?;
        let mut arms = Vec::new();
        let mut timeout = None;

        while !self.check(&TokenKind::RBrace) {
            // Check for after clause
            if self.check(&TokenKind::After) {
                self.advance();
                let timeout_ms = self.parse_expr()?;
                self.expect(&TokenKind::FatArrow)?;
                let timeout_body = self.parse_expr()?;
                timeout = Some((Box::new(timeout_ms), Box::new(timeout_body)));
                // after clause must be last
                if self.check(&TokenKind::Comma) {
                    self.advance();
                }
                break;
            }

            arms.push(self.parse_match_arm()?);
            if !self.check(&TokenKind::Comma) {
                break;
            }
            self.advance();
        }
        self.expect(&TokenKind::RBrace)?;
        Ok(Expr::Receive {
            arms,
            timeout,
            span: start.merge(self.prev_span()),
        })
    }

    fn parse_match_arm(&mut self) -> ParseResult<MatchArm> {
        let start = self.current_span();
        let pattern = self.parse_pattern()?;
        let guard = if self.check(&TokenKind::If) {
            self.advance();
            Some(Box::new(self.parse_expr()?))
        } else {
            None
        };
        self.expect(&TokenKind::FatArrow)?;
        let body = self.parse_expr()?;
        Ok(MatchArm {
            pattern,
            guard,
            body,
            span: start.merge(self.prev_span()),
        })
    }

    fn parse_pattern(&mut self) -> ParseResult<Pattern> {
        let start = self.current_span();

        match self.peek().clone() {
            TokenKind::Ident(name) if name == "_" => {
                self.advance();
                Ok(Pattern::Wildcard(start))
            }
            TokenKind::Ident(name) => {
                self.advance();
                Ok(Pattern::Var(name, start))
            }
            TokenKind::TypeIdent(name) => {
                self.advance();
                // Constructor pattern
                let mut path = vec![name];
                while self.check(&TokenKind::Colon2) {
                    self.advance();
                    path.push(self.expect_ident_or_type_ident()?);
                }
                let fields = if self.check(&TokenKind::LParen) {
                    self.advance();
                    let mut fs = Vec::new();
                    if !self.check(&TokenKind::RParen) {
                        loop {
                            fs.push(self.parse_pattern()?);
                            if !self.check(&TokenKind::Comma) {
                                break;
                            }
                            self.advance();
                        }
                    }
                    self.expect(&TokenKind::RParen)?;
                    fs
                } else {
                    Vec::new()
                };
                Ok(Pattern::Constructor(
                    path,
                    fields,
                    start.merge(self.prev_span()),
                ))
            }
            TokenKind::Int(n) => {
                self.advance();
                Ok(Pattern::Int(n, start))
            }
            TokenKind::Float(f) => {
                self.advance();
                Ok(Pattern::Float(f, start))
            }
            TokenKind::Char(c) => {
                self.advance();
                Ok(Pattern::Char(c, start))
            }
            TokenKind::String(s) => {
                self.advance();
                Ok(Pattern::String(s, start))
            }
            TokenKind::Bool(b) => {
                self.advance();
                Ok(Pattern::Bool(b, start))
            }
            TokenKind::Atom(a) => {
                self.advance();
                Ok(Pattern::Atom(a, start))
            }
            TokenKind::LParen => {
                self.advance();
                if self.check(&TokenKind::RParen) {
                    self.advance();
                    return Ok(Pattern::Tuple(Vec::new(), start.merge(self.prev_span())));
                }
                let mut patterns = vec![self.parse_pattern()?];
                while self.check(&TokenKind::Comma) {
                    self.advance();
                    patterns.push(self.parse_pattern()?);
                }
                self.expect(&TokenKind::RParen)?;
                Ok(Pattern::Tuple(patterns, start.merge(self.prev_span())))
            }
            TokenKind::LBracket => {
                self.advance();
                if self.check(&TokenKind::RBracket) {
                    self.advance();
                    return Ok(Pattern::List(
                        Vec::new(),
                        None,
                        start.merge(self.prev_span()),
                    ));
                }
                let mut patterns = vec![self.parse_pattern()?];
                while self.check(&TokenKind::Comma) {
                    self.advance();
                    if self.check(&TokenKind::RBracket) || self.check(&TokenKind::Pipe) {
                        break;
                    }
                    patterns.push(self.parse_pattern()?);
                }
                let tail = if self.check(&TokenKind::Pipe) {
                    self.advance();
                    Some(Box::new(self.parse_pattern()?))
                } else {
                    None
                };
                self.expect(&TokenKind::RBracket)?;
                Ok(Pattern::List(patterns, tail, start.merge(self.prev_span())))
            }
            TokenKind::LtLt => {
                self.advance();
                let mut segments = Vec::new();
                if !self.check(&TokenKind::GtGt) {
                    loop {
                        segments.push(self.parse_bitstring_pattern_segment()?);
                        if !self.check(&TokenKind::Comma) {
                            break;
                        }
                        self.advance();
                    }
                }
                self.expect(&TokenKind::GtGt)?;
                Ok(Pattern::BitString(segments, start.merge(self.prev_span())))
            }
            _ => Err(self.error("Expected pattern")),
        }
    }

    fn parse_bitstring_expr_segment(&mut self) -> ParseResult<BitStringSegment> {
        let tokens = self.collect_bitstring_segment_tokens();
        let span = tokens
            .first()
            .zip(tokens.last())
            .map(|(first, last)| first.span.merge(last.span))
            .unwrap_or_else(Span::empty);
        let (value_tokens, size, specifier) = self.split_bitstring_segment_parts(tokens)?;
        let value = self.parse_expr_from_tokens(value_tokens)?;
        Ok(BitStringSegment {
            value,
            size,
            specifier,
            span,
        })
    }

    fn parse_bitstring_pattern_segment(&mut self) -> ParseResult<BitStringPatternSegment> {
        let tokens = self.collect_bitstring_segment_tokens();
        let span = tokens
            .first()
            .zip(tokens.last())
            .map(|(first, last)| first.span.merge(last.span))
            .unwrap_or_else(Span::empty);
        let (value_tokens, size, specifier) = self.split_bitstring_segment_parts(tokens)?;
        let value = self.parse_pattern_from_tokens(value_tokens)?;
        Ok(BitStringPatternSegment {
            value,
            size,
            specifier,
            span,
        })
    }

    fn collect_bitstring_segment_tokens(&mut self) -> Vec<Token> {
        let mut tokens = Vec::new();
        let mut depth = 0usize;

        loop {
            let token = self.advance().clone();
            match token.kind {
                TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace | TokenKind::LtLt => {
                    depth += 1;
                }
                TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace | TokenKind::GtGt => {
                    if depth == 0 {
                        self.pos -= 1;
                        break;
                    }
                    depth -= 1;
                }
                TokenKind::Comma if depth == 0 => {
                    self.pos -= 1;
                    break;
                }
                TokenKind::Eof => break,
                _ => {}
            }
            tokens.push(token);
        }

        tokens
    }

    fn split_bitstring_segment_parts(
        &self,
        mut tokens: Vec<Token>,
    ) -> ParseResult<(Vec<Token>, Option<u32>, BinarySegmentSpecifier)> {
        let mut specifier = BinarySegmentSpecifier::Integer;
        let mut size = None;

        if tokens.len() >= 2 {
            let slash_idx = tokens.len() - 2;
            if matches!(tokens[slash_idx].kind, TokenKind::Slash) {
                specifier = self.parse_binary_segment_specifier_token(&tokens[slash_idx + 1])?;
                tokens.truncate(slash_idx);
            }
        }

        if tokens.len() >= 2 {
            let colon_idx = tokens.len() - 2;
            if matches!(tokens[colon_idx].kind, TokenKind::Colon) {
                size = Some(self.parse_binary_segment_size_token(&tokens[colon_idx + 1])?);
                tokens.truncate(colon_idx);
            }
        }

        if tokens.is_empty() {
            return Err(ParseError::new(
                "Expected bitstring segment value".to_string(),
                self.current_span(),
            ));
        }

        Ok((tokens, size, specifier))
    }

    fn parse_expr_from_tokens(&self, mut tokens: Vec<Token>) -> ParseResult<Expr> {
        let eof_span = tokens
            .last()
            .map(|token| token.span)
            .unwrap_or_else(Span::empty);
        tokens.push(Token::new(TokenKind::Eof, eof_span));
        let mut parser = Parser::new_with_options(tokens, self.options);
        let expr = parser.parse_expr()?;
        if !parser.is_at_end() {
            return Err(ParseError::new(
                "Unexpected trailing tokens in bitstring segment".to_string(),
                parser.current_span(),
            ));
        }
        Ok(expr)
    }

    fn parse_pattern_from_tokens(&self, mut tokens: Vec<Token>) -> ParseResult<Pattern> {
        let eof_span = tokens
            .last()
            .map(|token| token.span)
            .unwrap_or_else(Span::empty);
        tokens.push(Token::new(TokenKind::Eof, eof_span));
        let mut parser = Parser::new_with_options(tokens, self.options);
        let pattern = parser.parse_pattern()?;
        if !parser.is_at_end() {
            return Err(ParseError::new(
                "Unexpected trailing tokens in bitstring pattern segment".to_string(),
                parser.current_span(),
            ));
        }
        Ok(pattern)
    }

    fn parse_binary_segment_size_token(&self, token: &Token) -> ParseResult<u32> {
        match token.kind {
            TokenKind::Int(n) if n >= 0 => Ok(n as u32),
            _ => Err(ParseError::new(
                "Expected non-negative integer binary segment size".to_string(),
                token.span,
            )),
        }
    }

    fn parse_binary_segment_specifier_token(
        &self,
        token: &Token,
    ) -> ParseResult<BinarySegmentSpecifier> {
        match &token.kind {
            TokenKind::Ident(name) | TokenKind::TypeIdent(name) => match name.as_str() {
                "int" | "integer" => Ok(BinarySegmentSpecifier::Integer),
                "big" => Ok(BinarySegmentSpecifier::BigInteger),
                "binary" => Ok(BinarySegmentSpecifier::Binary),
                "utf8" => Ok(BinarySegmentSpecifier::Utf8),
                _ => Err(ParseError::new(
                    format!("Unsupported binary segment specifier: {}", name),
                    token.span,
                )),
            },
            _ => Err(ParseError::new(
                "Expected binary segment specifier".to_string(),
                token.span,
            )),
        }
    }

    fn parse_empty_lambda(&mut self) -> ParseResult<Expr> {
        let start = self.current_span();
        self.expect(&TokenKind::Or)?; // ||

        let ret_type = if self.check(&TokenKind::Arrow) {
            self.advance();
            Some(self.parse_type()?)
        } else {
            None
        };

        let body = self.parse_expr()?;
        Ok(Expr::Lambda(
            Vec::new(),
            ret_type,
            Box::new(body),
            start.merge(self.prev_span()),
        ))
    }

    fn parse_lambda(&mut self) -> ParseResult<Expr> {
        let start = self.current_span();
        self.expect(&TokenKind::Pipe)?;
        let mut params = Vec::new();
        if !self.check(&TokenKind::Pipe) {
            loop {
                let pstart = self.current_span();
                let name = self.expect_ident()?;
                let ty = if self.check(&TokenKind::Colon) {
                    self.advance();
                    Some(self.parse_type()?)
                } else {
                    None
                };
                params.push(Param {
                    name,
                    ty,
                    span: pstart.merge(self.prev_span()),
                });
                if !self.check(&TokenKind::Comma) {
                    break;
                }
                self.advance();
            }
        }
        self.expect(&TokenKind::Pipe)?;

        let ret_type = if self.check(&TokenKind::Arrow) {
            self.advance();
            Some(self.parse_type()?)
        } else {
            None
        };

        let body = self.parse_expr()?;
        Ok(Expr::Lambda(
            params,
            ret_type,
            Box::new(body),
            start.merge(self.prev_span()),
        ))
    }

    /// Parse a try expression: try { body } catch { pattern => handler }
    fn parse_try_expr(&mut self) -> ParseResult<Expr> {
        let start = self.current_span();
        self.expect(&TokenKind::Try)?;

        // Parse the try body as a block
        let body = self.parse_block_expr()?;

        // Expect catch keyword
        self.expect(&TokenKind::Catch)?;

        // Parse catch arms in braces
        self.expect(&TokenKind::LBrace)?;

        let mut catch_arms = Vec::new();
        while !self.check(&TokenKind::RBrace) {
            let arm_start = self.current_span();

            // Check for class:pattern or just pattern
            let (class, pattern) = if self.check(&TokenKind::Colon) {
                // Just :atom as pattern
                (None, self.parse_pattern()?)
            } else {
                // Could be class:pattern or just pattern
                let first = self.parse_pattern()?;
                if self.check(&TokenKind::Colon) {
                    self.advance();
                    // It was class:pattern
                    let class_name = match &first {
                        Pattern::Var(name, _) => name.clone(),
                        _ => {
                            return Err(self.error("Expected exception class (error, throw, exit)"));
                        }
                    };
                    let pattern = self.parse_pattern()?;
                    (Some(class_name), pattern)
                } else {
                    // Just pattern
                    (None, first)
                }
            };

            self.expect(&TokenKind::FatArrow)?;
            let body = self.parse_expr()?;

            catch_arms.push(CatchArm {
                class,
                pattern,
                body,
                span: arm_start.merge(self.prev_span()),
            });

            // Allow comma between arms
            if self.check(&TokenKind::Comma) {
                self.advance();
            }
        }

        self.expect(&TokenKind::RBrace)?;

        Ok(Expr::Try {
            body: Box::new(body),
            catch_arms,
            span: start.merge(self.prev_span()),
        })
    }

    /// Parse a map expression: %{key => value, ...}
    fn parse_map_expr(&mut self) -> ParseResult<Expr> {
        let start = self.current_span();
        self.expect(&TokenKind::HashBrace)?;

        let mut entries = Vec::new();

        if !self.check(&TokenKind::RBrace) {
            loop {
                let key = self.parse_expr()?;
                self.expect(&TokenKind::FatArrow)?;
                let value = self.parse_expr()?;
                entries.push((key, value));

                if !self.check(&TokenKind::Comma) {
                    break;
                }
                self.advance();
                if self.check(&TokenKind::RBrace) {
                    break;
                }
            }
        }

        self.expect(&TokenKind::RBrace)?;
        Ok(Expr::Map(entries, start.merge(self.prev_span())))
    }

    /// Parse a list comprehension after seeing `[expr for`
    fn parse_list_comp(&mut self, expr: Expr, start: Span) -> ParseResult<Expr> {
        let mut generators = Vec::new();
        let mut filters = Vec::new();

        // Parse generators: for pattern in source
        while self.check(&TokenKind::For) {
            self.advance();
            let gen_start = self.current_span();
            let pattern = self.parse_pattern()?;
            self.expect(&TokenKind::In)?;
            let source = self.parse_expr()?;
            generators.push(Generator {
                pattern,
                source,
                span: gen_start.merge(self.prev_span()),
            });

            // Allow comma between generators
            if self.check(&TokenKind::Comma) {
                self.advance();
            }
        }

        // Parse filters: if condition
        while self.check(&TokenKind::If) {
            self.advance();
            filters.push(self.parse_expr()?);

            // Allow comma between filters
            if self.check(&TokenKind::Comma) {
                self.advance();
            }
        }

        self.expect(&TokenKind::RBracket)?;

        Ok(Expr::ListComp {
            expr: Box::new(expr),
            generators,
            filters,
            span: start.merge(self.prev_span()),
        })
    }

    fn parse_interpolated_parts(
        &mut self,
        parts: Vec<StringPart>,
    ) -> ParseResult<Vec<InterpolatedPart>> {
        let mut result = Vec::new();
        for part in parts {
            match part {
                StringPart::Literal(s) => {
                    result.push(InterpolatedPart::Literal(s));
                }
                StringPart::Expr(tokens) => {
                    // Parse the tokens as an expression
                    let mut sub_parser = Parser::new(tokens);
                    let expr = sub_parser.parse_expr()?;
                    result.push(InterpolatedPart::Expr(Box::new(expr)));
                }
            }
        }
        Ok(result)
    }

    // Helper methods

    fn peek(&mut self) -> &TokenKind {
        if self.pending_type_gt > 0 {
            return &TokenKind::Gt;
        }
        self.skip_newlines();
        self.tokens
            .get(self.pos)
            .map(|t| &t.kind)
            .unwrap_or(&TokenKind::Eof)
    }

    /// Peek without skipping newlines - for checking call continuation
    fn peek_no_skip(&self) -> &TokenKind {
        self.tokens
            .get(self.pos)
            .map(|t| &t.kind)
            .unwrap_or(&TokenKind::Eof)
    }

    fn check(&mut self, kind: &TokenKind) -> bool {
        std::mem::discriminant(self.peek()) == std::mem::discriminant(kind)
    }

    /// Check without skipping newlines
    fn check_no_skip(&self, kind: &TokenKind) -> bool {
        std::mem::discriminant(self.peek_no_skip()) == std::mem::discriminant(kind)
    }

    fn advance(&mut self) -> &Token {
        self.skip_newlines();
        if !self.is_at_end() {
            self.pos += 1;
        }
        &self.tokens[self.pos - 1]
    }

    fn is_at_end(&mut self) -> bool {
        matches!(self.peek(), TokenKind::Eof)
    }

    fn current_span(&self) -> Span {
        if self.pending_type_gt > 0 {
            return self.prev_span();
        }
        self.tokens
            .get(self.pos)
            .map(|t| t.span)
            .unwrap_or(Span::empty())
    }

    fn prev_span(&self) -> Span {
        if self.pos > 0 {
            self.tokens[self.pos - 1].span
        } else {
            Span::empty()
        }
    }

    fn expect(&mut self, kind: &TokenKind) -> ParseResult<&Token> {
        if self.check(kind) {
            Ok(self.advance())
        } else {
            Err(self.error(format!("Expected {:?}", kind)))
        }
    }

    fn expect_type_gt(&mut self) -> ParseResult<()> {
        if self.pending_type_gt > 0 {
            self.pending_type_gt -= 1;
            return Ok(());
        }

        self.skip_newlines();
        match self.peek().clone() {
            TokenKind::Gt => {
                self.advance();
                Ok(())
            }
            TokenKind::GtGt => {
                self.advance();
                self.pending_type_gt += 1;
                Ok(())
            }
            _ => Err(self.error("Expected Gt")),
        }
    }

    fn expect_ident(&mut self) -> ParseResult<Ident> {
        match self.peek().clone() {
            TokenKind::Ident(name) => {
                self.advance();
                Ok(name)
            }
            _ => Err(self.error("Expected identifier")),
        }
    }

    fn expect_type_ident(&mut self) -> ParseResult<Ident> {
        match self.peek().clone() {
            TokenKind::TypeIdent(name) => {
                self.advance();
                Ok(name)
            }
            _ => Err(self.error("Expected type identifier")),
        }
    }

    fn expect_type_ident_or_ident(&mut self) -> ParseResult<Ident> {
        match self.peek().clone() {
            TokenKind::TypeIdent(name) | TokenKind::Ident(name) => {
                self.advance();
                Ok(name)
            }
            _ => Err(self.error("Expected identifier")),
        }
    }

    fn expect_ident_or_type_ident(&mut self) -> ParseResult<Ident> {
        self.expect_type_ident_or_ident()
    }

    fn error(&self, message: impl Into<String>) -> ParseError {
        ParseError::new(message, self.current_span())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::lexer::Lexer;

    fn parse(source: &str) -> ParseResult<Module> {
        let tokens = Lexer::new(source).tokenize();
        Parser::new(tokens).parse_module()
    }

    #[test]
    fn test_simple_function() {
        let module = parse("fn main() { 42 }").unwrap();
        assert_eq!(module.items.len(), 1);
    }

    #[test]
    fn test_function_with_params() {
        let module = parse("fn add(a: Int, b: Int) -> Int { a + b }").unwrap();
        assert_eq!(module.items.len(), 1);
    }

    #[test]
    fn test_enum() {
        let module = parse(
            "enum Option<T> {
                Some(T),
                None,
            }",
        )
        .unwrap();
        assert_eq!(module.items.len(), 1);
    }

    #[test]
    fn test_match_expr() {
        let module = parse(
            "fn test(x: Int) -> Int {
                match x {
                    0 => 1,
                    n => n * 2,
                }
            }",
        )
        .unwrap();
        assert_eq!(module.items.len(), 1);
    }

    #[test]
    fn test_if_expr() {
        let module = parse(
            "fn test(x: Int) -> Int {
                if x > 0 {
                    x
                } else {
                    0
                }
            }",
        )
        .unwrap();
        assert_eq!(module.items.len(), 1);
    }

    #[test]
    fn test_binary_segments_parse() {
        let module = parse(
            r#"
            fn main() {
                match <<"ABCD">> {
                    <<0x41:8, rest>> => rest,
                    _ => <<"">>,
                }
            }
            "#,
        )
        .unwrap();

        let Item::Function(function) = &module.items[0] else {
            panic!("expected function item");
        };
        let Expr::Block(_, Some(body), _) = &function.body else {
            panic!("expected block body");
        };
        let Expr::Match(_, arms, _) = body.as_ref() else {
            panic!("expected match body");
        };
        let Pattern::BitString(segments, _) = &arms[0].pattern else {
            panic!("expected binary pattern");
        };
        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0].size, Some(8));
        assert_eq!(segments[1].size, None);
    }

    #[test]
    fn test_binary_segment_specifiers_parse() {
        let module = parse(
            r#"
            fn main() {
                <<65/utf8, "BC"/binary, 1:16/integer, 2:32/big>>
            }
            "#,
        )
        .unwrap();

        let Item::Function(function) = &module.items[0] else {
            panic!("expected function item");
        };
        let Expr::Block(_, Some(body), _) = &function.body else {
            panic!("expected block body");
        };
        let Expr::BitString(segments, _) = body.as_ref() else {
            panic!("expected binary literal");
        };
        assert!(matches!(
            segments[0].specifier,
            BinarySegmentSpecifier::Utf8
        ));
        assert!(matches!(
            segments[1].specifier,
            BinarySegmentSpecifier::Binary
        ));
        assert!(matches!(
            segments[2].specifier,
            BinarySegmentSpecifier::Integer
        ));
        assert_eq!(segments[2].size, Some(16));
        assert!(matches!(
            segments[3].specifier,
            BinarySegmentSpecifier::BigInteger
        ));
        assert_eq!(segments[3].size, Some(32));
    }

    #[test]
    fn test_nested_generic_type_annotations_parse() {
        let module = parse(
            r#"
            fn decode_name(payload: Dynamic) -> DynamicResult<DynamicOption<String>> {
                DynamicResult::Err("todo")
            }
            "#,
        )
        .unwrap();

        let Item::Function(function) = &module.items[0] else {
            panic!("expected function item");
        };
        let Some(TypeExpr::Named(name, args, _)) = &function.return_type else {
            panic!("expected named return type");
        };
        assert_eq!(name, "DynamicResult");
        assert_eq!(args.len(), 1);
        let TypeExpr::Named(inner, inner_args, _) = &args[0] else {
            panic!("expected nested generic argument");
        };
        assert_eq!(inner, "DynamicOption");
        assert_eq!(inner_args.len(), 1);
    }

    #[test]
    fn test_extern_rejected_when_disabled() {
        let tokens = Lexer::new(
            r#"
            extern "erlang" {
                fn lists::reverse(List<Int>) -> List<Int>
            }
            "#,
        )
        .tokenize();
        let mut parser = Parser::new_with_options(
            tokens,
            ParserOptions {
                allow_extern: false,
            },
        );
        let err = parser.parse_module().unwrap_err();
        assert!(err.message.contains("disabled in sandbox mode"));
    }
}
