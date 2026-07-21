use std::collections::HashMap;

/// Type variable identifier
pub type TyVar = u32;

/// Unique type ID for nominal types
pub type TypeId = u32;

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    // Primitives
    Int,
    Float,
    Bool,
    String,
    Atom,
    Unit,

    // Type variable (for inference)
    Var(TyVar),

    // Compound types
    Tuple(Vec<Type>),
    List(Box<Type>),
    Map(Box<Type>, Box<Type>), // Map<K, V>
    Function(Vec<Type>, Box<Type>),
    Record(Vec<(String, Type)>),

    // Nominal types (enums, type aliases)
    Named(TypeId, String, Vec<Type>), // id, name, type args

    // Erlang-specific
    Pid,
    Ref,
    Dynamic, // runtime dynamic data boundary
    Any,     // escape hatch for FFI
    Never,   // for non-returning functions
}

impl Type {
    pub fn is_concrete(&self) -> bool {
        match self {
            Type::Var(_) => false,
            Type::Tuple(ts) => ts.iter().all(|t| t.is_concrete()),
            Type::List(t) => t.is_concrete(),
            Type::Map(k, v) => k.is_concrete() && v.is_concrete(),
            Type::Function(params, ret) => {
                params.iter().all(|t| t.is_concrete()) && ret.is_concrete()
            }
            Type::Record(fields) => fields.iter().all(|(_, t)| t.is_concrete()),
            Type::Named(_, _, args) => args.iter().all(|t| t.is_concrete()),
            _ => true,
        }
    }
}

/// A polymorphic type scheme: forall a b. Type
#[derive(Debug, Clone)]
pub struct Scheme {
    pub vars: Vec<TyVar>,
    pub ty: Type,
}

impl Scheme {
    pub fn mono(ty: Type) -> Self {
        Scheme { vars: vec![], ty }
    }

    pub fn poly(vars: Vec<TyVar>, ty: Type) -> Self {
        Scheme { vars, ty }
    }
}

/// Substitution mapping type variables to types
#[derive(Debug, Clone, Default)]
pub struct Substitution {
    mapping: HashMap<TyVar, Type>,
}

impl Substitution {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, var: TyVar, ty: Type) {
        self.mapping.insert(var, ty);
    }

    pub fn get(&self, var: TyVar) -> Option<&Type> {
        self.mapping.get(&var)
    }

    pub fn apply(&self, ty: &Type) -> Type {
        match ty {
            Type::Var(v) => self
                .mapping
                .get(v)
                .cloned()
                .map_or(ty.clone(), |t| self.apply(&t)),
            Type::Function(params, ret) => Type::Function(
                params.iter().map(|p| self.apply(p)).collect(),
                Box::new(self.apply(ret)),
            ),
            Type::Tuple(ts) => Type::Tuple(ts.iter().map(|t| self.apply(t)).collect()),
            Type::List(elem) => Type::List(Box::new(self.apply(elem))),
            Type::Map(k, v) => Type::Map(Box::new(self.apply(k)), Box::new(self.apply(v))),
            Type::Record(fields) => Type::Record(
                fields
                    .iter()
                    .map(|(n, t)| (n.clone(), self.apply(t)))
                    .collect(),
            ),
            Type::Named(id, name, args) => Type::Named(
                *id,
                name.clone(),
                args.iter().map(|a| self.apply(a)).collect(),
            ),
            _ => ty.clone(),
        }
    }

    pub fn compose(&self, other: &Substitution) -> Substitution {
        let mut result = Substitution::new();
        for (v, t) in &other.mapping {
            result.insert(*v, self.apply(t));
        }
        for (v, t) in &self.mapping {
            if !result.mapping.contains_key(v) {
                result.insert(*v, t.clone());
            }
        }
        result
    }
}
