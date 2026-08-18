#[allow(unused_imports)]
use crate::prelude::*;
use crate::collections::{HashMap, HashSet};

use crate::syntax::ast::{Item, Module};
use crate::syntax::lexer::Lexer;
use crate::syntax::parser::{ParseError, Parser, ParserOptions};
use crate::syntax::span::Span;
use crate::syntax::token::TokenKind;
use crate::types::env::TypeEnv;
use crate::types::infer::InferenceContext;
use crate::types::types::{Scheme, Type, TypeId};
use crate::types::unify::TypeError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityProfile {
    Trusted,
    Sandboxed,
}

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub security_profile: SecurityProfile,
    pub allowed_imports: Option<HashSet<String>>,
    pub allowed_extern_modules: Option<HashSet<String>>,
    pub resolved_functions: HashMap<(String, usize), ()>,
}

impl SessionConfig {
    pub fn trusted() -> Self {
        Self {
            security_profile: SecurityProfile::Trusted,
            allowed_imports: None,
            allowed_extern_modules: None,
            resolved_functions: HashMap::new(),
        }
    }

    pub fn sandboxed_default() -> Self {
        Self {
            security_profile: SecurityProfile::Sandboxed,
            allowed_imports: Some(
                ["prelude", "list", "option", "result"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
            ),
            allowed_extern_modules: Some(HashSet::new()),
            resolved_functions: HashMap::new(),
        }
    }

    pub fn capability_sandboxed() -> Self {
        let mut config = Self::sandboxed_default();
        config.allowed_extern_modules = Some(["lux_capability".to_string()].into_iter().collect());
        config
    }

    pub fn allow_extern(&self) -> bool {
        matches!(self.security_profile, SecurityProfile::Trusted)
            || self
                .allowed_extern_modules
                .as_ref()
                .is_some_and(|modules| !modules.is_empty())
    }
}

pub struct Session {
    pub errors: Vec<CompileError>,
    pub config: SessionConfig,
}

#[derive(Debug, Clone, Copy)]
struct BuiltinTypeIds {
    dynamic_result: TypeId,
    dynamic_option: TypeId,
}

#[derive(Debug)]
pub enum CompileError {
    Parse(ParseError),
    Type(TypeError),
    #[cfg(feature = "std")]
    Io(std::io::Error),
    Security(SecurityError),
}

#[derive(Debug, Clone)]
pub enum SecurityError {
    ExternDisallowed(Span),
    ExternModuleDisallowed { module: String, span: Span },
    ImportDisallowed { module: String, span: Span },
}

impl From<ParseError> for CompileError {
    fn from(e: ParseError) -> Self {
        CompileError::Parse(e)
    }
}

impl From<TypeError> for CompileError {
    fn from(e: TypeError) -> Self {
        CompileError::Type(e)
    }
}

#[cfg(feature = "std")]
impl From<std::io::Error> for CompileError {
    fn from(e: std::io::Error) -> Self {
        CompileError::Io(e)
    }
}

impl From<SecurityError> for CompileError {
    fn from(e: SecurityError) -> Self {
        CompileError::Security(e)
    }
}

impl Session {
    pub fn new() -> Self {
        Self::with_config(SessionConfig::trusted())
    }

    pub fn with_config(config: SessionConfig) -> Self {
        Session {
            errors: Vec::new(),
            config,
        }
    }

    pub fn compile_source(&mut self, source: &str) -> Result<Module, CompileError> {
        // Lex
        let tokens = Lexer::new(source).tokenize();
        for token in &tokens {
            if let TokenKind::Error(msg) = &token.kind {
                return Err(ParseError::new(format!("Lexer error: {}", msg), token.span).into());
            }
        }

        // Parse with security-aware parser settings.
        let mut parser = Parser::new_with_options(
            tokens,
            ParserOptions {
                allow_extern: self.config.allow_extern(),
            },
        );
        let module = parser.parse_module()?;

        let mut function_symbols = HashSet::new();
        let mut type_symbols = HashSet::new();
        for item in &module.items {
            let duplicate = match item {
                Item::Function(function) => (!function_symbols.insert(function.name.clone()))
                    .then_some((&function.name, function.span)),
                Item::TypeAlias(alias) => {
                    (!type_symbols.insert(alias.name.clone())).then_some((&alias.name, alias.span))
                }
                Item::Enum(definition) => (!type_symbols.insert(definition.name.clone()))
                    .then_some((&definition.name, definition.span)),
                Item::Struct(definition) => (!type_symbols.insert(definition.name.clone()))
                    .then_some((&definition.name, definition.span)),
                Item::Extern(_) | Item::Use(_) => None,
            };
            if let Some((symbol, span)) = duplicate {
                return Err(ParseError::new(
                    format!(
                        "Duplicate symbol `{symbol}`: a symbol name has exactly one definition"
                    ),
                    span,
                )
                .into());
            }
        }

        self.validate_module_security(&module)?;

        let mut ctx = InferenceContext::new();
        let builtin_types = self.register_builtin_types(&mut ctx);
        let mut env = TypeEnv::new();
        self.register_builtins(&mut env, builtin_types);
        self.register_resolved_functions(&mut env);

        // First pass: register type names so recursive/forward references can resolve.
        for item in &module.items {
            match item {
                Item::Enum(enum_def) => {
                    ctx.register_type(enum_def.name.clone(), enum_def.type_params.clone(), vec![]);
                }
                Item::Struct(struct_def) => {
                    ctx.register_struct(
                        struct_def.name.clone(),
                        struct_def.type_params.clone(),
                        vec![],
                    );
                }
                Item::TypeAlias(type_alias) => {
                    ctx.register_type_alias(
                        type_alias.name.clone(),
                        type_alias.type_params.clone(),
                        type_alias.ty.clone(),
                    );
                }
                _ => {}
            }
        }

        // Second pass: populate variant and field types.
        for item in &module.items {
            match item {
                Item::Enum(enum_def) => {
                    let mut type_params_map = HashMap::new();
                    for (i, param) in enum_def.type_params.iter().enumerate() {
                        type_params_map.insert(param.clone(), i as crate::types::types::TyVar);
                    }

                    let mut variants = Vec::new();
                    for variant in &enum_def.variants {
                        let mut field_types = Vec::new();
                        for field_ty in &variant.fields {
                            let ty = ctx.type_expr_to_type(field_ty, &type_params_map)?;
                            field_types.push(ty);
                        }
                        variants.push((variant.name.clone(), field_types));
                    }
                    ctx.register_type(
                        enum_def.name.clone(),
                        enum_def.type_params.clone(),
                        variants,
                    );
                }
                Item::Struct(struct_def) => {
                    let mut type_params_map = HashMap::new();
                    for (i, param) in struct_def.type_params.iter().enumerate() {
                        type_params_map.insert(param.clone(), i as crate::types::types::TyVar);
                    }

                    let mut fields = Vec::new();
                    for field in &struct_def.fields {
                        let ty = ctx.type_expr_to_type(&field.ty, &type_params_map)?;
                        fields.push((field.name.clone(), ty));
                    }
                    ctx.register_struct(
                        struct_def.name.clone(),
                        struct_def.type_params.clone(),
                        fields,
                    );
                }
                _ => {}
            }
        }

        // Register extern function declarations.
        for item in &module.items {
            if let Item::Extern(extern_block) = item {
                for extern_fn in &extern_block.decls {
                    let mut type_params_map = HashMap::new();
                    for (i, param) in extern_fn.type_params.iter().enumerate() {
                        type_params_map.insert(param.clone(), i as crate::types::types::TyVar);
                    }

                    let mut param_types = Vec::new();
                    for param_ty in &extern_fn.params {
                        let ty = ctx.type_expr_to_type(param_ty, &type_params_map)?;
                        param_types.push(ty);
                    }

                    let return_type =
                        ctx.type_expr_to_type(&extern_fn.return_type, &type_params_map)?;
                    ctx.register_extern_fn(
                        &extern_fn.module,
                        &extern_fn.name,
                        &extern_fn.type_params,
                        param_types,
                        return_type,
                    );
                }
            }
        }

        // Register functions in environment so they can call each other.
        for item in &module.items {
            if let Item::Function(func) = item {
                let mut param_types = Vec::new();
                for param in &func.params {
                    let ty = if let Some(ty_expr) = &param.ty {
                        ctx.type_expr_to_type(ty_expr, &HashMap::new())?
                    } else {
                        ctx.fresh_var()
                    };
                    param_types.push(ty);
                }
                let ret_type = if let Some(ret) = &func.return_type {
                    ctx.type_expr_to_type(ret, &HashMap::new())?
                } else {
                    ctx.fresh_var()
                };

                let fn_type = Type::Function(param_types, Box::new(ret_type));
                env.insert_function(func.name.clone(), Scheme::mono(fn_type));
            }
        }

        // Type check each function.
        for item in &module.items {
            if let Item::Function(func) = item {
                let mut func_env = env.clone();
                for param in &func.params {
                    let param_type = if let Some(ty) = &param.ty {
                        ctx.type_expr_to_type(ty, &HashMap::new())?
                    } else {
                        ctx.fresh_var()
                    };
                    func_env.insert(param.name.clone(), Scheme::mono(param_type));
                }

                let body_type = ctx
                    .infer_expr(&func_env, &func.body)
                    .map_err(|err| annotate_type_error(err, source, &func.name, None))?;
                if let Some(ret) = &func.return_type {
                    let ret_type = ctx.type_expr_to_type(ret, &HashMap::new())?;
                    ctx.unify(&ret_type, &body_type, func.body.span())
                        .map_err(|err| {
                            annotate_type_error(err, source, &func.name, Some(func.body.span()))
                        })?;
                }
            }
        }

        Ok(module)
    }

    fn validate_module_security(&self, module: &Module) -> Result<(), CompileError> {
        if self.config.security_profile == SecurityProfile::Sandboxed {
            for item in &module.items {
                if let Item::Extern(extern_block) = item {
                    let Some(allowed) = &self.config.allowed_extern_modules else {
                        return Err(SecurityError::ExternDisallowed(extern_block.span).into());
                    };
                    if allowed.is_empty() {
                        return Err(SecurityError::ExternDisallowed(extern_block.span).into());
                    }
                    for declaration in &extern_block.decls {
                        if !allowed.contains(&declaration.module) {
                            return Err(SecurityError::ExternModuleDisallowed {
                                module: declaration.module.clone(),
                                span: declaration.span,
                            }
                            .into());
                        }
                    }
                }
            }
        }

        if let Some(allowed) = &self.config.allowed_imports {
            for item in &module.items {
                if let Item::Use(use_decl) = item {
                    if !allowed.contains(&use_decl.module) {
                        return Err(SecurityError::ImportDisallowed {
                            module: use_decl.module.clone(),
                            span: use_decl.span,
                        }
                        .into());
                    }
                }
            }
        }

        Ok(())
    }

    fn register_builtin_types(&self, ctx: &mut InferenceContext) -> BuiltinTypeIds {
        let decode_path_item = ctx.register_type("DecodePathItem".to_string(), vec![], vec![]);
        let decode_path_item_ty =
            Type::Named(decode_path_item, "DecodePathItem".to_string(), vec![]);
        ctx.register_type(
            "DecodePathItem".to_string(),
            vec![],
            vec![
                ("Field".to_string(), vec![Type::String]),
                ("Index".to_string(), vec![Type::Int]),
            ],
        );

        let decode_error = ctx.register_type("DecodeError".to_string(), vec![], vec![]);
        let decode_error_ty = Type::Named(decode_error, "DecodeError".to_string(), vec![]);
        ctx.register_type(
            "DecodeError".to_string(),
            vec![],
            vec![
                ("Expected".to_string(), vec![Type::String]),
                ("MissingField".to_string(), vec![Type::String]),
                ("MissingKey".to_string(), vec![]),
                ("IndexOutOfBounds".to_string(), vec![Type::Int]),
                (
                    "At".to_string(),
                    vec![decode_path_item_ty.clone(), decode_error_ty.clone()],
                ),
                (
                    "OneOf".to_string(),
                    vec![decode_error_ty.clone(), decode_error_ty.clone()],
                ),
                ("InvalidJson".to_string(), vec![]),
                ("Message".to_string(), vec![Type::String]),
            ],
        );

        let value = Type::Var(0);
        let dynamic_result = ctx.register_type(
            "DynamicResult".to_string(),
            vec!["T".to_string()],
            vec![
                ("Ok".to_string(), vec![value]),
                ("Err".to_string(), vec![decode_error_ty]),
            ],
        );
        let dynamic_option = ctx.register_type(
            "DynamicOption".to_string(),
            vec!["T".to_string()],
            vec![
                ("Some".to_string(), vec![Type::Var(0)]),
                ("None".to_string(), vec![]),
            ],
        );

        BuiltinTypeIds {
            dynamic_result,
            dynamic_option,
        }
    }

    fn register_builtins(&self, env: &mut TypeEnv, builtin_types: BuiltinTypeIds) {
        let any = Type::Any;
        let dynamic = Type::Dynamic;
        let int = Type::Int;
        let float = Type::Float;
        let bool_ty = Type::Bool;
        let string = Type::String;
        let atom = Type::Atom;
        let unit = Type::Unit;
        let pid = Type::Pid;
        let ref_ty = Type::Ref;

        let dynamic_result = |value_ty: Type| {
            Type::Named(
                builtin_types.dynamic_result,
                "DynamicResult".to_string(),
                vec![value_ty],
            )
        };
        let dynamic_option = |value_ty: Type| {
            Type::Named(
                builtin_types.dynamic_option,
                "DynamicOption".to_string(),
                vec![value_ty],
            )
        };

        fn register_builtin(
            env: /*unused*/ &mut TypeEnv,
            name: &str,
            params: Vec<Type>,
            ret: Type,
        ) {
            let fn_type = Type::Function(params, Box::new(ret));
            env.insert_function(name.to_string(), Scheme::mono(fn_type));
        }

        fn register_builtin_scheme(env: /*unused*/ &mut TypeEnv, name: &str, scheme: Scheme) {
            env.insert_function(name.to_string(), scheme);
        }

        // Pure computational built-ins available in all profiles.
        register_builtin(
            env,
            "length",
            vec![Type::List(Box::new(any.clone()))],
            int.clone(),
        );
        register_builtin(
            env,
            "hd",
            vec![Type::List(Box::new(any.clone()))],
            any.clone(),
        );
        register_builtin(
            env,
            "tl",
            vec![Type::List(Box::new(any.clone()))],
            Type::List(Box::new(any.clone())),
        );
        register_builtin(
            env,
            "reverse",
            vec![Type::List(Box::new(any.clone()))],
            Type::List(Box::new(any.clone())),
        );
        register_builtin(
            env,
            "sort",
            vec![Type::List(Box::new(any.clone()))],
            Type::List(Box::new(any.clone())),
        );
        register_builtin(
            env,
            "append",
            vec![
                Type::List(Box::new(any.clone())),
                Type::List(Box::new(any.clone())),
            ],
            Type::List(Box::new(any.clone())),
        );
        register_builtin(
            env,
            "flatten",
            vec![Type::List(Box::new(any.clone()))],
            Type::List(Box::new(any.clone())),
        );
        register_builtin(
            env,
            "take",
            vec![int.clone(), Type::List(Box::new(any.clone()))],
            Type::List(Box::new(any.clone())),
        );
        register_builtin(
            env,
            "drop",
            vec![int.clone(), Type::List(Box::new(any.clone()))],
            Type::List(Box::new(any.clone())),
        );
        register_builtin(
            env,
            "nth",
            vec![int.clone(), Type::List(Box::new(any.clone()))],
            any.clone(),
        );
        register_builtin(
            env,
            "member",
            vec![any.clone(), Type::List(Box::new(any.clone()))],
            bool_ty.clone(),
        );
        register_builtin(
            env,
            "unique",
            vec![Type::List(Box::new(any.clone()))],
            Type::List(Box::new(any.clone())),
        );
        register_builtin(
            env,
            "zip",
            vec![
                Type::List(Box::new(any.clone())),
                Type::List(Box::new(any.clone())),
            ],
            Type::List(Box::new(any.clone())),
        );
        register_builtin(
            env,
            "unzip",
            vec![Type::List(Box::new(any.clone()))],
            Type::Tuple(vec![
                Type::List(Box::new(any.clone())),
                Type::List(Box::new(any.clone())),
            ]),
        );
        register_builtin(
            env,
            "enumerate",
            vec![Type::List(Box::new(any.clone()))],
            Type::List(Box::new(any.clone())),
        );

        register_builtin(env, "abs", vec![int.clone()], int.clone());
        register_builtin(env, "max", vec![int.clone(), int.clone()], int.clone());
        register_builtin(env, "min", vec![int.clone(), int.clone()], int.clone());
        register_builtin(env, "rem", vec![int.clone(), int.clone()], int.clone());
        register_builtin(env, "div", vec![int.clone(), int.clone()], int.clone());
        register_builtin(env, "band", vec![int.clone(), int.clone()], int.clone());
        register_builtin(env, "bor", vec![int.clone(), int.clone()], int.clone());
        register_builtin(env, "bxor", vec![int.clone(), int.clone()], int.clone());
        register_builtin(env, "bnot", vec![int.clone()], int.clone());
        register_builtin(env, "bsl", vec![int.clone(), int.clone()], int.clone());
        register_builtin(env, "bsr", vec![int.clone(), int.clone()], int.clone());

        register_builtin(env, "to_string", vec![any.clone()], string.clone());
        register_builtin(env, "to_int", vec![any.clone()], int.clone());
        register_builtin(env, "to_float", vec![any.clone()], float.clone());
        register_builtin(env, "to_atom", vec![string.clone()], atom.clone());

        register_builtin(
            env,
            "fst",
            vec![Type::Tuple(vec![any.clone(), any.clone()])],
            any.clone(),
        );
        register_builtin(
            env,
            "snd",
            vec![Type::Tuple(vec![any.clone(), any.clone()])],
            any.clone(),
        );
        register_builtin(env, "elem", vec![any.clone(), int.clone()], any.clone());
        register_builtin(
            env,
            "set_elem",
            vec![any.clone(), int.clone(), any.clone()],
            any.clone(),
        );
        register_builtin(
            env,
            "tuple_to_list",
            vec![any.clone()],
            Type::List(Box::new(any.clone())),
        );
        register_builtin(
            env,
            "list_to_tuple",
            vec![Type::List(Box::new(any.clone()))],
            any.clone(),
        );

        register_builtin(env, "is_int", vec![any.clone()], bool_ty.clone());
        register_builtin(env, "is_float", vec![any.clone()], bool_ty.clone());
        register_builtin(env, "is_number", vec![any.clone()], bool_ty.clone());
        register_builtin(env, "is_atom", vec![any.clone()], bool_ty.clone());
        register_builtin(env, "is_list", vec![any.clone()], bool_ty.clone());
        register_builtin(env, "is_tuple", vec![any.clone()], bool_ty.clone());
        register_builtin(env, "is_map", vec![any.clone()], bool_ty.clone());
        register_builtin(env, "is_bool", vec![any.clone()], bool_ty.clone());
        register_builtin(env, "is_nil", vec![any.clone()], bool_ty.clone());
        register_builtin(env, "is_string", vec![any.clone()], bool_ty.clone());
        register_builtin(env, "is_pid", vec![any.clone()], bool_ty.clone());
        register_builtin(env, "is_function", vec![any.clone()], bool_ty.clone());
        register_builtin(env, "is_binary", vec![any.clone()], bool_ty.clone());

        register_builtin(env, "str_length", vec![string.clone()], int.clone());
        register_builtin(
            env,
            "str_concat",
            vec![string.clone(), string.clone()],
            string.clone(),
        );
        register_builtin(
            env,
            "str_split",
            vec![string.clone(), string.clone()],
            Type::List(Box::new(string.clone())),
        );
        register_builtin(
            env,
            "str_join",
            vec![Type::List(Box::new(string.clone())), string.clone()],
            string.clone(),
        );
        register_builtin(env, "str_trim", vec![string.clone()], string.clone());
        register_builtin(env, "str_upper", vec![string.clone()], string.clone());
        register_builtin(env, "str_lower", vec![string.clone()], string.clone());
        register_builtin(
            env,
            "str_replace",
            vec![string.clone(), string.clone(), string.clone()],
            string.clone(),
        );
        register_builtin(
            env,
            "str_contains",
            vec![string.clone(), string.clone()],
            bool_ty.clone(),
        );
        register_builtin(
            env,
            "str_starts_with",
            vec![string.clone(), string.clone()],
            bool_ty.clone(),
        );
        register_builtin(
            env,
            "str_ends_with",
            vec![string.clone(), string.clone()],
            bool_ty.clone(),
        );
        register_builtin(
            env,
            "str_slice",
            vec![string.clone(), int.clone(), int.clone()],
            string.clone(),
        );
        register_builtin(
            env,
            "str_char_at",
            vec![string.clone(), int.clone()],
            int.clone(),
        );
        register_builtin(
            env,
            "chars",
            vec![string.clone()],
            Type::List(Box::new(int.clone())),
        );
        register_builtin(
            env,
            "str_from_chars",
            vec![Type::List(Box::new(int.clone()))],
            string.clone(),
        );

        register_builtin(
            env,
            "map_put",
            vec![
                Type::Map(Box::new(any.clone()), Box::new(any.clone())),
                any.clone(),
                any.clone(),
            ],
            Type::Map(Box::new(any.clone()), Box::new(any.clone())),
        );
        register_builtin(
            env,
            "map_get",
            vec![
                Type::Map(Box::new(any.clone()), Box::new(any.clone())),
                any.clone(),
            ],
            any.clone(),
        );
        register_builtin(
            env,
            "map_get_or",
            vec![
                Type::Map(Box::new(any.clone()), Box::new(any.clone())),
                any.clone(),
                any.clone(),
            ],
            any.clone(),
        );
        register_builtin(
            env,
            "map_remove",
            vec![
                Type::Map(Box::new(any.clone()), Box::new(any.clone())),
                any.clone(),
            ],
            Type::Map(Box::new(any.clone()), Box::new(any.clone())),
        );
        register_builtin(
            env,
            "map_keys",
            vec![Type::Map(Box::new(any.clone()), Box::new(any.clone()))],
            Type::List(Box::new(any.clone())),
        );
        register_builtin(
            env,
            "map_values",
            vec![Type::Map(Box::new(any.clone()), Box::new(any.clone()))],
            Type::List(Box::new(any.clone())),
        );
        register_builtin(
            env,
            "map_has_key",
            vec![
                Type::Map(Box::new(any.clone()), Box::new(any.clone())),
                any.clone(),
            ],
            bool_ty.clone(),
        );
        register_builtin(
            env,
            "map_merge",
            vec![
                Type::Map(Box::new(any.clone()), Box::new(any.clone())),
                Type::Map(Box::new(any.clone()), Box::new(any.clone())),
            ],
            Type::Map(Box::new(any.clone()), Box::new(any.clone())),
        );
        register_builtin(
            env,
            "map_size",
            vec![Type::Map(Box::new(any.clone()), Box::new(any.clone()))],
            int.clone(),
        );
        register_builtin(
            env,
            "map_to_list",
            vec![Type::Map(Box::new(any.clone()), Box::new(any.clone()))],
            Type::List(Box::new(any.clone())),
        );
        register_builtin(
            env,
            "list_to_map",
            vec![Type::List(Box::new(any.clone()))],
            Type::Map(Box::new(any.clone()), Box::new(any.clone())),
        );
        register_builtin(env, "size", vec![any.clone()], int.clone());

        register_builtin(env, "byte_size", vec![string.clone()], int.clone());
        register_builtin(env, "bit_size", vec![string.clone()], int.clone());
        register_builtin(
            env,
            "binary_slice",
            vec![string.clone(), int.clone(), int.clone()],
            string.clone(),
        );
        register_builtin(
            env,
            "binary_at",
            vec![string.clone(), int.clone()],
            int.clone(),
        );
        register_builtin(
            env,
            "binary_to_list",
            vec![string.clone()],
            Type::List(Box::new(int.clone())),
        );
        register_builtin(
            env,
            "list_to_binary",
            vec![Type::List(Box::new(int.clone()))],
            string.clone(),
        );
        register_builtin(
            env,
            "atom_to_list",
            vec![atom.clone()],
            Type::List(Box::new(int.clone())),
        );
        register_builtin(
            env,
            "list_to_atom",
            vec![Type::List(Box::new(int.clone()))],
            atom.clone(),
        );
        register_builtin(
            env,
            "integer_to_list",
            vec![int.clone()],
            Type::List(Box::new(int.clone())),
        );
        register_builtin(
            env,
            "list_to_integer",
            vec![Type::List(Box::new(int.clone()))],
            int.clone(),
        );
        register_builtin(
            env,
            "float_to_list",
            vec![float.clone()],
            Type::List(Box::new(int.clone())),
        );
        register_builtin(
            env,
            "list_to_float",
            vec![Type::List(Box::new(int.clone()))],
            float.clone(),
        );
        register_builtin(env, "iolist_to_binary", vec![any.clone()], string.clone());
        register_builtin(env, "term_to_binary", vec![any.clone()], string.clone());
        register_builtin(env, "binary_to_term", vec![string.clone()], any.clone());
        register_builtin(env, "dynamic", vec![any.clone()], dynamic.clone());
        register_builtin(env, "dynamic_typeof", vec![dynamic.clone()], atom.clone());
        register_builtin(
            env,
            "dynamic_json_decode",
            vec![string.clone()],
            dynamic.clone(),
        );
        register_builtin(
            env,
            "dynamic_json_encode",
            vec![dynamic.clone()],
            string.clone(),
        );
        register_builtin(
            env,
            "dynamic_json_decode_result",
            vec![string.clone()],
            dynamic_result(dynamic.clone()),
        );
        register_builtin(
            env,
            "dynamic_is_null",
            vec![dynamic.clone()],
            bool_ty.clone(),
        );
        register_builtin(
            env,
            "dynamic_is_bool",
            vec![dynamic.clone()],
            bool_ty.clone(),
        );
        register_builtin(
            env,
            "dynamic_is_int",
            vec![dynamic.clone()],
            bool_ty.clone(),
        );
        register_builtin(
            env,
            "dynamic_is_float",
            vec![dynamic.clone()],
            bool_ty.clone(),
        );
        register_builtin(
            env,
            "dynamic_is_atom",
            vec![dynamic.clone()],
            bool_ty.clone(),
        );
        register_builtin(
            env,
            "dynamic_is_string",
            vec![dynamic.clone()],
            bool_ty.clone(),
        );
        register_builtin(
            env,
            "dynamic_is_binary",
            vec![dynamic.clone()],
            bool_ty.clone(),
        );
        register_builtin(
            env,
            "dynamic_is_list",
            vec![dynamic.clone()],
            bool_ty.clone(),
        );
        register_builtin(
            env,
            "dynamic_is_map",
            vec![dynamic.clone()],
            bool_ty.clone(),
        );
        register_builtin(
            env,
            "dynamic_get",
            vec![dynamic.clone(), any.clone()],
            dynamic.clone(),
        );
        register_builtin(
            env,
            "dynamic_get_result",
            vec![dynamic.clone(), any.clone()],
            dynamic_result(dynamic.clone()),
        );
        register_builtin(
            env,
            "dynamic_get_or",
            vec![dynamic.clone(), any.clone(), dynamic.clone()],
            dynamic.clone(),
        );
        register_builtin(
            env,
            "dynamic_at",
            vec![dynamic.clone(), int.clone()],
            dynamic.clone(),
        );
        register_builtin(
            env,
            "dynamic_at_result",
            vec![dynamic.clone(), int.clone()],
            dynamic_result(dynamic.clone()),
        );
        register_builtin(env, "dynamic_string", vec![dynamic.clone()], string.clone());
        register_builtin(env, "dynamic_binary", vec![dynamic.clone()], string.clone());
        register_builtin(env, "dynamic_int", vec![dynamic.clone()], int.clone());
        register_builtin(env, "dynamic_float", vec![dynamic.clone()], float.clone());
        register_builtin(env, "dynamic_bool", vec![dynamic.clone()], bool_ty.clone());
        register_builtin(env, "dynamic_atom", vec![dynamic.clone()], atom.clone());
        register_builtin(
            env,
            "dynamic_list",
            vec![dynamic.clone()],
            Type::List(Box::new(dynamic.clone())),
        );
        register_builtin(
            env,
            "dynamic_map",
            vec![dynamic.clone()],
            Type::Map(Box::new(any.clone()), Box::new(dynamic.clone())),
        );
        register_builtin(
            env,
            "dynamic_string_result",
            vec![dynamic.clone()],
            dynamic_result(string.clone()),
        );
        register_builtin(
            env,
            "dynamic_binary_result",
            vec![dynamic.clone()],
            dynamic_result(string.clone()),
        );
        register_builtin(
            env,
            "dynamic_int_result",
            vec![dynamic.clone()],
            dynamic_result(int.clone()),
        );
        register_builtin(
            env,
            "dynamic_float_result",
            vec![dynamic.clone()],
            dynamic_result(float.clone()),
        );
        register_builtin(
            env,
            "dynamic_bool_result",
            vec![dynamic.clone()],
            dynamic_result(bool_ty.clone()),
        );
        register_builtin(
            env,
            "dynamic_atom_result",
            vec![dynamic.clone()],
            dynamic_result(atom.clone()),
        );
        register_builtin(
            env,
            "dynamic_list_result",
            vec![dynamic.clone()],
            dynamic_result(Type::List(Box::new(dynamic.clone()))),
        );
        register_builtin(
            env,
            "dynamic_map_result",
            vec![dynamic.clone()],
            dynamic_result(Type::Map(Box::new(any.clone()), Box::new(dynamic.clone()))),
        );
        register_builtin_scheme(
            env,
            "decode_map",
            Scheme::poly(
                vec![0, 1],
                Type::Function(
                    vec![
                        dynamic_result(Type::Var(0)),
                        Type::Function(vec![Type::Var(0)], Box::new(Type::Var(1))),
                    ],
                    Box::new(dynamic_result(Type::Var(1))),
                ),
            ),
        );
        register_builtin_scheme(
            env,
            "decode_then",
            Scheme::poly(
                vec![0, 1],
                Type::Function(
                    vec![
                        dynamic_result(Type::Var(0)),
                        Type::Function(vec![Type::Var(0)], Box::new(dynamic_result(Type::Var(1)))),
                    ],
                    Box::new(dynamic_result(Type::Var(1))),
                ),
            ),
        );
        register_builtin_scheme(
            env,
            "decode_field",
            Scheme::poly(
                vec![0],
                Type::Function(
                    vec![
                        dynamic.clone(),
                        string.clone(),
                        Type::Function(
                            vec![dynamic.clone()],
                            Box::new(dynamic_result(Type::Var(0))),
                        ),
                    ],
                    Box::new(dynamic_result(Type::Var(0))),
                ),
            ),
        );
        register_builtin_scheme(
            env,
            "decode_optional_field",
            Scheme::poly(
                vec![0],
                Type::Function(
                    vec![
                        dynamic.clone(),
                        string.clone(),
                        Type::Function(
                            vec![dynamic.clone()],
                            Box::new(dynamic_result(Type::Var(0))),
                        ),
                    ],
                    Box::new(dynamic_result(dynamic_option(Type::Var(0)))),
                ),
            ),
        );
        register_builtin_scheme(
            env,
            "decode_field_or",
            Scheme::poly(
                vec![0],
                Type::Function(
                    vec![
                        dynamic.clone(),
                        string.clone(),
                        Type::Var(0),
                        Type::Function(
                            vec![dynamic.clone()],
                            Box::new(dynamic_result(Type::Var(0))),
                        ),
                    ],
                    Box::new(dynamic_result(Type::Var(0))),
                ),
            ),
        );
        register_builtin_scheme(
            env,
            "decode_index",
            Scheme::poly(
                vec![0],
                Type::Function(
                    vec![
                        dynamic.clone(),
                        int.clone(),
                        Type::Function(
                            vec![dynamic.clone()],
                            Box::new(dynamic_result(Type::Var(0))),
                        ),
                    ],
                    Box::new(dynamic_result(Type::Var(0))),
                ),
            ),
        );
        register_builtin_scheme(
            env,
            "decode_list",
            Scheme::poly(
                vec![0],
                Type::Function(
                    vec![
                        dynamic.clone(),
                        Type::Function(
                            vec![dynamic.clone()],
                            Box::new(dynamic_result(Type::Var(0))),
                        ),
                    ],
                    Box::new(dynamic_result(Type::List(Box::new(Type::Var(0))))),
                ),
            ),
        );
        register_builtin_scheme(
            env,
            "decode_optional",
            Scheme::poly(
                vec![0],
                Type::Function(
                    vec![
                        dynamic.clone(),
                        Type::Function(
                            vec![dynamic.clone()],
                            Box::new(dynamic_result(Type::Var(0))),
                        ),
                    ],
                    Box::new(dynamic_result(dynamic_option(Type::Var(0)))),
                ),
            ),
        );
        register_builtin_scheme(
            env,
            "decode_dict",
            Scheme::poly(
                vec![0],
                Type::Function(
                    vec![
                        dynamic.clone(),
                        Type::Function(
                            vec![dynamic.clone()],
                            Box::new(dynamic_result(Type::Var(0))),
                        ),
                    ],
                    Box::new(dynamic_result(Type::Map(
                        Box::new(Type::String),
                        Box::new(Type::Var(0)),
                    ))),
                ),
            ),
        );
        register_builtin_scheme(
            env,
            "decode_one_of",
            Scheme::poly(
                vec![0],
                Type::Function(
                    vec![
                        dynamic.clone(),
                        Type::Function(
                            vec![dynamic.clone()],
                            Box::new(dynamic_result(Type::Var(0))),
                        ),
                        Type::Function(
                            vec![dynamic.clone()],
                            Box::new(dynamic_result(Type::Var(0))),
                        ),
                    ],
                    Box::new(dynamic_result(Type::Var(0))),
                ),
            ),
        );
        register_builtin(
            env,
            "apply",
            vec![any.clone(), Type::List(Box::new(any.clone()))],
            any.clone(),
        );
        register_builtin(
            env,
            "apply_module",
            vec![any.clone(), any.clone(), Type::List(Box::new(any.clone()))],
            any.clone(),
        );
        register_builtin(env, "typeof", vec![any.clone()], atom.clone());
        register_builtin(env, "assert", vec![bool_ty.clone()], unit.clone());

        if self.config.security_profile == SecurityProfile::Trusted {
            register_builtin(env, "print", vec![any.clone()], unit.clone());
            register_builtin(env, "println", vec![any.clone()], unit.clone());
            register_builtin(env, "dbg", vec![any.clone()], any.clone());

            register_builtin(env, "throw", vec![any.clone()], Type::Never);
            register_builtin(env, "exit", vec![any.clone()], Type::Never);
            register_builtin(env, "error", vec![any.clone()], Type::Never);

            register_builtin(env, "sleep", vec![int.clone()], atom.clone());
            register_builtin(env, "now", vec![], int.clone());
            register_builtin(env, "monotonic_time", vec![], int.clone());
            register_builtin(env, "random", vec![], float.clone());
            register_builtin(env, "random_below", vec![int.clone()], int.clone());
            register_builtin(env, "random_seed", vec![], any.clone());

            register_builtin(
                env,
                "spawn_link",
                vec![Type::Function(vec![], Box::new(any.clone()))],
                pid.clone(),
            );
            register_builtin(env, "link", vec![pid.clone()], bool_ty.clone());
            register_builtin(env, "unlink", vec![pid.clone()], bool_ty.clone());
            register_builtin(env, "monitor", vec![pid.clone()], ref_ty.clone());
            register_builtin(env, "demonitor", vec![ref_ty.clone()], bool_ty.clone());
            register_builtin(
                env,
                "registered",
                vec![],
                Type::List(Box::new(atom.clone())),
            );
            register_builtin(
                env,
                "register",
                vec![atom.clone(), pid.clone()],
                bool_ty.clone(),
            );
            register_builtin(env, "whereis", vec![atom.clone()], pid.clone());
            register_builtin(env, "make_ref", vec![], ref_ty.clone());

            register_builtin(env, "file_read", vec![string.clone()], any.clone());
            register_builtin(
                env,
                "file_write",
                vec![string.clone(), string.clone()],
                atom.clone(),
            );
            register_builtin(env, "file_exists", vec![string.clone()], bool_ty.clone());
            register_builtin(env, "file_delete", vec![string.clone()], atom.clone());
            register_builtin(env, "dir_list", vec![string.clone()], any.clone());
            register_builtin(env, "dir_make", vec![string.clone()], atom.clone());
            register_builtin(env, "get_cwd", vec![], any.clone());

            register_builtin(env, "argv", vec![], Type::List(Box::new(string.clone())));
            register_builtin(env, "env", vec![string.clone()], string.clone());
            register_builtin(
                env,
                "set_env",
                vec![string.clone(), string.clone()],
                bool_ty.clone(),
            );
            register_builtin(env, "exit_code", vec![int.clone()], Type::Never);
            register_builtin(env, "os_cmd", vec![string.clone()], string.clone());

            register_builtin(env, "get", vec![atom.clone()], any.clone());
            register_builtin(env, "put", vec![atom.clone(), any.clone()], any.clone());
            register_builtin(env, "erase", vec![atom.clone()], any.clone());
        }
    }

    fn register_resolved_functions(&self, env: &mut TypeEnv) {
        for ((name, arity), _) in &self.config.resolved_functions {
            let params = vec![Type::Any; *arity];
            let fn_type = Type::Function(params, Box::new(Type::Any));
            env.insert_function(name.clone(), Scheme::mono(fn_type));
        }
    }
}

fn annotate_type_error(
    err: TypeError,
    source: &str,
    function_name: &str,
    span_override: Option<Span>,
) -> TypeError {
    let expression = span_override
        .or_else(|| match &err {
            TypeError::Mismatch(_, _, span, _) => Some(*span),
            _ => None,
        })
        .and_then(|span| snippet_for_span(source, span));

    err.with_mismatch_context(function_name.to_string(), expression)
}

fn snippet_for_span(source: &str, span: Span) -> Option<String> {
    let start = span.start as usize;
    let end = span.end as usize;
    let raw = source.get(start..end)?.trim();
    if raw.is_empty() {
        return None;
    }

    let raw = raw
        .strip_prefix('{')
        .and_then(|text| text.strip_suffix('}'))
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or(raw);

    let single_line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.len() <= 120 {
        Some(single_line)
    } else {
        Some(format!("{}...", &single_line[..117]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_rejects_extern_declaration() {
        let source = r#"
            extern "erlang" {
                fn lists::reverse(List<Int>) -> List<Int>
            }

            fn main() { 1 }
        "#;

        let mut session = Session::with_config(SessionConfig::sandboxed_default());
        let err = session.compile_source(source).unwrap_err();

        match err {
            CompileError::Parse(parse_err) => {
                assert!(parse_err.message.contains("disabled in sandbox mode"));
            }
            other => panic!("expected parse error, got: {:?}", other),
        }
    }

    #[test]
    fn capability_sandbox_allows_only_the_capability_bridge() {
        let source = r#"
            extern "erlang" {
                fn lux_capability::invoke(String, String) -> String
            }

            fn main() { lux_capability::invoke("lux.test.echo", "{}") }
        "#;

        let mut session =
            Session::with_config(SessionConfig::capability_sandboxed());
        session.compile_source(source).unwrap();
    }

    #[test]
    fn capability_sandbox_rejects_other_external_modules() {
        let source = r#"
            extern "erlang" {
                fn os::cmd(String) -> String
            }

            fn main() { os::cmd("id") }
        "#;

        let mut session =
            Session::with_config(SessionConfig::capability_sandboxed());
        let err = session.compile_source(source).unwrap_err();
        match err {
            CompileError::Security(SecurityError::ExternModuleDisallowed { module, .. }) => {
                assert_eq!(module, "os");
            }
            other => panic!("expected external-module security error, got: {:?}", other),
        }
    }

    #[test]
    fn sandbox_rejects_non_whitelisted_import() {
        let source = r#"
            use sysutil
            fn main() { 1 }
        "#;

        let mut session = Session::with_config(SessionConfig::sandboxed_default());
        let err = session.compile_source(source).unwrap_err();

        match err {
            CompileError::Security(SecurityError::ImportDisallowed { module, .. }) => {
                assert_eq!(module, "sysutil");
            }
            other => panic!("expected import security error, got: {:?}", other),
        }
    }

    #[test]
    fn sandbox_env_blocks_dangerous_builtin() {
        let source = r#"
            fn main() { whereis("host") }
        "#;

        let mut session = Session::with_config(SessionConfig::sandboxed_default());
        let err = session.compile_source(source).unwrap_err();

        match err {
            CompileError::Type(TypeError::UnboundVariable(name, _)) => {
                assert_eq!(name, "whereis");
            }
            other => panic!("expected unbound variable error, got: {:?}", other),
        }
    }

    #[test]
    fn multi_arity_functions_are_rejected_as_duplicate_symbols() {
        let source = r#"
            fn foo() -> Int { 1 }
            fn foo(x: Int) -> Int { x }
            fn main() -> Int { foo() + foo(2) }
        "#;

        let mut session = Session::new();
        let error = session.compile_source(source).unwrap_err();
        assert!(matches!(
            error,
            CompileError::Parse(ParseError { message, .. })
                if message.contains("Duplicate symbol `foo`")
        ));
    }

    #[test]
    fn constructor_patterns_bind_variant_field_types() {
        let source = r#"
            enum Option<T> {
                Some(T),
                None,
            }

            fn bad(opt: Option<Int>) -> Bool {
                match opt {
                    Option::Some(x) => x,
                    Option::None => false,
                }
            }
        "#;

        let mut session = Session::new();
        let err = session.compile_source(source).unwrap_err();

        match err {
            CompileError::Type(TypeError::Mismatch(_, _, _, _)) => {}
            other => panic!(
                "expected constructor pattern type mismatch, got: {:?}",
                other
            ),
        }
    }

    #[test]
    fn receive_patterns_bind_variables() {
        let source = r#"
            enum Msg {
                IntMsg(Int),
                Done,
            }

            fn wait() -> Int {
                receive {
                    Msg::IntMsg(x) => x,
                    after 0 => 0
                }
            }
        "#;

        let mut session = Session::new();
        session.compile_source(source).unwrap();
    }

    #[test]
    fn type_aliases_resolve_in_annotations() {
        let source = r#"
            type UserId = Int

            fn id(x: UserId) -> UserId { x }
            fn main() -> UserId { id(1) }
        "#;

        let mut session = Session::new();
        session.compile_source(source).unwrap();
    }

    #[test]
    fn let_bindings_remain_polymorphic() {
        let source = r#"
            fn main() -> Int {
                let id = |x| x
                let value = id(1)
                if id(true) { value } else { 0 }
            }
        "#;

        let mut session = Session::new();
        session.compile_source(source).unwrap();
    }

    #[test]
    fn bool_match_requires_all_cases() {
        let source = r#"
            fn flag(x: Bool) -> Int {
                match x {
                    true => 1
                }
            }
        "#;

        let mut session = Session::new();
        let err = session.compile_source(source).unwrap_err();

        match err {
            CompileError::Type(TypeError::NonExhaustiveMatch(missing, _)) => {
                assert_eq!(missing, vec!["false".to_string()]);
            }
            other => panic!("expected non-exhaustive bool match, got: {:?}", other),
        }
    }

    #[test]
    fn enum_match_requires_all_variants() {
        let source = r#"
            enum Option<T> {
                Some(T),
                None,
            }

            fn unwrap(opt: Option<Int>) -> Int {
                match opt {
                    Option::Some(x) => x
                }
            }
        "#;

        let mut session = Session::new();
        let err = session.compile_source(source).unwrap_err();

        match err {
            CompileError::Type(TypeError::NonExhaustiveMatch(missing, _)) => {
                assert_eq!(missing, vec!["Option::None".to_string()]);
            }
            other => panic!("expected non-exhaustive enum match, got: {:?}", other),
        }
    }

    #[test]
    fn wildcard_match_is_exhaustive() {
        let source = r#"
            enum Option<T> {
                Some(T),
                None,
            }

            fn unwrap_or_zero(opt: Option<Int>) -> Int {
                match opt {
                    Option::Some(x) => x,
                    _ => 0
                }
            }
        "#;

        let mut session = Session::new();
        session.compile_source(source).unwrap();
    }

    #[test]
    fn always_true_guard_counts_toward_match_coverage() {
        let source = r#"
            fn flag(x: Bool) -> Int {
                match x {
                    true if true => 1,
                    false => 0
                }
            }
        "#;

        let mut session = Session::new();
        session.compile_source(source).unwrap();
    }

    #[test]
    fn duplicate_bool_match_arm_is_redundant() {
        let source = r#"
            fn flag(x: Bool) -> Int {
                match x {
                    true => 1,
                    true => 2,
                    false => 0
                }
            }
        "#;

        let mut session = Session::new();
        let err = session.compile_source(source).unwrap_err();

        match err {
            CompileError::Type(TypeError::RedundantMatchArm(_)) => {}
            other => panic!("expected redundant bool match arm, got: {:?}", other),
        }
    }

    #[test]
    fn always_false_guard_is_redundant() {
        let source = r#"
            fn flag(x: Bool) -> Int {
                match x {
                    true if false => 1,
                    true => 2,
                    false => 0
                }
            }
        "#;

        let mut session = Session::new();
        let err = session.compile_source(source).unwrap_err();

        match err {
            CompileError::Type(TypeError::RedundantMatchArm(_)) => {}
            other => panic!("expected redundant false-guard arm, got: {:?}", other),
        }
    }

    #[test]
    fn duplicate_enum_match_arm_is_redundant() {
        let source = r#"
            enum Option<T> {
                Some(T),
                None,
            }

            fn unwrap_or(opt: Option<Int>) -> Int {
                match opt {
                    Option::Some(x) => x,
                    Option::Some(_) => 0,
                    Option::None => 0
                }
            }
        "#;

        let mut session = Session::new();
        let err = session.compile_source(source).unwrap_err();

        match err {
            CompileError::Type(TypeError::RedundantMatchArm(_)) => {}
            other => panic!("expected redundant enum match arm, got: {:?}", other),
        }
    }

    #[test]
    fn arm_after_catch_all_is_redundant() {
        let source = r#"
            fn flag(x: Bool) -> Int {
                match x {
                    _ => 0,
                    false => 1
                }
            }
        "#;

        let mut session = Session::new();
        let err = session.compile_source(source).unwrap_err();

        match err {
            CompileError::Type(TypeError::RedundantMatchArm(_)) => {}
            other => panic!("expected redundant arm after catch-all, got: {:?}", other),
        }
    }

    #[test]
    fn receive_requires_all_variants_without_timeout() {
        let source = r#"
            enum Msg {
                IntMsg(Int),
                Done,
            }

            fn wait() -> Int {
                receive {
                    Msg::IntMsg(x) => x
                }
            }
        "#;

        let mut session = Session::new();
        let err = session.compile_source(source).unwrap_err();

        match err {
            CompileError::Type(TypeError::NonExhaustiveMatch(missing, _)) => {
                assert_eq!(missing, vec!["Msg::Done".to_string()]);
            }
            other => panic!("expected non-exhaustive receive, got: {:?}", other),
        }
    }

    #[test]
    fn duplicate_receive_arm_is_redundant() {
        let source = r#"
            fn wait() -> Int {
                receive {
                    true => 1,
                    true => 2,
                    false => 0
                }
            }
        "#;

        let mut session = Session::new();
        let err = session.compile_source(source).unwrap_err();

        match err {
            CompileError::Type(TypeError::RedundantMatchArm(_)) => {}
            other => panic!("expected redundant receive arm, got: {:?}", other),
        }
    }

    #[test]
    fn receive_with_timeout_need_not_be_exhaustive() {
        let source = r#"
            enum Msg {
                IntMsg(Int),
                Done,
            }

            fn wait() -> Int {
                receive {
                    Msg::IntMsg(x) => x,
                    after 0 => 0
                }
            }
        "#;

        let mut session = Session::new();
        session.compile_source(source).unwrap();
    }

    #[test]
    fn mismatch_errors_include_function_and_expression() {
        let source = r#"
            fn bad() -> Float { 1 }
        "#;

        let mut session = Session::new();
        let err = session.compile_source(source).unwrap_err();

        let rendered = match err {
            CompileError::Type(type_err) => type_err.to_string(),
            other => panic!("expected type error, got: {:?}", other),
        };

        assert!(rendered.contains("Type mismatch: expected Float, found Int"));
        assert!(rendered.contains("Function: bad"));
        assert!(rendered.contains("Expression: 1"));
    }

    #[test]
    fn always_true_guard_counts_toward_receive_coverage() {
        let source = r#"
            fn wait() -> Int {
                receive {
                    true if true => 1,
                    false => 0
                }
            }
        "#;

        let mut session = Session::new();
        session.compile_source(source).unwrap();
    }

    #[test]
    fn binary_patterns_bind_tail_as_string() {
        let source = r#"
            fn take_tail(x: String) -> String {
                match x {
                    <<0x41:8, rest>> => rest,
                    _ => <<"">>
                }
            }
        "#;

        let mut session = Session::new();
        session.compile_source(source).unwrap();
    }

    #[test]
    fn dynamic_type_predicates_accept_binaries_and_nil() {
        let source = r#"
            fn main() -> Bool {
                is_nil([]) && is_string("ABCD") && is_binary("ABCD")
            }
        "#;

        let mut session = Session::new();
        session.compile_source(source).unwrap();
    }

    #[test]
    fn builtin_names_have_one_signature() {
        let source = r#"
            fn main() -> Int {
                let a = random_below(100)
                let m = %{:name => "Alice"}
                let fallback = map_get_or(m, :missing, "default")
                print(random())
                print(fallback)
                a
            }
        "#;

        let mut session = Session::new();
        session.compile_source(source).unwrap();
    }

    #[test]
    fn builtin_names_are_not_overloaded_by_arity() {
        let mut session = Session::new();
        let error = session
            .compile_source("fn main() -> Int { random(100) }")
            .unwrap_err();

        assert!(matches!(
            error,
            CompileError::Type(TypeError::ArityMismatch(0, 1, _))
        ));
    }

    #[test]
    fn binary_specifiers_typecheck() {
        let source = r#"
            fn head_utf8(input: String) -> Int {
                match input {
                    <<x/utf8, rest/binary>> => {
                        print(rest)
                        x
                    },
                    _ => 0
                }
            }

            fn main() -> Bool {
                let packed = <<65/utf8, "BC"/binary>>
                is_string(packed) && head_utf8(packed) == 65
            }
        "#;

        let mut session = Session::new();
        session.compile_source(source).unwrap();
    }

    #[test]
    fn apply_with_module_function_and_args_typechecks() {
        let source = r#"
            fn main() -> Int {
                apply_module(:erlang, :abs, [-5])
            }
        "#;

        let mut session = Session::new();
        session.compile_source(source).unwrap();
    }

    #[test]
    fn dynamic_type_and_json_helpers_typecheck() {
        let source = r#"
            fn read_name(body: String) -> String {
                let payload: Dynamic = dynamic_json_decode(body)
                dynamic_string(dynamic_get(payload, "name"))
            }

            fn main() -> Bool {
                let payload: Dynamic = dynamic_json_decode("{\"name\":\"Lux\",\"items\":[1,2]}")
                dynamic_is_map(payload) && dynamic_int(dynamic_at(dynamic_get(payload, "items"), 2)) == 2
            }
        "#;

        let mut session = Session::new();
        session.compile_source(source).unwrap();
    }

    #[test]
    fn dynamic_result_decoders_and_combinators_typecheck() {
        let source = r#"
            fn decode_name(body: String) -> DynamicResult<String> {
                let payload = dynamic_json_decode(body)
                decode_then(dynamic_get_result(payload, "name"), |value| dynamic_string_result(value))
            }

            fn main() -> Bool {
                let payload = dynamic_json_decode("{\"items\":[1,2]}")
                match decode_then(dynamic_get_result(payload, "items"), |items| dynamic_at_result(items, 2)) {
                    DynamicResult::Ok(value) => dynamic_int_result(value) == DynamicResult::Ok(2),
                    DynamicResult::Err(_) => false
                }
            }
        "#;

        let mut session = Session::new();
        session.compile_source(source).unwrap();
    }

    #[test]
    fn dynamic_shape_combinators_typecheck() {
        let source = r#"
            fn decode_name(payload: Dynamic) -> DynamicResult<String> {
                decode_field(payload, "name", |value| dynamic_string_result(value))
            }

            fn decode_numbers(payload: Dynamic) -> DynamicResult<List<Int>> {
                decode_field(payload, "items", |items| {
                    decode_list(items, |value| dynamic_int_result(value))
                })
            }

            fn main() -> Bool {
                let payload = dynamic_json_decode("{\"name\":\"Lux\",\"items\":[1,2,3]}")
                match (decode_name(payload), decode_numbers(payload)) {
                    (DynamicResult::Ok(name), DynamicResult::Ok(numbers)) =>
                        name == "Lux" && nth(2, numbers) == 2,
                    _ => false
                }
            }
        "#;

        let mut session = Session::new();
        session.compile_source(source).unwrap();
    }

    #[test]
    fn dynamic_optional_and_dict_combinators_typecheck() {
        let source = r#"
            fn decode_nickname(payload: Dynamic) -> DynamicResult<DynamicOption<String>> {
                decode_optional_field(payload, "nickname", |item| dynamic_string_result(item))
            }

            fn decode_scores(payload: Dynamic) -> DynamicResult<Map<String, Int>> {
                decode_field(payload, "scores", |value| {
                    decode_dict(value, |item| dynamic_int_result(item))
                })
            }

            fn decode_mode(payload: Dynamic) -> DynamicResult<String> {
                decode_field_or(payload, "mode", "demo", |value| dynamic_string_result(value))
            }

            fn main() -> Bool {
                let payload = dynamic_json_decode("{\"nickname\":null,\"scores\":{\"a\":1,\"b\":2}}")
                match (decode_nickname(payload), decode_scores(payload), decode_mode(payload)) {
                    (DynamicResult::Ok(DynamicOption::None), DynamicResult::Ok(scores), DynamicResult::Ok(mode)) =>
                        map_get(scores, "b") == 2 && mode == "demo",
                    _ => false
                }
            }
        "#;

        let mut session = Session::new();
        session.compile_source(source).unwrap();
    }

    #[test]
    fn dynamic_decode_errors_are_structured() {
        let source = r#"
            fn main() -> Bool {
                let payload = dynamic_json_decode("{\"name\":7}")
                match decode_field(payload, "name", |value| dynamic_string_result(value)) {
                    DynamicResult::Err(
                        DecodeError::At(
                            DecodePathItem::Field(field),
                            DecodeError::Expected(kind)
                        )
                    ) => field == "name" && kind == "string",
                    _ => false
                }
            }
        "#;

        let mut session = Session::new();
        session.compile_source(source).unwrap();
    }
}
