/// HIR (High-level Intermediate Representation) — a semantics-oriented
/// intermediate representation.
///
/// Differences from the AST:
/// - **Name resolution is done**: variable references and function calls are
///   bound to `DefId`, no bare string names remain
/// - **Scope binding is done**: each block has its own `ScopeId`; variables
///   resolve lexically
/// - **Syntactic sugar is eliminated**: `let` type annotations are lowered
///   to executable `Ty`
/// - Downstream passes (type inference, borrow checking, arena analysis,
///   codegen) consume only this layer and never touch the AST
use aero_parse::ast::{BinOp, CmpOp, LogicOp, UnOp};
use aero_parse::span::Span;

use crate::ty::Ty;

/// Unique id of a definition (variable or function), assigned during
/// name resolution.
pub type DefId = u32;

/// Lexical scope id.
pub type ScopeId = u32;

/// Reduction operation for the `Reduce` builtin (Aero-Tensor IR / CPU backend).
/// Reduces a tensor over all elements to a single scalar.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ReduceOp {
    /// `sum(t)` — sum of all elements.
    Sum,
    /// `mean(t)` — arithmetic mean of all elements.
    Mean,
    /// `max(t)` — maximum element.
    Max,
    /// `min(t)` — minimum element.
    Min,
}

/// Element-wise tensor operation (Aero-Tensor IR / CPU backend).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ElemOp {
    /// `tensor_add(a, b)` — element-wise addition.
    Add,
    /// `tensor_sub(a, b)` — element-wise subtraction.
    Sub,
    /// `tensor_mul(a, b)` — element-wise multiplication.
    Mul,
    /// `tensor_div(a, b)` — element-wise division.
    Div,
    /// `tensor_neg(a)` — element-wise negation.
    Neg,
}

/// BLAS (Basic Linear Algebra Subprograms) compatible tensor builtins
/// (BLAS binding, CPU backend). These mirror the standard OpenBLAS / MKL
/// Level-1 subprograms and are implemented as compiler builtins using the same
/// general flat-index loop as `Reduce` / `ElemWise`, so any rank / shape is
/// supported without a hard external BLAS dependency.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BlasOp {
    /// `blas_dot(a, b)` — inner product Σ aᵢ·bᵢ, returns a scalar.
    Dot,
    /// `blas_nrm2(x)` — Euclidean norm √(Σ xᵢ²), returns a scalar (float only).
    Nrm2,
    /// `blas_asum(x)` — sum of absolute values Σ |xᵢ|, returns a scalar.
    Asum,
    /// `blas_amax(x)` — index of the element with the largest |xᵢ|, returns i64.
    Amax,
    /// `blas_scal(alpha, x)` — xᵢ ← alpha·xᵢ, returns a tensor.
    Scal,
    /// `blas_axpy(alpha, x, y)` — yᵢ ← alpha·xᵢ + yᵢ, returns a tensor.
    Axpy,
}

/// The whole program's HIR.
#[derive(Debug)]
pub struct HirProgram {
    /// Top-level function table (indexed by `FuncId`).
    pub funcs: Vec<HirFn>,
    /// Top-level struct definitions (indexed by insertion order).
    pub structs: Vec<HirStructDef>,
    /// Top-level union definitions (indexed by insertion order).
    pub unions: Vec<HirUnionDef>,
    /// Top-level const definitions (Phase P0-3). Each value is evaluated at
    /// compile time; references (HirExpr::ConstRef) are filled in by codegen.
    pub consts: Vec<HirConstDef>,
    /// Top-level enum definitions (indexed by insertion order).
    pub enums: Vec<HirEnumDef>,
    /// Top-level trait definitions (indexed by insertion order).
    pub traits: Vec<HirTraitDef>,
    /// Top-level impl blocks (indexed by insertion order).
    pub impls: Vec<HirImplBlock>,
    /// Method resolution table: (type_name, method_name) → function DefId.
    /// Covers both trait methods and inherent methods (lowering registers every
    /// impl method here).
    pub method_map: std::collections::HashMap<(String, String), DefId>,
    /// Top-level main block (statements outside any function).
    pub main: HirBlock,
}

/// A resolved struct definition.
#[derive(Debug, Clone)]
pub struct HirStructDef {
    pub name: String,
    /// Generic type parameter names (empty = non-generic struct)
    pub type_params: Vec<String>,
    /// (field name, field type)
    pub fields: Vec<(String, Ty)>,
    pub span: Span,
}

impl HirStructDef {
    /// Look up a field's type by name; returns (field_index, field_type).
    pub fn find_field(&self, name: &str) -> Option<(usize, &Ty)> {
        self.fields
            .iter()
            .enumerate()
            .find(|(_, (n, _))| n == name)
            .map(|(i, (_, t))| (i, t))
    }
}

/// A resolved union definition. All fields occupy the same memory (offset 0); the
/// union's layout is that of its largest field.
#[derive(Debug, Clone)]
pub struct HirUnionDef {
    pub name: String,
    /// (field name, field type)
    pub fields: Vec<(String, Ty)>,
    pub span: Span,
}

impl HirUnionDef {
    /// Look up a field's type by name; returns (field_index, field_type).
    pub fn find_field(&self, name: &str) -> Option<(usize, &Ty)> {
        self.fields
            .iter()
            .enumerate()
            .find(|(_, (n, _))| n == name)
            .map(|(i, (_, t))| (i, t))
    }
}

/// A resolved top-level const definition (Phase P0-3). The value expression is
/// evaluated at compile time; every reference is filled in by codegen.
#[derive(Debug, Clone)]
pub struct HirConstDef {
    pub name: String,
    pub ty: Ty,
    /// Whether an explicit `: TYPE` annotation was given. When false, the type is
    /// inferred from the value and `ty` is a placeholder.
    pub has_ty: bool,
    /// The constant's value expression (must fold to a scalar at compile time).
    pub value: HirExpr,
    pub span: Span,
}

impl HirConstDef {
    /// Look up a const by name in a program.
    pub fn lookup<'p>(consts: &'p [HirConstDef], name: &str) -> Option<&'p HirConstDef> {
        consts.iter().find(|c| c.name == name)
    }
}

/// A resolved enum definition.
#[derive(Debug, Clone)]
pub struct HirEnumDef {
    pub name: String,
    /// Generic type parameter names (empty = non-generic enum)
    pub type_params: Vec<String>,
    /// (variant name, payload type); `None` payload = unit variant.
    pub variants: Vec<(String, Option<Ty>)>,
    pub span: Span,
}

impl HirEnumDef {
    /// Look up a variant by name; returns (variant_index, payload_type).
    pub fn find_variant(&self, name: &str) -> Option<(usize, &Option<Ty>)> {
        self.variants
            .iter()
            .enumerate()
            .find(|(_, (n, _))| n == name)
            .map(|(i, (_, p))| (i, p))
    }
}

/// A resolved trait method signature.
#[derive(Debug, Clone)]
pub struct HirTraitMethodSig {
    pub name: String,
    pub params: Vec<(String, Ty, Span)>,
    pub ret: Option<Ty>,
    pub span: Span,
}

/// A resolved trait definition.
#[derive(Debug, Clone)]
pub struct HirTraitDef {
    pub name: String,
    /// Generic type parameter names of the trait (e.g. `trait Add<RHS, Output>`)
    pub type_params: Vec<String>,
    /// Associated type names declared by `type Name;` (e.g. `Iterator::Item`)
    pub assoc_types: Vec<String>,
    pub methods: Vec<HirTraitMethodSig>,
    pub span: Span,
}

impl HirTraitDef {
    /// Look up a method by name.
    pub fn find_method(&self, name: &str) -> Option<&HirTraitMethodSig> {
        self.methods.iter().find(|m| m.name == name)
    }
}

/// A resolved impl block: `impl [<T1, ...>] [Trait for] Type { fn ... }`
#[derive(Debug, Clone)]
pub struct HirImplBlock {
    /// Generic type parameter names of the impl header (empty = non-generic impl).
    /// For `impl<T> Vec<T> { ... }` these are the `T`s; impl methods are generic
    /// in these parameters and monomorphized per concrete instance.
    pub type_params: Vec<String>,
    /// Trait name (None = inherent impl, just adding methods to a type)
    pub trait_name: Option<String>,
    /// Concrete type arguments for the trait (`impl Add<i64, Complex> for Complex`
    /// → `[i64, Complex]`; empty for non-generic traits). Aligns with
    /// `HirTraitDef::type_params`.
    pub trait_args: Vec<Ty>,
    /// Associated type bindings (`type Item = i64;` inside the impl block).
    /// (name, bound type)
    pub assoc_types: Vec<(String, Ty)>,
    /// Target type name (a struct or enum name)
    pub type_name: String,
    /// Method definitions (each is a full HirFn with body)
    pub methods: Vec<HirFn>,
    pub span: Span,
}

impl HirProgram {
    pub fn lookup_func(&self, def_id: DefId) -> Option<&HirFn> {
        self.funcs.get(def_id as usize)
    }
}

/// A resolved function with a bound signature.
#[derive(Debug, Clone)]
pub struct HirFn {
    /// Function name
    pub name: String,
    /// The function's own DefId (also the index into the funcs table)
    pub def_id: DefId,
    /// Generic type parameter names (empty = non-generic function)
    pub type_params: Vec<String>,
    /// Named lifetime parameters (`'a`, `'b`, ...) declared on the function
    /// (Phase 10). Ties input reference lifetimes to the return reference.
    pub lifetimes: Vec<String>,
    /// Trait bounds: (type_param_name, trait_name)
    pub trait_bounds: Vec<(String, String)>,
    /// Parameters: (name, type, position)
    pub params: Vec<(String, Ty, Span)>,
    /// DefId of each parameter variable (one-to-one with params; bodies
    /// reference parameters by this id)
    pub param_defs: Vec<DefId>,
    /// Return type: `Some(T)` has a return value, `None` returns void
    pub ret: Option<Ty>,
    /// Whether this is a GPU kernel (`extern "gpu"` fn declaration, Campaign 3)
    pub is_gpu: bool,
    /// Whether this is a `const fn` (Phase 12.6). Such functions may be
    /// evaluated at compile time when called with constant arguments.
    pub is_const: bool,
    /// Whether this is an `extern "C"` function (FFI, no body; symbol
    /// resolved at link time, Campaign 5)
    pub is_extern: bool,
    /// C symbol name for `extern "C"` (`= "sym"`; defaults to the function name)
    pub extern_symbol: Option<String>,
    /// Whether this is a language builtin (assert/assert_eq; no LLVM body,
    /// special-cased in codegen)
    pub builtin: bool,
    /// Function body
    pub body: HirBlock,
    /// Source position of the definition (for error reporting)
    pub span: Span,
}

/// A block: statement sequence + scope id.
#[derive(Debug, Clone)]
pub struct HirBlock {
    pub stmts: Vec<HirStmt>,
    pub scope_id: ScopeId,
}

/// HIR statement.
#[derive(Debug, Clone)]
pub enum HirStmt {
    /// `let [mut] <name>[: ty] = <init>;`
    Let {
        name: String,
        mut_: bool,
        def_id: DefId,
        ty_ann: Option<Ty>,
        init: HirExpr,
        span: Span,
    },
    /// `name = value;` (name resolved to a DefId)
    Assign {
        def_id: DefId,
        value: HirExpr,
        span: Span,
    },
    /// `target[index] = value;` indexed write (array/raw pointer)
    AssignIndex {
        target: Box<HirExpr>,
        index: Box<HirExpr>,
        value: HirExpr,
        span: Span,
    },
    /// `*ptr = value;` deref write (requires an `&mut` reference)
    AssignDeref {
        target: Box<HirExpr>,
        value: HirExpr,
        span: Span,
    },
    /// `target.field = value;` field write
    AssignField {
        target: Box<HirExpr>,
        field: String,
        value: HirExpr,
        span: Span,
    },
    /// `print(...);`
    Print(Vec<HirExpr>, Span),
    /// Expression statement
    Expr(HirExpr, Span),
    /// `if (cond) { ... } else { ... }`
    If {
        cond: HirExpr,
        then_body: HirBlock,
        else_body: HirBlock,
        span: Span,
    },
    /// `while (cond) { ... }`
    While {
        cond: HirExpr,
        body: HirBlock,
        span: Span,
    },
    /// `loop { ... }` (infinite loop; exit via `break;`)
    Loop {
        body: HirBlock,
        span: Span,
    },
    /// `for (x in iter) { ... }`
    For {
        var: String,
        var_def: DefId,
        iter: HirExpr,
        body: HirBlock,
        span: Span,
    },
    /// `break;`
    Break(Span),
    /// `continue;`
    Continue(Span),
    /// `match (expr) { pattern => { body }, ... }`
    Match {
        scrutinee: HirExpr,
        arms: Vec<HirMatchArm>,
        span: Span,
    },
    /// `struct Name { field: type, ... }`
    StructDef {
        name: String,
        fields: Vec<(String, Ty)>,
        span: Span,
    },
    /// `enum Name { Variant, Variant(type), ... }`
    EnumDef {
        name: String,
        variants: Vec<(String, Option<Ty>)>,
        span: Span,
    },
    /// `trait Name { fn method(...); ... }`
    TraitDef {
        name: String,
        span: Span,
    },
    /// `impl [Trait for] Type { fn ... }`
    ImplBlock {
        trait_name: Option<String>,
        type_name: String,
        span: Span,
    },
    /// `return [expr];`
    Return(Option<HirExpr>, Span),
}

/// A match arm: pattern + body
#[derive(Debug, Clone)]
pub struct HirMatchArm {
    pub pattern: HirMatchPattern,
    pub body: HirBlock,
    pub span: Span,
}

/// Match patterns (HIR level)
#[derive(Debug, Clone)]
pub enum HirMatchPattern {
    /// `_`
    Wildcard,
    /// Integer literal
    IntLit(i64),
    /// Boolean literal
    BoolLit(bool),
    /// Char literal
    CharLit(char),
    /// String literal
    StrLit(String),
    /// Variable binding
    Bind(String, DefId),
    /// Enum variant pattern: `Variant(bind)` / `None` (resolved to the enum name)
    EnumVariant {
        /// Enum type name (resolved during lowering)
        enum_name: String,
        /// Variant index within the enum definition
        variant: String,
        /// Payload binding (None for unit variants / wildcard payloads)
        bind: Option<(String, DefId)>,
        span: Span,
    },
}

/// HIR expression (names resolved).
#[derive(Debug, Clone)]
pub enum HirExpr {
    IntLit(i64, Span),
    /// Float literal
    FloatLit(f64, Span),
    /// Char literal
    CharLit(char, Span),
    BoolLit(bool, Span),
    StrLit(String, Span),
    /// Variable reference (bound)
    Var(DefId, Span),
    /// Reference to a top-level `const NAME` (Phase P0-3). The name is kept so
    /// codegen can look up the compile-time-evaluated value and fill it in.
    ConstRef { name: String, ty: Ty, span: Span },
    /// Borrow `&x` / `&mut x` (target bound to a DefId)
    Borrow { mut_: bool, def_id: DefId, span: Span },
    /// Dereference `*p`
    Deref { target: Box<HirExpr>, span: Span },
    /// Try `expr?`: unwrap `Result<T, E>`, propagating the error to the caller.
    Try { target: Box<HirExpr>, span: Span },
    /// Method call `recv.method(args...)` (Arena's alloc/reset)
    MethodCall {
        recv: Box<HirExpr>,
        method: String,
        args: Vec<HirExpr>,
        span: Span,
    },
    /// Arena literal `arena(N)`
    ArenaLit(usize, Span),
    /// Tensor literal `tensor(3, 4, ...)` or `tensor<f64>(3, 4, ...)` (elements
    /// initialized to 0). Element type defaults to i64 when not annotated.
    TensorLit { dims: Vec<usize>, elem: Ty, span: Span },
    /// Matrix-multiply builtin `matmul(a, b)` (Campaign 3): compile-time
    /// dimension check, 2-D tensors only, requires `a.shape[1] == b.shape[0]`.
    Matmul {
        lhs: Box<HirExpr>,
        rhs: Box<HirExpr>,
        span: Span,
    },
    /// Reduction builtin `sum(t)` / `mean(t)` / `max(t)` / `min(t)` (Aero-Tensor
    /// IR, CPU backend): reduces a tensor over all elements to a single scalar.
    /// The result type equals the tensor's element type; `mean` returns the
    /// element type too (integer mean truncates toward zero).
    Reduce {
        op: ReduceOp,
        input: Box<HirExpr>,
        span: Span,
    },
    /// Element-wise tensor operation `tensor_add(a, b)` / `tensor_sub(a, b)` /
    /// `tensor_mul(a, b)` / `tensor_div(a, b)` / `tensor_neg(a)` (Aero-Tensor
    /// IR, CPU backend). Returns a tensor of the same shape and element type.
    ElemWise {
        op: ElemOp,
        lhs: Box<HirExpr>,
        rhs: Option<Box<HirExpr>>,
        span: Span,
    },
    /// BLAS Level-1 tensor operations `blas_dot` / `blas_nrm2` / `blas_asum` /
    /// `blas_amax` / `blas_scal` / `blas_axpy` (BLAS binding, CPU backend).
    /// `args` holds the already-lowered operands; the compiler decides per op
    /// which elements are scalar operands (`alpha`) vs tensors.
    Blas {
        op: BlasOp,
        args: Vec<HirExpr>,
        span: Span,
    },
    /// Tuple literal
    Tuple(Vec<HirExpr>, Span),
    /// Array literal
    Array(Vec<HirExpr>, Span),
    /// Index access `target[index]`
    Index {
        target: Box<HirExpr>,
        index: Box<HirExpr>,
        span: Span,
    },
    /// Unary operation
    Unary {
        op: UnOp,
        expr: Box<HirExpr>,
        span: Span,
    },
    /// Binary arithmetic
    Binary {
        op: BinOp,
        lhs: Box<HirExpr>,
        rhs: Box<HirExpr>,
        span: Span,
    },
    /// Comparison (boolean result)
    Cmp {
        op: CmpOp,
        lhs: Box<HirExpr>,
        rhs: Box<HirExpr>,
        span: Span,
    },
    /// Logical (short-circuit)
    Logic {
        op: LogicOp,
        lhs: Box<HirExpr>,
        rhs: Box<HirExpr>,
        span: Span,
    },
    /// Function call (bound to a function DefId)
    Call {
        def_id: DefId,
        args: Vec<HirExpr>,
        span: Span,
    },
    /// Reference to a named function as a first-class value `fn_ref` (a function
    /// pointer). Lowered from a bare function name used in value position (e.g.
    /// `let f = quicksort; f(v)`). The type is `Ty::Fn(params, ret)`.
    FnRef {
        def_id: DefId,
        span: Span,
    },
    /// Indirect call through a first-class function pointer: `callee(args...)`
    /// where `callee` is an expression of type `Ty::Fn(..)` (not a bare function
    /// name — those stay `Call`). Codegen loads the function pointer and calls it.
    CallPtr {
        callee: Box<HirExpr>,
        args: Vec<HirExpr>,
        span: Span,
    },
    /// Struct literal `Name { field: expr, ... }`
    StructLit {
        name: String,
        fields: Vec<(String, HirExpr)>,
        span: Span,
    },
    /// Enum variant constructor `Enum::Variant` / `Enum::Variant(expr)`
    EnumLit {
        name: String,
        variant: String,
        arg: Option<Box<HirExpr>>,
        span: Span,
    },
    /// Field access `expr.field`
    Field {
        target: Box<HirExpr>,
        field: String,
        span: Span,
    },
    /// Type cast `expr as dyn Trait` (Phase 9): boxes the target value on the
    /// heap, producing a `dyn Trait` fat pointer `{ data, vtable }`.
    Cast {
        target: Box<HirExpr>,
        ty: Ty,
        span: Span,
    },
}

impl HirExpr {
    pub fn span(&self) -> Span {
        match self {
            HirExpr::IntLit(_, s)
            | HirExpr::FloatLit(_, s)
            | HirExpr::CharLit(_, s)
            | HirExpr::BoolLit(_, s)
            | HirExpr::StrLit(_, s)
            | HirExpr::Var(_, s)
            | HirExpr::ArenaLit(_, s)
            | HirExpr::TensorLit { span: s, .. }
            | HirExpr::Tuple(_, s)
            | HirExpr::Array(_, s) => *s,
            HirExpr::Borrow { span, .. }
            | HirExpr::Deref { span, .. }
            | HirExpr::Try { span, .. }
            | HirExpr::MethodCall { span, .. }
            | HirExpr::Matmul { span, .. }
            | HirExpr::Reduce { span, .. }
            | HirExpr::ElemWise { span, .. }
            | HirExpr::Blas { span, .. }
            | HirExpr::Index { span, .. }
            | HirExpr::Unary { span, .. }
            | HirExpr::Binary { span, .. }
            | HirExpr::Cmp { span, .. }
            | HirExpr::Logic { span, .. }
            | HirExpr::Call { span, .. }
            | HirExpr::FnRef { span, .. }
            | HirExpr::CallPtr { span, .. }
            | HirExpr::StructLit { span, .. }
            | HirExpr::EnumLit { span, .. }
            | HirExpr::Field { span, .. }
            | HirExpr::Cast { span, .. } => *span,
            HirExpr::ConstRef { span, .. } => *span,
        }
    }
}
