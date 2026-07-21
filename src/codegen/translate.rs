use crate::codegen::emit::Emitter;
use crate::codegen::erlang::*;
use crate::syntax::ast::{self, Expr, InterpolatedPart, Item, Module, Pattern, Stmt};
use crate::syntax::content_address;
use std::collections::{HashMap, HashSet};

const ARTIFACT_HASH_SCHEMA: &str = "lux-artifact-v2";
const BUILD_KEY_SCHEMA: &str = "lux-build-v1";
const BUILD_TARGET: &str = "beam";
const BUILD_BACKEND: &str = "erlang-core";

/// Helper enum for let binding types
#[derive(Debug)]
enum LetPart {
    Simple(String),       // Simple variable binding
    Pattern(CorePattern), // Pattern destructuring
}

/// Convert PascalCase to snake_case
fn to_snake_case(s: &str) -> String {
    let mut result = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                result.push('_');
            }
            result.push(c.to_ascii_lowercase());
        } else {
            result.push(c);
        }
    }
    result
}

pub struct Translator {
    var_counter: u32,
    module_name: String,
    /// Track source/local function symbols and their content-addressed modules.
    functions: Vec<LocalFunctionSymbol>,
    resolved_symbols: HashMap<(String, usize), String>,
}

#[derive(Debug, Clone)]
struct LocalFunctionSymbol {
    source_name: String,
    body_hash: String,
    abi_hash: String,
    artifact_hash: String,
    build_key: String,
    module_name: String,
    arity: usize,
}

#[derive(Debug, Clone)]
pub struct FunctionModuleMetadata {
    pub source_name: String,
    pub body_hash: String,
    pub abi_hash: String,
    pub module_name: String,
    pub module_hash: String,
    pub build_key: String,
    pub arity: usize,
    pub dependencies: Vec<String>,
}

#[derive(Debug, Clone)]
struct FunctionHashInfo {
    body_hash: String,
    abi_hash: String,
    artifact_hash: String,
    build_key: String,
    direct_callees: HashSet<String>,
}

#[derive(Debug, Clone)]
struct SccMemberTemplate {
    body_hash: String,
    abi_hash: String,
    template_source: String,
    external_refs: Vec<String>,
}

#[derive(Debug, Clone)]
struct FingerprintState {
    next_binder_id: usize,
    scopes: Vec<HashMap<String, usize>>,
}

impl FingerprintState {
    fn new() -> Self {
        Self {
            next_binder_id: 0,
            scopes: vec![HashMap::new()],
        }
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn bind(&mut self, name: &str) -> usize {
        let id = self.next_binder_id;
        self.next_binder_id += 1;
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.to_string(), id);
        }
        id
    }

    fn lookup(&self, name: &str) -> Option<usize> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
    }
}

#[derive(Debug, Clone)]
pub struct TranslatedFunctionModules {
    pub modules: Vec<CoreModule>,
    pub metadata: Vec<FunctionModuleMetadata>,
    pub entry_module: Option<String>,
    pub entry_arity: Option<usize>,
}

impl Translator {
    pub fn new() -> Self {
        Translator {
            var_counter: 0,
            module_name: String::new(),
            functions: Vec::new(),
            resolved_symbols: HashMap::new(),
        }
    }

    fn fresh_var(&mut self) -> String {
        let v = self.var_counter;
        self.var_counter += 1;
        format!("_v{}", v)
    }

    pub fn translate_module(&mut self, module: &Module) -> CoreModule {
        let translated = self.translate_function_modules(module);
        if translated.modules.is_empty() {
            CoreModule {
                name: self.module_name.clone(),
                exports: vec![],
                functions: vec![],
            }
        } else {
            translated.modules[0].clone()
        }
    }

    pub fn translate_function_modules(&mut self, module: &Module) -> TranslatedFunctionModules {
        self.translate_function_modules_with_resolution(module, &HashMap::new())
    }

    pub fn translate_function_modules_with_resolution(
        &mut self,
        module: &Module,
        resolved_symbols: &HashMap<(String, usize), String>,
    ) -> TranslatedFunctionModules {
        self.module_name = module.name.clone().unwrap_or_else(|| "main".to_string());
        self.resolved_symbols = resolved_symbols.clone();

        let function_hashes = self.compute_function_hashes(module);
        self.functions.clear();
        for item in &module.items {
            if let Item::Function(func) = item {
                let key = format!("{}/{}", func.name, func.params.len());
                let hash_info = function_hashes
                    .get(&key)
                    .cloned()
                    .expect("function hash info must exist from hashing pass");
                self.functions.push(LocalFunctionSymbol {
                    source_name: func.name.clone(),
                    body_hash: hash_info.body_hash.clone(),
                    abi_hash: hash_info.abi_hash.clone(),
                    artifact_hash: hash_info.artifact_hash.clone(),
                    build_key: hash_info.build_key.clone(),
                    module_name: hash_info.artifact_hash,
                    arity: func.params.len(),
                });
            }
        }

        let mut modules = Vec::new();
        let mut metadata = Vec::new();
        let mut entry_module = None;
        let mut entry_arity = None;

        for item in &module.items {
            match item {
                Item::Function(func) => {
                    let symbol = self
                        .lookup_function_symbol(&func.name, func.params.len())
                        .expect("function symbol must exist from first pass")
                        .clone();
                    let core_func = self.translate_function(func, "apply");
                    modules.push(CoreModule {
                        name: symbol.module_name.clone(),
                        exports: vec![("apply".to_string(), func.params.len())],
                        functions: vec![core_func],
                    });
                    metadata.push(FunctionModuleMetadata {
                        source_name: symbol.source_name.clone(),
                        body_hash: symbol.body_hash.clone(),
                        abi_hash: symbol.abi_hash.clone(),
                        module_name: symbol.module_name.clone(),
                        module_hash: symbol.artifact_hash.clone(),
                        build_key: symbol.build_key.clone(),
                        arity: symbol.arity,
                        dependencies: function_hashes
                            .get(&format!("{}/{}", func.name, func.params.len()))
                            .map(|info| {
                                let mut deps: Vec<String> = info
                                    .direct_callees
                                    .iter()
                                    .filter_map(|callee| {
                                        function_hashes
                                            .get(callee)
                                            .map(|callee_info| callee_info.artifact_hash.clone())
                                    })
                                    .collect();
                                deps.sort();
                                deps
                            })
                            .unwrap_or_default(),
                    });

                    if symbol.source_name == "main" && symbol.arity == 0 {
                        entry_module = Some(symbol.module_name.clone());
                        entry_arity = Some(0);
                    }
                }
                Item::Enum(_)
                | Item::Struct(_)
                | Item::TypeAlias(_)
                | Item::Extern(_)
                | Item::Use(_) => {}
            }
        }

        TranslatedFunctionModules {
            modules,
            metadata,
            entry_module,
            entry_arity,
        }
    }

    fn compute_function_hashes(&mut self, module: &Module) -> HashMap<String, FunctionHashInfo> {
        let functions: Vec<&ast::Function> = module
            .items
            .iter()
            .filter_map(|item| match item {
                Item::Function(func) => Some(func),
                _ => None,
            })
            .collect();

        let local_keys: HashSet<String> = functions
            .iter()
            .map(|func| format!("{}/{}", func.name, func.params.len()))
            .collect();

        let mut body_hashes: HashMap<String, String> = HashMap::new();
        let mut abi_hashes: HashMap<String, String> = HashMap::new();
        let mut direct_callees: HashMap<String, HashSet<String>> = HashMap::new();
        let function_map: HashMap<String, &ast::Function> = functions
            .iter()
            .map(|func| (format!("{}/{}", func.name, func.params.len()), *func))
            .collect();

        for func in &functions {
            let key = format!("{}/{}", func.name, func.params.len());
            let (fingerprint, callees) = self.function_fingerprint(func, &key, &local_keys);
            let abi_fingerprint = self.function_abi_fingerprint(func);
            body_hashes.insert(
                key.clone(),
                content_address::hash_tagged("lux-body-v1", &[&fingerprint]),
            );
            abi_hashes.insert(
                key.clone(),
                content_address::hash_tagged("lux-abi-v1", &[&abi_fingerprint]),
            );
            direct_callees.insert(key, callees);
        }

        let sccs = self.compute_sccs(function_map.keys().cloned().collect(), &direct_callees);
        let mut scc_index_by_key = HashMap::new();
        for (index, scc) in sccs.iter().enumerate() {
            for key in scc {
                scc_index_by_key.insert(key.clone(), index);
            }
        }

        let scc_deps = self.compute_scc_dependencies(&sccs, &scc_index_by_key, &direct_callees);
        let topo_sccs = self.topo_sort_sccs(&scc_deps);

        let mut artifact_hashes = HashMap::new();
        let mut build_keys = HashMap::new();

        for scc_index in topo_sccs {
            let scc = &sccs[scc_index];
            let mut ordered_members = scc.clone();
            ordered_members.sort_by(|left, right| {
                let left_body = body_hashes.get(left).cloned().unwrap_or_default();
                let right_body = body_hashes.get(right).cloned().unwrap_or_default();
                let left_abi = abi_hashes.get(left).cloned().unwrap_or_default();
                let right_abi = abi_hashes.get(right).cloned().unwrap_or_default();
                left_body
                    .cmp(&right_body)
                    .then(left_abi.cmp(&right_abi))
                    .then(left.cmp(right))
            });

            let member_templates: Vec<SccMemberTemplate> = ordered_members
                .iter()
                .map(|key| {
                    let func = function_map
                        .get(key)
                        .copied()
                        .expect("function must exist for key");
                    let direct = direct_callees.get(key).cloned().unwrap_or_default();
                    let (template_source, external_refs) = self.emit_template_module(
                        func,
                        key,
                        &ordered_members,
                        &scc_index_by_key,
                        &body_hashes,
                        &abi_hashes,
                        &artifact_hashes,
                        &direct,
                    );
                    SccMemberTemplate {
                        body_hash: body_hashes.get(key).cloned().unwrap_or_default(),
                        abi_hash: abi_hashes.get(key).cloned().unwrap_or_default(),
                        template_source,
                        external_refs,
                    }
                })
                .collect();

            let scc_template = self.serialize_scc_template(&member_templates);

            for (slot, key) in ordered_members.iter().enumerate() {
                let slot_text = slot.to_string();
                let artifact_hash = content_address::hash_tagged(
                    ARTIFACT_HASH_SCHEMA,
                    &[&scc_template, &slot_text],
                );
                let build_key = content_address::hash_tagged(
                    BUILD_KEY_SCHEMA,
                    &[
                        &artifact_hash,
                        env!("CARGO_PKG_VERSION"),
                        BUILD_TARGET,
                        BUILD_BACKEND,
                        "erlc",
                        "opt:none",
                        "runtime:default",
                    ],
                );
                artifact_hashes.insert(key.clone(), artifact_hash);
                build_keys.insert(key.clone(), build_key);
            }
        }

        function_map
            .keys()
            .map(|key| {
                (
                    key.clone(),
                    FunctionHashInfo {
                        body_hash: body_hashes.get(key).cloned().unwrap_or_default(),
                        abi_hash: abi_hashes.get(key).cloned().unwrap_or_default(),
                        artifact_hash: artifact_hashes.get(key).cloned().unwrap_or_default(),
                        build_key: build_keys.get(key).cloned().unwrap_or_default(),
                        direct_callees: direct_callees.get(key).cloned().unwrap_or_default(),
                    },
                )
            })
            .collect()
    }

    fn resolved_symbol_hash(&self, name: &str, arity: usize) -> Option<&String> {
        self.resolved_symbols.get(&(name.to_string(), arity))
    }

    fn serialize_scc_template(&self, members: &[SccMemberTemplate]) -> String {
        let mut out = String::new();
        out.push_str("schema:");
        out.push_str(ARTIFACT_HASH_SCHEMA);
        out.push_str("|target:");
        out.push_str(BUILD_TARGET);
        out.push_str("|backend:");
        out.push_str(BUILD_BACKEND);
        out.push_str("|members:");
        for member in members {
            self.push_serialized_part(&mut out, &member.body_hash);
            self.push_serialized_part(&mut out, &member.abi_hash);
            self.push_serialized_part(&mut out, &member.external_refs.join(","));
            self.push_serialized_part(&mut out, &member.template_source);
        }
        out
    }

    fn push_serialized_part(&self, out: &mut String, value: &str) {
        out.push('|');
        out.push_str(&value.len().to_string());
        out.push(':');
        out.push_str(value);
    }

    fn function_abi_fingerprint(&self, func: &ast::Function) -> String {
        let type_params: HashMap<String, usize> = func
            .type_params
            .iter()
            .enumerate()
            .map(|(index, name)| (name.clone(), index))
            .collect();
        let params = func
            .params
            .iter()
            .map(|param| match &param.ty {
                Some(ty) => self.type_expr_fingerprint(ty, &type_params),
                None => "_".to_string(),
            })
            .collect::<Vec<_>>()
            .join(",");
        let return_type = func
            .return_type
            .as_ref()
            .map(|ty| self.type_expr_fingerprint(ty, &type_params))
            .unwrap_or_else(|| "_".to_string());
        format!(
            "fn$arity:{}|type_params:{}|params:[{}]|return:{}",
            func.params.len(),
            func.type_params.len(),
            params,
            return_type
        )
    }

    fn type_expr_fingerprint(
        &self,
        ty: &ast::TypeExpr,
        type_params: &HashMap<String, usize>,
    ) -> String {
        match ty {
            ast::TypeExpr::Named(name, args, _) => {
                if let Some(index) = type_params.get(name) {
                    format!("tvar:{index}")
                } else {
                    let mut out = format!("named:{name}[");
                    for arg in args {
                        out.push_str(&self.type_expr_fingerprint(arg, type_params));
                        out.push(',');
                    }
                    out.push(']');
                    out
                }
            }
            ast::TypeExpr::Tuple(items, _) => {
                let mut out = String::from("tuple(");
                for item in items {
                    out.push_str(&self.type_expr_fingerprint(item, type_params));
                    out.push(',');
                }
                out.push(')');
                out
            }
            ast::TypeExpr::Function(params, return_ty, _) => {
                let mut out = String::from("fun(");
                for param in params {
                    out.push_str(&self.type_expr_fingerprint(param, type_params));
                    out.push(',');
                }
                out.push_str(")->");
                out.push_str(&self.type_expr_fingerprint(return_ty, type_params));
                out
            }
            ast::TypeExpr::Record(fields, _) => {
                let mut out = String::from("record(");
                for (name, value) in fields {
                    out.push_str(name);
                    out.push(':');
                    out.push_str(&self.type_expr_fingerprint(value, type_params));
                    out.push(',');
                }
                out.push(')');
                out
            }
            ast::TypeExpr::Unit(_) => "unit".to_string(),
        }
    }

    fn compute_sccs(
        &self,
        mut keys: Vec<String>,
        graph: &HashMap<String, HashSet<String>>,
    ) -> Vec<Vec<String>> {
        keys.sort();
        let mut stack = Vec::new();
        let mut on_stack = HashSet::new();
        let mut index = 0usize;
        let mut indices = HashMap::new();
        let mut lowlinks = HashMap::new();
        let mut sccs = Vec::new();

        for key in keys {
            if !indices.contains_key(&key) {
                Self::strong_connect(
                    &key,
                    graph,
                    &mut index,
                    &mut indices,
                    &mut lowlinks,
                    &mut stack,
                    &mut on_stack,
                    &mut sccs,
                );
            }
        }

        sccs
    }

    fn strong_connect(
        key: &str,
        graph: &HashMap<String, HashSet<String>>,
        index: &mut usize,
        indices: &mut HashMap<String, usize>,
        lowlinks: &mut HashMap<String, usize>,
        stack: &mut Vec<String>,
        on_stack: &mut HashSet<String>,
        sccs: &mut Vec<Vec<String>>,
    ) {
        indices.insert(key.to_string(), *index);
        lowlinks.insert(key.to_string(), *index);
        *index += 1;
        stack.push(key.to_string());
        on_stack.insert(key.to_string());

        let mut neighbors: Vec<String> = graph
            .get(key)
            .into_iter()
            .flat_map(|deps| deps.iter().cloned())
            .collect();
        neighbors.sort();

        for neighbor in neighbors {
            if !indices.contains_key(&neighbor) {
                Self::strong_connect(
                    &neighbor, graph, index, indices, lowlinks, stack, on_stack, sccs,
                );
                let neighbor_low = lowlinks.get(&neighbor).copied().unwrap_or(usize::MAX);
                let current_low = lowlinks.get(key).copied().unwrap_or(usize::MAX);
                lowlinks.insert(key.to_string(), current_low.min(neighbor_low));
            } else if on_stack.contains(&neighbor) {
                let neighbor_index = indices.get(&neighbor).copied().unwrap_or(usize::MAX);
                let current_low = lowlinks.get(key).copied().unwrap_or(usize::MAX);
                lowlinks.insert(key.to_string(), current_low.min(neighbor_index));
            }
        }

        if lowlinks.get(key) == indices.get(key) {
            let mut scc = Vec::new();
            while let Some(member) = stack.pop() {
                on_stack.remove(&member);
                scc.push(member.clone());
                if member == key {
                    break;
                }
            }
            scc.sort();
            sccs.push(scc);
        }
    }

    fn compute_scc_dependencies(
        &self,
        sccs: &[Vec<String>],
        scc_index_by_key: &HashMap<String, usize>,
        direct_callees: &HashMap<String, HashSet<String>>,
    ) -> Vec<HashSet<usize>> {
        let mut deps = vec![HashSet::new(); sccs.len()];
        for (scc_index, scc) in sccs.iter().enumerate() {
            for key in scc {
                for callee in direct_callees
                    .get(key)
                    .into_iter()
                    .flat_map(|set| set.iter())
                {
                    if let Some(dep_scc) = scc_index_by_key.get(callee).copied() {
                        if dep_scc != scc_index {
                            deps[scc_index].insert(dep_scc);
                        }
                    }
                }
            }
        }
        deps
    }

    fn topo_sort_sccs(&self, scc_deps: &[HashSet<usize>]) -> Vec<usize> {
        fn visit(
            node: usize,
            scc_deps: &[HashSet<usize>],
            visiting: &mut HashSet<usize>,
            visited: &mut HashSet<usize>,
            order: &mut Vec<usize>,
        ) {
            if visited.contains(&node) {
                return;
            }
            if !visiting.insert(node) {
                return;
            }
            let mut deps: Vec<usize> = scc_deps[node].iter().copied().collect();
            deps.sort();
            for dep in deps {
                visit(dep, scc_deps, visiting, visited, order);
            }
            visiting.remove(&node);
            visited.insert(node);
            order.push(node);
        }

        let mut order = Vec::new();
        let mut visiting = HashSet::new();
        let mut visited = HashSet::new();
        for node in 0..scc_deps.len() {
            visit(node, scc_deps, &mut visiting, &mut visited, &mut order);
        }
        order
    }

    fn emit_template_module(
        &mut self,
        func: &ast::Function,
        self_key: &str,
        ordered_members: &[String],
        scc_index_by_key: &HashMap<String, usize>,
        body_hashes: &HashMap<String, String>,
        abi_hashes: &HashMap<String, String>,
        artifact_hashes: &HashMap<String, String>,
        direct_callees: &HashSet<String>,
    ) -> (String, Vec<String>) {
        let original_functions = self.functions.clone();
        let mut template_symbols = Vec::new();
        let self_scc_index = scc_index_by_key
            .get(self_key)
            .copied()
            .expect("self SCC index must exist");
        let mut internal_deps: Vec<String> = direct_callees
            .iter()
            .filter(|dep| dep.as_str() != self_key)
            .filter(|dep| scc_index_by_key.get(*dep).copied() == Some(self_scc_index))
            .cloned()
            .collect();
        internal_deps.sort_by(|left, right| {
            let left_body = body_hashes.get(left).cloned().unwrap_or_default();
            let right_body = body_hashes.get(right).cloned().unwrap_or_default();
            let left_abi = abi_hashes.get(left).cloned().unwrap_or_default();
            let right_abi = abi_hashes.get(right).cloned().unwrap_or_default();
            left_body
                .cmp(&right_body)
                .then(left_abi.cmp(&right_abi))
                .then(left.cmp(right))
        });
        let mut external_deps: Vec<(String, String)> = direct_callees
            .iter()
            .filter(|dep| scc_index_by_key.get(*dep).copied() != Some(self_scc_index))
            .filter_map(|dep| {
                artifact_hashes
                    .get(dep)
                    .cloned()
                    .map(|artifact_hash| (dep.clone(), artifact_hash))
            })
            .collect();
        external_deps.sort_by(|(left_key, left_hash), (right_key, right_hash)| {
            let left_body = body_hashes.get(left_key).cloned().unwrap_or_default();
            let right_body = body_hashes.get(right_key).cloned().unwrap_or_default();
            let left_abi = abi_hashes.get(left_key).cloned().unwrap_or_default();
            let right_abi = abi_hashes.get(right_key).cloned().unwrap_or_default();
            left_hash
                .cmp(right_hash)
                .then(left_body.cmp(&right_body))
                .then(left_abi.cmp(&right_abi))
                .then(left_key.cmp(right_key))
        });
        let internal_slots: HashMap<String, usize> = ordered_members
            .iter()
            .enumerate()
            .map(|(slot, key)| (key.clone(), slot))
            .collect();

        for original in &original_functions {
            let key = format!("{}/{}", original.source_name, original.arity);
            let module_name = if key == self_key {
                "__SELF__".to_string()
            } else if internal_deps.iter().any(|dep| dep == &key) {
                let slot = internal_slots
                    .get(&key)
                    .copied()
                    .expect("internal SCC slot must exist");
                format!("__SCC_SLOT_{slot}__")
            } else if let Some(index) = external_deps.iter().position(|(dep, _)| dep == &key) {
                format!("__EXT{index}__")
            } else {
                "__UNUSED__".to_string()
            };
            template_symbols.push(LocalFunctionSymbol {
                source_name: original.source_name.clone(),
                body_hash: original.body_hash.clone(),
                abi_hash: original.abi_hash.clone(),
                artifact_hash: module_name.clone(),
                build_key: original.build_key.clone(),
                module_name,
                arity: original.arity,
            });
        }

        self.functions = template_symbols;
        self.var_counter = 0;
        let core_func = self.translate_function(func, "apply");
        self.functions = original_functions;

        let core_module = CoreModule {
            name: "__MODULE__".to_string(),
            exports: vec![("apply".to_string(), func.params.len())],
            functions: vec![core_func],
        };
        let mut emitter = Emitter::new();
        (
            emitter.emit_module(&core_module),
            external_deps.into_iter().map(|(_, hash)| hash).collect(),
        )
    }

    fn function_fingerprint(
        &self,
        func: &ast::Function,
        current_key: &str,
        local_keys: &HashSet<String>,
    ) -> (String, HashSet<String>) {
        let mut state = FingerprintState::new();
        for param in &func.params {
            state.bind(&param.name);
        }
        let (body_fp, body_callees) =
            self.expr_fingerprint(&func.body, current_key, local_keys, &state);
        let mut fp = format!(
            "fn$arity:{}|params:{}|",
            func.params.len(),
            func.params.len()
        );
        fp.push_str("body:");
        fp.push_str(&body_fp);
        (fp, body_callees)
    }

    fn expr_fingerprint(
        &self,
        expr: &Expr,
        current_key: &str,
        local_keys: &HashSet<String>,
        state: &FingerprintState,
    ) -> (String, HashSet<String>) {
        let mut callees = HashSet::new();
        let fp = match expr {
            Expr::Int(n, _) => format!("int:{n}"),
            Expr::Float(f, _) => format!("float:{f}"),
            Expr::Char(c, _) => format!("char:{c}"),
            Expr::String(s, _) => format!("string:{s}"),
            Expr::InterpolatedString(parts, _) => {
                let mut out = String::from("istr[");
                for part in parts {
                    match part {
                        InterpolatedPart::Literal(s) => {
                            out.push_str("lit:");
                            out.push_str(s);
                            out.push(';');
                        }
                        InterpolatedPart::Expr(e) => {
                            let (e_fp, e_calls) =
                                self.expr_fingerprint(e, current_key, local_keys, state);
                            callees.extend(e_calls);
                            out.push_str("expr:");
                            out.push_str(&e_fp);
                            out.push(';');
                        }
                    }
                }
                out.push(']');
                out
            }
            Expr::Bool(b, _) => format!("bool:{b}"),
            Expr::Atom(a, _) => format!("atom:{a}"),
            Expr::Unit(_) => "unit".to_string(),
            Expr::Var(name, _) => match state.lookup(name) {
                Some(id) => format!("var:{id}"),
                None => format!("global:{name}"),
            },
            Expr::Tuple(items, _) => {
                let mut out = String::from("tuple(");
                for item in items {
                    let (i_fp, i_calls) =
                        self.expr_fingerprint(item, current_key, local_keys, state);
                    callees.extend(i_calls);
                    out.push_str(&i_fp);
                    out.push(',');
                }
                out.push(')');
                out
            }
            Expr::List(items, tail, _) => {
                let mut out = String::from("list(");
                for item in items {
                    let (i_fp, i_calls) =
                        self.expr_fingerprint(item, current_key, local_keys, state);
                    callees.extend(i_calls);
                    out.push_str(&i_fp);
                    out.push(',');
                }
                if let Some(t) = tail {
                    let (t_fp, t_calls) = self.expr_fingerprint(t, current_key, local_keys, state);
                    callees.extend(t_calls);
                    out.push('|');
                    out.push_str(&t_fp);
                }
                out.push(')');
                out
            }
            Expr::ListComp {
                expr,
                generators,
                filters,
                ..
            } => {
                let mut out = String::from("listcomp(");
                let (e_fp, e_calls) = self.expr_fingerprint(expr, current_key, local_keys, state);
                callees.extend(e_calls);
                out.push_str(&e_fp);
                let mut comp_state = state.clone();
                for generator_item in generators {
                    let (s_fp, s_calls) = self.expr_fingerprint(
                        &generator_item.source,
                        current_key,
                        local_keys,
                        &comp_state,
                    );
                    callees.extend(s_calls);
                    out.push_str("|gen:");
                    out.push_str(&s_fp);
                    let pattern_fp =
                        self.pattern_fingerprint(&generator_item.pattern, &mut comp_state);
                    out.push('|');
                    out.push_str(&pattern_fp);
                }
                for filter in filters {
                    let (f_fp, f_calls) =
                        self.expr_fingerprint(filter, current_key, local_keys, &comp_state);
                    callees.extend(f_calls);
                    out.push_str("|filter:");
                    out.push_str(&f_fp);
                }
                out.push(')');
                out
            }
            Expr::Range(a, b, inc, _) => {
                let (a_fp, a_calls) = self.expr_fingerprint(a, current_key, local_keys, state);
                let (b_fp, b_calls) = self.expr_fingerprint(b, current_key, local_keys, state);
                callees.extend(a_calls);
                callees.extend(b_calls);
                format!("range:{inc}:{a_fp}:{b_fp}")
            }
            Expr::Map(entries, _) => {
                let mut out = String::from("map(");
                for (k, v) in entries {
                    let (k_fp, k_calls) = self.expr_fingerprint(k, current_key, local_keys, state);
                    let (v_fp, v_calls) = self.expr_fingerprint(v, current_key, local_keys, state);
                    callees.extend(k_calls);
                    callees.extend(v_calls);
                    out.push_str(&k_fp);
                    out.push_str("=>");
                    out.push_str(&v_fp);
                    out.push(',');
                }
                out.push(')');
                out
            }
            Expr::Record(fields, _) => {
                let mut out = String::from("record(");
                for (name, value) in fields {
                    let (v_fp, v_calls) =
                        self.expr_fingerprint(value, current_key, local_keys, state);
                    callees.extend(v_calls);
                    out.push_str(name);
                    out.push(':');
                    out.push_str(&v_fp);
                    out.push(',');
                }
                out.push(')');
                out
            }
            Expr::StructInit(name, fields, _) => {
                let mut out = format!("struct:{name}(");
                for (field, value) in fields {
                    let (v_fp, v_calls) =
                        self.expr_fingerprint(value, current_key, local_keys, state);
                    callees.extend(v_calls);
                    out.push_str(field);
                    out.push(':');
                    out.push_str(&v_fp);
                    out.push(',');
                }
                out.push(')');
                out
            }
            Expr::BitString(items, _) => {
                let mut out = String::from("bits(");
                for item in items {
                    let (i_fp, i_calls) =
                        self.expr_fingerprint(&item.value, current_key, local_keys, state);
                    callees.extend(i_calls);
                    out.push_str(&i_fp);
                    if let Some(size) = item.size {
                        out.push(':');
                        out.push_str(&size.to_string());
                    }
                    out.push('/');
                    out.push_str(match item.specifier {
                        ast::BinarySegmentSpecifier::Integer => "int",
                        ast::BinarySegmentSpecifier::BigInteger => "big",
                        ast::BinarySegmentSpecifier::Binary => "binary",
                        ast::BinarySegmentSpecifier::Utf8 => "utf8",
                    });
                    out.push(',');
                }
                out.push(')');
                out
            }
            Expr::Binary(left, op, right, _) => {
                let (l_fp, l_calls) = self.expr_fingerprint(left, current_key, local_keys, state);
                let (r_fp, r_calls) = self.expr_fingerprint(right, current_key, local_keys, state);
                callees.extend(l_calls);
                callees.extend(r_calls);
                if matches!(op, ast::BinOp::Pipe)
                    && matches!(right.as_ref(), Expr::Var(_, _))
                    && let Expr::Var(name, _) = right.as_ref()
                {
                    let key = format!("{name}/1");
                    if local_keys.contains(&key) {
                        callees.insert(key.clone());
                        if key == current_key {
                            return (format!("pipe_self:1:{l_fp}"), callees);
                        }
                        return (format!("pipe_dep:1:{l_fp}"), callees);
                    }
                }
                format!("bin:{op:?}:{l_fp}:{r_fp}")
            }
            Expr::Unary(op, inner, _) => {
                let (i_fp, i_calls) = self.expr_fingerprint(inner, current_key, local_keys, state);
                callees.extend(i_calls);
                format!("un:{op:?}:{i_fp}")
            }
            Expr::If(c, t, e, _) => {
                let (c_fp, c_calls) = self.expr_fingerprint(c, current_key, local_keys, state);
                let (t_fp, t_calls) = self.expr_fingerprint(t, current_key, local_keys, state);
                callees.extend(c_calls);
                callees.extend(t_calls);
                let e_fp = if let Some(e) = e {
                    let (e_fp, e_calls) = self.expr_fingerprint(e, current_key, local_keys, state);
                    callees.extend(e_calls);
                    e_fp
                } else {
                    "none".to_string()
                };
                format!("if:{c_fp}:{t_fp}:{e_fp}")
            }
            Expr::Cond(arms, _) => {
                let mut out = String::from("cond(");
                for (c, b) in arms {
                    let (c_fp, c_calls) = self.expr_fingerprint(c, current_key, local_keys, state);
                    let (b_fp, b_calls) = self.expr_fingerprint(b, current_key, local_keys, state);
                    callees.extend(c_calls);
                    callees.extend(b_calls);
                    out.push_str(&c_fp);
                    out.push_str("=>");
                    out.push_str(&b_fp);
                    out.push(',');
                }
                out.push(')');
                out
            }
            Expr::Match(scrut, arms, _) => {
                let (s_fp, s_calls) = self.expr_fingerprint(scrut, current_key, local_keys, state);
                callees.extend(s_calls);
                let mut out = format!("match:{s_fp}(");
                for arm in arms {
                    let mut arm_state = state.clone();
                    let pattern_fp = self.pattern_fingerprint(&arm.pattern, &mut arm_state);
                    let guard_fp = if let Some(guard) = &arm.guard {
                        let (guard_fp, guard_calls) =
                            self.expr_fingerprint(guard, current_key, local_keys, &arm_state);
                        callees.extend(guard_calls);
                        guard_fp
                    } else {
                        "none".to_string()
                    };
                    let (b_fp, b_calls) =
                        self.expr_fingerprint(&arm.body, current_key, local_keys, &arm_state);
                    callees.extend(b_calls);
                    out.push_str(&pattern_fp);
                    out.push(':');
                    out.push_str(&guard_fp);
                    out.push(':');
                    out.push_str(&b_fp);
                    out.push(',');
                }
                out.push(')');
                out
            }
            Expr::Block(stmts, final_expr, _) => {
                let mut out = String::from("block(");
                let mut block_state = state.clone();
                for stmt in stmts {
                    match stmt {
                        Stmt::Let(pattern, _, init, _) => {
                            let (i_fp, i_calls) =
                                self.expr_fingerprint(init, current_key, local_keys, &block_state);
                            callees.extend(i_calls);
                            let pattern_fp = self.pattern_fingerprint(pattern, &mut block_state);
                            out.push_str("let:");
                            out.push_str(&pattern_fp);
                            out.push('=');
                            out.push_str(&i_fp);
                            out.push(';');
                        }
                        Stmt::Expr(e) => {
                            let (e_fp, e_calls) =
                                self.expr_fingerprint(e, current_key, local_keys, &block_state);
                            callees.extend(e_calls);
                            out.push_str("expr:");
                            out.push_str(&e_fp);
                            out.push(';');
                        }
                    }
                }
                if let Some(final_expr) = final_expr {
                    let (f_fp, f_calls) =
                        self.expr_fingerprint(final_expr, current_key, local_keys, &block_state);
                    callees.extend(f_calls);
                    out.push_str("final:");
                    out.push_str(&f_fp);
                }
                out.push(')');
                out
            }
            Expr::Call(func, args, _) => {
                let maybe_local = match func.as_ref() {
                    Expr::Var(name, _) => {
                        let key = format!("{name}/{}", args.len());
                        if local_keys.contains(&key) && state.lookup(name).is_none() {
                            Some(key)
                        } else {
                            None
                        }
                    }
                    _ => None,
                };

                let mut out = String::from("call(");
                match maybe_local {
                    Some(ref key) => {
                        callees.insert(key.clone());
                        if key == current_key {
                            out.push_str(&format!("self/{}", args.len()));
                        } else {
                            out.push_str(&format!("dep/{}", args.len()));
                        }
                    }
                    None => {
                        let (f_fp, f_calls) =
                            self.expr_fingerprint(func, current_key, local_keys, state);
                        callees.extend(f_calls);
                        out.push_str(&f_fp);
                    }
                }
                out.push(';');
                for arg in args {
                    let (a_fp, a_calls) =
                        self.expr_fingerprint(arg, current_key, local_keys, state);
                    callees.extend(a_calls);
                    out.push_str(&a_fp);
                    out.push(',');
                }
                out.push(')');
                out
            }
            Expr::MethodCall(obj, method, args, _) => {
                let (o_fp, o_calls) = self.expr_fingerprint(obj, current_key, local_keys, state);
                callees.extend(o_calls);
                let mut out = format!("mcall:{method}({o_fp};");
                for arg in args {
                    let (a_fp, a_calls) =
                        self.expr_fingerprint(arg, current_key, local_keys, state);
                    callees.extend(a_calls);
                    out.push_str(&a_fp);
                    out.push(',');
                }
                out.push(')');
                out
            }
            Expr::Lambda(params, _, body, _) => {
                let mut lambda_state = state.clone();
                lambda_state.push_scope();
                for param in params {
                    lambda_state.bind(&param.name);
                }
                let (b_fp, b_calls) =
                    self.expr_fingerprint(body, current_key, local_keys, &lambda_state);
                callees.extend(b_calls);
                format!("lambda:{}:{b_fp}", params.len())
            }
            Expr::Field(obj, field, _) => {
                let (o_fp, o_calls) = self.expr_fingerprint(obj, current_key, local_keys, state);
                callees.extend(o_calls);
                format!("field:{field}:{o_fp}")
            }
            Expr::Index(container, key, _) => {
                let (c_fp, c_calls) =
                    self.expr_fingerprint(container, current_key, local_keys, state);
                let (k_fp, k_calls) = self.expr_fingerprint(key, current_key, local_keys, state);
                callees.extend(c_calls);
                callees.extend(k_calls);
                format!("index:{c_fp}:{k_fp}")
            }
            Expr::Path(parts, _) => format!("path:{}", parts.join("::")),
            Expr::Spawn(e, _) => {
                let (e_fp, e_calls) = self.expr_fingerprint(e, current_key, local_keys, state);
                callees.extend(e_calls);
                format!("spawn:{e_fp}")
            }
            Expr::Send(a, b, _) => {
                let (a_fp, a_calls) = self.expr_fingerprint(a, current_key, local_keys, state);
                let (b_fp, b_calls) = self.expr_fingerprint(b, current_key, local_keys, state);
                callees.extend(a_calls);
                callees.extend(b_calls);
                format!("send:{a_fp}:{b_fp}")
            }
            Expr::Receive { arms, timeout, .. } => {
                let mut out = String::from("recv(");
                for arm in arms {
                    let mut arm_state = state.clone();
                    let pattern_fp = self.pattern_fingerprint(&arm.pattern, &mut arm_state);
                    let guard_fp = if let Some(guard) = &arm.guard {
                        let (guard_fp, guard_calls) =
                            self.expr_fingerprint(guard, current_key, local_keys, &arm_state);
                        callees.extend(guard_calls);
                        guard_fp
                    } else {
                        "none".to_string()
                    };
                    let (b_fp, b_calls) =
                        self.expr_fingerprint(&arm.body, current_key, local_keys, &arm_state);
                    callees.extend(b_calls);
                    out.push_str(&pattern_fp);
                    out.push(':');
                    out.push_str(&guard_fp);
                    out.push(':');
                    out.push_str(&b_fp);
                    out.push(',');
                }
                if let Some((ms, body)) = timeout {
                    let (ms_fp, ms_calls) =
                        self.expr_fingerprint(ms, current_key, local_keys, state);
                    let (b_fp, b_calls) =
                        self.expr_fingerprint(body, current_key, local_keys, state);
                    callees.extend(ms_calls);
                    callees.extend(b_calls);
                    out.push_str("timeout:");
                    out.push_str(&ms_fp);
                    out.push(':');
                    out.push_str(&b_fp);
                }
                out.push(')');
                out
            }
            Expr::SelfPid(_) => "self".to_string(),
            Expr::Return(e, _) => {
                if let Some(e) = e {
                    let (e_fp, e_calls) = self.expr_fingerprint(e, current_key, local_keys, state);
                    callees.extend(e_calls);
                    format!("ret:{e_fp}")
                } else {
                    "ret:none".to_string()
                }
            }
            Expr::Try {
                body, catch_arms, ..
            } => {
                let (b_fp, b_calls) = self.expr_fingerprint(body, current_key, local_keys, state);
                callees.extend(b_calls);
                let mut out = format!("try:{b_fp}(");
                for arm in catch_arms {
                    let mut arm_state = state.clone();
                    let pattern_fp = self.pattern_fingerprint(&arm.pattern, &mut arm_state);
                    let (a_fp, a_calls) =
                        self.expr_fingerprint(&arm.body, current_key, local_keys, &arm_state);
                    callees.extend(a_calls);
                    out.push_str(arm.class.as_deref().unwrap_or("_"));
                    out.push(':');
                    out.push_str(&pattern_fp);
                    out.push(':');
                    out.push_str(&a_fp);
                    out.push(',');
                }
                out.push(')');
                out
            }
        };

        (fp, callees)
    }

    fn pattern_fingerprint(&self, pattern: &Pattern, state: &mut FingerprintState) -> String {
        match pattern {
            Pattern::Wildcard(_) => "_".to_string(),
            Pattern::Var(name, _) => format!("bind:{}", state.bind(name)),
            Pattern::Int(n, _) => format!("int:{n}"),
            Pattern::Float(f, _) => format!("float:{f}"),
            Pattern::Char(c, _) => format!("char:{c}"),
            Pattern::String(s, _) => format!("string:{s}"),
            Pattern::Bool(b, _) => format!("bool:{b}"),
            Pattern::Atom(a, _) => format!("atom:{a}"),
            Pattern::Tuple(items, _) => {
                let mut out = String::from("tuple(");
                for item in items {
                    out.push_str(&self.pattern_fingerprint(item, state));
                    out.push(',');
                }
                out.push(')');
                out
            }
            Pattern::List(items, tail, _) => {
                let mut out = String::from("list(");
                for item in items {
                    out.push_str(&self.pattern_fingerprint(item, state));
                    out.push(',');
                }
                if let Some(tail) = tail {
                    out.push('|');
                    out.push_str(&self.pattern_fingerprint(tail, state));
                }
                out.push(')');
                out
            }
            Pattern::Constructor(path, fields, _) => {
                let mut out = format!("ctor:{}(", path.join("::"));
                for field in fields {
                    out.push_str(&self.pattern_fingerprint(field, state));
                    out.push(',');
                }
                out.push(')');
                out
            }
            Pattern::Record(fields, _) => {
                let mut out = String::from("record(");
                for (name, pat) in fields {
                    out.push_str(name);
                    out.push(':');
                    out.push_str(&self.pattern_fingerprint(pat, state));
                    out.push(',');
                }
                out.push(')');
                out
            }
            Pattern::BitString(segments, _) => {
                let mut out = String::from("bits(");
                for segment in segments {
                    out.push_str(&self.pattern_fingerprint(&segment.value, state));
                    if let Some(size) = segment.size {
                        out.push(':');
                        out.push_str(&size.to_string());
                    }
                    out.push('/');
                    out.push_str(match segment.specifier {
                        ast::BinarySegmentSpecifier::Integer => "int",
                        ast::BinarySegmentSpecifier::BigInteger => "big",
                        ast::BinarySegmentSpecifier::Binary => "binary",
                        ast::BinarySegmentSpecifier::Utf8 => "utf8",
                    });
                    out.push(',');
                }
                out.push(')');
                out
            }
            Pattern::Or(left, right, _) => {
                let mut left_state = state.clone();
                let left_fp = self.pattern_fingerprint(left, &mut left_state);
                let mut right_state = state.clone();
                let right_fp = self.pattern_fingerprint(right, &mut right_state);
                *state = left_state;
                format!("or:{left_fp}:{right_fp}")
            }
        }
    }

    fn lookup_function_symbol(&self, name: &str, arity: usize) -> Option<&LocalFunctionSymbol> {
        self.functions
            .iter()
            .find(|symbol| symbol.source_name == name && symbol.arity == arity)
    }

    fn translate_function(&mut self, func: &ast::Function, addressed_name: &str) -> CoreFunDef {
        // Generate parameter names
        let params: Vec<String> = func
            .params
            .iter()
            .map(|p| self.to_core_var(&p.name))
            .collect();

        let body = self.translate_expr(&func.body);

        self.canonicalize_core_function(CoreFunDef {
            name: addressed_name.to_string(),
            arity: func.params.len(),
            params,
            body,
        })
    }

    fn canonicalize_core_function(&self, func: CoreFunDef) -> CoreFunDef {
        let mut next_var_id = 0usize;
        let mut env = HashMap::new();
        let params = func
            .params
            .into_iter()
            .map(|param| {
                let canonical = Self::fresh_core_var(&mut next_var_id);
                env.insert(param, canonical.clone());
                canonical
            })
            .collect();
        let body = Self::canonicalize_core_expr(func.body, &mut next_var_id, &env);

        CoreFunDef {
            name: func.name,
            arity: func.arity,
            params,
            body,
        }
    }

    fn fresh_core_var(next_var_id: &mut usize) -> String {
        let id = *next_var_id;
        *next_var_id += 1;
        format!("V{id}")
    }

    fn canonicalize_core_expr(
        expr: CoreExpr,
        next_var_id: &mut usize,
        env: &HashMap<String, String>,
    ) -> CoreExpr {
        match expr {
            CoreExpr::Lit(_) | CoreExpr::LocalFunRef(_, _) | CoreExpr::RemoteFunRef(_, _, _) => {
                expr
            }
            CoreExpr::Var(name) => CoreExpr::Var(env.get(&name).cloned().unwrap_or(name)),
            CoreExpr::Tuple(items) => CoreExpr::Tuple(
                items
                    .into_iter()
                    .map(|item| Self::canonicalize_core_expr(item, next_var_id, env))
                    .collect(),
            ),
            CoreExpr::List(items, tail) => CoreExpr::List(
                items
                    .into_iter()
                    .map(|item| Self::canonicalize_core_expr(item, next_var_id, env))
                    .collect(),
                Box::new(Self::canonicalize_core_expr(*tail, next_var_id, env)),
            ),
            CoreExpr::Cons(head, tail) => CoreExpr::Cons(
                Box::new(Self::canonicalize_core_expr(*head, next_var_id, env)),
                Box::new(Self::canonicalize_core_expr(*tail, next_var_id, env)),
            ),
            CoreExpr::Map(entries) => CoreExpr::Map(
                entries
                    .into_iter()
                    .map(|(key, value)| {
                        (
                            Self::canonicalize_core_expr(key, next_var_id, env),
                            Self::canonicalize_core_expr(value, next_var_id, env),
                        )
                    })
                    .collect(),
            ),
            CoreExpr::Binary(items) => CoreExpr::Binary(
                items
                    .into_iter()
                    .map(|item| CoreBinarySegment {
                        value: Self::canonicalize_core_expr(item.value, next_var_id, env),
                        size: item.size,
                        kind: item.kind,
                    })
                    .collect(),
            ),
            CoreExpr::Apply(func, args) => CoreExpr::Apply(
                Box::new(Self::canonicalize_core_expr(*func, next_var_id, env)),
                args.into_iter()
                    .map(|arg| Self::canonicalize_core_expr(arg, next_var_id, env))
                    .collect(),
            ),
            CoreExpr::Call(module, func, args) => CoreExpr::Call(
                module,
                func,
                args.into_iter()
                    .map(|arg| Self::canonicalize_core_expr(arg, next_var_id, env))
                    .collect(),
            ),
            CoreExpr::Let(bindings, body) => {
                let values: Vec<CoreExpr> = bindings
                    .iter()
                    .map(|(_, value)| Self::canonicalize_core_expr(value.clone(), next_var_id, env))
                    .collect();
                let mut body_env = env.clone();
                let canonical_names: Vec<String> = bindings
                    .into_iter()
                    .map(|(name, _)| {
                        let canonical = Self::fresh_core_var(next_var_id);
                        body_env.insert(name, canonical.clone());
                        canonical
                    })
                    .collect();
                CoreExpr::Let(
                    canonical_names.into_iter().zip(values).collect(),
                    Box::new(Self::canonicalize_core_expr(*body, next_var_id, &body_env)),
                )
            }
            CoreExpr::Case(scrutinee, clauses) => CoreExpr::Case(
                Box::new(Self::canonicalize_core_expr(*scrutinee, next_var_id, env)),
                clauses
                    .into_iter()
                    .map(|clause| Self::canonicalize_core_clause(clause, next_var_id, env))
                    .collect(),
            ),
            CoreExpr::Receive { clauses, timeout } => CoreExpr::Receive {
                clauses: clauses
                    .into_iter()
                    .map(|clause| Self::canonicalize_core_clause(clause, next_var_id, env))
                    .collect(),
                timeout: timeout.map(|(ms, body)| {
                    (
                        Box::new(Self::canonicalize_core_expr(*ms, next_var_id, env)),
                        Box::new(Self::canonicalize_core_expr(*body, next_var_id, env)),
                    )
                }),
            },
            CoreExpr::Fun(params, body) => {
                let mut lambda_env = env.clone();
                let canonical_params: Vec<String> = params
                    .into_iter()
                    .map(|param| {
                        let canonical = Self::fresh_core_var(next_var_id);
                        lambda_env.insert(param, canonical.clone());
                        canonical
                    })
                    .collect();
                CoreExpr::Fun(
                    canonical_params,
                    Box::new(Self::canonicalize_core_expr(
                        *body,
                        next_var_id,
                        &lambda_env,
                    )),
                )
            }
            CoreExpr::Primop(name, args) => CoreExpr::Primop(
                name,
                args.into_iter()
                    .map(|arg| Self::canonicalize_core_expr(arg, next_var_id, env))
                    .collect(),
            ),
            CoreExpr::Seq(first, second) => CoreExpr::Seq(
                Box::new(Self::canonicalize_core_expr(*first, next_var_id, env)),
                Box::new(Self::canonicalize_core_expr(*second, next_var_id, env)),
            ),
            CoreExpr::Try {
                body,
                vars,
                handler,
                evars,
                catch,
            } => {
                let body = Box::new(Self::canonicalize_core_expr(*body, next_var_id, env));

                let mut handler_env = env.clone();
                let vars: Vec<String> = vars
                    .into_iter()
                    .map(|var| {
                        let canonical = Self::fresh_core_var(next_var_id);
                        handler_env.insert(var, canonical.clone());
                        canonical
                    })
                    .collect();
                let handler = Box::new(Self::canonicalize_core_expr(
                    *handler,
                    next_var_id,
                    &handler_env,
                ));

                let mut catch_env = env.clone();
                let evars: Vec<String> = evars
                    .into_iter()
                    .map(|var| {
                        let canonical = Self::fresh_core_var(next_var_id);
                        catch_env.insert(var, canonical.clone());
                        canonical
                    })
                    .collect();
                let catch = Box::new(Self::canonicalize_core_expr(
                    *catch,
                    next_var_id,
                    &catch_env,
                ));

                CoreExpr::Try {
                    body,
                    vars,
                    handler,
                    evars,
                    catch,
                }
            }
        }
    }

    fn canonicalize_core_clause(
        clause: CoreClause,
        next_var_id: &mut usize,
        env: &HashMap<String, String>,
    ) -> CoreClause {
        let mut clause_env = env.clone();
        let mut pattern_bindings = HashMap::new();
        let patterns = clause
            .patterns
            .into_iter()
            .map(|pattern| {
                Self::canonicalize_core_pattern(
                    pattern,
                    next_var_id,
                    &mut clause_env,
                    &mut pattern_bindings,
                )
            })
            .collect();
        let guard = Self::canonicalize_core_expr(clause.guard, next_var_id, &clause_env);
        let body = Self::canonicalize_core_expr(clause.body, next_var_id, &clause_env);

        CoreClause {
            patterns,
            guard,
            body,
        }
    }

    fn canonicalize_core_pattern(
        pattern: CorePattern,
        next_var_id: &mut usize,
        env: &mut HashMap<String, String>,
        pattern_bindings: &mut HashMap<String, String>,
    ) -> CorePattern {
        match pattern {
            CorePattern::Lit(_) | CorePattern::Nil => pattern,
            CorePattern::Var(name) => {
                if name == "_" {
                    CorePattern::Var(name)
                } else if let Some(existing) = pattern_bindings.get(&name).cloned() {
                    CorePattern::Var(existing)
                } else {
                    let canonical = Self::fresh_core_var(next_var_id);
                    pattern_bindings.insert(name.clone(), canonical.clone());
                    env.insert(name, canonical.clone());
                    CorePattern::Var(canonical)
                }
            }
            CorePattern::Tuple(items) => CorePattern::Tuple(
                items
                    .into_iter()
                    .map(|item| {
                        Self::canonicalize_core_pattern(item, next_var_id, env, pattern_bindings)
                    })
                    .collect(),
            ),
            CorePattern::Cons(head, tail) => CorePattern::Cons(
                Box::new(Self::canonicalize_core_pattern(
                    *head,
                    next_var_id,
                    env,
                    pattern_bindings,
                )),
                Box::new(Self::canonicalize_core_pattern(
                    *tail,
                    next_var_id,
                    env,
                    pattern_bindings,
                )),
            ),
            CorePattern::Binary(segments) => CorePattern::Binary(
                segments
                    .into_iter()
                    .map(|segment| CoreBinaryPatternSegment {
                        pattern: Self::canonicalize_core_pattern(
                            segment.pattern,
                            next_var_id,
                            env,
                            pattern_bindings,
                        ),
                        size: segment.size,
                        kind: segment.kind,
                    })
                    .collect(),
            ),
            CorePattern::Alias(name, pattern) => {
                let canonical = if let Some(existing) = pattern_bindings.get(&name).cloned() {
                    existing
                } else {
                    let canonical = Self::fresh_core_var(next_var_id);
                    pattern_bindings.insert(name.clone(), canonical.clone());
                    env.insert(name, canonical.clone());
                    canonical
                };
                CorePattern::Alias(
                    canonical,
                    Box::new(Self::canonicalize_core_pattern(
                        *pattern,
                        next_var_id,
                        env,
                        pattern_bindings,
                    )),
                )
            }
        }
    }

    fn translate_expr(&mut self, expr: &Expr) -> CoreExpr {
        match expr {
            Expr::Int(n, _) => CoreExpr::Lit(CoreLit::Int(*n)),
            Expr::Float(f, _) => CoreExpr::Lit(CoreLit::Float(*f)),
            Expr::Char(c, _) => CoreExpr::Lit(CoreLit::Int(*c as i64)), // Chars are integers in Erlang
            Expr::String(s, _) => self.string_literal_expr(s),
            Expr::InterpolatedString(parts, _) => {
                // Build format string and arguments for io_lib:format
                let mut format_str = String::new();
                let mut args = Vec::new();

                for part in parts {
                    match part {
                        InterpolatedPart::Literal(s) => {
                            // Escape ~ characters in literals for io_lib:format
                            format_str.push_str(&s.replace('~', "~~"));
                        }
                        InterpolatedPart::Expr(e) => {
                            format_str.push_str("~p");
                            args.push(self.translate_expr(e));
                        }
                    }
                }

                // io_lib:format returns an iolist, convert it to a binary string.
                let format_expr = CoreExpr::Lit(CoreLit::String(format_str));
                let args_list = self.build_list(args);
                let io_list = CoreExpr::Call(
                    "io_lib".into(),
                    "format".into(),
                    vec![format_expr, args_list],
                );
                CoreExpr::Call("erlang".into(), "iolist_to_binary".into(), vec![io_list])
            }
            Expr::Bool(b, _) => {
                CoreExpr::Lit(CoreLit::Atom(if *b { "true" } else { "false" }.into()))
            }
            Expr::Atom(a, _) => CoreExpr::Lit(CoreLit::Atom(a.clone())),
            Expr::Unit(_) => CoreExpr::Lit(CoreLit::Atom("ok".into())),

            Expr::Var(name, _) => CoreExpr::Var(self.to_core_var(name)),

            Expr::Tuple(exprs, _) => {
                let elems: Vec<CoreExpr> = exprs.iter().map(|e| self.translate_expr(e)).collect();
                CoreExpr::Tuple(elems)
            }

            Expr::List(exprs, tail, _) => {
                let elems: Vec<CoreExpr> = exprs.iter().map(|e| self.translate_expr(e)).collect();
                let tail_expr = tail
                    .as_ref()
                    .map(|t| self.translate_expr(t))
                    .unwrap_or(CoreExpr::Lit(CoreLit::Nil));

                // Build list from end
                elems.into_iter().rev().fold(tail_expr, |acc, elem| {
                    CoreExpr::Cons(Box::new(elem), Box::new(acc))
                })
            }

            Expr::Map(entries, _) => {
                let core_entries: Vec<(CoreExpr, CoreExpr)> = entries
                    .iter()
                    .map(|(k, v)| (self.translate_expr(k), self.translate_expr(v)))
                    .collect();
                CoreExpr::Map(core_entries)
            }

            Expr::Range(start, end, inclusive, _) => {
                let start_expr = self.translate_expr(start);
                let end_expr = if *inclusive {
                    self.translate_expr(end)
                } else {
                    // For exclusive range, end - 1
                    CoreExpr::Call(
                        "erlang".into(),
                        "-".into(),
                        vec![self.translate_expr(end), CoreExpr::Lit(CoreLit::Int(1))],
                    )
                };
                CoreExpr::Call("lists".into(), "seq".into(), vec![start_expr, end_expr])
            }

            Expr::ListComp {
                expr,
                generators,
                filters,
                ..
            } => self.translate_list_comp(expr, generators, filters),

            Expr::Binary(left, op, right, _) => {
                let l = self.translate_expr(left);
                let r = self.translate_expr(right);

                if matches!(op, ast::BinOp::Concat) {
                    let left_tmp = self.fresh_var();
                    let right_tmp = self.fresh_var();
                    let left_var = CoreExpr::Var(left_tmp.clone());
                    let right_var = CoreExpr::Var(right_tmp.clone());
                    let is_binary =
                        CoreExpr::Call("erlang".into(), "is_binary".into(), vec![left_var.clone()]);
                    let binary_concat = CoreExpr::Binary(vec![
                        CoreBinarySegment {
                            value: left_var.clone(),
                            size: CoreBinarySize::All,
                            kind: CoreBinaryKind::Binary,
                        },
                        CoreBinarySegment {
                            value: right_var.clone(),
                            size: CoreBinarySize::All,
                            kind: CoreBinaryKind::Binary,
                        },
                    ]);
                    let list_concat = CoreExpr::Call(
                        "erlang".into(),
                        "++".into(),
                        vec![left_var.clone(), right_var.clone()],
                    );

                    return CoreExpr::Let(
                        vec![(left_tmp, l), (right_tmp, r)],
                        Box::new(CoreExpr::Case(
                            Box::new(is_binary),
                            vec![
                                CoreClause {
                                    patterns: vec![CorePattern::Lit(CoreLit::Atom("true".into()))],
                                    guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                    body: binary_concat,
                                },
                                CoreClause {
                                    patterns: vec![CorePattern::Lit(CoreLit::Atom("false".into()))],
                                    guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                    body: list_concat,
                                },
                            ],
                        )),
                    );
                }

                let (module, func) = match op {
                    ast::BinOp::Add => ("erlang", "+"),
                    ast::BinOp::Sub => ("erlang", "-"),
                    ast::BinOp::Mul => ("erlang", "*"),
                    ast::BinOp::Div => ("erlang", "div"),
                    ast::BinOp::Mod => ("erlang", "rem"),
                    ast::BinOp::Eq => ("erlang", "=:="),
                    ast::BinOp::NotEq => ("erlang", "=/="),
                    ast::BinOp::Lt => ("erlang", "<"),
                    ast::BinOp::LtEq => ("erlang", "=<"),
                    ast::BinOp::Gt => ("erlang", ">"),
                    ast::BinOp::GtEq => ("erlang", ">="),
                    ast::BinOp::And => ("erlang", "and"),
                    ast::BinOp::Or => ("erlang", "or"),
                    ast::BinOp::Concat => unreachable!(),
                    ast::BinOp::Pipe => {
                        // x |> f becomes f(x)
                        // If right side is a function name, use LocalFunRef or Call
                        if let Expr::Var(name, _) = right.as_ref() {
                            // Check if it's a built-in function
                            if matches!(
                                name.as_str(),
                                "length" | "hd" | "tl" | "reverse" | "sort" | "flatten" | "abs"
                            ) {
                                let (module, func) = match name.as_str() {
                                    "length" | "hd" | "tl" | "abs" => ("erlang", name.as_str()),
                                    _ => ("lists", name.as_str()),
                                };
                                return CoreExpr::Call(module.into(), func.into(), vec![l]);
                            } else if let Some(symbol) = self.lookup_function_symbol(name, 1) {
                                return CoreExpr::Call(
                                    symbol.module_name.clone(),
                                    "apply".to_string(),
                                    vec![l],
                                );
                            } else if let Some(hash) = self.resolved_symbol_hash(name, 1) {
                                return CoreExpr::Call(hash.clone(), "apply".into(), vec![l]);
                            }
                        }
                        return CoreExpr::Apply(Box::new(r), vec![l]);
                    }
                };

                CoreExpr::Call(module.into(), func.into(), vec![l, r])
            }

            Expr::Unary(op, inner, _) => {
                let e = self.translate_expr(inner);
                match op {
                    ast::UnaryOp::Neg => CoreExpr::Call("erlang".into(), "-".into(), vec![e]),
                    ast::UnaryOp::Not => CoreExpr::Call("erlang".into(), "not".into(), vec![e]),
                }
            }

            Expr::Cond(arms, _) => {
                // Translate cond to nested if-else
                // cond { c1 => b1, c2 => b2, _ => b3 }
                // becomes: case c1 of true -> b1; false -> case c2 of true -> b2; false -> b3
                if arms.is_empty() {
                    return CoreExpr::Lit(CoreLit::Atom("ok".into()));
                }

                // Build from the end
                let mut result = CoreExpr::Lit(CoreLit::Atom("nil".into())); // fallback
                for (cond, body) in arms.iter().rev() {
                    let cond_expr = self.translate_expr(cond);
                    let body_expr = self.translate_expr(body);

                    // Check if condition is a wildcard pattern (variable named "_" or literal true)
                    let is_else = match cond {
                        Expr::Var(name, _) if name == "_" => true,
                        Expr::Bool(true, _) => true,
                        _ => false,
                    };

                    if is_else {
                        result = body_expr;
                    } else {
                        result = CoreExpr::Case(
                            Box::new(cond_expr),
                            vec![
                                CoreClause {
                                    patterns: vec![CorePattern::Lit(CoreLit::Atom("true".into()))],
                                    guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                    body: body_expr,
                                },
                                CoreClause {
                                    patterns: vec![CorePattern::Lit(CoreLit::Atom("false".into()))],
                                    guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                    body: result,
                                },
                            ],
                        );
                    }
                }
                result
            }

            Expr::If(cond, then_branch, else_branch, _) => {
                let cond_expr = self.translate_expr(cond);
                let then_expr = self.translate_expr(then_branch);
                let else_expr = else_branch
                    .as_ref()
                    .map(|e| self.translate_expr(e))
                    .unwrap_or(CoreExpr::Lit(CoreLit::Atom("ok".into())));

                CoreExpr::Case(
                    Box::new(cond_expr),
                    vec![
                        CoreClause {
                            patterns: vec![CorePattern::Lit(CoreLit::Atom("true".into()))],
                            guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                            body: then_expr,
                        },
                        CoreClause {
                            patterns: vec![CorePattern::Lit(CoreLit::Atom("false".into()))],
                            guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                            body: else_expr,
                        },
                    ],
                )
            }

            Expr::Match(scrutinee, arms, _) => {
                let scrut = self.translate_expr(scrutinee);
                let clauses: Vec<CoreClause> = arms
                    .iter()
                    .map(|arm| {
                        let pattern = self.translate_pattern(&arm.pattern);
                        let guard = arm
                            .guard
                            .as_ref()
                            .map(|g| self.translate_expr(g))
                            .unwrap_or(CoreExpr::Lit(CoreLit::Atom("true".into())));
                        let body = self.translate_expr(&arm.body);
                        CoreClause {
                            patterns: vec![pattern],
                            guard,
                            body,
                        }
                    })
                    .collect();

                CoreExpr::Case(Box::new(scrut), clauses)
            }

            Expr::Block(stmts, final_expr, _) => self.translate_block(stmts, final_expr.as_deref()),

            Expr::Call(func, args, _) => {
                let arg_exprs: Vec<CoreExpr> =
                    args.iter().map(|a| self.translate_expr(a)).collect();

                // Check if it's a direct function call (Var) or path
                match func.as_ref() {
                    Expr::Var(name, _) => {
                        // Check for built-in functions first
                        if name == "print" {
                            // print(x) -> io:format("~p~n", [x])
                            let format_str = CoreExpr::Lit(CoreLit::String("~p~n".to_string()));
                            let args_list = arg_exprs
                                .into_iter()
                                .rev()
                                .fold(CoreExpr::Lit(CoreLit::Nil), |acc, elem| {
                                    CoreExpr::Cons(Box::new(elem), Box::new(acc))
                                });
                            CoreExpr::Call(
                                "io".to_string(),
                                "format".to_string(),
                                vec![format_str, args_list],
                            )
                        } else if name == "println" {
                            // println(x) -> io:format("~p~n", [x]) (same as print for now)
                            let format_str = CoreExpr::Lit(CoreLit::String("~p~n".to_string()));
                            let args_list = arg_exprs
                                .into_iter()
                                .rev()
                                .fold(CoreExpr::Lit(CoreLit::Nil), |acc, elem| {
                                    CoreExpr::Cons(Box::new(elem), Box::new(acc))
                                });
                            CoreExpr::Call(
                                "io".to_string(),
                                "format".to_string(),
                                vec![format_str, args_list],
                            )
                        } else if name == "length" && arg_exprs.len() == 1 {
                            // length(list) -> erlang:length(list)
                            CoreExpr::Call("erlang".to_string(), "length".to_string(), arg_exprs)
                        } else if name == "hd" && arg_exprs.len() == 1 {
                            // hd(list) -> erlang:hd(list)
                            CoreExpr::Call("erlang".to_string(), "hd".to_string(), arg_exprs)
                        } else if name == "tl" && arg_exprs.len() == 1 {
                            // tl(list) -> erlang:tl(list)
                            CoreExpr::Call("erlang".to_string(), "tl".to_string(), arg_exprs)
                        } else if name == "abs" && arg_exprs.len() == 1 {
                            // abs(n) -> erlang:abs(n)
                            CoreExpr::Call("erlang".to_string(), "abs".to_string(), arg_exprs)
                        } else if name == "max" && arg_exprs.len() == 2 {
                            // max(a, b) -> erlang:max(a, b)
                            CoreExpr::Call("erlang".to_string(), "max".to_string(), arg_exprs)
                        } else if name == "min" && arg_exprs.len() == 2 {
                            // min(a, b) -> erlang:min(a, b)
                            CoreExpr::Call("erlang".to_string(), "min".to_string(), arg_exprs)
                        } else if name == "reverse" && arg_exprs.len() == 1 {
                            // reverse(list) -> lists:reverse(list)
                            CoreExpr::Call("lists".to_string(), "reverse".to_string(), arg_exprs)
                        } else if name == "sort" && arg_exprs.len() == 1 {
                            // sort(list) -> lists:sort(list)
                            CoreExpr::Call("lists".to_string(), "sort".to_string(), arg_exprs)
                        } else if name == "append" && arg_exprs.len() == 2 {
                            // append(a, b) -> lists:append(a, b)
                            CoreExpr::Call("lists".to_string(), "append".to_string(), arg_exprs)
                        } else if name == "flatten" && arg_exprs.len() == 1 {
                            // flatten(list) -> lists:flatten(list)
                            CoreExpr::Call("lists".to_string(), "flatten".to_string(), arg_exprs)
                        } else if name == "to_string" && arg_exprs.len() == 1 {
                            // to_string(x) -> io_lib:format("~p", [x]) |> lists:flatten
                            let format_str = CoreExpr::Lit(CoreLit::String("~p".to_string()));
                            let args_list = CoreExpr::Cons(
                                Box::new(arg_exprs.into_iter().next().unwrap()),
                                Box::new(CoreExpr::Lit(CoreLit::Nil)),
                            );
                            let io_list = CoreExpr::Call(
                                "io_lib".to_string(),
                                "format".to_string(),
                                vec![format_str, args_list],
                            );
                            CoreExpr::Call(
                                "lists".to_string(),
                                "flatten".to_string(),
                                vec![io_list],
                            )
                        } else if name == "to_int" && arg_exprs.len() == 1 {
                            // to_int(string) -> list_to_integer(string)
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "list_to_integer".to_string(),
                                arg_exprs,
                            )
                        } else if name == "to_float" && arg_exprs.len() == 1 {
                            // to_float(string) -> list_to_float(string)
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "list_to_float".to_string(),
                                arg_exprs,
                            )
                        } else if name == "to_atom" && arg_exprs.len() == 1 {
                            // to_atom(string) -> list_to_atom(string)
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "list_to_atom".to_string(),
                                arg_exprs,
                            )
                        } else if name == "fst" && arg_exprs.len() == 1 {
                            // fst(tuple) -> element(1, tuple)
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "element".to_string(),
                                vec![
                                    CoreExpr::Lit(CoreLit::Int(1)),
                                    arg_exprs.into_iter().next().unwrap(),
                                ],
                            )
                        } else if name == "snd" && arg_exprs.len() == 1 {
                            // snd(tuple) -> element(2, tuple)
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "element".to_string(),
                                vec![
                                    CoreExpr::Lit(CoreLit::Int(2)),
                                    arg_exprs.into_iter().next().unwrap(),
                                ],
                            )
                        } else if name == "size" && arg_exprs.len() == 1 {
                            // size(tuple) -> tuple_size(tuple)
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "tuple_size".to_string(),
                                arg_exprs,
                            )
                        } else if name == "throw" && arg_exprs.len() == 1 {
                            // throw(term) -> erlang:throw(term)
                            CoreExpr::Call("erlang".to_string(), "throw".to_string(), arg_exprs)
                        } else if name == "exit" && arg_exprs.len() == 1 {
                            // exit(reason) -> erlang:exit(reason)
                            CoreExpr::Call("erlang".to_string(), "exit".to_string(), arg_exprs)
                        } else if name == "error" && arg_exprs.len() == 1 {
                            // error(reason) -> erlang:error(reason)
                            CoreExpr::Call("erlang".to_string(), "error".to_string(), arg_exprs)
                        } else if name == "band" && arg_exprs.len() == 2 {
                            // band(a, b) -> erlang:band(a, b)
                            CoreExpr::Call("erlang".to_string(), "band".to_string(), arg_exprs)
                        } else if name == "bor" && arg_exprs.len() == 2 {
                            // bor(a, b) -> erlang:bor(a, b)
                            CoreExpr::Call("erlang".to_string(), "bor".to_string(), arg_exprs)
                        } else if name == "bxor" && arg_exprs.len() == 2 {
                            // bxor(a, b) -> erlang:bxor(a, b)
                            CoreExpr::Call("erlang".to_string(), "bxor".to_string(), arg_exprs)
                        } else if name == "bnot" && arg_exprs.len() == 1 {
                            // bnot(x) -> erlang:bnot(x)
                            CoreExpr::Call("erlang".to_string(), "bnot".to_string(), arg_exprs)
                        } else if name == "bsl" && arg_exprs.len() == 2 {
                            // bsl(n, shift) -> erlang:bsl(n, shift)
                            CoreExpr::Call("erlang".to_string(), "bsl".to_string(), arg_exprs)
                        } else if name == "bsr" && arg_exprs.len() == 2 {
                            // bsr(n, shift) -> erlang:bsr(n, shift)
                            CoreExpr::Call("erlang".to_string(), "bsr".to_string(), arg_exprs)
                        } else if name == "rem" && arg_exprs.len() == 2 {
                            // rem(a, b) -> erlang:rem(a, b) (alternative to %)
                            CoreExpr::Call("erlang".to_string(), "rem".to_string(), arg_exprs)
                        } else if name == "div" && arg_exprs.len() == 2 {
                            // div(a, b) -> erlang:div(a, b) (integer division)
                            CoreExpr::Call("erlang".to_string(), "div".to_string(), arg_exprs)
                        } else if name == "is_int" && arg_exprs.len() == 1 {
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "is_integer".to_string(),
                                arg_exprs,
                            )
                        } else if name == "is_float" && arg_exprs.len() == 1 {
                            CoreExpr::Call("erlang".to_string(), "is_float".to_string(), arg_exprs)
                        } else if name == "is_number" && arg_exprs.len() == 1 {
                            CoreExpr::Call("erlang".to_string(), "is_number".to_string(), arg_exprs)
                        } else if name == "is_atom" && arg_exprs.len() == 1 {
                            CoreExpr::Call("erlang".to_string(), "is_atom".to_string(), arg_exprs)
                        } else if name == "is_list" && arg_exprs.len() == 1 {
                            CoreExpr::Call("erlang".to_string(), "is_list".to_string(), arg_exprs)
                        } else if name == "is_tuple" && arg_exprs.len() == 1 {
                            CoreExpr::Call("erlang".to_string(), "is_tuple".to_string(), arg_exprs)
                        } else if name == "is_map" && arg_exprs.len() == 1 {
                            CoreExpr::Call("erlang".to_string(), "is_map".to_string(), arg_exprs)
                        } else if name == "is_bool" && arg_exprs.len() == 1 {
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "is_boolean".to_string(),
                                arg_exprs,
                            )
                        } else if name == "is_nil" && arg_exprs.len() == 1 {
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "=:=".to_string(),
                                vec![
                                    arg_exprs.into_iter().next().unwrap(),
                                    CoreExpr::Lit(CoreLit::Nil),
                                ],
                            )
                        } else if name == "is_string" && arg_exprs.len() == 1 {
                            CoreExpr::Call("erlang".to_string(), "is_binary".to_string(), arg_exprs)
                        } else if name == "dynamic_is_string" && arg_exprs.len() == 1 {
                            CoreExpr::Call("erlang".to_string(), "is_binary".to_string(), arg_exprs)
                        } else if name == "dynamic_is_binary" && arg_exprs.len() == 1 {
                            CoreExpr::Call("erlang".to_string(), "is_binary".to_string(), arg_exprs)
                        } else if name == "dynamic_is_int" && arg_exprs.len() == 1 {
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "is_integer".to_string(),
                                arg_exprs,
                            )
                        } else if name == "dynamic_is_float" && arg_exprs.len() == 1 {
                            CoreExpr::Call("erlang".to_string(), "is_float".to_string(), arg_exprs)
                        } else if name == "dynamic_is_bool" && arg_exprs.len() == 1 {
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "is_boolean".to_string(),
                                arg_exprs,
                            )
                        } else if name == "dynamic_is_atom" && arg_exprs.len() == 1 {
                            CoreExpr::Call("erlang".to_string(), "is_atom".to_string(), arg_exprs)
                        } else if name == "dynamic_is_list" && arg_exprs.len() == 1 {
                            CoreExpr::Call("erlang".to_string(), "is_list".to_string(), arg_exprs)
                        } else if name == "dynamic_is_map" && arg_exprs.len() == 1 {
                            CoreExpr::Call("erlang".to_string(), "is_map".to_string(), arg_exprs)
                        } else if name == "dynamic_is_null" && arg_exprs.len() == 1 {
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "=:=".to_string(),
                                vec![
                                    arg_exprs.into_iter().next().unwrap(),
                                    CoreExpr::Lit(CoreLit::Atom("null".into())),
                                ],
                            )
                        } else if name == "is_pid" && arg_exprs.len() == 1 {
                            CoreExpr::Call("erlang".to_string(), "is_pid".to_string(), arg_exprs)
                        } else if name == "is_function" && arg_exprs.len() == 1 {
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "is_function".to_string(),
                                arg_exprs,
                            )
                        } else if name == "assert" && arg_exprs.len() == 1 {
                            // assert(cond) -> case cond of true -> ok; false -> error(assertion_failed)
                            let cond = arg_exprs.into_iter().next().unwrap();
                            CoreExpr::Case(
                                Box::new(cond),
                                vec![
                                    CoreClause {
                                        patterns: vec![CorePattern::Lit(CoreLit::Atom(
                                            "true".into(),
                                        ))],
                                        guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                        body: CoreExpr::Lit(CoreLit::Atom("ok".into())),
                                    },
                                    CoreClause {
                                        patterns: vec![CorePattern::Lit(CoreLit::Atom(
                                            "false".into(),
                                        ))],
                                        guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                        body: CoreExpr::Call(
                                            "erlang".into(),
                                            "error".into(),
                                            vec![CoreExpr::Lit(CoreLit::Atom(
                                                "assertion_failed".into(),
                                            ))],
                                        ),
                                    },
                                ],
                            )
                        } else if name == "dbg" && arg_exprs.len() == 1 {
                            // dbg(x) -> io:format("DBG: ~p~n", [x]), x
                            let x = arg_exprs.into_iter().next().unwrap();
                            let tmp = self.fresh_var();
                            let format_str =
                                CoreExpr::Lit(CoreLit::String("DBG: ~p~n".to_string()));
                            let args_list = CoreExpr::Cons(
                                Box::new(CoreExpr::Var(tmp.clone())),
                                Box::new(CoreExpr::Lit(CoreLit::Nil)),
                            );
                            let print_call = CoreExpr::Call(
                                "io".into(),
                                "format".into(),
                                vec![format_str, args_list],
                            );
                            CoreExpr::Let(
                                vec![(tmp.clone(), x)],
                                Box::new(CoreExpr::Seq(
                                    Box::new(print_call),
                                    Box::new(CoreExpr::Var(tmp)),
                                )),
                            )
                        } else if name == "sleep" && arg_exprs.len() == 1 {
                            // sleep(ms) -> timer:sleep(ms)
                            CoreExpr::Call("timer".to_string(), "sleep".to_string(), arg_exprs)
                        } else if name == "now" && arg_exprs.is_empty() {
                            // now() -> erlang:system_time(millisecond)
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "system_time".to_string(),
                                vec![CoreExpr::Lit(CoreLit::Atom("millisecond".into()))],
                            )
                        } else if name == "monotonic_time" && arg_exprs.is_empty() {
                            // monotonic_time() -> erlang:monotonic_time(millisecond)
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "monotonic_time".to_string(),
                                vec![CoreExpr::Lit(CoreLit::Atom("millisecond".into()))],
                            )
                        } else if name == "random" && arg_exprs.is_empty() {
                            // random() -> rand:uniform()
                            CoreExpr::Call("rand".to_string(), "uniform".to_string(), vec![])
                        } else if name == "random_below" && arg_exprs.len() == 1 {
                            // random_below(n) -> rand:uniform(n)
                            CoreExpr::Call("rand".to_string(), "uniform".to_string(), arg_exprs)
                        } else if name == "random_seed" && arg_exprs.is_empty() {
                            // random_seed() -> rand:seed(exsss)
                            CoreExpr::Call(
                                "rand".to_string(),
                                "seed".to_string(),
                                vec![CoreExpr::Lit(CoreLit::Atom("exsss".into()))],
                            )
                        } else if name == "spawn_link" && arg_exprs.len() == 1 {
                            // spawn_link(fun) -> erlang:spawn_link(fun)
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "spawn_link".to_string(),
                                arg_exprs,
                            )
                        } else if name == "link" && arg_exprs.len() == 1 {
                            // link(pid) -> erlang:link(pid)
                            CoreExpr::Call("erlang".to_string(), "link".to_string(), arg_exprs)
                        } else if name == "unlink" && arg_exprs.len() == 1 {
                            // unlink(pid) -> erlang:unlink(pid)
                            CoreExpr::Call("erlang".to_string(), "unlink".to_string(), arg_exprs)
                        } else if name == "monitor" && arg_exprs.len() == 1 {
                            // monitor(pid) -> erlang:monitor(process, pid)
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "monitor".to_string(),
                                vec![
                                    CoreExpr::Lit(CoreLit::Atom("process".into())),
                                    arg_exprs.into_iter().next().unwrap(),
                                ],
                            )
                        } else if name == "demonitor" && arg_exprs.len() == 1 {
                            // demonitor(ref) -> erlang:demonitor(ref)
                            CoreExpr::Call("erlang".to_string(), "demonitor".to_string(), arg_exprs)
                        } else if name == "registered" && arg_exprs.is_empty() {
                            // registered() -> erlang:registered()
                            CoreExpr::Call("erlang".to_string(), "registered".to_string(), vec![])
                        } else if name == "register" && arg_exprs.len() == 2 {
                            // register(name, pid) -> erlang:register(name, pid)
                            CoreExpr::Call("erlang".to_string(), "register".to_string(), arg_exprs)
                        } else if name == "whereis" && arg_exprs.len() == 1 {
                            // whereis(name) -> erlang:whereis(name)
                            CoreExpr::Call("erlang".to_string(), "whereis".to_string(), arg_exprs)
                        } else if name == "make_ref" && arg_exprs.is_empty() {
                            // make_ref() -> erlang:make_ref()
                            CoreExpr::Call("erlang".to_string(), "make_ref".to_string(), vec![])
                        } else if name == "str_length" && arg_exprs.len() == 1 {
                            CoreExpr::Call("erlang".to_string(), "byte_size".to_string(), arg_exprs)
                        } else if name == "str_concat" && arg_exprs.len() == 2 {
                            let args: Vec<_> = arg_exprs.into_iter().collect();
                            CoreExpr::Binary(vec![
                                CoreBinarySegment {
                                    value: args[0].clone(),
                                    size: CoreBinarySize::All,
                                    kind: CoreBinaryKind::Binary,
                                },
                                CoreBinarySegment {
                                    value: args[1].clone(),
                                    size: CoreBinarySize::All,
                                    kind: CoreBinaryKind::Binary,
                                },
                            ])
                        } else if name == "str_split" && arg_exprs.len() == 2 {
                            // str_split(s, sep) -> string:split(s, sep, all)
                            let mut args = arg_exprs;
                            args.push(CoreExpr::Lit(CoreLit::Atom("all".into())));
                            CoreExpr::Call("string".to_string(), "split".to_string(), args)
                        } else if name == "str_join" && arg_exprs.len() == 2 {
                            // str_join(list, sep) -> lists:join(sep, list)
                            let mut args: Vec<_> = arg_exprs.into_iter().collect();
                            args.reverse(); // swap order for lists:join
                            let joined =
                                CoreExpr::Call("lists".to_string(), "join".to_string(), args);
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "iolist_to_binary".to_string(),
                                vec![joined],
                            )
                        } else if name == "str_trim" && arg_exprs.len() == 1 {
                            // str_trim(s) -> string:trim(s)
                            CoreExpr::Call("string".to_string(), "trim".to_string(), arg_exprs)
                        } else if name == "str_upper" && arg_exprs.len() == 1 {
                            // str_upper(s) -> string:uppercase(s)
                            CoreExpr::Call("string".to_string(), "uppercase".to_string(), arg_exprs)
                        } else if name == "str_lower" && arg_exprs.len() == 1 {
                            // str_lower(s) -> string:lowercase(s)
                            CoreExpr::Call("string".to_string(), "lowercase".to_string(), arg_exprs)
                        } else if name == "str_replace" && arg_exprs.len() == 3 {
                            // str_replace(s, from, to) -> string:replace(s, from, to, all)
                            let mut args = arg_exprs;
                            args.push(CoreExpr::Lit(CoreLit::Atom("all".into())));
                            CoreExpr::Call("string".to_string(), "replace".to_string(), args)
                        } else if name == "str_contains" && arg_exprs.len() == 2 {
                            // str_contains(s, sub) -> string:find(s, sub) != nomatch
                            let find_call =
                                CoreExpr::Call("string".to_string(), "find".to_string(), arg_exprs);
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "/=".to_string(),
                                vec![find_call, CoreExpr::Lit(CoreLit::Atom("nomatch".into()))],
                            )
                        } else if name == "str_starts_with" && arg_exprs.len() == 2 {
                            // str_starts_with(s, prefix) -> string:prefix(s, prefix)
                            CoreExpr::Call("string".to_string(), "prefix".to_string(), arg_exprs)
                        } else if name == "str_slice" && arg_exprs.len() == 3 {
                            // str_slice(s, start, len) -> string:slice(s, start, len)
                            CoreExpr::Call("string".to_string(), "slice".to_string(), arg_exprs)
                        } else if name == "binary_slice" && arg_exprs.len() == 3 {
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "binary_part".to_string(),
                                arg_exprs,
                            )
                        } else if name == "str_char_at" && arg_exprs.len() == 2 {
                            let args: Vec<_> = arg_exprs.into_iter().collect();
                            CoreExpr::Call(
                                "binary".to_string(),
                                "at".to_string(),
                                vec![args[0].clone(), args[1].clone()],
                            )
                        } else if name == "str_ends_with" && arg_exprs.len() == 2 {
                            // str_ends_with(s, suffix) -> string:find(s, suffix, trailing) != nomatch
                            let args: Vec<_> = arg_exprs.into_iter().collect();
                            let find_call = CoreExpr::Call(
                                "string".to_string(),
                                "find".to_string(),
                                vec![
                                    args[0].clone(),
                                    args[1].clone(),
                                    CoreExpr::Lit(CoreLit::Atom("trailing".into())),
                                ],
                            );
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "/=".to_string(),
                                vec![find_call, CoreExpr::Lit(CoreLit::Atom("nomatch".into()))],
                            )
                        } else if name == "chars" && arg_exprs.len() == 1 {
                            // chars(s) -> string:to_graphemes(s)
                            CoreExpr::Call(
                                "string".to_string(),
                                "to_graphemes".to_string(),
                                arg_exprs,
                            )
                        } else if name == "str_from_chars" && arg_exprs.len() == 1 {
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "iolist_to_binary".to_string(),
                                arg_exprs,
                            )
                        } else if name == "take" && arg_exprs.len() == 2 {
                            // take(n, list) -> lists:sublist(list, n)
                            let args: Vec<_> = arg_exprs.into_iter().collect();
                            CoreExpr::Call(
                                "lists".to_string(),
                                "sublist".to_string(),
                                vec![args[1].clone(), args[0].clone()],
                            )
                        } else if name == "drop" && arg_exprs.len() == 2 {
                            // drop(n, list) -> lists:nthtail(n, list)
                            CoreExpr::Call("lists".to_string(), "nthtail".to_string(), arg_exprs)
                        } else if name == "nth" && arg_exprs.len() == 2 {
                            // nth(n, list) -> lists:nth(n, list)
                            CoreExpr::Call("lists".to_string(), "nth".to_string(), arg_exprs)
                        } else if name == "zip" && arg_exprs.len() == 2 {
                            // zip(a, b) -> lists:zip(a, b)
                            CoreExpr::Call("lists".to_string(), "zip".to_string(), arg_exprs)
                        } else if name == "unzip" && arg_exprs.len() == 1 {
                            // unzip(list) -> lists:unzip(list)
                            CoreExpr::Call("lists".to_string(), "unzip".to_string(), arg_exprs)
                        } else if name == "enumerate" && arg_exprs.len() == 1 {
                            // enumerate(list) -> lists:enumerate(list)
                            CoreExpr::Call("lists".to_string(), "enumerate".to_string(), arg_exprs)
                        } else if name == "member" && arg_exprs.len() == 2 {
                            // member(elem, list) -> lists:member(elem, list)
                            CoreExpr::Call("lists".to_string(), "member".to_string(), arg_exprs)
                        } else if name == "unique" && arg_exprs.len() == 1 {
                            // unique(list) -> lists:usort(list)
                            CoreExpr::Call("lists".to_string(), "usort".to_string(), arg_exprs)
                        // Map functions
                        } else if name == "map_put" && arg_exprs.len() == 3 {
                            // map_put(map, key, value) -> maps:put(key, value, map)
                            let args: Vec<_> = arg_exprs.into_iter().collect();
                            CoreExpr::Call(
                                "maps".to_string(),
                                "put".to_string(),
                                vec![args[1].clone(), args[2].clone(), args[0].clone()],
                            )
                        } else if name == "map_get" && arg_exprs.len() == 2 {
                            // map_get(map, key) -> maps:get(key, map)
                            let args: Vec<_> = arg_exprs.into_iter().collect();
                            CoreExpr::Call(
                                "maps".to_string(),
                                "get".to_string(),
                                vec![args[1].clone(), args[0].clone()],
                            )
                        } else if name == "map_get_or" && arg_exprs.len() == 3 {
                            // map_get_or(map, key, default) -> maps:get(key, map, default)
                            let args: Vec<_> = arg_exprs.into_iter().collect();
                            CoreExpr::Call(
                                "maps".to_string(),
                                "get".to_string(),
                                vec![args[1].clone(), args[0].clone(), args[2].clone()],
                            )
                        } else if name == "map_remove" && arg_exprs.len() == 2 {
                            // map_remove(map, key) -> maps:remove(key, map)
                            let args: Vec<_> = arg_exprs.into_iter().collect();
                            CoreExpr::Call(
                                "maps".to_string(),
                                "remove".to_string(),
                                vec![args[1].clone(), args[0].clone()],
                            )
                        } else if name == "map_keys" && arg_exprs.len() == 1 {
                            // map_keys(map) -> maps:keys(map)
                            CoreExpr::Call("maps".to_string(), "keys".to_string(), arg_exprs)
                        } else if name == "map_values" && arg_exprs.len() == 1 {
                            // map_values(map) -> maps:values(map)
                            CoreExpr::Call("maps".to_string(), "values".to_string(), arg_exprs)
                        } else if name == "map_has_key" && arg_exprs.len() == 2 {
                            // map_has_key(map, key) -> maps:is_key(key, map)
                            let args: Vec<_> = arg_exprs.into_iter().collect();
                            CoreExpr::Call(
                                "maps".to_string(),
                                "is_key".to_string(),
                                vec![args[1].clone(), args[0].clone()],
                            )
                        } else if name == "map_merge" && arg_exprs.len() == 2 {
                            // map_merge(map1, map2) -> maps:merge(map1, map2)
                            CoreExpr::Call("maps".to_string(), "merge".to_string(), arg_exprs)
                        } else if name == "map_size" && arg_exprs.len() == 1 {
                            // map_size(map) -> maps:size(map)
                            CoreExpr::Call("maps".to_string(), "size".to_string(), arg_exprs)
                        } else if name == "map_to_list" && arg_exprs.len() == 1 {
                            // map_to_list(map) -> maps:to_list(map)
                            CoreExpr::Call("maps".to_string(), "to_list".to_string(), arg_exprs)
                        } else if name == "list_to_map" && arg_exprs.len() == 1 {
                            // list_to_map(list) -> maps:from_list(list)
                            CoreExpr::Call("maps".to_string(), "from_list".to_string(), arg_exprs)
                        // File I/O functions
                        } else if name == "file_read" && arg_exprs.len() == 1 {
                            // file_read(path) -> file:read_file(path)
                            CoreExpr::Call("file".to_string(), "read_file".to_string(), arg_exprs)
                        } else if name == "file_write" && arg_exprs.len() == 2 {
                            // file_write(path, content) -> file:write_file(path, content)
                            CoreExpr::Call("file".to_string(), "write_file".to_string(), arg_exprs)
                        } else if name == "file_exists" && arg_exprs.len() == 1 {
                            // file_exists(path) -> filelib:is_file(path)
                            CoreExpr::Call("filelib".to_string(), "is_file".to_string(), arg_exprs)
                        } else if name == "file_delete" && arg_exprs.len() == 1 {
                            // file_delete(path) -> file:delete(path)
                            CoreExpr::Call("file".to_string(), "delete".to_string(), arg_exprs)
                        } else if name == "dir_list" && arg_exprs.len() == 1 {
                            // dir_list(path) -> file:list_dir(path)
                            CoreExpr::Call("file".to_string(), "list_dir".to_string(), arg_exprs)
                        } else if name == "dir_make" && arg_exprs.len() == 1 {
                            // dir_make(path) -> file:make_dir(path)
                            CoreExpr::Call("file".to_string(), "make_dir".to_string(), arg_exprs)
                        } else if name == "get_cwd" && arg_exprs.is_empty() {
                            // get_cwd() -> file:get_cwd()
                            CoreExpr::Call("file".to_string(), "get_cwd".to_string(), vec![])
                        } else if name == "argv" && arg_exprs.is_empty() {
                            // argv() -> init:get_plain_arguments()
                            CoreExpr::Call(
                                "init".to_string(),
                                "get_plain_arguments".to_string(),
                                vec![],
                            )
                        } else if name == "env" && arg_exprs.len() == 1 {
                            let name_arg = arg_exprs.into_iter().next().unwrap();
                            let getenv = CoreExpr::Call(
                                "os".to_string(),
                                "getenv".to_string(),
                                vec![self.binary_to_list_expr(name_arg)],
                            );
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "list_to_binary".to_string(),
                                vec![getenv],
                            )
                        } else if name == "set_env" && arg_exprs.len() == 2 {
                            let args: Vec<_> = arg_exprs.into_iter().collect();
                            CoreExpr::Call(
                                "os".to_string(),
                                "putenv".to_string(),
                                vec![
                                    self.binary_to_list_expr(args[0].clone()),
                                    self.binary_to_list_expr(args[1].clone()),
                                ],
                            )
                        } else if name == "exit_code" && arg_exprs.len() == 1 {
                            // exit_code(n) -> erlang:halt(n)
                            CoreExpr::Call("erlang".to_string(), "halt".to_string(), arg_exprs)
                        } else if name == "os_cmd" && arg_exprs.len() == 1 {
                            let cmd_arg = arg_exprs.into_iter().next().unwrap();
                            let result = CoreExpr::Call(
                                "os".to_string(),
                                "cmd".to_string(),
                                vec![self.binary_to_list_expr(cmd_arg)],
                            );
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "list_to_binary".to_string(),
                                vec![result],
                            )
                        // Process dictionary
                        } else if name == "get" && arg_exprs.len() == 1 {
                            // get(key) -> erlang:get(key)
                            CoreExpr::Call("erlang".to_string(), "get".to_string(), arg_exprs)
                        } else if name == "put" && arg_exprs.len() == 2 {
                            // put(key, value) -> erlang:put(key, value)
                            CoreExpr::Call("erlang".to_string(), "put".to_string(), arg_exprs)
                        } else if name == "erase" && arg_exprs.len() == 1 {
                            // erase(key) -> erlang:erase(key)
                            CoreExpr::Call("erlang".to_string(), "erase".to_string(), arg_exprs)
                        } else if name == "elem" && arg_exprs.len() == 2 {
                            // elem(tuple, index) -> erlang:element(index, tuple) (1-based)
                            let args: Vec<_> = arg_exprs.into_iter().collect();
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "element".to_string(),
                                vec![args[1].clone(), args[0].clone()],
                            )
                        } else if name == "set_elem" && arg_exprs.len() == 3 {
                            // set_elem(tuple, index, value) -> erlang:setelement(index, tuple, value)
                            let args: Vec<_> = arg_exprs.into_iter().collect();
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "setelement".to_string(),
                                vec![args[1].clone(), args[0].clone(), args[2].clone()],
                            )
                        } else if name == "tuple_to_list" && arg_exprs.len() == 1 {
                            // tuple_to_list(tuple) -> erlang:tuple_to_list(tuple)
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "tuple_to_list".to_string(),
                                arg_exprs,
                            )
                        } else if name == "list_to_tuple" && arg_exprs.len() == 1 {
                            // list_to_tuple(list) -> erlang:list_to_tuple(list)
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "list_to_tuple".to_string(),
                                arg_exprs,
                            )
                        // Binary functions
                        } else if name == "byte_size" && arg_exprs.len() == 1 {
                            CoreExpr::Call("erlang".to_string(), "byte_size".to_string(), arg_exprs)
                        } else if name == "bit_size" && arg_exprs.len() == 1 {
                            CoreExpr::Call("erlang".to_string(), "bit_size".to_string(), arg_exprs)
                        } else if name == "binary_slice" && arg_exprs.len() == 3 {
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "binary_part".to_string(),
                                arg_exprs,
                            )
                        } else if name == "binary_to_list" && arg_exprs.len() == 1 {
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "binary_to_list".to_string(),
                                arg_exprs,
                            )
                        } else if name == "list_to_binary" && arg_exprs.len() == 1 {
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "list_to_binary".to_string(),
                                arg_exprs,
                            )
                        } else if name == "is_binary" && arg_exprs.len() == 1 {
                            CoreExpr::Call("erlang".to_string(), "is_binary".to_string(), arg_exprs)
                        // More utility functions
                        } else if name == "apply" && arg_exprs.len() == 2 {
                            // apply(fun, args) -> erlang:apply(fun, args)
                            CoreExpr::Call("erlang".to_string(), "apply".to_string(), arg_exprs)
                        } else if name == "apply_module" && arg_exprs.len() == 3 {
                            // apply_module(mod, fun, args) -> erlang:apply(mod, fun, args)
                            CoreExpr::Call("erlang".to_string(), "apply".to_string(), arg_exprs)
                        } else if name == "atom_to_list" && arg_exprs.len() == 1 {
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "atom_to_list".to_string(),
                                arg_exprs,
                            )
                        } else if name == "list_to_atom" && arg_exprs.len() == 1 {
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "list_to_atom".to_string(),
                                arg_exprs,
                            )
                        } else if name == "integer_to_list" && arg_exprs.len() == 1 {
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "integer_to_list".to_string(),
                                arg_exprs,
                            )
                        } else if name == "list_to_integer" && arg_exprs.len() == 1 {
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "list_to_integer".to_string(),
                                arg_exprs,
                            )
                        } else if name == "float_to_list" && arg_exprs.len() == 1 {
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "float_to_list".to_string(),
                                arg_exprs,
                            )
                        } else if name == "list_to_float" && arg_exprs.len() == 1 {
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "list_to_float".to_string(),
                                arg_exprs,
                            )
                        } else if name == "iolist_to_binary" && arg_exprs.len() == 1 {
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "iolist_to_binary".to_string(),
                                arg_exprs,
                            )
                        } else if name == "term_to_binary" && arg_exprs.len() == 1 {
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "term_to_binary".to_string(),
                                arg_exprs,
                            )
                        } else if name == "binary_to_term" && arg_exprs.len() == 1 {
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "binary_to_term".to_string(),
                                arg_exprs,
                            )
                        } else if name == "dynamic" && arg_exprs.len() == 1 {
                            arg_exprs.into_iter().next().unwrap()
                        } else if name == "dynamic_json_decode" && arg_exprs.len() == 1 {
                            CoreExpr::Call("json".to_string(), "decode".to_string(), arg_exprs)
                        } else if name == "dynamic_json_decode_result" && arg_exprs.len() == 1 {
                            self.dynamic_result_try(
                                CoreExpr::Call("json".to_string(), "decode".to_string(), arg_exprs),
                                self.decode_error_invalid_json(),
                            )
                        } else if name == "dynamic_json_encode" && arg_exprs.len() == 1 {
                            let encoded =
                                CoreExpr::Call("json".to_string(), "encode".to_string(), arg_exprs);
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "iolist_to_binary".to_string(),
                                vec![encoded],
                            )
                        } else if name == "dynamic_get" && arg_exprs.len() == 2 {
                            let args: Vec<_> = arg_exprs.into_iter().collect();
                            let checked_map = self.checked_dynamic_cast(
                                args[0].clone(),
                                "erlang",
                                "is_map",
                                "map",
                            );
                            CoreExpr::Call(
                                "maps".to_string(),
                                "get".to_string(),
                                vec![args[1].clone(), checked_map],
                            )
                        } else if name == "dynamic_get_result" && arg_exprs.len() == 2 {
                            let args: Vec<_> = arg_exprs.into_iter().collect();
                            let map_var = self.fresh_var();
                            let value_var = self.fresh_var();
                            let checked_map = self.checked_dynamic_result_cast(
                                args[0].clone(),
                                "erlang",
                                "is_map",
                                "map",
                            );
                            let find = CoreExpr::Call(
                                "maps".to_string(),
                                "find".to_string(),
                                vec![args[1].clone(), CoreExpr::Var(map_var.clone())],
                            );
                            CoreExpr::Case(
                                Box::new(checked_map),
                                vec![
                                    CoreClause {
                                        patterns: vec![CorePattern::Tuple(vec![
                                            CorePattern::Lit(CoreLit::Atom("ok".into())),
                                            CorePattern::Var(map_var.clone()),
                                        ])],
                                        guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                        body: CoreExpr::Case(
                                            Box::new(find),
                                            vec![
                                                CoreClause {
                                                    patterns: vec![CorePattern::Tuple(vec![
                                                        CorePattern::Lit(CoreLit::Atom(
                                                            "ok".into(),
                                                        )),
                                                        CorePattern::Var(value_var.clone()),
                                                    ])],
                                                    guard: CoreExpr::Lit(CoreLit::Atom(
                                                        "true".into(),
                                                    )),
                                                    body: self.dynamic_result_ok(CoreExpr::Var(
                                                        value_var,
                                                    )),
                                                },
                                                CoreClause {
                                                    patterns: vec![CorePattern::Lit(
                                                        CoreLit::Atom("error".into()),
                                                    )],
                                                    guard: CoreExpr::Lit(CoreLit::Atom(
                                                        "true".into(),
                                                    )),
                                                    body: self.dynamic_result_err(
                                                        self.decode_error_missing_key(),
                                                    ),
                                                },
                                            ],
                                        ),
                                    },
                                    CoreClause {
                                        patterns: vec![CorePattern::Tuple(vec![
                                            CorePattern::Lit(CoreLit::Atom("err".into())),
                                            CorePattern::Var("_Reason".into()),
                                        ])],
                                        guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                        body: self
                                            .dynamic_result_err(self.decode_error_expected("map")),
                                    },
                                ],
                            )
                        } else if name == "dynamic_get_or" && arg_exprs.len() == 3 {
                            let args: Vec<_> = arg_exprs.into_iter().collect();
                            let checked_map = self.checked_dynamic_cast(
                                args[0].clone(),
                                "erlang",
                                "is_map",
                                "map",
                            );
                            CoreExpr::Call(
                                "maps".to_string(),
                                "get".to_string(),
                                vec![args[1].clone(), checked_map, args[2].clone()],
                            )
                        } else if name == "dynamic_at" && arg_exprs.len() == 2 {
                            let args: Vec<_> = arg_exprs.into_iter().collect();
                            let checked_list = self.checked_dynamic_cast(
                                args[0].clone(),
                                "erlang",
                                "is_list",
                                "list",
                            );
                            CoreExpr::Call(
                                "lists".to_string(),
                                "nth".to_string(),
                                vec![args[1].clone(), checked_list],
                            )
                        } else if name == "dynamic_at_result" && arg_exprs.len() == 2 {
                            let args: Vec<_> = arg_exprs.into_iter().collect();
                            let checked_list = self.checked_dynamic_result_cast(
                                args[0].clone(),
                                "erlang",
                                "is_list",
                                "list",
                            );
                            let list_var = self.fresh_var();
                            CoreExpr::Case(
                                Box::new(checked_list),
                                vec![
                                    CoreClause {
                                        patterns: vec![CorePattern::Tuple(vec![
                                            CorePattern::Lit(CoreLit::Atom("ok".into())),
                                            CorePattern::Var(list_var.clone()),
                                        ])],
                                        guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                        body: self.dynamic_result_try(
                                            CoreExpr::Call(
                                                "lists".to_string(),
                                                "nth".to_string(),
                                                vec![args[1].clone(), CoreExpr::Var(list_var)],
                                            ),
                                            self.decode_error_index_out_of_bounds(args[1].clone()),
                                        ),
                                    },
                                    CoreClause {
                                        patterns: vec![CorePattern::Tuple(vec![
                                            CorePattern::Lit(CoreLit::Atom("err".into())),
                                            CorePattern::Var("_Reason".into()),
                                        ])],
                                        guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                        body: self
                                            .dynamic_result_err(self.decode_error_expected("list")),
                                    },
                                ],
                            )
                        } else if name == "dynamic_string" && arg_exprs.len() == 1 {
                            self.checked_dynamic_cast(
                                arg_exprs.into_iter().next().unwrap(),
                                "erlang",
                                "is_binary",
                                "string",
                            )
                        } else if name == "dynamic_binary" && arg_exprs.len() == 1 {
                            self.checked_dynamic_cast(
                                arg_exprs.into_iter().next().unwrap(),
                                "erlang",
                                "is_binary",
                                "binary",
                            )
                        } else if name == "dynamic_int" && arg_exprs.len() == 1 {
                            self.checked_dynamic_cast(
                                arg_exprs.into_iter().next().unwrap(),
                                "erlang",
                                "is_integer",
                                "int",
                            )
                        } else if name == "dynamic_float" && arg_exprs.len() == 1 {
                            self.checked_dynamic_cast(
                                arg_exprs.into_iter().next().unwrap(),
                                "erlang",
                                "is_float",
                                "float",
                            )
                        } else if name == "dynamic_bool" && arg_exprs.len() == 1 {
                            self.checked_dynamic_cast(
                                arg_exprs.into_iter().next().unwrap(),
                                "erlang",
                                "is_boolean",
                                "bool",
                            )
                        } else if name == "dynamic_atom" && arg_exprs.len() == 1 {
                            self.checked_dynamic_cast(
                                arg_exprs.into_iter().next().unwrap(),
                                "erlang",
                                "is_atom",
                                "atom",
                            )
                        } else if name == "dynamic_list" && arg_exprs.len() == 1 {
                            self.checked_dynamic_cast(
                                arg_exprs.into_iter().next().unwrap(),
                                "erlang",
                                "is_list",
                                "list",
                            )
                        } else if name == "dynamic_map" && arg_exprs.len() == 1 {
                            self.checked_dynamic_cast(
                                arg_exprs.into_iter().next().unwrap(),
                                "erlang",
                                "is_map",
                                "map",
                            )
                        } else if name == "dynamic_string_result" && arg_exprs.len() == 1 {
                            self.checked_dynamic_result_cast(
                                arg_exprs.into_iter().next().unwrap(),
                                "erlang",
                                "is_binary",
                                "string",
                            )
                        } else if name == "dynamic_binary_result" && arg_exprs.len() == 1 {
                            self.checked_dynamic_result_cast(
                                arg_exprs.into_iter().next().unwrap(),
                                "erlang",
                                "is_binary",
                                "binary",
                            )
                        } else if name == "dynamic_int_result" && arg_exprs.len() == 1 {
                            self.checked_dynamic_result_cast(
                                arg_exprs.into_iter().next().unwrap(),
                                "erlang",
                                "is_integer",
                                "int",
                            )
                        } else if name == "dynamic_float_result" && arg_exprs.len() == 1 {
                            self.checked_dynamic_result_cast(
                                arg_exprs.into_iter().next().unwrap(),
                                "erlang",
                                "is_float",
                                "float",
                            )
                        } else if name == "dynamic_bool_result" && arg_exprs.len() == 1 {
                            self.checked_dynamic_result_cast(
                                arg_exprs.into_iter().next().unwrap(),
                                "erlang",
                                "is_boolean",
                                "bool",
                            )
                        } else if name == "dynamic_atom_result" && arg_exprs.len() == 1 {
                            self.checked_dynamic_result_cast(
                                arg_exprs.into_iter().next().unwrap(),
                                "erlang",
                                "is_atom",
                                "atom",
                            )
                        } else if name == "dynamic_list_result" && arg_exprs.len() == 1 {
                            self.checked_dynamic_result_cast(
                                arg_exprs.into_iter().next().unwrap(),
                                "erlang",
                                "is_list",
                                "list",
                            )
                        } else if name == "dynamic_map_result" && arg_exprs.len() == 1 {
                            self.checked_dynamic_result_cast(
                                arg_exprs.into_iter().next().unwrap(),
                                "erlang",
                                "is_map",
                                "map",
                            )
                        } else if name == "decode_map" && arg_exprs.len() == 2 {
                            let args: Vec<_> = arg_exprs.into_iter().collect();
                            let ok_var = self.fresh_var();
                            let err_var = self.fresh_var();
                            CoreExpr::Case(
                                Box::new(args[0].clone()),
                                vec![
                                    CoreClause {
                                        patterns: vec![CorePattern::Tuple(vec![
                                            CorePattern::Lit(CoreLit::Atom("ok".into())),
                                            CorePattern::Var(ok_var.clone()),
                                        ])],
                                        guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                        body: self.dynamic_result_ok(CoreExpr::Apply(
                                            Box::new(args[1].clone()),
                                            vec![CoreExpr::Var(ok_var)],
                                        )),
                                    },
                                    CoreClause {
                                        patterns: vec![CorePattern::Tuple(vec![
                                            CorePattern::Lit(CoreLit::Atom("err".into())),
                                            CorePattern::Var(err_var.clone()),
                                        ])],
                                        guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                        body: CoreExpr::Tuple(vec![
                                            CoreExpr::Lit(CoreLit::Atom("err".into())),
                                            CoreExpr::Var(err_var),
                                        ]),
                                    },
                                ],
                            )
                        } else if name == "decode_then" && arg_exprs.len() == 2 {
                            let args: Vec<_> = arg_exprs.into_iter().collect();
                            let ok_var = self.fresh_var();
                            let err_var = self.fresh_var();
                            CoreExpr::Case(
                                Box::new(args[0].clone()),
                                vec![
                                    CoreClause {
                                        patterns: vec![CorePattern::Tuple(vec![
                                            CorePattern::Lit(CoreLit::Atom("ok".into())),
                                            CorePattern::Var(ok_var.clone()),
                                        ])],
                                        guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                        body: CoreExpr::Apply(
                                            Box::new(args[1].clone()),
                                            vec![CoreExpr::Var(ok_var)],
                                        ),
                                    },
                                    CoreClause {
                                        patterns: vec![CorePattern::Tuple(vec![
                                            CorePattern::Lit(CoreLit::Atom("err".into())),
                                            CorePattern::Var(err_var.clone()),
                                        ])],
                                        guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                        body: CoreExpr::Tuple(vec![
                                            CoreExpr::Lit(CoreLit::Atom("err".into())),
                                            CoreExpr::Var(err_var),
                                        ]),
                                    },
                                ],
                            )
                        } else if name == "decode_field" && arg_exprs.len() == 3 {
                            let args: Vec<_> = arg_exprs.into_iter().collect();
                            let value_var = self.fresh_var();
                            let err_var = self.fresh_var();
                            let decoded_var = self.fresh_var();
                            let field_key = args[1].clone();
                            CoreExpr::Case(
                                Box::new(
                                    self.translate_decode_field(args[0].clone(), args[1].clone()),
                                ),
                                vec![
                                    CoreClause {
                                        patterns: vec![CorePattern::Tuple(vec![
                                            CorePattern::Lit(CoreLit::Atom("ok".into())),
                                            CorePattern::Var(value_var.clone()),
                                        ])],
                                        guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                        body: CoreExpr::Let(
                                            vec![(
                                                decoded_var.clone(),
                                                CoreExpr::Apply(
                                                    Box::new(args[2].clone()),
                                                    vec![CoreExpr::Var(value_var.clone())],
                                                ),
                                            )],
                                            Box::new(CoreExpr::Case(
                                                Box::new(CoreExpr::Var(decoded_var)),
                                                vec![
                                                    CoreClause {
                                                        patterns: vec![CorePattern::Tuple(vec![
                                                            CorePattern::Lit(CoreLit::Atom(
                                                                "ok".into(),
                                                            )),
                                                            CorePattern::Var(value_var.clone()),
                                                        ])],
                                                        guard: CoreExpr::Lit(CoreLit::Atom(
                                                            "true".into(),
                                                        )),
                                                        body: self.dynamic_result_ok(
                                                            CoreExpr::Var(value_var.clone()),
                                                        ),
                                                    },
                                                    CoreClause {
                                                        patterns: vec![CorePattern::Tuple(vec![
                                                            CorePattern::Lit(CoreLit::Atom(
                                                                "err".into(),
                                                            )),
                                                            CorePattern::Var(err_var.clone()),
                                                        ])],
                                                        guard: CoreExpr::Lit(CoreLit::Atom(
                                                            "true".into(),
                                                        )),
                                                        body: self.dynamic_result_err(
                                                            self.decode_error_at(
                                                                self.decode_path_field(
                                                                    field_key.clone(),
                                                                ),
                                                                CoreExpr::Var(err_var.clone()),
                                                            ),
                                                        ),
                                                    },
                                                ],
                                            )),
                                        ),
                                    },
                                    CoreClause {
                                        patterns: vec![CorePattern::Tuple(vec![
                                            CorePattern::Lit(CoreLit::Atom("err".into())),
                                            CorePattern::Var(err_var.clone()),
                                        ])],
                                        guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                        body: self
                                            .dynamic_result_err(CoreExpr::Var(err_var.clone())),
                                    },
                                ],
                            )
                        } else if name == "decode_optional_field" && arg_exprs.len() == 3 {
                            let args: Vec<_> = arg_exprs.into_iter().collect();
                            self.translate_decode_optional_field(
                                args[0].clone(),
                                args[1].clone(),
                                args[2].clone(),
                            )
                        } else if name == "decode_field_or" && arg_exprs.len() == 4 {
                            let args: Vec<_> = arg_exprs.into_iter().collect();
                            self.translate_decode_field_or(
                                args[0].clone(),
                                args[1].clone(),
                                args[2].clone(),
                                args[3].clone(),
                            )
                        } else if name == "decode_index" && arg_exprs.len() == 3 {
                            let args: Vec<_> = arg_exprs.into_iter().collect();
                            let value_var = self.fresh_var();
                            let err_var = self.fresh_var();
                            let decoded_var = self.fresh_var();
                            let index_expr = args[1].clone();
                            CoreExpr::Case(
                                Box::new(
                                    self.translate_decode_index(args[0].clone(), args[1].clone()),
                                ),
                                vec![
                                    CoreClause {
                                        patterns: vec![CorePattern::Tuple(vec![
                                            CorePattern::Lit(CoreLit::Atom("ok".into())),
                                            CorePattern::Var(value_var.clone()),
                                        ])],
                                        guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                        body: CoreExpr::Let(
                                            vec![(
                                                decoded_var.clone(),
                                                CoreExpr::Apply(
                                                    Box::new(args[2].clone()),
                                                    vec![CoreExpr::Var(value_var.clone())],
                                                ),
                                            )],
                                            Box::new(CoreExpr::Case(
                                                Box::new(CoreExpr::Var(decoded_var)),
                                                vec![
                                                    CoreClause {
                                                        patterns: vec![CorePattern::Tuple(vec![
                                                            CorePattern::Lit(CoreLit::Atom(
                                                                "ok".into(),
                                                            )),
                                                            CorePattern::Var(value_var.clone()),
                                                        ])],
                                                        guard: CoreExpr::Lit(CoreLit::Atom(
                                                            "true".into(),
                                                        )),
                                                        body: self.dynamic_result_ok(
                                                            CoreExpr::Var(value_var.clone()),
                                                        ),
                                                    },
                                                    CoreClause {
                                                        patterns: vec![CorePattern::Tuple(vec![
                                                            CorePattern::Lit(CoreLit::Atom(
                                                                "err".into(),
                                                            )),
                                                            CorePattern::Var(err_var.clone()),
                                                        ])],
                                                        guard: CoreExpr::Lit(CoreLit::Atom(
                                                            "true".into(),
                                                        )),
                                                        body: self.dynamic_result_err(
                                                            self.decode_error_at(
                                                                self.decode_path_index(
                                                                    index_expr.clone(),
                                                                ),
                                                                CoreExpr::Var(err_var.clone()),
                                                            ),
                                                        ),
                                                    },
                                                ],
                                            )),
                                        ),
                                    },
                                    CoreClause {
                                        patterns: vec![CorePattern::Tuple(vec![
                                            CorePattern::Lit(CoreLit::Atom("err".into())),
                                            CorePattern::Var(err_var.clone()),
                                        ])],
                                        guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                        body: self
                                            .dynamic_result_err(CoreExpr::Var(err_var.clone())),
                                    },
                                ],
                            )
                        } else if name == "decode_list" && arg_exprs.len() == 2 {
                            let args: Vec<_> = arg_exprs.into_iter().collect();
                            self.translate_decode_list(args[0].clone(), args[1].clone())
                        } else if name == "decode_optional" && arg_exprs.len() == 2 {
                            let args: Vec<_> = arg_exprs.into_iter().collect();
                            self.translate_decode_optional(args[0].clone(), args[1].clone())
                        } else if name == "decode_dict" && arg_exprs.len() == 2 {
                            let args: Vec<_> = arg_exprs.into_iter().collect();
                            self.translate_decode_dict(args[0].clone(), args[1].clone())
                        } else if name == "decode_one_of" && arg_exprs.len() == 3 {
                            let args: Vec<_> = arg_exprs.into_iter().collect();
                            let first_err = self.fresh_var();
                            let second_err = self.fresh_var();
                            let ok_var = self.fresh_var();
                            let first_try =
                                CoreExpr::Apply(Box::new(args[1].clone()), vec![args[0].clone()]);
                            CoreExpr::Case(
                                Box::new(first_try),
                                vec![
                                    CoreClause {
                                        patterns: vec![CorePattern::Tuple(vec![
                                            CorePattern::Lit(CoreLit::Atom("ok".into())),
                                            CorePattern::Var(ok_var.clone()),
                                        ])],
                                        guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                        body: self.dynamic_result_ok(CoreExpr::Var(ok_var.clone())),
                                    },
                                    CoreClause {
                                        patterns: vec![CorePattern::Tuple(vec![
                                            CorePattern::Lit(CoreLit::Atom("err".into())),
                                            CorePattern::Var(first_err.clone()),
                                        ])],
                                        guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                        body: CoreExpr::Case(
                                            Box::new(CoreExpr::Apply(
                                                Box::new(args[2].clone()),
                                                vec![args[0].clone()],
                                            )),
                                            vec![
                                                CoreClause {
                                                    patterns: vec![CorePattern::Tuple(vec![
                                                        CorePattern::Lit(CoreLit::Atom(
                                                            "ok".into(),
                                                        )),
                                                        CorePattern::Var(ok_var.clone()),
                                                    ])],
                                                    guard: CoreExpr::Lit(CoreLit::Atom(
                                                        "true".into(),
                                                    )),
                                                    body: self.dynamic_result_ok(CoreExpr::Var(
                                                        ok_var.clone(),
                                                    )),
                                                },
                                                CoreClause {
                                                    patterns: vec![CorePattern::Tuple(vec![
                                                        CorePattern::Lit(CoreLit::Atom(
                                                            "err".into(),
                                                        )),
                                                        CorePattern::Var(second_err.clone()),
                                                    ])],
                                                    guard: CoreExpr::Lit(CoreLit::Atom(
                                                        "true".into(),
                                                    )),
                                                    body: self.dynamic_result_err(
                                                        self.decode_error_one_of(
                                                            CoreExpr::Var(first_err.clone()),
                                                            CoreExpr::Var(second_err),
                                                        ),
                                                    ),
                                                },
                                            ],
                                        ),
                                    },
                                ],
                            )
                        } else if (name == "typeof" || name == "dynamic_typeof")
                            && arg_exprs.len() == 1
                        {
                            // typeof(x) -> returns atom describing type
                            let x = arg_exprs.into_iter().next().unwrap();
                            let tmp = self.fresh_var();
                            CoreExpr::Let(
                                vec![(tmp.clone(), x)],
                                Box::new(CoreExpr::Case(
                                    Box::new(CoreExpr::Lit(CoreLit::Atom("true".into()))),
                                    vec![
                                        CoreClause {
                                            patterns: vec![CorePattern::Var("_".into())],
                                            guard: CoreExpr::Call(
                                                "erlang".into(),
                                                "is_integer".into(),
                                                vec![CoreExpr::Var(tmp.clone())],
                                            ),
                                            body: CoreExpr::Lit(CoreLit::Atom("integer".into())),
                                        },
                                        CoreClause {
                                            patterns: vec![CorePattern::Var("_".into())],
                                            guard: CoreExpr::Call(
                                                "erlang".into(),
                                                "is_float".into(),
                                                vec![CoreExpr::Var(tmp.clone())],
                                            ),
                                            body: CoreExpr::Lit(CoreLit::Atom("float".into())),
                                        },
                                        CoreClause {
                                            patterns: vec![CorePattern::Var("_".into())],
                                            guard: CoreExpr::Call(
                                                "erlang".into(),
                                                "is_atom".into(),
                                                vec![CoreExpr::Var(tmp.clone())],
                                            ),
                                            body: CoreExpr::Lit(CoreLit::Atom("atom".into())),
                                        },
                                        CoreClause {
                                            patterns: vec![CorePattern::Var("_".into())],
                                            guard: CoreExpr::Call(
                                                "erlang".into(),
                                                "is_list".into(),
                                                vec![CoreExpr::Var(tmp.clone())],
                                            ),
                                            body: CoreExpr::Lit(CoreLit::Atom("list".into())),
                                        },
                                        CoreClause {
                                            patterns: vec![CorePattern::Var("_".into())],
                                            guard: CoreExpr::Call(
                                                "erlang".into(),
                                                "is_tuple".into(),
                                                vec![CoreExpr::Var(tmp.clone())],
                                            ),
                                            body: CoreExpr::Lit(CoreLit::Atom("tuple".into())),
                                        },
                                        CoreClause {
                                            patterns: vec![CorePattern::Var("_".into())],
                                            guard: CoreExpr::Call(
                                                "erlang".into(),
                                                "is_map".into(),
                                                vec![CoreExpr::Var(tmp.clone())],
                                            ),
                                            body: CoreExpr::Lit(CoreLit::Atom("map".into())),
                                        },
                                        CoreClause {
                                            patterns: vec![CorePattern::Var("_".into())],
                                            guard: CoreExpr::Call(
                                                "erlang".into(),
                                                "is_pid".into(),
                                                vec![CoreExpr::Var(tmp.clone())],
                                            ),
                                            body: CoreExpr::Lit(CoreLit::Atom("pid".into())),
                                        },
                                        CoreClause {
                                            patterns: vec![CorePattern::Var("_".into())],
                                            guard: CoreExpr::Call(
                                                "erlang".into(),
                                                "is_function".into(),
                                                vec![CoreExpr::Var(tmp)],
                                            ),
                                            body: CoreExpr::Lit(CoreLit::Atom("function".into())),
                                        },
                                    ],
                                )),
                            )
                        } else if let Some(symbol) = self.lookup_function_symbol(name, args.len()) {
                            // Local function call - call content-addressed module's apply/N.
                            CoreExpr::Call(symbol.module_name.clone(), "apply".into(), arg_exprs)
                        } else if let Some(hash) = self.resolved_symbol_hash(name, args.len()) {
                            CoreExpr::Call(hash.clone(), "apply".into(), arg_exprs)
                        } else {
                            // Could be a variable holding a function
                            let func_expr = self.translate_expr(func);
                            CoreExpr::Apply(Box::new(func_expr), arg_exprs)
                        }
                    }
                    Expr::Path(parts, _) if parts.len() >= 2 => {
                        // Check if this looks like an enum constructor (Type::Variant)
                        // vs a module call (module:function)
                        let first = &parts[0];
                        let last = parts.last().unwrap();

                        // If the first part starts with uppercase, treat as enum constructor
                        if first
                            .chars()
                            .next()
                            .map(|c| c.is_uppercase())
                            .unwrap_or(false)
                        {
                            // Enum constructor with args: Message::Get(pid) -> {get, pid}
                            let tag = to_snake_case(last);
                            let mut tuple_elems = vec![CoreExpr::Lit(CoreLit::Atom(tag))];
                            tuple_elems.extend(arg_exprs);
                            CoreExpr::Tuple(tuple_elems)
                        } else {
                            // Module:function call
                            CoreExpr::Call(parts[0].clone(), last.clone(), arg_exprs)
                        }
                    }
                    _ => {
                        let func_expr = self.translate_expr(func);
                        CoreExpr::Apply(Box::new(func_expr), arg_exprs)
                    }
                }
            }

            Expr::Lambda(params, _, body, _) => {
                let param_names: Vec<String> =
                    params.iter().map(|p| self.to_core_var(&p.name)).collect();
                let body_expr = self.translate_expr(body);
                CoreExpr::Fun(param_names, Box::new(body_expr))
            }

            Expr::Spawn(thunk, _) => {
                let thunk_expr = self.translate_expr(thunk);
                // spawn/1 takes a fun() -> any()
                CoreExpr::Call("erlang".into(), "spawn".into(), vec![thunk_expr])
            }

            Expr::Send(pid, msg, _) => {
                let pid_expr = self.translate_expr(pid);
                let msg_expr = self.translate_expr(msg);
                // Use erlang:send/2 which is equivalent to Pid ! Msg
                CoreExpr::Call("erlang".into(), "send".into(), vec![pid_expr, msg_expr])
            }

            Expr::Receive { arms, timeout, .. } => {
                let clauses: Vec<CoreClause> = arms
                    .iter()
                    .map(|arm| {
                        let pattern = self.translate_pattern(&arm.pattern);
                        let guard = arm
                            .guard
                            .as_ref()
                            .map(|g| self.translate_expr(g))
                            .unwrap_or(CoreExpr::Lit(CoreLit::Atom("true".into())));
                        let body = self.translate_expr(&arm.body);
                        CoreClause {
                            patterns: vec![pattern],
                            guard,
                            body,
                        }
                    })
                    .collect();

                let timeout_expr = timeout
                    .as_ref()
                    .map(|(ms, body)| (self.translate_expr(ms), self.translate_expr(body)));

                CoreExpr::Receive {
                    clauses,
                    timeout: timeout_expr.map(|(ms, body)| (Box::new(ms), Box::new(body))),
                }
            }

            Expr::SelfPid(_) => CoreExpr::Call("erlang".into(), "self".into(), vec![]),

            Expr::Try {
                body, catch_arms, ..
            } => {
                let translated_body = self.translate_expr(body);
                let success_var = self.fresh_var();
                let class_var = self.fresh_var();
                let reason_var = self.fresh_var();
                let stack_var = self.fresh_var();

                // Build catch body: case on {class, reason} to match catch arms
                let catch_body = if catch_arms.is_empty() {
                    // No catch arms - re-raise
                    CoreExpr::Primop(
                        "raise".into(),
                        vec![
                            CoreExpr::Var(stack_var.clone()),
                            CoreExpr::Var(reason_var.clone()),
                        ],
                    )
                } else {
                    // Build case expression for catch arms
                    let scrutinee = CoreExpr::Tuple(vec![
                        CoreExpr::Var(class_var.clone()),
                        CoreExpr::Var(reason_var.clone()),
                    ]);

                    let mut clauses: Vec<CoreClause> = catch_arms
                        .iter()
                        .map(|arm| {
                            let class_pattern = if let Some(ref class) = arm.class {
                                CorePattern::Lit(CoreLit::Atom(class.clone()))
                            } else {
                                CorePattern::Var("_".into())
                            };
                            let reason_pattern = self.translate_pattern(&arm.pattern);
                            CoreClause {
                                patterns: vec![CorePattern::Tuple(vec![
                                    class_pattern,
                                    reason_pattern,
                                ])],
                                guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                body: self.translate_expr(&arm.body),
                            }
                        })
                        .collect();

                    // Add fallback clause that re-raises
                    clauses.push(CoreClause {
                        patterns: vec![CorePattern::Tuple(vec![
                            CorePattern::Var("_Class".into()),
                            CorePattern::Var("_Reason".into()),
                        ])],
                        guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                        body: CoreExpr::Primop(
                            "raise".into(),
                            vec![
                                CoreExpr::Var(stack_var.clone()),
                                CoreExpr::Var(reason_var.clone()),
                            ],
                        ),
                    });

                    CoreExpr::Case(Box::new(scrutinee), clauses)
                };

                CoreExpr::Try {
                    body: Box::new(translated_body),
                    vars: vec![success_var.clone()],
                    handler: Box::new(CoreExpr::Var(success_var)),
                    evars: vec![class_var, reason_var, stack_var],
                    catch: Box::new(catch_body),
                }
            }

            Expr::Return(expr, _) => expr
                .as_ref()
                .map(|e| self.translate_expr(e))
                .unwrap_or(CoreExpr::Lit(CoreLit::Atom("ok".into()))),

            Expr::Index(container, key, _) => {
                let container_expr = self.translate_expr(container);
                let key_expr = self.translate_expr(key);
                // Use maps:get for map access (also works for lists with integer keys via lists:nth)
                CoreExpr::Call("maps".into(), "get".into(), vec![key_expr, container_expr])
            }

            Expr::Field(obj, field, _) => {
                // Field access - translate to maps:get(field, obj)
                // Structs and records are translated to maps
                let obj_expr = self.translate_expr(obj);
                CoreExpr::Call(
                    "maps".into(),
                    "get".into(),
                    vec![CoreExpr::Lit(CoreLit::Atom(field.clone())), obj_expr],
                )
            }

            Expr::Path(parts, _) => {
                // Path like Foo::Bar - could be a constructor or module path
                if parts.len() == 1 {
                    CoreExpr::Var(self.to_core_var(&parts[0]))
                } else {
                    // Enum constructor - translate to tuple {tag}
                    // e.g., Message::Inc becomes {inc}
                    let tag = parts.last().unwrap();
                    CoreExpr::Tuple(vec![CoreExpr::Lit(CoreLit::Atom(to_snake_case(tag)))])
                }
            }

            Expr::MethodCall(obj, method, args, _) => {
                // Method calls become function calls with obj as first arg
                let obj_expr = self.translate_expr(obj);
                let mut all_args = vec![obj_expr];
                all_args.extend(args.iter().map(|a| self.translate_expr(a)));

                CoreExpr::Apply(
                    Box::new(CoreExpr::Var(format!("'{}'/{}", method, all_args.len()))),
                    all_args,
                )
            }

            Expr::BitString(elements, _) => self.translate_bitstring(elements),

            Expr::StructInit(_name, fields, _) => {
                // Translate struct to map with atom keys
                let entries: Vec<(CoreExpr, CoreExpr)> = fields
                    .iter()
                    .map(|(name, expr)| {
                        (
                            CoreExpr::Lit(CoreLit::Atom(name.clone())),
                            self.translate_expr(expr),
                        )
                    })
                    .collect();
                CoreExpr::Map(entries)
            }

            Expr::Record(fields, _) => {
                // Translate record to map
                let entries: Vec<CoreExpr> = fields
                    .iter()
                    .flat_map(|(name, expr)| {
                        vec![
                            CoreExpr::Lit(CoreLit::Atom(name.clone())),
                            self.translate_expr(expr),
                        ]
                    })
                    .collect();

                CoreExpr::Call(
                    "maps".into(),
                    "from_list".into(),
                    vec![self.build_list(entries)],
                )
            }
        }
    }

    fn translate_block(&mut self, stmts: &[Stmt], final_expr: Option<&Expr>) -> CoreExpr {
        let mut result_parts: Vec<(LetPart, CoreExpr)> = Vec::new();

        for stmt in stmts {
            match stmt {
                Stmt::Let(pattern, _, init, _) => {
                    let init_expr = self.translate_expr(init);
                    match pattern {
                        Pattern::Var(name, _) => {
                            // Simple variable binding
                            result_parts.push((LetPart::Simple(self.to_core_var(name)), init_expr));
                        }
                        Pattern::Wildcard(_) => {
                            // Wildcard - just evaluate for side effects
                            let var = self.fresh_var();
                            result_parts.push((LetPart::Simple(var), init_expr));
                        }
                        _ => {
                            // Pattern destructuring - use case expression
                            let core_pattern = self.translate_pattern(pattern);
                            result_parts.push((LetPart::Pattern(core_pattern), init_expr));
                        }
                    }
                }
                Stmt::Expr(expr) => {
                    // Expression statement - bind to throwaway var
                    let var = self.fresh_var();
                    let expr = self.translate_expr(expr);
                    result_parts.push((LetPart::Simple(var), expr));
                }
            }
        }

        let body = final_expr
            .map(|e| self.translate_expr(e))
            .unwrap_or(CoreExpr::Lit(CoreLit::Atom("ok".into())));

        if result_parts.is_empty() {
            body
        } else {
            // Build nested lets/cases from inside out
            result_parts
                .into_iter()
                .rev()
                .fold(body, |acc, (part, expr)| {
                    match part {
                        LetPart::Simple(name) => CoreExpr::Let(vec![(name, expr)], Box::new(acc)),
                        LetPart::Pattern(pattern) => {
                            // Use case expression for pattern destructuring
                            CoreExpr::Case(
                                Box::new(expr),
                                vec![CoreClause {
                                    patterns: vec![pattern],
                                    guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                    body: acc,
                                }],
                            )
                        }
                    }
                })
        }
    }

    fn translate_pattern(&mut self, pattern: &Pattern) -> CorePattern {
        match pattern {
            Pattern::Wildcard(_) => CorePattern::Var("_".into()),
            Pattern::Var(name, _) => CorePattern::Var(self.to_core_var(name)),
            Pattern::Int(n, _) => CorePattern::Lit(CoreLit::Int(*n)),
            Pattern::Float(f, _) => CorePattern::Lit(CoreLit::Float(*f)),
            Pattern::Char(c, _) => CorePattern::Lit(CoreLit::Int(*c as i64)),
            Pattern::String(s, _) => CorePattern::Binary(self.string_pattern_segments(s)),
            Pattern::Bool(b, _) => {
                CorePattern::Lit(CoreLit::Atom(if *b { "true" } else { "false" }.into()))
            }
            Pattern::Atom(a, _) => CorePattern::Lit(CoreLit::Atom(a.clone())),

            Pattern::Tuple(pats, _) => {
                // Empty tuple () is unit type, translate to 'ok' atom to match expression
                if pats.is_empty() {
                    return CorePattern::Lit(CoreLit::Atom("ok".into()));
                }
                let patterns: Vec<CorePattern> =
                    pats.iter().map(|p| self.translate_pattern(p)).collect();
                CorePattern::Tuple(patterns)
            }

            Pattern::List(pats, tail, _) => {
                let tail_pat = tail
                    .as_ref()
                    .map(|t| self.translate_pattern(t))
                    .unwrap_or(CorePattern::Nil);

                pats.iter().rev().fold(tail_pat, |acc, pat| {
                    CorePattern::Cons(Box::new(self.translate_pattern(pat)), Box::new(acc))
                })
            }

            Pattern::Constructor(path, fields, _) => {
                // Constructor pattern - translate to tuple {tag, field1, field2, ...}
                // e.g., Message::Get(sender) -> {get, Sender}
                let tag = path.last().unwrap();
                let mut patterns = vec![CorePattern::Lit(CoreLit::Atom(to_snake_case(tag)))];
                patterns.extend(fields.iter().map(|p| self.translate_pattern(p)));
                CorePattern::Tuple(patterns)
            }

            Pattern::Record(fields, _) => {
                // Record pattern - for now just match as map
                // This is simplified - real impl needs proper map patterns
                let mut patterns = Vec::new();
                for (name, pat) in fields {
                    patterns.push(CorePattern::Tuple(vec![
                        CorePattern::Lit(CoreLit::Atom(name.clone())),
                        self.translate_pattern(pat),
                    ]));
                }
                CorePattern::Tuple(patterns)
            }
            Pattern::BitString(segments, _) => {
                CorePattern::Binary(self.translate_bitstring_pattern_segments(segments))
            }

            Pattern::Or(left, _right, _) => {
                // Or-patterns need special handling in Core Erlang
                // For now, just use the left pattern (simplified)
                self.translate_pattern(left)
            }
        }
    }

    fn string_literal_expr(&self, s: &str) -> CoreExpr {
        CoreExpr::Binary(self.string_expr_segments(s))
    }

    fn string_expr_segments(&self, s: &str) -> Vec<CoreBinarySegment> {
        s.as_bytes()
            .iter()
            .map(|byte| CoreBinarySegment {
                value: CoreExpr::Lit(CoreLit::Int(i64::from(*byte))),
                size: CoreBinarySize::Bits(8),
                kind: CoreBinaryKind::Integer,
            })
            .collect()
    }

    fn string_pattern_segments(&self, s: &str) -> Vec<CoreBinaryPatternSegment> {
        s.as_bytes()
            .iter()
            .map(|byte| CoreBinaryPatternSegment {
                pattern: CorePattern::Lit(CoreLit::Int(i64::from(*byte))),
                size: CoreBinarySize::Bits(8),
                kind: CoreBinaryKind::Integer,
            })
            .collect()
    }

    fn binary_to_list_expr(&self, expr: CoreExpr) -> CoreExpr {
        CoreExpr::Call("erlang".into(), "binary_to_list".into(), vec![expr])
    }

    fn translate_bitstring(&mut self, segments: &[ast::BitStringSegment]) -> CoreExpr {
        let mut out = Vec::new();
        for segment in segments {
            out.extend(self.translate_bitstring_segment(segment));
        }
        CoreExpr::Binary(out)
    }

    fn translate_bitstring_segment(
        &mut self,
        segment: &ast::BitStringSegment,
    ) -> Vec<CoreBinarySegment> {
        if segment.size.is_none()
            && matches!(
                segment.value,
                Expr::String(_, _) | Expr::InterpolatedString(_, _) | Expr::BitString(_, _)
            )
            && matches!(segment.specifier, ast::BinarySegmentSpecifier::Integer)
        {
            return vec![CoreBinarySegment {
                value: self.translate_expr(&segment.value),
                size: CoreBinarySize::All,
                kind: CoreBinaryKind::Binary,
            }];
        }

        let kind = match segment.specifier {
            ast::BinarySegmentSpecifier::Integer => CoreBinaryKind::Integer,
            ast::BinarySegmentSpecifier::BigInteger => CoreBinaryKind::BigInteger,
            ast::BinarySegmentSpecifier::Binary => CoreBinaryKind::Binary,
            ast::BinarySegmentSpecifier::Utf8 => CoreBinaryKind::Utf8,
        };
        let size = match kind {
            CoreBinaryKind::Binary => segment
                .size
                .map(CoreBinarySize::Bits)
                .unwrap_or(CoreBinarySize::All),
            CoreBinaryKind::Utf8 => CoreBinarySize::Bits(segment.size.unwrap_or(8)),
            CoreBinaryKind::Integer => CoreBinarySize::Bits(segment.size.unwrap_or(8)),
            CoreBinaryKind::BigInteger => CoreBinarySize::Bits(segment.size.unwrap_or(8)),
        };
        vec![CoreBinarySegment {
            value: self.translate_expr(&segment.value),
            size,
            kind,
        }]
    }

    fn translate_bitstring_pattern_segments(
        &mut self,
        segments: &[ast::BitStringPatternSegment],
    ) -> Vec<CoreBinaryPatternSegment> {
        let mut out = Vec::new();
        for (index, segment) in segments.iter().enumerate() {
            let is_tail_binary = segment.size.is_none()
                && index + 1 == segments.len()
                && matches!(segment.value, Pattern::Var(_, _) | Pattern::Wildcard(_));
            let is_binary_segment =
                matches!(segment.specifier, ast::BinarySegmentSpecifier::Binary)
                    || is_tail_binary
                    || (segment.size.is_none()
                        && matches!(
                            segment.value,
                            Pattern::String(_, _) | Pattern::BitString(_, _)
                        ));
            let kind = match segment.specifier {
                ast::BinarySegmentSpecifier::Integer => {
                    if is_binary_segment {
                        CoreBinaryKind::Binary
                    } else {
                        CoreBinaryKind::Integer
                    }
                }
                ast::BinarySegmentSpecifier::BigInteger => CoreBinaryKind::BigInteger,
                ast::BinarySegmentSpecifier::Binary => CoreBinaryKind::Binary,
                ast::BinarySegmentSpecifier::Utf8 => CoreBinaryKind::Utf8,
            };

            out.push(CoreBinaryPatternSegment {
                pattern: self.translate_pattern(&segment.value),
                size: match kind {
                    CoreBinaryKind::Binary => segment
                        .size
                        .map(CoreBinarySize::Bits)
                        .unwrap_or(CoreBinarySize::All),
                    _ => CoreBinarySize::Bits(segment.size.unwrap_or(8)),
                },
                kind,
            });
        }
        out
    }

    fn to_core_var(&self, name: &str) -> String {
        // Core Erlang variables must start with uppercase
        let mut chars: Vec<char> = name.chars().collect();
        if let Some(first) = chars.first_mut() {
            *first = first.to_ascii_uppercase();
        }
        chars.into_iter().collect()
    }

    fn build_list(&self, exprs: Vec<CoreExpr>) -> CoreExpr {
        exprs
            .into_iter()
            .rev()
            .fold(CoreExpr::Lit(CoreLit::Nil), |acc, e| {
                CoreExpr::Cons(Box::new(e), Box::new(acc))
            })
    }

    /// Translate a list comprehension to Core Erlang
    /// [expr for x in list if cond] becomes:
    /// lists:filtermap(fun(X) -> case Cond of true -> {true, Expr}; false -> false end end, List)
    fn translate_list_comp(
        &mut self,
        expr: &Expr,
        generators: &[crate::syntax::ast::Generator],
        filters: &[Expr],
    ) -> CoreExpr {
        // For simplicity, handle single generator case with lists:filtermap
        // Multiple generators use nested flatmap

        if generators.is_empty() {
            // No generators - just return a list with the expr
            return CoreExpr::Cons(
                Box::new(self.translate_expr(expr)),
                Box::new(CoreExpr::Lit(CoreLit::Nil)),
            );
        }

        // Build from innermost generator outward
        self.translate_list_comp_inner(expr, generators, filters, 0, false)
    }

    fn translate_list_comp_inner(
        &mut self,
        expr: &Expr,
        generators: &[crate::syntax::ast::Generator],
        filters: &[Expr],
        gen_idx: usize,
        use_map: bool, // true if we can use map (single generator, no filters)
    ) -> CoreExpr {
        if gen_idx >= generators.len() {
            // All generators processed - apply filters and return expr
            let translated_expr = self.translate_expr(expr);

            if filters.is_empty() {
                if use_map {
                    // Just return the expression directly (used with lists:map)
                    translated_expr
                } else {
                    // Return expression in a singleton list (used with lists:flatmap)
                    CoreExpr::Cons(
                        Box::new(translated_expr),
                        Box::new(CoreExpr::Lit(CoreLit::Nil)),
                    )
                }
            } else {
                // Build combined filter condition
                let mut combined_filter = self.translate_expr(&filters[0]);
                for filter in &filters[1..] {
                    let f = self.translate_expr(filter);
                    combined_filter =
                        CoreExpr::Call("erlang".into(), "and".into(), vec![combined_filter, f]);
                }

                // case Filter of true -> [Expr]; false -> [] end
                CoreExpr::Case(
                    Box::new(combined_filter),
                    vec![
                        CoreClause {
                            patterns: vec![CorePattern::Lit(CoreLit::Atom("true".into()))],
                            guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                            body: CoreExpr::Cons(
                                Box::new(translated_expr),
                                Box::new(CoreExpr::Lit(CoreLit::Nil)),
                            ),
                        },
                        CoreClause {
                            patterns: vec![CorePattern::Lit(CoreLit::Atom("false".into()))],
                            guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                            body: CoreExpr::Lit(CoreLit::Nil),
                        },
                    ],
                )
            }
        } else {
            // Process this generator
            let generator = &generators[gen_idx];
            let source = self.translate_expr(&generator.source);
            let pattern = self.translate_pattern(&generator.pattern);

            // Check if this is the simple case: single generator, single variable pattern, no filters
            let is_simple_map = generators.len() == 1
                && filters.is_empty()
                && matches!(&generator.pattern, Pattern::Var(_, _));

            // Recurse for inner generators
            let inner = self.translate_list_comp_inner(
                expr,
                generators,
                filters,
                gen_idx + 1,
                is_simple_map,
            );

            if is_simple_map {
                // Simple case: [expr for x in list] -> lists:map(fun(X) -> Expr end, List)
                let param_var = self.to_core_var(match &generator.pattern {
                    Pattern::Var(name, _) => name,
                    _ => unreachable!(),
                });
                let lambda = CoreExpr::Fun(vec![param_var], Box::new(inner));
                CoreExpr::Call("lists".into(), "map".into(), vec![lambda, source])
            } else {
                // Complex case: use flatmap with pattern matching
                let param_var = self.fresh_var();
                let lambda_body = CoreExpr::Case(
                    Box::new(CoreExpr::Var(param_var.clone())),
                    vec![
                        CoreClause {
                            patterns: vec![pattern],
                            guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                            body: inner,
                        },
                        // Default case for non-matching patterns (skip)
                        CoreClause {
                            patterns: vec![CorePattern::Var("_".into())],
                            guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                            body: CoreExpr::Lit(CoreLit::Nil),
                        },
                    ],
                );
                let lambda = CoreExpr::Fun(vec![param_var], Box::new(lambda_body));
                CoreExpr::Call("lists".into(), "flatmap".into(), vec![lambda, source])
            }
        }
    }

    fn checked_dynamic_cast(
        &mut self,
        expr: CoreExpr,
        predicate_module: &str,
        predicate_function: &str,
        expected_kind: &str,
    ) -> CoreExpr {
        let tmp = self.fresh_var();
        let value = CoreExpr::Var(tmp.clone());
        let predicate = CoreExpr::Call(
            predicate_module.to_string(),
            predicate_function.to_string(),
            vec![value.clone()],
        );
        CoreExpr::Let(
            vec![(tmp.clone(), expr)],
            Box::new(CoreExpr::Case(
                Box::new(predicate),
                vec![
                    CoreClause {
                        patterns: vec![CorePattern::Lit(CoreLit::Atom("true".into()))],
                        guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                        body: value.clone(),
                    },
                    CoreClause {
                        patterns: vec![CorePattern::Lit(CoreLit::Atom("false".into()))],
                        guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                        body: CoreExpr::Call(
                            "erlang".into(),
                            "error".into(),
                            vec![CoreExpr::Tuple(vec![
                                CoreExpr::Lit(CoreLit::Atom("bad_dynamic".into())),
                                CoreExpr::Lit(CoreLit::Atom(expected_kind.into())),
                                value,
                            ])],
                        ),
                    },
                ],
            )),
        )
    }

    fn checked_dynamic_result_cast(
        &mut self,
        expr: CoreExpr,
        predicate_module: &str,
        predicate_function: &str,
        expected_kind: &str,
    ) -> CoreExpr {
        let tmp = self.fresh_var();
        let value = CoreExpr::Var(tmp.clone());
        let predicate = CoreExpr::Call(
            predicate_module.to_string(),
            predicate_function.to_string(),
            vec![value.clone()],
        );
        CoreExpr::Let(
            vec![(tmp, expr)],
            Box::new(CoreExpr::Case(
                Box::new(predicate),
                vec![
                    CoreClause {
                        patterns: vec![CorePattern::Lit(CoreLit::Atom("true".into()))],
                        guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                        body: self.dynamic_result_ok(value),
                    },
                    CoreClause {
                        patterns: vec![CorePattern::Lit(CoreLit::Atom("false".into()))],
                        guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                        body: self.dynamic_result_err(self.decode_error_expected(expected_kind)),
                    },
                ],
            )),
        )
    }

    fn dynamic_result_ok(&self, value: CoreExpr) -> CoreExpr {
        CoreExpr::Tuple(vec![CoreExpr::Lit(CoreLit::Atom("ok".into())), value])
    }

    fn dynamic_option_some(&self, value: CoreExpr) -> CoreExpr {
        CoreExpr::Tuple(vec![CoreExpr::Lit(CoreLit::Atom("some".into())), value])
    }

    fn dynamic_option_none(&self) -> CoreExpr {
        CoreExpr::Tuple(vec![CoreExpr::Lit(CoreLit::Atom("none".into()))])
    }

    fn decode_path_field(&self, field: CoreExpr) -> CoreExpr {
        CoreExpr::Tuple(vec![CoreExpr::Lit(CoreLit::Atom("field".into())), field])
    }

    fn decode_path_index(&self, index: CoreExpr) -> CoreExpr {
        CoreExpr::Tuple(vec![CoreExpr::Lit(CoreLit::Atom("index".into())), index])
    }

    fn decode_error_expected(&self, kind: &str) -> CoreExpr {
        CoreExpr::Tuple(vec![
            CoreExpr::Lit(CoreLit::Atom("expected".into())),
            self.string_literal_expr(kind),
        ])
    }

    fn decode_error_missing_field(&self, field: CoreExpr) -> CoreExpr {
        CoreExpr::Tuple(vec![
            CoreExpr::Lit(CoreLit::Atom("missing_field".into())),
            field,
        ])
    }

    fn decode_error_missing_key(&self) -> CoreExpr {
        CoreExpr::Tuple(vec![CoreExpr::Lit(CoreLit::Atom("missing_key".into()))])
    }

    fn decode_error_index_out_of_bounds(&self, index: CoreExpr) -> CoreExpr {
        CoreExpr::Tuple(vec![
            CoreExpr::Lit(CoreLit::Atom("index_out_of_bounds".into())),
            index,
        ])
    }

    fn decode_error_invalid_json(&self) -> CoreExpr {
        CoreExpr::Tuple(vec![CoreExpr::Lit(CoreLit::Atom("invalid_json".into()))])
    }

    fn decode_error_at(&self, path: CoreExpr, error: CoreExpr) -> CoreExpr {
        CoreExpr::Tuple(vec![CoreExpr::Lit(CoreLit::Atom("at".into())), path, error])
    }

    fn decode_error_one_of(&self, left: CoreExpr, right: CoreExpr) -> CoreExpr {
        CoreExpr::Tuple(vec![
            CoreExpr::Lit(CoreLit::Atom("one_of".into())),
            left,
            right,
        ])
    }

    fn dynamic_result_err(&self, error: CoreExpr) -> CoreExpr {
        CoreExpr::Tuple(vec![CoreExpr::Lit(CoreLit::Atom("err".into())), error])
    }

    fn dynamic_result_try(&mut self, expr: CoreExpr, error_expr: CoreExpr) -> CoreExpr {
        let success_var = self.fresh_var();
        let class_var = self.fresh_var();
        let reason_var = self.fresh_var();
        let stack_var = self.fresh_var();

        CoreExpr::Try {
            body: Box::new(expr),
            vars: vec![success_var.clone()],
            handler: Box::new(self.dynamic_result_ok(CoreExpr::Var(success_var))),
            evars: vec![class_var, reason_var, stack_var],
            catch: Box::new(self.dynamic_result_err(error_expr)),
        }
    }

    fn translate_decode_field(&mut self, target: CoreExpr, key: CoreExpr) -> CoreExpr {
        let checked_map = self.checked_dynamic_result_cast(target, "erlang", "is_map", "map");
        let map_var = self.fresh_var();
        let value_var = self.fresh_var();
        let missing_key = key.clone();
        let find = CoreExpr::Call(
            "maps".to_string(),
            "find".to_string(),
            vec![key, CoreExpr::Var(map_var.clone())],
        );
        CoreExpr::Case(
            Box::new(checked_map),
            vec![
                CoreClause {
                    patterns: vec![CorePattern::Tuple(vec![
                        CorePattern::Lit(CoreLit::Atom("ok".into())),
                        CorePattern::Var(map_var.clone()),
                    ])],
                    guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                    body: CoreExpr::Case(
                        Box::new(find),
                        vec![
                            CoreClause {
                                patterns: vec![CorePattern::Tuple(vec![
                                    CorePattern::Lit(CoreLit::Atom("ok".into())),
                                    CorePattern::Var(value_var.clone()),
                                ])],
                                guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                body: self.dynamic_result_ok(CoreExpr::Var(value_var)),
                            },
                            CoreClause {
                                patterns: vec![CorePattern::Lit(CoreLit::Atom("error".into()))],
                                guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                body: self.dynamic_result_err(
                                    self.decode_error_missing_field(missing_key.clone()),
                                ),
                            },
                        ],
                    ),
                },
                CoreClause {
                    patterns: vec![CorePattern::Tuple(vec![
                        CorePattern::Lit(CoreLit::Atom("err".into())),
                        CorePattern::Var("_Reason".into()),
                    ])],
                    guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                    body: self.dynamic_result_err(self.decode_error_expected("map")),
                },
            ],
        )
    }

    fn translate_decode_optional_field(
        &mut self,
        target: CoreExpr,
        key: CoreExpr,
        decoder: CoreExpr,
    ) -> CoreExpr {
        let checked_map = self.checked_dynamic_result_cast(target, "erlang", "is_map", "map");
        let map_var = self.fresh_var();
        let value_var = self.fresh_var();
        let decoded_var = self.fresh_var();
        let ok_var = self.fresh_var();
        let err_var = self.fresh_var();
        let is_null_var = self.fresh_var();
        let field_key = key.clone();
        let find = CoreExpr::Call(
            "maps".to_string(),
            "find".to_string(),
            vec![key, CoreExpr::Var(map_var.clone())],
        );

        CoreExpr::Case(
            Box::new(checked_map),
            vec![
                CoreClause {
                    patterns: vec![CorePattern::Tuple(vec![
                        CorePattern::Lit(CoreLit::Atom("ok".into())),
                        CorePattern::Var(map_var.clone()),
                    ])],
                    guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                    body: CoreExpr::Case(
                        Box::new(find),
                        vec![
                            CoreClause {
                                patterns: vec![CorePattern::Tuple(vec![
                                    CorePattern::Lit(CoreLit::Atom("ok".into())),
                                    CorePattern::Var(value_var.clone()),
                                ])],
                                guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                body: CoreExpr::Let(
                                    vec![(
                                        is_null_var.clone(),
                                        CoreExpr::Call(
                                            "erlang".to_string(),
                                            "=:=".to_string(),
                                            vec![
                                                CoreExpr::Var(value_var.clone()),
                                                CoreExpr::Lit(CoreLit::Atom("null".into())),
                                            ],
                                        ),
                                    )],
                                    Box::new(CoreExpr::Case(
                                        Box::new(CoreExpr::Var(is_null_var)),
                                        vec![
                                            CoreClause {
                                                patterns: vec![CorePattern::Lit(CoreLit::Atom(
                                                    "true".into(),
                                                ))],
                                                guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                                body: self
                                                    .dynamic_result_ok(self.dynamic_option_none()),
                                            },
                                            CoreClause {
                                                patterns: vec![CorePattern::Lit(CoreLit::Atom(
                                                    "false".into(),
                                                ))],
                                                guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                                body: CoreExpr::Let(
                                                    vec![(
                                                        decoded_var.clone(),
                                                        CoreExpr::Apply(
                                                            Box::new(decoder),
                                                            vec![CoreExpr::Var(value_var.clone())],
                                                        ),
                                                    )],
                                                    Box::new(CoreExpr::Case(
                                                        Box::new(CoreExpr::Var(decoded_var)),
                                                        vec![
                                                            CoreClause {
                                                                patterns: vec![CorePattern::Tuple(
                                                                    vec![
                                                                        CorePattern::Lit(
                                                                            CoreLit::Atom(
                                                                                "ok".into(),
                                                                            ),
                                                                        ),
                                                                        CorePattern::Var(
                                                                            ok_var.clone(),
                                                                        ),
                                                                    ],
                                                                )],
                                                                guard: CoreExpr::Lit(
                                                                    CoreLit::Atom("true".into()),
                                                                ),
                                                                body: self.dynamic_result_ok(
                                                                    self.dynamic_option_some(
                                                                        CoreExpr::Var(
                                                                            ok_var.clone(),
                                                                        ),
                                                                    ),
                                                                ),
                                                            },
                                                            CoreClause {
                                                                patterns: vec![CorePattern::Tuple(
                                                                    vec![
                                                                        CorePattern::Lit(
                                                                            CoreLit::Atom(
                                                                                "err".into(),
                                                                            ),
                                                                        ),
                                                                        CorePattern::Var(
                                                                            err_var.clone(),
                                                                        ),
                                                                    ],
                                                                )],
                                                                guard: CoreExpr::Lit(
                                                                    CoreLit::Atom("true".into()),
                                                                ),
                                                                body: self.dynamic_result_err(
                                                                    self.decode_error_at(
                                                                        self.decode_path_field(
                                                                            field_key.clone(),
                                                                        ),
                                                                        CoreExpr::Var(
                                                                            err_var.clone(),
                                                                        ),
                                                                    ),
                                                                ),
                                                            },
                                                        ],
                                                    )),
                                                ),
                                            },
                                        ],
                                    )),
                                ),
                            },
                            CoreClause {
                                patterns: vec![CorePattern::Lit(CoreLit::Atom("error".into()))],
                                guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                body: self.dynamic_result_ok(self.dynamic_option_none()),
                            },
                        ],
                    ),
                },
                CoreClause {
                    patterns: vec![CorePattern::Tuple(vec![
                        CorePattern::Lit(CoreLit::Atom("err".into())),
                        CorePattern::Var(err_var.clone()),
                    ])],
                    guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                    body: self.dynamic_result_err(CoreExpr::Var(err_var.clone())),
                },
            ],
        )
    }

    fn translate_decode_field_or(
        &mut self,
        target: CoreExpr,
        key: CoreExpr,
        fallback: CoreExpr,
        decoder: CoreExpr,
    ) -> CoreExpr {
        let checked_map = self.checked_dynamic_result_cast(target, "erlang", "is_map", "map");
        let map_var = self.fresh_var();
        let value_var = self.fresh_var();
        let decoded_var = self.fresh_var();
        let ok_var = self.fresh_var();
        let err_var = self.fresh_var();
        let field_key = key.clone();
        let find = CoreExpr::Call(
            "maps".to_string(),
            "find".to_string(),
            vec![key, CoreExpr::Var(map_var.clone())],
        );

        CoreExpr::Case(
            Box::new(checked_map),
            vec![
                CoreClause {
                    patterns: vec![CorePattern::Tuple(vec![
                        CorePattern::Lit(CoreLit::Atom("ok".into())),
                        CorePattern::Var(map_var.clone()),
                    ])],
                    guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                    body: CoreExpr::Case(
                        Box::new(find),
                        vec![
                            CoreClause {
                                patterns: vec![CorePattern::Tuple(vec![
                                    CorePattern::Lit(CoreLit::Atom("ok".into())),
                                    CorePattern::Var(value_var.clone()),
                                ])],
                                guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                body: CoreExpr::Let(
                                    vec![(
                                        decoded_var.clone(),
                                        CoreExpr::Apply(
                                            Box::new(decoder),
                                            vec![CoreExpr::Var(value_var.clone())],
                                        ),
                                    )],
                                    Box::new(CoreExpr::Case(
                                        Box::new(CoreExpr::Var(decoded_var)),
                                        vec![
                                            CoreClause {
                                                patterns: vec![CorePattern::Tuple(vec![
                                                    CorePattern::Lit(CoreLit::Atom("ok".into())),
                                                    CorePattern::Var(ok_var.clone()),
                                                ])],
                                                guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                                body: self.dynamic_result_ok(CoreExpr::Var(
                                                    ok_var.clone(),
                                                )),
                                            },
                                            CoreClause {
                                                patterns: vec![CorePattern::Tuple(vec![
                                                    CorePattern::Lit(CoreLit::Atom("err".into())),
                                                    CorePattern::Var(err_var.clone()),
                                                ])],
                                                guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                                body: self.dynamic_result_err(
                                                    self.decode_error_at(
                                                        self.decode_path_field(field_key.clone()),
                                                        CoreExpr::Var(err_var.clone()),
                                                    ),
                                                ),
                                            },
                                        ],
                                    )),
                                ),
                            },
                            CoreClause {
                                patterns: vec![CorePattern::Lit(CoreLit::Atom("error".into()))],
                                guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                body: self.dynamic_result_ok(fallback),
                            },
                        ],
                    ),
                },
                CoreClause {
                    patterns: vec![CorePattern::Tuple(vec![
                        CorePattern::Lit(CoreLit::Atom("err".into())),
                        CorePattern::Var(err_var.clone()),
                    ])],
                    guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                    body: self.dynamic_result_err(CoreExpr::Var(err_var.clone())),
                },
            ],
        )
    }

    fn translate_decode_index(&mut self, target: CoreExpr, index: CoreExpr) -> CoreExpr {
        let checked_list = self.checked_dynamic_result_cast(target, "erlang", "is_list", "list");
        let list_var = self.fresh_var();
        let failing_index = index.clone();
        CoreExpr::Case(
            Box::new(checked_list),
            vec![
                CoreClause {
                    patterns: vec![CorePattern::Tuple(vec![
                        CorePattern::Lit(CoreLit::Atom("ok".into())),
                        CorePattern::Var(list_var.clone()),
                    ])],
                    guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                    body: self.dynamic_result_try(
                        CoreExpr::Call(
                            "lists".to_string(),
                            "nth".to_string(),
                            vec![index, CoreExpr::Var(list_var)],
                        ),
                        self.decode_error_index_out_of_bounds(failing_index),
                    ),
                },
                CoreClause {
                    patterns: vec![CorePattern::Tuple(vec![
                        CorePattern::Lit(CoreLit::Atom("err".into())),
                        CorePattern::Var("_Reason".into()),
                    ])],
                    guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                    body: self.dynamic_result_err(self.decode_error_expected("list")),
                },
            ],
        )
    }

    fn translate_decode_list(&mut self, target: CoreExpr, decoder: CoreExpr) -> CoreExpr {
        let checked_list = self.checked_dynamic_result_cast(target, "erlang", "is_list", "list");
        let list_var = self.fresh_var();
        let item_var = self.fresh_var();
        let state_var = self.fresh_var();
        let current_index_var = self.fresh_var();
        let rev_tail_var = self.fresh_var();
        let decoded_var = self.fresh_var();
        let decoded_value_var = self.fresh_var();
        let err_var = self.fresh_var();
        let final_rev_var = self.fresh_var();
        let final_err_var = self.fresh_var();
        let _final_index_var = self.fresh_var();

        let step = CoreExpr::Fun(
            vec![item_var.clone(), state_var.clone()],
            Box::new(CoreExpr::Case(
                Box::new(CoreExpr::Var(state_var)),
                vec![
                    CoreClause {
                        patterns: vec![CorePattern::Tuple(vec![
                            CorePattern::Tuple(vec![
                                CorePattern::Lit(CoreLit::Atom("ok".into())),
                                CorePattern::Var(rev_tail_var.clone()),
                            ]),
                            CorePattern::Var(current_index_var.clone()),
                        ])],
                        guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                        body: CoreExpr::Let(
                            vec![(
                                decoded_var.clone(),
                                CoreExpr::Apply(
                                    Box::new(decoder),
                                    vec![CoreExpr::Var(item_var.clone())],
                                ),
                            )],
                            Box::new(CoreExpr::Case(
                                Box::new(CoreExpr::Var(decoded_var)),
                                vec![
                                    CoreClause {
                                        patterns: vec![CorePattern::Tuple(vec![
                                            CorePattern::Lit(CoreLit::Atom("ok".into())),
                                            CorePattern::Var(decoded_value_var.clone()),
                                        ])],
                                        guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                        body: CoreExpr::Tuple(vec![
                                            self.dynamic_result_ok(CoreExpr::Cons(
                                                Box::new(CoreExpr::Var(decoded_value_var)),
                                                Box::new(CoreExpr::Var(rev_tail_var)),
                                            )),
                                            CoreExpr::Call(
                                                "erlang".to_string(),
                                                "+".to_string(),
                                                vec![
                                                    CoreExpr::Var(current_index_var.clone()),
                                                    CoreExpr::Lit(CoreLit::Int(1)),
                                                ],
                                            ),
                                        ]),
                                    },
                                    CoreClause {
                                        patterns: vec![CorePattern::Tuple(vec![
                                            CorePattern::Lit(CoreLit::Atom("err".into())),
                                            CorePattern::Var(err_var.clone()),
                                        ])],
                                        guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                        body: CoreExpr::Tuple(vec![
                                            self.dynamic_result_err(self.decode_error_at(
                                                self.decode_path_index(CoreExpr::Var(
                                                    current_index_var.clone(),
                                                )),
                                                CoreExpr::Var(err_var.clone()),
                                            )),
                                            CoreExpr::Call(
                                                "erlang".to_string(),
                                                "+".to_string(),
                                                vec![
                                                    CoreExpr::Var(current_index_var.clone()),
                                                    CoreExpr::Lit(CoreLit::Int(1)),
                                                ],
                                            ),
                                        ]),
                                    },
                                ],
                            )),
                        ),
                    },
                    CoreClause {
                        patterns: vec![CorePattern::Tuple(vec![
                            CorePattern::Tuple(vec![
                                CorePattern::Lit(CoreLit::Atom("err".into())),
                                CorePattern::Var(err_var.clone()),
                            ]),
                            CorePattern::Var(current_index_var.clone()),
                        ])],
                        guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                        body: CoreExpr::Tuple(vec![
                            self.dynamic_result_err(CoreExpr::Var(err_var)),
                            CoreExpr::Call(
                                "erlang".to_string(),
                                "+".to_string(),
                                vec![
                                    CoreExpr::Var(current_index_var),
                                    CoreExpr::Lit(CoreLit::Int(1)),
                                ],
                            ),
                        ]),
                    },
                ],
            )),
        );

        CoreExpr::Case(
            Box::new(checked_list),
            vec![
                CoreClause {
                    patterns: vec![CorePattern::Tuple(vec![
                        CorePattern::Lit(CoreLit::Atom("ok".into())),
                        CorePattern::Var(list_var.clone()),
                    ])],
                    guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                    body: CoreExpr::Case(
                        Box::new(CoreExpr::Call(
                            "lists".to_string(),
                            "foldl".to_string(),
                            vec![
                                step,
                                CoreExpr::Tuple(vec![
                                    self.dynamic_result_ok(CoreExpr::Lit(CoreLit::Nil)),
                                    CoreExpr::Lit(CoreLit::Int(1)),
                                ]),
                                CoreExpr::Var(list_var),
                            ],
                        )),
                        vec![
                            CoreClause {
                                patterns: vec![CorePattern::Tuple(vec![
                                    CorePattern::Tuple(vec![
                                        CorePattern::Lit(CoreLit::Atom("ok".into())),
                                        CorePattern::Var(final_rev_var.clone()),
                                    ]),
                                    CorePattern::Var("_FinalIndex".into()),
                                ])],
                                guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                body: self.dynamic_result_ok(CoreExpr::Call(
                                    "lists".to_string(),
                                    "reverse".to_string(),
                                    vec![CoreExpr::Var(final_rev_var)],
                                )),
                            },
                            CoreClause {
                                patterns: vec![CorePattern::Tuple(vec![
                                    CorePattern::Tuple(vec![
                                        CorePattern::Lit(CoreLit::Atom("err".into())),
                                        CorePattern::Var(final_err_var.clone()),
                                    ]),
                                    CorePattern::Var("_FinalIndex".into()),
                                ])],
                                guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                body: self.dynamic_result_err(CoreExpr::Var(final_err_var)),
                            },
                        ],
                    ),
                },
                CoreClause {
                    patterns: vec![CorePattern::Tuple(vec![
                        CorePattern::Lit(CoreLit::Atom("err".into())),
                        CorePattern::Var("_Reason".into()),
                    ])],
                    guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                    body: self.dynamic_result_err(self.decode_error_expected("list")),
                },
            ],
        )
    }

    fn translate_decode_optional(&mut self, target: CoreExpr, decoder: CoreExpr) -> CoreExpr {
        let value_var = self.fresh_var();
        let decoded_var = self.fresh_var();
        let ok_var = self.fresh_var();
        let err_var = self.fresh_var();

        CoreExpr::Let(
            vec![(value_var.clone(), target)],
            Box::new(CoreExpr::Case(
                Box::new(CoreExpr::Call(
                    "erlang".to_string(),
                    "=:=".to_string(),
                    vec![
                        CoreExpr::Var(value_var.clone()),
                        CoreExpr::Lit(CoreLit::Atom("null".into())),
                    ],
                )),
                vec![
                    CoreClause {
                        patterns: vec![CorePattern::Lit(CoreLit::Atom("true".into()))],
                        guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                        body: self.dynamic_result_ok(self.dynamic_option_none()),
                    },
                    CoreClause {
                        patterns: vec![CorePattern::Lit(CoreLit::Atom("false".into()))],
                        guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                        body: CoreExpr::Let(
                            vec![(
                                decoded_var.clone(),
                                CoreExpr::Apply(
                                    Box::new(decoder),
                                    vec![CoreExpr::Var(value_var.clone())],
                                ),
                            )],
                            Box::new(CoreExpr::Case(
                                Box::new(CoreExpr::Var(decoded_var)),
                                vec![
                                    CoreClause {
                                        patterns: vec![CorePattern::Tuple(vec![
                                            CorePattern::Lit(CoreLit::Atom("ok".into())),
                                            CorePattern::Var(ok_var.clone()),
                                        ])],
                                        guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                        body: self.dynamic_result_ok(
                                            self.dynamic_option_some(CoreExpr::Var(ok_var)),
                                        ),
                                    },
                                    CoreClause {
                                        patterns: vec![CorePattern::Tuple(vec![
                                            CorePattern::Lit(CoreLit::Atom("err".into())),
                                            CorePattern::Var(err_var.clone()),
                                        ])],
                                        guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                        body: CoreExpr::Tuple(vec![
                                            CoreExpr::Lit(CoreLit::Atom("err".into())),
                                            CoreExpr::Var(err_var),
                                        ]),
                                    },
                                ],
                            )),
                        ),
                    },
                ],
            )),
        )
    }

    fn translate_decode_dict(&mut self, target: CoreExpr, decoder: CoreExpr) -> CoreExpr {
        let checked_map = self.checked_dynamic_result_cast(target, "erlang", "is_map", "map");
        let map_var = self.fresh_var();
        let key_var = self.fresh_var();
        let value_var = self.fresh_var();
        let acc_var = self.fresh_var();
        let decoded_var = self.fresh_var();
        let decoded_value_var = self.fresh_var();
        let map_acc_var = self.fresh_var();
        let err_var = self.fresh_var();

        let step = CoreExpr::Fun(
            vec![key_var.clone(), value_var.clone(), acc_var.clone()],
            Box::new(CoreExpr::Case(
                Box::new(CoreExpr::Var(acc_var.clone())),
                vec![
                    CoreClause {
                        patterns: vec![CorePattern::Tuple(vec![
                            CorePattern::Lit(CoreLit::Atom("ok".into())),
                            CorePattern::Var(map_acc_var.clone()),
                        ])],
                        guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                        body: CoreExpr::Let(
                            vec![(
                                decoded_var.clone(),
                                CoreExpr::Apply(
                                    Box::new(decoder),
                                    vec![CoreExpr::Var(value_var.clone())],
                                ),
                            )],
                            Box::new(CoreExpr::Case(
                                Box::new(CoreExpr::Var(decoded_var)),
                                vec![
                                    CoreClause {
                                        patterns: vec![CorePattern::Tuple(vec![
                                            CorePattern::Lit(CoreLit::Atom("ok".into())),
                                            CorePattern::Var(decoded_value_var.clone()),
                                        ])],
                                        guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                        body: self.dynamic_result_ok(CoreExpr::Call(
                                            "maps".to_string(),
                                            "put".to_string(),
                                            vec![
                                                CoreExpr::Var(key_var.clone()),
                                                CoreExpr::Var(decoded_value_var),
                                                CoreExpr::Var(map_acc_var),
                                            ],
                                        )),
                                    },
                                    CoreClause {
                                        patterns: vec![CorePattern::Tuple(vec![
                                            CorePattern::Lit(CoreLit::Atom("err".into())),
                                            CorePattern::Var(err_var.clone()),
                                        ])],
                                        guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                                        body: self.dynamic_result_err(self.decode_error_at(
                                            self.decode_path_field(CoreExpr::Var(key_var.clone())),
                                            CoreExpr::Var(err_var.clone()),
                                        )),
                                    },
                                ],
                            )),
                        ),
                    },
                    CoreClause {
                        patterns: vec![CorePattern::Tuple(vec![
                            CorePattern::Lit(CoreLit::Atom("err".into())),
                            CorePattern::Var(err_var.clone()),
                        ])],
                        guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                        body: self.dynamic_result_err(CoreExpr::Var(err_var)),
                    },
                ],
            )),
        );

        CoreExpr::Case(
            Box::new(checked_map),
            vec![
                CoreClause {
                    patterns: vec![CorePattern::Tuple(vec![
                        CorePattern::Lit(CoreLit::Atom("ok".into())),
                        CorePattern::Var(map_var.clone()),
                    ])],
                    guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                    body: CoreExpr::Call(
                        "maps".to_string(),
                        "fold".to_string(),
                        vec![
                            step,
                            self.dynamic_result_ok(CoreExpr::Map(vec![])),
                            CoreExpr::Var(map_var),
                        ],
                    ),
                },
                CoreClause {
                    patterns: vec![CorePattern::Tuple(vec![
                        CorePattern::Lit(CoreLit::Atom("err".into())),
                        CorePattern::Var("_Reason".into()),
                    ])],
                    guard: CoreExpr::Lit(CoreLit::Atom("true".into())),
                    body: self.dynamic_result_err(self.decode_error_expected("map")),
                },
            ],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::lexer::Lexer;
    use crate::syntax::parser::Parser;

    fn parse_module(source: &str) -> Module {
        let tokens = Lexer::new(source).tokenize();
        let mut parser = Parser::new(tokens);
        parser.parse_module().expect("module should parse")
    }

    fn function_hash(source: &str, source_name: &str, arity: usize) -> String {
        let module = parse_module(source);
        let mut translator = Translator::new();
        let translated = translator.translate_function_modules(&module);
        translated
            .metadata
            .into_iter()
            .find(|m| m.source_name == source_name && m.arity == arity)
            .map(|m| m.module_hash)
            .expect("function metadata not found")
    }

    #[test]
    fn content_address_invariance_diff_name_same_ast_same_hash() {
        let a = r#"
            fn alpha() { 42 }
        "#;
        let b = r#"
            fn beta() { 42 }
        "#;

        let hash_a = function_hash(a, "alpha", 0);
        let hash_b = function_hash(b, "beta", 0);
        assert_eq!(hash_a, hash_b);
    }

    #[test]
    fn same_name_same_call_different_callee_content_produces_different_hash() {
        let a = r#"
            fn helper() { 1 }
            fn main() { helper() }
        "#;
        let b = r#"
            fn helper() { 2 }
            fn main() { helper() }
        "#;

        let hash_a = function_hash(a, "main", 0);
        let hash_b = function_hash(b, "main", 0);
        assert_ne!(hash_a, hash_b);
    }

    #[test]
    fn parameter_rename_same_structure_same_hash() {
        let a = r#"
            fn f(x: Int) { x }
        "#;
        let b = r#"
            fn f(y: Int) { y }
        "#;

        let hash_a = function_hash(a, "f", 1);
        let hash_b = function_hash(b, "f", 1);
        assert_eq!(hash_a, hash_b);
    }

    #[test]
    fn local_rename_same_structure_same_hash() {
        let a = r#"
            fn f() {
                let x = 1
                x
            }
        "#;
        let b = r#"
            fn f() {
                let y = 1
                y
            }
        "#;

        let hash_a = function_hash(a, "f", 0);
        let hash_b = function_hash(b, "f", 0);
        assert_eq!(hash_a, hash_b);
    }

    #[test]
    fn transitive_dependency_change_produces_different_hash() {
        let a = r#"
            fn leaf() { 1 }
            fn middle() { leaf() }
            fn main() { middle() }
        "#;
        let b = r#"
            fn leaf() { 2 }
            fn middle() { leaf() }
            fn main() { middle() }
        "#;

        let hash_a = function_hash(a, "main", 0);
        let hash_b = function_hash(b, "main", 0);
        assert_ne!(hash_a, hash_b);
    }
}

impl Default for Translator {
    fn default() -> Self {
        Self::new()
    }
}
