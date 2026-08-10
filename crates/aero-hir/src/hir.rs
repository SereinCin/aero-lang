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

/// The whole program's HIR.
#[derive(Debug)]
pub struct HirProgram {
    /// Top-level function table (indexed by `FuncId`).
    pub funcs: Vec<HirFn>,
    /// Top-level main block (statements outside any function).
    pub main: HirBlock,
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
    /// Parameters: (name, type, position)
    pub params: Vec<(String, Ty, Span)>,
    /// DefId of each parameter variable (one-to-one with params; bodies
    /// reference parameters by this id)
    pub param_defs: Vec<DefId>,
    /// Return type: `Some(T)` has a return value, `None` returns void
    pub ret: Option<Ty>,
    /// Whether this is a GPU kernel (`extern "gpu"` fn declaration, Campaign 3)
    pub is_gpu: bool,
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
    /// `let <name>[: ty] = <init>;`
    Let {
        name: String,
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
    /// `return [expr];`
    Return(Option<HirExpr>, Span),
}

/// HIR expression (names resolved).
#[derive(Debug, Clone)]
pub enum HirExpr {
    IntLit(i64, Span),
    BoolLit(bool, Span),
    StrLit(String, Span),
    /// Variable reference (bound)
    Var(DefId, Span),
    /// Borrow `&x` / `&mut x` (target bound to a DefId)
    Borrow { mut_: bool, def_id: DefId, span: Span },
    /// Dereference `*p`
    Deref { target: Box<HirExpr>, span: Span },
    /// Method call `recv.method(args...)` (Arena's alloc/reset)
    MethodCall {
        recv: Box<HirExpr>,
        method: String,
        args: Vec<HirExpr>,
        span: Span,
    },
    /// Arena literal `arena(N)`
    ArenaLit(usize, Span),
    /// Tensor literal `tensor(3, 4, ...)` (elements initialized to 0)
    TensorLit(Vec<usize>, Span),
    /// Matrix-multiply builtin `matmul(a, b)` (Campaign 3): compile-time
    /// dimension check, 2-D tensors only, requires `a.shape[1] == b.shape[0]`.
    Matmul {
        lhs: Box<HirExpr>,
        rhs: Box<HirExpr>,
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
}

impl HirExpr {
    pub fn span(&self) -> Span {
        match self {
            HirExpr::IntLit(_, s)
            | HirExpr::BoolLit(_, s)
            | HirExpr::StrLit(_, s)
            | HirExpr::Var(_, s)
            | HirExpr::ArenaLit(_, s)
            | HirExpr::TensorLit(_, s)
            | HirExpr::Tuple(_, s)
            | HirExpr::Array(_, s) => *s,
            HirExpr::Borrow { span, .. }
            | HirExpr::Deref { span, .. }
            | HirExpr::MethodCall { span, .. }
            | HirExpr::Matmul { span, .. }
            | HirExpr::Index { span, .. }
            | HirExpr::Unary { span, .. }
            | HirExpr::Binary { span, .. }
            | HirExpr::Cmp { span, .. }
            | HirExpr::Logic { span, .. }
            | HirExpr::Call { span, .. } => *span,
        }
    }
}
