use crate::syntax::span::Span;
use crate::types::types::{Substitution, TyVar, Type};

#[derive(Debug, Clone)]
pub struct MismatchContext {
    pub function: Option<String>,
    pub expression: Option<String>,
}

#[derive(Debug, Clone)]
pub enum TypeError {
    Mismatch(Type, Type, Span, Option<MismatchContext>),
    InfiniteType(TyVar, Type, Span),
    ArityMismatch(usize, usize, Span),
    NonExhaustiveMatch(Vec<String>, Span),
    RedundantMatchArm(Span),
    UnboundVariable(String, Span),
    UnboundType(String, Span),
    FieldNotFound(String, Span),
    NotAFunction(Type, Span),
    NotARecord(Type, Span),
}

impl std::fmt::Display for TypeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TypeError::Mismatch(t1, t2, _, context) => {
                write!(f, "Type mismatch: expected {:?}, found {:?}", t1, t2)?;
                if let Some(context) = context {
                    if let Some(function) = &context.function {
                        write!(f, "\nFunction: {}", function)?;
                    }
                    if let Some(expression) = &context.expression {
                        write!(f, "\nExpression: {}", expression)?;
                    }
                }
                Ok(())
            }
            TypeError::InfiniteType(v, t, _) => {
                write!(f, "Infinite type: {} occurs in {:?}", v, t)
            }
            TypeError::ArityMismatch(expected, found, _) => {
                write!(
                    f,
                    "Arity mismatch: expected {} args, found {}",
                    expected, found
                )
            }
            TypeError::NonExhaustiveMatch(missing, _) => {
                write!(f, "Non-exhaustive match; missing: {}", missing.join(", "))
            }
            TypeError::RedundantMatchArm(_) => {
                write!(f, "Redundant match arm")
            }
            TypeError::UnboundVariable(name, _) => {
                write!(f, "Unbound variable: {}", name)
            }
            TypeError::UnboundType(name, _) => {
                write!(f, "Unbound type: {}", name)
            }
            TypeError::FieldNotFound(name, _) => {
                write!(f, "Field not found: {}", name)
            }
            TypeError::NotAFunction(t, _) => {
                write!(f, "Not a function: {:?}", t)
            }
            TypeError::NotARecord(t, _) => {
                write!(f, "Not a record: {:?}", t)
            }
        }
    }
}

impl std::error::Error for TypeError {}

impl TypeError {
    pub fn with_mismatch_context(
        self,
        function: impl Into<String>,
        expression: Option<String>,
    ) -> Self {
        match self {
            TypeError::Mismatch(expected, found, span, _) => TypeError::Mismatch(
                expected,
                found,
                span,
                Some(MismatchContext {
                    function: Some(function.into()),
                    expression,
                }),
            ),
            other => other,
        }
    }
}

/// Unify two types, returning a substitution that makes them equal
pub fn unify(t1: &Type, t2: &Type, span: Span) -> Result<Substitution, TypeError> {
    match (t1, t2) {
        // Identical primitive types
        (Type::Int, Type::Int)
        | (Type::Float, Type::Float)
        | (Type::Bool, Type::Bool)
        | (Type::String, Type::String)
        | (Type::Atom, Type::Atom)
        | (Type::Unit, Type::Unit)
        | (Type::Pid, Type::Pid)
        | (Type::Ref, Type::Ref)
        | (Type::Dynamic, Type::Dynamic)
        | (Type::Any, _)
        | (_, Type::Any)
        | (Type::Never, Type::Never) => Ok(Substitution::new()),

        // Type variables
        (Type::Var(v), t) | (t, Type::Var(v)) => {
            if let Type::Var(v2) = t {
                if v == v2 {
                    return Ok(Substitution::new());
                }
            }
            if occurs_check(*v, t) {
                return Err(TypeError::InfiniteType(*v, t.clone(), span));
            }
            let mut subst = Substitution::new();
            subst.insert(*v, t.clone());
            Ok(subst)
        }

        // Functions
        (Type::Function(params1, ret1), Type::Function(params2, ret2)) => {
            if params1.len() != params2.len() {
                return Err(TypeError::ArityMismatch(params1.len(), params2.len(), span));
            }
            let mut subst = Substitution::new();
            for (p1, p2) in params1.iter().zip(params2.iter()) {
                let s = unify(&subst.apply(p1), &subst.apply(p2), span)?;
                subst = subst.compose(&s);
            }
            let s = unify(&subst.apply(ret1), &subst.apply(ret2), span)?;
            Ok(subst.compose(&s))
        }

        // Tuples
        (Type::Tuple(ts1), Type::Tuple(ts2)) => {
            if ts1.len() != ts2.len() {
                return Err(TypeError::Mismatch(t1.clone(), t2.clone(), span, None));
            }
            let mut subst = Substitution::new();
            for (t1, t2) in ts1.iter().zip(ts2.iter()) {
                let s = unify(&subst.apply(t1), &subst.apply(t2), span)?;
                subst = subst.compose(&s);
            }
            Ok(subst)
        }

        // Lists
        (Type::List(elem1), Type::List(elem2)) => unify(elem1, elem2, span),

        // Maps
        (Type::Map(k1, v1), Type::Map(k2, v2)) => {
            let subst = unify(k1, k2, span)?;
            let s2 = unify(&subst.apply(v1), &subst.apply(v2), span)?;
            Ok(subst.compose(&s2))
        }

        // Records (structural)
        (Type::Record(fields1), Type::Record(fields2)) => {
            if fields1.len() != fields2.len() {
                return Err(TypeError::Mismatch(t1.clone(), t2.clone(), span, None));
            }
            let mut subst = Substitution::new();
            for (name, ty1) in fields1 {
                let ty2 = fields2
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, t)| t)
                    .ok_or_else(|| TypeError::FieldNotFound(name.clone(), span))?;
                let s = unify(&subst.apply(ty1), &subst.apply(ty2), span)?;
                subst = subst.compose(&s);
            }
            Ok(subst)
        }

        // Named types (nominal)
        (Type::Named(id1, _, args1), Type::Named(id2, _, args2)) if id1 == id2 => {
            if args1.len() != args2.len() {
                return Err(TypeError::Mismatch(t1.clone(), t2.clone(), span, None));
            }
            let mut subst = Substitution::new();
            for (a1, a2) in args1.iter().zip(args2.iter()) {
                let s = unify(&subst.apply(a1), &subst.apply(a2), span)?;
                subst = subst.compose(&s);
            }
            Ok(subst)
        }

        _ => Err(TypeError::Mismatch(t1.clone(), t2.clone(), span, None)),
    }
}

/// Check if a type variable occurs in a type (prevents infinite types)
fn occurs_check(var: TyVar, ty: &Type) -> bool {
    match ty {
        Type::Var(v) => *v == var,
        Type::Function(params, ret) => {
            params.iter().any(|p| occurs_check(var, p)) || occurs_check(var, ret)
        }
        Type::Tuple(ts) => ts.iter().any(|t| occurs_check(var, t)),
        Type::List(elem) => occurs_check(var, elem),
        Type::Map(key, value) => occurs_check(var, key) || occurs_check(var, value),
        Type::Record(fields) => fields.iter().any(|(_, t)| occurs_check(var, t)),
        Type::Named(_, _, args) => args.iter().any(|a| occurs_check(var, a)),
        _ => false,
    }
}
