/// Borrow checker: an independent HIR-level pass (Campaign 2).
///
/// Semantics: Rust-style ownership + borrows. This pass runs after type inference and
/// before codegen, statically checking these memory-safety issues:
/// 1. **Mutable-borrow exclusivity**: at most one active mutable borrow per source
/// 2. **Immutable/mutable exclusivity**: `&x` and `&mut x` cannot coexist
/// 3. **No writes to the source while borrowed**: assigning/index-writing the source is an
///    error (prevents dangling references)
///
/// Borrow lifetimes (NLL-lite): bounded by the last use.
/// - **Named borrows** (`let r = &x` / `r = &x`): live until the last use of `r`
/// - **Temporary borrows** (call args, intermediate expressions): live until the end of
///
///   their statement (or if/while condition)
use std::collections::HashMap;

use aero_parse::span::Span;

use crate::hir::{DefId, HirBlock, HirExpr, HirProgram, HirStmt, ScopeId};

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

struct Checker {
    /// Variable DefId → defining scope
    var_scopes: HashMap<DefId, ScopeId>,
    /// Active borrows, grouped by source variable
    live: HashMap<DefId, Vec<LiveBorrow>>,
}

/// Entry point: run the borrow check over an HIR program.
pub fn check(program: &HirProgram) -> Result<(), BorrowError> {
    let mut c = Checker {
        var_scopes: HashMap::new(),
        live: HashMap::new(),
    };
    // Function parameters live in the outermost scope (0); inner borrows cannot escape
    for f in &program.funcs {
        for &pd in &f.param_defs {
            c.var_scopes.insert(pd, 0);
        }
    }
    let mut seq = 0usize;
    c.check_block(&program.main, &mut seq)?;
    for f in &program.funcs {
        c.check_block(&f.body, &mut seq)?;
    }
    Ok(())
}

impl Checker {
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
            HirStmt::Return(value, _) => {
                if let Some(v) = value {
                    self.collect_uses_expr(v, i, map);
                }
            }
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
            HirExpr::Tuple(elems, _) | HirExpr::Array(elems, _) => {
                for e in elems {
                    self.collect_uses_expr(e, i, map);
                }
            }
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
                    self.activate_borrow(*def_id, *src, *mut_, *span, last_use, cur, false)?;
                } else {
                    self.scan_expr(init, last_use, cur)?;
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
                    self.activate_borrow(*def_id, *src, *mut_, *bspan, last_use, cur, false)?;
                } else {
                    self.scan_expr(value, last_use, cur)?;
                }
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
                self.scan_expr(target, last_use, cur)?;
                self.scan_expr(index, last_use, cur)?;
                self.scan_expr(value, last_use, cur)?;
                Ok(())
            }
            HirStmt::AssignDeref { target, value, .. } => {
                // Index-writes count as writing the base variable (arrays); pointer bases cannot be
                self.scan_expr(target, last_use, cur)?;
                self.scan_expr(value, last_use, cur)?;
                Ok(())
            }
            HirStmt::Print(args, _) => {
                for a in args {
                    self.scan_expr(a, last_use, cur)?;
                }
                Ok(())
            }
            HirStmt::Expr(expr, _) => self.scan_expr(expr, last_use, cur),
            HirStmt::If {
                cond,
                then_body,
                else_body,
                ..
            } => {
                self.scan_expr(cond, last_use, cur)?;
                // borrowed, so the write check is a no-op for them
                self.clean_temps();
                self.check_block(then_body, seq)?;
                self.check_block(else_body, seq)?;
                Ok(())
            }
            HirStmt::While { cond, body, .. } => {
                self.scan_expr(cond, last_use, cur)?;
                self.clean_temps();
                self.check_block(body, seq)?;
                Ok(())
            }
            HirStmt::Return(value, _) => {
                if let Some(v) = value {
                    self.scan_expr(v, last_use, cur)?;
                }
                Ok(())
            }
        }
    }

    // *p = v writes the pointee; type checking guarantees p is &mut; no direct write check
    fn scan_expr(
        &mut self,
        expr: &HirExpr,
        last_use: &HashMap<DefId, usize>,
        cur: usize,
    ) -> Result<(), BorrowError> {
        match expr {
            HirExpr::Borrow {
                mut_,
                def_id,
                span,
            } => self.activate_borrow(0, *def_id, *mut_, *span, last_use, cur, true),
            HirExpr::Deref { target, .. } => self.scan_expr(target, last_use, cur),
            HirExpr::MethodCall { recv, args, .. } => {
                self.scan_expr(recv, last_use, cur)?;
                for a in args {
                    self.scan_expr(a, last_use, cur)?;
                }
                Ok(())
            }
            HirExpr::Matmul { lhs, rhs, .. } => {
                self.scan_expr(lhs, last_use, cur)?;
                self.scan_expr(rhs, last_use, cur)
            }
            HirExpr::Index {
                target,
                index,
                span: _,
            } => {
                self.scan_expr(target, last_use, cur)?;
                self.scan_expr(index, last_use, cur)
            }
            HirExpr::Unary { expr, .. } => self.scan_expr(expr, last_use, cur),
            HirExpr::Binary { lhs, rhs, .. }
            | HirExpr::Cmp { lhs, rhs, .. }
            | HirExpr::Logic { lhs, rhs, .. } => {
                self.scan_expr(lhs, last_use, cur)?;
                self.scan_expr(rhs, last_use, cur)
            }
            HirExpr::Call { args, .. } => {
                for a in args {
                    self.scan_expr(a, last_use, cur)?;
                }
                Ok(())
            }
            HirExpr::Tuple(elems, _) | HirExpr::Array(elems, _) => {
                for e in elems {
                    self.scan_expr(e, last_use, cur)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
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
