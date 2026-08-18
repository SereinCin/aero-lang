//! Minimal compile-time evaluation for `const fn` (Phase 12.6).
//!
//! A `const fn` whose body is a pure scalar computation (literals, arithmetic,
//! comparisons, boolean logic, `let` bindings, `if`/`while`/`match`, and calls to
//! other `const fn`) can be evaluated at compile time when it is called with
//! constant arguments. Codegen folds the result into an LLVM constant instead of
//! emitting a runtime call.
//!
//! This is intentionally minimal: it returns `None` (fall back to a normal runtime
//! call) for any construct it cannot evaluate. It never silently produces a wrong
//! value.

use aero_hir::hir::{HirConstDef, HirExpr, HirFn, HirStmt, DefId};
use aero_parse::ast::{BinOp, CmpOp, LogicOp, UnOp};
use std::collections::HashMap;

/// A compile-time scalar value.
#[derive(Debug, Clone, PartialEq)]
pub enum ConstVal {
    Int(i64),
    Float(f64),
    Bool(bool),
    Char(char),
    Str(String),
}

/// Guard against non-terminating `while` loops and recursion at compile time.
const MAX_LOOP_ITERATIONS: u32 = 1_000_000;
const MAX_CALL_DEPTH: u32 = 512;

/// Control-flow signal produced by evaluating a statement/block.
enum Flow {
    Fallthrough,
    Return(ConstVal),
    Break,
    Continue,
}

struct Evaluator<'a> {
    funcs: &'a [HirFn],
    /// Top-level const definitions, so `const X = <other const/expr>` references
    /// can be resolved recursively (Phase P0-3).
    consts: &'a [HirConstDef],
    env: HashMap<DefId, ConstVal>,
    depth: u32,
}

/// Fold a self-contained expression (no free variables) to a constant, or `None`.
pub fn const_fold_expr(consts: &[HirConstDef], funcs: &[HirFn], e: &HirExpr) -> Option<ConstVal> {
    let mut ev = Evaluator {
        funcs,
        consts,
        env: HashMap::new(),
        depth: 0,
    };
    ev.eval_expr(e).ok()
}

/// Try to evaluate a `const fn` call where every argument folds to a constant.
/// Returns `None` if any argument is non-constant or the body cannot be folded.
pub fn try_eval_call(
    consts: &[HirConstDef],
    funcs: &[HirFn],
    f: &HirFn,
    args: &[HirExpr],
) -> Option<ConstVal> {
    let mut vals = Vec::with_capacity(args.len());
    for a in args {
        vals.push(const_fold_expr(consts, funcs, a)?);
    }
    eval_const_fn(consts, funcs, f, &vals)
}

/// Evaluate a `const fn` body with concrete constant arguments, or `None` if the
/// body cannot be evaluated at compile time.
pub fn eval_const_fn(
    consts: &[HirConstDef],
    funcs: &[HirFn],
    f: &HirFn,
    args: &[ConstVal],
) -> Option<ConstVal> {
    if !f.is_const || f.is_gpu || f.is_extern || f.builtin {
        return None;
    }
    if args.len() != f.params.len() {
        return None;
    }
    let mut ev = Evaluator {
        funcs,
        consts,
        env: HashMap::new(),
        depth: 0,
    };
    // Bind parameters to their DefIds (one-to-one with `param_defs`).
    for (def_id, arg) in f.param_defs.iter().zip(args.iter()) {
        ev.env.insert(*def_id, arg.clone());
    }
    match ev.eval_block(&f.body) {
        Ok(Flow::Return(v)) => Some(v),
        _ => None,
    }
}

impl<'a> Evaluator<'a> {
    fn eval_block(&mut self, block: &aero_hir::hir::HirBlock) -> Result<Flow, ()> {
        for stmt in &block.stmts {
            if let Flow::Return(v) = self.eval_stmt(stmt)? {
                return Ok(Flow::Return(v));
            }
        }
        Ok(Flow::Fallthrough)
    }

    fn eval_stmt(&mut self, stmt: &HirStmt) -> Result<Flow, ()> {
        match stmt {
            HirStmt::Let { def_id, init, .. } => {
                let v = self.eval_expr(init)?;
                self.env.insert(*def_id, v);
                Ok(Flow::Fallthrough)
            }
            HirStmt::Assign { def_id, value, .. } => {
                // Rebind the variable to the new constant value. Only scalar
                // reassignment is supported (env is a flat map of DefId -> value).
                let v = self.eval_expr(value)?;
                self.env.insert(*def_id, v);
                Ok(Flow::Fallthrough)
            }
            HirStmt::Return(Some(e), _) => {
                Ok(Flow::Return(self.eval_expr(e)?))
            }
            HirStmt::Return(None, _) => Err(()),
            HirStmt::Expr(e, _) => {
                self.eval_expr(e)?;
                Ok(Flow::Fallthrough)
            }
            HirStmt::If {
                cond, then_body, else_body, ..
            } => {
                let c = self.eval_expr(cond)?;
                let branch = match c {
                    ConstVal::Bool(true) => then_body,
                    ConstVal::Bool(false) => else_body,
                    _ => return Err(()),
                };
                self.eval_block(branch)
            }
            HirStmt::While { cond, body, .. } => {
                let mut iters = 0u32;
                loop {
                    let c = self.eval_expr(cond)?;
                    let keep = match c {
                        ConstVal::Bool(true) => true,
                        ConstVal::Bool(false) => false,
                        _ => return Err(()),
                    };
                    if !keep {
                        return Ok(Flow::Fallthrough);
                    }
                    iters += 1;
                    if iters > MAX_LOOP_ITERATIONS {
                        return Err(());
                    }
                    match self.eval_block(body)? {
                        Flow::Break => return Ok(Flow::Fallthrough),
                        Flow::Continue => continue,
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        Flow::Fallthrough => {}
                    }
                }
            }
            HirStmt::Loop { body, .. } => {
                // `loop { ... }` with a `break` can be folded; guard with an
                // iteration cap like `while`.
                let mut iters = 0u32;
                loop {
                    iters += 1;
                    if iters > MAX_LOOP_ITERATIONS {
                        return Err(());
                    }
                    match self.eval_block(body)? {
                        Flow::Break => return Ok(Flow::Fallthrough),
                        Flow::Continue => continue,
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        Flow::Fallthrough => {}
                    }
                }
            }
            HirStmt::Match {
                scrutinee, arms, ..
            } => {
                let s = self.eval_expr(scrutinee)?;
                for arm in arms {
                    let hit = match &arm.pattern {
                        aero_hir::hir::HirMatchPattern::Wildcard => true,
                        aero_hir::hir::HirMatchPattern::IntLit(n) => {
                            matches!(s, ConstVal::Int(v) if v == *n)
                        }
                        aero_hir::hir::HirMatchPattern::BoolLit(b) => {
                            matches!(s, ConstVal::Bool(v) if v == *b)
                        }
                        aero_hir::hir::HirMatchPattern::CharLit(c) => {
                            matches!(s, ConstVal::Char(v) if v == *c)
                        }
                        aero_hir::hir::HirMatchPattern::StrLit(t) => {
                            matches!(&s, ConstVal::Str(v) if v == t)
                        }
                        aero_hir::hir::HirMatchPattern::Bind(name, def_id) => {
                            self.env.insert(*def_id, s.clone());
                            let _ = name;
                            true
                        }
                        // Enum-variant patterns are beyond the scalar minimal scope.
                        aero_hir::hir::HirMatchPattern::EnumVariant { .. } => false,
                    };
                    if hit {
                        return self.eval_block(&arm.body);
                    }
                }
                Err(())
            }
            // Non-scalar statements (assignments to variables, struct/enum defs,
            // impl blocks, break/continue outside loops handled above) are out of
            // the minimal const scope.
            _ => Err(()),
        }
    }

    fn eval_expr(&mut self, e: &HirExpr) -> Result<ConstVal, ()> {
        match e {
            HirExpr::IntLit(n, _) => Ok(ConstVal::Int(*n)),
            HirExpr::FloatLit(n, _) => Ok(ConstVal::Float(*n)),
            HirExpr::CharLit(c, _) => Ok(ConstVal::Char(*c)),
            HirExpr::BoolLit(b, _) => Ok(ConstVal::Bool(*b)),
            HirExpr::StrLit(s, _) => Ok(ConstVal::Str(s.clone())),
            HirExpr::Var(def_id, _) => self.env.get(def_id).cloned().ok_or(()),
            HirExpr::ConstRef { name, .. } => {
                // Resolve a top-level const reference by recursively evaluating its
                // value expression (consts may reference other consts).
                let def = self
                    .consts
                    .iter()
                    .find(|c| c.name == *name)
                    .ok_or(())?;
                self.eval_expr(&def.value)
            }
            HirExpr::Unary { op, expr, .. } => {
                let v = self.eval_expr(expr)?;
                let r = match (op, v) {
                    (UnOp::Neg, ConstVal::Int(n)) => ConstVal::Int(n.wrapping_neg()),
                    (UnOp::Neg, ConstVal::Float(n)) => ConstVal::Float(-n),
                    _ => return Err(()),
                };
                Ok(r)
            }
            HirExpr::Binary { op, lhs, rhs, .. } => {
                let a = self.eval_expr(lhs)?;
                let b = self.eval_expr(rhs)?;
                let v = match (op, a, b) {
                    (BinOp::Add, ConstVal::Int(x), ConstVal::Int(y)) => {
                        ConstVal::Int(x.wrapping_add(y))
                    }
                    (BinOp::Sub, ConstVal::Int(x), ConstVal::Int(y)) => {
                        ConstVal::Int(x.wrapping_sub(y))
                    }
                    (BinOp::Mul, ConstVal::Int(x), ConstVal::Int(y)) => {
                        ConstVal::Int(x.wrapping_mul(y))
                    }
                    (BinOp::Div, ConstVal::Int(x), ConstVal::Int(y)) if y != 0 => {
                        ConstVal::Int(x.wrapping_div(y))
                    }
                    (BinOp::Rem, ConstVal::Int(x), ConstVal::Int(y)) if y != 0 => {
                        ConstVal::Int(x.wrapping_rem(y))
                    }
                    (BinOp::BitAnd, ConstVal::Int(x), ConstVal::Int(y)) => {
                        ConstVal::Int(x & y)
                    }
                    (BinOp::BitOr, ConstVal::Int(x), ConstVal::Int(y)) => {
                        ConstVal::Int(x | y)
                    }
                    (BinOp::BitXor, ConstVal::Int(x), ConstVal::Int(y)) => {
                        ConstVal::Int(x ^ y)
                    }
                    (BinOp::Shl, ConstVal::Int(x), ConstVal::Int(y)) => {
                        ConstVal::Int(x.wrapping_shl(y as u32))
                    }
                    (BinOp::Shr, ConstVal::Int(x), ConstVal::Int(y)) => {
                        ConstVal::Int(x.wrapping_shr(y as u32))
                    }
                    (BinOp::Add, ConstVal::Float(x), ConstVal::Float(y)) => {
                        ConstVal::Float(x + y)
                    }
                    (BinOp::Sub, ConstVal::Float(x), ConstVal::Float(y)) => {
                        ConstVal::Float(x - y)
                    }
                    (BinOp::Mul, ConstVal::Float(x), ConstVal::Float(y)) => {
                        ConstVal::Float(x * y)
                    }
                    (BinOp::Div, ConstVal::Float(x), ConstVal::Float(y)) if y != 0.0 => {
                        ConstVal::Float(x / y)
                    }
                    (BinOp::Rem, ConstVal::Float(x), ConstVal::Float(y)) if y != 0.0 => {
                        ConstVal::Float(x % y)
                    }
                    _ => return Err(()),
                };
                Ok(v)
            }
            HirExpr::Cmp { op, lhs, rhs, .. } => {
                let a = self.eval_expr(lhs)?;
                let b = self.eval_expr(rhs)?;
                let r = match (op, a, b) {
                    (CmpOp::Eq, x, y) => x == y,
                    (CmpOp::Ne, x, y) => x != y,
                    (CmpOp::Lt, ConstVal::Int(x), ConstVal::Int(y)) => x < y,
                    (CmpOp::Gt, ConstVal::Int(x), ConstVal::Int(y)) => x > y,
                    (CmpOp::Le, ConstVal::Int(x), ConstVal::Int(y)) => x <= y,
                    (CmpOp::Ge, ConstVal::Int(x), ConstVal::Int(y)) => x >= y,
                    (CmpOp::Lt, ConstVal::Float(x), ConstVal::Float(y)) => x < y,
                    (CmpOp::Gt, ConstVal::Float(x), ConstVal::Float(y)) => x > y,
                    (CmpOp::Le, ConstVal::Float(x), ConstVal::Float(y)) => x <= y,
                    (CmpOp::Ge, ConstVal::Float(x), ConstVal::Float(y)) => x >= y,
                    _ => return Err(()),
                };
                Ok(ConstVal::Bool(r))
            }
            HirExpr::Logic { op, lhs, rhs, .. } => {
                match op {
                    LogicOp::And => {
                        let a = self.eval_expr(lhs)?;
                        let a = match a {
                            ConstVal::Bool(b) => b,
                            _ => return Err(()),
                        };
                        if !a {
                            return Ok(ConstVal::Bool(false));
                        }
                        let b = self.eval_expr(rhs)?;
                        match b {
                            ConstVal::Bool(b) => Ok(ConstVal::Bool(b)),
                            _ => Err(()),
                        }
                    }
                    LogicOp::Or => {
                        let a = self.eval_expr(lhs)?;
                        let a = match a {
                            ConstVal::Bool(b) => b,
                            _ => return Err(()),
                        };
                        if a {
                            return Ok(ConstVal::Bool(true));
                        }
                        let b = self.eval_expr(rhs)?;
                        match b {
                            ConstVal::Bool(b) => Ok(ConstVal::Bool(b)),
                            _ => Err(()),
                        }
                    }
                }
            }
            HirExpr::Call { def_id, args, .. } => {
                if self.depth >= MAX_CALL_DEPTH {
                    return Err(());
                }
                let callee = self
                    .funcs
                    .get(*def_id as usize)
                    .ok_or(())?;
                if !callee.is_const {
                    // Fall back only if the call is to a genuine const fn; a call
                    // to a non-const fn cannot be folded.
                    return Err(());
                }
                let mut arg_vals = Vec::with_capacity(args.len());
                for a in args {
                    arg_vals.push(self.eval_expr(a)?);
                }
                self.depth += 1;
                let result = eval_const_fn(self.consts, self.funcs, callee, &arg_vals);
                self.depth -= 1;
                result.ok_or(())
            }
            // Everything else (borrows, derefs, method calls, struct/enum literals,
            // arrays/tuples, casts, arenas, ...) is outside the scalar const scope.
            _ => Err(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lower a source and return the full function table plus the function with
    /// the given name. The full table is needed because a `const fn` body refers
    /// to callees by their DefId (index into the funcs table).
    fn lower_fn(source: &str, name: &str) -> (Vec<HirFn>, HirFn) {
        let mut tokens = aero_std::std_tokens().to_vec();
        tokens.extend(aero_lex::lex(source).unwrap());
        let program = aero_parse::parse(&tokens).unwrap();
        let (hir, _result) = aero_hir::lower_and_check(&program).unwrap();
        let f = hir
            .funcs
            .iter()
            .find(|f| f.name == name)
            .cloned()
            .unwrap_or_else(|| panic!("function {name} not found"));
        (hir.funcs, f)
    }

    #[test]
    fn const_fn_add_folds() {
        let (funcs, f) = lower_fn("const fn add(a: i64, b: i64) -> i64 { return a + b; }", "add");
        assert!(f.is_const);
        let consts = &[];
        let v = eval_const_fn(consts, &funcs, &f, &[ConstVal::Int(2), ConstVal::Int(3)]).unwrap();
        assert_eq!(v, ConstVal::Int(5));
    }

    #[test]
    fn const_fn_recursion_folds() {
        let (funcs, f) = lower_fn(
            "const fn fact(n: i64) -> i64 { if (n <= 1) { return 1; } return n * fact(n - 1); }",
            "fact",
        );
        let consts = &[];
        let v = eval_const_fn(consts, &funcs, &f, &[ConstVal::Int(5)]).unwrap();
        assert_eq!(v, ConstVal::Int(120));
    }

    #[test]
    fn const_fn_while_loop_folds() {
        let (funcs, f) = lower_fn(
            "const fn fib(n: i64) -> i64 { let a = 0; let b = 1; let i = 0; while (i < n) { let t = a + b; a = b; b = t; i = i + 1; } return a; }",
            "fib",
        );
        let consts = &[];
        let v = eval_const_fn(consts, &funcs, &f, &[ConstVal::Int(10)]).unwrap();
        assert_eq!(v, ConstVal::Int(55));
    }

    #[test]
    fn const_fn_bool_result() {
        let (funcs, f) = lower_fn(
            "const fn is_even(n: i64) -> bool { return n % 2 == 0; }",
            "is_even",
        );
        let consts = &[];
        let v = eval_const_fn(consts, &funcs, &f, &[ConstVal::Int(7)]).unwrap();
        assert_eq!(v, ConstVal::Bool(false));
    }

    #[test]
    fn non_const_fn_is_not_evaluable() {
        // A regular (non-const) fn must never be treated as a const fn.
        let (funcs, f) = lower_fn("fn add(a: i64, b: i64) -> i64 { return a + b; }", "add");
        assert!(!f.is_const);
        let consts = &[];
        let v = eval_const_fn(consts, &funcs, &f, &[ConstVal::Int(2), ConstVal::Int(3)]);
        assert!(v.is_none());
    }
}