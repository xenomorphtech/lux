//! Yggdrasil bytecode backend.
//!
//! Lux lowers source functions into content-addressed Core Erlang modules. The
//! Yggdrasil backend preserves that boundary: every Core module becomes one
//! `.yggm` module and fully-qualified Core calls become Yggdrasil `CALL_EXT`
//! instructions. This lets Yggdrasil's module table retain responsibility for
//! hot-code lookup while its verifier remains the execution safety boundary.

#[allow(unused_imports)]
use crate::prelude::*;
use crate::collections::HashMap;
use core::fmt;

use ygg_bytecode::verify::{self, VerifyError};
use ygg_bytecode::{CodeBuilder, Function, Module, op};

use crate::codegen::erlang::{
    CoreBinaryKind, CoreBinarySize, CoreClause, CoreExpr, CoreFunDef, CoreLit, CoreModule,
    CorePattern,
};

/// One named Yggdrasil module ready to encode as a `.yggm` artifact.
#[derive(Debug, Clone)]
pub struct YggdrasilModule {
    pub name: String,
    pub module: Module,
}

impl YggdrasilModule {
    pub fn encode(&self) -> Vec<u8> {
        self.module.encode()
    }
}

/// A complete set of modules plus the translated Lux entry point.
#[derive(Debug, Clone)]
pub struct YggdrasilOutput {
    pub modules: Vec<YggdrasilModule>,
    pub entry_module: Option<String>,
    pub entry_arity: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum YggdrasilError {
    Unsupported {
        module: String,
        function: String,
        feature: String,
    },
    UnboundVariable {
        module: String,
        function: String,
        variable: String,
    },
    UnknownLocalFunction {
        module: String,
        function: String,
        callee: String,
        arity: usize,
    },
    TooManyFunctions {
        module: String,
        count: usize,
    },
    TooManyArguments {
        module: String,
        function: String,
        count: usize,
    },
    RegisterLimit {
        module: String,
        function: String,
    },
    MissingLabel {
        module: String,
        function: String,
        label: u32,
    },
    MissingEntryModule(String),
    Verification {
        module: String,
        error: VerifyError,
    },
}

impl fmt::Display for YggdrasilError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported {
                module,
                function,
                feature,
            } => {
                write!(
                    f,
                    "{module}:{function}: unsupported by Yggdrasil backend: {feature}"
                )
            }
            Self::UnboundVariable {
                module,
                function,
                variable,
            } => {
                write!(f, "{module}:{function}: unbound Core variable `{variable}`")
            }
            Self::UnknownLocalFunction {
                module,
                function,
                callee,
                arity,
            } => write!(
                f,
                "{module}:{function}: unknown local function {callee}/{arity}"
            ),
            Self::TooManyFunctions { module, count } => write!(
                f,
                "{module}: {count} functions exceed Yggdrasil's u32 function-index limit"
            ),
            Self::TooManyArguments {
                module,
                function,
                count,
            } => write!(
                f,
                "{module}:{function}: {count} arguments exceed Yggdrasil's 255-argument limit"
            ),
            Self::RegisterLimit { module, function } => write!(
                f,
                "{module}:{function}: requires more than Yggdrasil's 255 registers"
            ),
            Self::MissingLabel {
                module,
                function,
                label,
            } => {
                write!(f, "{module}:{function}: unresolved bytecode label {label}")
            }
            Self::MissingEntryModule(module) => {
                write!(f, "translated entry module `{module}` was not compiled")
            }
            Self::Verification { module, error } => {
                write!(
                    f,
                    "Yggdrasil rejected generated module `{module}`: {error:?}"
                )
            }
        }
    }
}

impl core::error::Error for YggdrasilError {}

/// Compiles Lux's Core Erlang IR to Yggdrasil's verified register bytecode.
pub struct YggdrasilCompiler;

impl YggdrasilCompiler {
    pub fn compile(
        modules: &[CoreModule],
        entry_module: Option<&str>,
        entry_arity: usize,
    ) -> Result<YggdrasilOutput, YggdrasilError> {
        let mut output = Vec::with_capacity(modules.len());
        for core in modules {
            let module = ModuleCompiler::new(core)?.compile()?;
            verify::verify(&module).map_err(|error| YggdrasilError::Verification {
                module: core.name.clone(),
                error,
            })?;
            output.push(YggdrasilModule {
                name: core.name.clone(),
                module,
            });
        }

        if let Some(entry) = entry_module
            && !output.iter().any(|module| module.name == entry)
        {
            return Err(YggdrasilError::MissingEntryModule(entry.to_owned()));
        }

        Ok(YggdrasilOutput {
            modules: output,
            entry_module: entry_module.map(str::to_owned),
            entry_arity,
        })
    }
}

#[derive(Default)]
struct AtomTable {
    names: Vec<String>,
    indices: HashMap<String, u32>,
}

impl AtomTable {
    fn intern(&mut self, atom: &str) -> u32 {
        if let Some(index) = self.indices.get(atom) {
            return *index;
        }
        let index = self.names.len() as u32;
        self.names.push(atom.to_owned());
        self.indices.insert(atom.to_owned(), index);
        index
    }
}

/// Does `expr`, in the given tail context, contain a *non-tail* self-call
/// (which compiles to an internal `op::CALL`)? Mirrors `compile_expr`'s tail
/// propagation exactly.
///
/// Why it matters: a tail call compiles to a stash + TAIL_SENTINEL return,
/// and the engines' internal-CALL arms *propagate* the sentinel, unwinding
/// the caller's frame and abandoning its pending work (the cons an
/// `[x | recurse(rest)]` was about to build, the concat after a recursive
/// call, ...). External call sites are always contained — the dynamic path
/// runs the trampoline loop and bound callers resume inline — so the only
/// hazardous mix is internal: a module with both a non-tail self-call and a
/// tail call. Such modules get their tail calls demoted to plain contained
/// calls (`demote_tails`): native stack grows with data size there, but the
/// results stop being silently wrong.
fn has_nontail_self_call(module_name: &str, expr: &CoreExpr, in_tail: bool) -> bool {
    let has = |e: &CoreExpr, tail: bool| has_nontail_self_call(module_name, e, tail);
    let any = |es: &[CoreExpr]| es.iter().any(|e| has(e, false));
    let clauses_have = |clauses: &[CoreClause], tail: bool| {
        clauses
            .iter()
            .any(|c| has(&c.guard, false) || has(&c.body, tail))
    };
    match expr {
        CoreExpr::Lit(_)
        | CoreExpr::Var(_)
        | CoreExpr::LocalFunRef(_, _)
        | CoreExpr::RemoteFunRef(_, _, _) => false,
        CoreExpr::Tuple(es) | CoreExpr::Primop(_, es) => any(es),
        CoreExpr::List(es, tail) => any(es) || has(tail, false),
        CoreExpr::Cons(h, t) => has(h, false) || has(t, false),
        CoreExpr::Map(entries) => entries.iter().any(|(k, v)| has(k, false) || has(v, false)),
        CoreExpr::Binary(segments) => segments.iter().any(|s| has(&s.value, false)),
        CoreExpr::Apply(callee, args) => {
            let is_self = match callee.as_ref() {
                CoreExpr::LocalFunRef(_, arity) => *arity == args.len(),
                CoreExpr::RemoteFunRef(module, _, arity) => {
                    *arity == args.len() && module == module_name
                }
                _ => false,
            };
            (is_self && !in_tail) || any(args)
        }
        CoreExpr::Call(module, _, args) => (module == module_name && !in_tail) || any(args),
        CoreExpr::Let(bindings, body) => {
            bindings.iter().any(|(_, v)| has(v, false)) || has(body, in_tail)
        }
        CoreExpr::Case(scrutinee, clauses) => {
            has(scrutinee, false) || clauses_have(clauses, in_tail)
        }
        CoreExpr::Receive { clauses, timeout } => {
            clauses_have(clauses, in_tail)
                || timeout
                    .as_ref()
                    .is_some_and(|(ms, body)| has(ms, false) || has(body, in_tail))
        }
        CoreExpr::Fun(_, body) => has(body, false),
        CoreExpr::Seq(first, second) => has(first, false) || has(second, in_tail),
        // Unsupported by this backend anyway; be conservative.
        CoreExpr::Try { .. } => true,
    }
}

struct ModuleCompiler<'a> {
    core: &'a CoreModule,
    atoms: AtomTable,
    functions: HashMap<(String, usize), u32>,
}

impl<'a> ModuleCompiler<'a> {
    fn new(core: &'a CoreModule) -> Result<Self, YggdrasilError> {
        if core.functions.len() > u32::MAX as usize {
            return Err(YggdrasilError::TooManyFunctions {
                module: core.name.clone(),
                count: core.functions.len(),
            });
        }

        let functions = core
            .functions
            .iter()
            .enumerate()
            .map(|(index, function)| ((function.name.clone(), function.arity), index as u32))
            .collect();
        Ok(Self {
            core,
            atoms: AtomTable::default(),
            functions,
        })
    }

    fn compile(mut self) -> Result<Module, YggdrasilError> {
        for function in &self.core.functions {
            self.atoms.intern(&function.name);
        }

        // See `has_nontail_self_call`: a unit mixing non-tail internal calls
        // with tail calls would leak the tail sentinel through the internal
        // frames, so its tail calls are demoted to contained calls.
        let demote_tails = self
            .core
            .functions
            .iter()
            .any(|f| has_nontail_self_call(&self.core.name, &f.body, true));

        let mut functions = Vec::with_capacity(self.core.functions.len());
        for function in &self.core.functions {
            let name_atom = self.atoms.intern(&function.name);
            let compiler = FunctionCompiler::new(
                &self.core.name,
                function,
                &self.functions,
                &mut self.atoms,
                demote_tails,
            )?;
            functions.push(compiler.compile(name_atom)?);
        }

        Ok(Module {
            atoms: self.atoms.names,
            functions,
        })
    }
}

struct FunctionCompiler<'a> {
    module_name: &'a str,
    function_name: &'a str,
    function: &'a CoreFunDef,
    functions: &'a HashMap<(String, usize), u32>,
    atoms: &'a mut AtomTable,
    code: CodeBuilder,
    next_register: u16,
    next_label: u32,
    demote_tails: bool,
}

impl<'a> FunctionCompiler<'a> {
    fn new(
        module_name: &'a str,
        function: &'a CoreFunDef,
        functions: &'a HashMap<(String, usize), u32>,
        atoms: &'a mut AtomTable,
        demote_tails: bool,
    ) -> Result<Self, YggdrasilError> {
        if function.params.len() > u8::MAX as usize || function.arity > u8::MAX as usize {
            return Err(YggdrasilError::TooManyArguments {
                module: module_name.to_owned(),
                function: function.name.clone(),
                count: function.params.len(),
            });
        }
        Ok(Self {
            module_name,
            function_name: &function.name,
            function,
            functions,
            atoms,
            code: CodeBuilder::new(),
            next_register: function.params.len() as u16,
            next_label: 0,
            demote_tails,
        })
    }

    fn compile(mut self, name_atom: u32) -> Result<Function, YggdrasilError> {
        let mut variables = HashMap::new();
        for (index, parameter) in self.function.params.iter().enumerate() {
            variables.insert(parameter.clone(), index as u8);
        }

        let result = self.compile_expr(&self.function.body, &variables, true)?;
        self.code.u8(op::RET).u8(result);
        let nregs = self.next_register as u8;
        let code = core::mem::take(&mut self.code).finish().map_err(|label| {
            YggdrasilError::MissingLabel {
                module: self.module_name.to_owned(),
                function: self.function_name.to_owned(),
                label,
            }
        })?;
        Ok(Function {
            name_atom,
            arity: self.function.arity as u8,
            nregs,
            code,
        })
    }

    fn compile_expr(
        &mut self,
        expression: &CoreExpr,
        variables: &HashMap<String, u8>,
        in_tail: bool,
    ) -> Result<u8, YggdrasilError> {
        match expression {
            CoreExpr::Lit(literal) => self.compile_literal(literal),
            CoreExpr::Var(name) => {
                variables
                    .get(name)
                    .copied()
                    .ok_or_else(|| YggdrasilError::UnboundVariable {
                        module: self.module_name.to_owned(),
                        function: self.function_name.to_owned(),
                        variable: name.clone(),
                    })
            }
            CoreExpr::Tuple(elements) => {
                let registers = self.compile_expressions(elements, variables)?;
                let element_count = self.argument_count(registers.len())?;
                let destination = self.allocate_register()?;
                self.code
                    .u8(op::MAKE_TUPLE)
                    .u8(destination)
                    .u8(element_count);
                for register in registers {
                    self.code.u8(register);
                }
                Ok(destination)
            }
            CoreExpr::List(elements, tail) => {
                let element_registers = self.compile_expressions(elements, variables)?;
                let mut list = self.compile_expr(tail, variables, false)?;
                for head in element_registers.into_iter().rev() {
                    let destination = self.allocate_register()?;
                    self.code.u8(op::CONS).u8(destination).u8(head).u8(list);
                    list = destination;
                }
                Ok(list)
            }
            CoreExpr::Cons(head, tail) => {
                let head = self.compile_expr(head, variables, false)?;
                let tail = self.compile_expr(tail, variables, false)?;
                let destination = self.allocate_register()?;
                self.code.u8(op::CONS).u8(destination).u8(head).u8(tail);
                Ok(destination)
            }
            CoreExpr::Map(entries) => {
                let mut registers = Vec::with_capacity(entries.len() * 2);
                for (key, value) in entries {
                    registers.push(self.compile_expr(key, variables, false)?);
                    registers.push(self.compile_expr(value, variables, false)?);
                }
                let pair_count = self.argument_count(entries.len())?;
                let destination = self.allocate_register()?;
                self.code.u8(op::MAP_NEW).u8(destination).u8(pair_count);
                for register in registers {
                    self.code.u8(register);
                }
                Ok(destination)
            }
            CoreExpr::Binary(segments) => self.compile_binary_construction(segments, variables),
            CoreExpr::LocalFunRef(_, _) | CoreExpr::RemoteFunRef(_, _, _) => {
                self.unsupported("first-class function references")
            }
            CoreExpr::Apply(callee, arguments) => match callee.as_ref() {
                CoreExpr::LocalFunRef(name, arity) if *arity == arguments.len() => {
                    self.compile_direct_call(self.module_name, name, arguments, variables, in_tail)
                }
                CoreExpr::RemoteFunRef(module, name, arity) if *arity == arguments.len() => {
                    self.compile_direct_call(module, name, arguments, variables, in_tail)
                }
                _ => self.unsupported("dynamic function application and closures"),
            },
            CoreExpr::Call(module, name, arguments) => {
                self.compile_direct_call(module, name, arguments, variables, in_tail)
            }
            CoreExpr::Let(bindings, body) => {
                let mut scope = variables.clone();
                for (name, value) in bindings {
                    let register = self.compile_expr(value, &scope, false)?;
                    scope.insert(name.clone(), register);
                }
                self.compile_expr(body, &scope, in_tail)
            }
            CoreExpr::Case(scrutinee, clauses) => {
                let scrutinee = self.compile_expr(scrutinee, variables, false)?;
                self.compile_case(scrutinee, clauses, variables, in_tail)
            }
            CoreExpr::Receive { clauses, timeout } => {
                if let Some((ms, body)) = timeout {
                    // Clause-less `receive after N => body` is a pure sleep
                    // (parks on the timer wheel, consumes nothing). Timeouts
                    // combined with message clauses still need real receive
                    // timeout support.
                    if !clauses.is_empty() {
                        return self.unsupported("receive timeouts with message clauses");
                    }
                    let ms = self.compile_expr(ms, variables, false)?;
                    self.code.u8(op::SLEEP_MS).u8(ms);
                    return self.compile_expr(body, variables, in_tail);
                }
                let message = self.allocate_register()?;
                self.code.u8(op::RECV).u8(message);
                self.compile_case(message, clauses, variables, in_tail)
            }
            CoreExpr::Fun(_, _) => self.unsupported("closures"),
            CoreExpr::Primop(name, arguments) => self.compile_primop(name, arguments, variables),
            CoreExpr::Seq(first, second) => {
                self.compile_expr(first, variables, false)?;
                self.compile_expr(second, variables, in_tail)
            }
            CoreExpr::Try { .. } => self.unsupported("try/catch"),
        }
    }

    fn compile_expressions(
        &mut self,
        expressions: &[CoreExpr],
        variables: &HashMap<String, u8>,
    ) -> Result<Vec<u8>, YggdrasilError> {
        expressions
            .iter()
            .map(|expression| self.compile_expr(expression, variables, false))
            .collect()
    }

    fn compile_literal(&mut self, literal: &CoreLit) -> Result<u8, YggdrasilError> {
        let destination = self.allocate_register()?;
        self.emit_literal_to(literal, destination)?;
        Ok(destination)
    }

    fn emit_literal_to(
        &mut self,
        literal: &CoreLit,
        destination: u8,
    ) -> Result<(), YggdrasilError> {
        match literal {
            CoreLit::Int(value) => {
                self.code.u8(op::LOAD_INT).u8(destination).i64(*value);
                Ok(())
            }
            CoreLit::Atom(atom) => {
                let atom = self.atoms.intern(atom);
                self.code.u8(op::LOAD_ATOM).u8(destination).u32(atom);
                Ok(())
            }
            CoreLit::Nil => {
                self.code.u8(op::LOAD_NIL).u8(destination);
                Ok(())
            }
            CoreLit::Float(_) => self.unsupported("floating-point literals"),
            // Lux String semantics: an arbitrary binary.
            CoreLit::String(value) => {
                let bytes = value.clone().into_bytes();
                self.emit_binary_const_to(&bytes, destination);
                Ok(())
            }
        }
    }

    fn emit_binary_const_to(&mut self, bytes: &[u8], destination: u8) {
        self.code
            .u8(op::BIN_NEW)
            .u8(destination)
            .u32(bytes.len() as u32)
            .bytes(bytes);
    }

    /// Binary construction `<<...>>`. Fully-constant contents (strings,
    /// 8-bit integer literals, nested constant binaries) become one `BIN_NEW`
    /// constant; dynamic construction goes through `list_to_binary`.
    fn compile_binary_construction(
        &mut self,
        segments: &[crate::codegen::erlang::CoreBinarySegment],
        variables: &HashMap<String, u8>,
    ) -> Result<u8, YggdrasilError> {
        if let Some(bytes) = Self::constant_binary_bytes(segments) {
            let destination = self.allocate_register()?;
            self.emit_binary_const_to(&bytes, destination);
            return Ok(destination);
        }
        // Dynamic path: whole-binary segments (`Value/binary`) concatenated
        // left to right with BIN_CAT — what binary `++`/`str_concat` lower to.
        let mut accumulator: Option<u8> = None;
        for segment in segments {
            if !matches!(
                (segment.kind, &segment.size),
                (CoreBinaryKind::Binary | CoreBinaryKind::Utf8, CoreBinarySize::All)
            ) && Self::constant_binary_bytes(core::slice::from_ref(segment)).is_none()
            {
                return self.unsupported("mixed dynamic binary construction");
            }
            let part = if let Some(bytes) = Self::constant_binary_bytes(core::slice::from_ref(segment)) {
                let destination = self.allocate_register()?;
                self.emit_binary_const_to(&bytes, destination);
                destination
            } else {
                self.compile_expr(&segment.value, variables, false)?
            };
            accumulator = Some(match accumulator {
                None => part,
                Some(previous) => {
                    let destination = self.allocate_register()?;
                    self.code
                        .u8(op::BIN_CAT)
                        .u8(destination)
                        .u8(previous)
                        .u8(part);
                    destination
                }
            });
        }
        match accumulator {
            Some(register) => Ok(register),
            None => {
                let destination = self.allocate_register()?;
                self.emit_binary_const_to(&[], destination);
                Ok(destination)
            }
        }
    }

    fn constant_binary_bytes(
        segments: &[crate::codegen::erlang::CoreBinarySegment],
    ) -> Option<Vec<u8>> {
        let mut bytes = Vec::new();
        for segment in segments {
            match (&segment.value, segment.kind, &segment.size) {
                (
                    CoreExpr::Lit(CoreLit::String(text)),
                    CoreBinaryKind::Utf8 | CoreBinaryKind::Binary,
                    _,
                ) => bytes.extend_from_slice(text.as_bytes()),
                (
                    CoreExpr::Lit(CoreLit::Int(value)),
                    CoreBinaryKind::Integer,
                    CoreBinarySize::Bits(bits),
                ) if matches!(bits, 8 | 16 | 32 | 64) && *value >= 0 => {
                    let width = (*bits / 8) as usize;
                    let raw = *value as u64;
                    bytes.extend_from_slice(&raw.to_le_bytes()[..width]);
                }
                (CoreExpr::Binary(inner), CoreBinaryKind::Binary, _) => {
                    bytes.extend_from_slice(&Self::constant_binary_bytes(inner)?);
                }
                _ => return None,
            }
        }
        Some(bytes)
    }

    fn compile_direct_call(
        &mut self,
        module: &str,
        name: &str,
        arguments: &[CoreExpr],
        variables: &HashMap<String, u8>,
        in_tail: bool,
    ) -> Result<u8, YggdrasilError> {
        if module == "ygg" {
            return self.compile_ygg_call(name, arguments, variables);
        }
        if module == "erlang" {
            return self.compile_erlang_call(name, arguments, variables);
        }
        if module == "binary" && name == "at" && arguments.len() == 2 {
            // Allocation-free byte indexing (the `binary_at` builtin).
            let binary = self.compile_expr(&arguments[0], variables, false)?;
            let index = self.compile_expr(&arguments[1], variables, false)?;
            let destination = self.allocate_register()?;
            self.code.u8(op::BIN_AT).u8(destination).u8(binary).u8(index);
            return Ok(destination);
        }
        if module == "io" && name == "format" {
            return self.compile_io_format(arguments, variables);
        }
        if module == "maps" {
            return self.compile_maps_call(name, arguments, variables);
        }
        if module == "lists" {
            return self.compile_lists_call(name, arguments, variables);
        }

        let argument_registers = self.compile_expressions(arguments, variables)?;
        let argument_count = self.argument_count(argument_registers.len())?;
        if in_tail && !self.demote_tails {
            // Tail position: hand the target to the engine trampoline —
            // constant native stack, GC-safe point per hop. This includes
            // self-recursion: the hashing pass canonicalizes self-references
            // (`__SELF__`) before addressing, so every function's Core names
            // itself by its own artifact hash and `module` is that hash here.
            let module_atom = self.atoms.intern(module);
            let function_atom = self.atoms.intern(name);
            self.code
                .u8(op::TAIL_CALL_EXT)
                .u32(module_atom)
                .u32(function_atom)
                .u8(argument_count);
            for register in argument_registers {
                self.code.u8(register);
            }
            // Unreachable continuation register (dead code after a terminal).
            return self.allocate_register();
        }
        let destination = self.allocate_register()?;
        if module == self.module_name {
            // Non-tail self-call: the target hash is this module (immutable),
            // so a direct local CALL is the same call spelled faster — pure
            // instruction selection, not a naming limitation.
            let Some(function_index) = self.functions.get(&(name.to_owned(), arguments.len()))
            else {
                return Err(YggdrasilError::UnknownLocalFunction {
                    module: self.module_name.to_owned(),
                    function: self.function_name.to_owned(),
                    callee: name.to_owned(),
                    arity: arguments.len(),
                });
            };
            self.code
                .u8(op::CALL)
                .u8(destination)
                .u32(*function_index)
                .u8(argument_count);
        } else {
            let module_atom = self.atoms.intern(module);
            let function_atom = self.atoms.intern(name);
            self.code
                .u8(op::CALL_EXT)
                .u8(destination)
                .u32(module_atom)
                .u32(function_atom)
                .u8(argument_count);
        }
        for register in argument_registers {
            self.code.u8(register);
        }
        Ok(destination)
    }

    /// Kernel port intrinsics (`extern` module `ygg`): the raw device surface
    /// a Lux driver programs against. Yggdrasil-only — the BEAM backend has no
    /// `ygg` module, so these must not be reachable from `main` when the
    /// example also runs on BEAM.
    fn compile_ygg_call(
        &mut self,
        name: &str,
        arguments: &[CoreExpr],
        variables: &HashMap<String, u8>,
    ) -> Result<u8, YggdrasilError> {
        match (name, arguments) {
            // The bytecode op takes the kind as an immediate: literal only.
            ("port_open", [CoreExpr::Lit(CoreLit::Int(kind))]) => {
                let destination = self.allocate_register()?;
                self.code.u8(op::PORT_OPEN).u8(destination).u8(*kind as u8);
                Ok(destination)
            }
            ("port_submit", [_, _, _, _, _]) => {
                let registers = self.compile_expressions(arguments, variables)?;
                self.code.u8(op::PORT_SUBMIT2);
                for register in registers {
                    self.code.u8(register);
                }
                // Submission traps on failure; the expression's value is 0.
                let destination = self.allocate_register()?;
                self.code.u8(op::LOAD_INT).u8(destination).i64(0);
                Ok(destination)
            }
            ("buf_to_bin", [id]) => {
                let source = self.compile_expr(id, variables, false)?;
                let destination = self.allocate_register()?;
                self.code.u8(op::BUF_TO_BIN).u8(destination).u8(source);
                Ok(destination)
            }
            ("bin_to_buf", [data]) => {
                let source = self.compile_expr(data, variables, false)?;
                let destination = self.allocate_register()?;
                self.code.u8(op::BIN_TO_BUF).u8(destination).u8(source);
                Ok(destination)
            }
            ("buf_new", [size]) => {
                let size = self.compile_expr(size, variables, false)?;
                let destination = self.allocate_register()?;
                self.code.u8(op::BUF_NEW).u8(destination).u8(size);
                Ok(destination)
            }
            ("buf_read", [buffer, offset, length]) => {
                let buffer = self.compile_expr(buffer, variables, false)?;
                let offset = self.compile_expr(offset, variables, false)?;
                let length = self.compile_expr(length, variables, false)?;
                let destination = self.allocate_register()?;
                self.code.u8(op::BUF_READ).u8(destination).u8(buffer).u8(offset).u8(length);
                Ok(destination)
            }
            ("buf_write", [buffer, offset, data]) => {
                let buffer = self.compile_expr(buffer, variables, false)?;
                let offset = self.compile_expr(offset, variables, false)?;
                let data = self.compile_expr(data, variables, false)?;
                let destination = self.allocate_register()?;
                self.code.u8(op::BUF_WRITE).u8(destination).u8(buffer).u8(offset).u8(data);
                Ok(destination)
            }
            ("ticks", []) => {
                let destination = self.allocate_register()?;
                self.code.u8(op::TICKS).u8(destination);
                Ok(destination)
            }
            _ => self.unsupported(&format!("ygg::{name}/{}", arguments.len())),
        }
    }

    fn compile_lists_call(
        &mut self,
        name: &str,
        arguments: &[CoreExpr],
        variables: &HashMap<String, u8>,
    ) -> Result<u8, YggdrasilError> {
        match (name, arguments) {
            ("append", [left, right]) => {
                let left = self.compile_expr(left, variables, false)?;
                let right = self.compile_expr(right, variables, false)?;
                let destination = self.allocate_register()?;
                self.code.u8(op::LIST_CAT).u8(destination).u8(left).u8(right);
                Ok(destination)
            }
            _ => self.unsupported(&format!("lists:{name}/{}", arguments.len())),
        }
    }

    fn compile_maps_call(
        &mut self,
        name: &str,
        arguments: &[CoreExpr],
        variables: &HashMap<String, u8>,
    ) -> Result<u8, YggdrasilError> {
        match (name, arguments) {
            // maps:get(Key, Map)
            ("get", [key, map]) => {
                let key = self.compile_expr(key, variables, false)?;
                let map = self.compile_expr(map, variables, false)?;
                let destination = self.allocate_register()?;
                self.code.u8(op::MAP_GET).u8(destination).u8(map).u8(key);
                Ok(destination)
            }
            // maps:put(Key, Value, Map)
            ("put", [key, value, map]) => {
                let key = self.compile_expr(key, variables, false)?;
                let value = self.compile_expr(value, variables, false)?;
                let map = self.compile_expr(map, variables, false)?;
                let destination = self.allocate_register()?;
                self.code
                    .u8(op::MAP_PUT)
                    .u8(destination)
                    .u8(map)
                    .u8(key)
                    .u8(value);
                Ok(destination)
            }
            _ => self.unsupported(&format!("maps:{name}/{}", arguments.len())),
        }
    }

    fn compile_erlang_call(
        &mut self,
        name: &str,
        arguments: &[CoreExpr],
        variables: &HashMap<String, u8>,
    ) -> Result<u8, YggdrasilError> {
        match (name, arguments) {
            ("band" | "bor" | "bxor" | "bsl" | "bsr", [left, right]) => {
                let left = self.compile_expr(left, variables, false)?;
                let right = self.compile_expr(right, variables, false)?;
                let destination = self.allocate_register()?;
                let opcode = match name {
                    "band" => op::BAND,
                    "bor" => op::BOR,
                    "bxor" => op::BXOR,
                    "bsl" => op::BSL,
                    _ => op::BSR,
                };
                self.code.u8(opcode).u8(destination).u8(left).u8(right);
                Ok(destination)
            }
            ("is_binary", [value]) => {
                let value = self.compile_expr(value, variables, false)?;
                let condition = self.allocate_register()?;
                self.code.u8(op::IS_BINARY).u8(condition).u8(value);
                self.boolean_from_int(condition, false)
            }
            ("++", [left, right]) => {
                let left = self.compile_expr(left, variables, false)?;
                let right = self.compile_expr(right, variables, false)?;
                let destination = self.allocate_register()?;
                self.code.u8(op::LIST_CAT).u8(destination).u8(left).u8(right);
                Ok(destination)
            }
            ("bnot", [value]) => {
                let value = self.compile_expr(value, variables, false)?;
                let destination = self.allocate_register()?;
                self.code.u8(op::BNOT).u8(destination).u8(value);
                Ok(destination)
            }
            ("binary_to_list", [binary]) => {
                let binary = self.compile_expr(binary, variables, false)?;
                let destination = self.allocate_register()?;
                self.code.u8(op::BIN_TO_LIST).u8(destination).u8(binary);
                Ok(destination)
            }
            ("list_to_binary" | "iolist_to_binary", [list]) => {
                let list = self.compile_expr(list, variables, false)?;
                let destination = self.allocate_register()?;
                self.code.u8(op::BIN_FROM_LIST).u8(destination).u8(list);
                Ok(destination)
            }
            ("binary_part", [binary, offset, length]) => {
                let binary = self.compile_expr(binary, variables, false)?;
                let offset = self.compile_expr(offset, variables, false)?;
                let length = self.compile_expr(length, variables, false)?;
                let destination = self.allocate_register()?;
                self.code
                    .u8(op::BIN_PART)
                    .u8(destination)
                    .u8(binary)
                    .u8(offset)
                    .u8(length);
                Ok(destination)
            }
            ("byte_size", [binary]) => {
                let binary = self.compile_expr(binary, variables, false)?;
                let destination = self.allocate_register()?;
                self.code.u8(op::BIN_SIZE).u8(destination).u8(binary);
                Ok(destination)
            }
            ("+", [left, right]) | ("-", [left, right]) | ("*", [left, right]) => {
                let left = self.compile_expr(left, variables, false)?;
                let right = self.compile_expr(right, variables, false)?;
                let destination = self.allocate_register()?;
                let opcode = match name {
                    "+" => op::ADD,
                    "-" => op::SUB,
                    _ => op::MUL,
                };
                self.code.u8(opcode).u8(destination).u8(left).u8(right);
                Ok(destination)
            }
            ("-", [value]) => {
                let zero = self.compile_literal(&CoreLit::Int(0))?;
                let value = self.compile_expr(value, variables, false)?;
                let destination = self.allocate_register()?;
                self.code.u8(op::SUB).u8(destination).u8(zero).u8(value);
                Ok(destination)
            }
            ("==" | "=:=" | "/=" | "=/=" | "<" | ">" | "=<" | "<=" | ">=", [left, right]) => {
                self.compile_comparison(name, left, right, variables)
            }
            ("not", [value]) => {
                let value = self.compile_expr(value, variables, false)?;
                let condition = self.compare_with_atom(value, "true")?;
                self.boolean_from_int(condition, true)
            }
            ("and" | "andalso", [left, right]) => {
                let left = self.compile_expr(left, variables, false)?;
                let right = self.compile_expr(right, variables, false)?;
                let left = self.compare_with_atom(left, "true")?;
                let right = self.compare_with_atom(right, "true")?;
                let condition = self.allocate_register()?;
                self.code.u8(op::MUL).u8(condition).u8(left).u8(right);
                self.boolean_from_int(condition, false)
            }
            ("or" | "orelse", [left, right]) => {
                let left = self.compile_expr(left, variables, false)?;
                let right = self.compile_expr(right, variables, false)?;
                let left = self.compare_with_atom(left, "true")?;
                let right = self.compare_with_atom(right, "true")?;
                let sum = self.allocate_register()?;
                self.code.u8(op::ADD).u8(sum).u8(left).u8(right);
                let zero = self.compile_literal(&CoreLit::Int(0))?;
                let is_zero = self.allocate_register()?;
                self.code.u8(op::CMP_EQ).u8(is_zero).u8(sum).u8(zero);
                self.boolean_from_int(is_zero, true)
            }
            ("self", []) => {
                let destination = self.allocate_register()?;
                self.code.u8(op::SELF_PID).u8(destination);
                Ok(destination)
            }
            ("send", [target, message]) => {
                let target = self.compile_expr(target, variables, false)?;
                let message = self.compile_expr(message, variables, false)?;
                self.code.u8(op::SEND).u8(target).u8(message);
                Ok(message)
            }
            ("hd", [list]) => {
                let list = self.compile_expr(list, variables, false)?;
                let destination = self.allocate_register()?;
                self.code.u8(op::HEAD).u8(destination).u8(list);
                Ok(destination)
            }
            ("tl", [list]) => {
                let list = self.compile_expr(list, variables, false)?;
                let destination = self.allocate_register()?;
                self.code.u8(op::TAIL).u8(destination).u8(list);
                Ok(destination)
            }
            ("element", [CoreExpr::Lit(CoreLit::Int(index)), tuple])
                if (1..=u8::MAX as i64 + 1).contains(index) =>
            {
                let tuple = self.compile_expr(tuple, variables, false)?;
                let destination = self.allocate_register()?;
                self.code
                    .u8(op::GET_ELEM)
                    .u8(destination)
                    .u8(tuple)
                    .u8((*index - 1) as u8);
                Ok(destination)
            }
            ("exit", [reason]) => {
                let reason = self.compile_expr(reason, variables, false)?;
                self.code.u8(op::EXIT_ATOM).u8(reason);
                Ok(reason)
            }
            // `spawn(|| f(x))` compiles by lifting the thunk: its body must be
            // a direct call with at most one argument. The argument is
            // evaluated here in the parent (it must be an immediate at
            // runtime — pid, int, atom) and the callee is spawned *by name*
            // through the module table (`SPAWN_EXT`), which is the only way
            // to spawn a content-addressed function module.
            ("spawn", [CoreExpr::Fun(params, body)]) if params.is_empty() => {
                let CoreExpr::Call(module, fname, call_args) = &**body else {
                    return self.unsupported("spawn of a non-call thunk");
                };
                if call_args.len() > 1 {
                    return self.unsupported("spawn thunk with more than one argument");
                }
                let argument = match call_args.first() {
                    Some(a) => self.compile_expr(a, variables, false)?,
                    None => {
                        let r = self.allocate_register()?;
                        self.code.u8(op::LOAD_INT).u8(r).i64(0);
                        r
                    }
                };
                let destination = self.allocate_register()?;
                let module_atom = self.atoms.intern(module);
                let function_atom = self.atoms.intern(fname);
                self.code
                    .u8(op::SPAWN_EXT)
                    .u8(destination)
                    .u32(module_atom)
                    .u32(function_atom)
                    .u8(argument);
                Ok(destination)
            }
            // `monitor(pid)` (lowered as erlang:monitor(process, Pid)): the
            // watched process's death delivers {'DOWN', Ref, Pid, Reason}.
            ("monitor", [CoreExpr::Lit(CoreLit::Atom(kind)), target]) if kind == "process" => {
                let target = self.compile_expr(target, variables, false)?;
                let destination = self.allocate_register()?;
                self.code.u8(op::MONITOR).u8(destination).u8(target);
                Ok(destination)
            }
            _ => self.unsupported(&format!("erlang:{name}/{}", arguments.len())),
        }
    }

    fn compile_comparison(
        &mut self,
        operator: &str,
        left: &CoreExpr,
        right: &CoreExpr,
        variables: &HashMap<String, u8>,
    ) -> Result<u8, YggdrasilError> {
        let left = self.compile_expr(left, variables, false)?;
        let right = self.compile_expr(right, variables, false)?;
        let condition = self.allocate_register()?;
        let (opcode, first, second, invert) = match operator {
            "==" | "=:=" => (op::CMP_EQ, left, right, false),
            "/=" | "=/=" => (op::CMP_EQ, left, right, true),
            "<" => (op::CMP_LT, left, right, false),
            ">" => (op::CMP_LT, right, left, false),
            "=<" | "<=" => (op::CMP_LT, right, left, true),
            ">=" => (op::CMP_LT, left, right, true),
            _ => unreachable!("comparison operators are exhaustively matched"),
        };
        self.code.u8(opcode).u8(condition).u8(first).u8(second);
        self.boolean_from_int(condition, invert)
    }

    fn boolean_from_int(&mut self, condition: u8, invert: bool) -> Result<u8, YggdrasilError> {
        let destination = self.allocate_register()?;
        let condition_true = self.allocate_label();
        let done = self.allocate_label();
        let (fallthrough, taken) = if invert {
            ("true", "false")
        } else {
            ("false", "true")
        };

        self.code
            .u8(op::JMP_IF)
            .u8(condition)
            .label_ref(condition_true);
        self.emit_atom_to(fallthrough, destination);
        self.code.u8(op::JMP).label_ref(done);
        self.code.bind(condition_true);
        self.emit_atom_to(taken, destination);
        self.code.bind(done);
        Ok(destination)
    }

    fn compile_io_format(
        &mut self,
        arguments: &[CoreExpr],
        variables: &HashMap<String, u8>,
    ) -> Result<u8, YggdrasilError> {
        let [CoreExpr::Lit(CoreLit::String(format)), values] = arguments else {
            return self.unsupported("io:format calls other than io:format(String, List)");
        };
        if format != "~p~n" {
            return self.unsupported("io:format formats other than \"~p~n\"");
        }
        match values {
            CoreExpr::List(values, tail)
                if matches!(tail.as_ref(), CoreExpr::Lit(CoreLit::Nil)) =>
            {
                for value in values {
                    let value = self.compile_expr(value, variables, false)?;
                    self.code.u8(op::PRINT).u8(value);
                }
            }
            CoreExpr::Cons(value, tail) if matches!(tail.as_ref(), CoreExpr::Lit(CoreLit::Nil)) => {
                let value = self.compile_expr(value, variables, false)?;
                self.code.u8(op::PRINT).u8(value);
            }
            _ => return self.unsupported("io:format argument lists with a non-nil tail"),
        }
        self.compile_literal(&CoreLit::Atom("ok".to_owned()))
    }

    fn compile_primop(
        &mut self,
        name: &str,
        arguments: &[CoreExpr],
        variables: &HashMap<String, u8>,
    ) -> Result<u8, YggdrasilError> {
        match (name, arguments) {
            ("send", [target, message]) => {
                let target = self.compile_expr(target, variables, false)?;
                let message = self.compile_expr(message, variables, false)?;
                self.code.u8(op::SEND).u8(target).u8(message);
                Ok(message)
            }
            ("self", []) => {
                let destination = self.allocate_register()?;
                self.code.u8(op::SELF_PID).u8(destination);
                Ok(destination)
            }
            _ => self.unsupported(&format!("primop {name}/{}", arguments.len())),
        }
    }

    fn compile_case(
        &mut self,
        scrutinee: u8,
        clauses: &[CoreClause],
        variables: &HashMap<String, u8>,
        in_tail: bool,
    ) -> Result<u8, YggdrasilError> {
        let destination = self.allocate_register()?;
        let done = self.allocate_label();

        for clause in clauses {
            let next_clause = self.allocate_label();
            let [pattern] = clause.patterns.as_slice() else {
                return self.unsupported("case clauses with multiple values");
            };
            let mut scope = variables.clone();
            self.compile_pattern(pattern, scrutinee, next_clause, &mut scope)?;

            let guard = self.compile_expr(&clause.guard, &scope, false)?;
            let guard_true = self.compare_with_atom(guard, "true")?;
            self.jump_unless(guard_true, next_clause);

            let body = self.compile_expr(&clause.body, &scope, in_tail)?;
            if body != destination {
                self.code.u8(op::MOVE).u8(destination).u8(body);
            }
            self.code.u8(op::JMP).label_ref(done);
            self.code.bind(next_clause);
        }

        let no_match = self.allocate_register()?;
        self.emit_atom_to("nomatch", no_match);
        self.code.u8(op::EXIT_ATOM).u8(no_match);
        self.code.bind(done);
        Ok(destination)
    }

    fn compile_pattern(
        &mut self,
        pattern: &CorePattern,
        value: u8,
        fail: u32,
        variables: &mut HashMap<String, u8>,
    ) -> Result<(), YggdrasilError> {
        match pattern {
            CorePattern::Lit(literal) => {
                let expected = self.compile_literal(literal)?;
                self.jump_unless_equal(value, expected, fail)?;
            }
            CorePattern::Var(name) => {
                if name != "_" {
                    variables.insert(name.clone(), value);
                }
            }
            CorePattern::Tuple(elements) => {
                if elements.len() > u8::MAX as usize + 1 {
                    return self.unsupported("tuples with more than 256 elements");
                }
                for (index, element) in elements.iter().enumerate() {
                    let field = self.allocate_register()?;
                    self.code
                        .u8(op::GET_ELEM)
                        .u8(field)
                        .u8(value)
                        .u8(index as u8);
                    self.compile_pattern(element, field, fail, variables)?;
                }
            }
            CorePattern::Cons(head, tail) => {
                // HEAD/TAIL trap on non-cons values, but a failed pattern
                // must fall through to the next clause. Lists are cons|nil in
                // typed code, so a nil guard makes the destructure total —
                // without it, `[13, 10 | _]` against `[13]` kills the process.
                let nil = self.compile_literal(&CoreLit::Nil)?;
                let is_nil = self.allocate_register()?;
                self.code.u8(op::CMP_EQ).u8(is_nil).u8(value).u8(nil);
                self.code.u8(op::JMP_IF).u8(is_nil).label_ref(fail);
                let head_value = self.allocate_register()?;
                let tail_value = self.allocate_register()?;
                self.code.u8(op::HEAD).u8(head_value).u8(value);
                self.code.u8(op::TAIL).u8(tail_value).u8(value);
                self.compile_pattern(head, head_value, fail, variables)?;
                self.compile_pattern(tail, tail_value, fail, variables)?;
            }
            CorePattern::Nil => {
                let nil = self.compile_literal(&CoreLit::Nil)?;
                self.jump_unless_equal(value, nil, fail)?;
            }
            CorePattern::Binary(_) => return self.unsupported("binary patterns"),
            CorePattern::Alias(name, inner) => {
                variables.insert(name.clone(), value);
                self.compile_pattern(inner, value, fail, variables)?;
            }
        }
        Ok(())
    }

    fn compare_with_atom(&mut self, value: u8, atom: &str) -> Result<u8, YggdrasilError> {
        let atom_register = self.allocate_register()?;
        self.emit_atom_to(atom, atom_register);
        let condition = self.allocate_register()?;
        self.code
            .u8(op::CMP_EQ)
            .u8(condition)
            .u8(value)
            .u8(atom_register);
        Ok(condition)
    }

    fn jump_unless_equal(&mut self, left: u8, right: u8, fail: u32) -> Result<(), YggdrasilError> {
        let equal = self.allocate_register()?;
        self.code.u8(op::CMP_EQ).u8(equal).u8(left).u8(right);
        self.jump_unless(equal, fail);
        Ok(())
    }

    fn jump_unless(&mut self, condition: u8, fail: u32) {
        let success = self.allocate_label();
        self.code.u8(op::JMP_IF).u8(condition).label_ref(success);
        self.code.u8(op::JMP).label_ref(fail);
        self.code.bind(success);
    }

    fn emit_atom_to(&mut self, atom: &str, destination: u8) {
        let atom = self.atoms.intern(atom);
        self.code.u8(op::LOAD_ATOM).u8(destination).u32(atom);
    }

    fn allocate_register(&mut self) -> Result<u8, YggdrasilError> {
        if self.next_register >= u8::MAX as u16 {
            return Err(YggdrasilError::RegisterLimit {
                module: self.module_name.to_owned(),
                function: self.function_name.to_owned(),
            });
        }
        let register = self.next_register as u8;
        self.next_register += 1;
        Ok(register)
    }

    fn allocate_label(&mut self) -> u32 {
        let label = self.next_label;
        self.next_label = self.next_label.wrapping_add(1);
        label
    }

    fn argument_count(&self, count: usize) -> Result<u8, YggdrasilError> {
        u8::try_from(count).map_err(|_| YggdrasilError::TooManyArguments {
            module: self.module_name.to_owned(),
            function: self.function_name.to_owned(),
            count,
        })
    }

    fn unsupported<T>(&self, feature: &str) -> Result<T, YggdrasilError> {
        Err(YggdrasilError::Unsupported {
            module: self.module_name.to_owned(),
            function: self.function_name.to_owned(),
            feature: feature.to_owned(),
        })
    }
}
