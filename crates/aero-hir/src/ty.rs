/// Aero's type system: the foundation built in Campaign 1.
///
/// Design notes:
/// - Base types: `I32` / `I64` / `Bool` / `Str`
/// - Composite types: `Tuple` / fixed-size `Array`
/// - Function signatures: `Fn(params, ret)`
/// - References: `Ref { mut_, inner }` — the semantic basis of the borrow
///   checker (Campaign 2)
/// - Raw pointers: `Ptr(inner)` — `*i64` returned by the arena allocator
///   (Campaign 2)
/// - Arenas: `Arena(size)` — GC-free bump allocator (Campaign 2)
/// - `Var(TypeVar)`: type variables for the constraint-unification inference
///   engine (paving the way for generics)
/// - `Void`: internal marker for void-returning functions; not user-annotatable
///
/// Ownership design (Campaign 2 decision): Rust-style ownership + borrows +
/// moves. All base types and aggregates (arrays/tuples) use value semantics;
/// references cannot be re-borrowed; arenas cannot be copied or moved.
use std::fmt;

/// Unique id of a type variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeVar(pub u32);

/// Aero's static type.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Ty {
    /// 32-bit signed integer
    I32,
    /// 64-bit signed integer (default type of integer literals)
    I64,
    /// Boolean
    Bool,
    /// Immutable string
    Str,
    /// Tuple `(T1, T2, ...)`
    Tuple(Vec<Ty>),
    /// Fixed-size array `[T; N]`, length known at compile time
    Array(Box<Ty>, usize),
    /// Reference `&T` (mut_=false) / `&mut T` (mut_=true)
    Ref { mut_: bool, inner: Box<Ty> },
    /// Raw pointer `*T`
    Ptr(Box<Ty>),
    /// Arena allocator `arena(N)` (N = byte capacity)
    Arena(usize),
    /// N-dimensional tensor `tensor<D1xD2x...xDn>` (Campaign 3). Shape is
    /// statically fixed at compile time; dimension checks (e.g. matmul) run
    /// during type inference.
    Tensor { elem: Box<Ty>, shape: Vec<usize> },
    /// Function signature `(T1, T2, ...) -> R`
    Fn(Vec<Ty>, Box<Ty>),
    /// No return value (internal only; not user-annotatable)
    Void,
    /// Generic type parameter (appears only in generic signatures and bodies;
    /// substituted with concrete types at instantiation)
    Generic(String),
    /// Type variable (intermediate inference state)
    Var(TypeVar),
}

impl Ty {
    /// Whether this type is an integer family member (i32 / i64, or an
    /// undetermined variable that may resolve to an integer).
    pub fn is_int(&self) -> bool {
        matches!(self, Ty::I32 | Ty::I64 | Ty::Var(_))
    }

    /// Whether this is an ordinary value type that may be borrowed
    /// (references/pointers/arenas cannot be re-borrowed). Generic parameters
    /// are conservatively borrowable (re-checked with the concrete type at
    /// instantiation).
    pub fn is_borrowable(&self) -> bool {
        !matches!(self, Ty::Ref { .. } | Ty::Ptr(_) | Ty::Arena(_))
    }
}

impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ty::I32 => write!(f, "i32"),
            Ty::I64 => write!(f, "i64"),
            Ty::Bool => write!(f, "bool"),
            Ty::Str => write!(f, "str"),
            Ty::Void => write!(f, "void"),
            Ty::Tuple(elems) => {
                write!(f, "(")?;
                for (i, ty) in elems.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{ty}")?;
                }
                write!(f, ")")
            }
            Ty::Array(elem, n) => write!(f, "[{elem}; {n}]"),
            Ty::Ref { mut_, inner } => {
                if *mut_ {
                    write!(f, "&mut {inner}")
                } else {
                    write!(f, "&{inner}")
                }
            }
            Ty::Ptr(inner) => write!(f, "*{inner}"),
            Ty::Arena(size) => write!(f, "arena({size})"),
            Ty::Tensor { elem, shape } => {
                write!(f, "tensor<")?;
                for (i, d) in shape.iter().enumerate() {
                    if i > 0 {
                        write!(f, "x")?;
                    }
                    write!(f, "{d}")?;
                }
                write!(f, "x{elem}>")
            }
            Ty::Fn(params, ret) => {
                write!(f, "fn(")?;
                for (i, ty) in params.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{ty}")?;
                }
                write!(f, ") -> {ret}")
            }
            Ty::Var(_) => write!(f, "<uninferred type>"),
            Ty::Generic(name) => write!(f, "{name}"),
        }
    }
}
