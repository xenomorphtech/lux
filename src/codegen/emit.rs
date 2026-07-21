use crate::codegen::erlang::*;
use std::fmt::Write;

pub struct Emitter {
    output: String,
    indent: usize,
}

impl Emitter {
    pub fn new() -> Self {
        Emitter {
            output: String::new(),
            indent: 0,
        }
    }

    pub fn emit_module(&mut self, module: &CoreModule) -> String {
        // Module declaration
        writeln!(&mut self.output, "module '{}'", module.name).unwrap();

        // Exports
        write!(&mut self.output, "    [").unwrap();
        for (i, (name, arity)) in module.exports.iter().enumerate() {
            if i > 0 {
                write!(&mut self.output, ", ").unwrap();
            }
            write!(&mut self.output, "'{}'/{}", name, arity).unwrap();
        }
        writeln!(&mut self.output, "]").unwrap();

        writeln!(&mut self.output, "attributes []").unwrap();
        writeln!(&mut self.output).unwrap();

        // Functions
        for func in &module.functions {
            self.emit_fundef(func);
            writeln!(&mut self.output).unwrap();
        }

        writeln!(&mut self.output, "end").unwrap();

        std::mem::take(&mut self.output)
    }

    fn emit_fundef(&mut self, func: &CoreFunDef) {
        write!(&mut self.output, "'{}'/{} = ", func.name, func.arity).unwrap();
        write!(&mut self.output, "fun (").unwrap();
        for (i, param) in func.params.iter().enumerate() {
            if i > 0 {
                write!(&mut self.output, ", ").unwrap();
            }
            write!(&mut self.output, "{}", param).unwrap();
        }
        write!(&mut self.output, ") -> ").unwrap();
        self.emit_expr(&func.body);
        writeln!(&mut self.output).unwrap();
    }

    fn emit_expr(&mut self, expr: &CoreExpr) {
        match expr {
            CoreExpr::Lit(lit) => self.emit_lit(lit),

            CoreExpr::Var(name) => {
                write!(&mut self.output, "{}", name).unwrap();
            }

            CoreExpr::LocalFunRef(name, arity) => {
                write!(&mut self.output, "'{}'/{}", name, arity).unwrap();
            }

            CoreExpr::RemoteFunRef(module, name, arity) => {
                write!(&mut self.output, "'{}':'{}'/{}", module, name, arity).unwrap();
            }

            CoreExpr::Tuple(elems) => {
                write!(&mut self.output, "{{").unwrap();
                for (i, elem) in elems.iter().enumerate() {
                    if i > 0 {
                        write!(&mut self.output, ", ").unwrap();
                    }
                    self.emit_expr(elem);
                }
                write!(&mut self.output, "}}").unwrap();
            }

            CoreExpr::List(elems, tail) => {
                write!(&mut self.output, "[").unwrap();
                for (i, elem) in elems.iter().enumerate() {
                    if i > 0 {
                        write!(&mut self.output, ", ").unwrap();
                    }
                    self.emit_expr(elem);
                }
                write!(&mut self.output, "|").unwrap();
                self.emit_expr(tail);
                write!(&mut self.output, "]").unwrap();
            }

            CoreExpr::Cons(head, tail) => {
                write!(&mut self.output, "[").unwrap();
                self.emit_expr(head);
                write!(&mut self.output, "|").unwrap();
                self.emit_expr(tail);
                write!(&mut self.output, "]").unwrap();
            }

            CoreExpr::Map(entries) => {
                write!(&mut self.output, "~{{").unwrap();
                for (i, (key, val)) in entries.iter().enumerate() {
                    if i > 0 {
                        write!(&mut self.output, ",").unwrap();
                    }
                    self.emit_expr(key);
                    write!(&mut self.output, "=>").unwrap();
                    self.emit_expr(val);
                }
                write!(&mut self.output, "}}~").unwrap();
            }

            CoreExpr::Binary(elements) => {
                write!(&mut self.output, "#{{").unwrap();
                for (i, elem) in elements.iter().enumerate() {
                    if i > 0 {
                        write!(&mut self.output, ",").unwrap();
                    }
                    self.emit_binary_segment(elem);
                }
                write!(&mut self.output, "}}#").unwrap();
            }

            CoreExpr::Fun(params, body) => {
                write!(&mut self.output, "fun (").unwrap();
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        write!(&mut self.output, ", ").unwrap();
                    }
                    write!(&mut self.output, "{}", p).unwrap();
                }
                write!(&mut self.output, ") -> ").unwrap();
                self.emit_expr(body);
            }

            CoreExpr::Let(bindings, body) => {
                write!(&mut self.output, "let <").unwrap();
                for (i, (name, _)) in bindings.iter().enumerate() {
                    if i > 0 {
                        write!(&mut self.output, ", ").unwrap();
                    }
                    write!(&mut self.output, "{}", name).unwrap();
                }
                write!(&mut self.output, "> = <").unwrap();
                for (i, (_, val)) in bindings.iter().enumerate() {
                    if i > 0 {
                        write!(&mut self.output, ", ").unwrap();
                    }
                    self.emit_expr(val);
                }
                write!(&mut self.output, "> in ").unwrap();
                self.emit_expr(body);
            }

            CoreExpr::Apply(func, args) => {
                write!(&mut self.output, "apply ").unwrap();
                self.emit_expr(func);
                write!(&mut self.output, " (").unwrap();
                for (i, arg) in args.iter().enumerate() {
                    if i > 0 {
                        write!(&mut self.output, ", ").unwrap();
                    }
                    self.emit_expr(arg);
                }
                write!(&mut self.output, ")").unwrap();
            }

            CoreExpr::Call(module, func, args) => {
                write!(&mut self.output, "call '{}':'{}'(", module, func).unwrap();
                for (i, arg) in args.iter().enumerate() {
                    if i > 0 {
                        write!(&mut self.output, ", ").unwrap();
                    }
                    self.emit_expr(arg);
                }
                write!(&mut self.output, ")").unwrap();
            }

            CoreExpr::Case(scrutinee, clauses) => {
                write!(&mut self.output, "case ").unwrap();
                self.emit_expr(scrutinee);
                writeln!(&mut self.output, " of").unwrap();
                self.indent += 1;
                for clause in clauses {
                    self.emit_clause(clause);
                }
                self.indent -= 1;
                self.emit_indent();
                write!(&mut self.output, "end").unwrap();
            }

            CoreExpr::Receive { clauses, timeout } => {
                // Generate receive using primops (required for modern Core Erlang)
                self.emit_receive_primop(
                    clauses,
                    timeout
                        .as_ref()
                        .map(|(ms, body)| (ms.as_ref(), body.as_ref())),
                );
            }

            CoreExpr::Primop(op, args) => {
                write!(&mut self.output, "primop '{}' (", op).unwrap();
                for (i, arg) in args.iter().enumerate() {
                    if i > 0 {
                        write!(&mut self.output, ", ").unwrap();
                    }
                    self.emit_expr(arg);
                }
                write!(&mut self.output, ")").unwrap();
            }

            CoreExpr::Seq(first, second) => {
                write!(&mut self.output, "do ").unwrap();
                self.emit_expr(first);
                writeln!(&mut self.output).unwrap();
                self.emit_indent();
                self.emit_expr(second);
            }

            CoreExpr::Try {
                body,
                vars,
                handler,
                evars,
                catch,
            } => {
                write!(&mut self.output, "try ").unwrap();
                self.emit_expr(body);
                write!(&mut self.output, " of <").unwrap();
                write!(&mut self.output, "{}", vars.join(", ")).unwrap();
                write!(&mut self.output, "> -> ").unwrap();
                self.emit_expr(handler);
                write!(&mut self.output, " catch <").unwrap();
                write!(&mut self.output, "{}", evars.join(", ")).unwrap();
                write!(&mut self.output, "> -> ").unwrap();
                self.emit_expr(catch);
            }
        }
    }

    fn emit_lit(&mut self, lit: &CoreLit) {
        match lit {
            CoreLit::Int(n) => write!(&mut self.output, "{}", n).unwrap(),
            CoreLit::Float(f) => write!(&mut self.output, "{}", f).unwrap(),
            CoreLit::Atom(a) => write!(&mut self.output, "'{}'", a).unwrap(),
            CoreLit::String(s) => write!(&mut self.output, "\"{}\"", escape_string(s)).unwrap(),
            CoreLit::Nil => write!(&mut self.output, "[]").unwrap(),
        }
    }

    fn emit_clause(&mut self, clause: &CoreClause) {
        self.emit_indent();
        write!(&mut self.output, "<").unwrap();
        for (i, pat) in clause.patterns.iter().enumerate() {
            if i > 0 {
                write!(&mut self.output, ", ").unwrap();
            }
            self.emit_pattern(pat);
        }
        write!(&mut self.output, "> when ").unwrap();
        self.emit_expr(&clause.guard);
        write!(&mut self.output, " -> ").unwrap();
        self.emit_expr(&clause.body);
        writeln!(&mut self.output).unwrap();
    }

    fn emit_pattern(&mut self, pat: &CorePattern) {
        match pat {
            CorePattern::Lit(lit) => self.emit_lit(lit),
            CorePattern::Var(name) => write!(&mut self.output, "{}", name).unwrap(),
            CorePattern::Tuple(pats) => {
                write!(&mut self.output, "{{").unwrap();
                for (i, p) in pats.iter().enumerate() {
                    if i > 0 {
                        write!(&mut self.output, ", ").unwrap();
                    }
                    self.emit_pattern(p);
                }
                write!(&mut self.output, "}}").unwrap();
            }
            CorePattern::Cons(head, tail) => {
                write!(&mut self.output, "[").unwrap();
                self.emit_pattern(head);
                write!(&mut self.output, "|").unwrap();
                self.emit_pattern(tail);
                write!(&mut self.output, "]").unwrap();
            }
            CorePattern::Nil => write!(&mut self.output, "[]").unwrap(),
            CorePattern::Binary(segments) => {
                write!(&mut self.output, "#{{").unwrap();
                for (i, segment) in segments.iter().enumerate() {
                    if i > 0 {
                        write!(&mut self.output, ",").unwrap();
                    }
                    self.emit_binary_pattern_segment(segment);
                }
                write!(&mut self.output, "}}#").unwrap();
            }
            CorePattern::Alias(name, pat) => {
                write!(&mut self.output, "{} = ", name).unwrap();
                self.emit_pattern(pat);
            }
        }
    }

    fn emit_binary_segment(&mut self, segment: &CoreBinarySegment) {
        write!(&mut self.output, "#<").unwrap();
        self.emit_expr(&segment.value);
        self.emit_binary_segment_suffix(segment.size.clone(), segment.kind);
    }

    fn emit_binary_pattern_segment(&mut self, segment: &CoreBinaryPatternSegment) {
        write!(&mut self.output, "#<").unwrap();
        self.emit_pattern(&segment.pattern);
        self.emit_binary_segment_suffix(segment.size.clone(), segment.kind);
    }

    fn emit_binary_segment_suffix(&mut self, size: CoreBinarySize, kind: CoreBinaryKind) {
        write!(&mut self.output, ">(").unwrap();
        match kind {
            CoreBinaryKind::Utf8 => {
                write!(
                    &mut self.output,
                    "'undefined','undefined','utf8',['unsigned'|['big']])"
                )
                .unwrap();
            }
            CoreBinaryKind::BigInteger => {
                let unit = self.binary_segment_unit(kind);
                self.emit_binary_segment_size(&size);
                write!(&mut self.output, ",{},", unit).unwrap();
                self.emit_binary_segment_kind(kind);
                write!(&mut self.output, ",['unsigned'|['big']])").unwrap();
            }
            _ => {
                let unit = self.binary_segment_unit(kind);
                self.emit_binary_segment_size(&size);
                write!(&mut self.output, ",{},", unit).unwrap();
                self.emit_binary_segment_kind(kind);
                write!(&mut self.output, ",['unsigned'|['little']])").unwrap();
            }
        }
    }

    fn emit_binary_segment_size(&mut self, size: &CoreBinarySize) {
        match size {
            CoreBinarySize::Bits(bits) => write!(&mut self.output, "{}", bits).unwrap(),
            CoreBinarySize::All => write!(&mut self.output, "'all'").unwrap(),
        }
    }

    fn emit_binary_segment_kind(&mut self, kind: CoreBinaryKind) {
        match kind {
            CoreBinaryKind::Integer => write!(&mut self.output, "'integer'").unwrap(),
            CoreBinaryKind::BigInteger => write!(&mut self.output, "'integer'").unwrap(),
            CoreBinaryKind::Binary => write!(&mut self.output, "'binary'").unwrap(),
            CoreBinaryKind::Utf8 => write!(&mut self.output, "'utf8'").unwrap(),
        }
    }

    fn binary_segment_unit(&self, kind: CoreBinaryKind) -> u8 {
        match kind {
            CoreBinaryKind::Integer => 1,
            CoreBinaryKind::BigInteger => 1,
            CoreBinaryKind::Binary => 8,
            CoreBinaryKind::Utf8 => 1,
        }
    }

    fn emit_indent(&mut self) {
        for _ in 0..self.indent {
            write!(&mut self.output, "    ").unwrap();
        }
    }

    /// Generate receive using the primop-based approach required by modern Core Erlang
    fn emit_receive_primop(
        &mut self,
        clauses: &[CoreClause],
        timeout: Option<(&CoreExpr, &CoreExpr)>,
    ) {
        // Generate a letrec with a receive loop matching Erlang compiler output format exactly
        // The letrec_goto annotation tells the compiler this is a receive loop
        writeln!(&mut self.output, "( letrec").unwrap();
        self.indent += 1;
        self.emit_indent();
        writeln!(&mut self.output, "'recv$^0'/0 =").unwrap();
        self.indent += 1;
        self.emit_indent();
        writeln!(&mut self.output, "fun () ->").unwrap();
        self.indent += 1;

        self.emit_indent();
        writeln!(&mut self.output, "let <_has_msg,_msg> =").unwrap();
        self.indent += 1;
        self.emit_indent();
        writeln!(&mut self.output, "primop 'recv_peek_message' ()").unwrap();
        self.indent -= 1;
        self.emit_indent();
        writeln!(&mut self.output, "in case _has_msg of").unwrap();
        self.indent += 1;

        // Case: message available
        self.emit_indent();
        writeln!(&mut self.output, "<'true'> when 'true' ->").unwrap();
        self.indent += 1;
        self.emit_indent();
        writeln!(&mut self.output, "case _msg of").unwrap();
        self.indent += 1;

        // Emit user clauses
        for clause in clauses {
            self.emit_indent();
            write!(&mut self.output, "<").unwrap();
            for (i, pat) in clause.patterns.iter().enumerate() {
                if i > 0 {
                    write!(&mut self.output, ", ").unwrap();
                }
                self.emit_pattern(pat);
            }
            write!(&mut self.output, "> when ").unwrap();
            self.emit_expr(&clause.guard);
            writeln!(&mut self.output, " ->").unwrap();
            self.indent += 1;
            self.emit_indent();
            write!(&mut self.output, "do primop 'remove_message' () ").unwrap();
            self.emit_expr(&clause.body);
            writeln!(&mut self.output).unwrap();
            self.indent -= 1;
        }

        // Default case: no match, try next message
        self.emit_indent();
        writeln!(&mut self.output, "<_Other> when 'true' ->").unwrap();
        self.indent += 1;
        self.emit_indent();
        writeln!(
            &mut self.output,
            "do primop 'recv_next' () apply 'recv$^0'/0 ()"
        )
        .unwrap();
        self.indent -= 1;

        self.indent -= 1;
        self.emit_indent();
        writeln!(&mut self.output, "end").unwrap();
        self.indent -= 1;

        // Case: no message available
        self.emit_indent();
        writeln!(&mut self.output, "<'false'> when 'true' ->").unwrap();
        self.indent += 1;
        self.emit_indent();
        writeln!(&mut self.output, "let <_timeout_result> =").unwrap();
        self.indent += 1;
        self.emit_indent();

        // Emit timeout value or infinity
        write!(&mut self.output, "primop 'recv_wait_timeout' (").unwrap();
        if let Some((timeout_ms, _)) = timeout {
            self.emit_expr(timeout_ms);
        } else {
            write!(&mut self.output, "'infinity'").unwrap();
        }
        writeln!(&mut self.output, ")").unwrap();

        self.indent -= 1;
        self.emit_indent();
        writeln!(&mut self.output, "in case _timeout_result of").unwrap();
        self.indent += 1;
        self.emit_indent();

        // Timeout occurred - emit timeout body or default
        write!(&mut self.output, "<'true'> when 'true' -> ").unwrap();
        if let Some((_, timeout_body)) = timeout {
            self.emit_expr(timeout_body);
        } else {
            write!(&mut self.output, "'timeout'").unwrap();
        }
        writeln!(&mut self.output).unwrap();

        self.emit_indent();
        writeln!(
            &mut self.output,
            "<'false'> when 'true' -> apply 'recv$^0'/0 ()"
        )
        .unwrap();
        self.indent -= 1;
        self.emit_indent();
        writeln!(&mut self.output, "end").unwrap();
        self.indent -= 1;

        self.indent -= 1;
        self.emit_indent();
        writeln!(&mut self.output, "end").unwrap();

        // Close letrec - go back to the letrec level
        self.indent -= 1;
        self.indent -= 1;
        self.emit_indent();
        writeln!(&mut self.output, "in apply 'recv$^0'/0 ()").unwrap();
        self.indent -= 1;
        self.emit_indent();
        write!(&mut self.output, "-| ['letrec_goto'] )").unwrap();
    }
}

impl Default for Emitter {
    fn default() -> Self {
        Self::new()
    }
}

fn escape_string(s: &str) -> String {
    let mut result = String::new();
    for c in s.chars() {
        match c {
            '\n' => result.push_str("\\n"),
            '\t' => result.push_str("\\t"),
            '\r' => result.push_str("\\r"),
            '\\' => result.push_str("\\\\"),
            '"' => result.push_str("\\\""),
            c => result.push(c),
        }
    }
    result
}
