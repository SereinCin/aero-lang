use crate::span::Span;

/// Binary arithmetic operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
}

impl BinOp {
    pub fn symbol(&self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
        }
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

/// Type annotation expression: `i32`, `[i64; 3]`, `(i64, bool)`, `&i64`, `&mut i64`, `*i64`.
#[derive(Debug, Clone, PartialEq)]
pub enum TypeExpr {
    /// Named base type: `i32`, `i64`, `bool`, `str`
    Named(String, Span),
    /// Fixed-size array: `[T; N]`
    Array(Box<TypeExpr>, usize, Span),
    /// Tuple: `(T, U, ...)`
    Tuple(Vec<TypeExpr>, Span),
    /// Immutable/mutable reference: `&T` / `&mut T`
    Ref { mut_: bool, inner: Box<TypeExpr>, span: Span },
    /// Raw pointer: `*T`
    Ptr(Box<TypeExpr>, Span),
}

impl TypeExpr {
    pub fn span(&self) -> Span {
        match self {
            TypeExpr::Named(_, span)
            | TypeExpr::Array(_, _, span)
            | TypeExpr::Tuple(_, span)
            | TypeExpr::Ref { span, .. }
            | TypeExpr::Ptr(_, span) => *span,
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
    /// Variable reference
    Var(String, Span),
    /// Borrow `&x` / `&mut x` (target must be a variable)
    Borrow { mut_: bool, target: Box<Expr>, span: Span },
    /// Dereference `*p`
    Deref { target: Box<Expr>, span: Span },
    /// Method call `recv.method(args...)` (e.g. Arena's alloc/reset)
    MethodCall {
        recv: Box<Expr>,
        method: String,
        args: Vec<Expr>,
        span: Span,
    },
    /// Arena literal `arena(N)`
    ArenaLit(usize, Span),
    /// Tensor literal `tensor(3, 4)` (N dims, zero-initialized)
    TensorLit(Vec<usize>, Span),
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
            | Expr::Var(_, span)
            | Expr::ArenaLit(_, span)
            | Expr::TensorLit(_, span)
            | Expr::Tuple(_, span)
            | Expr::Array(_, span) => *span,
            Expr::Index { span, .. }
            | Expr::Borrow { span, .. }
            | Expr::Deref { span, .. }
            | Expr::MethodCall { span, .. }
            | Expr::Call { span, .. }
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
    /// `let <name>[: <ty>] = <expr>;`
    Let {
        name: String,
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
        params: Vec<(String, TypeExpr)>,
        ret: Option<TypeExpr>,
        body: Vec<Stmt>,
        is_gpu: bool,
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
}

impl Stmt {
    pub fn span(&self) -> Span {
        match self {
            Stmt::Let { span, .. }
            | Stmt::Assign { span, .. }
            | Stmt::AssignIndex { span, .. }
            | Stmt::AssignDeref { span, .. } => *span,
            Stmt::Print(_, span) | Stmt::Expr(_, span) => *span,
            Stmt::FnDef { span, .. } | Stmt::Return(_, span) => *span,
            Stmt::If { span, .. } | Stmt::While { span, .. } => *span,
        }
    }
}

/// A whole program: a list of statements.
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub stmts: Vec<Stmt>,
}
