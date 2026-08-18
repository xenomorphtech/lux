#[allow(unused_imports)]
use crate::prelude::*;
use crate::types::types::{Scheme, Substitution, TyVar, Type};
use crate::collections::HashMap;

/// Type environment mapping names to type schemes
#[derive(Debug, Clone, Default)]
pub struct TypeEnv {
    bindings: HashMap<String, Scheme>,
    function_bindings: HashMap<String, Scheme>,
}

impl TypeEnv {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, name: String, scheme: Scheme) {
        self.bindings.insert(name, scheme);
    }

    pub fn insert_function(&mut self, name: String, scheme: Scheme) {
        self.function_bindings.insert(name, scheme);
    }

    pub fn lookup(&self, name: &str) -> Option<&Scheme> {
        self.bindings.get(name)
    }

    pub fn lookup_function(&self, name: &str) -> Option<&Scheme> {
        self.function_bindings.get(name)
    }

    pub fn lookup_name(&self, name: &str) -> Option<Scheme> {
        if let Some(scheme) = self.lookup(name) {
            return Some(scheme.clone());
        }

        self.function_bindings.get(name).cloned()
    }

    pub fn extend(&self, name: String, scheme: Scheme) -> Self {
        let mut new = self.clone();
        new.insert(name, scheme);
        new
    }

    pub fn remove(&mut self, name: &str) {
        self.bindings.remove(name);
    }

    /// Get all free type variables in the environment
    pub fn free_vars(&self) -> crate::collections::HashSet<TyVar> {
        let mut vars = crate::collections::HashSet::new();
        for scheme in self.bindings.values() {
            let scheme_vars = free_vars_in_type(&scheme.ty);
            for v in scheme_vars {
                if !scheme.vars.contains(&v) {
                    vars.insert(v);
                }
            }
        }
        for scheme in self.function_bindings.values() {
            let scheme_vars = free_vars_in_type(&scheme.ty);
            for v in scheme_vars {
                if !scheme.vars.contains(&v) {
                    vars.insert(v);
                }
            }
        }
        vars
    }

    pub fn apply(&self, subst: &Substitution) -> Self {
        let mut new = TypeEnv::new();
        for (name, scheme) in &self.bindings {
            let new_ty = subst.apply(&scheme.ty);
            new.insert(
                name.clone(),
                Scheme {
                    vars: scheme.vars.clone(),
                    ty: new_ty,
                },
            );
        }
        for (name, scheme) in &self.function_bindings {
            let new_ty = subst.apply(&scheme.ty);
            new.insert_function(
                name.clone(),
                Scheme {
                    vars: scheme.vars.clone(),
                    ty: new_ty,
                },
            );
        }
        new
    }
}

/// Get free type variables in a type
pub fn free_vars_in_type(ty: &Type) -> crate::collections::HashSet<TyVar> {
    let mut vars = crate::collections::HashSet::new();
    collect_free_vars(ty, &mut vars);
    vars
}

fn collect_free_vars(ty: &Type, vars: &mut crate::collections::HashSet<TyVar>) {
    match ty {
        Type::Var(v) => {
            vars.insert(*v);
        }
        Type::Function(params, ret) => {
            for p in params {
                collect_free_vars(p, vars);
            }
            collect_free_vars(ret, vars);
        }
        Type::Tuple(ts) => {
            for t in ts {
                collect_free_vars(t, vars);
            }
        }
        Type::List(elem) => {
            collect_free_vars(elem, vars);
        }
        Type::Map(key, value) => {
            collect_free_vars(key, vars);
            collect_free_vars(value, vars);
        }
        Type::Record(fields) => {
            for (_, t) in fields {
                collect_free_vars(t, vars);
            }
        }
        Type::Named(_, _, args) => {
            for a in args {
                collect_free_vars(a, vars);
            }
        }
        _ => {}
    }
}
