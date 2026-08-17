use crate::span::Span;

/// Binary arithmetic operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
}

impl BinOp {
    pub fn symbol(&self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Rem => "%",
            BinOp::BitAnd => "&",
            BinOp::BitOr => "|",
            BinOp::BitXor => "^",
            BinOp::Shl => "<<",
            BinOp::Shr => ">>",
        }
    }

    /// Whether this operator is a bitwise/shift operator (integer-only, not
    /// overloadable, not supported on floats).
    pub fn is_bitwise(&self) -> bool {
        matches!(
            self,
            BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr
        )
    }
}

/// Unary operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
}

/// Comparison operator (result type is boolean; codegen emits LLVM i1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Lt,
    Gt,
    Le,
    Ge,
    Eq,
    Ne,
}

impl CmpOp {
    pub fn symbol(&self) -> &'static str {
        match self {
            CmpOp::Lt => "<",
            CmpOp::Gt => ">",
            CmpOp::Le => "<=",
            CmpOp::Ge => ">=",
            CmpOp::Eq => "==",
            CmpOp::Ne => "!=",
        }
    }
}

/// Logical operator (short-circuit evaluated; result type is boolean).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicOp {
    And,
    Or,
}

/// Type annotation expression: `i32`, `[i64; 3]`, `(i64, bool)`, `&i64`, `&mut i64`, `*i64`,
/// `Vec<i64>` (generic type application).
#[derive(Debug, Clone, PartialEq)]
pub enum TypeExpr {
    /// Named base type: `i32`, `i64`, `bool`, `str`, `f32`, `f64`, `char`
    Named(String, Span),
    /// Generic type application: `Name<Arg1, Arg2, ...>` (e.g. `Vec<i64>`)
    Generic { name: String, args: Vec<TypeExpr>, span: Span },
    /// Fixed-size array: `[T; N]`
    Array(Box<TypeExpr>, usize, Span),
    /// Tuple: `(T, U, ...)`
    Tuple(Vec<TypeExpr>, Span),
    /// Immutable/mutable reference: `&T` / `&mut T` / `&'a T` / `&'a mut T`.
    /// `lifetime` is the optional named lifetime (`'a`), None for elision.
    Ref {
        mut_: bool,
        lifetime: Option<String>,
        inner: Box<TypeExpr>,
        span: Span,
    },
    /// Raw pointer: `*T`
    Ptr(Box<TypeExpr>, Span),
    /// Qualified path type: `Self::Item` (root + associated type name)
    Path {
        root: String,
        name: String,
        span: Span,
    },
    /// Dynamic trait object type: `dyn Drawable`. Lowered to a fat pointer
    /// `{ data: i8*, vtable: i8* }` (a heap-allocated value + a vtable of
    /// function pointers, one per trait method).
    Dyn { name: String, span: Span },
}

impl TypeExpr {
    pub fn span(&self) -> Span {
        match self {
            TypeExpr::Named(_, span)
            | TypeExpr::Generic { span, .. }
            | TypeExpr::Array(_, _, span)
            | TypeExpr::Tuple(_, span)
            | TypeExpr::Ref { span, .. }
            | TypeExpr::Ptr(_, span)
            | TypeExpr::Path { span, .. }
            | TypeExpr::Dyn { span, .. } => *span,
        }
    }
}

/// Expression node.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// Integer literal
    Int(i64, Span),
    /// Boolean literal
    Bool(bool, Span),
    /// String literal (escapes decoded; quotes not included)
    Str(String, Span),
    /// Float literal
    Float(f64, Span),
    /// Char literal
    Char(char, Span),
    /// Variable reference
    Var(String, Span),
    /// Borrow `&x` / `&mut x` (target must be a variable)
    Borrow { mut_: bool, target: Box<Expr>, span: Span },
    /// Dereference `*p`
    Deref { target: Box<Expr>, span: Span },
    /// Try `expr?`: unwrap `Result<T, E>`, propagating the error to the caller.
    Try { target: Box<Expr>, span: Span },
    /// Method call `recv.method(args...)` (e.g. Arena's alloc/reset)
    MethodCall {
        recv: Box<Expr>,
        method: String,
        args: Vec<Expr>,
        span: Span,
    },
    /// Arena literal `arena(N)`
    ArenaLit(usize, Span),
    /// Tensor literal `tensor(3, 4)` or `tensor<f64>(3, 4)` (N dims, zero-initialized).
    /// The optional element type defaults to `i64` when omitted.
    TensorLit(Vec<usize>, Option<TypeExpr>, Span),
    /// Tuple literal `(a, b, ...)`
    Tuple(Vec<Expr>, Span),
    /// Array literal `[a, b, ...]`
    Array(Vec<Expr>, Span),
    /// Index access `target[index]`
    Index {
        target: Box<Expr>,
        index: Box<Expr>,
        span: Span,
    },
    /// Function call `name(args...)`
    Call {
        callee: String,
        args: Vec<Expr>,
        span: Span,
    },
    /// Struct literal `Name { field: expr, ... }`
    StructLit {
        name: String,
        fields: Vec<(String, Expr)>,
        span: Span,
    },
    /// Enum variant constructor `Enum::Variant` / `Enum::Variant(expr)`
    EnumLit {
        name: String,
        variant: String,
        arg: Option<Box<Expr>>,
        span: Span,
    },
    /// Module-path function call `a::b::c(args...)` (multi-segment path).
    /// Lowering dispatches to module functions, or to native/enum constructors when
    /// the path names `String`/`Vec`/`Box` or a user enum.
    PathCall {
        path: Vec<String>,
        args: Vec<Expr>,
        span: Span,
    },
    /// Field access `expr.field`
    Field {
        target: Box<Expr>,
        field: String,
        span: Span,
    },
    /// Type cast `expr as dyn Trait` (Phase 9): boxes the target value on the
    /// heap and produces a `dyn Trait` fat pointer `{ data, vtable }`.
    Cast {
        target: Box<Expr>,
        ty: TypeExpr,
        span: Span,
    },
    /// Unary operation, e.g. `-x`
    Unary { op: UnOp, expr: Box<Expr>, span: Span },
    /// Binary arithmetic, e.g. `1 + 2`
    Binary { op: BinOp, lhs: Box<Expr>, rhs: Box<Expr>, span: Span },
    /// Comparison, e.g. `x > 0` (boolean result)
    Cmp { op: CmpOp, lhs: Box<Expr>, rhs: Box<Expr>, span: Span },
    /// Logical operation, e.g. `a AND b` (short-circuit, boolean result)
    Logic { op: LogicOp, lhs: Box<Expr>, rhs: Box<Expr>, span: Span },
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Int(_, span)
            | Expr::Bool(_, span)
            | Expr::Str(_, span)
            | Expr::Float(_, span)
            | Expr::Char(_, span)
            | Expr::Var(_, span)
            | Expr::ArenaLit(_, span)
            | Expr::TensorLit(_, _, span)
            | Expr::Tuple(_, span)
            | Expr::Array(_, span) => *span,
            Expr::Index { span, .. }
            | Expr::Borrow { span, .. }
            | Expr::Deref { span, .. }
            | Expr::Try { span, .. }
            | Expr::MethodCall { span, .. }
            | Expr::Call { span, .. }
            | Expr::StructLit { span, .. }
            | Expr::EnumLit { span, .. }
            | Expr::PathCall { span, .. }
            | Expr::Field { span, .. }
            | Expr::Cast { span, .. }
            | Expr::Unary { span, .. }
            | Expr::Binary { span, .. }
            | Expr::Cmp { span, .. }
            | Expr::Logic { span, .. } => *span,
        }
    }
}

/// Statement node.
#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    /// `let [mut] <name>[: <ty>] = <expr>;`
    Let {
        name: String,
        mut_: bool,
        ty_ann: Option<TypeExpr>,
        init: Expr,
        span: Span,
    },
    /// `<name> = <expr>;` reassigns a declared variable
    Assign { name: String, value: Expr, span: Span },
    /// `target[index] = <expr>;` indexed write (array/pointer)
    AssignIndex {
        target: Box<Expr>,
        index: Box<Expr>,
        value: Expr,
        span: Span,
    },
    /// `*ptr = <expr>;` dereference write (requires mutable reference)
    AssignDeref {
        target: Box<Expr>,
        value: Expr,
        span: Span,
    },
    /// `target.field = <expr>;` field write
    AssignField {
        target: Box<Expr>,
        field: String,
        value: Expr,
        span: Span,
    },
    /// `print(<expr>, <expr>, ...);` — with a format string as the first
    /// argument, remaining arguments are formatted into it.
    Print(Vec<Expr>, Span),
    /// Expression statement `<expr>;`
    Expr(Expr, Span),
    /// `fn <name>[<T1, T2, ...>](<params>) [-> <ret>] { ... }` definition.
    /// The angle brackets hold generic type parameters (e.g. `fn max<T>(a: T, b: T) -> T`).
    /// `extern "gpu" fn ...` (is_gpu=true) is a GPU kernel declaration.
    /// `extern "C" fn ...;` (is_extern=true, no body) is an FFI declaration.
    FnDef {
        name: String,
        /// Generic type parameter names (empty = non-generic function)
        type_params: Vec<String>,
        /// Named lifetime parameters (empty = none), e.g. `fn foo<'a>(...)`
        lifetimes: Vec<String>,
        /// Trait bounds: (type_param_name, trait_name)
        trait_bounds: Vec<(String, String)>,
        params: Vec<(String, TypeExpr)>,
        ret: Option<TypeExpr>,
        body: Vec<Stmt>,
        is_gpu: bool,
        /// Whether this is a `const fn` (eligible for compile-time evaluation
        /// when called with constant arguments, Phase 12.6)
        is_const: bool,
        /// Whether this is an `extern "C"` function (no body; symbol resolved at link time)
        is_extern: bool,
        /// C symbol name for `extern "C"` (`= "sym"`; defaults to the function name)
        extern_symbol: Option<String>,
        span: Span,
    },
    /// `return [<expr>];`
    Return(Option<Expr>, Span),
    /// `if (<cond>) { ... } else { ... }`
    If {
        cond: Expr,
        then_body: Vec<Stmt>,
        else_body: Vec<Stmt>,
        span: Span,
    },
    /// `while (<cond>) { ... }`
    While { cond: Expr, body: Vec<Stmt>, span: Span },
    /// `loop { ... }` (infinite loop; exit via `break;`)
    Loop { body: Vec<Stmt>, span: Span },
    /// `for (x in iter) { ... }`
    For { var: String, iter: Expr, body: Vec<Stmt>, span: Span },
    /// `break;`
    Break(Span),
    /// `continue;`
    Continue(Span),
    /// `match (scrutinee) { pattern => { ... } ... }`
    Match { scrutinee: Expr, arms: Vec<MatchArm>, span: Span },
    /// `struct Name[<T1, T2, ...>] { field: type, ... }` definition.
    /// `type_params` holds the generic type parameter names (empty = non-generic struct).
    /// `derives` holds `#[derive(...)]` trait names (e.g. `Copy`).
    StructDef {
        name: String,
        type_params: Vec<String>,
        fields: Vec<(String, TypeExpr)>,
        derives: Vec<String>,
        span: Span,
    },
    /// `union Name { field: type, ... }` definition. All fields share the same
    /// memory (union semantics); the union's size is the largest field.
    UnionDef {
        name: String,
        fields: Vec<(String, TypeExpr)>,
        span: Span,
    },
    /// `const NAME: TYPE = <expr>;` definition at the top level. The value is
    /// evaluated at compile time and every reference is filled in (Phase P0-3).
    ConstDef {
        name: String,
        ty: Option<TypeExpr>,
        value: Box<Expr>,
        span: Span,
    },
    /// `enum Name[<T1, T2, ...>] { Variant, Variant(type), ... }` definition.
    /// `type_params` holds the generic type parameter names (empty = non-generic enum).
    /// `derives` holds `#[derive(...)]` trait names (e.g. `Copy`).
    EnumDef {
        name: String,
        type_params: Vec<String>,
        variants: Vec<EnumVariant>,
        derives: Vec<String>,
        span: Span,
    },
    /// `trait Name[<T1, T2, ...>] { fn method(...); ... }`
    /// `type_params` holds the trait's generic type parameter names (empty = non-generic).
    /// `assoc_types` holds the associated type names declared by `type Item;`.
    TraitDef {
        name: String,
        type_params: Vec<String>,
        assoc_types: Vec<String>,
        methods: Vec<TraitMethodSig>,
        span: Span,
    },
    /// `impl [<T1, T2, ...>] [Trait[<Args>] for] Type { ... }`
    ImplBlock {
        /// Generic type parameter names of the impl (empty = non-generic impl).
        /// For `impl<T> Vec<T> { ... }` these are the `T`s bound in the impl header.
        type_params: Vec<String>,
        /// Trait name (None = inherent impl, just adding methods to a type)
        trait_name: Option<String>,
        /// Generic arguments of the trait in `impl Trait<Args> for Type`
        /// (empty = non-generic trait application, e.g. `impl Drop for Rect`).
        /// `impl Iterator<i64> for Vec<i64>` ⇒ `[i64]`.
        trait_args: Vec<TypeExpr>,
        /// Target type name (must be a defined struct or enum)
        type_name: String,
        /// Associated type bindings: `type Item = i64;` → `("Item", i64)`.
        assoc_types: Vec<(String, TypeExpr)>,
        /// Method definitions (each is a full FnDef)
        methods: Vec<Stmt>,
        span: Span,
    },
    /// `mod name { ... }` — an inline module (a named namespace whose items are
    /// registered under `name::item`). Nested modules are allowed.
    ModDef {
        name: String,
        items: Vec<Stmt>,
        span: Span,
    },
    /// `mod name;` — a module loaded from the file `name.aero` (phase 2, multi-file).
    /// Parsed so we can produce a clear "not yet supported" diagnostic.
    ModFile {
        name: String,
        span: Span,
    },
    /// `use a::b::C;` — import `C` (or the last path segment) into the current
    /// module's scope. `use a::b::*;` imports all public items.
    UseDecl {
        /// Path segments, e.g. `["a", "b", "C"]`, or `["a", "b", "*"]` for a glob.
        path: Vec<String>,
        span: Span,
    },
    /// `pub <item>` — marks the wrapped item as externally visible (module-system
    /// visibility; private module items are not importable from outside).
    Pub(Box<Stmt>, Span),
}

impl Stmt {
    pub fn span(&self) -> Span {
        match self {
            Stmt::Let { span, .. }
            | Stmt::Assign { span, .. }
            | Stmt::AssignIndex { span, .. }
            | Stmt::AssignDeref { span, .. }
            | Stmt::AssignField { span, .. } => *span,
            Stmt::Print(_, span) | Stmt::Expr(_, span) => *span,
            Stmt::FnDef { span, .. } | Stmt::Return(_, span) => *span,
            Stmt::If { span, .. } | Stmt::While { span, .. } => *span,
            Stmt::Loop { span, .. } => *span,
            Stmt::For { span, .. } | Stmt::Match { span, .. } => *span,
            Stmt::StructDef { span, .. } => *span,
            Stmt::UnionDef { span, .. } => *span,
            Stmt::ConstDef { span, .. } => *span,
            Stmt::EnumDef { span, .. } => *span,
            Stmt::TraitDef { span, .. } | Stmt::ImplBlock { span, .. } => *span,
            Stmt::Break(span) | Stmt::Continue(span) => *span,
            Stmt::ModDef { span, .. }
            | Stmt::ModFile { span, .. }
            | Stmt::UseDecl { span, .. }
            | Stmt::Pub(_, span) => *span,
        }
    }
}

/// One arm of a `match` expression: `pattern => { body }`.
#[derive(Debug, Clone, PartialEq)]
pub struct MatchArm {
    pub pattern: MatchPattern,
    pub body: Vec<Stmt>,
    pub span: Span,
}

/// A variant of an `enum` definition: `None` (unit) or `Some(i64)` (payload).
#[derive(Debug, Clone, PartialEq)]
pub struct EnumVariant {
    pub name: String,
    /// Payload type for non-unit variants (`None` = unit variant)
    pub payload: Option<TypeExpr>,
    pub span: Span,
}

/// A trait method signature: `fn name(params) -> ret;` (no body)
#[derive(Debug, Clone, PartialEq)]
pub struct TraitMethodSig {
    pub name: String,
    pub params: Vec<(String, TypeExpr)>,
    pub ret: Option<TypeExpr>,
    pub span: Span,
}

/// Simple match patterns (literals, wildcard, binding, enum variant).
#[derive(Debug, Clone, PartialEq)]
pub enum MatchPattern {
    /// Wildcard `_`
    Wildcard,
    /// Integer literal pattern
    IntLit(i64),
    /// Boolean literal pattern
    BoolLit(bool),
    /// Char literal pattern
    CharLit(char),
    /// String literal pattern
    StrLit(String),
    /// Variable binding (binds the matched value to a name)
    Bind(String),
    /// Enum variant pattern: `Some(v)` / `Some(_)` / `None` / `Enum::Variant`.
    /// `enum_name` is `Some` only for the explicit `Enum::Variant` form; bare
    /// `Variant` resolves against the scrutinee type during lowering.
    EnumVariant {
        enum_name: Option<String>,
        variant: String,
        bind: Option<String>,
        span: Span,
    },
}

/// A whole program: a list of statements.
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub stmts: Vec<Stmt>,
}
