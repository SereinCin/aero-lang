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
    /// 32-bit float (single precision)
    F32,
    /// 64-bit float (double precision, default type of float literals)
    F64,
    /// Unicode character (32-bit)
    Char,
    /// Boolean
    Bool,
    /// Immutable string
    Str,
    /// Tuple `(T1, T2, ...)`
    Tuple(Vec<Ty>),
    /// Fixed-size array `[T; N]`, length known at compile time
    Array(Box<Ty>, usize),
    /// Reference `&T` (mut_=false) / `&mut T` (mut_=true). `lifetime` is the
    /// optional named lifetime (`'a`, Phase 10); None for elision.
    Ref { mut_: bool, lifetime: Option<String>, inner: Box<Ty> },
    /// Raw pointer `*T`
    Ptr(Box<Ty>),
    /// Arena allocator `arena(N)` (N = byte capacity)
    Arena(usize),
    /// N-dimensional tensor `tensor<D1xD2x...xDn>` (Campaign 3). Shape is
    /// statically fixed at compile time; dimension checks (e.g. matmul) run
    /// during type inference.
    Tensor { elem: Box<Ty>, shape: Vec<usize> },
    /// Named struct type (definition looked up by name in HirProgram.structs)
    Struct(String),
    /// Named union type (definition looked up by name in HirProgram.unions).
    /// All fields share the same storage; layout is that of the largest field.
    Union(String),
    /// Generic struct instance `Name<Arg1, Arg2, ...>` (e.g. `Vec<i64>`).
    /// `args` holds the concrete type arguments; the definition is looked up by
    /// `name` in HirProgram.structs (whose `type_params` align with `args`).
    StructGeneric { name: String, args: Vec<Ty> },
    /// Named enum type (definition looked up by name in HirProgram.enums)
    Enum(String),
    /// Generic enum instance `Name<Arg1, Arg2, ...>` (e.g. `Maybe<i64>`).
    EnumGeneric { name: String, args: Vec<Ty> },
    /// Native growable heap vector `Vec<T>`. Stored as `{ data: i8*, len: i64,
    /// cap: i64 }`; the buffer is malloc-managed (see `aero-ir` codegen).
    Vec(Box<Ty>),
    /// Native growable heap string `String`. Stored as `{ data: i8*, len: i64,
    /// cap: i64 }`; the buffer is malloc-managed and always NUL-terminated at
    /// `data[len]`. Unlike `str` (an `i8*` C string), `String` tracks its byte
    /// length, enabling O(1) `len` and embedded-NUL-safe buffers.
    String,
    /// Native smart pointer `Box<T>` (Phase 11). A single `i8*` to a
    /// malloc-allocated copy of a `T`. Owning and non-Copy: `free` releases the
    /// allocation; deref reads through the pointer. `Box::new(value)` allocates
    /// and copies `value` onto the heap.
    Box(Box<Ty>),
    /// Function signature `(T1, T2, ...) -> R`
    Fn(Vec<Ty>, Box<Ty>),
    /// No return value (internal only; not user-annotatable)
    Void,
    /// Generic type parameter (appears only in generic signatures and bodies;
    /// substituted with concrete types at instantiation)
    Generic(String),
    /// Associated type reference `Self::Item`. Appears only in trait method
    /// signatures (e.g. `Option<Self::Item>`); substituted with the impl's
    /// concrete binding when the trait signature is validated against an impl.
    Assoc(String),
    /// Type variable (intermediate inference state)
    Var(TypeVar),
    /// Dynamic trait object `dyn Trait` (Phase 9). A fat pointer laid out as
    /// `{ data: i8*, vtable: i8* }`: `data` points to the heap-allocated concrete
    /// value, `vtable` points to a function-pointer table (one entry per trait
    /// method, in declaration order). Method calls go through the vtable
    /// (indirect dispatch). The cast `x as dyn Trait` boxes `x` on the heap.
    Dyn { trait_name: String },
}

impl Ty {
    /// Whether this type is an integer family member (i32 / i64, or an
    /// undetermined variable that may resolve to an integer).
    pub fn is_int(&self) -> bool {
        matches!(self, Ty::I32 | Ty::I64 | Ty::Var(_))
    }

    /// Whether this type is a float family member (f32 / f64)
    pub fn is_float(&self) -> bool {
        matches!(self, Ty::F32 | Ty::F64 | Ty::Var(_))
    }

    /// Whether this type is numeric (integer or float)
    pub fn is_numeric(&self) -> bool {
        matches!(self, Ty::I32 | Ty::I64 | Ty::F32 | Ty::F64 | Ty::Char | Ty::Var(_))
    }

    /// Whether this is a user-defined named type (struct or enum, including
    /// generic instances). Used to dispatch operator overloading (`Add`/`Eq`/`Ord`).
    pub fn is_named_type(&self) -> bool {
        matches!(
            self,
            Ty::Struct(_) | Ty::Union(_) | Ty::Enum(_) | Ty::StructGeneric { .. } | Ty::EnumGeneric { .. }
        )
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
            Ty::F32 => write!(f, "f32"),
            Ty::F64 => write!(f, "f64"),
            Ty::Char => write!(f, "char"),
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
            Ty::Ref {
                mut_,
                lifetime,
                inner,
            } => {
                if *mut_ {
                    if let Some(lt) = lifetime {
                        write!(f, "&{} mut {inner}", lt)
                    } else {
                        write!(f, "&mut {inner}")
                    }
                } else if let Some(lt) = lifetime {
                    write!(f, "&{} {inner}", lt)
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
            Ty::Struct(name) => write!(f, "{name}"),
            Ty::Union(name) => write!(f, "{name}"),
            Ty::Enum(name) => write!(f, "{name}"),
            Ty::StructGeneric { name, args } => {
                write!(f, "{name}<")?;
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{a}")?;
                }
                write!(f, ">")
            }
            Ty::EnumGeneric { name, args } => {
                write!(f, "{name}<")?;
                for (i, a) in args.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{a}")?;
                }
                write!(f, ">")
            }
            Ty::Vec(elem) => write!(f, "Vec<{elem}>"),
            Ty::String => write!(f, "String"),
            Ty::Box(inner) => write!(f, "Box<{inner}>"),
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
            Ty::Assoc(name) => write!(f, "Self::{name}"),
            Ty::Dyn { trait_name } => write!(f, "dyn {trait_name}"),
        }
    }
}
