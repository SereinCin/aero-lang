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

use aero_parse::ast::{BinOp, CmpOp};

use crate::borrowck::structurally_copyable;
use crate::hir::{
    BlasOp, DefId, HirBlock, HirConstDef, HirEnumDef, HirExpr, HirFn, HirImplBlock,
    HirMatchPattern, HirProgram, HirStmt, HirStructDef, HirTraitDef, HirUnionDef,
};
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
    /// Generic struct literal sites (keyed by expr byte offset span.start) → concrete
    /// type arguments (e.g. `Box { value: 1 }` with `Box<T>` → `[i64]`). Type args are
    /// resolved in a final pass after unification (unconstrained params default to i64).
    pub struct_lit_types: HashMap<usize, Vec<Ty>>,
    /// Generic enum literal sites (keyed by expr byte offset span.start) → concrete
    /// type arguments (e.g. `Maybe::Some(1)` with `Maybe<T>` → `[i64]`).
    pub enum_lit_types: HashMap<usize, Vec<Ty>>,
    /// Top-level const name → inferred value type (Phase P0-3). Written back to
    /// `HirConstDef.ty` by `lower_and_check` so unannotated consts (and their
    /// `ConstRef` uses) carry the real type instead of the placeholder `i64`.
    pub const_tys: HashMap<String, Ty>,
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
    /// The whole program (for Copy-ness lookups via `is_copy`)
    program: &'a HirProgram,
    /// Struct definitions (field lookup)
    structs: &'a [HirStructDef],
    /// Union definitions (field lookup)
    unions: &'a [HirUnionDef],
    /// Enum definitions (variant/payload lookup)
    enums: &'a [HirEnumDef],
    /// Const definitions (Phase P0-3): type inferred from the value expression.
    consts: &'a [HirConstDef],
    /// Impl blocks (trait bound validation)
    impls: &'a [HirImplBlock],
    /// Trait definitions (trait method signature lookup for generic receivers)
    traits: &'a [HirTraitDef],
    /// Method resolution table: (type_name, method_name) → DefId
    method_map: &'a HashMap<(String, String), DefId>,
    /// Trait bounds of the function currently being checked: (type_param, trait_name)
    cur_bounds: Vec<(String, String)>,
    /// Variable DefId → resolved type
    var_tys: HashMap<DefId, Ty>,
    /// Type variable → substitution target
    subs: HashMap<TypeVar, Ty>,
    /// Integer type variables (from integer literals): only bindable to the integer family
    int_vars: std::collections::HashSet<TypeVar>,
    /// Float type variables (from float literals): only bindable to the float family
    float_vars: std::collections::HashSet<TypeVar>,
    /// Generic instances (deduplicated)
    instances: Vec<GenericInstance>,
    /// Generic call site span.start → type arguments
    call_types: HashMap<usize, Vec<Ty>>,
    /// Generic struct literal span.start → type arguments (fresh vars; resolved at the end)
    struct_lit_types: HashMap<usize, Vec<Ty>>,
    /// Generic enum literal span.start → type arguments (fresh vars; resolved at the end)
    enum_lit_types: HashMap<usize, Vec<Ty>>,
    /// Top-level const name → inferred value type (Phase P0-3).
    const_tys: HashMap<String, Ty>,
    /// Type variable counter
    next_var: u32,
    /// Return type of the function currently being checked (None at top level).
    /// Used by `?` to validate that propagated errors match the function's result type.
    cur_ret: Option<Ty>,
    /// When processing a `let x: [T; 0] = [];` statement, this holds `Some(T)`
    /// so that the empty array literal can infer its element type from the annotation.
    empty_array_elem_ty: Option<Ty>,
}

impl<'a> Infer<'a> {
    pub fn check(program: &HirProgram) -> Result<InferResult, InferError> {
        let mut infer = Infer {
            funcs: &program.funcs,
            program,
            structs: &program.structs,
            unions: &program.unions,
            enums: &program.enums,
            consts: &program.consts,
            impls: &program.impls,
            traits: &program.traits,
            method_map: &program.method_map,
            cur_bounds: Vec::new(),
            var_tys: HashMap::new(),
            subs: HashMap::new(),
            int_vars: std::collections::HashSet::new(),
            float_vars: std::collections::HashSet::new(),
            instances: Vec::new(),
            call_types: HashMap::new(),
            struct_lit_types: HashMap::new(),
            enum_lit_types: HashMap::new(),
            const_tys: HashMap::new(),
            next_var: 0,
            cur_ret: None,
            empty_array_elem_ty: None,
        };
        // Function parameters register their types first (referenced by bodies); builtins have none
        for f in &program.funcs {
            if f.builtin {
                continue;
            }
            for (i, (pname, ty, pspan)) in f.params.iter().enumerate() {
                // NOTE: `Ty::Generic("__aero_fn__")` is kept as-is intentionally —
                // treated as a magic polymorphic higher-order type accepted by
                // `unify` at call sites and `compact` preserves it unchanged.
                // This lets builtin HOF stubs (filter/map/reduce) take any
                // function pointer as an argument without full monomorphization.
                let _ = (pname, pspan);
                infer
                    .var_tys
                    .insert(f.param_defs[i], ty.clone());
            }
        }
        // Validate `impl Copy for X {}` blocks: every field / variant payload must be
        // bitwise-copyable, and generic params cannot appear in payloads (a `T: Copy`
        // bound is not yet expressible). Runs before any body checking.
        infer.check_copy_impls()?;
        // Infer the type of each top-level const from its value expression. A const
        // that is used before this point carries its declared type; when no annotation
        // is given, the value's inferred type is recorded for codegen.
        infer.check_consts()?;
        // Check the main block (no function context; ``return`` not allowed)
        infer.check_block(&program.main, None)?;
        // Check each function body
        for f in &program.funcs {
            let ctx = FnRetCtx {
                ret: f.ret.clone(),
                name: &f.name,
            };
            infer.cur_bounds = f.trait_bounds.clone();
            infer.cur_ret = f.ret.clone();
            infer.check_block(&f.body, Some(ctx))?;
        }
        infer.cur_ret = None;
        // Deduplicate instances (same (function, type args) kept once)
        let mut seen: std::collections::HashSet<(DefId, String)> = std::collections::HashSet::new();
        infer.instances.retain(|inst| {
            let key = (inst.fn_def_id, format!("{:?}", inst.type_args));
            seen.insert(key)
        });
        // Final pass: resolve the (possibly deferred) type args of generic struct/enum
        // literals. Unconstrained fresh variables default to i64, mirroring the defaulting
        // applied to generic function call sites.
        let struct_lit_types: HashMap<usize, Vec<Ty>> = infer
            .struct_lit_types
            .iter()
            .map(|(k, args)| (*k, args.iter().map(|a| infer.compact(&infer.resolve(a))).collect()))
            .collect();
        let enum_lit_types: HashMap<usize, Vec<Ty>> = infer
            .enum_lit_types
            .iter()
            .map(|(k, args)| (*k, args.iter().map(|a| infer.compact(&infer.resolve(a))).collect()))
            .collect();
        Ok(InferResult {
            var_tys: infer.var_tys,
            instances: infer.instances,
            call_types: infer.call_types,
            struct_lit_types,
            enum_lit_types,
            const_tys: infer.const_tys,
        })
    }

    /// Validate every `impl Copy for X {}`: each field / variant payload of `X` must be
    /// structurally copyable (no `String`/`Vec`/`arena`/non-Copy nesting), and generic
    /// params may not appear in a payload (a `T: Copy` bound is not expressible yet).
    fn check_copy_impls(&self) -> Result<(), InferError> {
        for imp in self.impls {
            if imp.trait_name.as_deref() != Some("Copy") {
                continue;
            }
            // Generic params of the impl stay unresolved (Generic) in field types; they
            // are rejected below — mirroring Rust's `T: Copy` requirement.
            let subst: HashMap<String, Ty> = imp
                .type_params
                .iter()
                .map(|tp| (tp.clone(), Ty::Generic(tp.clone())))
                .collect();
            let mut err_field: Option<(String, Ty)> = None;
            let mut err_generic = false;
            if let Some(def) = self.structs.iter().find(|s| s.name == imp.type_name) {
                for (fname, fty) in &def.fields {
                    let fty = substitute(fty, &subst);
                    if contains_generic(&fty) {
                        err_generic = true;
                        break;
                    }
                    if !structurally_copyable(&fty, self.impls) {
                        err_field = Some((fname.clone(), fty));
                        break;
                    }
                }
            } else if let Some(def) = self.enums.iter().find(|e| e.name == imp.type_name) {
                for (vname, payload) in &def.variants {
                    if let Some(pt) = payload {
                        let pt = substitute(pt, &subst);
                        if contains_generic(&pt) {
                            err_generic = true;
                            break;
                        }
                        if !structurally_copyable(&pt, self.impls) {
                            err_field = Some((vname.clone(), pt));
                            break;
                        }
                    }
                }
            } else {
                return Err(InferError::new(
                    format!("`impl Copy for {}` targets an unknown type", imp.type_name),
                    imp.span,
                ));
            }
            if err_generic {
                return Err(InferError::new(
                    format!(
                        "`impl Copy for {}` uses a generic parameter in its fields; a `T: Copy` bound is not supported yet (remove the type parameter or use concrete types)",
                        imp.type_name
                    ),
                    imp.span,
                ));
            }
            if let Some((fname, fty)) = err_field {
                return Err(InferError::new(
                    format!(
                        "cannot implement `Copy` for `{}`: field `{}` has type `{}`, which is not Copy (String/Vec/arena and non-Copy types cannot be bitwise-copied)",
                        imp.type_name, fname, fty
                    ),
                    imp.span,
                ));
            }
        }
        Ok(())
    }

    /// Infer the type of each top-level const from its value expression and
    /// validate it against the declared annotation (Phase P0-3). The type is
    /// recorded so codegen emits the constant with the right bit width.
    fn check_consts(&mut self) -> Result<(), InferError> {
        for c in self.consts {
            let value_ty = self.infer_expr(&c.value)?;
            // `const X: i32 = 1` with an unconstrained integer literal: bind the
            // literal to the annotated integer width *before* compacting, so it
            // resolves to `i32` instead of defaulting to `i64` (required by FFI
            // constants like `const REG_EXTENDED: i32 = 1`).
            let resolved = self.resolve(&value_ty);
            if c.has_ty
                && matches!(resolved, Ty::Var(_))
                && matches!(self.resolve(&c.ty), Ty::I32 | Ty::I64)
            {
                self.unify(&c.ty, &value_ty, c.span, &format!("const `{}`", c.name))?;
            }
            // `compact` defaults still-unconstrained int literals to i64 (and
            // resolves bound vars), giving every const a concrete scalar type.
            let value_ty = self.compact(&value_ty);
            // A scalar const must fold at compile time; codegen will verify by
            // attempting evaluation. Here we only type-check the value.
            if !is_scalar_const_ty(&value_ty) {
                return Err(InferError::new(
                    format!(
                        "const `{}` must have a scalar type (i32/i64/f32/f64/bool/char), but its value has type `{value_ty}`",
                        c.name
                    ),
                    c.span,
                ));
            }
            // Unify the declared type (if an annotation was given) with the
            // value's inferred type. Without an annotation, the value's type wins.
            if c.has_ty {
                self.unify(&c.ty, &value_ty, c.span, &format!("const `{}`", c.name))?;
            }
            // Record the resolved value type so `lower_and_check` can write it back
            // to `HirConstDef.ty` (unannotated consts would otherwise keep the
            // placeholder `i64`, breaking float consts at use sites).
            self.const_tys.insert(c.name.clone(), value_ty);
        }
        Ok(())
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
                mut_: _,
                def_id,
                ty_ann,
                init,
                span,
            } => {
                // If the let statement has a `[T; 0]` type annotation, record the element
                // type so that an empty `[]` initializer can infer its element type from it.
                if let Some(Ty::Array(elem, 0)) = ty_ann {
                    self.empty_array_elem_ty = Some((**elem).clone());
                }
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
                        let ann_r = self.resolve(ann);
                        // FFI NULL init: a raw-pointer variable initialized from an
                        // integer literal (`let stmt: *T = 0;`) is allowed — the value
                        // is the NULL pointer, mirroring Rust's `let p: *mut T = null_mut()`.
                        if matches!(ann_r, Ty::Ptr(_))
                            && matches!(init, HirExpr::IntLit(..))
                        {
                            let final_ty = self.compact(&ann_r);
                            self.var_tys.insert(*def_id, final_ty);
                        } else {
                            // Unify first (literal variables can adapt to i32/i64), then default
                            self.unify(ann, &init_ty, *span, &format!("variable `{name}`"))?;
                            let final_ty = self.compact(&self.resolve(ann));
                            self.var_tys.insert(*def_id, final_ty);
                        }
                    }
                    None => {
                        let final_ty = self.compact(&self.resolve(&init_ty));
                        self.var_tys.insert(*def_id, final_ty);
                    }
                }
                self.empty_array_elem_ty = None;
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
                // String/str index-write stores a raw byte: accept integers or `char`.
                if matches!(target_ty, Ty::Str | Ty::String) {
                    let value_ty = self.infer_expr(value)?;
                    self.require_byte(&value_ty, *span, "index-write value")?;
                    return Ok(());
                }
                let elem_ty = match &target_ty {
                    Ty::Array(elem, _) => (**elem).clone(),
                    Ty::Ptr(elem) => (**elem).clone(),
                    Ty::Vec(elem) => (**elem).clone(),
                    Ty::Tensor { shape, elem } if !shape.is_empty() => (**elem).clone(),
                    other => {
                        return Err(InferError::new(
                            format!("cannot index-write into type `{other}` (only arrays/pointers/tensors/vecs/strings)"),
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
                    Ty::Ref { mut_: true, inner, .. } => (**inner).clone(),
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
            HirStmt::AssignField { target, field, value, span } => {
                // `recv.field = value`: recv must be a struct (value semantics; writes copy back)
                let recv_ty = self.infer_expr(target)?;
                let recv_ty = self.compact(&self.resolve(&recv_ty));
                // Auto-deref: writing through `&mut T` (e.g. inside `fn next(it: &mut Self)`)
                let recv_ty = match &recv_ty {
                    Ty::Ref { inner, .. } => (**inner).clone(),
                    other => other.clone(),
                };
                let field_ty = match &recv_ty {
                    Ty::Struct(name) => match self.lookup_struct_field(name, field, *span)? {
                        Some(t) => t,
                        None => {
                            return Err(InferError::new(
                                format!("struct `{name}` has no field `{field}`"),
                                *span,
                            ));
                        }
                    },
                    Ty::Union(name) => {
                        let def = self
                            .unions
                            .iter()
                            .find(|u| u.name == *name)
                            .ok_or_else(|| {
                                InferError::new(format!("undefined union `{name}`"), *span)
                            })?;
                        match def.find_field(field) {
                            Some((_, t)) => t.clone(),
                            None => {
                                return Err(InferError::new(
                                    format!("union `{name}` has no field `{field}`"),
                                    *span,
                                ));
                            }
                        }
                    }
                    Ty::StructGeneric { name, args } => {
                        let def = match self.lookup_struct(name, *span)? {
                            Some(d) => d,
                            None => {
                                return Err(InferError::new(
                                    format!("undefined struct `{name}`"),
                                    *span,
                                ));
                            }
                        };
                        let subst: HashMap<String, Ty> = def
                            .type_params
                            .iter()
                            .cloned()
                            .zip(args.iter().cloned())
                            .collect();
                        let fty = match def.find_field(field) {
                            Some((_, t)) => t.clone(),
                            None => {
                                return Err(InferError::new(
                                    format!("struct `{name}` has no field `{field}`"),
                                    *span,
                                ));
                            }
                        };
                        substitute(&fty, &subst)
                    }
                    other => {
                        return Err(InferError::new(
                            format!("cannot assign field `.{field}` on type `{other}` (not a struct)"),
                            *span,
                        ));
                    }
                };
                let value_ty = self.infer_expr(value)?;
                self.unify(&field_ty, &value_ty, *span, "field-write value")?;
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
            HirStmt::Loop { body, .. } => {
                self.check_block(body, ctx)?;
                Ok(())
            }
            HirStmt::For { var_def, iter, body, span, .. } => {
                let iter_ty = self.infer_expr(iter)?;
                let iter_ty = self.compact(&self.resolve(&iter_ty));
                // Native iterables (arrays / tensors / `Vec<T>`) are iterated directly
                // by the code generator with an index loop; the loop variable gets the
                // element type.
                // User-defined iterables follow the `IntoIterator`/`Iterator` protocol:
                // `for x in iter` ≈ `for x in iter.into_iter()`, and the element type is
                // the payload of the `Option<Item>` returned by the iterator's `next`.
                let elem_ty = match &iter_ty {
                    Ty::Array(elem, _) => (**elem).clone(),
                    Ty::Vec(elem) => (**elem).clone(),
                    Ty::Tensor { shape, elem } if !shape.is_empty() => {
                        if shape.len() == 1 {
                            (**elem).clone()
                        } else {
                            Ty::Tensor {
                                elem: elem.clone(),
                                shape: shape[1..].to_vec(),
                            }
                        }
                    }
                    other => self.iter_protocol_item(other, *span)?,
                };
                self.var_tys.insert(*var_def, elem_ty);
                self.check_block(body, ctx)?;
                Ok(())
            }
            HirStmt::Break(_) => Ok(()),
            HirStmt::Continue(_) => Ok(()),
            HirStmt::Match { scrutinee, arms, span } => {
                let scrut_ty = self.infer_expr(scrutinee)?;
                let scrut_ty = self.compact(&self.resolve(&scrut_ty));
                for arm in arms {
                    // Check pattern matches scrutinee type
                    match &arm.pattern {
                        HirMatchPattern::IntLit(_) => {
                            if !scrut_ty.is_int() {
                                return Err(InferError::new(
                                    format!(
                                        "match pattern is integer but scrutinee is `{scrut_ty}`"
                                    ),
                                    arm.span,
                                ));
                            }
                        }
                        HirMatchPattern::BoolLit(_) => {
                            if scrut_ty != Ty::Bool {
                                return Err(InferError::new(
                                    format!(
                                        "match pattern is boolean but scrutinee is `{scrut_ty}`"
                                    ),
                                    arm.span,
                                ));
                            }
                        }
                        HirMatchPattern::CharLit(_) => {
                            if scrut_ty != Ty::Char {
                                return Err(InferError::new(
                                    format!(
                                        "match pattern is char but scrutinee is `{scrut_ty}`"
                                    ),
                                    arm.span,
                                ));
                            }
                        }
                        HirMatchPattern::StrLit(_) => {
                            if scrut_ty != Ty::Str {
                                return Err(InferError::new(
                                    format!(
                                        "match pattern is string but scrutinee is `{scrut_ty}`"
                                    ),
                                    arm.span,
                                ));
                            }
                        }
                        HirMatchPattern::Bind(_, def_id) => {
                            self.var_tys.insert(*def_id, scrut_ty.clone());
                        }
                        HirMatchPattern::EnumVariant {
                            enum_name,
                            variant,
                            bind,
                            span,
                        } => {
                            // Scrutinee must be the resolved enum type (possibly a
                            // generic instance whose args substitute into the payload).
                            let (expected_name, instance_args) = match &scrut_ty {
                                Ty::Enum(n) => (n.clone(), None),
                                Ty::EnumGeneric { name, args } => (name.clone(), Some(args.clone())),
                                other => {
                                    return Err(InferError::new(
                                        format!(
                                            "match pattern is `{enum_name}::{variant}` but scrutinee is `{other}`"
                                        ),
                                        *span,
                                    ));
                                }
                            };
                            if &expected_name != enum_name {
                                return Err(InferError::new(
                                    format!(
                                        "match pattern is `{enum_name}::{variant}` but scrutinee is `{scrut_ty}`"
                                    ),
                                    *span,
                                ));
                            }
                            let def = self.lookup_enum(enum_name, *span)?;
                            let payload = match def.find_variant(variant) {
                                Some((_, p)) => p.clone(),
                                None => {
                                    return Err(InferError::new(
                                        format!("enum `{enum_name}` has no variant `{variant}`"),
                                        *span,
                                    ));
                                }
                            };
                            if let Some((_, def_id)) = bind {
                                let pt = payload.ok_or_else(|| {
                                    InferError::new(
                                        format!("variant `{enum_name}::{variant}` has no payload to bind"),
                                        *span,
                                    )
                                })?;
                                // Substitute the generic instance args into the payload type.
                                let pt = match instance_args {
                                    Some(args) => {
                                        let mut subst: HashMap<String, Ty> = def
                                            .type_params
                                            .iter()
                                            .cloned()
                                            .zip(args.iter().cloned())
                                            .collect();
                                        substitute(&pt, &subst)
                                    }
                                    None => pt,
                                };
                                self.var_tys.insert(*def_id, pt);
                            }
                        }
                        HirMatchPattern::Wildcard => {}
                    }
                    self.check_block(&arm.body, ctx.clone())?;
                }
                let _ = span;
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
            HirStmt::StructDef { .. } => Ok(()),
            // Enum definitions are collected at lowering; nothing to type-check here.
            HirStmt::EnumDef { .. } => Ok(()),
            HirStmt::TraitDef { .. } => Ok(()),
            HirStmt::ImplBlock { .. } => Ok(()),
        }
    }

    fn infer_expr(&mut self, expr: &HirExpr) -> Result<Ty, InferError> {
        match expr {
            HirExpr::IntLit(_, _) => Ok(self.fresh_int_var()),
            HirExpr::FloatLit(_, _) => Ok(self.fresh_float_var()),
            HirExpr::CharLit(_, _) => Ok(Ty::Char),
            HirExpr::BoolLit(_, _) => Ok(Ty::Bool),
            HirExpr::StrLit(_, _) => Ok(Ty::Str),
            HirExpr::Var(def_id, span) => self.lookup_var(*def_id, *span),
            HirExpr::ConstRef { ty, .. } => Ok(ty.clone()),
            HirExpr::Borrow {
                mut_,
                def_id,
                span,
            } => {
                let src_ty = self.lookup_var(*def_id, *span)?;
                let src_ty = self.resolve(&src_ty);
                // Raw pointers are borrowable too: FFI out-params need `&handle` where
                // `handle: *T` (e.g. `sqlite3_open(path, &db)`), mirroring Rust's `&mut *mut T`.
                if !(src_ty.is_borrowable() || matches!(&src_ty, Ty::Ptr(_))) {
                    return Err(InferError::new(
                        format!("cannot borrow a value of type `{src_ty}`"),
                        *span,
                    ));
                }
                let t = self.fresh_var();
                self.unify(&src_ty, &t, *span, "borrow target type")?;
                // FFI out-params: borrowing a raw pointer `*T` yields `**T` (a raw pointer
                // to the pointer slot), so `&handle` matches a C signature expecting
                // `T**` (e.g. `sqlite3_open(path, &db)`), mirroring Rust's `&mut *mut T`.
                if matches!(&src_ty, Ty::Ptr(_)) {
                    Ok(Ty::Ptr(Box::new(src_ty)))
                } else {
                    Ok(Ty::Ref {
                        mut_: *mut_,
                        lifetime: None,
                        inner: Box::new(t),
                    })
                }
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
            HirExpr::Try { target, span } => {
                // `expr?`: the target must be a `Result<T, E>`; the expression evaluates to
                // `T`, and on `Err(e)` the enclosing function returns `Err(e)` immediately.
                let rt = self.infer_expr(target)?;
                let rt = self.compact(&self.resolve(&rt));
                let (ok_ty, err_ty) = match &rt {
                    Ty::EnumGeneric { name, args } if name == "Result" && args.len() == 2 => {
                        (args[0].clone(), args[1].clone())
                    }
                    other => {
                        return Err(InferError::new(
                            format!(
                                "`?` can only be applied to a `Result<T, E>`, got `{other}`"
                            ),
                            *span,
                        ));
                    }
                };
                // The enclosing function must return a `Result` whose error type matches.
                match self.cur_ret.clone() {
                    Some(ret) => {
                        let ret = self.compact(&self.resolve(&ret));
                        match &ret {
                            Ty::EnumGeneric { name, args }
                                if name == "Result" && args.len() == 2 =>
                            {
                                let ret_e = args[1].clone();
                                self.unify(
                                    &err_ty,
                                    &ret_e,
                                    *span,
                                    "`?` error type (must match the function's return error type)",
                                )?;
                            }
                            other => {
                                return Err(InferError::new(
                                    format!(
                                        "`?` requires the enclosing function to return `Result<_, E>`, got `{other}`"
                                    ),
                                    *span,
                                ));
                            }
                        }
                    }
                    None => {
                        return Err(InferError::new(
                            "`?` can only be used inside a function that returns `Result<_, E>`",
                            *span,
                        ));
                    }
                }
                Ok(ok_ty)
            }
            HirExpr::MethodCall {
                recv,
                method,
                args,
                span,
            } => {
                let recv_ty = self.infer_expr(recv)?;
                let recv_ty = self.compact(&self.resolve(&recv_ty));
                // Auto-deref: calling a native method through a reference receiver
                // (`&T` / `&mut T`) dispatches on the inner type, e.g. `v.push(1)` when
                // `v` is a `&mut Vec<i64>` parameter. The generic method path below
                // still sees the original (possibly reference) receiver.
                let recv_ty = match &recv_ty {
                    Ty::Ref { inner, .. } | Ty::Ptr(inner) => {
                        self.compact(&self.resolve(inner))
                    }
                    other => other.clone(),
                };
                // Arena methods (existing behavior): alloc/reset
                if let Ty::Arena(size) = &recv_ty {
                    let arena_size = *size;
                    return match method.as_str() {
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
                    };
                }
                // Native `Vec<T>` methods (compiler-provided heap vector).
                if let Ty::Vec(elem) = &recv_ty {
                    let elem = (*elem).clone();
                    return match method.as_str() {
                        "push" => {
                            if args.len() != 1 {
                                return Err(InferError::new(
                                    "`push` requires 1 argument (the element to append)",
                                    *span,
                                ));
                            }
                            let a_ty = self.infer_expr(&args[0])?;
                            self.unify(&elem, &a_ty, *span, "`push` element")?;
                            Ok(Ty::Void)
                        }
                        "pop" => {
                            if !args.is_empty() {
                                return Err(InferError::new(
                                    "`pop` takes no arguments",
                                    *span,
                                ));
                            }
                            Ok((*elem).clone())
                        }
                        "len" => {
                            if !args.is_empty() {
                                return Err(InferError::new(
                                    "`len` takes no arguments",
                                    *span,
                                ));
                            }
                            Ok(Ty::I64)
                        }
                        "is_empty" => {
                            if !args.is_empty() {
                                return Err(InferError::new(
                                    "`is_empty` takes no arguments",
                                    *span,
                                ));
                            }
                            Ok(Ty::Bool)
                        }
                        "get" => {
                            if args.len() != 1 {
                                return Err(InferError::new(
                                    "`get` requires 1 argument (the index)",
                                    *span,
                                ));
                            }
                            let i_ty = self.infer_expr(&args[0])?;
                            self.require_int(&i_ty, *span, "`get` index")?;
                            Ok((*elem).clone())
                        }
                        "set" => {
                            if args.len() != 2 {
                                return Err(InferError::new(
                                    "`set` requires 2 arguments (index, element)",
                                    *span,
                                ));
                            }
                            let i_ty = self.infer_expr(&args[0])?;
                            self.require_int(&i_ty, *span, "`set` index")?;
                            let v_ty = self.infer_expr(&args[1])?;
                            self.unify(&elem, &v_ty, *span, "`set` element")?;
                            Ok(Ty::Void)
                        }
                        "free" => {
                            if !args.is_empty() {
                                return Err(InferError::new(
                                    "`free` takes no arguments",
                                    *span,
                                ));
                            }
                            Ok(Ty::Void)
                        }
                        other => Err(InferError::new(
                            format!("`Vec` has no method `{other}` (supported: push/pop/len/get/set/free/is_empty)"),
                            *span,
                        )),
                    };
                }
                // Native `Box<T>` methods (compiler-provided heap smart pointer).
                if let Ty::Box(inner) = &recv_ty {
                    let inner = (*inner).clone();
                    return match method.as_str() {
                        "deref" => {
                            if !args.is_empty() {
                                return Err(InferError::new(
                                    "`deref` takes no arguments",
                                    *span,
                                ));
                            }
                            Ok(*inner)
                        }
                        "free" => {
                            if !args.is_empty() {
                                return Err(InferError::new(
                                    "`free` takes no arguments",
                                    *span,
                                ));
                            }
                            Ok(Ty::Void)
                        }
                        other => Err(InferError::new(
                            format!("`Box` has no method `{other}` (supported: deref/free)"),
                            *span,
                        )),
                    };
                }
                // Native `String` methods (compiler-provided heap string).
                if let Ty::String = &recv_ty {
                    return match method.as_str() {
                        "push" => {
                            if args.len() != 1 {
                                return Err(InferError::new(
                                    "`push` requires 1 argument (the byte to append)",
                                    *span,
                                ));
                            }
                            let a_ty = self.infer_expr(&args[0])?;
                            self.require_byte(&a_ty, *span, "`push` byte")?;
                            Ok(Ty::Void)
                        }
                        "push_str" => {
                            if args.len() != 1 {
                                return Err(InferError::new(
                                    "`push_str` requires 1 argument (the string to append)",
                                    *span,
                                ));
                            }
                            let a_ty = self.infer_expr(&args[0])?;
                            let a_ty = self.compact(&self.resolve(&a_ty));
                            if a_ty != Ty::Str {
                                return Err(InferError::new(
                                    format!("`push_str` requires a `str` argument, got `{a_ty}`"),
                                    *span,
                                ));
                            }
                            Ok(Ty::Void)
                        }
                        "pop" => {
                            if !args.is_empty() {
                                return Err(InferError::new(
                                    "`pop` takes no arguments",
                                    *span,
                                ));
                            }
                            Ok(Ty::I64)
                        }
                        "len" => {
                            if !args.is_empty() {
                                return Err(InferError::new(
                                    "`len` takes no arguments",
                                    *span,
                                ));
                            }
                            Ok(Ty::I64)
                        }
                        "is_empty" => {
                            if !args.is_empty() {
                                return Err(InferError::new(
                                    "`is_empty` takes no arguments",
                                    *span,
                                ));
                            }
                            Ok(Ty::Bool)
                        }
                        "clear" => {
                            if !args.is_empty() {
                                return Err(InferError::new(
                                    "`clear` takes no arguments",
                                    *span,
                                ));
                            }
                            Ok(Ty::Void)
                        }
                        "at" => {
                            if args.len() != 1 {
                                return Err(InferError::new(
                                    "`at` requires 1 argument (the index)",
                                    *span,
                                ));
                            }
                            let i_ty = self.infer_expr(&args[0])?;
                            self.require_int(&i_ty, *span, "`at` index")?;
                            Ok(Ty::I64)
                        }
                        "data" => {
                            if !args.is_empty() {
                                return Err(InferError::new(
                                    "`data` takes no arguments",
                                    *span,
                                ));
                            }
                            Ok(Ty::Str)
                        }
                        "free" => {
                            if !args.is_empty() {
                                return Err(InferError::new(
                                    "`free` takes no arguments",
                                    *span,
                                ));
                            }
                            Ok(Ty::Void)
                        }
                        "starts_with" | "ends_with" => {
                            if args.len() != 1 {
                                return Err(InferError::new(
                                    format!("`{method}` requires 1 argument (the substring)"),
                                    *span,
                                ));
                            }
                            let sub_ty = self.infer_expr(&args[0])?;
                            let sub_ty = self.compact(&self.resolve(&sub_ty));
                            if sub_ty != Ty::Str {
                                return Err(InferError::new(
                                    format!("`{method}` requires a `str` argument, got `{sub_ty}`"),
                                    *span,
                                ));
                            }
                            Ok(Ty::Bool)
                        }
                        "utf8_push" => {
                            if args.len() != 1 {
                                return Err(InferError::new(
                                    "`utf8_push` requires 1 argument (the code point to append)",
                                    *span,
                                ));
                            }
                            let cp_ty = self.infer_expr(&args[0])?;
                            self.require_byte(&cp_ty, *span, "`utf8_push` code point")?;
                            Ok(Ty::Void)
                        }
                        "utf8_pop" => {
                            if !args.is_empty() {
                                return Err(InferError::new(
                                    "`utf8_pop` takes no arguments",
                                    *span,
                                ));
                            }
                            Ok(Ty::I64)
                        }
                        other => Err(InferError::new(
                            format!("`String` has no method `{other}` (supported: push/push_str/utf8_push/pop/utf8_pop/len/is_empty/clear/at/data/starts_with/ends_with/free)"),
                            *span,
                        )),
                    };
                }
                // Trait / inherent method call: resolve via the method table.
                // The receiver becomes the implicit first argument (`self`).
                // Generic receiver `T`: resolve through the trait bound `<T: Trait>`.
                // The method must be declared by the bound trait; signature verification
                // for the concrete type happens at instantiation (impl sigs are checked
                // against trait sigs in lowering, bounds at the call site).
                if let Ty::Generic(tp) = &recv_ty {
                    let trait_name = self
                        .cur_bounds
                        .iter()
                        .find(|(bound_tp, _)| bound_tp == tp)
                        .map(|(_, tn)| tn.clone())
                        .ok_or_else(|| {
                            InferError::new(
                                format!(
                                    "type parameter `{tp}` has no trait bound, so it has no methods"
                                ),
                                *span,
                            )
                        })?;
                    let trait_def = self
                        .traits
                        .iter()
                        .find(|t| t.name == trait_name)
                        .ok_or_else(|| {
                            InferError::new("internal error: bound trait table missing", *span)
                        })?;
                    let m = trait_def.find_method(method).ok_or_else(|| {
                        InferError::new(
                            format!("trait `{trait_name}` has no method `{method}`"),
                            *span,
                        )
                    })?;
                    if m.params.len() != 1 + args.len() {
                        return Err(InferError::new(
                            format!(
                                "method `{method}` takes {} argument(s), but {} were passed",
                                m.params.len() - 1,
                                args.len()
                            ),
                            *span,
                        ));
                    }
                    return Ok(m.ret.clone().unwrap_or(Ty::Void));
                }
                // `dyn Trait` receiver: dispatch through the trait's method signature.
                // The concrete implementation is chosen at runtime via the vtable.
                if let Ty::Dyn { trait_name } = &recv_ty {
                    let trait_def = self
                        .traits
                        .iter()
                        .find(|t| t.name == *trait_name)
                        .ok_or_else(|| {
                            InferError::new("internal error: dyn trait table missing", *span)
                        })?;
                    let m = trait_def.find_method(method).ok_or_else(|| {
                        InferError::new(
                            format!("trait `{trait_name}` has no method `{method}`"),
                            *span,
                        )
                    })?;
                    if m.params.len() != 1 + args.len() {
                        return Err(InferError::new(
                            format!(
                                "method `{method}` takes {} argument(s), but {} were passed",
                                m.params.len() - 1,
                                args.len()
                            ),
                            *span,
                        ));
                    }
                    // Validate the extra arguments against the trait method's parameter
                    // types (the receiver itself is the fat pointer, not passed by value).
                    for (i, arg) in args.iter().enumerate() {
                        let arg_ty = self.infer_expr(arg)?;
                        self.unify(
                            &m.params[i + 1].1,
                            &arg_ty,
                            arg.span(),
                            &format!("argument {} of method `{method}`", i + 1),
                        )?;
                    }
                    return Ok(m.ret.clone().unwrap_or(Ty::Void));
                }
                let type_name = match &recv_ty {
                    Ty::Struct(n) | Ty::Union(n) | Ty::Enum(n) => n.clone(),
                    Ty::StructGeneric { name, .. } | Ty::EnumGeneric { name, .. } => name.clone(),
                    other => {
                        return Err(InferError::new(
                            format!(
                                "type `{other}` has no methods (method calls require a struct/enum receiver or an arena)"
                            ),
                            *span,
                        ));
                    }
                };
                let f_def_id = self
                    .method_map
                    .get(&(type_name.clone(), method.clone()))
                    .copied()
                    .ok_or_else(|| {
                        InferError::new(
                            format!("type `{type_name}` has no method `{method}`"),
                            *span,
                        )
                    })?;
                let f = self.funcs.get(f_def_id as usize).ok_or_else(|| {
                    InferError::new("internal error: method function table missing", *span)
                })?;
                // f.params[0] is the receiver parameter; remaining params match args
                if f.params.len() != 1 + args.len() {
                    return Err(InferError::new(
                        format!(
                            "method `{method}` takes {} argument(s), but {} were passed",
                            f.params.len() - 1,
                            args.len()
                        ),
                        *span,
                    ));
                }
                if f.type_params.is_empty() {
                    // Non-generic method: unify the receiver and arguments directly.
                    // If the method takes `&T`/`&mut T` (e.g. `fn next(it: &mut Self)`),
                    // auto-reference the receiver: `recv.method()` ≈ `method(&recv)`.
                    self.unify_receiver(
                        &f.params[0].1,
                        &recv_ty,
                        recv.span(),
                        &format!("receiver of method `{method}`"),
                    )?;
                    for (i, arg) in args.iter().enumerate() {
                        let arg_ty = self.infer_expr(arg)?;
                        self.unify(
                            &f.params[i + 1].1,
                            &arg_ty,
                            arg.span(),
                            &format!("argument {} of method `{method}`", i + 1),
                        )?;
                    }
                    Ok(match &f.ret {
                        Some(t) => t.clone(),
                        None => Ty::Void,
                    })
                } else {
                    // Generic method (from `impl<T> Type<T>`): instantiate with the
                    // receiver's type arguments. The impl's type params align 1:1 with
                    // the receiver's args (`impl<T> Vec<T>` ⇒ receiver `Vec<i64>` ⇒ T=i64).
                    let recv_args = match &recv_ty {
                        Ty::StructGeneric { args, .. } | Ty::EnumGeneric { args, .. } => args.clone(),
                        _ => {
                            return Err(InferError::new(
                                format!(
                                    "generic method `{method}` on `{type_name}` requires a generic receiver (e.g. `{type_name}<i64>`); got `{recv_ty}`"
                                ),
                                *span,
                            ));
                        }
                    };
                    if f.type_params.len() != recv_args.len() {
                        return Err(InferError::new(
                            format!(
                                "internal error: generic method `{method}` parameter count mismatch (declared {}, receiver has {})",
                                f.type_params.len(),
                                recv_args.len()
                            ),
                            *span,
                        ));
                    }
                    let subst: HashMap<String, Ty> = f
                        .type_params
                        .iter()
                        .cloned()
                        .zip(recv_args.iter().cloned())
                        .collect();
                    let this_ty = substitute(&f.params[0].1, &subst);
                    self.unify_receiver(
                        &this_ty,
                        &recv_ty,
                        recv.span(),
                        &format!("receiver of method `{method}`"),
                    )?;
                    for (i, arg) in args.iter().enumerate() {
                        let arg_ty = self.infer_expr(arg)?;
                        let param_ty = substitute(&f.params[i + 1].1, &subst);
                        self.unify(
                            &param_ty,
                            &arg_ty,
                            arg.span(),
                            &format!("argument {} of method `{method}`", i + 1),
                        )?;
                    }
                    // Record the concrete instance for codegen monomorphization.
                    self.instances.push(GenericInstance {
                        fn_def_id: f_def_id,
                        type_args: recv_args.clone(),
                    });
                    self.call_types.insert(span.start, recv_args);
                    Ok(match &f.ret {
                        Some(t) => substitute(t, &subst),
                        None => Ty::Void,
                    })
                }
            }
            HirExpr::ArenaLit(n, _) => Ok(Ty::Arena(*n)),
            HirExpr::TensorLit { dims, elem, .. } => Ok(Ty::Tensor {
                elem: Box::new(elem.clone()),
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
            HirExpr::Reduce { input, span, .. } => {
                let ty = self.infer_expr(input)?;
                let t = self.compact(&self.resolve(&ty));
                match t {
                    Ty::Tensor { elem, shape } if !shape.is_empty() => Ok(*elem),
                    other => Err(InferError::new(
                        format!("`sum`/`mean`/`max`/`min` expects a tensor argument, got `{other}`"),
                        *span,
                    )),
                }
            }
            HirExpr::ElemWise { lhs, rhs, span, .. } => {
                let l_ty = self.infer_expr(lhs)?;
                let lt = self.compact(&self.resolve(&l_ty));
                let (elem, shape) = match &lt {
                    Ty::Tensor { elem, shape } if !shape.is_empty() => (elem.clone(), shape.clone()),
                    other => {
                        return Err(InferError::new(
                            format!("element-wise tensor op expects a tensor argument, got `{other}`"),
                            *span,
                        ));
                    }
                };
                if let Some(rhs) = rhs {
                    let r_ty = self.infer_expr(rhs)?;
                    let rt = self.compact(&self.resolve(&r_ty));
                    let (relem, rshape) = match &rt {
                        Ty::Tensor { elem, shape } if !shape.is_empty() => (elem.clone(), shape.clone()),
                        other => {
                            return Err(InferError::new(
                                format!("element-wise tensor op expects a tensor argument, got `{other}`"),
                                *span,
                            ));
                        }
                    };
                    if shape != rshape {
                        return Err(InferError::new(
                            format!("element-wise tensor op shape mismatch: {shape:?} vs {rshape:?}"),
                            *span,
                        ));
                    }
                    self.unify(&elem, &relem, *span, "element types of element-wise tensor operands")?;
                }
                let elem = self.compact(&self.resolve(&elem));
                Ok(Ty::Tensor {
                    elem: Box::new(elem),
                    shape,
                })
            }
            HirExpr::Blas { op, args, span } => {
                // `scal` / `axpy` take a scalar `alpha` before the tensor operand(s).
                let first_tensor = match op {
                    BlasOp::Dot | BlasOp::Nrm2 | BlasOp::Asum | BlasOp::Amax => 0,
                    BlasOp::Scal | BlasOp::Axpy => 1,
                };
                // Resolve (elem, shape) of a tensor-typed argument.
                let pull_tensor = |arg: &HirExpr,
                                   self_: &mut Self|
                 -> Result<(Ty, Vec<usize>), InferError> {
                    let ty = self_.infer_expr(arg)?;
                    let t = self_.compact(&self_.resolve(&ty));
                    match t {
                        Ty::Tensor { elem, shape } if !shape.is_empty() => {
                            Ok((*elem, shape))
                        }
                        other => Err(InferError::new(
                            format!("BLAS op `{op:?}` expects a tensor argument, got `{other}`"),
                            *span,
                        )),
                    }
                };
                let (mut elem, shape) = pull_tensor(&args[first_tensor], self)?;
                // Every subsequent tensor operand must match the first's shape.
                for (i, a) in args.iter().enumerate() {
                    if i == first_tensor {
                        continue;
                    }
                    let a_is_tensor = match op {
                        BlasOp::Dot => i == 1,
                        BlasOp::Axpy => i == 2,
                        _ => false,
                    };
                    if !a_is_tensor {
                        continue;
                    }
                    let (aelem, ashape) = pull_tensor(a, self)?;
                    if shape != ashape {
                        return Err(InferError::new(
                            format!("BLAS op `{op:?}` shape mismatch: {shape:?} vs {ashape:?}"),
                            *span,
                        ));
                    }
                    self.unify(&elem, &aelem, *span, "element types of BLAS tensor operands")?;
                }
                let elem = self.compact(&self.resolve(&elem));
                match op {
                    // Dot / Nrm2 / Asum → scalar.
                    BlasOp::Dot | BlasOp::Nrm2 | BlasOp::Asum => {
                        if *op == BlasOp::Nrm2 && !matches!(elem, Ty::F32 | Ty::F64) {
                            return Err(InferError::new(
                                format!(
                                    "`blas_nrm2` requires a float element type, got `{elem}`"
                                ),
                                *span,
                            ));
                        }
                        Ok(elem)
                    }
                    // Amax → index (i64).
                    BlasOp::Amax => Ok(Ty::I64),
                    // Scal / Axpy → tensor of the same shape.
                    BlasOp::Scal | BlasOp::Axpy => Ok(Ty::Tensor {
                        elem: Box::new(elem),
                        shape,
                    }),
                }
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
                    if let Some(elem_ty) = self.empty_array_elem_ty.clone() {
                        return Ok(Ty::Array(Box::new(elem_ty), 0));
                    }
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
                    // v[i]: a Vec index yields its element type
                    Ty::Vec(elem) => Ok((**elem).clone()),
                    // s[i]: a string index yields the byte value at position i
                    Ty::Str => Ok(Ty::I64),
                    // s[i]: indexing a String yields the byte value at position i
                    Ty::String => Ok(Ty::I64),
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
                self.require_numeric(&ty, *span, "unary minus operand")?;
                Ok(ty)
            }
            HirExpr::Binary { op, lhs, rhs, span } => {
                let l = self.infer_expr(lhs)?;
                let r = self.infer_expr(rhs)?;
                let lc = self.compact(&self.resolve(&l));
                // Bitwise/shift operators are integer-only: no float path, no
                // operator overloading, no string concatenation.
                if op.is_bitwise() {
                    self.unify(&l, &r, *span, "the two bitwise operands")?;
                    self.require_int(&l, *span, "bitwise operand")?;
                    return Ok(l);
                }
                // Operator overloading: a non-numeric user type resolves arithmetic
                // through the `Add` trait (`a + b` -> `Add::add(a, b)`).
                if lc.is_named_type() {
                    return self.binop_through_trait(&l, &r, *op, *span);
                }
                // String concatenation check (existing): only `+` is supported
                if lc == Ty::Str {
                    self.unify(&l, &r, *span, "the two arithmetic operands")?;
                    if *op == BinOp::Add {
                        return Ok(Ty::Str);
                    }
                    return Err(InferError::new(
                        "strings only support concatenation with `+`",
                        *span,
                    ));
                }
                // Float arithmetic: if either side is float, unify as float
                let rc = self.compact(&self.resolve(&r));
                if lc.is_float() || rc.is_float() {
                    self.unify(&l, &r, *span, "the two arithmetic operands")?;
                    self.require_float(&l, *span, "arithmetic operand")?;
                    return Ok(l);
                }
                // Integer arithmetic (existing path)
                self.unify(&l, &r, *span, "the two arithmetic operands")?;
                self.require_int(&l, *span, "arithmetic operand")?;
                Ok(l)
            }
            HirExpr::Cmp { op, lhs, rhs, span } => {
                let l = self.infer_expr(lhs)?;
                let r = self.infer_expr(rhs)?;
                let lc = self.compact(&self.resolve(&l));
                // Operator overloading: a non-numeric user type resolves comparisons
                // through the `Eq`/`Ord` traits (`==`/`!=` via `eq`, the rest via `lt`).
                if lc.is_named_type() {
                    return self.cmp_through_trait(*op, &l, &r, *span);
                }
                let rc = self.compact(&self.resolve(&r));
                let l_is_ptr = matches!(lc, Ty::Ptr(_));
                let r_is_ptr = matches!(rc, Ty::Ptr(_));
                if l_is_ptr || r_is_ptr {
                    // Raw pointer comparisons. `p == 0` / `0 != p` are null checks
                    // (the integer literal is treated as NULL), `p == q` is pointer
                    // equality. Only `==` / `!=` are meaningful for pointers.
                    if matches!(*op, CmpOp::Eq | CmpOp::Ne) {
                        let other_is_int_lit = if l_is_ptr {
                            matches!(rhs.as_ref(), HirExpr::IntLit(..))
                        } else {
                            matches!(lhs.as_ref(), HirExpr::IntLit(..))
                        };
                        if l_is_ptr && r_is_ptr {
                            return Ok(Ty::Bool);
                        }
                        if !other_is_int_lit {
                            return Err(InferError::new(
                                "pointer comparison requires the other operand to be an integer literal (NULL check)",
                                *span,
                            ));
                        }
                        return Ok(Ty::Bool);
                    }
                    return Err(InferError::new(
                        "only `==` and `!=` are supported for pointer comparisons",
                        *span,
                    ));
                }
                self.unify(&l, &r, *span, "the two comparison operands")?;
                let lc = self.compact(&self.resolve(&l));
                if lc == Ty::Str {
                    // String comparison: all six operators are supported via strcmp
                    return Ok(Ty::Bool);
                }
                // Allow both int and float (and char) comparisons
                self.require_numeric(&l, *span, "comparison operand")?;
                Ok(Ty::Bool)
            }
            HirExpr::Logic { lhs, rhs, span, .. } => {
                let l = self.infer_expr(lhs)?;
                let r = self.infer_expr(rhs)?;
                self.require_bool(&l, *span, "logic operand")?;
                self.require_bool(&r, *span, "logic operand")?;
                Ok(Ty::Bool)
            }
            HirExpr::FnRef { def_id, span } => {
                // A first-class function reference: its type is the function's
                // signature `Ty::Fn(params, ret)`.
                let f = self
                    .funcs
                    .get(*def_id as usize)
                    .ok_or_else(|| InferError::new("internal error: function table missing", *span))?;
                if f.type_params.is_empty() {
                    let params = f.params.iter().map(|(_, t, _)| t.clone()).collect();
                    let ret = f.ret.clone().unwrap_or(Ty::Void);
                    Ok(Ty::Fn(params, Box::new(ret)))
                } else {
                    Err(InferError::new(
                        format!(
                            "generic function `{}` cannot be used as a value (function pointers require a concrete signature); call it or use a non-generic wrapper",
                            f.name
                        ),
                        *span,
                    ))
                }
            }
            HirExpr::CallPtr {
                callee,
                args,
                span,
            } => {
                // Indirect call through a function pointer: the callee must be a
                // `Ty::Fn(params, ret)`; unify the args with `params` and return `ret`.
                let callee_ty = self.infer_expr(callee)?;
                let callee_ty = self.compact(&self.resolve(&callee_ty));
                let (params, ret) = match &callee_ty {
                    Ty::Fn(params, ret) => (params.clone(), (**ret).clone()),
                    // Polymorphic higher-order parameter placeholder: accept any
                    // argument signature; return type is a fresh variable.
                    Ty::Var(_) | Ty::Generic(_) => {
                        let mut p = Vec::new();
                        for _a in args { p.push(self.fresh_var()); }
                        (p, self.fresh_var())
                    }
                    other => {
                        return Err(InferError::new(
                            format!("cannot call a value of type `{other}` (expected a function pointer)"),
                            *span,
                        ))
                    }
                };
                if params.len() != args.len() {
                    return Err(InferError::new(
                        format!(
                            "function pointer takes {} arguments, but {} were passed",
                            params.len(),
                            args.len()
                        ),
                        *span,
                    ));
                }
                for (i, a) in args.iter().enumerate() {
                    let aty = self.infer_expr(a)?;
                    self.unify_arg(&params[i], &aty, a.span(), &format!("argument {} of indirect call", i + 1))?;
                }
                Ok(ret)
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
                // `format` is variadic (printf-style): `format(fmt, args...)` with
                // a `str` format string first, or `format(value)` auto-formatted by
                // type. Returns a freshly allocated `str`.
                if f.builtin && f.name == "format" {
                    if args.is_empty() {
                        return Err(InferError::new(
                            "`format` requires at least one argument",
                            *span,
                        ));
                    }
                    if args.len() == 1 {
                        let t = self.infer_expr(&args[0])?;
                        if t == Ty::Void {
                            return Err(InferError::new(
                                "`format` cannot format a void expression",
                                args[0].span(),
                            ));
                        }
                        return Ok(Ty::Str);
                    }
                    let t0 = self.infer_expr(&args[0])?;
                    self.unify(&Ty::Str, &t0, args[0].span(), "format string")?;
                    for a in &args[1..] {
                        let t = self.infer_expr(a)?;
                        if t == Ty::Void {
                            return Err(InferError::new(
                                "`format` cannot format a void expression",
                                a.span(),
                            ));
                        }
                    }
                    return Ok(Ty::Str);
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
                        self.unify_arg(
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
                        self.unify_arg(
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
                    // Validate trait bounds: each (type_param, trait) must be satisfied
                    // by the instantiated concrete type (the type must impl the trait).
                    for (tp, tn) in &f.trait_bounds {
                        let idx = f
                            .type_params
                            .iter()
                            .position(|n| n == tp)
                            .ok_or_else(|| InferError::new("internal error: bound on unknown type parameter", *span))?;
                        let concrete = &type_args[idx];
                        let ok = self.type_impls_trait(concrete, tn);
                        if !ok {
                            return Err(InferError::new(
                                format!(
                                    "type `{concrete}` does not implement trait `{tn}` (required by the bound on `{tp}` of function `{}`)",
                                    f.name
                                ),
                                *span,
                            ));
                        }
                    }
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
            HirExpr::StructLit { name, fields, span } => {
                // Union literal: `U { field: value }` — exactly one field may be set,
                // and the union's value is the field (all fields share storage).
                if self.unions.iter().any(|u| u.name == *name) {
                    let def = self
                        .unions
                        .iter()
                        .find(|u| u.name == *name)
                        .expect("already checked");
                    if fields.len() != 1 {
                        return Err(InferError::new(
                            format!(
                                "union `{name}` literal must set exactly one field, but {} were given",
                                fields.len()
                            ),
                            *span,
                        ));
                    }
                    let (fname, fval) = &fields[0];
                    let expected = match def.find_field(fname) {
                        Some((_, t)) => t.clone(),
                        None => {
                            return Err(InferError::new(
                                format!("union `{name}` has no field `{fname}`"),
                                *span,
                            ));
                        }
                    };
                    let val_ty = self.infer_expr(fval)?;
                    self.unify(
                        &expected,
                        &val_ty,
                        *span,
                        &format!("field `{fname}` of union `{name}`"),
                    )?;
                    return Ok(Ty::Union(name.clone()));
                }
                // Look up the struct definition and clone the field list / type params to
                // avoid holding an immutable borrow across the mutable infer/unify calls.
                let (type_params, def_fields): (Vec<String>, Vec<(String, Ty)>) =
                    match self.lookup_struct(name, *span)? {
                        Some(d) => (d.type_params.clone(), d.fields.clone()),
                        None => {
                            return Err(InferError::new(
                                format!("undefined struct `{name}`"),
                                *span,
                            ));
                        }
                    };
                // Check all fields are present
                if fields.len() != def_fields.len() {
                    return Err(InferError::new(
                        format!(
                            "struct `{name}` has {} fields, but {} were given in the literal",
                            def_fields.len(),
                            fields.len()
                        ),
                        *span,
                    ));
                }
                if type_params.is_empty() {
                    // Non-generic struct: every field is checked against its declared type.
                    for (fname, fval) in fields {
                        let expected = match def_fields.iter().find(|(n, _)| n == fname) {
                            Some((_, t)) => t.clone(),
                            None => {
                                return Err(InferError::new(
                                    format!("struct `{name}` has no field `{fname}`"),
                                    *span,
                                ));
                            }
                        };
                        let val_ty = self.infer_expr(fval)?;
                        self.unify(
                            &expected,
                            &val_ty,
                            *span,
                            &format!("field `{fname}` of struct `{name}`"),
                        )?;
                    }
                    Ok(Ty::Struct(name.clone()))
                } else {
                    // Generic struct literal: bind each type parameter to a fresh variable,
                    // unify the fields against the substituted field types, then record the
                    // concrete type arguments (resolved in a final pass so a surrounding
                    // type annotation can still drive e.g. `let v: Box<i32> = Box { .. }`).
                    let subst: HashMap<String, Ty> = type_params
                        .iter()
                        .map(|n| (n.clone(), self.fresh_var()))
                        .collect();
                    for (fname, fval) in fields {
                        let expected = match def_fields.iter().find(|(n, _)| n == fname) {
                            Some((_, t)) => substitute(t, &subst),
                            None => {
                                return Err(InferError::new(
                                    format!("struct `{name}` has no field `{fname}`"),
                                    *span,
                                ));
                            }
                        };
                        let val_ty = self.infer_expr(fval)?;
                        self.unify(
                            &expected,
                            &val_ty,
                            *span,
                            &format!("field `{fname}` of struct `{name}`"),
                        )?;
                    }
                    let args: Vec<Ty> = type_params
                        .iter()
                        .map(|n| subst.get(n).expect("subst covers all type params").clone())
                        .collect();
                    self.struct_lit_types.insert(span.start, args.clone());
                    Ok(Ty::StructGeneric {
                        name: name.clone(),
                        args,
                    })
                }
            }
            HirExpr::EnumLit {
                name,
                variant,
                arg,
                span,
            } => {
                // Native `Vec<T>` construction: `Vec::new` / `Vec::new()` (empty) and
                // `Vec::with_cap(n)` (pre-allocated capacity). The element type is a fresh
                // variable unified with any surrounding type annotation (`let v: Vec<i64> = ...`),
                // then recorded for codegen (mirroring generic enum literals).
                if name == "Vec" {
                    let elem = self.fresh_var();
                    match variant.as_str() {
                        "new" => {
                            if arg.is_some() {
                                return Err(InferError::new(
                                    "`Vec::new` takes no arguments",
                                    *span,
                                ));
                            }
                        }
                        "with_cap" => {
                            if let Some(a) = arg {
                                let a_ty = self.infer_expr(a)?;
                                self.require_int(&a_ty, *span, "`Vec::with_cap` capacity")?;
                            } else {
                                return Err(InferError::new(
                                    "`Vec::with_cap` requires a capacity argument",
                                    *span,
                                ));
                            }
                        }
                        other => {
                            return Err(InferError::new(
                                format!("`Vec` has no constructor `{other}` (supported: new/with_cap)"),
                                *span,
                            ));
                        }
                    }
                    self.enum_lit_types.insert(span.start, vec![elem.clone()]);
                    return Ok(Ty::Vec(Box::new(elem)));
                }
                // Native `Box<T>` construction: `Box::new(value)` allocates `value`
                // on the heap and returns a `Box<T>`. The element type is inferred
                // from the argument and recorded for codegen.
                if name == "Box" {
                    if variant != "new" {
                        return Err(InferError::new(
                            format!("`Box` has no constructor `{variant}` (supported: new)"),
                            *span,
                        ));
                    }
                    let inner = match arg {
                        Some(a) => self.infer_expr(a)?,
                        None => {
                            return Err(InferError::new(
                                "`Box::new` requires an argument",
                                *span,
                            ));
                        }
                    };
                    let inner = self.compact(&self.resolve(&inner));
                    return Ok(Ty::Box(Box::new(inner)));
                }
                // Native `String` construction: `String::new` / `String::new()` (empty),
                // `String::with_cap(n)` (pre-allocated capacity), and `String::from(s)`
                // (copy a C string into a managed buffer).
                if name == "String" {
                    match variant.as_str() {
                        "new" => {
                            if arg.is_some() {
                                return Err(InferError::new(
                                    "`String::new` takes no arguments",
                                    *span,
                                ));
                            }
                        }
                        "with_cap" => {
                            if let Some(a) = arg {
                                let a_ty = self.infer_expr(a)?;
                                self.require_int(&a_ty, *span, "`String::with_cap` capacity")?;
                            } else {
                                return Err(InferError::new(
                                    "`String::with_cap` requires a capacity argument",
                                    *span,
                                ));
                            }
                        }
                        "from" => {
                            if let Some(a) = arg {
                                let a_ty = self.infer_expr(a)?;
                                let a_ty = self.compact(&self.resolve(&a_ty));
                                if a_ty != Ty::Str {
                                    return Err(InferError::new(
                                        format!("`String::from` requires a `str` argument, got `{a_ty}`"),
                                        *span,
                                    ));
                                }
                            } else {
                                return Err(InferError::new(
                                    "`String::from` requires a `str` argument",
                                    *span,
                                ));
                            }
                        }
                        other => {
                            return Err(InferError::new(
                                format!("`String` has no constructor `{other}` (supported: new/with_cap/from)"),
                                *span,
                            ));
                        }
                    }
                    return Ok(Ty::String);
                }
                // Lowering validated enum/variant existence; look up the payload type and
                // type parameters to check the constructor argument against them. Both are
                // cloned to avoid holding an immutable borrow across infer/unify calls.
                let (type_params, payload) = {
                    let def = self.lookup_enum(name, *span)?;
                    let payload = match def.find_variant(variant) {
                        Some((_, p)) => p.clone(),
                        None => {
                            return Err(InferError::new(
                                format!("enum `{name}` has no variant `{variant}`"),
                                *span,
                            ));
                        }
                    };
                    (def.type_params.clone(), payload)
                };
                if type_params.is_empty() {
                    // Non-generic enum: check the payload against the variant's declared type.
                    match (arg, payload) {
                        (Some(a), Some(pt)) => {
                            let a_ty = self.infer_expr(a)?;
                            self.unify(
                                &pt,
                                &a_ty,
                                *span,
                                &format!("payload of `{name}::{variant}`"),
                            )?;
                            Ok(Ty::Enum(name.clone()))
                        }
                        (None, None) => Ok(Ty::Enum(name.clone())),
                        (None, Some(_)) => Err(InferError::new(
                            format!("variant `{name}::{variant}` requires a payload argument"),
                            *span,
                        )),
                        (Some(_), None) => Err(InferError::new(
                            format!("variant `{name}::{variant}` takes no payload"),
                            *span,
                        )),
                    }
                } else {
                    // Generic enum literal: bind each type parameter to a fresh variable,
                    // unify the payload (if any) with the substituted payload type, then
                    // record the concrete type arguments (resolved in a final pass).
                    let subst: HashMap<String, Ty> = type_params
                        .iter()
                        .map(|n| (n.clone(), self.fresh_var()))
                        .collect();
                    match (arg, payload) {
                        (Some(a), Some(pt)) => {
                            let a_ty = self.infer_expr(a)?;
                            self.unify(
                                &substitute(&pt, &subst),
                                &a_ty,
                                *span,
                                &format!("payload of `{name}::{variant}`"),
                            )?;
                        }
                        (None, None) => {}
                        (None, Some(_)) => {
                            return Err(InferError::new(
                                format!("variant `{name}::{variant}` requires a payload argument"),
                                *span,
                            ));
                        }
                        (Some(_), None) => {
                            return Err(InferError::new(
                                format!("variant `{name}::{variant}` takes no payload"),
                                *span,
                            ));
                        }
                    }
                    let args: Vec<Ty> = type_params
                        .iter()
                        .map(|n| subst.get(n).expect("subst covers all type params").clone())
                        .collect();
                    self.enum_lit_types.insert(span.start, args.clone());
                    Ok(Ty::EnumGeneric { name: name.clone(), args })
                }
            }
            HirExpr::Field { target, field, span } => {
                let recv_ty = self.infer_expr(target)?;
                let recv_ty = self.compact(&self.resolve(&recv_ty));
                // Auto-deref: `recv.field` where recv is `&T` / `&mut T` accesses `(*recv).field`
                let recv_ty = match &recv_ty {
                    Ty::Ref { inner, .. } => (**inner).clone(),
                    other => other.clone(),
                };
                match &recv_ty {
                    Ty::Struct(name) => {
                        match self.lookup_struct_field(name, field, *span)? {
                            Some(t) => Ok(t.clone()),
                            None => Err(InferError::new(
                                format!("struct `{name}` has no field `{field}`"),
                                *span,
                            )),
                        }
                    }
                    Ty::Union(name) => {
                        let def = self
                            .unions
                            .iter()
                            .find(|u| u.name == *name)
                            .ok_or_else(|| InferError::new(format!("undefined union `{name}`"), *span))?;
                        match def.find_field(field) {
                            Some((_, t)) => Ok(t.clone()),
                            None => Err(InferError::new(
                                format!("union `{name}` has no field `{field}`"),
                                *span,
                            )),
                        }
                    }
                    Ty::StructGeneric { name, args } => {
                        let def = match self.lookup_struct(name, *span)? {
                            Some(d) => d,
                            None => {
                                return Err(InferError::new(
                                    format!("undefined struct `{name}`"),
                                    *span,
                                ));
                            }
                        };
                        // Substitute the receiver's concrete type args into the field type
                        // (e.g. `box.value` where `box: Box<i64>` and `Box<T>.value: T` → i64).
                        let subst: HashMap<String, Ty> = def
                            .type_params
                            .iter()
                            .cloned()
                            .zip(args.iter().cloned())
                            .collect();
                        let fty = match def.find_field(field) {
                            Some((_, t)) => t.clone(),
                            None => {
                                return Err(InferError::new(
                                    format!("struct `{name}` has no field `{field}`"),
                                    *span,
                                ));
                            }
                        };
                        Ok(substitute(&fty, &subst))
                    }
                    other => Err(InferError::new(
                        format!("cannot access field `.{field}` on type `{other}` (not a struct)"),
                        *span,
                    )),
                }
            }
            HirExpr::Cast { target, ty, span } => {
                // `expr as dyn Trait`: the target's concrete type must implement the trait.
                let target_ty = self.infer_expr(target)?;
                let target_ty = self.compact(&self.resolve(&target_ty));
                let Ty::Dyn { trait_name } = ty else {
                    return Err(InferError::new(
                        format!(
                            "`as` only casts to a `dyn Trait` type for now, got `{ty}`"
                        ),
                        *span,
                    ));
                };
                // The vtable is built at compile time from a concrete impl, so the
                // trait must be non-generic (no type params to instantiate).
                let trait_def = self.traits.iter().find(|t| t.name == *trait_name).ok_or_else(|| {
                    InferError::new(
                        format!("`dyn {trait_name}`: trait not found in trait table"),
                        *span,
                    )
                })?;
                if !trait_def.type_params.is_empty() {
                    return Err(InferError::new(
                        format!(
                            "`dyn {trait_name}` is not supported yet: generic traits cannot be used as trait objects in this phase"
                        ),
                        *span,
                    ));
                }
                if !self.type_impls_trait(&target_ty, trait_name) {
                    return Err(InferError::new(
                        format!(
                            "type `{target_ty}` does not implement trait `{trait_name}`, so it cannot be cast to `dyn {trait_name}`"
                        ),
                        *span,
                    ));
                }
                // The vtable is built from the concrete type's impl at compile time;
                // generic concrete types (e.g. `Box<i64>`) are not supported yet.
                if !matches!(target_ty, Ty::Struct(_) | Ty::Enum(_)) {
                    return Err(InferError::new(
                        format!(
                            "`dyn {trait_name}` boxing is not supported for generic or non-nominal type `{target_ty}` in this phase"
                        ),
                        *span,
                    ));
                }
                // Boxing `expr as dyn Trait` copies the target onto the heap; with a
                // single (non-Drop) concrete payload this requires a `Copy` target.
                if !crate::borrowck::is_copy(&target_ty, self.program) {
                    return Err(InferError::new(
                        format!(
                            "cannot cast a non-`Copy` value of type `{target_ty}` to `dyn {trait_name}` (boxing requires a `Copy` payload in this phase)"
                        ),
                        *span,
                    ));
                }
                Ok(ty.clone())
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

    /// Create a fresh float-family type variable (for float literals).
    fn fresh_float_var(&mut self) -> Ty {
        let v = TypeVar(self.next_var);
        self.next_var += 1;
        self.float_vars.insert(v);
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

    /// Default remaining type variables to `i64` (or `f64` for unconstrained
    /// float literals) (recursive).
    fn compact(&self, ty: &Ty) -> Ty {
        match ty {
            // Resolve variables along the chain first: bound ones default to their concrete type,
            // still-unconstrained ones default to `i64` (integer family) or `f64` (float family).
            Ty::Var(_) => {
                let r = self.resolve(ty);
                if matches!(r, Ty::Var(_)) {
                    if let Ty::Var(v) = r {
                        if self.float_vars.contains(&v) {
                            Ty::F64
                        } else {
                            Ty::I64
                        }
                    } else {
                        Ty::I64
                    }
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
            Ty::Ref { mut_, lifetime, inner } => Ty::Ref {
                mut_: *mut_,
                lifetime: lifetime.clone(),
                inner: Box::new(self.compact(inner)),
            },
            Ty::Ptr(inner) => Ty::Ptr(Box::new(self.compact(inner))),
            Ty::Fn(params, ret) => Ty::Fn(
                params.iter().map(|t| self.compact(t)).collect(),
                Box::new(self.compact(ret)),
            ),
            Ty::StructGeneric { name, args } => Ty::StructGeneric {
                name: name.clone(),
                args: args.iter().map(|t| self.compact(t)).collect(),
            },
            Ty::EnumGeneric { name, args } => Ty::EnumGeneric {
                name: name.clone(),
                args: args.iter().map(|t| self.compact(t)).collect(),
            },
            Ty::Vec(elem) => Ty::Vec(Box::new(self.compact(elem))),
            Ty::Box(inner) => Ty::Box(Box::new(self.compact(inner))),
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
            // Magic higher-order placeholder `__aero_fn__` accepts any concrete type
            // (typically a `Ty::Fn(params, ret)` from a user function reference).
            (Ty::Generic(x), _) if x == "__aero_fn__" => Ok(a),
            (_, Ty::Generic(x)) if x == "__aero_fn__" => Ok(e),
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
                // Float variables bind only to the float family (no silent `3.14 + 1`-style mix)
                if self.float_vars.contains(v) && !a.is_float() {
                    return Err(InferError::new(
                        format!(
                            "type mismatch: {context} expected float, got `{a}`"
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
                if self.float_vars.contains(v) && !e.is_float() {
                    return Err(InferError::new(
                        format!(
                            "type mismatch: {context} expected float, got `{e}`"
                        ),
                        span,
                    ));
                }
                self.subs.insert(*v, e.clone());
                Ok(e)
            }
            (Ty::I32, Ty::I32)
            | (Ty::I64, Ty::I64)
            | (Ty::F32, Ty::F32)
            | (Ty::F64, Ty::F64)
            | (Ty::Char, Ty::Char)
            | (Ty::Bool, Ty::Bool)
            | (Ty::Str, Ty::Str)
            | (Ty::String, Ty::String)
            | (Ty::Void, Ty::Void) => Ok(e),
            // Two named structs unify iff they have the same name
            (Ty::Struct(a), Ty::Struct(b)) if a == b => Ok(e),
            // Two named unions unify iff they have the same name
            (Ty::Union(a), Ty::Union(b)) if a == b => Ok(e),
            // Two named enums unify iff they have the same name
            (Ty::Enum(a), Ty::Enum(b)) if a == b => Ok(e),
            // Two generic struct instances unify iff same name and each arg unifies
            (Ty::StructGeneric { name: n1, args: a1 }, Ty::StructGeneric { name: n2, args: a2 })
                if n1 == n2 && a1.len() == a2.len() =>
            {
                for (x, y) in a1.iter().zip(a2.iter()) {
                    self.unify(x, y, span, context)?;
                }
                Ok(e)
            }
            // Two generic enum instances unify iff same name and each arg unifies
            (Ty::EnumGeneric { name: n1, args: a1 }, Ty::EnumGeneric { name: n2, args: a2 })
                if n1 == n2 && a1.len() == a2.len() =>
            {
                for (x, y) in a1.iter().zip(a2.iter()) {
                    self.unify(x, y, span, context)?;
                }
                Ok(e)
            }
            // Two native Vecs unify iff their element types unify
            (Ty::Vec(a), Ty::Vec(b)) => {
                self.unify(a, b, span, context)?;
                Ok(e)
            }
            // Two native Boxes unify iff their inner types unify
            (Ty::Box(a), Ty::Box(b)) => {
                self.unify(a, b, span, context)?;
                Ok(e)
            }
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
            (Ty::Ref { mut_: m1, inner: i1, .. }, Ty::Ref { mut_: m2, inner: i2, .. })
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

    /// Receiver unification for method calls, with auto-referencing: when a method's
    /// receiver parameter is `&T` / `&mut T` (e.g. `fn next(it: &mut Self)`), the
    /// receiver expression `recv.method()` is treated as `method(&recv)` — the inner
    /// type `T` is unified against the receiver instead of the reference.
    fn unify_receiver(
        &mut self,
        param_ty: &Ty,
        recv_ty: &Ty,
        span: Span,
        context: &str,
    ) -> Result<(), InferError> {
        if let Ty::Ref { inner, .. } = param_ty {
            self.unify(inner, recv_ty, span, context).map(|_| ())
        } else {
            self.unify(param_ty, recv_ty, span, context).map(|_| ())
        }
    }

    /// Argument unification for function calls, with auto-borrowing: when a
    /// parameter is `&T` / `&mut T` (e.g. `fn sort(v: &mut Vec<i64>)`), the value
    /// argument `sort(v)` is treated as `sort(&v)` — the inner type `T` is unified
    /// against the argument instead of the reference.
    fn unify_arg(
        &mut self,
        param_ty: &Ty,
        arg_ty: &Ty,
        span: Span,
        context: &str,
    ) -> Result<(), InferError> {
        // Resolve the argument's type but do NOT `compact` (default) it here:
        // compacting an unconstrained integer literal to i64 would lock it in
        // before unifying with the parameter, so `f(5)` could never satisfy an
        // `i32` parameter. `unify` binds the still-fresh int var to the param's
        // concrete type instead.
        let arg_ty = self.resolve(arg_ty);
        // If the argument is already a reference (e.g. a `&mut` parameter forwarded
        // on), unify the references directly so reference param chains keep working.
        let param_ref = matches!(param_ty, Ty::Ref { .. } | Ty::Ptr(_));
        let arg_ref = matches!(arg_ty, Ty::Ref { .. } | Ty::Ptr(_));
        if param_ref {
            if arg_ref {
                self.unify(param_ty, &arg_ty, span, context).map(|_| ())
            } else {
                // A raw-pointer parameter accepts an integer argument: this is the
                // C-FFI NULL/reinterpret case (`sqlite3_exec(db, sql, 0, 0, 0)`).
                // The integer may still be an unbound literal var, so accept both
                // concrete integers and int-typed vars. References (`&T`) do not
                // allow it — only raw pointers do.
                if matches!(param_ty, Ty::Ptr(_)) {
                    let int_like = matches!(arg_ty, Ty::I32 | Ty::I64)
                        || matches!(&arg_ty, Ty::Var(v) if self.int_vars.contains(v));
                    if int_like {
                        return Ok(());
                    }
                    // C-style array decay: a raw-pointer parameter `*T` accepts an
                    // array `[T; N]` argument (e.g. `regcomp(preg, ...)` where
                    // `preg` is `[i32; 16]` and the param is `*i32`).
                    if let Ty::Array(elem, _) = &arg_ty {
                        if let Ty::Ptr(inner) = param_ty {
                            return self.unify(inner, elem, span, context).map(|_| ());
                        }
                    }
                    // A `str` is a NUL-terminated C string pointer, so it can be
                    // passed as an opaque `*T` parameter (e.g. the variadic
                    // `curl_easy_setopt(h, CURLOPT_URL, url)` third argument).
                    if matches!(arg_ty, Ty::Str) {
                        return Ok(());
                    }
                }
                // Auto-borrow: a value argument `f(v)` unifies against the inner type.
                let inner = match param_ty {
                    Ty::Ref { inner, .. } | Ty::Ptr(inner) => inner,
                    _ => param_ty,
                };
                self.unify(inner, &arg_ty, span, context).map(|_| ())
            }
        } else {
            self.unify(param_ty, &arg_ty, span, context).map(|_| ())
        }
    }

    /// Resolve the element type of a user-defined iterable via the `IntoIterator` /
    /// `Iterator` protocol: `iter.into_iter()` yields an iterator `It`, and
    /// `It.next()` returns `Option<Item>`; the loop variable's type is `Item`.
    fn iter_protocol_item(&self, iter_ty: &Ty, span: Span) -> Result<Ty, InferError> {
        // `iter.into_iter()` → `It` (the `IntoIter` associated type)
        let it_ty = self.method_ret_ty(iter_ty, "into_iter", span)?;
        // `<It>::next()` → `Option<Item>`
        let next_ret = self.method_ret_ty(&it_ty, "next", span)?;
        let next_ret = self.compact(&self.resolve(&next_ret));
        let item_ty = match &next_ret {
            Ty::EnumGeneric { name, args } if name == "Option" => args
                .first()
                .cloned()
                .ok_or_else(|| InferError::new("`Option` requires one type argument", span))?,
            other => {
                return Err(InferError::new(
                    format!("iterator's `next` must return `Option<Item>`, got `{other}`"),
                    span,
                ))
            }
        };
        Ok(item_ty)
    }

    /// Resolve the return type of `recv_ty.method(...)` via the method table, without
    /// type-checking the arguments. Generic methods (from `impl<T> Type<T>`) have the
    /// impl's type params substituted by the receiver's type arguments (aligned 1:1).
    /// Used by `for`-loop `IntoIterator` protocol lowering.
    fn method_ret_ty(&self, recv_ty: &Ty, method: &str, span: Span) -> Result<Ty, InferError> {
        let type_name = match recv_ty {
            Ty::Struct(n) | Ty::Union(n) | Ty::Enum(n) => n.clone(),
            Ty::StructGeneric { name, .. } | Ty::EnumGeneric { name, .. } => name.clone(),
            other => {
                return Err(InferError::new(
                    format!(
                        "type `{other}` has no method `{method}` (iteration requires a type implementing `IntoIterator`)"
                    ),
                    span,
                ))
            }
        };
        let def_id = self
            .method_map
            .get(&(type_name.clone(), method.to_string()))
            .copied()
            .ok_or_else(|| {
                InferError::new(
                    format!(
                        "type `{type_name}` has no method `{method}` (iteration requires `IntoIterator`/`Iterator` impls)"
                    ),
                    span,
                )
            })?;
        let f = self
            .funcs
            .get(def_id as usize)
            .ok_or_else(|| InferError::new("internal error: method function table missing", span))?;
        if f.type_params.is_empty() {
            return Ok(f.ret.clone().unwrap_or(Ty::Void));
        }
        let recv_args = match recv_ty {
            Ty::StructGeneric { args, .. } | Ty::EnumGeneric { args, .. } => args.clone(),
            _ => {
                return Err(InferError::new(
                    format!(
                        "generic method `{method}` on `{type_name}` requires a generic receiver (e.g. `{type_name}<i64>`); got `{recv_ty}`"
                    ),
                    span,
                ))
            }
        };
        if f.type_params.len() != recv_args.len() {
            return Err(InferError::new(
                format!(
                    "internal error: generic method `{method}` parameter count mismatch (declared {}, receiver has {})",
                    f.type_params.len(),
                    recv_args.len()
                ),
                span,
            ));
        }
        let subst: HashMap<String, Ty> = f
            .type_params
            .iter()
            .cloned()
            .zip(recv_args.iter().cloned())
            .collect();
        Ok(f.ret
            .as_ref()
            .map(|t| substitute(t, &subst))
            .unwrap_or(Ty::Void))
    }

    /// Resolve an operator-overload trait method (`add`/`eq`/`lt`) for the left
    /// operand's type, returning `(type_name, lhs_param_ty, rhs_param_ty, ret_ty)`.
    /// Generic operator impls (`impl<T> Add<..> for Type<T>`) are instantiated with
    /// the receiver's concrete type arguments (like ordinary method calls).
    fn op_method_parts(
        &self,
        lhs: &Ty,
        method: &str,
        op_symbol: &str,
        trait_hint: &str,
        span: Span,
    ) -> Result<(String, Ty, Ty, Ty), InferError> {
        let type_name = match self.compact(&self.resolve(lhs)) {
            Ty::Struct(n) | Ty::Union(n) | Ty::Enum(n) => n.clone(),
            Ty::StructGeneric { name, .. } | Ty::EnumGeneric { name, .. } => name.clone(),
            other => {
                return Err(InferError::new(
                    format!(
                        "operator `{op_symbol}` is not supported for type `{other}` ({trait_hint})"
                    ),
                    span,
                ))
            }
        };
        let def_id = self
            .method_map
            .get(&(type_name.clone(), method.to_string()))
            .copied()
            .ok_or_else(|| {
                InferError::new(
                    format!(
                        "type `{type_name}` does not implement operator `{op_symbol}` (missing trait method `{method}`; {trait_hint})"
                    ),
                    span,
                )
            })?;
        let f = self
            .funcs
            .get(def_id as usize)
            .ok_or_else(|| InferError::new("internal error: method function table missing", span))?;
        if f.params.len() != 2 {
            return Err(InferError::new(
                format!(
                    "operator method `{method}` on `{type_name}` must take exactly 2 parameters (lhs, rhs)"
                ),
                span,
            ));
        }
        let (lhs_ty, rhs_ty, ret_ty) = if f.type_params.is_empty() {
            (
                f.params[0].1.clone(),
                f.params[1].1.clone(),
                f.ret.clone().unwrap_or(Ty::Void),
            )
        } else {
            let recv_args = match self.compact(&self.resolve(lhs)) {
                Ty::StructGeneric { args, .. } | Ty::EnumGeneric { args, .. } => args.clone(),
                _ => {
                    return Err(InferError::new(
                        format!(
                            "generic operator method `{method}` on `{type_name}` requires a generic receiver"
                        ),
                        span,
                    ))
                }
            };
            if recv_args.len() != f.type_params.len() {
                return Err(InferError::new(
                    format!(
                        "internal error: operator method `{method}` parameter count mismatch (declared {}, receiver has {})",
                        f.type_params.len(),
                        recv_args.len()
                    ),
                    span,
                ));
            }
            let subst: HashMap<String, Ty> = f
                .type_params
                .iter()
                .cloned()
                .zip(recv_args.into_iter())
                .collect();
            (
                substitute(&f.params[0].1, &subst),
                substitute(&f.params[1].1, &subst),
                substitute(&f.ret.clone().unwrap_or(Ty::Void), &subst),
            )
        };
        Ok((type_name, lhs_ty, rhs_ty, ret_ty))
    }

    /// Operator overloading for arithmetic: `a op b` on a non-numeric user type
    /// resolves to the trait method (`+`/`-`/`*`/`/` -> `add`/`sub`/`mul`/`div`).
    fn binop_through_trait(
        &mut self,
        lhs: &Ty,
        rhs: &Ty,
        op: BinOp,
        span: Span,
    ) -> Result<Ty, InferError> {
        let (method, trait_hint) = match op {
            BinOp::Add => ("add", "implement the `Add` trait to overload it"),
            BinOp::Sub => ("sub", "implement the `Sub` trait to overload it"),
            BinOp::Mul => ("mul", "implement the `Mul` trait to overload it"),
            BinOp::Div => ("div", "implement the `Div` trait to overload it"),
            BinOp::Rem => ("rem", "implement the `Rem` trait to overload it"),
            BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr => {
                return Err(InferError::new(
                    "bitwise operators are only supported on integers",
                    span,
                ));
            }
        };
        let (_, lhs_ty, rhs_ty, ret_ty) =
            self.op_method_parts(lhs, method, op.symbol(), trait_hint, span)?;
        self.unify(&lhs_ty, lhs, span, "operator `lhs`")?;
        self.unify(&rhs_ty, rhs, span, "operator `rhs`")?;
        Ok(ret_ty)
    }

    /// Operator overloading for comparisons: `a == b`/`a != b` bind to `Eq::eq`;
    /// `<`/`>`/`<=`/`>=` bind to `Ord::lt` (codegen derives `!=`/`>`/`<=`/`>=`
    /// from `eq`/`lt` by negation or operand swap).
    fn cmp_through_trait(
        &mut self,
        op: CmpOp,
        lhs: &Ty,
        rhs: &Ty,
        span: Span,
    ) -> Result<Ty, InferError> {
        let (method, trait_hint) = match op {
            CmpOp::Eq | CmpOp::Ne => ("eq", "implement the `Eq` trait to overload it"),
            CmpOp::Lt | CmpOp::Gt | CmpOp::Le | CmpOp::Ge => {
                ("lt", "implement the `Ord` trait to overload it")
            }
        };
        let (_, lhs_ty, rhs_ty, _) = self.op_method_parts(lhs, method, op.symbol(), trait_hint, span)?;
        self.unify(&lhs_ty, lhs, span, "operator `lhs`")?;
        self.unify(&rhs_ty, rhs, span, "operator `rhs`")?;
        Ok(Ty::Bool)
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

    /// String buffers store raw bytes: both integers and `char` may be written.
    fn require_byte(&self, ty: &Ty, span: Span, context: &str) -> Result<(), InferError> {
        match self.resolve(ty) {
            Ty::I32 | Ty::I64 | Ty::Char | Ty::Var(_) => Ok(()),
            Ty::Generic(_) => Ok(()),
            other => Err(InferError::new(
                format!("{context} requires an integer or `char`, got `{other}`"),
                span,
            )),
        }
    }

    fn require_float(&self, ty: &Ty, span: Span, context: &str) -> Result<(), InferError> {
        match self.resolve(ty) {
            Ty::F32 | Ty::F64 | Ty::Var(_) => Ok(()),
            Ty::Generic(_) => Ok(()),
            other => Err(InferError::new(
                format!("{context} requires a float type, got `{other}`"),
                span,
            )),
        }
    }

    fn require_numeric(&self, ty: &Ty, span: Span, context: &str) -> Result<(), InferError> {
        match self.resolve(ty) {
            Ty::I32 | Ty::I64 | Ty::F32 | Ty::F64 | Ty::Char | Ty::Var(_) => Ok(()),
            Ty::Generic(_) => Ok(()),
            other => Err(InferError::new(
                format!("{context} requires a numeric type, got `{other}`"),
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

    /// Look up a struct definition by name.
    fn lookup_struct(&self, name: &str, span: Span) -> Result<Option<&HirStructDef>, InferError> {
        match self.structs.iter().find(|s| s.name == name) {
            Some(d) => Ok(Some(d)),
            None => Err(InferError::new(
                format!("undefined struct `{name}`"),
                span,
            )),
        }
    }

    /// Look up an enum definition by name.
    fn lookup_enum(&self, name: &str, span: Span) -> Result<&HirEnumDef, InferError> {
        self.enums
            .iter()
            .find(|e| e.name == name)
            .ok_or_else(|| InferError::new(format!("undefined enum `{name}`"), span))
    }

    /// Look up a field's type in a struct by name. Returns None if the field
    /// does not exist (caller decides whether that's an error).
    fn lookup_struct_field(
        &self,
        struct_name: &str,
        field: &str,
        span: Span,
    ) -> Result<Option<Ty>, InferError> {
        match self.lookup_struct(struct_name, span)? {
            Some(def) => Ok(def.find_field(field).map(|(_, t)| t.clone())),
            None => Ok(None),
        }
    }

    /// Whether a concrete type implements a trait (searches the impl table).
    fn type_impls_trait(&self, ty: &Ty, trait_name: &str) -> bool {
        let type_name = match ty {
            Ty::Struct(n) | Ty::Union(n) | Ty::Enum(n) => n.as_str(),
            Ty::StructGeneric { name, .. } | Ty::EnumGeneric { name, .. } => name.as_str(),
            // Generic type parameters resolve to their concrete instance before
            // this check runs; anything else cannot implement a trait.
            _ => return false,
        };
        self.impls.iter().any(|imp| {
            imp.trait_name.as_deref() == Some(trait_name) && imp.type_name == type_name
        })
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
        Ty::Assoc(name) => subst
            .get(name)
            .cloned()
            .unwrap_or_else(|| ty.clone()),
        Ty::Tuple(elems) => Ty::Tuple(elems.iter().map(|t| substitute(t, subst)).collect()),
        Ty::Array(elem, n) => Ty::Array(Box::new(substitute(elem, subst)), *n),
        Ty::Ref { mut_, lifetime, inner } => Ty::Ref {
            mut_: *mut_,
            lifetime: lifetime.clone(),
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
        Ty::Struct(name) => Ty::Struct(name.clone()),
        Ty::Union(name) => Ty::Union(name.clone()),
        Ty::Enum(name) => Ty::Enum(name.clone()),
        Ty::StructGeneric { name, args } => Ty::StructGeneric {
            name: name.clone(),
            args: args.iter().map(|t| substitute(t, subst)).collect(),
        },
        Ty::EnumGeneric { name, args } => Ty::EnumGeneric {
            name: name.clone(),
            args: args.iter().map(|t| substitute(t, subst)).collect(),
        },
        Ty::Vec(elem) => Ty::Vec(Box::new(substitute(elem, subst))),
        Ty::Box(inner) => Ty::Box(Box::new(substitute(inner, subst))),
        other => other.clone(),
    }
}

/// Whether a type is a scalar that can be a top-level const value
/// (Phase P0-3). Aggregates (structs/unions/enums/strings/arrays) are not
/// supported as const values yet.
pub fn is_scalar_const_ty(ty: &Ty) -> bool {
    matches!(
        ty,
        Ty::I32 | Ty::I64 | Ty::F32 | Ty::F64 | Ty::Bool | Ty::Char
    )
}

/// Whether a type mentions an unresolved generic parameter (`Ty::Generic`), directly or
/// nested. Used to reject `impl Copy for X<T>` payloads that depend on `T` without a
/// `T: Copy` bound (which the language cannot express yet).
fn contains_generic(ty: &Ty) -> bool {
    match ty {
        Ty::Generic(_) => true,
        Ty::Assoc(_) => true,
        Ty::Tuple(ts) => ts.iter().any(contains_generic),
        Ty::Array(t, _) => contains_generic(t),
        Ty::Ref { inner, .. } => contains_generic(inner),
        Ty::Ptr(inner) => contains_generic(inner),
        Ty::Tensor { elem, .. } => contains_generic(elem),
        Ty::Fn(params, ret) => params.iter().any(contains_generic) || contains_generic(ret),
        Ty::StructGeneric { args, .. } | Ty::EnumGeneric { args, .. } => {
            args.iter().any(contains_generic)
        }
        Ty::Vec(elem) => contains_generic(elem),
        Ty::Box(inner) => contains_generic(inner),
        _ => false,
    }
}
