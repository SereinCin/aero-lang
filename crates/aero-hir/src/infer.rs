/// Type inference engine: constraint-unification Hindley-Milner style (non-generic).
///
/// Design notes:
/// - Integer literals produce **type variables** that can unify with `i32`/`i64`
///   annotations; unconstrained variables are defaulted to `i64` at `let` bindings
///   (so `let x = 1 + 2` infers i64)
/// - `let y: i32 = 1` is legal (variable adapts), but `let y: i32 = x` with x already
///   defaulted to i64 reports a type mismatch — implicit narrowing is rejected
use std::collections::HashMap;

use aero_parse::span::Span;

use aero_parse::ast::BinOp;

use crate::hir::{DefId, HirBlock, HirExpr, HirFn, HirProgram, HirStmt};
use crate::ty::{Ty, TypeVar};

/// Error from the type-checking phase.
#[derive(Debug, Clone)]
pub struct InferError {
    pub msg: String,
    pub line: u32,
    pub col: u32,
}

/// A concrete instantiation of a generic function: `type_args` holds the resolved
/// concrete type of each generic parameter; codegen monomorphizes (one LLVM function per instance).
#[derive(Debug, Clone, PartialEq)]
pub struct GenericInstance {
    pub fn_def_id: DefId,
    pub type_args: Vec<Ty>,
}

/// Complete output of inference: variable type table + generic instance info.
#[derive(Debug, Default)]
pub struct InferResult {
    /// Variable DefId → resolved type
    pub var_tys: HashMap<DefId, Ty>,
    /// All generic instances in the program (deduplicated)
    pub instances: Vec<GenericInstance>,
    /// Generic call sites (keyed by expr byte offset span.start) → their type arguments
    pub call_types: HashMap<usize, Vec<Ty>>,
}

impl InferError {
    fn new(msg: impl Into<String>, span: Span) -> Self {
        InferError {
            msg: msg.into(),
            line: span.line,
            col: span.col,
        }
    }
}

pub struct Infer<'a> {
    /// Function table (signature lookup)
    funcs: &'a [HirFn],
    /// Variable DefId → resolved type
    var_tys: HashMap<DefId, Ty>,
    /// Type variable → substitution target
    subs: HashMap<TypeVar, Ty>,
    /// Integer type variables (from integer literals): only bindable to the integer family
    int_vars: std::collections::HashSet<TypeVar>,
    /// Generic instances (deduplicated)
    instances: Vec<GenericInstance>,
    /// Generic call site span.start → type arguments
    call_types: HashMap<usize, Vec<Ty>>,
    /// Type variable counter
    next_var: u32,
}

impl<'a> Infer<'a> {
    pub fn check(program: &HirProgram) -> Result<InferResult, InferError> {
        let mut infer = Infer {
            funcs: &program.funcs,
            var_tys: HashMap::new(),
            subs: HashMap::new(),
            int_vars: std::collections::HashSet::new(),
            instances: Vec::new(),
            call_types: HashMap::new(),
            next_var: 0,
        };
        // Function parameters register their types first (referenced by bodies); builtins have none
        for f in &program.funcs {
            if f.builtin {
                continue;
            }
            for (i, (_, ty, _)) in f.params.iter().enumerate() {
                infer
                    .var_tys
                    .insert(f.param_defs[i], ty.clone());
            }
        }
        // Check the main block (no function context; ``return`` not allowed)
        infer.check_block(&program.main, None)?;
        // Check each function body
        for f in &program.funcs {
            let ctx = FnRetCtx {
                ret: f.ret.clone(),
                name: &f.name,
            };
            infer.check_block(&f.body, Some(ctx))?;
        }
        // Deduplicate instances (same (function, type args) kept once)
        let mut seen: std::collections::HashSet<(DefId, String)> = std::collections::HashSet::new();
        infer.instances.retain(|inst| {
            let key = (inst.fn_def_id, format!("{:?}", inst.type_args));
            seen.insert(key)
        });
        Ok(InferResult {
            var_tys: infer.var_tys,
            instances: infer.instances,
            call_types: infer.call_types,
        })
    }

    fn check_block(
        &mut self,
        block: &HirBlock,
        ctx: Option<FnRetCtx>,
    ) -> Result<(), InferError> {
        for stmt in &block.stmts {
            self.check_stmt(stmt, ctx.clone())?;
        }
        Ok(())
    }

    fn check_stmt(&mut self, stmt: &HirStmt, ctx: Option<FnRetCtx>) -> Result<(), InferError> {
        match stmt {
            HirStmt::Let {
                name,
                def_id,
                ty_ann,
                init,
                span,
            } => {
                let init_ty = self.infer_expr(init)?;
                // Arenas cannot be copied or moved: only the `arena(N)` literal produces an arena value
                let init_resolved = self.resolve(&init_ty);
                if matches!(init_resolved, Ty::Arena(_)) && !matches!(init, HirExpr::ArenaLit(..)) {
                    return Err(InferError::new(
                        "Arenas cannot be copied or moved; create them directly with `arena(N)`",
                        *span,
                    ));
                }
                match ty_ann {
                    Some(ann) => {
                        // Unify first (literal variables can adapt to i32/i64), then default
                        self.unify(ann, &init_ty, *span, &format!("variable `{name}`"))?;
                        let final_ty = self.compact(&self.resolve(ann));
                        self.var_tys.insert(*def_id, final_ty);
                    }
                    None => {
                        let final_ty = self.compact(&self.resolve(&init_ty));
                        self.var_tys.insert(*def_id, final_ty);
                    }
                }
                Ok(())
            }
            HirStmt::Assign {
                def_id,
                value,
                span,
            } => {
                let declared = self.lookup_var(*def_id, *span)?;
                let value_ty = self.infer_expr(value)?;
                // Arenas cannot be copied or moved: `a = ...` is not allowed
                let declared_r = self.resolve(&declared);
                if matches!(declared_r, Ty::Arena(_)) {
                    return Err(InferError::new("arena variables cannot be reassigned", *span));
                }
                let declared = self.compact(&declared_r);
                self.unify(&declared, &value_ty, *span, "assignment")?;
                Ok(())
            }
            HirStmt::AssignIndex {
                target,
                index,
                value,
                span,
            } => {
                let target_ty = self.infer_expr(target)?;
                let target_ty = self.compact(&self.resolve(&target_ty));
                let elem_ty = match &target_ty {
                    Ty::Array(elem, _) => (**elem).clone(),
                    Ty::Ptr(elem) => (**elem).clone(),
                    Ty::Tensor { shape, elem } if !shape.is_empty() => (**elem).clone(),
                    other => {
                        return Err(InferError::new(
                            format!("cannot index-write into type `{other}` (only arrays/pointers/tensors)"),
                            *span,
                        ));
                    }
                };
                let index_ty = self.infer_expr(index)?;
                self.require_int(&index_ty, *span, "index")?;
                let value_ty = self.infer_expr(value)?;
                self.unify(&elem_ty, &value_ty, *span, "index-write value")?;
                Ok(())
            }
            HirStmt::AssignDeref { target, value, span } => {
                // `*ptr = v`: ptr must be an `&mut` reference
                let ptr_ty = self.infer_expr(target)?;
                let ptr_ty = self.compact(&self.resolve(&ptr_ty));
                let inner = match &ptr_ty {
                    Ty::Ref { mut_: true, inner } => (**inner).clone(),
                    Ty::Ref { mut_: false, .. } => {
                        return Err(InferError::new(
                            "immutable references (`&T`) cannot be written to; `&mut T` is required",
                            *span,
                        ));
                    }
                    other => {
                        return Err(InferError::new(
                            format!("cannot deref-write into type `{other}`"),
                            *span,
                        ));
                    }
                };
                let value_ty = self.infer_expr(value)?;
                self.unify(&inner, &value_ty, *span, "deref-write value")?;
                Ok(())
            }
            HirStmt::Print(args, span) => {
                for arg in args {
                    let ty = self.infer_expr(arg)?;
                    let ty = self.compact(&self.resolve(&ty));
                    if ty == Ty::Void {
                        return Err(InferError::new(
                            "print arguments cannot be void function calls",
                            arg.span(),
                        ));
                    }
                }
                let _ = span;
                Ok(())
            }
            HirStmt::Expr(expr, _) => {
                self.infer_expr(expr)?;
                Ok(())
            }
            HirStmt::If {
                cond,
                then_body,
                else_body,
                span,
            } => {
                let cond_ty = self.infer_expr(cond)?;
                self.require_bool(&cond_ty, *span, "if condition")?;
                self.check_block(then_body, ctx.clone())?;
                self.check_block(else_body, ctx)?;
                Ok(())
            }
            HirStmt::While { cond, body, span } => {
                let cond_ty = self.infer_expr(cond)?;
                self.require_bool(&cond_ty, *span, "while condition")?;
                self.check_block(body, ctx)?;
                Ok(())
            }
            HirStmt::Return(value, span) => match ctx {
                None => Err(InferError::new(
                    "`return` is only allowed inside function bodies",
                    *span,
                )),
                Some(fctx) => match (&fctx.ret, value) {
                    (Some(ret_ty), Some(v)) => {
                        let v_ty = self.infer_expr(v)?;
                        self.unify(
                            ret_ty,
                            &v_ty,
                            *span,
                            &format!("return type of function `{}`", fctx.name),
                        )?;
                        Ok(())
                    }
                    (Some(_), None) => Err(InferError::new(
                        format!(
                            "function `{}` declares a return type, so `return` must carry a value",
                            fctx.name
                        ),
                        *span,
                    )),
                    (None, Some(_)) => Err(InferError::new(
                        format!(
                            "function `{}` has no declared return type, so `return` cannot carry a value",
                            fctx.name
                        ),
                        *span,
                    )),
                    (None, None) => Ok(()),
                },
            },
        }
    }

    fn infer_expr(&mut self, expr: &HirExpr) -> Result<Ty, InferError> {
        match expr {
            HirExpr::IntLit(_, _) => Ok(self.fresh_int_var()),
            HirExpr::BoolLit(_, _) => Ok(Ty::Bool),
            HirExpr::StrLit(_, _) => Ok(Ty::Str),
            HirExpr::Var(def_id, span) => self.lookup_var(*def_id, *span),
            HirExpr::Borrow {
                mut_,
                def_id,
                span,
            } => {
                let src_ty = self.lookup_var(*def_id, *span)?;
                let src_ty = self.resolve(&src_ty);
                if !src_ty.is_borrowable() {
                    return Err(InferError::new(
                        format!("cannot borrow a value of type `{src_ty}`"),
                        *span,
                    ));
                }
                let t = self.fresh_var();
                self.unify(&src_ty, &t, *span, "borrow target type")?;
                Ok(Ty::Ref {
                    mut_: *mut_,
                    inner: Box::new(t),
                })
            }
            HirExpr::Deref { target, span } => {
                let ptr_ty = self.infer_expr(target)?;
                let ptr_ty = self.compact(&self.resolve(&ptr_ty));
                match &ptr_ty {
                    Ty::Ref { inner, .. } | Ty::Ptr(inner) => Ok((**inner).clone()),
                    other => Err(InferError::new(
                        format!("cannot dereference type `{other}`"),
                        *span,
                    )),
                }
            }
            HirExpr::MethodCall {
                recv,
                method,
                args,
                span,
            } => {
                let recv_ty = self.infer_expr(recv)?;
                let recv_ty = self.compact(&self.resolve(&recv_ty));
                let arena_size = match &recv_ty {
                    Ty::Arena(size) => *size,
                    other => {
                        return Err(InferError::new(
                            format!("method `{method}` only applies to arenas, got `{other}`"),
                            *span,
                        ));
                    }
                };
                match method.as_str() {
                    "alloc" => {
                        if args.len() != 1 {
                            return Err(InferError::new(
                                "`alloc` requires 1 argument (slot count)",
                                *span,
                            ));
                        }
                        let n_ty = self.infer_expr(&args[0])?;
                        self.require_int(&n_ty, *span, "`alloc` slot count")?;
                        let _ = arena_size;
                        Ok(Ty::Ptr(Box::new(Ty::I64)))
                    }
                    "reset" => {
                        if !args.is_empty() {
                            return Err(InferError::new(
                                "`reset` takes no arguments",
                                *span,
                            ));
                        }
                        Ok(Ty::Void)
                    }
                    other => Err(InferError::new(
                        format!("arena has no method `{other}` (supported: alloc/reset)"),
                        *span,
                    )),
                }
            }
            HirExpr::ArenaLit(n, _) => Ok(Ty::Arena(*n)),
            HirExpr::TensorLit(dims, _) => Ok(Ty::Tensor {
                elem: Box::new(Ty::I64),
                shape: dims.clone(),
            }),
            HirExpr::Matmul { lhs, rhs, span } => {
                let l_ty = self.infer_expr(lhs)?;
                let lt = self.compact(&self.resolve(&l_ty));
                let r_ty = self.infer_expr(rhs)?;
                let rt = self.compact(&self.resolve(&r_ty));
                let (lshape, lelem) = match &lt {
                    Ty::Tensor { shape, elem } if shape.len() == 2 => (shape.clone(), (**elem).clone()),
                    other => {
                        return Err(InferError::new(
                            format!("left operand of `matmul` must be a 2-D tensor, got `{other}`"),
                            *span,
                        ));
                    }
                };
                let (rshape, relem) = match &rt {
                    Ty::Tensor { shape, elem } if shape.len() == 2 => (shape.clone(), (**elem).clone()),
                    other => {
                        return Err(InferError::new(
                            format!("right operand of `matmul` must be a 2-D tensor, got `{other}`"),
                            *span,
                        ));
                    }
                };
                if lshape[1] != rshape[0] {
                    return Err(InferError::new(
                        format!(
                            "matmul dimension mismatch: {}x{} and {}x{} cannot be multiplied (left columns must equal right rows)",
                            lshape[0], lshape[1], rshape[0], rshape[1]
                        ),
                        *span,
                    ));
                }
                self.unify(&lelem, &relem, *span, "element types of the two `matmul` tensors")?;
                let elem = self.compact(&self.resolve(&lelem));
                Ok(Ty::Tensor {
                    elem: Box::new(elem),
                    shape: vec![lshape[0], rshape[1]],
                })
            }
            HirExpr::Tuple(elems, _) => {
                let mut tys = Vec::new();
                for e in elems {
                    tys.push(self.infer_expr(e)?);
                }
                Ok(Ty::Tuple(tys))
            }
            HirExpr::Array(elems, _) => {
                if elems.is_empty() {
                    return Err(InferError::new(
                        "the empty array `[]` cannot infer an element type; annotate it, e.g. `let a: [i64; 0] = [];`",
                        expr.span(),
                    ));
                }
                let first = self.infer_expr(&elems[0])?;
                for e in &elems[1..] {
                    let ty = self.infer_expr(e)?;
                    self.unify(&first, &ty, expr.span(), "array element")?;
                }
                Ok(Ty::Array(Box::new(first), elems.len()))
            }
            HirExpr::Index {
                target,
                index,
                span,
            } => {
                let target_ty = self.infer_expr(target)?;
                let target_ty = self.compact(&self.resolve(&target_ty));
                let index_ty = self.infer_expr(index)?;
                self.require_int(&index_ty, *span, "index")?;
                match &target_ty {
                    Ty::Array(elem, _) => Ok((**elem).clone()),
                    Ty::Ptr(elem) => Ok((**elem).clone()),
                    // s[i]: a string index yields the byte value at position i
                    Ty::Str => Ok(Ty::I64),
                    Ty::Tensor { shape, elem } => {
                        // a[i]: indexing a 1-D tensor yields a scalar; otherwise strip the first dim (sub-tensor)
                        if shape.len() == 1 {
                            Ok((**elem).clone())
                        } else {
                            Ok(Ty::Tensor {
                                elem: elem.clone(),
                                shape: shape[1..].to_vec(),
                            })
                        }
                    }
                    Ty::Tuple(elems) => {
                        if let HirExpr::IntLit(k, _) = &**index {
                            if *k >= 0 && (*k as usize) < elems.len() {
                                return Ok(elems[*k as usize].clone());
                            }
                        }
                        Err(InferError::new(
                            "tuple index must be an integer constant within range",
                            *span,
                        ))
                    }
                    Ty::Var(_) => {
                        // Variable element types from array literals must be resolved after defaulting
                        Err(InferError::new(
                            "cannot index a value whose type is not yet determined",
                            *span,
                        ))
                    }
                    other => Err(InferError::new(
                        format!("cannot index into type `{other}` (only arrays/tuples/pointers)"),
                        *span,
                    )),
                }
            }
            HirExpr::Unary { expr, span, .. } => {
                let ty = self.infer_expr(expr)?;
                let v = self.fresh_var();
                self.unify(&ty, &v, *span, "unary minus operand")?;
                self.require_int(&ty, *span, "unary minus operand")?;
                Ok(ty)
            }
            HirExpr::Binary { op, lhs, rhs, span } => {
                let l = self.infer_expr(lhs)?;
                let r = self.infer_expr(rhs)?;
                self.unify(&l, &r, *span, "the two arithmetic operands")?;
                let lc = self.compact(&self.resolve(&l));
                if lc == Ty::Str {
                    // String concatenation: only `+` is supported
                    if *op == BinOp::Add {
                        return Ok(Ty::Str);
                    }
                    return Err(InferError::new(
                        "strings only support concatenation with `+`",
                        *span,
                    ));
                }
                self.require_int(&l, *span, "arithmetic operand")?;
                Ok(l)
            }
            HirExpr::Cmp { lhs, rhs, span, .. } => {
                let l = self.infer_expr(lhs)?;
                let r = self.infer_expr(rhs)?;
                self.unify(&l, &r, *span, "the two comparison operands")?;
                let lc = self.compact(&self.resolve(&l));
                if lc == Ty::Str {
                    // String comparison: all six operators are supported via strcmp
                    return Ok(Ty::Bool);
                }
                self.require_int(&l, *span, "comparison operand")?;
                Ok(Ty::Bool)
            }
            HirExpr::Logic { lhs, rhs, span, .. } => {
                let l = self.infer_expr(lhs)?;
                let r = self.infer_expr(rhs)?;
                self.require_bool(&l, *span, "logic operand")?;
                self.require_bool(&r, *span, "logic operand")?;
                Ok(Ty::Bool)
            }
            HirExpr::Call {
                def_id, args, span,
            } => {
                let f = self
                    .funcs
                    .get(*def_id as usize)
                    .ok_or_else(|| InferError::new("internal error: function table missing", *span))?;
                // GPU kernels are isolated from the CPU side: not callable from normal code
                if f.is_gpu {
                    return Err(InferError::new(
                        format!(
                            "GPU kernel `{}` cannot be called directly from CPU code (GPU/CPU isolation)",
                            f.name
                        ),
                        *span,
                    ));
                }
                if f.params.len() != args.len() {
                    return Err(InferError::new(
                        format!(
                            "function `{}` takes {} arguments, but {} were passed",
                            f.name,
                            f.params.len(),
                            args.len()
                        ),
                        *span,
                    ));
                }
                if f.type_params.is_empty() {
                    // -- non-generic call: unify the signature directly with the arguments --
                    for (i, arg) in args.iter().enumerate() {
                        let arg_ty = self.infer_expr(arg)?;
                        let param_ty = &f.params[i].1;
                        self.unify(
                            param_ty,
                            &arg_ty,
                            arg.span(),
                            &format!("argument {} of function `{}`", i + 1, f.name),
                        )?;
                    }
                    Ok(match &f.ret {
                        Some(t) => t.clone(),
                        None => Ty::Void,
                    })
                } else {
                    // -- generic call: fresh type variables per generic parameter, instantiate the signature --
                    // Replace `Ty::Generic(name)` in the signature with fresh type variables,
                    // unify with the arguments, then default to concrete types: the call site's type args.
                    let subst: HashMap<String, Ty> = f
                        .type_params
                        .iter()
                        .map(|name| (name.clone(), self.fresh_var()))
                        .collect();
                    let inst_params: Vec<Ty> = f
                        .params
                        .iter()
                        .map(|(_, pty, _)| substitute(pty, &subst))
                        .collect();
                    for (i, arg) in args.iter().enumerate() {
                        let arg_ty = self.infer_expr(arg)?;
                        self.unify(
                            &inst_params[i],
                            &arg_ty,
                            arg.span(),
                            &format!("argument {} of function `{}`", i + 1, f.name),
                        )?;
                    }
                    // Default to concrete type args (unconstrained generic params default to i64)
                    let type_args: Vec<Ty> = f
                        .type_params
                        .iter()
                        .map(|name| {
                            let v = &subst[name];
                            self.compact(&self.resolve(v))
                        })
                        .collect();
                    self.instances.push(GenericInstance {
                        fn_def_id: *def_id,
                        type_args: type_args.clone(),
                    });
                    self.call_types.insert(span.start, type_args);
                    Ok(match &f.ret {
                        Some(t) => substitute(t, &subst),
                        None => Ty::Void,
                    })
                }
            }
        }
    }

    // ---------- unification engine ----------

    /// Create a fresh type variable.
    fn fresh_var(&mut self) -> Ty {
        let v = TypeVar(self.next_var);
        self.next_var += 1;
        Ty::Var(v)
    }

    /// Create a fresh integer-family type variable (for integer literals).
    fn fresh_int_var(&mut self) -> Ty {
        let v = TypeVar(self.next_var);
        self.next_var += 1;
        self.int_vars.insert(v);
        Ty::Var(v)
    }

    /// Resolve a type variable along the substitution chain.
    fn resolve(&self, ty: &Ty) -> Ty {
        let mut cur = ty.clone();
        for _ in 0..64 {
            match &cur {
                Ty::Var(v) => match self.subs.get(v) {
                    Some(next) => cur = next.clone(),
                    None => return cur,
                },
                _ => return cur,
            }
        }
        cur
    }

    /// Default remaining type variables to `i64` (recursive).
    fn compact(&self, ty: &Ty) -> Ty {
        match ty {
            // Resolve variables along the chain first: bound ones default to their concrete type,
            // only still-unconstrained ones (still variables after resolution) default to `i64`.
            Ty::Var(_) => {
                let r = self.resolve(ty);
                if matches!(r, Ty::Var(_)) {
                    Ty::I64
                } else {
                    self.compact(&r)
                }
            }
            Ty::Tuple(elems) => Ty::Tuple(elems.iter().map(|t| self.compact(t)).collect()),
            Ty::Array(elem, n) => Ty::Array(Box::new(self.compact(elem)), *n),
            Ty::Tensor { elem, shape } => Ty::Tensor {
                elem: Box::new(self.compact(elem)),
                shape: shape.clone(),
            },
            Ty::Ref { mut_, inner } => Ty::Ref {
                mut_: *mut_,
                inner: Box::new(self.compact(inner)),
            },
            Ty::Ptr(inner) => Ty::Ptr(Box::new(self.compact(inner))),
            Ty::Fn(params, ret) => Ty::Fn(
                params.iter().map(|t| self.compact(t)).collect(),
                Box::new(self.compact(ret)),
            ),
            other => other.clone(),
        }
    }

    /// Unify: `expected` and `actual` must resolve to the same type.
    fn unify(
        &mut self,
        expected: &Ty,
        actual: &Ty,
        span: Span,
        context: &str,
    ) -> Result<Ty, InferError> {
        let e = self.resolve(expected);
        let a = self.resolve(actual);
        match (&e, &a) {
            (Ty::Var(v), _) => {
                // Integer variables bind only to the integer family (no silent `[1, true]`)
                if self.int_vars.contains(v) && !a.is_int() {
                    return Err(InferError::new(
                        format!(
                            "type mismatch: {context} expected integer, got `{a}`"
                        ),
                        span,
                    ));
                }
                self.subs.insert(*v, a.clone());
                Ok(a)
            }
            (_, Ty::Var(v)) => {
                if self.int_vars.contains(v) && !e.is_int() {
                    return Err(InferError::new(
                        format!(
                            "type mismatch: {context} expected integer, got `{e}`"
                        ),
                        span,
                    ));
                }
                self.subs.insert(*v, e.clone());
                Ok(e)
            }
            (Ty::I32, Ty::I32)
            | (Ty::I64, Ty::I64)
            | (Ty::Bool, Ty::Bool)
            | (Ty::Str, Ty::Str)
            | (Ty::Void, Ty::Void) => Ok(e),
            // The same generic parameter unifies with itself (e.g. `a` and `b` in `fn max<T>(a: T, b: T)`)
            (Ty::Generic(x), Ty::Generic(y)) if x == y => Ok(e),
            (Ty::Tuple(xs), Ty::Tuple(ys)) if xs.len() == ys.len() => {
                for (x, y) in xs.iter().zip(ys.iter()) {
                    self.unify(x, y, span, context)?;
                }
                Ok(e)
            }
            (Ty::Array(x, n), Ty::Array(y, m)) if n == m => {
                self.unify(x, y, span, context)?;
                Ok(e)
            }
            (Ty::Tensor { elem: e1, shape: s1 }, Ty::Tensor { elem: e2, shape: s2 })
                if s1 == s2 =>
            {
                self.unify(e1, e2, span, context)?;
                Ok(e)
            }
            (Ty::Ref { mut_: m1, inner: i1 }, Ty::Ref { mut_: m2, inner: i2 })
                if m1 == m2 =>
            {
                self.unify(i1, i2, span, context)?;
                Ok(e)
            }
            (Ty::Ptr(p1), Ty::Ptr(p2)) => {
                self.unify(p1, p2, span, context)?;
                Ok(e)
            }
            (Ty::Arena(n), Ty::Arena(m)) if n == m => Ok(e),
            (Ty::Fn(p1, r1), Ty::Fn(p2, r2)) if p1.len() == p2.len() => {
                for (x, y) in p1.iter().zip(p2.iter()) {
                    self.unify(x, y, span, context)?;
                }
                self.unify(r1, r2, span, context)?;
                Ok(e)
            }
            _ => Err(InferError::new(
                format!(
                    "type mismatch: {context} expected `{e}`, got `{a}`"
                ),
                span,
            )),
        }
    }

    fn require_int(&self, ty: &Ty, span: Span, context: &str) -> Result<(), InferError> {
        match self.resolve(ty) {
            // Unbound type variables come from integer literals (default to integer); treat as legal
            Ty::I32 | Ty::I64 | Ty::Var(_) => Ok(()),
            // Generic parameters are undetermined inside a signature: allow, re-check at instantiation
            Ty::Generic(_) => Ok(()),
            other => Err(InferError::new(
                format!("{context} requires an integer type, got `{other}`"),
                span,
            )),
        }
    }

    fn require_bool(&self, ty: &Ty, span: Span, context: &str) -> Result<(), InferError> {
        match self.resolve(ty) {
            Ty::Bool => Ok(()),
            // Generic parameters are undetermined inside a signature: allow, re-check at instantiation
            Ty::Generic(_) => Ok(()),
            other => Err(InferError::new(
                format!("{context} requires a boolean type, got `{other}`"),
                span,
            )),
        }
    }

    fn lookup_var(&self, def_id: DefId, span: Span) -> Result<Ty, InferError> {
        match self.var_tys.get(&def_id) {
            Some(t) => Ok(t.clone()),
            None => Err(InferError::new(
                "variable is used before initialization; its type cannot be determined",
                span,
            )),
        }
    }
}

/// Function return-type context (for `return` statement checks).
#[derive(Clone)]
struct FnRetCtx<'a> {
    ret: Option<Ty>,
    name: &'a str,
}

/// Recursively replace `Ty::Generic(name)` in a type with the matching entry in `subst`.
/// Used to instantiate generic signatures at call sites (`T` in `fn max<T>(a: T) -> T`).
pub fn substitute(ty: &Ty, subst: &HashMap<String, Ty>) -> Ty {
    match ty {
        Ty::Generic(name) => subst
            .get(name)
            .cloned()
            .unwrap_or_else(|| ty.clone()),
        Ty::Tuple(elems) => Ty::Tuple(elems.iter().map(|t| substitute(t, subst)).collect()),
        Ty::Array(elem, n) => Ty::Array(Box::new(substitute(elem, subst)), *n),
        Ty::Ref { mut_, inner } => Ty::Ref {
            mut_: *mut_,
            inner: Box::new(substitute(inner, subst)),
        },
        Ty::Ptr(inner) => Ty::Ptr(Box::new(substitute(inner, subst))),
        Ty::Arena(n) => Ty::Arena(*n),
        Ty::Tensor { elem, shape } => Ty::Tensor {
            elem: Box::new(substitute(elem, subst)),
            shape: shape.clone(),
        },
        Ty::Fn(params, ret) => Ty::Fn(
            params.iter().map(|t| substitute(t, subst)).collect(),
            Box::new(substitute(ret, subst)),
        ),
        other => other.clone(),
    }
}
