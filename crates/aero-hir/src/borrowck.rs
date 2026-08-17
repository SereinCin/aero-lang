/// Borrow checker: an independent HIR-level pass (Campaign 2).
///
/// Semantics: Rust-style ownership + borrows + moves. This pass runs after type
/// inference and before codegen, statically checking these memory-safety issues:
/// 1. **Mutable-borrow exclusivity**: at most one active mutable borrow per source
/// 2. **Immutable/mutable exclusivity**: `&x` and `&mut x` cannot coexist
/// 3. **No writes to the source while borrowed**: assigning/index-writing the source is an
///    error (prevents dangling references)
/// 4. **Move semantics (Campaign 4)**: non-`Copy` values are *moved* (transferred, not
///    copied) when consumed — bound by `let`, assigned, passed by value, stored into a
///    struct/enum/tuple/array, or returned. Reading a moved variable is an error
///    (use-after-move), and moving twice in a row is an error.
///
/// Copy-ness is decided structurally from the type:
/// - Scalars (`i32`/`i64`/`f32`/`f64`/`char`/`bool`), string literals (`str`), references
///   (`&T`), raw pointers (`*T`) and function types are `Copy`.
/// - Heap-owning values (`String`, `Vec<T>`, `arena`) are never `Copy`.
/// - A struct/enum is `Copy` iff every field/variant payload is `Copy`.
/// - Generic parameters have no `Copy` bound, so they are treated as non-`Copy` (moved),
///   matching Rust's default. This is refined in Campaign 5 (Copy/Clone + derive).
///
/// Borrow lifetimes (NLL-lite): bounded by the last use.
/// - **Named borrows** (`let r = &x` / `r = &x`): live until the last use of `r`
/// - **Temporary borrows** (call args, intermediate expressions): live until the end of
///   their statement (or if/while condition)
///
/// Move tracking is sequential (statement order) within each function body. Moves inside
/// nested blocks (if/else/while/for/match arms) are scoped to that block and do not leak
/// outward — this keeps divergent branches (e.g. `match` arms that `return`) compiling,
/// while catching straight-line use-after-move bugs.
use std::collections::{HashMap, HashSet};

use aero_parse::span::Span;

use crate::hir::{DefId, HirBlock, HirExpr, HirImplBlock, HirProgram, HirStmt, ScopeId};
use crate::ty::Ty;

/// Borrow-check error (with line/column).
#[derive(Debug, Clone)]
pub struct BorrowError {
    pub msg: String,
    pub line: u32,
    pub col: u32,
}

impl BorrowError {
    fn new(msg: impl Into<String>, span: Span) -> Self {
        BorrowError {
            msg: msg.into(),
            line: span.line,
            col: span.col,
        }
    }
}

/// An active borrow record.
#[derive(Clone)]
struct LiveBorrow {
    /// DefId of the variable holding the borrow (0 for temporary borrows)
    borrow_def: DefId,
    /// Whether this is a mutable borrow
    mut_: bool,
    /// Temporary borrow (reclaimed at the end of the statement)
    temp: bool,
}

struct Checker<'a> {
    /// The whole program (struct/enum definitions for Copy-ness lookups)
    program: &'a HirProgram,
    /// Variable DefId → resolved type (from type inference)
    var_tys: &'a HashMap<DefId, Ty>,
    /// Variable DefId → defining scope
    var_scopes: HashMap<DefId, ScopeId>,
    /// Active borrows, grouped by source variable
    live: HashMap<DefId, Vec<LiveBorrow>>,
    /// Variables whose (non-`Copy`) value has been moved; reading them is an error.
    /// Cleared at the start of each top-level block (main + each function body).
    moved: HashSet<DefId>,
    /// Per-block-scope snapshot of `moved` at that block's end (for codegen's drop
    /// decisions). Branch bodies save/restore `moved`, so a move inside a branch
    /// only appears in that branch's own scope entry.
    moved_by_scope: HashMap<ScopeId, HashSet<DefId>>,
    /// Whether the current function returns a reference (`&T` / `&mut T`).
    /// When true, `return` values must be references derived from parameters
    /// (Phase 10) — returning a reference to a local is a dangling-reference error.
    cur_return_is_ref: bool,
    /// DefIds of the current function's parameters (reference-return permission set).
    cur_param_defs: HashSet<DefId>,
    /// For local variables that hold a reference, whether that reference ultimately
    /// originates from a function parameter (safe to return) vs a local (dangling).
    /// Propagated through `let` / assignment of reference-typed values.
    ref_origins: HashMap<DefId, bool>,
}

/// Whether `ty` may be copied freely (value semantics) instead of moved.
pub fn is_copy(ty: &Ty, program: &HirProgram) -> bool {
    match ty {
        // Scalars, string literals, references, pointers, function types: Copy
        Ty::I32
        | Ty::I64
        | Ty::F32
        | Ty::F64
        | Ty::Char
        | Ty::Bool
        | Ty::Str
        | Ty::Ref { .. }
        | Ty::Ptr(_)
        | Ty::Fn(..)
        | Ty::Void => true,
        Ty::Tuple(ts) => ts.iter().all(|t| is_copy(t, program)),
        Ty::Array(t, _) => is_copy(t, program),
        Ty::Tensor { elem, .. } => is_copy(elem, program),
        // Heap-owning values can never be copied (only moved or borrowed)
        Ty::String | Ty::Vec(_) | Ty::Box(_) | Ty::Arena(_) => false,
        // `dyn Trait` is a fat pointer `{ data, vtable }` that owns a heap
        // allocation → moved, never copied.
        Ty::Dyn { .. } => false,
        // User-defined types (structs/enums) are `Copy` only when explicitly
        // marked with `impl Copy for X {}` (or `#[derive(Copy)]`). The structural
        // "all fields are copyable" check is NOT enough on its own — a type must
        // opt in explicitly. See `structurally_copyable` below for the shape check
        // used when validating a `Copy` impl.
        Ty::Struct(name) => has_copy_impl(name, &program.impls),
        Ty::StructGeneric { name, .. } => has_copy_impl(name, &program.impls),
        Ty::Enum(name) => has_copy_impl(name, &program.impls),
        Ty::EnumGeneric { name, .. } => has_copy_impl(name, &program.impls),
        // Unions are POD: all fields share storage and no field ever owns heap
        // resources, so a union is always bitwise-copyable.
        Ty::Union(_) => true,
        // Unbound generic parameters / unresolved type variables: no Copy bound → moved
        Ty::Generic(_) | Ty::Assoc(_) | Ty::Var(_) => false,
    }
}

/// Whether a user-defined type has an explicit `impl Copy for X {}` (or was
/// derived with `#[derive(Copy)]`, which lowers to the same empty impl).
fn has_copy_impl(name: &str, impls: &[HirImplBlock]) -> bool {
    impls
        .iter()
        .any(|imp| imp.trait_name.as_deref() == Some("Copy") && imp.type_name == name)
}

/// Structural copyability (used to validate `impl Copy` and `#[derive(Copy)]`):
/// whether every field / variant payload of `ty` may be bitwise-copied without
/// leaking or double-freeing heap memory. This does NOT grant `Copy` semantics —
/// it only decides whether a type is *allowed* to implement `Copy`.
pub fn structurally_copyable(ty: &Ty, impls: &[HirImplBlock]) -> bool {
    match ty {
        Ty::I32
        | Ty::I64
        | Ty::F32
        | Ty::F64
        | Ty::Char
        | Ty::Bool
        | Ty::Str
        | Ty::Ref { .. }
        | Ty::Ptr(_)
        | Ty::Fn(..)
        | Ty::Void => true,
        Ty::Tuple(ts) => ts.iter().all(|t| structurally_copyable(t, impls)),
        Ty::Array(t, _) => structurally_copyable(t, impls),
        Ty::Tensor { elem, .. } => structurally_copyable(elem, impls),
        // Heap-owning values cannot be bitwise-copied
        Ty::String | Ty::Vec(_) | Ty::Box(_) | Ty::Arena(_) => false,
        // `dyn Trait` owns a heap allocation → not bitwise-copyable
        Ty::Dyn { .. } => false,
        // A nested user type is copyable iff it itself explicitly implements Copy
        Ty::Struct(name) | Ty::StructGeneric { name, .. } => has_copy_impl(name, impls),
        Ty::Enum(name) | Ty::EnumGeneric { name, .. } => has_copy_impl(name, impls),
        // Unions are POD (bitwise-copyable regardless of field shapes)
        Ty::Union(_) => true,
        Ty::Generic(_) | Ty::Assoc(_) | Ty::Var(_) => false,
    }
}

/// Entry point: run the borrow + move check over an HIR program.
///
/// Returns the set of variables moved (straight-line) per block scope, keyed by
/// `ScopeId`. Codegen uses this to decide which variables must NOT be dropped
/// (a moved value's ownership has been transferred to its new owner, which drops
/// it instead). Branch-body moves are scoped: they do not leak into the enclosing
/// scope's moved set, matching the checker's save/restore semantics.
pub fn check<'a>(
    program: &'a HirProgram,
    var_tys: &'a HashMap<DefId, Ty>,
) -> Result<HashMap<ScopeId, HashSet<DefId>>, BorrowError> {
    let mut c = Checker {
        program,
        var_tys,
        var_scopes: HashMap::new(),
        live: HashMap::new(),
        moved: HashSet::new(),
        moved_by_scope: HashMap::new(),
        cur_return_is_ref: false,
        cur_param_defs: HashSet::new(),
        ref_origins: HashMap::new(),
    };
    // Function parameters live in their function body's scope; inner borrows cannot escape.
    // (Bodies get a real scope id from lower.rs, distinct from the main scope's 0.)
    for f in &program.funcs {
        for &pd in &f.param_defs {
            c.var_scopes.insert(pd, f.body.scope_id);
        }
    }
    let mut seq = 0usize;
    c.cur_return_is_ref = false;
    c.cur_param_defs.clear();
    c.check_block(&program.main, &mut seq)?;
    for f in &program.funcs {
        // Skip builtin / generic / extern functions: builtins carry a placeholder
        // `scope_id: 0` body (see lower.rs) that would otherwise collide with the
        // main scope and overwrite its recorded moved set; generic/extern bodies are
        // not materialized here (codegen skips them for the same reason).
        if f.builtin || !f.type_params.is_empty() || f.is_extern {
            continue;
        }
        // Each function body is an independent move domain
        c.moved.clear();
        c.ref_origins.clear();
        c.cur_return_is_ref = matches!(f.ret, Some(Ty::Ref { .. }));
        c.cur_param_defs = f.param_defs.iter().copied().collect();
        c.check_block(&f.body, &mut seq)?;
        c.cur_return_is_ref = false;
        c.cur_param_defs.clear();
    }
    Ok(c.moved_by_scope)
}

impl<'a> Checker<'a> {
    /// Check a block. `seq` is the global statement counter (DFS order), increasing
    fn check_block(&mut self, block: &HirBlock, seq: &mut usize) -> Result<(), BorrowError> {
        let base = *seq;
        // across nested blocks.
        let last_use = self.compute_last_use(block, seq);
        let end = *seq;
        // Precompute the last-use position of every variable in this block (incl. nested)
        *seq = base;
        // Rewind to the block start and replay the checks with the same numbering
        for stmt in &block.stmts {
            if let HirStmt::Let { def_id, .. } = stmt {
                self.var_scopes.insert(*def_id, block.scope_id);
            }
        }
        for stmt in &block.stmts {
            let cur = *seq;
            *seq += 1;
            self.check_stmt(stmt, cur, &last_use, seq)?;
            // Register the scopes of this block's lets
            self.clean_temps();
        }
        *seq = end;
        // End of statement: reclaim temporary borrows
        self.live.retain(|_, v| {
            v.retain(|b| {
                b.temp || self.var_scopes.get(&b.borrow_def).copied() != Some(block.scope_id)
            });
            !v.is_empty()
        });
        // Record the moved set at this block's end (codegen consults it for drop).
        self.moved_by_scope
            .insert(block.scope_id, self.moved.clone());
        Ok(())
    }

    // End of block: reclaim named borrows defined in this block
    fn compute_last_use(&self, block: &HirBlock, seq: &mut usize) -> HashMap<DefId, usize> {
        let mut map = HashMap::new();
        for stmt in &block.stmts {
            let i = *seq;
            *seq += 1;
            self.collect_uses_stmt(stmt, i, &mut map, seq);
        }
        map
    }

    fn collect_uses_stmt(
        &self,
        stmt: &HirStmt,
        i: usize,
        map: &mut HashMap<DefId, usize>,
        seq: &mut usize,
    ) {
        match stmt {
            HirStmt::Let { init, .. } => self.collect_uses_expr(init, i, map),
            HirStmt::Assign { value, .. } => self.collect_uses_expr(value, i, map),
            HirStmt::AssignIndex {
                target,
                index,
                value,
                ..
            } => {
                self.collect_uses_expr(target, i, map);
                self.collect_uses_expr(index, i, map);
                self.collect_uses_expr(value, i, map);
            }
            HirStmt::AssignDeref { target, value, .. } => {
                self.collect_uses_expr(target, i, map);
                self.collect_uses_expr(value, i, map);
            }
            HirStmt::AssignField { target, value, .. } => {
                self.collect_uses_expr(target, i, map);
                self.collect_uses_expr(value, i, map);
            }
            HirStmt::Print(args, _) => {
                for a in args {
                    self.collect_uses_expr(a, i, map);
                }
            }
            HirStmt::Expr(expr, _) => self.collect_uses_expr(expr, i, map),
            HirStmt::If {
                cond,
                then_body,
                else_body,
                ..
            } => {
                self.collect_uses_expr(cond, i, map);
                let m1 = self.compute_last_use(then_body, seq);
                let m2 = self.compute_last_use(else_body, seq);
                for (d, idx) in m1.into_iter().chain(m2) {
                    map.entry(d)
                        .and_modify(|e| *e = (*e).max(idx))
                        .or_insert(idx);
                }
            }
            HirStmt::While { cond, body, .. } => {
                self.collect_uses_expr(cond, i, map);
                let m = self.compute_last_use(body, seq);
                for (d, idx) in m {
                    map.entry(d)
                        .and_modify(|e| *e = (*e).max(idx))
                        .or_insert(idx);
                }
            }
            HirStmt::Loop { body, .. } => {
                let m = self.compute_last_use(body, seq);
                for (d, idx) in m {
                    map.entry(d)
                        .and_modify(|e| *e = (*e).max(idx))
                        .or_insert(idx);
                }
            }
            HirStmt::For { iter, body, .. } => {
                self.collect_uses_expr(iter, i, map);
                let m = self.compute_last_use(body, seq);
                for (d, idx) in m {
                    map.entry(d)
                        .and_modify(|e| *e = (*e).max(idx))
                        .or_insert(idx);
                }
            }
            HirStmt::Break(_) | HirStmt::Continue(_) => {}
            HirStmt::Match { scrutinee, arms, .. } => {
                self.collect_uses_expr(scrutinee, i, map);
                for arm in arms {
                    let m = self.compute_last_use(&arm.body, seq);
                    for (d, idx) in m {
                        map.entry(d)
                            .and_modify(|e| *e = (*e).max(idx))
                            .or_insert(idx);
                    }
                }
            }
            HirStmt::Return(value, _) => {
                if let Some(v) = value {
                    self.collect_uses_expr(v, i, map);
                }
            }
            HirStmt::StructDef { .. } => {}
            HirStmt::EnumDef { .. } => {}
            HirStmt::TraitDef { .. } => {}
            HirStmt::ImplBlock { .. } => {}
        }
    }

    fn collect_uses_expr(&self, expr: &HirExpr, i: usize, map: &mut HashMap<DefId, usize>) {
        let mut mark = |d: DefId| {
            map.entry(d)
                .and_modify(|e| *e = (*e).max(i))
                .or_insert(i);
        };
        match expr {
            HirExpr::Var(d, _) => mark(*d),
            // Compute the last-use global id of every variable in a block (incl. nested blocks).
            HirExpr::Borrow { def_id, .. } => mark(*def_id),
            HirExpr::Deref { target, .. } => self.collect_uses_expr(target, i, map),
            HirExpr::MethodCall { recv, args, .. } => {
                self.collect_uses_expr(recv, i, map);
                for a in args {
                    self.collect_uses_expr(a, i, map);
                }
            }
            HirExpr::Matmul { lhs, rhs, .. } => {
                self.collect_uses_expr(lhs, i, map);
                self.collect_uses_expr(rhs, i, map);
            }
            HirExpr::Reduce { input, .. } => self.collect_uses_expr(input, i, map),
            HirExpr::ElemWise { lhs, rhs, .. } => {
                self.collect_uses_expr(lhs, i, map);
                if let Some(rhs) = rhs {
                    self.collect_uses_expr(rhs, i, map);
                }
            }
            HirExpr::Blas { args, .. } => {
                for a in args {
                    self.collect_uses_expr(a, i, map);
                }
            }
            HirExpr::Index {
                target,
                index,
                span: _,
            } => {
                self.collect_uses_expr(target, i, map);
                self.collect_uses_expr(index, i, map);
            }
            HirExpr::Unary { expr, .. } => self.collect_uses_expr(expr, i, map),
            HirExpr::Binary { lhs, rhs, .. }
            | HirExpr::Cmp { lhs, rhs, .. }
            | HirExpr::Logic { lhs, rhs, .. } => {
                self.collect_uses_expr(lhs, i, map);
                self.collect_uses_expr(rhs, i, map);
            }
            HirExpr::Call { args, .. } => {
                for a in args {
                    self.collect_uses_expr(a, i, map);
                }
            }
            HirExpr::FnRef { .. } => {}
            HirExpr::CallPtr { callee, args, .. } => {
                self.collect_uses_expr(callee, i, map);
                for a in args {
                    self.collect_uses_expr(a, i, map);
                }
            }
            HirExpr::Tuple(elems, _) | HirExpr::Array(elems, _) => {
                for e in elems {
                    self.collect_uses_expr(e, i, map);
                }
            }
            HirExpr::StructLit { fields, .. } => {
                for (_, v) in fields {
                    self.collect_uses_expr(v, i, map);
                }
            }
            HirExpr::EnumLit { arg, .. } => {
                if let Some(a) = arg {
                    self.collect_uses_expr(a, i, map);
                }
            }
            HirExpr::Field { target, .. } => self.collect_uses_expr(target, i, map),
            HirExpr::Cast { target, .. } => self.collect_uses_expr(target, i, map),
            HirExpr::Try { target, .. } => self.collect_uses_expr(target, i, map),
            _ => {}
        }
    }

    // A borrow expression reads the source variable
    fn check_stmt(
        &mut self,
        stmt: &HirStmt,
        cur: usize,
        last_use: &HashMap<DefId, usize>,
        seq: &mut usize,
    ) -> Result<(), BorrowError> {
        match stmt {
            HirStmt::Let { def_id, init, .. } => {
                // Check one statement sequentially.
                if let HirExpr::Borrow {
                    mut_,
                    def_id: src,
                    span,
                } = init
                {
                    self.check_not_moved(*src, *span)?;
                    self.record_ref_origin(*def_id, *src);
                    self.activate_borrow(*def_id, *src, *mut_, *span, last_use, cur, false)?;
                } else {
                    // A `let` binds the initializer by value: a non-Copy initializer moves.
                    self.scan_expr(init, true, last_use, cur)?;
                    // `let r = x` where x is a reference copies it; r inherits x's origin.
                    if let HirExpr::Var(x, _) = init {
                        self.record_ref_origin(*def_id, *x);
                    }
                }
                Ok(())
            }
            HirStmt::Assign {
                def_id,
                value,
                span,
            } => {
                // Named borrow: let r = &x / &mut x
                self.end_named_borrow(*def_id);
                self.check_write(*def_id, *span, last_use, cur)?;
                if let HirExpr::Borrow {
                    mut_,
                    def_id: src,
                    span: bspan,
                } = value
                {
                    // Reassigning a reference variable ends its old borrow
                    self.check_not_moved(*src, *bspan)?;
                    self.record_ref_origin(*def_id, *src);
                    self.activate_borrow(*def_id, *src, *mut_, *bspan, last_use, cur, false)?;
                } else {
                    // Assignment transfers the value (moves a non-Copy source)
                    self.scan_expr(value, true, last_use, cur)?;
                    if let HirExpr::Var(x, _) = value {
                        self.record_ref_origin(*def_id, *x);
                    }
                }
                // Assigning re-initializes the variable: a moved value becomes usable again.
                self.moved.remove(def_id);
                Ok(())
            }
            HirStmt::AssignIndex {
                target,
                index,
                value,
                span,
            } => {
                // Assigning a reference variable establishes a new named borrow
                if let Some(base) = self.base_var(target) {
                    self.check_write(base, *span, last_use, cur)?;
                }
                self.scan_expr(target, false, last_use, cur)?;
                self.scan_expr(index, false, last_use, cur)?;
                self.scan_expr(value, true, last_use, cur)?;
                Ok(())
            }
            HirStmt::AssignDeref { target, value, .. } => {
                // Index-writes count as writing the base variable (arrays); pointer bases cannot be
                self.scan_expr(target, false, last_use, cur)?;
                self.scan_expr(value, true, last_use, cur)?;
                Ok(())
            }
            HirStmt::AssignField { target, value, .. } => {
                // Field-writes read the base variable (structs use value semantics)
                self.scan_expr(target, false, last_use, cur)?;
                self.scan_expr(value, true, last_use, cur)?;
                Ok(())
            }
            HirStmt::Print(args, _) => {
                // print reads its arguments (no ownership transfer, like Rust's formatting)
                for a in args {
                    self.scan_expr(a, false, last_use, cur)?;
                }
                Ok(())
            }
            HirStmt::Expr(expr, _) => self.scan_expr(expr, false, last_use, cur),
            HirStmt::If {
                cond,
                then_body,
                else_body,
                ..
            } => {
                self.scan_expr(cond, false, last_use, cur)?;
                // borrowed, so the write check is a no-op for them
                self.clean_temps();
                // Branch bodies are independent move domains: moves inside a branch do not
                // leak to the code after the if (both branches are checked from the same
                // pre-if state, and their moves are discarded at the end).
                let saved = self.moved.clone();
                self.check_block(then_body, seq)?;
                self.moved = saved.clone();
                self.check_block(else_body, seq)?;
                self.moved = saved;
                Ok(())
            }
            HirStmt::While { cond, body, .. } => {
                self.scan_expr(cond, false, last_use, cur)?;
                self.clean_temps();
                let saved = self.moved.clone();
                self.check_block(body, seq)?;
                self.moved = saved;
                Ok(())
            }
            HirStmt::Loop { body, .. } => {
                self.clean_temps();
                let saved = self.moved.clone();
                self.check_block(body, seq)?;
                self.moved = saved;
                Ok(())
            }
            HirStmt::For { iter, body, .. } => {
                self.scan_expr(iter, false, last_use, cur)?;
                self.clean_temps();
                let saved = self.moved.clone();
                self.check_block(body, seq)?;
                self.moved = saved;
                Ok(())
            }
            HirStmt::Break(_) | HirStmt::Continue(_) => Ok(()),
            HirStmt::Match { scrutinee, arms, .. } => {
                self.scan_expr(scrutinee, false, last_use, cur)?;
                self.clean_temps();
                // Each arm is checked from the same pre-match state; arm moves are scoped
                // to the arm (so arms that move a param and diverge keep compiling).
                let saved = self.moved.clone();
                for arm in arms {
                    self.moved = saved.clone();
                    self.check_block(&arm.body, seq)?;
                }
                self.moved = saved;
                Ok(())
            }
            HirStmt::Return(value, span) => {
                if let Some(v) = value {
                    // Returning a value consumes it: a non-Copy variable is moved out.
                    self.scan_expr(v, true, last_use, cur)?;
                    // A function whose return type is `&T` must return a reference
                    // derived from a parameter (not from a local, which would dangle).
                    if self.cur_return_is_ref {
                        self.check_return_ref(v, *span)?;
                    }
                }
                Ok(())
            }
            HirStmt::StructDef { .. } => Ok(()),
            HirStmt::EnumDef { .. } => Ok(()),
            HirStmt::TraitDef { .. } => Ok(()),
            HirStmt::ImplBlock { .. } => Ok(()),
        }
    }

    // *p = v writes the pointee; type checking guarantees p is &mut; no direct write check
    //
    // `consume` marks whether THIS expression is at an ownership-transferring position
    // (let initializer, assignment RHS, function-call argument, struct/enum/tuple/array
    // element, `return` value, `?` target). At such a position a non-Copy variable is
    // moved; everywhere else variables are only read (checked against use-after-move).
    fn scan_expr(
        &mut self,
        expr: &HirExpr,
        consume: bool,
        last_use: &HashMap<DefId, usize>,
        cur: usize,
    ) -> Result<(), BorrowError> {
        match expr {
            HirExpr::Var(def_id, span) => {
                // Reading a moved value is an error; consuming a non-Copy value moves it.
                self.check_not_moved(*def_id, *span)?;
                if consume && !self.is_copy_var(*def_id) {
                    self.moved.insert(*def_id);
                }
                Ok(())
            }
            HirExpr::Borrow {
                mut_,
                def_id,
                span,
            } => {
                // &x / &mut x reads the source without moving it
                self.check_not_moved(*def_id, *span)?;
                self.activate_borrow(0, *def_id, *mut_, *span, last_use, cur, true)
            }
            HirExpr::Deref { target, .. } => self.scan_expr(target, false, last_use, cur),
            HirExpr::MethodCall { recv, args, .. } => {
                // The receiver is borrowed in place (never moved); arguments pass by value.
                self.scan_expr(recv, false, last_use, cur)?;
                for a in args {
                    self.scan_expr(a, true, last_use, cur)?;
                }
                Ok(())
            }
            HirExpr::Matmul { lhs, rhs, .. } => {
                self.scan_expr(lhs, false, last_use, cur)?;
                self.scan_expr(rhs, false, last_use, cur)
            }
            HirExpr::Reduce { input, .. } => self.scan_expr(input, false, last_use, cur),
            HirExpr::ElemWise { lhs, rhs, .. } => {
                self.scan_expr(lhs, false, last_use, cur)?;
                if let Some(rhs) = rhs {
                    self.scan_expr(rhs, false, last_use, cur)?;
                }
                Ok(())
            }
            HirExpr::Blas { args, .. } => {
                for a in args {
                    self.scan_expr(a, false, last_use, cur)?;
                }
                Ok(())
            }
            HirExpr::Index {
                target,
                index,
                span: _,
            } => {
                self.scan_expr(target, false, last_use, cur)?;
                self.scan_expr(index, false, last_use, cur)
            }
            HirExpr::Unary { expr, .. } => self.scan_expr(expr, false, last_use, cur),
            HirExpr::Binary { lhs, rhs, .. }
            | HirExpr::Cmp { lhs, rhs, .. }
            | HirExpr::Logic { lhs, rhs, .. } => {
                self.scan_expr(lhs, false, last_use, cur)?;
                self.scan_expr(rhs, false, last_use, cur)
            }
            HirExpr::Call { def_id, args, .. } => {
                // A reference/pointer parameter auto-borrows its argument (`f(v)` ⇒
                // `f(&v)`), so the argument is read but not moved. Find the callee's
                // parameter types from the function table.
                let param_ref = self
                    .program
                    .funcs
                    .get(*def_id as usize)
                    .map(|f| {
                        f.params
                            .iter()
                            .map(|(_, t, _)| matches!(t, Ty::Ref { .. } | Ty::Ptr(_)))
                            .collect::<Vec<_>>()
                    });
                for (i, a) in args.iter().enumerate() {
                    let is_ref = param_ref.as_ref().map(|r| r.get(i).copied().unwrap_or(false)).unwrap_or(false);
                    self.scan_expr(a, !is_ref, last_use, cur)?;
                }
                Ok(())
            }
            HirExpr::FnRef { .. } => {
                // A function reference is Copy; nothing is moved.
                Ok(())
            }
            HirExpr::CallPtr { callee, args, .. } => {
                // Indirect call through a function pointer: the callee variable is
                // read (Copy), and the arguments are consumed by value.
                self.scan_expr(callee, false, last_use, cur)?;
                for a in args {
                    self.scan_expr(a, true, last_use, cur)?;
                }
                Ok(())
            }
            HirExpr::Tuple(elems, _) | HirExpr::Array(elems, _) => {
                for e in elems {
                    self.scan_expr(e, true, last_use, cur)?;
                }
                Ok(())
            }
            HirExpr::StructLit { fields, .. } => {
                for (_, v) in fields {
                    self.scan_expr(v, true, last_use, cur)?;
                }
                Ok(())
            }
            HirExpr::EnumLit { arg, .. } => {
                if let Some(a) = arg {
                    self.scan_expr(a, true, last_use, cur)?;
                }
                Ok(())
            }
            HirExpr::Field { target, .. } => self.scan_expr(target, false, last_use, cur),
            HirExpr::Cast { target, .. } => {
                // `expr as dyn Trait` boxes (moves) the target onto the heap.
                self.scan_expr(target, true, last_use, cur)
            }
            HirExpr::Try { target, .. } => {
                // `expr?` unwraps (consumes) the Result, moving its payload out.
                self.scan_expr(target, true, last_use, cur)
            }
            _ => Ok(()),
        }
    }

    /// Whether a variable's type is `Copy` (may be freely duplicated on use).
    fn is_copy_var(&self, def_id: DefId) -> bool {
        match self.var_tys.get(&def_id) {
            Some(ty) => is_copy(ty, self.program),
            // Unknown type: assume Copy to avoid false positives.
            None => true,
        }
    }

    /// Whether a variable's (reference) value originates from a function
    /// parameter — the only sources safe to return as a reference. A parameter
    /// is always safe; a local is safe only if it was recorded (via
    /// `record_ref_origin`) as a copy of a parameter's reference.
    fn origin_is_param(&self, def_id: DefId) -> bool {
        if self.cur_param_defs.contains(&def_id) {
            return true;
        }
        self.ref_origins.get(&def_id).copied().unwrap_or(false)
    }

    /// Record that `def_id` holds a reference copied from `src`'s value. Only
    /// meaningful when `def_id` itself is a reference-typed variable; queries
    /// never treat a non-reference as a returnable reference.
    fn record_ref_origin(&mut self, def_id: DefId, src: DefId) {
        if matches!(self.var_tys.get(&def_id), Some(Ty::Ref { .. })) {
            self.ref_origins.insert(def_id, self.origin_is_param(src));
        }
    }

    /// Validate a reference return: the value must ultimately derive from a
    /// parameter reference. Returning a reference to a local would dangle after
    /// the function's frame is reclaimed.
    fn check_return_ref(&self, value: &HirExpr, span: Span) -> Result<(), BorrowError> {
        match value {
            HirExpr::Var(d, _) => {
                if !self.origin_is_param(*d) {
                    return Err(BorrowError::new(
                        "cannot return a reference to a local variable (dangling reference); the returned reference must be derived from a parameter",
                        span,
                    ));
                }
            }
            HirExpr::Borrow { def_id: src, .. } => {
                if !self.origin_is_param(*src) {
                    return Err(BorrowError::new(
                        "cannot return a reference to a local variable (dangling reference); borrow from a parameter instead",
                        span,
                    ));
                }
            }
            // Auto-deref: `return &*x` re-borrows through a reference; recurse.
            HirExpr::Deref { target, .. } => self.check_return_ref(target, span)?,
            _ => {}
        }
        Ok(())
    }

    /// Error if the variable's value has already been moved.
    fn check_not_moved(&self, def_id: DefId, span: Span) -> Result<(), BorrowError> {
        if self.moved.contains(&def_id) {
            return Err(BorrowError::new(
                "use of moved value: the value was moved by an earlier assignment, argument pass, or return",
                span,
            ));
        }
        Ok(())
    }

    // Condition evaluation ends: temporary borrows are reclaimed immediately (the body may write)
    fn activate_borrow(
        &mut self,
        borrow_def: DefId,
        src: DefId,
        mut_: bool,
        span: Span,
        last_use: &HashMap<DefId, usize>,
        cur: usize,
        temp: bool,
    ) -> Result<(), BorrowError> {
        // Recursively activate temporary borrows inside an expression.
        if !temp {
            if let Some(bscope) = self.var_scopes.get(&borrow_def).copied() {
                let sscope = self
                    .var_scopes
                    .get(&src)
                    .copied()
                    .ok_or_else(|| self.internal_err(span))?;
                if bscope < sscope {
                    return Err(BorrowError::new(
                        "borrow escapes: the borrow value is defined in a scope shallower than the borrowed variable (possible dangling reference)",
                        span,
                    ));
                }
            }
        }
        // Escape check: the borrow value's defining scope must not be shallower than the source
        let has_active = |c: &Self| -> (bool, bool) {
            // (only for named borrows)
            match c.live.get(&src) {
                Some(v) => {
                    let active: Vec<&LiveBorrow> =
                        v.iter().filter(|b| c.is_active(b, cur, last_use)).collect();
                    let any_mut = active.iter().any(|b| b.mut_);
                    (!active.is_empty(), any_mut)
                }
                None => (false, false),
            }
        };
        let (any_active, any_mut) = has_active(self);
        if any_active {
            if mut_ {
                return Err(BorrowError::new(
                    "cannot mutably borrow: the variable is already borrowed (mutable borrows must be exclusive)",
                    span,
                ));
            } else if any_mut {
                return Err(BorrowError::new(
                    "cannot immutably borrow: the variable is already mutably borrowed",
                    span,
                ));
            }
        }
        self.live.entry(src).or_default().push(LiveBorrow {
            borrow_def,
            mut_,
            temp,
        });
        Ok(())
    }

    fn check_write(
        &self,
        def_id: DefId,
        span: Span,
        last_use: &HashMap<DefId, usize>,
        cur: usize,
    ) -> Result<(), BorrowError> {
        if let Some(v) = self.live.get(&def_id) {
            if v.iter()
                .any(|b| self.is_active(b, cur, last_use))
            {
                return Err(BorrowError::new(
                    "variable cannot be assigned while borrowed (end the borrow first)",
                    span,
                ));
            }
        }
        Ok(())
    }

    /// Whether a borrow is still active: temporary borrows always are; named borrows check
    fn is_active(&self, b: &LiveBorrow, cur: usize, last_use: &HashMap<DefId, usize>) -> bool {
        if b.temp {
            true
        } else {
            match last_use.get(&b.borrow_def) {
                Some(last) => *last >= cur,
                None => true,
            }
        }
    }

    /// End a variable's borrows as a holder (r = ... overwrites the old reference).
    fn end_named_borrow(&mut self, def_id: DefId) {
        self.live.retain(|_, v| {
            v.retain(|b| b.borrow_def != def_id);
            !v.is_empty()
        });
    }

    /// Reclaim all temporary borrows (called at the end of statements or if/while conditions).
    fn clean_temps(&mut self) {
        self.live.retain(|_, v| {
            v.retain(|b| !b.temp);
            !v.is_empty()
        });
    }

    /// Index-write base variable (arrays are written; pointers skip the write check).
    fn base_var(&self, expr: &HirExpr) -> Option<DefId> {
        match expr {
            HirExpr::Var(d, _) => Some(*d),
            HirExpr::Index { target, .. } => self.base_var(target),
            _ => None,
        }
    }

    fn internal_err(&self, span: Span) -> BorrowError {
        BorrowError::new("internal error: borrow source scope missing", span)
    }
}
