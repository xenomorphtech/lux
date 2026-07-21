use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use cranelift_codegen::Context;
use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::I64;
use cranelift_codegen::ir::{AbiParam, BlockArg, Function, InstBuilder, UserFuncName};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_module::{FuncId, Linkage, Module};
use cranelift_object::{ObjectBuilder, ObjectModule};

use crate::codegen::erlang::*;

use super::runtime;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum NativeError {
    Message(String),
    Io(std::io::Error),
}

impl std::fmt::Display for NativeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NativeError::Message(msg) => write!(f, "{}", msg),
            NativeError::Io(err) => write!(f, "I/O error: {}", err),
        }
    }
}

impl From<std::io::Error> for NativeError {
    fn from(err: std::io::Error) -> Self {
        NativeError::Io(err)
    }
}

pub struct NativeOutput {
    pub object_bytes: Vec<u8>,
    pub runtime_c_source: String,
    pub entry_symbol: String,
}

// ---------------------------------------------------------------------------
// Compiler
// ---------------------------------------------------------------------------

pub struct NativeCompiler {
    /// Atom table: index → name. Indices 0..RESERVED are fixed.
    atom_table: Vec<String>,
    /// Map from (module_hash, func_name) → FuncId in the Cranelift module
    func_ids: HashMap<String, FuncId>,
    /// Runtime helper function IDs
    rt_func_ids: HashMap<String, FuncId>,
    /// Counter for generating unique lambda names
    lambda_counter: usize,
    /// Lambdas discovered during translation, to be compiled after current function
    deferred_lambdas: Vec<DeferredLambda>,
}

struct DeferredLambda {
    func_id: FuncId,
    symbol: String,
    /// User params (not including the env param)
    params: Vec<String>,
    /// Captured variable names (in order)
    captures: Vec<String>,
    body: CoreExpr,
    self_module: String,
}

impl NativeCompiler {
    pub fn new() -> Self {
        let atom_table: Vec<String> = runtime::RESERVED_ATOMS
            .iter()
            .map(|s| s.to_string())
            .collect();
        // Ensure reserved atoms are at fixed indices
        assert_eq!(atom_table[0], "true");
        assert_eq!(atom_table[1], "false");

        Self {
            atom_table,
            func_ids: HashMap::new(),
            rt_func_ids: HashMap::new(),
            lambda_counter: 0,
            deferred_lambdas: Vec::new(),
        }
    }

    fn intern_atom(&mut self, name: &str) -> usize {
        if let Some(pos) = self.atom_table.iter().position(|s| s == name) {
            return pos;
        }
        let idx = self.atom_table.len();
        self.atom_table.push(name.to_string());
        idx
    }

    /// Compile a set of CoreModules into native object code + C runtime source.
    pub fn compile(
        modules: &[CoreModule],
        entry_module: Option<&str>,
        entry_arity: usize,
    ) -> Result<NativeOutput, NativeError> {
        let mut compiler = NativeCompiler::new();

        // Pre-scan atoms from all modules
        for module in modules {
            compiler.scan_atoms_module(module);
        }
        // Ensure commonly needed atoms are always available
        compiler.intern_atom("nomatch");
        compiler.intern_atom("undefined");

        // Set up Cranelift target
        let mut flag_builder = settings::builder();
        flag_builder.set("opt_level", "speed").unwrap();
        let isa_builder = cranelift_native::builder().map_err(|e| {
            NativeError::Message(format!("failed to create native ISA builder: {}", e))
        })?;
        let isa = isa_builder
            .finish(settings::Flags::new(flag_builder))
            .map_err(|e| NativeError::Message(format!("failed to build ISA: {}", e)))?;

        let obj_builder =
            ObjectBuilder::new(isa, "lux_native", cranelift_module::default_libcall_names())
                .map_err(|e| NativeError::Message(format!("ObjectBuilder error: {}", e)))?;
        let mut obj_module = ObjectModule::new(obj_builder);

        // Declare runtime helper functions (external, linked from C runtime)
        compiler.declare_runtime_functions(&mut obj_module)?;

        // First pass: declare all Lux functions so we can reference them
        for module in modules {
            for func_def in &module.functions {
                let symbol = mangle_symbol(&module.name, &func_def.name, func_def.arity);
                let mut sig = obj_module.make_signature();
                for _ in 0..func_def.arity {
                    sig.params.push(AbiParam::new(I64));
                }
                sig.returns.push(AbiParam::new(I64));
                let func_id = obj_module
                    .declare_function(&symbol, Linkage::Export, &sig)
                    .map_err(|e| NativeError::Message(format!("declare_function: {}", e)))?;
                compiler.func_ids.insert(symbol, func_id);
            }
        }

        // Second pass: define each function body
        let mut fb_ctx = FunctionBuilderContext::new();
        for module in modules {
            for func_def in &module.functions {
                let symbol = mangle_symbol(&module.name, &func_def.name, func_def.arity);
                let func_id = compiler.func_ids[&symbol];

                let mut sig = obj_module.make_signature();
                for _ in 0..func_def.arity {
                    sig.params.push(AbiParam::new(I64));
                }
                sig.returns.push(AbiParam::new(I64));

                let mut func = Function::with_name_signature(UserFuncName::default(), sig.clone());

                {
                    let mut builder = FunctionBuilder::new(&mut func, &mut fb_ctx);

                    let entry_block = builder.create_block();
                    builder.append_block_params_for_function_params(entry_block);
                    builder.switch_to_block(entry_block);
                    builder.seal_block(entry_block);

                    // Map parameter names to variables
                    let mut vars: HashMap<String, Variable> = HashMap::new();
                    for (i, param_name) in func_def.params.iter().enumerate() {
                        let var = builder.declare_var(I64);
                        let param_val = builder.block_params(entry_block)[i];
                        builder.def_var(var, param_val);
                        vars.insert(param_name.clone(), var);
                    }

                    let result = compiler.translate_expr(
                        &mut builder,
                        &func_def.body,
                        &mut vars,
                        &module.name,
                        &mut obj_module,
                    );
                    builder.ins().return_(&[result]);
                    builder.finalize();
                }

                let mut ctx = Context::for_function(func);
                obj_module
                    .define_function(func_id, &mut ctx)
                    .map_err(|e| NativeError::Message(format!("define_function: {}", e)))?;
            }
        }

        // Compile deferred lambdas (iterate until no new ones are generated)
        while !compiler.deferred_lambdas.is_empty() {
            let lambdas: Vec<DeferredLambda> = std::mem::take(&mut compiler.deferred_lambdas);
            for lambda in lambdas {
                let mut sig = obj_module.make_signature();
                // First param is always the closure env pointer
                sig.params.push(AbiParam::new(I64));
                for _ in 0..lambda.params.len() {
                    sig.params.push(AbiParam::new(I64));
                }
                sig.returns.push(AbiParam::new(I64));

                let mut func = Function::with_name_signature(UserFuncName::default(), sig.clone());

                {
                    let mut builder = FunctionBuilder::new(&mut func, &mut fb_ctx);
                    let entry_block = builder.create_block();
                    builder.append_block_params_for_function_params(entry_block);
                    builder.switch_to_block(entry_block);
                    builder.seal_block(entry_block);

                    let mut vars: HashMap<String, Variable> = HashMap::new();

                    // Param 0 is env (closure pointer)
                    let env_val = builder.block_params(entry_block)[0];

                    // Extract captures from the closure env
                    if !lambda.captures.is_empty() {
                        let env_ptr = builder.ins().band_imm(env_val, !runtime::TAG_MASK);
                        for (i, cap_name) in lambda.captures.iter().enumerate() {
                            // Captures stored at offset (2 + i) * 8
                            // [func_ptr, n_captures, cap0, cap1, ...]
                            let offset = ((2 + i) * 8) as i32;
                            let cap_val = builder.ins().load(
                                I64,
                                cranelift_codegen::ir::MemFlags::new(),
                                env_ptr,
                                offset,
                            );
                            let var = builder.declare_var(I64);
                            builder.def_var(var, cap_val);
                            vars.insert(cap_name.clone(), var);
                        }
                    }

                    // Bind user params (starting at block param index 1)
                    for (i, param_name) in lambda.params.iter().enumerate() {
                        let var = builder.declare_var(I64);
                        let param_val = builder.block_params(entry_block)[1 + i];
                        builder.def_var(var, param_val);
                        vars.insert(param_name.clone(), var);
                    }

                    let result = compiler.translate_expr(
                        &mut builder,
                        &lambda.body,
                        &mut vars,
                        &lambda.self_module,
                        &mut obj_module,
                    );
                    builder.ins().return_(&[result]);
                    builder.finalize();
                }

                let mut ctx = Context::for_function(func);
                obj_module
                    .define_function(lambda.func_id, &mut ctx)
                    .map_err(|e| {
                        NativeError::Message(format!("define lambda {}: {}", lambda.symbol, e))
                    })?;
            }
        }

        let object_product = obj_module.finish();
        let object_bytes = object_product
            .emit()
            .map_err(|e| NativeError::Message(format!("object emit: {}", e)))?;

        // Determine entry symbol
        let entry_symbol = if let Some(entry_mod) = entry_module {
            mangle_symbol(entry_mod, "apply", entry_arity)
        } else {
            // Fall back to first module's first function
            let m = &modules[0];
            let f = &m.functions[0];
            mangle_symbol(&m.name, &f.name, f.arity)
        };

        let runtime_c =
            runtime::generate_runtime_c(&compiler.atom_table, &entry_symbol, entry_arity);

        Ok(NativeOutput {
            object_bytes,
            runtime_c_source: runtime_c,
            entry_symbol,
        })
    }

    /// Link the object file and C runtime into a native executable.
    pub fn link(output: &NativeOutput, out_path: &Path) -> Result<(), NativeError> {
        let dir = out_path.parent().unwrap_or(Path::new("."));
        let obj_path = dir.join("_lux_native.o");
        let rt_path = dir.join("_lux_runtime.c");

        std::fs::write(&obj_path, &output.object_bytes)?;
        std::fs::write(&rt_path, &output.runtime_c_source)?;

        let status = Command::new("cc")
            .arg("-o")
            .arg(out_path)
            .arg(&rt_path)
            .arg(&obj_path)
            .arg("-lm")
            .arg("-no-pie")
            .status()?;

        // Clean up temp files
        let _ = std::fs::remove_file(&obj_path);
        let _ = std::fs::remove_file(&rt_path);

        if !status.success() {
            return Err(NativeError::Message(format!(
                "linker (cc) failed with exit code: {:?}",
                status.code()
            )));
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Runtime function declarations
    // -----------------------------------------------------------------------

    fn declare_runtime_functions(&mut self, module: &mut ObjectModule) -> Result<(), NativeError> {
        // Helper to declare a single runtime function
        let mut declare = |module: &mut ObjectModule,
                           name: &str,
                           params: usize,
                           returns: usize|
         -> Result<(), NativeError> {
            let mut sig = module.make_signature();
            for _ in 0..params {
                sig.params.push(AbiParam::new(I64));
            }
            for _ in 0..returns {
                sig.returns.push(AbiParam::new(I64));
            }
            let id = module
                .declare_function(name, Linkage::Import, &sig)
                .map_err(|e| NativeError::Message(format!("declare rt func {}: {}", name, e)))?;
            self.rt_func_ids.insert(name.to_string(), id);
            Ok(())
        };

        declare(module, "lux_rt_io_format", 2, 1)?;
        declare(module, "lux_rt_display", 1, 1)?;
        declare(module, "lux_rt_make_tuple2", 2, 1)?;
        declare(module, "lux_rt_make_tuple3", 3, 1)?;
        declare(module, "lux_rt_tuple_element", 2, 1)?;
        declare(module, "lux_rt_tuple_arity", 1, 1)?;
        declare(module, "lux_rt_make_cons", 2, 1)?;
        declare(module, "lux_rt_cons_head", 1, 1)?;
        declare(module, "lux_rt_cons_tail", 1, 1)?;
        declare(module, "lux_rt_list_length", 1, 1)?;
        declare(module, "lux_rt_to_string", 1, 1)?;
        declare(module, "lux_rt_string_concat", 2, 1)?;
        declare(module, "lux_rt_make_string", 2, 1)?;
        declare(module, "lux_rt_binary_slice", 3, 1)?;
        declare(module, "lux_rt_match_error", 0, 0)?;
        declare(module, "lux_rt_case_clause_error", 1, 0)?;
        declare(module, "lux_rt_erlang_error", 1, 0)?;
        declare(module, "lux_rt_erlang_throw", 1, 0)?;
        declare(module, "lux_rt_apply0", 1, 1)?;
        declare(module, "lux_rt_apply1", 2, 1)?;
        declare(module, "lux_rt_apply2", 3, 1)?;
        declare(module, "lux_rt_apply3", 4, 1)?;
        declare(module, "lux_rt_make_map", 3, 1)?;
        declare(module, "lux_rt_safe_div", 2, 1)?;
        declare(module, "lux_rt_safe_rem", 2, 1)?;
        // Mixed int/float arithmetic
        declare(module, "lux_rt_add", 2, 1)?;
        declare(module, "lux_rt_sub", 2, 1)?;
        declare(module, "lux_rt_mul", 2, 1)?;
        declare(module, "lux_rt_float_div", 2, 1)?;
        declare(module, "lux_rt_negate", 1, 1)?;
        // Mixed comparisons
        declare(module, "lux_rt_less_than", 2, 1)?;
        declare(module, "lux_rt_less_equal", 2, 1)?;
        declare(module, "lux_rt_greater_than", 2, 1)?;
        declare(module, "lux_rt_greater_equal", 2, 1)?;
        declare(module, "lux_rt_try_begin", 0, 1)?;
        declare(module, "lux_rt_try_end", 0, 0)?;
        declare(module, "lux_rt_try_get_error", 0, 1)?;

        // List stdlib
        declare(module, "lux_lists_reverse1", 1, 1)?;
        declare(module, "lux_lists_sort1", 1, 1)?;
        declare(module, "lux_lists_append2", 2, 1)?;
        declare(module, "lux_lists_flatten1", 1, 1)?;
        declare(module, "lux_lists_seq2", 2, 1)?;
        declare(module, "lux_lists_nth2", 2, 1)?;
        declare(module, "lux_lists_member2", 2, 1)?;
        declare(module, "lux_lists_map2", 2, 1)?;
        declare(module, "lux_lists_flatmap2", 2, 1)?;
        declare(module, "lux_lists_filter2", 2, 1)?;
        declare(module, "lux_lists_foldl3", 3, 1)?;
        declare(module, "lux_lists_usort1", 1, 1)?;
        declare(module, "lux_lists_zip2", 2, 1)?;
        declare(module, "lux_lists_enumerate1", 1, 1)?;
        declare(module, "lux_lists_join2", 2, 1)?;
        declare(module, "lux_lists_sublist2", 2, 1)?;
        declare(module, "lux_lists_nthtail2", 2, 1)?;

        // Map stdlib
        declare(module, "lux_maps_get2", 2, 1)?;
        declare(module, "lux_maps_get3", 3, 1)?;
        declare(module, "lux_maps_put3", 3, 1)?;
        declare(module, "lux_maps_remove2", 2, 1)?;
        declare(module, "lux_maps_is_key2", 2, 1)?;
        declare(module, "lux_maps_find2", 2, 1)?;
        declare(module, "lux_maps_keys1", 1, 1)?;
        declare(module, "lux_maps_values1", 1, 1)?;
        declare(module, "lux_maps_to_list1", 1, 1)?;
        declare(module, "lux_maps_from_list1", 1, 1)?;
        declare(module, "lux_maps_merge2", 2, 1)?;
        declare(module, "lux_maps_size1", 1, 1)?;

        // String operations
        declare(module, "lux_string_trim1", 1, 1)?;
        declare(module, "lux_string_uppercase1", 1, 1)?;
        declare(module, "lux_string_lowercase1", 1, 1)?;
        declare(module, "lux_string_find2", 2, 1)?;
        declare(module, "lux_string_split3", 3, 1)?;
        declare(module, "lux_string_replace4", 4, 1)?;

        // String comparison for pattern matching
        declare(module, "lux_rt_string_equal", 2, 1)?;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Atom scanning — pre-populate atom table from all expressions
    // -----------------------------------------------------------------------

    fn scan_atoms_module(&mut self, module: &CoreModule) {
        for func in &module.functions {
            self.scan_atoms_expr(&func.body);
        }
    }

    fn scan_atoms_expr(&mut self, expr: &CoreExpr) {
        match expr {
            CoreExpr::Lit(CoreLit::Atom(name)) => {
                self.intern_atom(name);
            }
            CoreExpr::Tuple(elems) => {
                for e in elems {
                    self.scan_atoms_expr(e);
                }
            }
            CoreExpr::List(elems, tail) => {
                for e in elems {
                    self.scan_atoms_expr(e);
                }
                self.scan_atoms_expr(tail);
            }
            CoreExpr::Cons(h, t) => {
                self.scan_atoms_expr(h);
                self.scan_atoms_expr(t);
            }
            CoreExpr::Map(entries) => {
                for (k, v) in entries {
                    self.scan_atoms_expr(k);
                    self.scan_atoms_expr(v);
                }
            }
            CoreExpr::Binary(segs) => {
                for seg in segs {
                    self.scan_atoms_expr(&seg.value);
                }
            }
            CoreExpr::Apply(f, args) => {
                self.scan_atoms_expr(f);
                for a in args {
                    self.scan_atoms_expr(a);
                }
            }
            CoreExpr::Call(_, _, args) => {
                for a in args {
                    self.scan_atoms_expr(a);
                }
            }
            CoreExpr::Let(bindings, body) => {
                for (_, v) in bindings {
                    self.scan_atoms_expr(v);
                }
                self.scan_atoms_expr(body);
            }
            CoreExpr::Case(scrutinee, clauses) => {
                self.scan_atoms_expr(scrutinee);
                for clause in clauses {
                    self.scan_atoms_clause(clause);
                }
            }
            CoreExpr::Receive { clauses, timeout } => {
                for clause in clauses {
                    self.scan_atoms_clause(clause);
                }
                if let Some((ms, body)) = timeout {
                    self.scan_atoms_expr(ms);
                    self.scan_atoms_expr(body);
                }
            }
            CoreExpr::Fun(_, body) => self.scan_atoms_expr(body),
            CoreExpr::Primop(_, args) => {
                for a in args {
                    self.scan_atoms_expr(a);
                }
            }
            CoreExpr::Seq(a, b) => {
                self.scan_atoms_expr(a);
                self.scan_atoms_expr(b);
            }
            CoreExpr::Try {
                body,
                handler,
                catch,
                ..
            } => {
                self.scan_atoms_expr(body);
                self.scan_atoms_expr(handler);
                self.scan_atoms_expr(catch);
            }
            CoreExpr::Lit(_)
            | CoreExpr::Var(_)
            | CoreExpr::LocalFunRef(_, _)
            | CoreExpr::RemoteFunRef(_, _, _) => {}
        }
    }

    fn scan_atoms_clause(&mut self, clause: &CoreClause) {
        for pat in &clause.patterns {
            self.scan_atoms_pattern(pat);
        }
        self.scan_atoms_expr(&clause.guard);
        self.scan_atoms_expr(&clause.body);
    }

    fn scan_atoms_pattern(&mut self, pat: &CorePattern) {
        match pat {
            CorePattern::Lit(CoreLit::Atom(name)) => {
                self.intern_atom(name);
            }
            CorePattern::Tuple(pats) => {
                for p in pats {
                    self.scan_atoms_pattern(p);
                }
            }
            CorePattern::Cons(h, t) => {
                self.scan_atoms_pattern(h);
                self.scan_atoms_pattern(t);
            }
            CorePattern::Alias(_, p) => self.scan_atoms_pattern(p),
            CorePattern::Binary(segs) => {
                for seg in segs {
                    self.scan_atoms_pattern(&seg.pattern);
                }
            }
            _ => {}
        }
    }

    // -----------------------------------------------------------------------
    // Expression translation
    // -----------------------------------------------------------------------

    fn translate_expr(
        &mut self,
        builder: &mut FunctionBuilder,
        expr: &CoreExpr,
        vars: &mut HashMap<String, Variable>,
        self_module: &str,
        obj_module: &mut ObjectModule,
    ) -> cranelift_codegen::ir::Value {
        match expr {
            CoreExpr::Lit(lit) => self.translate_lit(builder, lit, obj_module),

            CoreExpr::Var(name) => {
                if let Some(&var) = vars.get(name) {
                    builder.use_var(var)
                } else {
                    // Unknown variable — return nil as fallback
                    builder.ins().iconst(I64, runtime::VALUE_NIL)
                }
            }

            CoreExpr::Let(bindings, body) => {
                for (name, val_expr) in bindings {
                    let val = self.translate_expr(builder, val_expr, vars, self_module, obj_module);
                    let var = builder.declare_var(I64);
                    builder.def_var(var, val);
                    vars.insert(name.clone(), var);
                }
                self.translate_expr(builder, body, vars, self_module, obj_module)
            }

            CoreExpr::Seq(first, second) => {
                self.translate_expr(builder, first, vars, self_module, obj_module);
                self.translate_expr(builder, second, vars, self_module, obj_module)
            }

            CoreExpr::Call(module, func, args) => {
                self.translate_call(builder, module, func, args, vars, self_module, obj_module)
            }

            CoreExpr::Apply(func_expr, args) => {
                match func_expr.as_ref() {
                    CoreExpr::LocalFunRef(name, _arity) => {
                        let symbol = mangle_symbol(self_module, name, args.len());
                        self.translate_direct_call(
                            builder,
                            &symbol,
                            args,
                            vars,
                            self_module,
                            obj_module,
                        )
                    }
                    CoreExpr::RemoteFunRef(module, name, _arity) => {
                        let symbol = mangle_symbol(module, name, args.len());
                        self.translate_direct_call(
                            builder,
                            &symbol,
                            args,
                            vars,
                            self_module,
                            obj_module,
                        )
                    }
                    _ => {
                        // Dynamic apply through closure
                        let closure_val =
                            self.translate_expr(builder, func_expr, vars, self_module, obj_module);
                        let mut arg_vals = Vec::new();
                        for arg in args {
                            arg_vals.push(self.translate_expr(
                                builder,
                                arg,
                                vars,
                                self_module,
                                obj_module,
                            ));
                        }
                        // Call via runtime apply
                        match arg_vals.len() {
                            0 => {
                                let apply0 = self.rt_func_ids["lux_rt_apply0"];
                                let apply0_ref =
                                    obj_module.declare_func_in_func(apply0, builder.func);
                                let call = builder.ins().call(apply0_ref, &[closure_val]);
                                builder.inst_results(call)[0]
                            }
                            1 => {
                                let apply1 = self.rt_func_ids["lux_rt_apply1"];
                                let apply1_ref =
                                    obj_module.declare_func_in_func(apply1, builder.func);
                                let call =
                                    builder.ins().call(apply1_ref, &[closure_val, arg_vals[0]]);
                                builder.inst_results(call)[0]
                            }
                            2 => {
                                let apply2 = self.rt_func_ids["lux_rt_apply2"];
                                let apply2_ref =
                                    obj_module.declare_func_in_func(apply2, builder.func);
                                let call = builder
                                    .ins()
                                    .call(apply2_ref, &[closure_val, arg_vals[0], arg_vals[1]]);
                                builder.inst_results(call)[0]
                            }
                            3 => {
                                let apply3 = self.rt_func_ids["lux_rt_apply3"];
                                let apply3_ref =
                                    obj_module.declare_func_in_func(apply3, builder.func);
                                let call = builder.ins().call(
                                    apply3_ref,
                                    &[closure_val, arg_vals[0], arg_vals[1], arg_vals[2]],
                                );
                                builder.inst_results(call)[0]
                            }
                            _ => builder.ins().iconst(I64, runtime::VALUE_NIL),
                        }
                    }
                }
            }

            CoreExpr::Case(scrutinee, clauses) => {
                self.translate_case(builder, scrutinee, clauses, vars, self_module, obj_module)
            }

            CoreExpr::Tuple(elems) => {
                self.translate_tuple(builder, elems, vars, self_module, obj_module)
            }

            CoreExpr::List(elems, tail) => {
                // Build list from right to left: fold elements onto tail
                let mut acc = self.translate_expr(builder, tail, vars, self_module, obj_module);
                for elem in elems.iter().rev() {
                    let head_val =
                        self.translate_expr(builder, elem, vars, self_module, obj_module);
                    let make_cons = self.rt_func_ids["lux_rt_make_cons"];
                    let make_cons_ref = obj_module.declare_func_in_func(make_cons, builder.func);
                    let call = builder.ins().call(make_cons_ref, &[head_val, acc]);
                    acc = builder.inst_results(call)[0];
                }
                acc
            }

            CoreExpr::Cons(head, tail) => {
                let h = self.translate_expr(builder, head, vars, self_module, obj_module);
                let t = self.translate_expr(builder, tail, vars, self_module, obj_module);
                let make_cons = self.rt_func_ids["lux_rt_make_cons"];
                let make_cons_ref = obj_module.declare_func_in_func(make_cons, builder.func);
                let call = builder.ins().call(make_cons_ref, &[h, t]);
                builder.inst_results(call)[0]
            }

            CoreExpr::LocalFunRef(name, arity) => {
                // Create a closure wrapper around the named function so it can
                // be passed as a value (e.g. to map/filter).
                self.translate_fun_ref_closure(builder, self_module, name, *arity, vars, obj_module)
            }

            CoreExpr::RemoteFunRef(module, name, arity) => {
                self.translate_fun_ref_closure(builder, module, name, *arity, vars, obj_module)
            }

            CoreExpr::Fun(params, body) => {
                self.translate_closure(builder, params, body, vars, self_module, obj_module)
            }

            CoreExpr::Map(entries) => {
                self.translate_map(builder, entries, vars, self_module, obj_module)
            }

            CoreExpr::Binary(segments) => {
                self.translate_binary(builder, segments, vars, self_module, obj_module)
            }

            CoreExpr::Primop(op, args) => {
                self.translate_primop(builder, op, args, vars, self_module, obj_module)
            }

            CoreExpr::Try {
                body,
                vars: try_vars,
                handler,
                evars,
                catch,
            } => {
                // Wrap the try body in a closure, then call lux_rt_try_call
                // which uses setjmp with correct stack frame semantics.
                let body_closure = self.translate_closure(
                    builder,
                    &[], // no params — the body closure takes a dummy arg
                    body,
                    vars,
                    self_module,
                    obj_module,
                );

                // Declare lux_rt_try_call if not done
                let try_call_name = "lux_rt_try_call";
                let try_call_id = if let Some(&id) = self.rt_func_ids.get(try_call_name) {
                    id
                } else {
                    let mut sig = obj_module.make_signature();
                    sig.params.push(AbiParam::new(I64));
                    sig.returns.push(AbiParam::new(I64));
                    let id = obj_module
                        .declare_function(try_call_name, Linkage::Import, &sig)
                        .unwrap();
                    self.rt_func_ids.insert(try_call_name.to_string(), id);
                    id
                };
                let try_call_ref = obj_module.declare_func_in_func(try_call_id, builder.func);
                let call = builder.ins().call(try_call_ref, &[body_closure]);
                let result_tuple = builder.inst_results(call)[0];

                // result_tuple is {ok, value} or {error, reason}
                // Extract tag (element 0) and value (element 1)
                let tag = self.extract_tuple_element(builder, result_tuple, 0);
                let value = self.extract_tuple_element(builder, result_tuple, 1);

                // Check if ok (atom index 2 = "ok")
                let ok_atom_idx = self.intern_atom("ok");
                let ok_val = builder
                    .ins()
                    .iconst(I64, runtime::make_tagged_atom(ok_atom_idx));
                let is_ok = builder.ins().icmp(IntCC::Equal, tag, ok_val);

                let ok_block = builder.create_block();
                let err_block = builder.create_block();
                let merge_block = builder.create_block();
                builder.append_block_param(merge_block, I64);

                builder.ins().brif(is_ok, ok_block, &[], err_block, &[]);

                // OK path: bind try vars and evaluate handler
                builder.switch_to_block(ok_block);
                builder.seal_block(ok_block);
                if let Some(var_name) = try_vars.first() {
                    let var = builder.declare_var(I64);
                    builder.def_var(var, value);
                    vars.insert(var_name.clone(), var);
                }
                let handler_result =
                    self.translate_expr(builder, handler, vars, self_module, obj_module);
                builder
                    .ins()
                    .jump(merge_block, &[BlockArg::Value(handler_result)]);

                // Error path: bind evars and evaluate catch
                builder.switch_to_block(err_block);
                builder.seal_block(err_block);
                let nil = builder.ins().iconst(I64, runtime::VALUE_NIL);
                for (i, evar_name) in evars.iter().enumerate() {
                    let val = if i == 1 { value } else { nil };
                    let var = builder.declare_var(I64);
                    builder.def_var(var, val);
                    vars.insert(evar_name.clone(), var);
                }
                let catch_result =
                    self.translate_expr(builder, catch, vars, self_module, obj_module);
                builder
                    .ins()
                    .jump(merge_block, &[BlockArg::Value(catch_result)]);

                builder.switch_to_block(merge_block);
                builder.seal_block(merge_block);
                builder.block_params(merge_block)[0]
            }

            CoreExpr::Receive { .. } => {
                // Processes not supported in native backend
                builder.ins().iconst(I64, runtime::VALUE_NIL)
            }
        }
    }

    fn translate_lit(
        &mut self,
        builder: &mut FunctionBuilder,
        lit: &CoreLit,
        obj_module: &mut ObjectModule,
    ) -> cranelift_codegen::ir::Value {
        match lit {
            CoreLit::Int(n) => {
                let tagged = runtime::make_tagged_int(*n);
                builder.ins().iconst(I64, tagged)
            }
            CoreLit::Atom(name) => {
                let idx = self.intern_atom(name);
                let tagged = runtime::make_tagged_atom(idx);
                builder.ins().iconst(I64, tagged)
            }
            CoreLit::Nil => builder.ins().iconst(I64, runtime::VALUE_NIL),
            CoreLit::Float(f) => self.translate_float_lit(builder, *f, obj_module),
            CoreLit::String(s) => self.translate_string_lit(builder, s, obj_module),
        }
    }

    fn translate_string_lit(
        &mut self,
        builder: &mut FunctionBuilder,
        s: &str,
        obj_module: &mut ObjectModule,
    ) -> cranelift_codegen::ir::Value {
        let bytes = s.as_bytes();
        let len = bytes.len();

        if len == 0 {
            return builder.ins().iconst(I64, runtime::VALUE_NIL);
        }

        // Allocate stack slot for string data
        let ss = builder.create_sized_stack_slot(cranelift_codegen::ir::StackSlotData::new(
            cranelift_codegen::ir::StackSlotKind::ExplicitSlot,
            (len + 1) as u32,
            0,
        ));

        // Store each byte
        for (i, &byte) in bytes.iter().enumerate() {
            let byte_val = builder
                .ins()
                .iconst(cranelift_codegen::ir::types::I8, byte as i64);
            builder.ins().stack_store(byte_val, ss, i as i32);
        }
        // Null terminator
        let zero_byte = builder.ins().iconst(cranelift_codegen::ir::types::I8, 0);
        builder.ins().stack_store(zero_byte, ss, len as i32);

        let ptr = builder.ins().stack_addr(I64, ss, 0);
        let len_val = builder.ins().iconst(I64, len as i64);

        let make_string = self.rt_func_ids["lux_rt_make_string"];
        let make_string_ref = obj_module.declare_func_in_func(make_string, builder.func);
        let call = builder.ins().call(make_string_ref, &[ptr, len_val]);
        builder.inst_results(call)[0]
    }

    fn translate_float_lit(
        &mut self,
        builder: &mut FunctionBuilder,
        f: f64,
        obj_module: &mut ObjectModule,
    ) -> cranelift_codegen::ir::Value {
        // Allocate boxed float on heap: [BOXED_FLOAT(2), double_bits]
        let alloc_name = "lux_alloc";
        let alloc_id = if let Some(&id) = self.rt_func_ids.get(alloc_name) {
            id
        } else {
            let mut sig2 = obj_module.make_signature();
            sig2.params.push(AbiParam::new(I64));
            sig2.returns.push(AbiParam::new(I64));
            let id = obj_module
                .declare_function(alloc_name, Linkage::Import, &sig2)
                .unwrap();
            self.rt_func_ids.insert(alloc_name.to_string(), id);
            id
        };
        let alloc_ref = obj_module.declare_func_in_func(alloc_id, builder.func);
        let alloc_size = builder.ins().iconst(I64, 16); // 2 * sizeof(Value)
        let call = builder.ins().call(alloc_ref, &[alloc_size]);
        let heap_ptr = builder.inst_results(call)[0];

        // Store BOXED_FLOAT marker
        let marker = builder.ins().iconst(I64, runtime::BOXED_FLOAT);
        builder
            .ins()
            .store(cranelift_codegen::ir::MemFlags::new(), marker, heap_ptr, 0);

        // Store double bits as i64
        let bits = f.to_bits() as i64;
        let bits_val = builder.ins().iconst(I64, bits);
        builder.ins().store(
            cranelift_codegen::ir::MemFlags::new(),
            bits_val,
            heap_ptr,
            8,
        );

        // Tag with TAG_BOXED
        builder.ins().bor_imm(heap_ptr, runtime::TAG_BOXED)
    }

    // -----------------------------------------------------------------------
    // Call translation
    // -----------------------------------------------------------------------

    fn translate_call(
        &mut self,
        builder: &mut FunctionBuilder,
        module: &str,
        func: &str,
        args: &[CoreExpr],
        vars: &mut HashMap<String, Variable>,
        self_module: &str,
        obj_module: &mut ObjectModule,
    ) -> cranelift_codegen::ir::Value {
        // Handle erlang BIFs
        if module == "erlang" {
            return self.translate_erlang_bif(builder, func, args, vars, self_module, obj_module);
        }

        // Handle io:format (used by print)
        if module == "io" && func == "format" {
            let mut arg_vals = Vec::new();
            for arg in args {
                arg_vals.push(self.translate_expr(builder, arg, vars, self_module, obj_module));
            }
            if arg_vals.len() == 2 {
                let io_format = self.rt_func_ids["lux_rt_io_format"];
                let io_format_ref = obj_module.declare_func_in_func(io_format, builder.func);
                let call = builder
                    .ins()
                    .call(io_format_ref, &[arg_vals[0], arg_vals[1]]);
                return builder.inst_results(call)[0];
            }
            return builder.ins().iconst(I64, runtime::VALUE_NIL);
        }

        // Handle io_lib:format — used by string interpolation and to_string
        if module == "io_lib" && func == "format" {
            if args.len() == 2 {
                let fmt_val = self.translate_expr(builder, &args[0], vars, self_module, obj_module);
                let args_val =
                    self.translate_expr(builder, &args[1], vars, self_module, obj_module);
                let rt_name = "lux_rt_io_lib_format";
                let rt_id = if let Some(&id) = self.rt_func_ids.get(rt_name) {
                    id
                } else {
                    let mut sig = obj_module.make_signature();
                    sig.params.push(AbiParam::new(I64));
                    sig.params.push(AbiParam::new(I64));
                    sig.returns.push(AbiParam::new(I64));
                    let id = obj_module
                        .declare_function(rt_name, Linkage::Import, &sig)
                        .unwrap();
                    self.rt_func_ids.insert(rt_name.to_string(), id);
                    id
                };
                let rt_ref = obj_module.declare_func_in_func(rt_id, builder.func);
                let call = builder.ins().call(rt_ref, &[fmt_val, args_val]);
                return builder.inst_results(call)[0];
            }
        }

        // Route lists:* calls to C runtime
        if module == "lists" {
            let rt_name = format!("lux_lists_{}{}", func, args.len());
            if let Some(&rt_id) = self.rt_func_ids.get(&rt_name) {
                let mut arg_vals = Vec::new();
                for arg in args {
                    arg_vals.push(self.translate_expr(builder, arg, vars, self_module, obj_module));
                }
                let rt_ref = obj_module.declare_func_in_func(rt_id, builder.func);
                let call = builder.ins().call(rt_ref, &arg_vals);
                return builder.inst_results(call)[0];
            }
        }

        // Route string:* calls to C runtime
        if module == "string" {
            let rt_name = format!("lux_string_{}{}", func, args.len());
            if let Some(&rt_id) = self.rt_func_ids.get(&rt_name) {
                let mut arg_vals = Vec::new();
                for arg in args {
                    arg_vals.push(self.translate_expr(builder, arg, vars, self_module, obj_module));
                }
                let rt_ref = obj_module.declare_func_in_func(rt_id, builder.func);
                let call = builder.ins().call(rt_ref, &arg_vals);
                return builder.inst_results(call)[0];
            }
        }

        // Route maps:* calls to C runtime
        if module == "maps" {
            let rt_name = format!("lux_maps_{}{}", func, args.len());
            if let Some(&rt_id) = self.rt_func_ids.get(&rt_name) {
                let mut arg_vals = Vec::new();
                for arg in args {
                    arg_vals.push(self.translate_expr(builder, arg, vars, self_module, obj_module));
                }
                let rt_ref = obj_module.declare_func_in_func(rt_id, builder.func);
                let call = builder.ins().call(rt_ref, &arg_vals);
                return builder.inst_results(call)[0];
            }
        }

        // Cross-module call to another Lux function
        let symbol = mangle_symbol(module, func, args.len());
        self.translate_direct_call(builder, &symbol, args, vars, self_module, obj_module)
    }

    fn translate_direct_call(
        &mut self,
        builder: &mut FunctionBuilder,
        symbol: &str,
        args: &[CoreExpr],
        vars: &mut HashMap<String, Variable>,
        self_module: &str,
        obj_module: &mut ObjectModule,
    ) -> cranelift_codegen::ir::Value {
        let mut arg_vals = Vec::new();
        for arg in args {
            arg_vals.push(self.translate_expr(builder, arg, vars, self_module, obj_module));
        }

        let func_id = if let Some(&id) = self.func_ids.get(symbol) {
            id
        } else {
            // Declare it as an import (might be from another compilation unit)
            let mut sig = obj_module.make_signature();
            for _ in 0..args.len() {
                sig.params.push(AbiParam::new(I64));
            }
            sig.returns.push(AbiParam::new(I64));
            match obj_module.declare_function(symbol, Linkage::Import, &sig) {
                Ok(id) => {
                    self.func_ids.insert(symbol.to_string(), id);
                    id
                }
                Err(e) => {
                    // If declaration fails, return nil
                    eprintln!("warning: failed to declare function {}: {}", symbol, e);
                    return builder.ins().iconst(I64, runtime::VALUE_NIL);
                }
            }
        };

        let func_ref = obj_module.declare_func_in_func(func_id, builder.func);
        let call = builder.ins().call(func_ref, &arg_vals);
        builder.inst_results(call)[0]
    }

    // -----------------------------------------------------------------------
    // Erlang BIF translation
    // -----------------------------------------------------------------------

    fn translate_erlang_bif(
        &mut self,
        builder: &mut FunctionBuilder,
        func: &str,
        args: &[CoreExpr],
        vars: &mut HashMap<String, Variable>,
        self_module: &str,
        obj_module: &mut ObjectModule,
    ) -> cranelift_codegen::ir::Value {
        // Evaluate all arguments
        let arg_vals: Vec<cranelift_codegen::ir::Value> = args
            .iter()
            .map(|a| self.translate_expr(builder, a, vars, self_module, obj_module))
            .collect();

        match func {
            // Arithmetic (handles both tagged ints and boxed floats)
            "+" => {
                let rt = self.rt_func_ids["lux_rt_add"];
                let rt_ref = obj_module.declare_func_in_func(rt, builder.func);
                let call = builder.ins().call(rt_ref, &[arg_vals[0], arg_vals[1]]);
                builder.inst_results(call)[0]
            }
            "-" if arg_vals.len() == 2 => {
                let rt = self.rt_func_ids["lux_rt_sub"];
                let rt_ref = obj_module.declare_func_in_func(rt, builder.func);
                let call = builder.ins().call(rt_ref, &[arg_vals[0], arg_vals[1]]);
                builder.inst_results(call)[0]
            }
            "-" if arg_vals.len() == 1 => {
                let rt = self.rt_func_ids["lux_rt_negate"];
                let rt_ref = obj_module.declare_func_in_func(rt, builder.func);
                let call = builder.ins().call(rt_ref, &[arg_vals[0]]);
                builder.inst_results(call)[0]
            }
            "*" => {
                let rt = self.rt_func_ids["lux_rt_mul"];
                let rt_ref = obj_module.declare_func_in_func(rt, builder.func);
                let call = builder.ins().call(rt_ref, &[arg_vals[0], arg_vals[1]]);
                builder.inst_results(call)[0]
            }
            "/" => {
                let rt = self.rt_func_ids["lux_rt_float_div"];
                let rt_ref = obj_module.declare_func_in_func(rt, builder.func);
                let call = builder.ins().call(rt_ref, &[arg_vals[0], arg_vals[1]]);
                builder.inst_results(call)[0]
            }
            "div" => {
                let safe_div = self.rt_func_ids["lux_rt_safe_div"];
                let safe_div_ref = obj_module.declare_func_in_func(safe_div, builder.func);
                let call = builder
                    .ins()
                    .call(safe_div_ref, &[arg_vals[0], arg_vals[1]]);
                builder.inst_results(call)[0]
            }
            "rem" => {
                let safe_rem = self.rt_func_ids["lux_rt_safe_rem"];
                let safe_rem_ref = obj_module.declare_func_in_func(safe_rem, builder.func);
                let call = builder
                    .ins()
                    .call(safe_rem_ref, &[arg_vals[0], arg_vals[1]]);
                builder.inst_results(call)[0]
            }

            // Bitwise operations
            "band" => {
                let a = self.untag_int(builder, arg_vals[0]);
                let b = self.untag_int(builder, arg_vals[1]);
                let result = builder.ins().band(a, b);
                self.tag_int(builder, result)
            }
            "bor" => {
                let a = self.untag_int(builder, arg_vals[0]);
                let b = self.untag_int(builder, arg_vals[1]);
                let result = builder.ins().bor(a, b);
                self.tag_int(builder, result)
            }
            "bxor" => {
                let a = self.untag_int(builder, arg_vals[0]);
                let b = self.untag_int(builder, arg_vals[1]);
                let result = builder.ins().bxor(a, b);
                self.tag_int(builder, result)
            }
            "bnot" => {
                let a = self.untag_int(builder, arg_vals[0]);
                let result = builder.ins().bnot(a);
                self.tag_int(builder, result)
            }
            "bsl" => {
                let a = self.untag_int(builder, arg_vals[0]);
                let b = self.untag_int(builder, arg_vals[1]);
                let result = builder.ins().ishl(a, b);
                self.tag_int(builder, result)
            }
            "bsr" => {
                let a = self.untag_int(builder, arg_vals[0]);
                let b = self.untag_int(builder, arg_vals[1]);
                let result = builder.ins().sshr(a, b);
                self.tag_int(builder, result)
            }

            // Comparisons — return tagged atom true/false
            // Use deep equality (lux_rt_value_equal) to support strings and
            // other boxed types, falling back to raw comparison for immediates.
            "=:=" | "==" => {
                let eq_name = "lux_rt_value_equal";
                let eq_id = if let Some(&id) = self.rt_func_ids.get(eq_name) {
                    id
                } else {
                    let mut sig = obj_module.make_signature();
                    sig.params.push(AbiParam::new(I64));
                    sig.params.push(AbiParam::new(I64));
                    sig.returns.push(AbiParam::new(I64));
                    let id = obj_module
                        .declare_function(eq_name, Linkage::Import, &sig)
                        .unwrap();
                    self.rt_func_ids.insert(eq_name.to_string(), id);
                    id
                };
                let eq_ref = obj_module.declare_func_in_func(eq_id, builder.func);
                let call = builder.ins().call(eq_ref, &[arg_vals[0], arg_vals[1]]);
                builder.inst_results(call)[0]
            }
            "=/=" | "/=" => {
                let eq_name = "lux_rt_value_equal";
                let eq_id = if let Some(&id) = self.rt_func_ids.get(eq_name) {
                    id
                } else {
                    let mut sig = obj_module.make_signature();
                    sig.params.push(AbiParam::new(I64));
                    sig.params.push(AbiParam::new(I64));
                    sig.returns.push(AbiParam::new(I64));
                    let id = obj_module
                        .declare_function(eq_name, Linkage::Import, &sig)
                        .unwrap();
                    self.rt_func_ids.insert(eq_name.to_string(), id);
                    id
                };
                let eq_ref = obj_module.declare_func_in_func(eq_id, builder.func);
                let call = builder.ins().call(eq_ref, &[arg_vals[0], arg_vals[1]]);
                let eq_result = builder.inst_results(call)[0];
                // Flip: if VALUE_TRUE -> VALUE_FALSE, if VALUE_FALSE -> VALUE_TRUE
                let is_true = builder
                    .ins()
                    .icmp_imm(IntCC::Equal, eq_result, runtime::VALUE_TRUE);
                let flipped = builder.ins().bxor_imm(is_true, 1);
                self.bool_to_atom(builder, flipped)
            }
            "<" => {
                let rt = self.rt_func_ids["lux_rt_less_than"];
                let rt_ref = obj_module.declare_func_in_func(rt, builder.func);
                let call = builder.ins().call(rt_ref, &[arg_vals[0], arg_vals[1]]);
                builder.inst_results(call)[0]
            }
            "=<" => {
                let rt = self.rt_func_ids["lux_rt_less_equal"];
                let rt_ref = obj_module.declare_func_in_func(rt, builder.func);
                let call = builder.ins().call(rt_ref, &[arg_vals[0], arg_vals[1]]);
                builder.inst_results(call)[0]
            }
            ">" => {
                let rt = self.rt_func_ids["lux_rt_greater_than"];
                let rt_ref = obj_module.declare_func_in_func(rt, builder.func);
                let call = builder.ins().call(rt_ref, &[arg_vals[0], arg_vals[1]]);
                builder.inst_results(call)[0]
            }
            ">=" => {
                let rt = self.rt_func_ids["lux_rt_greater_equal"];
                let rt_ref = obj_module.declare_func_in_func(rt, builder.func);
                let call = builder.ins().call(rt_ref, &[arg_vals[0], arg_vals[1]]);
                builder.inst_results(call)[0]
            }

            // Boolean operations
            "and" => {
                // true = atom(0), false = atom(1)
                // and: both must be true (both equal to VALUE_TRUE)
                let a_is_true =
                    builder
                        .ins()
                        .icmp_imm(IntCC::Equal, arg_vals[0], runtime::VALUE_TRUE);
                let b_is_true =
                    builder
                        .ins()
                        .icmp_imm(IntCC::Equal, arg_vals[1], runtime::VALUE_TRUE);
                let both = builder.ins().band(a_is_true, b_is_true);
                self.bool_to_atom(builder, both)
            }
            "or" => {
                let a_is_true =
                    builder
                        .ins()
                        .icmp_imm(IntCC::Equal, arg_vals[0], runtime::VALUE_TRUE);
                let b_is_true =
                    builder
                        .ins()
                        .icmp_imm(IntCC::Equal, arg_vals[1], runtime::VALUE_TRUE);
                let either = builder.ins().bor(a_is_true, b_is_true);
                self.bool_to_atom(builder, either)
            }
            "not" => {
                let is_true =
                    builder
                        .ins()
                        .icmp_imm(IntCC::Equal, arg_vals[0], runtime::VALUE_TRUE);
                // Flip: if true → false, if false → true
                let flipped = builder.ins().bxor_imm(is_true, 1);
                self.bool_to_atom(builder, flipped)
            }

            // Type checks
            "is_integer" => {
                let tag = builder.ins().band_imm(arg_vals[0], runtime::TAG_MASK);
                let cmp = builder.ins().icmp_imm(IntCC::Equal, tag, runtime::TAG_INT);
                self.bool_to_atom(builder, cmp)
            }
            "is_atom" => {
                let tag = builder.ins().band_imm(arg_vals[0], runtime::TAG_MASK);
                let cmp = builder.ins().icmp_imm(IntCC::Equal, tag, runtime::TAG_ATOM);
                self.bool_to_atom(builder, cmp)
            }
            "is_list" => {
                let tag = builder.ins().band_imm(arg_vals[0], runtime::TAG_MASK);
                let is_cons = builder.ins().icmp_imm(IntCC::Equal, tag, runtime::TAG_CONS);
                let is_nil = builder
                    .ins()
                    .icmp_imm(IntCC::Equal, arg_vals[0], runtime::VALUE_NIL);
                let either = builder.ins().bor(is_cons, is_nil);
                self.bool_to_atom(builder, either)
            }
            "is_tuple" => {
                let tag = builder.ins().band_imm(arg_vals[0], runtime::TAG_MASK);
                let cmp = builder
                    .ins()
                    .icmp_imm(IntCC::Equal, tag, runtime::TAG_TUPLE);
                // Also check it's not null (tag 0 with value 0 = null pointer)
                let not_zero = builder.ins().icmp_imm(IntCC::NotEqual, arg_vals[0], 0);
                let both = builder.ins().band(cmp, not_zero);
                self.bool_to_atom(builder, both)
            }
            "is_binary" | "is_bitstring" => {
                // Strings are boxed with TAG_BOXED (4), check for BOXED_STRING(1) type
                let tag = builder.ins().band_imm(arg_vals[0], runtime::TAG_MASK);
                let is_boxed = builder
                    .ins()
                    .icmp_imm(IntCC::Equal, tag, runtime::TAG_BOXED);
                let raw_ptr = builder.ins().band_imm(arg_vals[0], !runtime::TAG_MASK);
                let safe = self.make_safe_dummy(builder);
                let ptr = builder.ins().select(is_boxed, raw_ptr, safe);
                let first_word =
                    builder
                        .ins()
                        .load(I64, cranelift_codegen::ir::MemFlags::new(), ptr, 0);
                let is_string = builder.ins().icmp_imm(IntCC::Equal, first_word, 1); // BOXED_STRING
                let result = builder.ins().band(is_boxed, is_string);
                self.bool_to_atom(builder, result)
            }
            "is_map" => {
                // Maps are TAG_BOXED with MAP_MARKER as first word
                // For simplicity, check TAG_BOXED (4) — could also be string
                // Use a runtime call for more precise check
                let tag = builder.ins().band_imm(arg_vals[0], runtime::TAG_MASK);
                let is_boxed = builder.ins().icmp_imm(IntCC::Equal, tag, 4);
                // Load first word to check for MAP_MARKER (0x4D41505F)
                let raw_ptr = builder.ins().band_imm(arg_vals[0], !runtime::TAG_MASK);
                let safe = self.make_safe_dummy(builder);
                let ptr = builder.ins().select(is_boxed, raw_ptr, safe);
                let first_word =
                    builder
                        .ins()
                        .load(I64, cranelift_codegen::ir::MemFlags::new(), ptr, 0);
                let is_map_marker = builder.ins().icmp_imm(IntCC::Equal, first_word, 0x4D41505F);
                let result = builder.ins().band(is_boxed, is_map_marker);
                self.bool_to_atom(builder, result)
            }
            "is_function" => {
                let tag = builder.ins().band_imm(arg_vals[0], runtime::TAG_MASK);
                let is_fun = builder.ins().icmp_imm(IntCC::Equal, tag, runtime::TAG_FUN);
                self.bool_to_atom(builder, is_fun)
            }
            "is_boolean" => {
                let is_true =
                    builder
                        .ins()
                        .icmp_imm(IntCC::Equal, arg_vals[0], runtime::VALUE_TRUE);
                let is_false =
                    builder
                        .ins()
                        .icmp_imm(IntCC::Equal, arg_vals[0], runtime::VALUE_FALSE);
                let result = builder.ins().bor(is_true, is_false);
                self.bool_to_atom(builder, result)
            }
            "is_float" => {
                // Boxed float: TAG_BOXED with BOXED_FLOAT(2) as first word
                let tag = builder.ins().band_imm(arg_vals[0], runtime::TAG_MASK);
                let is_boxed = builder
                    .ins()
                    .icmp_imm(IntCC::Equal, tag, runtime::TAG_BOXED);
                let raw_ptr = builder.ins().band_imm(arg_vals[0], !runtime::TAG_MASK);
                let safe = self.make_safe_dummy(builder);
                let ptr = builder.ins().select(is_boxed, raw_ptr, safe);
                let first_word =
                    builder
                        .ins()
                        .load(I64, cranelift_codegen::ir::MemFlags::new(), ptr, 0);
                let is_float_marker =
                    builder
                        .ins()
                        .icmp_imm(IntCC::Equal, first_word, runtime::BOXED_FLOAT);
                let result = builder.ins().band(is_boxed, is_float_marker);
                self.bool_to_atom(builder, result)
            }
            "is_number" => {
                // Number: either tagged int or boxed float
                let tag = builder.ins().band_imm(arg_vals[0], runtime::TAG_MASK);
                let is_int = builder.ins().icmp_imm(IntCC::Equal, tag, runtime::TAG_INT);
                let is_boxed = builder
                    .ins()
                    .icmp_imm(IntCC::Equal, tag, runtime::TAG_BOXED);
                let raw_ptr = builder.ins().band_imm(arg_vals[0], !runtime::TAG_MASK);
                let safe = self.make_safe_dummy(builder);
                let ptr = builder.ins().select(is_boxed, raw_ptr, safe);
                let first_word =
                    builder
                        .ins()
                        .load(I64, cranelift_codegen::ir::MemFlags::new(), ptr, 0);
                let is_float_marker =
                    builder
                        .ins()
                        .icmp_imm(IntCC::Equal, first_word, runtime::BOXED_FLOAT);
                let is_float = builder.ins().band(is_boxed, is_float_marker);
                let result = builder.ins().bor(is_int, is_float);
                self.bool_to_atom(builder, result)
            }
            "is_pid" | "is_reference" => builder.ins().iconst(I64, runtime::VALUE_FALSE),

            // List operations
            "length" => {
                let list_len = self.rt_func_ids["lux_rt_list_length"];
                let list_len_ref = obj_module.declare_func_in_func(list_len, builder.func);
                let call = builder.ins().call(list_len_ref, &[arg_vals[0]]);
                builder.inst_results(call)[0]
            }
            "hd" => {
                let cons_head = self.rt_func_ids["lux_rt_cons_head"];
                let cons_head_ref = obj_module.declare_func_in_func(cons_head, builder.func);
                let call = builder.ins().call(cons_head_ref, &[arg_vals[0]]);
                builder.inst_results(call)[0]
            }
            "tl" => {
                let cons_tail = self.rt_func_ids["lux_rt_cons_tail"];
                let cons_tail_ref = obj_module.declare_func_in_func(cons_tail, builder.func);
                let call = builder.ins().call(cons_tail_ref, &[arg_vals[0]]);
                builder.inst_results(call)[0]
            }
            "abs" => {
                // abs: negate if negative
                let zero = builder.ins().iconst(I64, runtime::make_tagged_int(0));
                let rt_lt = self.rt_func_ids["lux_rt_less_than"];
                let rt_lt_ref = obj_module.declare_func_in_func(rt_lt, builder.func);
                let call = builder.ins().call(rt_lt_ref, &[arg_vals[0], zero]);
                let cmp_result = builder.inst_results(call)[0];
                let is_neg = builder
                    .ins()
                    .icmp_imm(IntCC::Equal, cmp_result, runtime::VALUE_TRUE);
                let rt_neg = self.rt_func_ids["lux_rt_negate"];
                let rt_neg_ref = obj_module.declare_func_in_func(rt_neg, builder.func);
                let neg_call = builder.ins().call(rt_neg_ref, &[arg_vals[0]]);
                let neg_val = builder.inst_results(neg_call)[0];
                builder.ins().select(is_neg, neg_val, arg_vals[0])
            }
            "max" => {
                let rt = self.rt_func_ids["lux_rt_greater_than"];
                let rt_ref = obj_module.declare_func_in_func(rt, builder.func);
                let call = builder.ins().call(rt_ref, &[arg_vals[0], arg_vals[1]]);
                let cmp_result = builder.inst_results(call)[0];
                let a_gt_b = builder
                    .ins()
                    .icmp_imm(IntCC::Equal, cmp_result, runtime::VALUE_TRUE);
                builder.ins().select(a_gt_b, arg_vals[0], arg_vals[1])
            }
            "min" => {
                let rt = self.rt_func_ids["lux_rt_less_than"];
                let rt_ref = obj_module.declare_func_in_func(rt, builder.func);
                let call = builder.ins().call(rt_ref, &[arg_vals[0], arg_vals[1]]);
                let cmp_result = builder.inst_results(call)[0];
                let a_lt_b = builder
                    .ins()
                    .icmp_imm(IntCC::Equal, cmp_result, runtime::VALUE_TRUE);
                builder.ins().select(a_lt_b, arg_vals[0], arg_vals[1])
            }

            // Tuple operations
            "element" => {
                let tuple_elem = self.rt_func_ids["lux_rt_tuple_element"];
                let tuple_elem_ref = obj_module.declare_func_in_func(tuple_elem, builder.func);
                // element(index, tuple) — Erlang convention: 1-based index
                let idx = self.untag_int(builder, arg_vals[0]);
                let one = builder.ins().iconst(I64, 1);
                let zero_idx = builder.ins().isub(idx, one);
                let call = builder.ins().call(tuple_elem_ref, &[arg_vals[1], zero_idx]);
                builder.inst_results(call)[0]
            }
            "tuple_size" => {
                let tuple_arity = self.rt_func_ids["lux_rt_tuple_arity"];
                let tuple_arity_ref = obj_module.declare_func_in_func(tuple_arity, builder.func);
                let call = builder.ins().call(tuple_arity_ref, &[arg_vals[0]]);
                let raw = builder.inst_results(call)[0];
                self.tag_int(builder, raw)
            }

            // String / conversion
            "iolist_to_binary" => {
                // Pass through — string values already are our "binary" type
                arg_vals[0]
            }
            "byte_size" | "bit_size" => {
                // Return the byte length of a boxed string as a tagged int
                let rt_name = "lux_rt_byte_size";
                let rt_id = if let Some(&id) = self.rt_func_ids.get(rt_name) {
                    id
                } else {
                    let mut sig = obj_module.make_signature();
                    sig.params.push(AbiParam::new(I64));
                    sig.returns.push(AbiParam::new(I64));
                    let id = obj_module
                        .declare_function(rt_name, Linkage::Import, &sig)
                        .unwrap();
                    self.rt_func_ids.insert(rt_name.to_string(), id);
                    id
                };
                let rt_ref = obj_module.declare_func_in_func(rt_id, builder.func);
                let call = builder.ins().call(rt_ref, &[arg_vals[0]]);
                builder.inst_results(call)[0]
            }
            "binary_part" => {
                let rt = self.rt_func_ids["lux_rt_binary_slice"];
                let rt_ref = obj_module.declare_func_in_func(rt, builder.func);
                let call = builder
                    .ins()
                    .call(rt_ref, &[arg_vals[0], arg_vals[1], arg_vals[2]]);
                builder.inst_results(call)[0]
            }
            "integer_to_list" | "integer_to_binary" => {
                let to_string = self.rt_func_ids["lux_rt_to_string"];
                let to_string_ref = obj_module.declare_func_in_func(to_string, builder.func);
                let call = builder.ins().call(to_string_ref, &[arg_vals[0]]);
                builder.inst_results(call)[0]
            }

            "display" => {
                let display = self.rt_func_ids["lux_rt_display"];
                let display_ref = obj_module.declare_func_in_func(display, builder.func);
                let call = builder.ins().call(display_ref, &[arg_vals[0]]);
                builder.inst_results(call)[0]
            }

            // Error / control
            "throw" => {
                let throw_fn = self.rt_func_ids["lux_rt_erlang_throw"];
                let throw_ref = obj_module.declare_func_in_func(throw_fn, builder.func);
                builder.ins().call(throw_ref, &[arg_vals[0]]);
                builder.ins().iconst(I64, runtime::VALUE_NIL)
            }
            "error" => {
                let error_fn = self.rt_func_ids["lux_rt_erlang_error"];
                let error_ref = obj_module.declare_func_in_func(error_fn, builder.func);
                builder.ins().call(error_ref, &[arg_vals[0]]);
                builder.ins().iconst(I64, runtime::VALUE_NIL)
            }
            "exit" => {
                let error_fn = self.rt_func_ids["lux_rt_erlang_error"];
                let error_ref = obj_module.declare_func_in_func(error_fn, builder.func);
                builder.ins().call(error_ref, &[arg_vals[0]]);
                builder.ins().iconst(I64, runtime::VALUE_NIL)
            }

            // String/list concatenation
            "++" => {
                // Check if args are strings (boxed) — use string_concat; else list append
                let string_concat = self.rt_func_ids["lux_rt_string_concat"];
                let string_concat_ref =
                    obj_module.declare_func_in_func(string_concat, builder.func);
                let call = builder
                    .ins()
                    .call(string_concat_ref, &[arg_vals[0], arg_vals[1]]);
                builder.inst_results(call)[0]
            }

            // list_to_binary / binary_to_list
            "list_to_binary" => {
                let rt_name = "lux_rt_list_to_binary";
                let rt_id = if let Some(&id) = self.rt_func_ids.get(rt_name) {
                    id
                } else {
                    let mut sig = obj_module.make_signature();
                    sig.params.push(AbiParam::new(I64));
                    sig.returns.push(AbiParam::new(I64));
                    let id = obj_module
                        .declare_function(rt_name, Linkage::Import, &sig)
                        .unwrap();
                    self.rt_func_ids.insert(rt_name.to_string(), id);
                    id
                };
                let rt_ref = obj_module.declare_func_in_func(rt_id, builder.func);
                let call = builder.ins().call(rt_ref, &[arg_vals[0]]);
                builder.inst_results(call)[0]
            }
            "binary_to_list" => {
                let rt_name = "lux_rt_binary_to_list";
                let rt_id = if let Some(&id) = self.rt_func_ids.get(rt_name) {
                    id
                } else {
                    let mut sig = obj_module.make_signature();
                    sig.params.push(AbiParam::new(I64));
                    sig.returns.push(AbiParam::new(I64));
                    let id = obj_module
                        .declare_function(rt_name, Linkage::Import, &sig)
                        .unwrap();
                    self.rt_func_ids.insert(rt_name.to_string(), id);
                    id
                };
                let rt_ref = obj_module.declare_func_in_func(rt_id, builder.func);
                let call = builder.ins().call(rt_ref, &[arg_vals[0]]);
                builder.inst_results(call)[0]
            }
            // Process dictionary
            "put" => {
                let rt_name = "lux_rt_put";
                let rt_id = if let Some(&id) = self.rt_func_ids.get(rt_name) {
                    id
                } else {
                    let mut sig = obj_module.make_signature();
                    sig.params.push(AbiParam::new(I64));
                    sig.params.push(AbiParam::new(I64));
                    sig.returns.push(AbiParam::new(I64));
                    let id = obj_module
                        .declare_function(rt_name, Linkage::Import, &sig)
                        .unwrap();
                    self.rt_func_ids.insert(rt_name.to_string(), id);
                    id
                };
                let rt_ref = obj_module.declare_func_in_func(rt_id, builder.func);
                let call = builder.ins().call(rt_ref, &[arg_vals[0], arg_vals[1]]);
                builder.inst_results(call)[0]
            }
            "get" => {
                let rt_name = "lux_rt_get";
                let rt_id = if let Some(&id) = self.rt_func_ids.get(rt_name) {
                    id
                } else {
                    let mut sig = obj_module.make_signature();
                    sig.params.push(AbiParam::new(I64));
                    sig.returns.push(AbiParam::new(I64));
                    let id = obj_module
                        .declare_function(rt_name, Linkage::Import, &sig)
                        .unwrap();
                    self.rt_func_ids.insert(rt_name.to_string(), id);
                    id
                };
                let rt_ref = obj_module.declare_func_in_func(rt_id, builder.func);
                let call = builder.ins().call(rt_ref, &[arg_vals[0]]);
                builder.inst_results(call)[0]
            }
            "system_time" => {
                let rt_name = "lux_rt_system_time";
                let rt_id = if let Some(&id) = self.rt_func_ids.get(rt_name) {
                    id
                } else {
                    let mut sig = obj_module.make_signature();
                    sig.params.push(AbiParam::new(I64));
                    sig.returns.push(AbiParam::new(I64));
                    let id = obj_module
                        .declare_function(rt_name, Linkage::Import, &sig)
                        .unwrap();
                    self.rt_func_ids.insert(rt_name.to_string(), id);
                    id
                };
                let rt_ref = obj_module.declare_func_in_func(rt_id, builder.func);
                let call = builder.ins().call(rt_ref, &[arg_vals[0]]);
                builder.inst_results(call)[0]
            }
            "make_ref" => {
                let rt_name = "lux_rt_make_ref";
                let rt_id = if let Some(&id) = self.rt_func_ids.get(rt_name) {
                    id
                } else {
                    let mut sig = obj_module.make_signature();
                    sig.returns.push(AbiParam::new(I64));
                    let id = obj_module
                        .declare_function(rt_name, Linkage::Import, &sig)
                        .unwrap();
                    self.rt_func_ids.insert(rt_name.to_string(), id);
                    id
                };
                let rt_ref = obj_module.declare_func_in_func(rt_id, builder.func);
                let call = builder.ins().call(rt_ref, &[]);
                builder.inst_results(call)[0]
            }
            "send" => {
                // send(pid, msg) — return msg
                if arg_vals.len() >= 2 {
                    arg_vals[1]
                } else {
                    builder.ins().iconst(I64, runtime::VALUE_NIL)
                }
            }

            // Self (not supported natively)
            "self" => builder.ins().iconst(I64, runtime::VALUE_NIL),

            // Spawn (not supported natively)
            "spawn" | "spawn_link" => builder.ins().iconst(I64, runtime::VALUE_NIL),

            // Default: unsupported BIF
            _ => {
                eprintln!(
                    "warning: unsupported erlang BIF: erlang:{}/{}",
                    func,
                    args.len()
                );
                builder.ins().iconst(I64, runtime::VALUE_NIL)
            }
        }
    }

    // -----------------------------------------------------------------------
    // Pattern matching / case
    // -----------------------------------------------------------------------

    fn translate_case(
        &mut self,
        builder: &mut FunctionBuilder,
        scrutinee: &CoreExpr,
        clauses: &[CoreClause],
        vars: &mut HashMap<String, Variable>,
        self_module: &str,
        obj_module: &mut ObjectModule,
    ) -> cranelift_codegen::ir::Value {
        let scrut_val = self.translate_expr(builder, scrutinee, vars, self_module, obj_module);

        // Save scrutinee in a variable so it's available across blocks
        let scrut_var = builder.declare_var(I64);
        builder.def_var(scrut_var, scrut_val);

        let merge_block = builder.create_block();
        builder.append_block_param(merge_block, I64);

        // For each clause, create: test block, guard block, body block
        let clause_blocks: Vec<_> = clauses
            .iter()
            .map(|_| {
                let test = builder.create_block();
                let guard = builder.create_block();
                let body = builder.create_block();
                (test, guard, body)
            })
            .collect();

        let fail_block = builder.create_block();

        // Jump to first clause test
        if !clause_blocks.is_empty() {
            builder.ins().jump(clause_blocks[0].0, &[]);
        } else {
            builder.ins().jump(fail_block, &[]);
        }

        for (i, clause) in clauses.iter().enumerate() {
            let (test_block, guard_block, body_block) = clause_blocks[i];
            let next_block = if i + 1 < clauses.len() {
                clause_blocks[i + 1].0
            } else {
                fail_block
            };

            // --- Test block: try to match pattern ---
            builder.switch_to_block(test_block);
            builder.seal_block(test_block);

            let scrut = builder.use_var(scrut_var);
            let mut clause_vars = vars.clone();

            let matched = if clause.patterns.len() == 1 {
                self.translate_pattern_match(
                    builder,
                    scrut,
                    &clause.patterns[0],
                    &mut clause_vars,
                    obj_module,
                )
            } else {
                builder.ins().iconst(cranelift_codegen::ir::types::I8, 1)
            };

            builder
                .ins()
                .brif(matched, guard_block, &[], next_block, &[]);

            // --- Guard block: bind pattern vars, evaluate guard ---
            builder.switch_to_block(guard_block);
            builder.seal_block(guard_block);

            let scrut_for_guard = builder.use_var(scrut_var);
            let mut guard_vars = vars.clone();
            if !clause.patterns.is_empty() {
                self.bind_pattern_vars(
                    builder,
                    scrut_for_guard,
                    &clause.patterns[0],
                    &mut guard_vars,
                );
            }

            // Evaluate guard expression
            let guard_is_true = match &clause.guard {
                CoreExpr::Lit(CoreLit::Atom(a)) if a == "true" => {
                    // Guard is always true — skip check
                    None
                }
                guard_expr => {
                    let guard_val = self.translate_expr(
                        builder,
                        guard_expr,
                        &mut guard_vars,
                        self_module,
                        obj_module,
                    );
                    let is_true =
                        builder
                            .ins()
                            .icmp_imm(IntCC::Equal, guard_val, runtime::VALUE_TRUE);
                    Some(is_true)
                }
            };

            if let Some(guard_check) = guard_is_true {
                builder
                    .ins()
                    .brif(guard_check, body_block, &[], next_block, &[]);
            } else {
                builder.ins().jump(body_block, &[]);
            }

            // --- Body block: evaluate clause body ---
            builder.switch_to_block(body_block);
            builder.seal_block(body_block);

            // Re-bind pattern variables for the body block
            let scrut2 = builder.use_var(scrut_var);
            let mut body_vars = vars.clone();
            if !clause.patterns.is_empty() {
                self.bind_pattern_vars(builder, scrut2, &clause.patterns[0], &mut body_vars);
            }

            let body_result = self.translate_expr(
                builder,
                &clause.body,
                &mut body_vars,
                self_module,
                obj_module,
            );
            builder
                .ins()
                .jump(merge_block, &[BlockArg::Value(body_result)]);
        }

        // Fail block
        builder.switch_to_block(fail_block);
        builder.seal_block(fail_block);
        let fail_scrut = builder.use_var(scrut_var);
        let case_error = self.rt_func_ids["lux_rt_case_clause_error"];
        let case_error_ref = obj_module.declare_func_in_func(case_error, builder.func);
        builder.ins().call(case_error_ref, &[fail_scrut]);
        // Unreachable, but Cranelift needs a terminator; use trap
        builder
            .ins()
            .trap(cranelift_codegen::ir::TrapCode::unwrap_user(1));

        // Merge block
        builder.switch_to_block(merge_block);
        builder.seal_block(merge_block);

        builder.block_params(merge_block)[0]
    }

    /// Check if a value matches a pattern; returns an i8 boolean (0 or 1).
    /// Does NOT bind variables (use bind_pattern_vars for that).
    fn translate_pattern_match(
        &mut self,
        builder: &mut FunctionBuilder,
        value: cranelift_codegen::ir::Value,
        pattern: &CorePattern,
        _vars: &mut HashMap<String, Variable>,
        obj_module: &mut ObjectModule,
    ) -> cranelift_codegen::ir::Value {
        match pattern {
            CorePattern::Var(_) => {
                // Variable always matches
                builder.ins().iconst(cranelift_codegen::ir::types::I8, 1)
            }
            CorePattern::Lit(CoreLit::String(s)) => {
                // String pattern: build the string, then call lux_rt_string_equal
                let expected = self.translate_string_lit(builder, s, obj_module);
                let str_eq = self.rt_func_ids["lux_rt_string_equal"];
                let str_eq_ref = obj_module.declare_func_in_func(str_eq, builder.func);
                let call = builder.ins().call(str_eq_ref, &[value, expected]);
                let result_i64 = builder.inst_results(call)[0];
                // Convert i64 result to i8
                builder
                    .ins()
                    .ireduce(cranelift_codegen::ir::types::I8, result_i64)
            }
            CorePattern::Lit(lit) => {
                let expected = self.translate_lit_as_i64(lit);
                let expected_val = builder.ins().iconst(I64, expected);
                builder.ins().icmp(IntCC::Equal, value, expected_val)
            }
            CorePattern::Nil => builder
                .ins()
                .icmp_imm(IntCC::Equal, value, runtime::VALUE_NIL),
            CorePattern::Tuple(pats) => {
                // Check: tag is TUPLE
                let tag = builder.ins().band_imm(value, runtime::TAG_MASK);
                let is_tuple = builder
                    .ins()
                    .icmp_imm(IntCC::Equal, tag, runtime::TAG_TUPLE);

                if pats.is_empty() {
                    return is_tuple;
                }

                // Use a safe pointer: if tag doesn't match, use a dummy to avoid segfault
                let raw_ptr = builder.ins().band_imm(value, !runtime::TAG_MASK);
                let safe_dummy = self.make_safe_dummy(builder);
                let ptr = builder.ins().select(is_tuple, raw_ptr, safe_dummy);

                // Check: arity matches
                let arity = builder
                    .ins()
                    .load(I64, cranelift_codegen::ir::MemFlags::new(), ptr, 0);
                let expected_arity = builder.ins().iconst(I64, pats.len() as i64);
                let arity_ok = builder.ins().icmp(IntCC::Equal, arity, expected_arity);
                let mut result = builder.ins().band(is_tuple, arity_ok);

                // Check each element pattern
                for (i, pat) in pats.iter().enumerate() {
                    match pat {
                        CorePattern::Var(_) => {}
                        _ => {
                            let offset = ((i + 1) * 8) as i32;
                            let elem = builder.ins().load(
                                I64,
                                cranelift_codegen::ir::MemFlags::new(),
                                ptr,
                                offset,
                            );
                            let elem_match =
                                self.translate_pattern_match(builder, elem, pat, _vars, obj_module);
                            result = builder.ins().band(result, elem_match);
                        }
                    }
                }

                result
            }
            CorePattern::Cons(head_pat, tail_pat) => {
                // Check tag is CONS
                let tag = builder.ins().band_imm(value, runtime::TAG_MASK);
                let is_cons = builder.ins().icmp_imm(IntCC::Equal, tag, runtime::TAG_CONS);

                let head_needs_check = !matches!(head_pat.as_ref(), CorePattern::Var(_));
                let tail_needs_check = !matches!(tail_pat.as_ref(), CorePattern::Var(_));

                if !head_needs_check && !tail_needs_check {
                    return is_cons;
                }

                // Safe pointer to avoid segfault on non-cons values
                let raw_ptr = builder.ins().band_imm(value, !runtime::TAG_MASK);
                let safe_dummy = self.make_safe_dummy(builder);
                let ptr = builder.ins().select(is_cons, raw_ptr, safe_dummy);
                let mut result = is_cons;

                if head_needs_check {
                    let head =
                        builder
                            .ins()
                            .load(I64, cranelift_codegen::ir::MemFlags::new(), ptr, 0);
                    let head_match =
                        self.translate_pattern_match(builder, head, head_pat, _vars, obj_module);
                    result = builder.ins().band(result, head_match);
                }

                if tail_needs_check {
                    let tail =
                        builder
                            .ins()
                            .load(I64, cranelift_codegen::ir::MemFlags::new(), ptr, 8);
                    let tail_match =
                        self.translate_pattern_match(builder, tail, tail_pat, _vars, obj_module);
                    result = builder.ins().band(result, tail_match);
                }

                result
            }
            CorePattern::Alias(_, inner) => {
                self.translate_pattern_match(builder, value, inner, _vars, obj_module)
            }
            CorePattern::Binary(_) => {
                // Binary pattern matching not supported
                builder.ins().iconst(cranelift_codegen::ir::types::I8, 0)
            }
        }
    }

    /// Bind pattern variables after we know the match succeeded.
    fn bind_pattern_vars(
        &mut self,
        builder: &mut FunctionBuilder,
        value: cranelift_codegen::ir::Value,
        pattern: &CorePattern,
        vars: &mut HashMap<String, Variable>,
    ) {
        match pattern {
            CorePattern::Var(name) => {
                if name != "_" {
                    let var = builder.declare_var(I64);
                    builder.def_var(var, value);
                    vars.insert(name.clone(), var);
                }
            }
            CorePattern::Lit(_) | CorePattern::Nil | CorePattern::Binary(_) => {
                // No variables to bind
            }
            CorePattern::Tuple(pats) => {
                // Extract each element and bind recursively
                for (i, pat) in pats.iter().enumerate() {
                    let elem = self.extract_tuple_element(builder, value, i as i64);
                    self.bind_pattern_vars(builder, elem, pat, vars);
                }
            }
            CorePattern::Cons(head_pat, tail_pat) => {
                let head = self.extract_cons_head(builder, value);
                let tail = self.extract_cons_tail(builder, value);
                self.bind_pattern_vars(builder, head, head_pat, vars);
                self.bind_pattern_vars(builder, tail, tail_pat, vars);
            }
            CorePattern::Alias(name, inner) => {
                // Bind the alias name to the whole value
                let var = builder.declare_var(I64);
                builder.def_var(var, value);
                vars.insert(name.clone(), var);
                self.bind_pattern_vars(builder, value, inner, vars);
            }
        }
    }

    // -----------------------------------------------------------------------
    // Tuple / list helpers (inline, no runtime call)
    // -----------------------------------------------------------------------

    fn translate_tuple(
        &mut self,
        builder: &mut FunctionBuilder,
        elems: &[CoreExpr],
        vars: &mut HashMap<String, Variable>,
        self_module: &str,
        obj_module: &mut ObjectModule,
    ) -> cranelift_codegen::ir::Value {
        let elem_vals: Vec<_> = elems
            .iter()
            .map(|e| self.translate_expr(builder, e, vars, self_module, obj_module))
            .collect();

        match elem_vals.len() {
            2 => {
                let rt = self.rt_func_ids["lux_rt_make_tuple2"];
                let rt_ref = obj_module.declare_func_in_func(rt, builder.func);
                let call = builder.ins().call(rt_ref, &[elem_vals[0], elem_vals[1]]);
                builder.inst_results(call)[0]
            }
            3 => {
                let rt = self.rt_func_ids["lux_rt_make_tuple3"];
                let rt_ref = obj_module.declare_func_in_func(rt, builder.func);
                let call = builder
                    .ins()
                    .call(rt_ref, &[elem_vals[0], elem_vals[1], elem_vals[2]]);
                builder.inst_results(call)[0]
            }
            _ => {
                // For other arities, build on the stack and call make_tuple_n
                // Allocate a stack slot for the element array
                let slot_size = (elem_vals.len() * 8) as u32;
                let ss =
                    builder.create_sized_stack_slot(cranelift_codegen::ir::StackSlotData::new(
                        cranelift_codegen::ir::StackSlotKind::ExplicitSlot,
                        slot_size,
                        8,
                    ));
                for (i, val) in elem_vals.iter().enumerate() {
                    builder.ins().stack_store(*val, ss, (i * 8) as i32);
                }
                let ptr = builder.ins().stack_addr(I64, ss, 0);
                let arity = builder.ins().iconst(I64, elem_vals.len() as i64);

                // Call lux_rt_make_tuple_n
                // We need to declare this function if not already done
                let rt_name = "lux_rt_make_tuple_n";
                let rt_id = if let Some(&id) = self.rt_func_ids.get(rt_name) {
                    id
                } else {
                    let mut sig = obj_module.make_signature();
                    sig.params.push(AbiParam::new(I64));
                    sig.params.push(AbiParam::new(I64));
                    sig.returns.push(AbiParam::new(I64));
                    let id = obj_module
                        .declare_function(rt_name, Linkage::Import, &sig)
                        .unwrap();
                    self.rt_func_ids.insert(rt_name.to_string(), id);
                    id
                };
                let rt_ref = obj_module.declare_func_in_func(rt_id, builder.func);
                let call = builder.ins().call(rt_ref, &[arity, ptr]);
                builder.inst_results(call)[0]
            }
        }
    }

    fn extract_tuple_element(
        &self,
        builder: &mut FunctionBuilder,
        tuple: cranelift_codegen::ir::Value,
        index: i64,
    ) -> cranelift_codegen::ir::Value {
        // ptr = tuple & ~TAG_MASK
        let ptr = builder.ins().band_imm(tuple, !runtime::TAG_MASK);
        // element address = ptr + (index + 1) * 8
        let offset = (index + 1) * 8;
        let elem = builder.ins().load(
            I64,
            cranelift_codegen::ir::MemFlags::new(),
            ptr,
            offset as i32,
        );
        elem
    }

    fn extract_cons_head(
        &self,
        builder: &mut FunctionBuilder,
        cons: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        let ptr = builder.ins().band_imm(cons, !runtime::TAG_MASK);
        builder
            .ins()
            .load(I64, cranelift_codegen::ir::MemFlags::new(), ptr, 0)
    }

    fn extract_cons_tail(
        &self,
        builder: &mut FunctionBuilder,
        cons: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        let ptr = builder.ins().band_imm(cons, !runtime::TAG_MASK);
        builder
            .ins()
            .load(I64, cranelift_codegen::ir::MemFlags::new(), ptr, 8)
    }

    // -----------------------------------------------------------------------
    // Primop
    // -----------------------------------------------------------------------

    fn translate_primop(
        &mut self,
        builder: &mut FunctionBuilder,
        op: &str,
        args: &[CoreExpr],
        vars: &mut HashMap<String, Variable>,
        self_module: &str,
        obj_module: &mut ObjectModule,
    ) -> cranelift_codegen::ir::Value {
        match op {
            "match_fail" | "case_clause" => {
                let match_error = self.rt_func_ids["lux_rt_match_error"];
                let match_error_ref = obj_module.declare_func_in_func(match_error, builder.func);
                builder.ins().call(match_error_ref, &[]);
                builder
                    .ins()
                    .trap(cranelift_codegen::ir::TrapCode::unwrap_user(1));
                // This is unreachable, but we need to return something for the type system
                // Create a new block and return nil from there
                let unreachable_block = builder.create_block();
                builder.switch_to_block(unreachable_block);
                builder.seal_block(unreachable_block);
                builder.ins().iconst(I64, runtime::VALUE_NIL)
            }
            "send" => {
                // send(pid, msg) — not supported natively, return msg
                if args.len() >= 2 {
                    self.translate_expr(builder, &args[1], vars, self_module, obj_module)
                } else {
                    builder.ins().iconst(I64, runtime::VALUE_NIL)
                }
            }
            _ => {
                // Unknown primop — return nil
                builder.ins().iconst(I64, runtime::VALUE_NIL)
            }
        }
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    /// Create a stack-allocated "safe zone" for guarded loads in pattern matching.
    /// Returns a pointer to a zeroed 64-byte region on the stack.
    fn make_safe_dummy(&self, builder: &mut FunctionBuilder) -> cranelift_codegen::ir::Value {
        let ss = builder.create_sized_stack_slot(cranelift_codegen::ir::StackSlotData::new(
            cranelift_codegen::ir::StackSlotKind::ExplicitSlot,
            64, // enough for a tuple/cons with several elements
            8,
        ));
        // Zero out the first 64 bytes (8 values)
        let zero = builder.ins().iconst(I64, 0);
        for i in 0..8 {
            builder.ins().stack_store(zero, ss, i * 8);
        }
        builder.ins().stack_addr(I64, ss, 0)
    }

    fn untag_int(
        &self,
        builder: &mut FunctionBuilder,
        val: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        builder.ins().sshr_imm(val, runtime::TAG_BITS)
    }

    fn tag_int(
        &self,
        builder: &mut FunctionBuilder,
        val: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        let shifted = builder.ins().ishl_imm(val, runtime::TAG_BITS);
        builder.ins().bor_imm(shifted, runtime::TAG_INT)
    }

    /// Convert an i8 boolean (0/1) to a tagged atom (VALUE_TRUE / VALUE_FALSE).
    fn bool_to_atom(
        &self,
        builder: &mut FunctionBuilder,
        cond: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        let true_val = builder.ins().iconst(I64, runtime::VALUE_TRUE);
        let false_val = builder.ins().iconst(I64, runtime::VALUE_FALSE);
        builder.ins().select(cond, true_val, false_val)
    }

    /// Compute the i64 representation of a literal (for pattern matching comparisons).
    fn translate_lit_as_i64(&mut self, lit: &CoreLit) -> i64 {
        match lit {
            CoreLit::Int(n) => runtime::make_tagged_int(*n),
            CoreLit::Atom(name) => {
                let idx = self.intern_atom(name);
                runtime::make_tagged_atom(idx)
            }
            CoreLit::Nil => runtime::VALUE_NIL,
            CoreLit::Float(_) => runtime::make_tagged_int(0), // Placeholder
            CoreLit::String(_) => runtime::VALUE_NIL,         // Placeholder
        }
    }

    // -----------------------------------------------------------------------
    // Binary / string translation
    // -----------------------------------------------------------------------

    fn translate_binary(
        &mut self,
        builder: &mut FunctionBuilder,
        segments: &[CoreBinarySegment],
        vars: &mut HashMap<String, Variable>,
        self_module: &str,
        obj_module: &mut ObjectModule,
    ) -> cranelift_codegen::ir::Value {
        // Check if this is a binary concatenation (all segments are Binary/All)
        let all_binary_concat = segments.iter().all(|seg| {
            matches!(
                (&seg.size, &seg.kind),
                (CoreBinarySize::All, CoreBinaryKind::Binary)
            )
        });
        if all_binary_concat && segments.len() >= 2 {
            // Concatenate binary values
            let mut result =
                self.translate_expr(builder, &segments[0].value, vars, self_module, obj_module);
            for seg in &segments[1..] {
                let next = self.translate_expr(builder, &seg.value, vars, self_module, obj_module);
                let concat = self.rt_func_ids["lux_rt_string_concat"];
                let concat_ref = obj_module.declare_func_in_func(concat, builder.func);
                let call = builder.ins().call(concat_ref, &[result, next]);
                result = builder.inst_results(call)[0];
            }
            return result;
        }

        // Try to extract all segments as constant bytes (common case for string literals)
        let mut const_bytes: Vec<u8> = Vec::new();
        let mut all_const = true;

        for seg in segments {
            match (&seg.value, &seg.size, &seg.kind) {
                (
                    CoreExpr::Lit(CoreLit::Int(n)),
                    CoreBinarySize::Bits(8),
                    CoreBinaryKind::Integer,
                ) => {
                    const_bytes.push(*n as u8);
                }
                (
                    CoreExpr::Lit(CoreLit::Int(n)),
                    CoreBinarySize::Bits(8),
                    CoreBinaryKind::BigInteger,
                ) => {
                    const_bytes.push(*n as u8);
                }
                _ => {
                    all_const = false;
                    break;
                }
            }
        }

        if all_const && !const_bytes.is_empty() {
            // Optimize: create string from constant bytes on the stack
            let len = const_bytes.len();
            let ss = builder.create_sized_stack_slot(cranelift_codegen::ir::StackSlotData::new(
                cranelift_codegen::ir::StackSlotKind::ExplicitSlot,
                (len + 1) as u32,
                0,
            ));
            for (i, &byte) in const_bytes.iter().enumerate() {
                let byte_val = builder
                    .ins()
                    .iconst(cranelift_codegen::ir::types::I8, byte as i64);
                builder.ins().stack_store(byte_val, ss, i as i32);
            }
            let zero = builder.ins().iconst(cranelift_codegen::ir::types::I8, 0);
            builder.ins().stack_store(zero, ss, len as i32);

            let ptr = builder.ins().stack_addr(I64, ss, 0);
            let len_val = builder.ins().iconst(I64, len as i64);

            let make_string = self.rt_func_ids["lux_rt_make_string"];
            let make_string_ref = obj_module.declare_func_in_func(make_string, builder.func);
            let call = builder.ins().call(make_string_ref, &[ptr, len_val]);
            return builder.inst_results(call)[0];
        }

        if segments.is_empty() {
            // Empty binary
            let ptr = builder.ins().iconst(I64, 0);
            let len_val = builder.ins().iconst(I64, 0);
            let make_string = self.rt_func_ids["lux_rt_make_string"];
            let make_string_ref = obj_module.declare_func_in_func(make_string, builder.func);
            let call = builder.ins().call(make_string_ref, &[ptr, len_val]);
            return builder.inst_results(call)[0];
        }

        // Dynamic case: evaluate segments at runtime
        // Allocate a buffer on the stack for up to 256 bytes
        let max_len = segments.len().min(256);
        let ss = builder.create_sized_stack_slot(cranelift_codegen::ir::StackSlotData::new(
            cranelift_codegen::ir::StackSlotKind::ExplicitSlot,
            (max_len + 1) as u32,
            0,
        ));

        for (i, seg) in segments.iter().enumerate().take(max_len) {
            let val = self.translate_expr(builder, &seg.value, vars, self_module, obj_module);
            // Untag if it's a tagged int
            let byte = builder.ins().sshr_imm(val, runtime::TAG_BITS);
            let byte_trunc = builder
                .ins()
                .ireduce(cranelift_codegen::ir::types::I8, byte);
            builder.ins().stack_store(byte_trunc, ss, i as i32);
        }
        let zero = builder.ins().iconst(cranelift_codegen::ir::types::I8, 0);
        builder.ins().stack_store(zero, ss, max_len as i32);

        let ptr = builder.ins().stack_addr(I64, ss, 0);
        let len_val = builder.ins().iconst(I64, max_len as i64);

        let make_string = self.rt_func_ids["lux_rt_make_string"];
        let make_string_ref = obj_module.declare_func_in_func(make_string, builder.func);
        let call = builder.ins().call(make_string_ref, &[ptr, len_val]);
        builder.inst_results(call)[0]
    }

    // -----------------------------------------------------------------------
    // Closure / lambda translation
    // -----------------------------------------------------------------------

    fn translate_closure(
        &mut self,
        builder: &mut FunctionBuilder,
        params: &[String],
        body: &CoreExpr,
        vars: &HashMap<String, Variable>,
        self_module: &str,
        obj_module: &mut ObjectModule,
    ) -> cranelift_codegen::ir::Value {
        // Compute free variables (captures)
        let mut bound: std::collections::HashSet<String> = params.iter().cloned().collect();
        bound.insert("_".to_string());
        let free = free_vars_expr(body, &bound);
        let captures: Vec<String> = free.into_iter().collect();

        // Generate unique lambda name
        let lambda_idx = self.lambda_counter;
        self.lambda_counter += 1;
        let symbol = format!("lux_lambda_{}", lambda_idx);

        // Declare the lambda function: (env: I64, params...) -> I64
        let mut sig = obj_module.make_signature();
        sig.params.push(AbiParam::new(I64)); // env
        for _ in params {
            sig.params.push(AbiParam::new(I64));
        }
        sig.returns.push(AbiParam::new(I64));
        let func_id = obj_module
            .declare_function(&symbol, Linkage::Export, &sig)
            .unwrap();
        self.func_ids.insert(symbol.clone(), func_id);

        // Defer lambda compilation
        self.deferred_lambdas.push(DeferredLambda {
            func_id,
            symbol,
            params: params.to_vec(),
            captures: captures.clone(),
            body: body.clone(),
            self_module: self_module.to_string(),
        });

        // Create closure object: [func_ptr, n_captures, cap0, cap1, ...]
        let func_ref = obj_module.declare_func_in_func(func_id, builder.func);
        let func_addr = builder.ins().func_addr(I64, func_ref);

        let n_captures = captures.len();
        let obj_size = (2 + n_captures) * 8;
        // Allocate on stack temporarily, then copy to heap via make_tuple_n
        // Actually, let's build the closure manually
        let ss = builder.create_sized_stack_slot(cranelift_codegen::ir::StackSlotData::new(
            cranelift_codegen::ir::StackSlotKind::ExplicitSlot,
            obj_size as u32,
            8,
        ));
        builder.ins().stack_store(func_addr, ss, 0);
        let n_caps_val = builder.ins().iconst(I64, n_captures as i64);
        builder.ins().stack_store(n_caps_val, ss, 8);
        for (i, cap_name) in captures.iter().enumerate() {
            let cap_val = if let Some(&var) = vars.get(cap_name) {
                builder.use_var(var)
            } else {
                builder.ins().iconst(I64, runtime::VALUE_NIL)
            };
            builder.ins().stack_store(cap_val, ss, (16 + i * 8) as i32);
        }

        // Copy to heap (use make_tuple_n with arity = 2+n_captures-1, but
        // simpler to just alloc and memcpy conceptually).
        // For now, use the stack slot address directly — it's stable within the function.
        // Actually, stack slots won't survive after the function returns!
        // We need to allocate on the heap. Use lux_alloc from C runtime.
        // But we don't have lux_alloc declared... Let me use make_tuple_n.

        // Alternative: use make_tuple_n to allocate on heap.
        // The closure layout is the same as a tuple with (2+n_captures) elements,
        // except element 0 is func_ptr and element 1 is n_captures.
        // make_tuple_n(arity, elems_ptr) allocates [arity, e0, e1, ...] on heap.
        // We can abuse it by treating the closure as a "tuple" for allocation.
        // But the tuple tag is 0, and we need TAG_FUN (6).
        // So let's just manually allocate via a runtime call.

        // Declare lux_alloc if not done
        let alloc_name = "lux_alloc";
        let alloc_id = if let Some(&id) = self.rt_func_ids.get(alloc_name) {
            id
        } else {
            let mut sig2 = obj_module.make_signature();
            sig2.params.push(AbiParam::new(I64));
            sig2.returns.push(AbiParam::new(I64));
            let id = obj_module
                .declare_function(alloc_name, Linkage::Import, &sig2)
                .unwrap();
            self.rt_func_ids.insert(alloc_name.to_string(), id);
            id
        };
        let alloc_ref = obj_module.declare_func_in_func(alloc_id, builder.func);
        let alloc_size = builder.ins().iconst(I64, obj_size as i64);
        let call = builder.ins().call(alloc_ref, &[alloc_size]);
        let heap_ptr = builder.inst_results(call)[0];

        // Store func_ptr, n_captures, captures into heap
        builder.ins().store(
            cranelift_codegen::ir::MemFlags::new(),
            func_addr,
            heap_ptr,
            0,
        );
        builder.ins().store(
            cranelift_codegen::ir::MemFlags::new(),
            n_caps_val,
            heap_ptr,
            8,
        );
        for (i, cap_name) in captures.iter().enumerate() {
            let cap_val = if let Some(&var) = vars.get(cap_name) {
                builder.use_var(var)
            } else {
                builder.ins().iconst(I64, runtime::VALUE_NIL)
            };
            builder.ins().store(
                cranelift_codegen::ir::MemFlags::new(),
                cap_val,
                heap_ptr,
                (16 + i * 8) as i32,
            );
        }

        // Tag with TAG_FUN
        builder.ins().bor_imm(heap_ptr, runtime::TAG_FUN)
    }

    /// Create a closure value wrapping a named function (LocalFunRef / RemoteFunRef).
    /// The closure has no captures; its body simply forwards args to the target.
    fn translate_fun_ref_closure(
        &mut self,
        builder: &mut FunctionBuilder,
        module: &str,
        name: &str,
        arity: usize,
        _vars: &HashMap<String, Variable>,
        obj_module: &mut ObjectModule,
    ) -> cranelift_codegen::ir::Value {
        // Generate a unique wrapper lambda that forwards to the target
        let lambda_idx = self.lambda_counter;
        self.lambda_counter += 1;
        let wrapper_symbol = format!("lux_funref_wrap_{}", lambda_idx);

        // Declare wrapper: (env: I64, param0..paramN) -> I64
        let mut sig = obj_module.make_signature();
        sig.params.push(AbiParam::new(I64)); // env (unused)
        for _ in 0..arity {
            sig.params.push(AbiParam::new(I64));
        }
        sig.returns.push(AbiParam::new(I64));
        let wrapper_id = obj_module
            .declare_function(&wrapper_symbol, Linkage::Export, &sig)
            .unwrap();
        self.func_ids.insert(wrapper_symbol.clone(), wrapper_id);

        // Build the wrapper params list (just forwarding names)
        let params: Vec<String> = (0..arity).map(|i| format!("__fref_p{}", i)).collect();

        // Build the body: Call(module, name, [Var(p0), Var(p1), ...])
        let body = CoreExpr::Call(
            module.to_string(),
            name.to_string(),
            params.iter().map(|p| CoreExpr::Var(p.clone())).collect(),
        );

        self.deferred_lambdas.push(DeferredLambda {
            func_id: wrapper_id,
            symbol: wrapper_symbol,
            params,
            captures: Vec::new(),
            body,
            self_module: module.to_string(),
        });

        // Build closure object on heap: [func_ptr, 0 captures]
        let func_ref = obj_module.declare_func_in_func(wrapper_id, builder.func);
        let func_addr = builder.ins().func_addr(I64, func_ref);

        let alloc_name = "lux_alloc";
        let alloc_id = if let Some(&id) = self.rt_func_ids.get(alloc_name) {
            id
        } else {
            let mut sig2 = obj_module.make_signature();
            sig2.params.push(AbiParam::new(I64));
            sig2.returns.push(AbiParam::new(I64));
            let id = obj_module
                .declare_function(alloc_name, Linkage::Import, &sig2)
                .unwrap();
            self.rt_func_ids.insert(alloc_name.to_string(), id);
            id
        };
        let alloc_ref = obj_module.declare_func_in_func(alloc_id, builder.func);
        let alloc_size = builder.ins().iconst(I64, 16); // func_ptr + n_captures
        let call = builder.ins().call(alloc_ref, &[alloc_size]);
        let heap_ptr = builder.inst_results(call)[0];

        builder.ins().store(
            cranelift_codegen::ir::MemFlags::new(),
            func_addr,
            heap_ptr,
            0,
        );
        let zero = builder.ins().iconst(I64, 0);
        builder
            .ins()
            .store(cranelift_codegen::ir::MemFlags::new(), zero, heap_ptr, 8);

        // Tag with TAG_FUN
        builder.ins().bor_imm(heap_ptr, runtime::TAG_FUN)
    }

    // -----------------------------------------------------------------------
    // Map creation
    // -----------------------------------------------------------------------

    fn translate_map(
        &mut self,
        builder: &mut FunctionBuilder,
        entries: &[(CoreExpr, CoreExpr)],
        vars: &mut HashMap<String, Variable>,
        self_module: &str,
        obj_module: &mut ObjectModule,
    ) -> cranelift_codegen::ir::Value {
        if entries.is_empty() {
            // Empty map
            let zero = builder.ins().iconst(I64, 0);
            let make_map = self.rt_func_ids["lux_rt_make_map"];
            let make_map_ref = obj_module.declare_func_in_func(make_map, builder.func);
            let null = builder.ins().iconst(I64, 0);
            let call = builder.ins().call(make_map_ref, &[zero, null, null]);
            return builder.inst_results(call)[0];
        }

        let n = entries.len();
        // Build keys and vals arrays on the stack
        let keys_slot = builder.create_sized_stack_slot(cranelift_codegen::ir::StackSlotData::new(
            cranelift_codegen::ir::StackSlotKind::ExplicitSlot,
            (n * 8) as u32,
            8,
        ));
        let vals_slot = builder.create_sized_stack_slot(cranelift_codegen::ir::StackSlotData::new(
            cranelift_codegen::ir::StackSlotKind::ExplicitSlot,
            (n * 8) as u32,
            8,
        ));

        for (i, (k, v)) in entries.iter().enumerate() {
            let key_val = self.translate_expr(builder, k, vars, self_module, obj_module);
            let val_val = self.translate_expr(builder, v, vars, self_module, obj_module);
            builder
                .ins()
                .stack_store(key_val, keys_slot, (i * 8) as i32);
            builder
                .ins()
                .stack_store(val_val, vals_slot, (i * 8) as i32);
        }

        let keys_ptr = builder.ins().stack_addr(I64, keys_slot, 0);
        let vals_ptr = builder.ins().stack_addr(I64, vals_slot, 0);
        let n_val = builder.ins().iconst(I64, n as i64);

        let make_map = self.rt_func_ids["lux_rt_make_map"];
        let make_map_ref = obj_module.declare_func_in_func(make_map, builder.func);
        let call = builder
            .ins()
            .call(make_map_ref, &[n_val, keys_ptr, vals_ptr]);
        builder.inst_results(call)[0]
    }
}

// ---------------------------------------------------------------------------
// Free variable analysis
// ---------------------------------------------------------------------------

fn free_vars_expr(
    expr: &CoreExpr,
    bound: &std::collections::HashSet<String>,
) -> std::collections::HashSet<String> {
    use std::collections::HashSet;
    match expr {
        CoreExpr::Var(name) => {
            if bound.contains(name) || name == "_" || name.starts_with('_') {
                HashSet::new()
            } else {
                let mut s = HashSet::new();
                s.insert(name.clone());
                s
            }
        }
        CoreExpr::Lit(_) | CoreExpr::LocalFunRef(_, _) | CoreExpr::RemoteFunRef(_, _, _) => {
            HashSet::new()
        }
        CoreExpr::Tuple(elems) => {
            let mut free = HashSet::new();
            for e in elems {
                free.extend(free_vars_expr(e, bound));
            }
            free
        }
        CoreExpr::List(elems, tail) => {
            let mut free = HashSet::new();
            for e in elems {
                free.extend(free_vars_expr(e, bound));
            }
            free.extend(free_vars_expr(tail, bound));
            free
        }
        CoreExpr::Cons(h, t) => {
            let mut free = free_vars_expr(h, bound);
            free.extend(free_vars_expr(t, bound));
            free
        }
        CoreExpr::Map(entries) => {
            let mut free = HashSet::new();
            for (k, v) in entries {
                free.extend(free_vars_expr(k, bound));
                free.extend(free_vars_expr(v, bound));
            }
            free
        }
        CoreExpr::Binary(segs) => {
            let mut free = HashSet::new();
            for seg in segs {
                free.extend(free_vars_expr(&seg.value, bound));
            }
            free
        }
        CoreExpr::Apply(f, args) => {
            let mut free = free_vars_expr(f, bound);
            for a in args {
                free.extend(free_vars_expr(a, bound));
            }
            free
        }
        CoreExpr::Call(_, _, args) => {
            let mut free = HashSet::new();
            for a in args {
                free.extend(free_vars_expr(a, bound));
            }
            free
        }
        CoreExpr::Let(bindings, body) => {
            let mut free = HashSet::new();
            let mut new_bound = bound.clone();
            for (name, val) in bindings {
                free.extend(free_vars_expr(val, &new_bound));
                new_bound.insert(name.clone());
            }
            free.extend(free_vars_expr(body, &new_bound));
            free
        }
        CoreExpr::Case(scrut, clauses) => {
            let mut free = free_vars_expr(scrut, bound);
            for clause in clauses {
                let mut cb = bound.clone();
                for pat in &clause.patterns {
                    collect_pattern_bound_vars(pat, &mut cb);
                }
                free.extend(free_vars_expr(&clause.guard, &cb));
                free.extend(free_vars_expr(&clause.body, &cb));
            }
            free
        }
        CoreExpr::Fun(params, body) => {
            let mut new_bound = bound.clone();
            for p in params {
                new_bound.insert(p.clone());
            }
            free_vars_expr(body, &new_bound)
        }
        CoreExpr::Seq(a, b) => {
            let mut free = free_vars_expr(a, bound);
            free.extend(free_vars_expr(b, bound));
            free
        }
        CoreExpr::Primop(_, args) => {
            let mut free = HashSet::new();
            for a in args {
                free.extend(free_vars_expr(a, bound));
            }
            free
        }
        CoreExpr::Try {
            body,
            vars,
            handler,
            evars,
            catch,
        } => {
            let mut free = free_vars_expr(body, bound);
            let mut hb = bound.clone();
            for v in vars {
                hb.insert(v.clone());
            }
            free.extend(free_vars_expr(handler, &hb));
            let mut eb = bound.clone();
            for v in evars {
                eb.insert(v.clone());
            }
            free.extend(free_vars_expr(catch, &eb));
            free
        }
        CoreExpr::Receive { clauses, timeout } => {
            let mut free = HashSet::new();
            for clause in clauses {
                let mut cb = bound.clone();
                for pat in &clause.patterns {
                    collect_pattern_bound_vars(pat, &mut cb);
                }
                free.extend(free_vars_expr(&clause.guard, &cb));
                free.extend(free_vars_expr(&clause.body, &cb));
            }
            if let Some((ms, body)) = timeout {
                free.extend(free_vars_expr(ms, bound));
                free.extend(free_vars_expr(body, bound));
            }
            free
        }
    }
}

fn collect_pattern_bound_vars(pat: &CorePattern, bound: &mut std::collections::HashSet<String>) {
    match pat {
        CorePattern::Var(name) => {
            bound.insert(name.clone());
        }
        CorePattern::Tuple(pats) => {
            for p in pats {
                collect_pattern_bound_vars(p, bound);
            }
        }
        CorePattern::Cons(h, t) => {
            collect_pattern_bound_vars(h, bound);
            collect_pattern_bound_vars(t, bound);
        }
        CorePattern::Alias(name, inner) => {
            bound.insert(name.clone());
            collect_pattern_bound_vars(inner, bound);
        }
        CorePattern::Binary(segs) => {
            for seg in segs {
                collect_pattern_bound_vars(&seg.pattern, bound);
            }
        }
        CorePattern::Lit(_) | CorePattern::Nil => {}
    }
}

// ---------------------------------------------------------------------------
// Symbol mangling
// ---------------------------------------------------------------------------

fn mangle_symbol(module_hash: &str, func_name: &str, arity: usize) -> String {
    // Use a simple mangling scheme: lux_{hash}_{name}_{arity}
    // Replace any non-alphanumeric characters with underscores
    let clean_hash: String = module_hash
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    let clean_name: String = func_name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("lux_{}_{}{}", clean_hash, clean_name, arity)
}
