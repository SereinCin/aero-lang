/// AST → HIR lowering: name resolution + scope binding + type annotations
/// lowered to `Ty`.
///
/// Two-pass flow:
/// 1. First pass scans top-level `fn` definitions and collects all function
///    signatures (supports forward calls and recursion)
/// 2. Second pass lowers the main block and every function body (parameters
///    bound to the function scope)
use aero_parse::ast::{Expr, Program, Stmt, TypeExpr};
use aero_parse::span::Span;

use crate::hir::{DefId, HirBlock, HirExpr, HirFn, HirProgram, HirStmt, ScopeId};
use crate::ty::Ty;

/// Error from the name-resolution phase.
#[derive(Debug, Clone)]
pub struct LowerError {
    pub msg: String,
    pub line: u32,
    pub col: u32,
}

impl LowerError {
    fn new(msg: impl Into<String>, span: Span) -> Self {
        LowerError {
            msg: msg.into(),
            line: span.line,
            col: span.col,
        }
    }
}

/// Function context: the function's return type (used by the type-checking
/// phase to validate ``return`` statements).
#[derive(Clone)]
pub struct FnCtx {
    pub ret: Option<Ty>,
}

pub struct Lowerer {
    /// Function signature (collected in pass 1, body filled in pass 2)
    funcs: Vec<FuncSig>,
    /// Function name → DefId
    func_by_name: std::collections::HashMap<String, DefId>,
    /// Variable scope stack: outer scopes first
    scopes: Vec<std::collections::HashMap<String, DefId>>,
    /// Generic type parameter names of the current function (used by lower_type
/// to recognize `T` as a generic parameter)
    gen_params: Vec<String>,
    /// Next variable DefId
    next_var: DefId,
    /// Next scope id
    next_scope: ScopeId,
}

/// Signatures collected in pass 1.
struct FuncSig {
    name: String,
    def_id: DefId,
    type_params: Vec<String>,
    params: Vec<(String, Ty, Span)>,
    ret: Option<Ty>,
    is_gpu: bool,
    is_extern: bool,
    extern_symbol: Option<String>,
    builtin: bool,
    span: Span,
}

/// Language builtins (no body; codegen special-cases them: assert/assert_eq assertions,
/// string ops len/int_to_str/str_free). Format: (name, parameter types, return type)
const BUILTINS: &[(&str, &[Ty], Option<Ty>)] = &[
    ("assert", &[Ty::Bool], None),
    ("assert_eq", &[Ty::I64, Ty::I64], None),
    ("len", &[Ty::Str], Some(Ty::I64)),
    ("int_to_str", &[Ty::I64], Some(Ty::Str)),
    ("str_free", &[Ty::Str], None),
    // String library (string system extension): slicing, parsing, search, ordering.
    ("substr", &[Ty::Str, Ty::I64, Ty::I64], Some(Ty::Str)),
    ("str_to_int", &[Ty::Str], Some(Ty::I64)),
    ("str_contains", &[Ty::Str, Ty::Str], Some(Ty::Bool)),
    ("str_find", &[Ty::Str, Ty::Str], Some(Ty::I64)),
    ("str_cmp", &[Ty::Str, Ty::Str], Some(Ty::I64)),
];

impl Lowerer {
    pub fn lower(program: &Program) -> Result<HirProgram, LowerError> {
        let mut lowerer = Lowerer {
            funcs: Vec::new(),
            func_by_name: std::collections::HashMap::new(),
            scopes: Vec::new(),
            gen_params: Vec::new(),
            next_var: 0,
            next_scope: 0,
        };
        // Pass 1: register language builtins first (assert/assert_eq, no body)
        let dummy_span = Span {
            line: 0,
            col: 0,
            start: 0,
            end: 0,
        };
        for (name, params, ret) in BUILTINS {
            let def_id = lowerer.funcs.len() as DefId;
            lowerer.func_by_name.insert(name.to_string(), def_id);
            lowerer.funcs.push(FuncSig {
                name: name.to_string(),
                def_id,
                type_params: Vec::new(),
                params: params
                    .iter()
                    .cloned()
                    .map(|t| (String::new(), t, dummy_span))
                    .collect(),
                ret: ret.clone(),
                is_gpu: false,
                is_extern: false,
                extern_symbol: None,
                builtin: true,
                span: dummy_span,
            });
        }
        // Pass 1: collect top-level function signatures (forward calls and recursion)
        let mut fn_bodies: Vec<(&str, &[Stmt])> = Vec::new();
        for stmt in &program.stmts {
            if let Stmt::FnDef {
                name,
                type_params,
                params,
                ret,
                is_gpu,
                is_extern,
                extern_symbol,
                span,
                ..
            } = stmt
            {
                if name == "matmul" {
                    return Err(LowerError::new(
                        "`matmul` is a builtin matrix-multiply operation and cannot be redefined",
                        *span,
                    ));
                }
                if lowerer.func_by_name.contains_key(name) {
                    return Err(LowerError::new(
                        format!("duplicate definition of function `{name}`"),
                        *span,
                    ));
                }
                // Generic parameter names must not collide with builtin type names
                for tp in type_params {
                    if matches!(tp.as_str(), "i32" | "i64" | "bool" | "str") {
                        return Err(LowerError::new(
                            format!("generic type parameter `{tp}` collides with a builtin type name"),
                            *span,
                        ));
                    }
                }
                // Lower params and return type in the generic-parameter context (the
// function body also needs this context)
                let mut hir_params = Vec::new();
                lowerer.gen_params = type_params.clone();
                for (pname, pty) in params {
                    let ty = lowerer.lower_type(pty)?;
                    hir_params.push((pname.clone(), ty, pty.span()));
                }
                let ret_ty = match ret {
                    Some(t) => Some(lowerer.lower_type(t)?),
                    None => None,
                };
                lowerer.gen_params.clear();
                if *is_extern {
                    // extern "C" (FFI) ABI checks: only C ABI compatible types
                    if !type_params.is_empty() {
                        return Err(LowerError::new(
                            "extern \"C\" functions do not support generic type parameters",
                            *span,
                        ));
                    }
                    if let Some(sym) = extern_symbol {
                        let ok = !sym.is_empty()
                            && sym.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
                        if !ok {
                            return Err(LowerError::new(
                                format!("invalid extern \"C\" symbol name `{sym}` (letters/digits/underscore only)"),
                                *span,
                            ));
                        }
                    }
                    for (pname, pty, psp) in &hir_params {
                        if !matches!(pty, Ty::I32 | Ty::I64 | Ty::Ptr(_) | Ty::Str) {
                            return Err(LowerError::new(
                                format!("extern \"C\" parameter `{pname}` type `{pty}` is not C ABI compatible (only i32/i64/*T/str)"),
                                *psp,
                            ));
                        }
                    }
                    if let Some(rt) = &ret_ty {
                        if !matches!(rt, Ty::I32 | Ty::I64 | Ty::Ptr(_) | Ty::Void) {
                            return Err(LowerError::new(
                                format!("extern \"C\" return type `{rt}` is not C ABI compatible (only i32/i64/*T/void)"),
                                *span,
                            ));
                        }
                    }
                } else if let Some(rt) = &ret_ty {
                    // References/pointers/arenas cannot be return types (prevents dangling
// references escaping the function)
                    if !rt.is_borrowable() {
                        return Err(LowerError::new(
                            format!("function `{name}` cannot return type `{rt}` (references/pointers/arenas are forbidden as return types)"),
                            *span,
                        ));
                    }
                }
                // GPU kernels must return void (NVPTX backend constraint)
                if *is_gpu && ret_ty.is_some() {
                    return Err(LowerError::new(
                        "GPU kernels cannot return a value (kernels must return void)",
                        *span,
                    ));
                }
                let def_id = lowerer.funcs.len() as DefId;
                lowerer.func_by_name.insert(name.clone(), def_id);
                lowerer.funcs.push(FuncSig {
                    name: name.clone(),
                    def_id,
                    type_params: type_params.clone(),
                    params: hir_params,
                    ret: ret_ty,
                    is_gpu: *is_gpu,
                    is_extern: *is_extern,
                    extern_symbol: extern_symbol.clone(),
                    builtin: false,
                    span: *span,
                });
                fn_bodies.push((name.as_str(), stmt_body(stmt)));
            }
        }
        // Pass 2: the main block
        let main = lowerer.lower_block(&program.stmts, None)?;
        // Pass 2: function bodies (each function has its own scope; parameters are
        // bound first). Clone the signatures first so we do not mutably borrow
        let sigs: Vec<(
            String,
            DefId,
            Vec<String>,
            Vec<(String, Ty, Span)>,
            Option<Ty>,
            bool,
            bool,
            Option<String>,
            bool,
            Span,
        )> = lowerer
            .funcs
            .iter()
            .map(|s| {
                (
                    s.name.clone(),
                    s.def_id,
                    s.type_params.clone(),
                    s.params.clone(),
                    s.ret.clone(),
                    s.is_gpu,
                    s.is_extern,
                    s.extern_symbol.clone(),
                    s.builtin,
                    s.span,
                )
            })
            .collect();
        let mut hir_funcs = Vec::new();
        for (name, def_id, type_params, params, ret, is_gpu, is_extern, extern_symbol, builtin, span) in
            sigs
        {
            lowerer.scopes.push(std::collections::HashMap::new());
            let mut param_defs = Vec::new();
            if !builtin {
                // Builtins (assert/assert_eq) have no real parameters; no variable DefId
                for (pname, _, _) in &params {
                    let def_id = lowerer.next_var;
                    lowerer.next_var += 1;
                    lowerer
                        .scopes
                        .last_mut()
                        .expect("function scope already created")
                        .insert(pname.clone(), def_id);
                    param_defs.push(def_id);
                }
            }
            let body = if builtin {
                // Builtins have no body: keep the empty block to preserve DefId indices
                HirBlock {
                    stmts: Vec::new(),
                    scope_id: 0,
                }
            } else {
                // Lower the body in the generic-parameter context (so `let x: T = ...` can resolve `T`)
                let body_ast = fn_bodies
                    .iter()
                    .find(|(n, _)| *n == name.as_str())
                    .map(|(_, body)| *body)
                    .expect("same-name function body collected in pass 1");
                lowerer.gen_params = type_params.clone();
                let lowered = lowerer.lower_block_stmts(body_ast, Some(FnCtx { ret: ret.clone() }));
                lowerer.gen_params.clear();
                lowered?
            };
            lowerer.scopes.pop();
            hir_funcs.push(HirFn {
                name,
                def_id,
                type_params,
                params,
                param_defs,
                ret,
                is_gpu,
                is_extern,
                extern_symbol,
                builtin,
                body,
                span,
            });
        }
        Ok(HirProgram {
            funcs: hir_funcs,
            main,
        })
    }

    /// Type annotation → Ty (unknown type names error out; generic parameter
/// names resolve to `Generic`).
    fn lower_type(&mut self, te: &TypeExpr) -> Result<Ty, LowerError> {
        match te {
            TypeExpr::Named(name, span) => match name.as_str() {
                "i32" => Ok(Ty::I32),
                "i64" => Ok(Ty::I64),
                "bool" => Ok(Ty::Bool),
                "str" => Ok(Ty::Str),
                _ => {
                    // Current function's generic type parameter names → generic type
                    if self.gen_params.iter().any(|p| p == name) {
                        Ok(Ty::Generic(name.clone()))
                    } else {
                        Err(LowerError::new(format!("unknown type `{name}`"), *span))
                    }
                }
            },
            TypeExpr::Array(elem, n, _) => {
                let elem_ty = self.lower_type(elem)?;
                Ok(Ty::Array(Box::new(elem_ty), *n))
            }
            TypeExpr::Tuple(elems, _) => {
                let mut tys = Vec::new();
                for e in elems {
                    tys.push(self.lower_type(e)?);
                }
                Ok(Ty::Tuple(tys))
            }
            TypeExpr::Ref { mut_, inner, .. } => {
                let inner = self.lower_type(inner)?;
                if !inner.is_borrowable() {
                    return Err(LowerError::new(
                        format!("cannot create a reference to type `{inner}`"),
                        te.span(),
                    ));
                }
                Ok(Ty::Ref {
                    mut_: *mut_,
                    inner: Box::new(inner),
                })
            }
            TypeExpr::Ptr(inner, _) => {
                let inner = self.lower_type(inner)?;
                Ok(Ty::Ptr(Box::new(inner)))
            }
        }
    }

    /// Lower a block (creates a new scope). `fn_ctx` is used to reject nested
/// function definitions inside function bodies.
    fn lower_block(
        &mut self,
        stmts: &[Stmt],
        fn_ctx: Option<FnCtx>,
    ) -> Result<HirBlock, LowerError> {
        let scope_id = self.new_scope();
        self.scopes.push(std::collections::HashMap::new());
        let block = self.lower_block_stmts(stmts, fn_ctx);
        self.scopes.pop();
        match block {
            Ok(HirBlock { stmts, .. }) => Ok(HirBlock { stmts, scope_id }),
            Err(e) => Err(e),
        }
    }

    /// Lower the statements of a block (scope assumed to exist).
    fn lower_block_stmts(
        &mut self,
        stmts: &[Stmt],
        fn_ctx: Option<FnCtx>,
    ) -> Result<HirBlock, LowerError> {
        let mut hir_stmts = Vec::new();
        for stmt in stmts {
            if let Stmt::FnDef { span, .. } = stmt {
                if fn_ctx.is_some() {
                    return Err(LowerError::new("function definitions cannot be nested inside function bodies", *span));
                }
                continue; // top-level functions were collected in pass 1
            }
            hir_stmts.push(self.lower_stmt(stmt, fn_ctx.clone())?);
        }
        Ok(HirBlock {
            stmts: hir_stmts,
            scope_id: 0, // placeholder, overwritten by the caller
        })
    }

    fn lower_stmt(&mut self, stmt: &Stmt, fn_ctx: Option<FnCtx>) -> Result<HirStmt, LowerError> {
        match stmt {
            Stmt::Let {
                name,
                ty_ann,
                init,
                span,
            } => {
                let ty_ann = match ty_ann {
                    Some(t) => Some(self.lower_type(t)?),
                    None => None,
                };
                let init = self.lower_expr(init)?;
                let def_id = self.bind_var(name, *span)?;
                Ok(HirStmt::Let {
                    name: name.clone(),
                    def_id,
                    ty_ann,
                    init,
                    span: *span,
                })
            }
            Stmt::Assign { name, value, span } => {
                let def_id = self.resolve_var(name, *span)?;
                let value = self.lower_expr(value)?;
                Ok(HirStmt::Assign {
                    def_id,
                    value,
                    span: *span,
                })
            }
            Stmt::AssignIndex {
                target,
                index,
                value,
                span,
            } => {
                let t = self.lower_expr(target)?;
                let i = self.lower_expr(index)?;
                let v = self.lower_expr(value)?;
                Ok(HirStmt::AssignIndex {
                    target: Box::new(t),
                    index: Box::new(i),
                    value: v,
                    span: *span,
                })
            }
            Stmt::AssignDeref { target, value, span } => {
                let t = self.lower_expr(target)?;
                let v = self.lower_expr(value)?;
                Ok(HirStmt::AssignDeref {
                    target: Box::new(t),
                    value: v,
                    span: *span,
                })
            }
            Stmt::Print(args, span) => {
                let mut hir_args = Vec::new();
                for a in args {
                    hir_args.push(self.lower_expr(a)?);
                }
                Ok(HirStmt::Print(hir_args, *span))
            }
            Stmt::Expr(expr, span) => {
                let e = self.lower_expr(expr)?;
                Ok(HirStmt::Expr(e, *span))
            }
            Stmt::If {
                cond,
                then_body,
                else_body,
                span,
            } => {
                let cond = self.lower_expr(cond)?;
                let then_body = self.lower_block(then_body, fn_ctx.clone())?;
                let else_body = self.lower_block(else_body, fn_ctx)?;
                Ok(HirStmt::If {
                    cond,
                    then_body,
                    else_body,
                    span: *span,
                })
            }
            Stmt::While { cond, body, span } => {
                let cond = self.lower_expr(cond)?;
                let body = self.lower_block(body, fn_ctx)?;
                Ok(HirStmt::While {
                    cond,
                    body,
                    span: *span,
                })
            }
            Stmt::Return(value, span) => {
                let v = match value {
                    Some(e) => Some(self.lower_expr(e)?),
                    None => None,
                };
                Ok(HirStmt::Return(v, *span))
            }
            Stmt::FnDef { span, .. } => Err(LowerError::new(
                "function definitions are not allowed in this position",
                *span,
            )),
        }
    }

    fn lower_expr(&mut self, expr: &Expr) -> Result<HirExpr, LowerError> {
        match expr {
            Expr::Int(v, span) => Ok(HirExpr::IntLit(*v, *span)),
            Expr::Bool(v, span) => Ok(HirExpr::BoolLit(*v, *span)),
            Expr::Str(s, span) => Ok(HirExpr::StrLit(s.clone(), *span)),
            Expr::Var(name, span) => {
                let def_id = self.resolve_var(name, *span)?;
                Ok(HirExpr::Var(def_id, *span))
            }
            Expr::Borrow {
                mut_,
                target,
                span,
            } => {
                // The borrow target must be a variable: `&x` / `&mut x`
                let def_id = match &**target {
                    Expr::Var(name, _) => self.resolve_var(name, *span)?,
                    _ => {
                        return Err(LowerError::new(
                            "the borrow target must be a variable (`&x` / `&mut x`)",
                            *span,
                        ));
                    }
                };
                Ok(HirExpr::Borrow {
                    mut_: *mut_,
                    def_id,
                    span: *span,
                })
            }
            Expr::Deref { target, span } => {
                let t = self.lower_expr(target)?;
                Ok(HirExpr::Deref {
                    target: Box::new(t),
                    span: *span,
                })
            }
            Expr::MethodCall {
                recv,
                method,
                args,
                span,
            } => {
                let r = self.lower_expr(recv)?;
                let mut hir_args = Vec::new();
                for a in args {
                    hir_args.push(self.lower_expr(a)?);
                }
                Ok(HirExpr::MethodCall {
                    recv: Box::new(r),
                    method: method.clone(),
                    args: hir_args,
                    span: *span,
                })
            }
            Expr::ArenaLit(n, span) => Ok(HirExpr::ArenaLit(*n, *span)),
            Expr::TensorLit(dims, span) => Ok(HirExpr::TensorLit(dims.clone(), *span)),
            Expr::Tuple(elems, span) => {
                let mut hir = Vec::new();
                for e in elems {
                    hir.push(self.lower_expr(e)?);
                }
                Ok(HirExpr::Tuple(hir, *span))
            }
            Expr::Array(elems, span) => {
                let mut hir = Vec::new();
                for e in elems {
                    hir.push(self.lower_expr(e)?);
                }
                Ok(HirExpr::Array(hir, *span))
            }
            Expr::Index {
                target,
                index,
                span,
            } => {
                let t = self.lower_expr(target)?;
                let i = self.lower_expr(index)?;
                Ok(HirExpr::Index {
                    target: Box::new(t),
                    index: Box::new(i),
                    span: *span,
                })
            }
            Expr::Unary { op, expr, span } => {
                let e = self.lower_expr(expr)?;
                Ok(HirExpr::Unary {
                    op: *op,
                    expr: Box::new(e),
                    span: *span,
                })
            }
            Expr::Binary { op, lhs, rhs, span } => {
                let l = self.lower_expr(lhs)?;
                let r = self.lower_expr(rhs)?;
                Ok(HirExpr::Binary {
                    op: *op,
                    lhs: Box::new(l),
                    rhs: Box::new(r),
                    span: *span,
                })
            }
            Expr::Cmp { op, lhs, rhs, span } => {
                let l = self.lower_expr(lhs)?;
                let r = self.lower_expr(rhs)?;
                Ok(HirExpr::Cmp {
                    op: *op,
                    lhs: Box::new(l),
                    rhs: Box::new(r),
                    span: *span,
                })
            }
            Expr::Logic { op, lhs, rhs, span } => {
                let l = self.lower_expr(lhs)?;
                let r = self.lower_expr(rhs)?;
                Ok(HirExpr::Logic {
                    op: *op,
                    lhs: Box::new(l),
                    rhs: Box::new(r),
                    span: *span,
                })
            }
            Expr::Call {
                callee,
                args,
                span,
            } => {
                // Builtin matrix multiply: matmul(a, b) (dimension checks run during
// type inference)
                if callee == "matmul" {
                    if args.len() != 2 {
                        return Err(LowerError::new(
                            "`matmul` requires 2 arguments (two 2-D tensors)",
                            *span,
                        ));
                    }
                    let lhs = self.lower_expr(&args[0])?;
                    let rhs = self.lower_expr(&args[1])?;
                    return Ok(HirExpr::Matmul {
                        lhs: Box::new(lhs),
                        rhs: Box::new(rhs),
                        span: *span,
                    });
                }
                let def_id = match self.func_by_name.get(callee) {
                    Some(&id) => id,
                    None => {
                        return Err(LowerError::new(
                            format!("undefined function `{callee}`"),
                            *span,
                        ));
                    }
                };
                let mut hir_args = Vec::new();
                for a in args {
                    hir_args.push(self.lower_expr(a)?);
                }
                Ok(HirExpr::Call {
                    def_id,
                    args: hir_args,
                    span: *span,
                })
            }
        }
    }

    // ---------- name resolution ----------

    /// Look up a variable in the current scope chain; error if undefined.
    fn resolve_var(&self, name: &str, span: Span) -> Result<DefId, LowerError> {
        for scope in self.scopes.iter().rev() {
            if let Some(&id) = scope.get(name) {
                return Ok(id);
            }
        }
        Err(LowerError::new(format!("undefined variable `{name}`"), span))
    }

    /// Declare a new variable: duplicate declaration in the current scope is
/// an error (no silent shadowing).
    fn bind_var(&mut self, name: &str, span: Span) -> Result<DefId, LowerError> {
        if let Some(cur) = self.scopes.last() {
            if cur.contains_key(name) {
                return Err(LowerError::new(
                    format!("variable `{name}` is already declared in this scope"),
                    span,
                ));
            }
        }
        let def_id = self.next_var;
        self.next_var += 1;
        self.scopes
            .last_mut()
            .expect("lower_block always creates a scope")
            .insert(name.to_string(), def_id);
        Ok(def_id)
    }

    fn new_scope(&mut self) -> ScopeId {
        let id = self.next_scope;
        self.next_scope += 1;
        id
    }
}

/// Extract the function body statement slice from `Stmt::FnDef`.
fn stmt_body(stmt: &Stmt) -> &[Stmt] {
    match stmt {
        Stmt::FnDef { body, .. } => body,
        _ => unreachable!("caller guaranteed a FnDef"),
    }
}
