use std::collections::HashMap;

use crate::syntax::ast::*;
use crate::syntax::span::Span;
use crate::types::env::{TypeEnv, free_vars_in_type};
use crate::types::types::{Scheme, Substitution, TyVar, Type, TypeId};
use crate::types::unify::{TypeError, unify};

pub struct InferenceContext {
    next_var: TyVar,
    next_type_id: TypeId,
    substitution: Substitution,
    type_defs: HashMap<String, TypeDef>,
    type_aliases: HashMap<String, TypeAliasDef>,
    /// Extern function signatures: key is "module::name", value is function type scheme
    extern_fns: HashMap<String, Scheme>,
    /// Struct definitions for type-checking StructInit
    struct_defs: HashMap<String, StructDef>,
}

/// Struct definition for type checking
#[derive(Debug, Clone)]
pub struct StructDef {
    pub type_id: TypeId,
    pub type_params: Vec<String>,
    pub fields: Vec<(String, Type)>,
}

#[derive(Debug, Clone)]
pub struct TypeDef {
    pub id: TypeId,
    pub params: Vec<String>,
    pub variants: Vec<(String, Vec<Type>)>, // for enums
}

#[derive(Debug, Clone)]
pub struct TypeAliasDef {
    pub type_params: Vec<String>,
    pub ty: TypeExpr,
}

impl InferenceContext {
    pub fn new() -> Self {
        InferenceContext {
            next_var: 0,
            next_type_id: 0,
            substitution: Substitution::new(),
            type_defs: HashMap::new(),
            type_aliases: HashMap::new(),
            extern_fns: HashMap::new(),
            struct_defs: HashMap::new(),
        }
    }

    /// Generate a fresh type variable
    pub fn fresh_var(&mut self) -> Type {
        let var = self.next_var;
        self.next_var += 1;
        Type::Var(var)
    }

    fn fresh_tyvar(&mut self) -> TyVar {
        match self.fresh_var() {
            Type::Var(var) => var,
            _ => unreachable!("fresh_var always returns a type variable"),
        }
    }

    /// Generate a fresh type ID
    pub fn fresh_type_id(&mut self) -> TypeId {
        let id = self.next_type_id;
        self.next_type_id += 1;
        id
    }

    /// Register a type definition
    /// If the type already exists, update it in place (keeping the same ID)
    pub fn register_type(
        &mut self,
        name: String,
        params: Vec<String>,
        variants: Vec<(String, Vec<Type>)>,
    ) -> TypeId {
        if let Some(existing) = self.type_defs.get(&name) {
            // Type already registered - update in place, keeping the same ID
            let id = existing.id;
            self.type_defs.insert(
                name,
                TypeDef {
                    id,
                    params,
                    variants,
                },
            );
            id
        } else {
            // New type - allocate a fresh ID
            let id = self.fresh_type_id();
            self.type_defs.insert(
                name,
                TypeDef {
                    id,
                    params,
                    variants,
                },
            );
            id
        }
    }

    /// Register an extern function with its type signature
    /// Key format: "module::name" (e.g., "lists::reverse")
    pub fn register_extern_fn(
        &mut self,
        module: &str,
        name: &str,
        type_params: &[String],
        param_types: Vec<Type>,
        return_type: Type,
    ) {
        let key = format!("{}::{}", module, name);

        // Collect type variables from type parameters
        let vars: Vec<TyVar> = (0..type_params.len() as TyVar).collect();

        // Create function type
        let fn_type = Type::Function(param_types, Box::new(return_type));

        // Create scheme (generalized over type params)
        let scheme = Scheme { vars, ty: fn_type };

        self.extern_fns.insert(key, scheme);
    }

    /// Look up an extern function by module and name
    pub fn lookup_extern_fn(&self, module: &str, name: &str) -> Option<&Scheme> {
        let key = format!("{}::{}", module, name);
        self.extern_fns.get(&key)
    }

    /// Register a struct definition
    /// If the struct already exists, update it in place (keeping the same ID)
    pub fn register_struct(
        &mut self,
        name: String,
        type_params: Vec<String>,
        fields: Vec<(String, Type)>,
    ) -> TypeId {
        if let Some(existing) = self.struct_defs.get(&name) {
            // Struct already registered - update in place, keeping the same ID
            let type_id = existing.type_id;
            self.struct_defs.insert(
                name,
                StructDef {
                    type_id,
                    type_params,
                    fields,
                },
            );
            type_id
        } else {
            // New struct - allocate a fresh ID
            let type_id = self.fresh_type_id();
            self.struct_defs.insert(
                name,
                StructDef {
                    type_id,
                    type_params,
                    fields,
                },
            );
            type_id
        }
    }

    /// Look up a struct definition
    pub fn lookup_struct(&self, name: &str) -> Option<&StructDef> {
        self.struct_defs.get(name)
    }

    pub fn register_type_alias(&mut self, name: String, type_params: Vec<String>, ty: TypeExpr) {
        self.type_aliases
            .insert(name, TypeAliasDef { type_params, ty });
    }

    /// Instantiate a type scheme with fresh variables
    pub fn instantiate(&mut self, scheme: &Scheme) -> Type {
        let mapping: HashMap<TyVar, Type> =
            scheme.vars.iter().map(|&v| (v, self.fresh_var())).collect();
        self.substitute_vars(&scheme.ty, &mapping)
    }

    fn substitute_vars(&self, ty: &Type, mapping: &HashMap<TyVar, Type>) -> Type {
        match ty {
            Type::Var(v) => mapping.get(v).cloned().unwrap_or_else(|| ty.clone()),
            Type::Function(params, ret) => Type::Function(
                params
                    .iter()
                    .map(|p| self.substitute_vars(p, mapping))
                    .collect(),
                Box::new(self.substitute_vars(ret, mapping)),
            ),
            Type::Tuple(ts) => Type::Tuple(
                ts.iter()
                    .map(|t| self.substitute_vars(t, mapping))
                    .collect(),
            ),
            Type::List(elem) => Type::List(Box::new(self.substitute_vars(elem, mapping))),
            Type::Map(key, value) => Type::Map(
                Box::new(self.substitute_vars(key, mapping)),
                Box::new(self.substitute_vars(value, mapping)),
            ),
            Type::Record(fields) => Type::Record(
                fields
                    .iter()
                    .map(|(n, t)| (n.clone(), self.substitute_vars(t, mapping)))
                    .collect(),
            ),
            Type::Named(id, name, args) => Type::Named(
                *id,
                name.clone(),
                args.iter()
                    .map(|a| self.substitute_vars(a, mapping))
                    .collect(),
            ),
            _ => ty.clone(),
        }
    }

    /// Generalize a type to a scheme (quantify free vars not in env)
    pub fn generalize(&self, env: &TypeEnv, ty: &Type) -> Scheme {
        let ty = self.substitution.apply(ty);
        let ty_vars = free_vars_in_type(&ty);
        let env_vars = env.free_vars();
        let vars: Vec<TyVar> = ty_vars.difference(&env_vars).copied().collect();
        Scheme { vars, ty }
    }

    /// Apply current substitution
    pub fn apply(&self, ty: &Type) -> Type {
        self.substitution.apply(ty)
    }

    /// Unify two types and update substitution
    pub fn unify(&mut self, t1: &Type, t2: &Type, span: Span) -> Result<(), TypeError> {
        let t1 = self.apply(t1);
        let t2 = self.apply(t2);
        let subst = unify(&t1, &t2, span)?;
        self.substitution = self.substitution.compose(&subst);
        Ok(())
    }

    /// Convert a type expression to a type
    pub fn type_expr_to_type(
        &mut self,
        expr: &TypeExpr,
        type_params: &HashMap<String, TyVar>,
    ) -> Result<Type, TypeError> {
        match expr {
            TypeExpr::Named(name, args, span) => {
                // Check if it's a type parameter
                if args.is_empty() {
                    if let Some(&var) = type_params.get(name) {
                        return Ok(Type::Var(var));
                    }
                }

                // Check built-in types
                let ty = match name.as_str() {
                    "Int" => Type::Int,
                    "Float" => Type::Float,
                    "Bool" => Type::Bool,
                    "String" => Type::String,
                    "Atom" => Type::Atom,
                    "Pid" => Type::Pid,
                    "Ref" => Type::Ref,
                    "Dynamic" => Type::Dynamic,
                    "Never" => Type::Never,
                    "Any" => Type::Any,
                    "List" => {
                        // List<T> is a built-in generic type
                        if args.len() == 1 {
                            let elem_type = self.type_expr_to_type(&args[0], type_params)?;
                            Type::List(Box::new(elem_type))
                        } else if args.is_empty() {
                            // List without type param - use fresh var
                            Type::List(Box::new(self.fresh_var()))
                        } else {
                            return Err(TypeError::UnboundType(
                                format!("List expects 1 type argument, got {}", args.len()),
                                *span,
                            ));
                        }
                    }
                    "Map" => {
                        // Map<K, V> is a built-in generic type
                        if args.len() == 2 {
                            let key_type = self.type_expr_to_type(&args[0], type_params)?;
                            let value_type = self.type_expr_to_type(&args[1], type_params)?;
                            Type::Map(Box::new(key_type), Box::new(value_type))
                        } else if args.is_empty() {
                            Type::Map(Box::new(self.fresh_var()), Box::new(self.fresh_var()))
                        } else {
                            return Err(TypeError::UnboundType(
                                format!("Map expects 2 type arguments, got {}", args.len()),
                                *span,
                            ));
                        }
                    }
                    _ => {
                        // Look up user-defined type (check enums first, then structs)
                        if let Some(type_def) = self.type_defs.get(name) {
                            let id = type_def.id;
                            let mut type_args = Vec::new();
                            for a in args {
                                type_args.push(self.type_expr_to_type(a, type_params)?);
                            }
                            Type::Named(id, name.clone(), type_args)
                        } else if let Some(struct_def) = self.struct_defs.get(name) {
                            let id = struct_def.type_id;
                            let mut type_args = Vec::new();
                            for a in args {
                                type_args.push(self.type_expr_to_type(a, type_params)?);
                            }
                            Type::Named(id, name.clone(), type_args)
                        } else if let Some(alias_def) = self.type_aliases.get(name).cloned() {
                            if args.len() != alias_def.type_params.len() {
                                return Err(TypeError::UnboundType(
                                    format!(
                                        "{} expects {} type arguments, got {}",
                                        name,
                                        alias_def.type_params.len(),
                                        args.len()
                                    ),
                                    *span,
                                ));
                            }

                            let mut alias_params = HashMap::new();
                            let mut alias_subst = Substitution::new();
                            for (param_name, arg_expr) in alias_def.type_params.iter().zip(args) {
                                let var = self.fresh_tyvar();
                                alias_params.insert(param_name.clone(), var);
                                alias_subst
                                    .insert(var, self.type_expr_to_type(arg_expr, type_params)?);
                            }

                            let expanded = self.type_expr_to_type(&alias_def.ty, &alias_params)?;
                            alias_subst.apply(&expanded)
                        } else {
                            return Err(TypeError::UnboundType(name.clone(), *span));
                        }
                    }
                };
                Ok(ty)
            }
            TypeExpr::Tuple(types, _) => {
                let tys: Vec<Type> = types
                    .iter()
                    .map(|t| self.type_expr_to_type(t, type_params))
                    .collect::<Result<_, _>>()?;
                Ok(Type::Tuple(tys))
            }
            TypeExpr::Function(params, ret, _) => {
                let param_types: Vec<Type> = params
                    .iter()
                    .map(|t| self.type_expr_to_type(t, type_params))
                    .collect::<Result<_, _>>()?;
                let ret_type = self.type_expr_to_type(ret, type_params)?;
                Ok(Type::Function(param_types, Box::new(ret_type)))
            }
            TypeExpr::Record(fields, _) => {
                let field_types: Vec<(String, Type)> = fields
                    .iter()
                    .map(|(n, t)| Ok((n.clone(), self.type_expr_to_type(t, type_params)?)))
                    .collect::<Result<_, TypeError>>()?;
                Ok(Type::Record(field_types))
            }
            TypeExpr::Unit(_) => Ok(Type::Unit),
        }
    }

    /// Infer the type of an expression
    pub fn infer_expr(&mut self, env: &TypeEnv, expr: &Expr) -> Result<Type, TypeError> {
        match expr {
            Expr::Int(_, _) => Ok(Type::Int),
            Expr::Float(_, _) => Ok(Type::Float),
            Expr::String(_, _) => Ok(Type::String),
            Expr::InterpolatedString(parts, _) => {
                // Check that all interpolated expressions are valid
                for part in parts {
                    if let crate::syntax::ast::InterpolatedPart::Expr(e) = part {
                        self.infer_expr(env, e)?;
                    }
                }
                Ok(Type::String)
            }
            Expr::Bool(_, _) => Ok(Type::Bool),
            Expr::Atom(_, _) => Ok(Type::Atom),
            Expr::Unit(_) => Ok(Type::Unit),

            Expr::Var(name, span) => match env.lookup_name(name) {
                Some(scheme) => Ok(self.instantiate(&scheme)),
                None => Err(TypeError::UnboundVariable(name.clone(), *span)),
            },

            Expr::Tuple(exprs, _) => {
                let types: Vec<Type> = exprs
                    .iter()
                    .map(|e| self.infer_expr(env, e))
                    .collect::<Result<_, _>>()?;
                Ok(Type::Tuple(types))
            }

            Expr::List(exprs, tail, span) => {
                let elem_type = self.fresh_var();
                for e in exprs {
                    let t = self.infer_expr(env, e)?;
                    self.unify(&elem_type, &t, *span)?;
                }
                if let Some(t) = tail {
                    let tail_type = self.infer_expr(env, t)?;
                    self.unify(&Type::List(Box::new(elem_type.clone())), &tail_type, *span)?;
                }
                Ok(Type::List(Box::new(self.apply(&elem_type))))
            }

            Expr::Map(entries, span) => {
                let key_type = self.fresh_var();
                let value_type = self.fresh_var();
                for (k, v) in entries {
                    let kt = self.infer_expr(env, k)?;
                    let vt = self.infer_expr(env, v)?;
                    self.unify(&key_type, &kt, *span)?;
                    self.unify(&value_type, &vt, *span)?;
                }
                Ok(Type::Map(
                    Box::new(self.apply(&key_type)),
                    Box::new(self.apply(&value_type)),
                ))
            }

            Expr::Range(start, end, _inclusive, span) => {
                let start_type = self.infer_expr(env, start)?;
                let end_type = self.infer_expr(env, end)?;
                self.unify(&start_type, &Type::Int, *span)?;
                self.unify(&end_type, &Type::Int, *span)?;
                Ok(Type::List(Box::new(Type::Int)))
            }

            Expr::ListComp {
                expr,
                generators,
                filters,
                span,
            } => {
                let mut local_env = env.clone();

                // Process generators - each binds variables
                for generator in generators {
                    let source_type = self.infer_expr(&local_env, &generator.source)?;
                    // Expect source to be a list
                    let elem_type = self.fresh_var();
                    self.unify(
                        &source_type,
                        &Type::List(Box::new(elem_type.clone())),
                        *span,
                    )?;
                    // Bind pattern variables with the element type
                    self.bind_pattern_vars(
                        &mut local_env,
                        &generator.pattern,
                        &self.apply(&elem_type),
                    )?;
                }

                // Check filters are boolean
                for filter in filters {
                    let filter_type = self.infer_expr(&local_env, filter)?;
                    self.unify(&filter_type, &Type::Bool, *span)?;
                }

                // Infer the expression type
                let expr_type = self.infer_expr(&local_env, expr)?;
                Ok(Type::List(Box::new(self.apply(&expr_type))))
            }

            Expr::Binary(left, op, right, span) => {
                let left_type = self.infer_expr(env, left)?;
                let right_type = self.infer_expr(env, right)?;

                match op {
                    BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod => {
                        self.unify(&left_type, &Type::Int, *span)?;
                        self.unify(&right_type, &Type::Int, *span)?;
                        Ok(Type::Int)
                    }
                    BinOp::Eq | BinOp::NotEq => {
                        self.unify(&left_type, &right_type, *span)?;
                        Ok(Type::Bool)
                    }
                    BinOp::Lt | BinOp::LtEq | BinOp::Gt | BinOp::GtEq => {
                        self.unify(&left_type, &Type::Int, *span)?;
                        self.unify(&right_type, &Type::Int, *span)?;
                        Ok(Type::Bool)
                    }
                    BinOp::And | BinOp::Or => {
                        self.unify(&left_type, &Type::Bool, *span)?;
                        self.unify(&right_type, &Type::Bool, *span)?;
                        Ok(Type::Bool)
                    }
                    BinOp::Concat => {
                        // ++ concatenates lists or strings
                        // Both sides must be the same type
                        self.unify(&left_type, &right_type, *span)?;
                        Ok(self.apply(&left_type))
                    }
                    BinOp::Pipe => {
                        // Pipe operator: x |> f  means f(x)
                        let ret_type = self.fresh_var();
                        let right_type = match &**right {
                            Expr::Var(name, _) if env.lookup(name).is_none() => env
                                .lookup_function(name)
                                .map(|scheme| self.instantiate(scheme))
                                .unwrap_or(right_type),
                            _ => right_type,
                        };
                        let fn_type = Type::Function(vec![left_type], Box::new(ret_type.clone()));
                        self.unify(&right_type, &fn_type, *span)?;
                        Ok(self.apply(&ret_type))
                    }
                }
            }

            Expr::Unary(op, inner, span) => {
                let inner_type = self.infer_expr(env, inner)?;
                match op {
                    UnaryOp::Neg => {
                        self.unify(&inner_type, &Type::Int, *span)?;
                        Ok(Type::Int)
                    }
                    UnaryOp::Not => {
                        self.unify(&inner_type, &Type::Bool, *span)?;
                        Ok(Type::Bool)
                    }
                }
            }

            Expr::Cond(arms, span) => {
                if arms.is_empty() {
                    return Ok(Type::Unit);
                }
                // All conditions must be Bool, all bodies must have same type
                let result_type = self.fresh_var();
                for (cond, body) in arms {
                    let cond_type = self.infer_expr(env, cond)?;
                    self.unify(&cond_type, &Type::Bool, *span)?;
                    let body_type = self.infer_expr(env, body)?;
                    self.unify(&result_type, &body_type, *span)?;
                }
                Ok(self.apply(&result_type))
            }

            Expr::If(cond, then_branch, else_branch, span) => {
                let cond_type = self.infer_expr(env, cond)?;
                self.unify(&cond_type, &Type::Bool, *span)?;

                let then_type = self.infer_expr(env, then_branch)?;

                if let Some(else_br) = else_branch {
                    let else_type = self.infer_expr(env, else_br)?;
                    self.unify(&then_type, &else_type, *span)?;
                    Ok(self.apply(&then_type))
                } else {
                    self.unify(&then_type, &Type::Unit, *span)?;
                    Ok(Type::Unit)
                }
            }

            Expr::Block(stmts, final_expr, _) => {
                let mut local_env = env.clone();

                for stmt in stmts {
                    match stmt {
                        Stmt::Let(pattern, ty_ann, init, span) => {
                            let init_type = self.infer_expr(&local_env, init)?;

                            if let Some(ann) = ty_ann {
                                let ann_type = self.type_expr_to_type(ann, &HashMap::new())?;
                                self.unify(&init_type, &ann_type, *span)?;
                            }

                            // Extract variables from pattern and add to environment
                            self.bind_let_pattern_vars(
                                &mut local_env,
                                pattern,
                                &self.apply(&init_type),
                            )?;
                        }
                        Stmt::Expr(e) => {
                            self.infer_expr(&local_env, e)?;
                        }
                    }
                }

                if let Some(e) = final_expr {
                    self.infer_expr(&local_env, e)
                } else {
                    Ok(Type::Unit)
                }
            }

            Expr::Call(func, args, span) => {
                let arg_types: Vec<Type> = args
                    .iter()
                    .map(|a| self.infer_expr(env, a))
                    .collect::<Result<_, _>>()?;
                let func_type = match &**func {
                    Expr::Var(name, _) if env.lookup(name).is_none() => {
                        if let Some(scheme) = env.lookup_function(name) {
                            self.instantiate(scheme)
                        } else {
                            self.infer_expr(env, func)?
                        }
                    }
                    _ => self.infer_expr(env, func)?,
                };

                let return_type = self.fresh_var();
                let expected = Type::Function(arg_types, Box::new(return_type.clone()));

                self.unify(&func_type, &expected, *span)?;
                Ok(self.apply(&return_type))
            }

            Expr::Lambda(params, ret_ann, body, _) => {
                let param_types: Vec<Type> = params
                    .iter()
                    .map(|p| {
                        if let Some(ty) = &p.ty {
                            self.type_expr_to_type(ty, &HashMap::new())
                        } else {
                            Ok(self.fresh_var())
                        }
                    })
                    .collect::<Result<_, _>>()?;

                let mut body_env = env.clone();
                for (param, ty) in params.iter().zip(&param_types) {
                    body_env.insert(param.name.clone(), Scheme::mono(ty.clone()));
                }

                let body_type = self.infer_expr(&body_env, body)?;

                if let Some(ret) = ret_ann {
                    let ret_type = self.type_expr_to_type(ret, &HashMap::new())?;
                    self.unify(&body_type, &ret_type, body.span())?;
                }

                Ok(Type::Function(
                    param_types.into_iter().map(|t| self.apply(&t)).collect(),
                    Box::new(self.apply(&body_type)),
                ))
            }

            // Process primitives
            Expr::Spawn(thunk, span) => {
                let thunk_type = self.infer_expr(env, thunk)?;
                let ret = self.fresh_var();
                self.unify(&thunk_type, &Type::Function(vec![], Box::new(ret)), *span)?;
                Ok(Type::Pid)
            }

            Expr::Send(pid, msg, span) => {
                let pid_type = self.infer_expr(env, pid)?;
                self.unify(&pid_type, &Type::Pid, *span)?;
                let msg_type = self.infer_expr(env, msg)?;
                Ok(msg_type)
            }

            Expr::Receive {
                arms,
                timeout,
                span,
            } => {
                let result_type = self.fresh_var();
                let message_type = self.fresh_var();
                for arm in arms {
                    let mut arm_env = env.clone();
                    self.bind_pattern_vars(&mut arm_env, &arm.pattern, &message_type)?;
                    if let Some(guard) = &arm.guard {
                        let guard_type = self.infer_expr(&arm_env, guard)?;
                        self.unify(&guard_type, &Type::Bool, *span)?;
                    }
                    let arm_type = self.infer_expr(&arm_env, &arm.body)?;
                    self.unify(&result_type, &arm_type, *span)?;
                }
                let applied_message_type = self.apply(&message_type);
                self.check_match_redundant(&applied_message_type, arms)?;
                // Check timeout body type if present
                if let Some((_, timeout_body)) = timeout {
                    let timeout_type = self.infer_expr(env, timeout_body)?;
                    self.unify(&result_type, &timeout_type, *span)?;
                } else {
                    self.check_match_exhaustive(&applied_message_type, arms, *span)?;
                }
                Ok(self.apply(&result_type))
            }

            Expr::SelfPid(_) => Ok(Type::Pid),

            Expr::Return(expr, _) => {
                if let Some(e) = expr {
                    self.infer_expr(env, e)
                } else {
                    Ok(Type::Unit)
                }
            }

            Expr::Index(container, key, span) => {
                let container_type = self.infer_expr(env, container)?;
                let key_type = self.infer_expr(env, key)?;

                // Check if it's a map or list access
                let value_type = self.fresh_var();

                // Try to unify with Map(key_type, value_type)
                let map_type = Type::Map(Box::new(key_type.clone()), Box::new(value_type.clone()));
                if self.unify(&container_type, &map_type, *span).is_ok() {
                    return Ok(self.apply(&value_type));
                }

                // Try to unify with List(value_type) where key is Int
                let list_type = Type::List(Box::new(value_type.clone()));
                if self.unify(&container_type, &list_type, *span).is_ok() {
                    self.unify(&key_type, &Type::Int, *span)?;
                    return Ok(self.apply(&value_type));
                }

                // Default: return Any
                Ok(Type::Any)
            }

            Expr::Try {
                body,
                catch_arms,
                span,
            } => {
                let body_type = self.infer_expr(env, body)?;

                // Each catch arm should return the same type as body
                for arm in catch_arms {
                    // Bind the pattern variable
                    let mut local_env = env.clone();
                    let pattern_type = self.fresh_var();
                    self.bind_pattern_vars(&mut local_env, &arm.pattern, &pattern_type)?;

                    let arm_type = self.infer_expr(&local_env, &arm.body)?;
                    self.unify(&body_type, &arm_type, *span)?;
                }

                Ok(self.apply(&body_type))
            }

            Expr::Char(_, _) => Ok(Type::Int), // Chars are integers in Erlang

            Expr::Path(segments, span) => {
                // Path can be:
                // 1. An extern function reference: module::function
                // 2. An enum variant: Option::Some
                // 3. A qualified function call: module::function (same as 1)
                if segments.len() == 2 {
                    let module = &segments[0];
                    let name = &segments[1];

                    // Try extern function lookup
                    if let Some(scheme) = self.lookup_extern_fn(module, name).cloned() {
                        return Ok(self.instantiate(&scheme));
                    }

                    // Try enum variant lookup
                    if let Some(type_def) = self.type_defs.get(module).cloned() {
                        for (variant_name, fields) in &type_def.variants {
                            if variant_name == name {
                                // Create constructor type
                                let type_args: Vec<Type> =
                                    type_def.params.iter().map(|_| self.fresh_var()).collect();
                                let result_type =
                                    Type::Named(type_def.id, module.clone(), type_args.clone());

                                if fields.is_empty() {
                                    return Ok(result_type);
                                } else {
                                    // Substitute type params in field types
                                    let mapping: HashMap<TyVar, Type> = type_def
                                        .params
                                        .iter()
                                        .enumerate()
                                        .map(|(i, _)| (i as TyVar, type_args[i].clone()))
                                        .collect();
                                    let field_types: Vec<Type> = fields
                                        .iter()
                                        .map(|f| self.substitute_vars(f, &mapping))
                                        .collect();
                                    return Ok(Type::Function(field_types, Box::new(result_type)));
                                }
                            }
                        }
                    }
                }

                // Unknown path - return fresh var (will be caught by later unification if wrong)
                Err(TypeError::UnboundVariable(segments.join("::"), *span))
            }

            Expr::Match(scrutinee, arms, span) => {
                let scrutinee_type = self.infer_expr(env, scrutinee)?;
                let result_type = self.fresh_var();

                for arm in arms {
                    // Create environment with pattern bindings
                    let mut arm_env = env.clone();
                    self.bind_pattern_vars(
                        &mut arm_env,
                        &arm.pattern,
                        &self.apply(&scrutinee_type),
                    )?;

                    // Check guard if present
                    if let Some(guard) = &arm.guard {
                        let guard_type = self.infer_expr(&arm_env, guard)?;
                        self.unify(&guard_type, &Type::Bool, *span)?;
                    }

                    // Infer arm body and unify with result type
                    let body_type = self.infer_expr(&arm_env, &arm.body)?;
                    self.unify(&result_type, &body_type, *span)?;
                }

                let applied_scrutinee = self.apply(&scrutinee_type);
                self.check_match_redundant(&applied_scrutinee, arms)?;
                self.check_match_exhaustive(&applied_scrutinee, arms, *span)?;

                Ok(self.apply(&result_type))
            }

            Expr::StructInit(name, fields, span) => {
                // Look up struct definition
                if let Some(struct_def) = self.struct_defs.get(name).cloned() {
                    // Create fresh type variables for type parameters
                    let type_args: Vec<Type> = struct_def
                        .type_params
                        .iter()
                        .map(|_| self.fresh_var())
                        .collect();

                    // Build substitution from type params to fresh vars
                    let mapping: HashMap<TyVar, Type> = struct_def
                        .type_params
                        .iter()
                        .enumerate()
                        .map(|(i, _)| (i as TyVar, type_args[i].clone()))
                        .collect();

                    // Check each field
                    for (field_name, field_expr) in fields {
                        let expr_type = self.infer_expr(env, field_expr)?;

                        // Find the expected field type
                        if let Some((_, expected_type)) =
                            struct_def.fields.iter().find(|(n, _)| n == field_name)
                        {
                            let expected = self.substitute_vars(expected_type, &mapping);
                            self.unify(&expr_type, &expected, *span)?;
                        } else {
                            return Err(TypeError::UnboundVariable(
                                format!("{}::{}", name, field_name),
                                *span,
                            ));
                        }
                    }

                    Ok(Type::Named(struct_def.type_id, name.clone(), type_args))
                } else {
                    Err(TypeError::UnboundType(name.clone(), *span))
                }
            }

            Expr::Field(expr, field_name, _span) => {
                let expr_type = self.infer_expr(env, expr)?;
                let applied = self.apply(&expr_type);

                // Try record field access
                if let Type::Record(fields) = &applied {
                    for (name, ty) in fields {
                        if name == field_name {
                            return Ok(ty.clone());
                        }
                    }
                }

                // Try struct field access
                if let Type::Named(_, struct_name, type_args) = &applied {
                    if let Some(struct_def) = self.struct_defs.get(struct_name).cloned() {
                        // Build substitution
                        let mapping: HashMap<TyVar, Type> = struct_def
                            .type_params
                            .iter()
                            .enumerate()
                            .map(|(i, _)| {
                                (
                                    i as TyVar,
                                    type_args
                                        .get(i)
                                        .cloned()
                                        .unwrap_or_else(|| self.fresh_var()),
                                )
                            })
                            .collect();

                        for (name, ty) in &struct_def.fields {
                            if name == field_name {
                                return Ok(self.substitute_vars(ty, &mapping));
                            }
                        }
                    }
                }

                // Try tuple access for .0, .1, etc.
                if let Ok(index) = field_name.parse::<usize>() {
                    if let Type::Tuple(elem_types) = &applied {
                        if index < elem_types.len() {
                            return Ok(elem_types[index].clone());
                        }
                    }
                }

                // Unknown field - return fresh var
                Ok(self.fresh_var())
            }

            Expr::MethodCall(receiver, method_name, args, span) => {
                let receiver_type = self.infer_expr(env, receiver)?;

                // Infer argument types
                let mut arg_types: Vec<Type> = vec![receiver_type];
                for arg in args {
                    arg_types.push(self.infer_expr(env, arg)?);
                }

                // For now, method calls are treated as function calls
                // The return type is fresh
                let return_type = self.fresh_var();

                // Try to look up the method in extern functions as "Type::method"
                let applied = self.apply(&arg_types[0]);
                if let Type::Named(_, type_name, _) = &applied {
                    if let Some(scheme) = self.lookup_extern_fn(type_name, method_name).cloned() {
                        let fn_type = self.instantiate(&scheme);
                        let expected = Type::Function(arg_types, Box::new(return_type.clone()));
                        self.unify(&fn_type, &expected, *span)?;
                        return Ok(self.apply(&return_type));
                    }
                }

                Ok(self.apply(&return_type))
            }

            Expr::Record(fields, _span) => {
                let field_types: Vec<(String, Type)> = fields
                    .iter()
                    .map(|(name, expr)| {
                        let ty = self.infer_expr(env, expr)?;
                        Ok((name.clone(), ty))
                    })
                    .collect::<Result<_, TypeError>>()?;
                Ok(Type::Record(field_types))
            }

            Expr::BitString(elements, _span) => {
                for segment in elements {
                    let elem_type = self.infer_expr(env, &segment.value)?;
                    match segment.specifier {
                        crate::syntax::ast::BinarySegmentSpecifier::Integer
                        | crate::syntax::ast::BinarySegmentSpecifier::BigInteger
                        | crate::syntax::ast::BinarySegmentSpecifier::Utf8 => {
                            if segment.size.is_some()
                                || matches!(
                                    segment.specifier,
                                    crate::syntax::ast::BinarySegmentSpecifier::Utf8
                                )
                            {
                                self.unify(&elem_type, &Type::Int, segment.span)?;
                                continue;
                            }

                            let int_unify = self.unify(&elem_type, &Type::Int, segment.span);
                            if int_unify.is_err() {
                                self.unify(&elem_type, &Type::String, segment.span)?;
                            }
                        }
                        crate::syntax::ast::BinarySegmentSpecifier::Binary => {
                            self.unify(&elem_type, &Type::String, segment.span)?;
                        }
                    }
                }
                Ok(Type::String)
            }
        }
    }

    fn bind_let_pattern_vars(
        &mut self,
        env: &mut TypeEnv,
        pattern: &crate::syntax::ast::Pattern,
        ty: &Type,
    ) -> Result<(), TypeError> {
        self.bind_pattern_vars_internal(env, pattern, ty, true)
    }

    /// Bind variables from a pattern to the environment with appropriate types
    fn bind_pattern_vars(
        &mut self,
        env: &mut TypeEnv,
        pattern: &crate::syntax::ast::Pattern,
        ty: &Type,
    ) -> Result<(), TypeError> {
        self.bind_pattern_vars_internal(env, pattern, ty, false)
    }

    fn bind_pattern_vars_internal(
        &mut self,
        env: &mut TypeEnv,
        pattern: &crate::syntax::ast::Pattern,
        ty: &Type,
        generalize_vars: bool,
    ) -> Result<(), TypeError> {
        use crate::syntax::ast::Pattern;

        // Apply substitution to resolve type variables
        let resolved_ty = self.apply(ty);

        match pattern {
            Pattern::Var(name, _) => {
                let scheme = if generalize_vars {
                    self.generalize(env, &resolved_ty)
                } else {
                    Scheme::mono(resolved_ty)
                };
                env.insert(name.clone(), scheme);
                Ok(())
            }
            Pattern::Wildcard(_) => {
                // No binding needed
                Ok(())
            }
            Pattern::Int(_, span) => {
                self.unify(&resolved_ty, &Type::Int, *span)?;
                Ok(())
            }
            Pattern::Float(_, span) => {
                self.unify(&resolved_ty, &Type::Float, *span)?;
                Ok(())
            }
            Pattern::Char(_, span) => {
                self.unify(&resolved_ty, &Type::Int, *span)?;
                Ok(())
            }
            Pattern::String(_, span) => {
                self.unify(&resolved_ty, &Type::String, *span)?;
                Ok(())
            }
            Pattern::Bool(_, span) => {
                self.unify(&resolved_ty, &Type::Bool, *span)?;
                Ok(())
            }
            Pattern::Atom(_, span) => {
                self.unify(&resolved_ty, &Type::Atom, *span)?;
                Ok(())
            }
            Pattern::Tuple(pats, span) => {
                let elem_types = match &resolved_ty {
                    Type::Tuple(elem_types) if elem_types.len() == pats.len() => elem_types.clone(),
                    _ => (0..pats.len()).map(|_| self.fresh_var()).collect(),
                };
                self.unify(&resolved_ty, &Type::Tuple(elem_types.clone()), *span)?;
                for (pat, elem_ty) in pats.iter().zip(elem_types.iter()) {
                    self.bind_pattern_vars_internal(env, pat, elem_ty, generalize_vars)?;
                }
                Ok(())
            }
            Pattern::List(pats, tail, span) => {
                let elem_ty = match &resolved_ty {
                    Type::List(elem_ty) => elem_ty.as_ref().clone(),
                    _ => self.fresh_var(),
                };
                let list_ty = Type::List(Box::new(elem_ty.clone()));
                self.unify(&resolved_ty, &list_ty, *span)?;
                for pat in pats {
                    self.bind_pattern_vars_internal(env, pat, &elem_ty, generalize_vars)?;
                }
                if let Some(tail_pat) = tail {
                    self.bind_pattern_vars_internal(env, tail_pat, &list_ty, generalize_vars)?;
                }
                Ok(())
            }
            Pattern::Constructor(path, fields, span) => {
                let Some((type_name, variant_name)) = path
                    .split_last()
                    .map(|(last, rest)| (rest.last().cloned().unwrap_or_default(), last.clone()))
                else {
                    return Err(TypeError::UnboundVariable("".to_string(), *span));
                };
                let Some(type_def) = self.type_defs.get(&type_name).cloned() else {
                    return Err(TypeError::UnboundType(type_name, *span));
                };
                let Some((_, field_types)) = type_def
                    .variants
                    .iter()
                    .find(|(name, _)| name == &variant_name)
                else {
                    return Err(TypeError::UnboundVariable(path.join("::"), *span));
                };
                if field_types.len() != fields.len() {
                    return Err(TypeError::ArityMismatch(
                        field_types.len(),
                        fields.len(),
                        *span,
                    ));
                }

                let type_args: Vec<Type> =
                    type_def.params.iter().map(|_| self.fresh_var()).collect();
                let result_type = Type::Named(type_def.id, type_name, type_args.clone());
                self.unify(&resolved_ty, &result_type, *span)?;

                let mapping: HashMap<TyVar, Type> = type_def
                    .params
                    .iter()
                    .enumerate()
                    .map(|(i, _)| (i as TyVar, type_args[i].clone()))
                    .collect();
                for (pat, field_type) in fields.iter().zip(field_types.iter()) {
                    let expected = self.substitute_vars(field_type, &mapping);
                    self.bind_pattern_vars_internal(env, pat, &expected, generalize_vars)?;
                }
                Ok(())
            }
            Pattern::Record(fields, span) => {
                let record_fields = match &resolved_ty {
                    Type::Record(existing_fields) => existing_fields.clone(),
                    _ => fields
                        .iter()
                        .map(|(name, _)| (name.clone(), self.fresh_var()))
                        .collect(),
                };
                self.unify(&resolved_ty, &Type::Record(record_fields.clone()), *span)?;
                for (name, pat) in fields {
                    let Some((_, field_ty)) = record_fields.iter().find(|(field, _)| field == name)
                    else {
                        return Err(TypeError::FieldNotFound(name.clone(), *span));
                    };
                    self.bind_pattern_vars_internal(env, pat, field_ty, generalize_vars)?;
                }
                Ok(())
            }
            Pattern::BitString(segments, span) => {
                self.unify(&resolved_ty, &Type::String, *span)?;
                for (index, segment) in segments.iter().enumerate() {
                    let is_tail_binary = matches!(
                        segment.specifier,
                        crate::syntax::ast::BinarySegmentSpecifier::Binary
                    ) || (segment.size.is_none()
                        && index + 1 == segments.len()
                        && matches!(segment.value, Pattern::Var(_, _) | Pattern::Wildcard(_)));
                    let segment_ty = if is_tail_binary
                        || matches!(
                            segment.specifier,
                            crate::syntax::ast::BinarySegmentSpecifier::Binary
                        )
                        || (segment.size.is_none()
                            && matches!(
                                segment.value,
                                Pattern::String(_, _) | Pattern::BitString(_, _)
                            )) {
                        Type::String
                    } else {
                        Type::Int
                    };
                    self.bind_pattern_vars_internal(
                        env,
                        &segment.value,
                        &segment_ty,
                        generalize_vars,
                    )?;
                }
                Ok(())
            }
            Pattern::Or(left, right, _) => {
                let mut left_env = env.clone();
                self.bind_pattern_vars_internal(
                    &mut left_env,
                    left,
                    &resolved_ty,
                    generalize_vars,
                )?;
                self.bind_pattern_vars_internal(env, right, &resolved_ty, generalize_vars)?;
                Ok(())
            }
        }
    }

    fn check_match_exhaustive(
        &self,
        scrutinee_type: &Type,
        arms: &[MatchArm],
        span: Span,
    ) -> Result<(), TypeError> {
        match scrutinee_type {
            Type::Bool => {
                let mut seen_true = false;
                let mut seen_false = false;
                for arm in arms {
                    if !matches!(guard_truth(arm.guard.as_deref()), GuardTruth::AlwaysTrue) {
                        continue;
                    }
                    let (covers_true, covers_false) = bool_pattern_coverage(&arm.pattern);
                    seen_true |= covers_true;
                    seen_false |= covers_false;
                }

                let mut missing = Vec::new();
                if !seen_true {
                    missing.push("true".to_string());
                }
                if !seen_false {
                    missing.push("false".to_string());
                }
                if missing.is_empty() {
                    Ok(())
                } else {
                    Err(TypeError::NonExhaustiveMatch(missing, span))
                }
            }
            Type::Named(type_id, type_name, _) => {
                let Some(type_def) = self.type_defs.get(type_name) else {
                    return Ok(());
                };
                if type_def.id != *type_id {
                    return Ok(());
                }

                let mut seen = std::collections::HashSet::new();
                let all_variants: std::collections::HashSet<String> = type_def
                    .variants
                    .iter()
                    .map(|(variant, _)| format!("{type_name}::{variant}"))
                    .collect();
                for arm in arms {
                    if !matches!(guard_truth(arm.guard.as_deref()), GuardTruth::AlwaysTrue) {
                        continue;
                    }
                    if pattern_is_catch_all(&arm.pattern) {
                        seen = all_variants.clone();
                    } else {
                        enum_pattern_coverage(&arm.pattern, type_name, &mut seen);
                    }
                }

                let missing: Vec<String> = type_def
                    .variants
                    .iter()
                    .map(|(variant, _)| format!("{type_name}::{variant}"))
                    .filter(|variant| !seen.contains(variant))
                    .collect();
                if missing.is_empty() {
                    Ok(())
                } else {
                    Err(TypeError::NonExhaustiveMatch(missing, span))
                }
            }
            _ => Ok(()),
        }
    }

    fn check_match_redundant(
        &self,
        scrutinee_type: &Type,
        arms: &[MatchArm],
    ) -> Result<(), TypeError> {
        match scrutinee_type {
            Type::Bool => {
                let mut seen_true = false;
                let mut seen_false = false;
                for arm in arms {
                    let guard_truth = guard_truth(arm.guard.as_deref());
                    if matches!(guard_truth, GuardTruth::AlwaysFalse) {
                        return Err(TypeError::RedundantMatchArm(arm.span));
                    }
                    let (covers_true, covers_false) = bool_pattern_coverage(&arm.pattern);
                    if (!covers_true || seen_true) && (!covers_false || seen_false) {
                        return Err(TypeError::RedundantMatchArm(arm.span));
                    }
                    if matches!(guard_truth, GuardTruth::AlwaysTrue) {
                        seen_true |= covers_true;
                        seen_false |= covers_false;
                    }
                }
                Ok(())
            }
            Type::Named(type_id, type_name, _) => {
                let Some(type_def) = self.type_defs.get(type_name) else {
                    return Ok(());
                };
                if type_def.id != *type_id {
                    return Ok(());
                }

                let mut seen = std::collections::HashSet::new();
                let all_variants: std::collections::HashSet<String> = type_def
                    .variants
                    .iter()
                    .map(|(variant, _)| format!("{type_name}::{variant}"))
                    .collect();
                for arm in arms {
                    let guard_truth = guard_truth(arm.guard.as_deref());
                    if matches!(guard_truth, GuardTruth::AlwaysFalse) {
                        return Err(TypeError::RedundantMatchArm(arm.span));
                    }
                    let mut arm_coverage = std::collections::HashSet::new();
                    if pattern_is_catch_all(&arm.pattern) {
                        arm_coverage = all_variants.clone();
                    } else {
                        enum_pattern_coverage(&arm.pattern, type_name, &mut arm_coverage);
                    }
                    if arm_coverage.is_subset(&seen) {
                        return Err(TypeError::RedundantMatchArm(arm.span));
                    }
                    if matches!(guard_truth, GuardTruth::AlwaysTrue) {
                        seen.extend(arm_coverage);
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GuardTruth {
    AlwaysTrue,
    AlwaysFalse,
    Unknown,
}

fn pattern_is_catch_all(pattern: &Pattern) -> bool {
    match pattern {
        Pattern::Wildcard(_) | Pattern::Var(_, _) => true,
        Pattern::Or(left, right, _) => pattern_is_catch_all(left) || pattern_is_catch_all(right),
        _ => false,
    }
}

fn guard_truth(guard: Option<&Expr>) -> GuardTruth {
    match guard.and_then(eval_const_bool) {
        Some(true) => GuardTruth::AlwaysTrue,
        Some(false) => GuardTruth::AlwaysFalse,
        None => {
            if guard.is_none() {
                GuardTruth::AlwaysTrue
            } else {
                GuardTruth::Unknown
            }
        }
    }
}

fn eval_const_bool(expr: &Expr) -> Option<bool> {
    match expr {
        Expr::Bool(value, _) => Some(*value),
        Expr::Unary(UnaryOp::Not, inner, _) => eval_const_bool(inner).map(|value| !value),
        Expr::Binary(left, BinOp::And, right, _) => {
            Some(eval_const_bool(left)? && eval_const_bool(right)?)
        }
        Expr::Binary(left, BinOp::Or, right, _) => {
            Some(eval_const_bool(left)? || eval_const_bool(right)?)
        }
        Expr::Binary(left, BinOp::Eq, right, _) => {
            Some(eval_const_value(left)? == eval_const_value(right)?)
        }
        Expr::Binary(left, BinOp::NotEq, right, _) => {
            Some(eval_const_value(left)? != eval_const_value(right)?)
        }
        Expr::Binary(left, BinOp::Lt, right, _) => {
            Some(eval_const_order(left)? < eval_const_order(right)?)
        }
        Expr::Binary(left, BinOp::LtEq, right, _) => {
            Some(eval_const_order(left)? <= eval_const_order(right)?)
        }
        Expr::Binary(left, BinOp::Gt, right, _) => {
            Some(eval_const_order(left)? > eval_const_order(right)?)
        }
        Expr::Binary(left, BinOp::GtEq, right, _) => {
            Some(eval_const_order(left)? >= eval_const_order(right)?)
        }
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum ConstValue {
    Int(i64),
    Bool(bool),
    Char(char),
    String(String),
    Atom(String),
    Unit,
}

fn eval_const_value(expr: &Expr) -> Option<ConstValue> {
    match expr {
        Expr::Int(value, _) => Some(ConstValue::Int(*value)),
        Expr::Bool(value, _) => Some(ConstValue::Bool(*value)),
        Expr::Char(value, _) => Some(ConstValue::Char(*value)),
        Expr::String(value, _) => Some(ConstValue::String(value.clone())),
        Expr::Atom(value, _) => Some(ConstValue::Atom(value.clone())),
        Expr::Unit(_) => Some(ConstValue::Unit),
        _ => None,
    }
}

fn eval_const_order(expr: &Expr) -> Option<ConstValue> {
    eval_const_value(expr)
}

fn bool_pattern_coverage(pattern: &Pattern) -> (bool, bool) {
    match pattern {
        Pattern::Wildcard(_) | Pattern::Var(_, _) => (true, true),
        Pattern::Bool(value, _) => (*value, !*value),
        Pattern::Or(left, right, _) => {
            let left_cov = bool_pattern_coverage(left);
            let right_cov = bool_pattern_coverage(right);
            (left_cov.0 || right_cov.0, left_cov.1 || right_cov.1)
        }
        _ => (false, false),
    }
}

fn enum_pattern_coverage(
    pattern: &Pattern,
    type_name: &str,
    seen: &mut std::collections::HashSet<String>,
) {
    match pattern {
        Pattern::Constructor(path, _, _) => {
            if path.len() >= 2 && path[path.len() - 2] == type_name {
                seen.insert(format!("{}::{}", type_name, path[path.len() - 1]));
            }
        }
        Pattern::Or(left, right, _) => {
            enum_pattern_coverage(left, type_name, seen);
            enum_pattern_coverage(right, type_name, seen);
        }
        Pattern::Wildcard(_) | Pattern::Var(_, _) => {
            // handled earlier as catch-all
        }
        _ => {}
    }
}

impl Default for InferenceContext {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::ast::Pattern;
    use crate::syntax::span::Span;
    use crate::types::types::Scheme;

    #[test]
    fn record_patterns_bind_field_types() {
        let mut ctx = InferenceContext::new();
        let mut env = TypeEnv::new();
        let span = Span::empty();
        let pattern = Pattern::Record(
            vec![("x".to_string(), Pattern::Var("value".to_string(), span))],
            span,
        );
        let record_type = Type::Record(vec![("x".to_string(), Type::Int)]);

        ctx.bind_pattern_vars(&mut env, &pattern, &record_type)
            .unwrap();

        let scheme = env.lookup("value").unwrap();
        assert_eq!(ctx.apply(&scheme.ty), Type::Int);
    }

    #[test]
    fn instantiate_substitutes_type_vars_inside_maps() {
        let mut ctx = InferenceContext::new();
        let scheme = Scheme::poly(
            vec![7],
            Type::Named(
                1,
                "DynamicResult".to_string(),
                vec![Type::Map(Box::new(Type::String), Box::new(Type::Var(7)))],
            ),
        );

        let instantiated = ctx.instantiate(&scheme);
        match instantiated {
            Type::Named(_, name, args) => {
                assert_eq!(name, "DynamicResult");
                let Type::Map(key, value) = &args[0] else {
                    panic!("expected map payload");
                };
                assert_eq!(**key, Type::String);
                assert!(matches!(**value, Type::Var(v) if v != 7));
            }
            _ => panic!("expected named type"),
        }
    }
}
