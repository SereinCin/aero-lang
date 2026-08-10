//! aero-hir: a semantics-oriented high-level intermediate representation.
//!
//! Pipeline position: `AST → (lower) → HIR → (infer) → (borrowck) → typed HIR → codegen`
//!
//! - [`lower`]: AST → HIR lowering (name resolution + scope binding + type annotation lowering)
//! - [`infer`]: constraint-unification (HM-style) static type inference and checking
//! - [`borrowck`]: the borrow checker (Campaign 2, Rust-style ownership/borrow semantics)
//! - [`hir`]: HIR data structures
//! - [`ty`]: the type system (Int/Bool/Str/Tuple/Array/Fn/Ref/Ptr/Arena)

pub mod borrowck;
pub mod hir;
pub mod infer;
pub mod lower;
pub mod ty;

pub use borrowck::BorrowError;
pub use hir::{HirBlock, HirExpr, HirFn, HirProgram, HirStmt};
pub use infer::{GenericInstance, InferError, InferResult};
pub use lower::LowerError;
pub use ty::Ty;

use aero_parse::ast::Program;

/// One-stop: AST → HIR + type checking; returns the variable type table (for codegen)
///
/// plus generic instance info. On success returns `(HirProgram, InferResult)`.
pub fn lower_and_check(program: &Program) -> Result<(HirProgram, InferResult), HircError> {
    let hir = lower::Lowerer::lower(program)?;
    let result = infer::Infer::check(&hir)?;
    Ok((hir, result))
}

/// Borrow check (Campaign 2): an independent memory-safety pass over the typed HIR.
pub fn check_borrows(program: &HirProgram) -> Result<(), BorrowError> {
    borrowck::check(program)
}

/// Unified HIR-phase error.
#[derive(Debug, Clone)]
pub enum HircError {
    /// Name-resolution / scope error
    Lower(LowerError),
    /// Type error
    Infer(InferError),
}

impl From<LowerError> for HircError {
    fn from(e: LowerError) -> Self {
        HircError::Lower(e)
    }
}

impl From<InferError> for HircError {
    fn from(e: InferError) -> Self {
        HircError::Infer(e)
    }
}

impl HircError {
    /// Phase name (for error reporting).
    pub fn phase(&self) -> &'static str {
        match self {
            HircError::Lower(_) => "lowering",
            HircError::Infer(_) => "type checking",
        }
    }

    pub fn msg(&self) -> &str {
        match self {
            HircError::Lower(e) => &e.msg,
            HircError::Infer(e) => &e.msg,
        }
    }

    pub fn line(&self) -> u32 {
        match self {
            HircError::Lower(e) => e.line,
            HircError::Infer(e) => e.line,
        }
    }

    pub fn col(&self) -> u32 {
        match self {
            HircError::Lower(e) => e.col,
            HircError::Infer(e) => e.col,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aero_parse::{parse_source, span::Span};

    fn check(src: &str) -> Result<(HirProgram, InferResult), HircError> {
        let ast = parse_source(src).map_err(|e| {
            HircError::Lower(LowerError {
                msg: format!("parse error: {}", e.msg),
                line: e.line,
                col: e.col,
            })
        })?;
        lower_and_check(&ast)
    }

    fn err_msg(err: &HircError) -> &str {
        err.msg()
    }

    #[test]
    fn int_literal_infers_to_i64() {
        let (hir, result) = check("let x = 1 + 2;").unwrap();
        let _ = hir;
        let def = result.var_tys.values().next().unwrap();
        assert_eq!(*def, Ty::I64);
    }

    #[test]
    fn annotation_i32_accepts_literal() {
        let (hir, result) = check("let x: i32 = 1;").unwrap();
        let _ = hir;
        let def = result.var_tys.values().next().unwrap();
        assert_eq!(*def, Ty::I32);
    }

    #[test]
    fn annotation_mismatch_rejected() {
        let err = check("let x = 1 + 2; let y: i32 = x;").unwrap_err();
        assert!(err_msg(&err).contains("type mismatch"));
        assert!(err_msg(&err).contains("i32"));
        assert!(err_msg(&err).contains("i64"));
    }

    #[test]
    fn undefined_var_rejected() {
        let err = check("print(nope);").unwrap_err();
        assert!(err_msg(&err).contains("undefined variable"));
    }

    #[test]
    fn bool_arith_rejected() {
        let err = check("print(true + 1);").unwrap_err();
        assert!(err_msg(&err).contains("integer"));
    }

    #[test]
    fn if_cond_must_be_bool() {
        let err = check("if (1) { print(1); }").unwrap_err();
        assert!(err_msg(&err).contains("boolean"));
    }

    #[test]
    fn while_cond_must_be_bool() {
        let err = check("let x = 0; while (x) { x = x + 1; }").unwrap_err();
        assert!(err_msg(&err).contains("boolean"));
    }

    #[test]
    fn array_inferred_with_element_type() {
        let (hir, result) = check("let a = [1, 2, 3];").unwrap();
        let _ = hir;
        let ty = result.var_tys.values().next().unwrap();
        assert_eq!(*ty, Ty::Array(Box::new(Ty::I64), 3));
    }

    #[test]
    fn array_element_mismatch_rejected() {
        let err = check("let a = [1, true];").unwrap_err();
        assert!(err_msg(&err).contains("type mismatch"));
    }

    #[test]
    fn tuple_indexed() {
        let (hir, result) = check("let t = (1, true); print(t[1]);").unwrap();
        let _ = hir;
        let t = result.var_tys.values().next().unwrap();
        assert_eq!(*t, Ty::Tuple(vec![Ty::I64, Ty::Bool]));
    }

    #[test]
    fn tuple_index_out_of_range_rejected() {
        let err = check("let t = (1, 2); print(t[5]);").unwrap_err();
        assert!(err_msg(&err).contains("tuple index"));
    }

    #[test]
    fn fn_call_type_checked() {
        let (_, result) =
            check("fn add(a: i64, b: i64) -> i64 { return a + b; } let x = add(1, 2);").unwrap();
        let _ = result;
    }

    #[test]
    fn fn_call_arity_mismatch_rejected() {
        let err = check("fn add(a: i64, b: i64) -> i64 { return a + b; } print(add(1));").unwrap_err();
        assert!(err_msg(&err).contains("argument"));
    }

    #[test]
    fn fn_arg_type_mismatch_rejected() {
        let err =
            check("fn add(a: i64, b: i64) -> i64 { return a + b; } print(add(1, true));").unwrap_err();
        assert!(err_msg(&err).contains("type mismatch"));
    }

    #[test]
    fn fn_ret_type_mismatch_rejected() {
        let err = check("fn f() -> i64 { return true; }").unwrap_err();
        assert!(err_msg(&err).contains("return type"));
    }

    #[test]
    fn fn_recursion_supported() {
        // Pass-1 signature collection supports forward references and recursion
        let (_, _) = check("fn fact(n: i64) -> i64 { if (n <= 1) { return 1; } return n * fact(n - 1); } print(fact(5));").unwrap();
    }

    #[test]
    fn return_outside_fn_rejected() {
        let err = check("return 1;").unwrap_err();
        assert!(err_msg(&err).contains("return"));
    }

    #[test]
    fn void_fn_return_value_rejected() {
        let err = check("fn f() { return 1; }").unwrap_err();
        assert!(err_msg(&err).contains("no declared return type"));
    }

    #[test]
    fn scope_shadowing_outer_allowed() {
        // Inner scopes may shadow outer variables
        let _ = check("let x = 1; if (true) { let x = 2; print(x); } print(x);").unwrap();
    }

    #[test]
    fn duplicate_var_in_same_scope_rejected() {
        let err = check("let x = 1; let x = 2;").unwrap_err();
        assert!(err_msg(&err).contains("already declared"));
    }

    #[test]
    fn duplicate_fn_rejected() {
        let err = check("fn f() { } fn f() { }").unwrap_err();
        assert!(err_msg(&err).contains("duplicate definition"));
    }

    #[test]
    fn unknown_type_rejected() {
        let err = check("let x: float = 1;").unwrap_err();
        assert!(err_msg(&err).contains("unknown type"));
    }

    // Allow parse_source to return an AST directly (for span utility tests)
    #[allow(dead_code)]
    fn _span_smoke(span: Span) -> Span {
        span
    }
}
