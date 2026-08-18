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
pub use hir::{HirBlock, HirEnumDef, HirExpr, HirFn, HirMatchArm, HirMatchPattern, HirProgram, HirStmt, HirStructDef};
pub use infer::{GenericInstance, InferError, InferResult};
pub use lower::LowerError;
pub use ty::Ty;

use aero_parse::ast::Program;

/// One-stop: AST → HIR + type checking; returns the variable type table (for codegen)
///
/// plus generic instance info. On success returns `(HirProgram, InferResult)`.
pub fn lower_and_check(program: &Program) -> Result<(HirProgram, InferResult), HircError> {
    let mut hir = lower::Lowerer::lower(program)?;
    let result = infer::Infer::check(&hir)?;
    // Write back the inferred const value types so unannotated consts (and their
    // `ConstRef` uses) carry the real type instead of the placeholder `i64`
    // (Phase P0-3, e.g. `const f = 2.5 * 2.0;` must infer `f64`).
    for c in &mut hir.consts {
        if let Some(t) = result.const_tys.get(&c.name) {
            c.ty = t.clone();
        }
    }
    Ok((hir, result))
}

/// Borrow + move check (Campaign 2/4): an independent memory-safety pass over the typed
/// HIR. `var_tys` (from inference) is needed to decide which types are `Copy`.
///
/// Returns the per-scope moved-variable map (see [`borrowck::check`]) on success;
/// codegen uses it to avoid dropping moved values (Phase 6 Drop/RAII).
pub fn check_borrows(
    program: &HirProgram,
    var_tys: &std::collections::HashMap<hir::DefId, Ty>,
) -> Result<std::collections::HashMap<hir::ScopeId, std::collections::HashSet<hir::DefId>>, BorrowError> {
    borrowck::check(program, var_tys)
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

    #[test]
    fn struct_literal_type_checked() {
        let (hir, result) =
            check("struct Point { x: i64, y: i64 } let p = Point { x: 1, y: 2 };").unwrap();
        // The struct definition is collected at the HIR level
        assert_eq!(hir.structs.len(), 1);
        assert_eq!(hir.structs[0].name, "Point");
        // The variable `p` has the named struct type
        let p_ty = result.var_tys.values().next().unwrap();
        assert_eq!(*p_ty, Ty::Struct("Point".to_string()));
    }

    #[test]
    fn struct_field_access_type_checked() {
        let (_, result) = check(
            "struct Point { x: i64, y: i64 } let p = Point { x: 1, y: 2 }; print(p.y);",
        )
        .unwrap();
        let _ = result;
    }

    #[test]
    fn struct_missing_field_rejected() {
        let err = check("struct Point { x: i64 } let p = Point { y: 2 };").unwrap_err();
        assert!(err_msg(&err).contains("no field"));
    }

    #[test]
    fn struct_unknown_field_rejected() {
        let err = check("struct Point { x: i64 } let p = Point { x: 1 }; print(p.z);").unwrap_err();
        assert!(err_msg(&err).contains("no field"));
    }

    #[test]
    fn struct_field_type_mismatch_rejected() {
        let err = check("struct Point { x: i64 } let p = Point { x: true };").unwrap_err();
        assert!(err_msg(&err).contains("type mismatch"));
    }

    #[test]
    fn struct_duplicate_field_rejected() {
        let err = check("struct Point { x: i64, x: i64 }").unwrap_err();
        assert!(err_msg(&err).contains("duplicate field"));
    }

    #[test]
    fn struct_unknown_type_in_field_rejected() {
        let err = check("struct Point { x: Nope }").unwrap_err();
        assert!(err_msg(&err).contains("unknown type"));
    }

    #[test]
    fn struct_as_fn_param_and_return() {
        let (_, result) = check(
            "struct Point { x: i64, y: i64 }
             fn dist2(p: Point) -> i64 { return p.x * p.x + p.y * p.y; }
             let d = dist2(Point { x: 3, y: 4 });",
        )
        .unwrap();
        let _ = result;
    }

    // ---------- enum algebraic data types ----------

    #[test]
    fn enum_definition_collected() {
        let (hir, result) =
            check("enum Maybe { Nothing, Just(i64) } let a = Maybe::Nothing; let b = Maybe::Just(1);")
                .unwrap();
        assert_eq!(hir.enums.len(), 1);
        assert_eq!(hir.enums[0].name, "Maybe");
        assert_eq!(hir.enums[0].variants.len(), 2);
        let a_ty = result.var_tys.iter().find(|(_, t)| **t == Ty::Enum("Maybe".into())).map(|(_, t)| t.clone()).unwrap();
        assert_eq!(a_ty, Ty::Enum("Maybe".to_string()));
    }

    #[test]
    fn enum_literal_payload_type_checked() {
        // f64 payload rejects an i64 literal under annotation, but the plain literal adapts
        let err = check("enum E { A(i64) } let e: E = E::A(true);").unwrap_err();
        assert!(err_msg(&err).contains("type mismatch"));
    }

    #[test]
    fn enum_variant_payload_required_and_forbidden() {
        let err = check("enum E { A(i64), B } let e = E::A;").unwrap_err();
        assert!(err_msg(&err).contains("payload"));
        let err = check("enum E { A(i64), B } let e = E::B(1);").unwrap_err();
        assert!(err_msg(&err).contains("payload"));
    }

    #[test]
    fn enum_unknown_variant_rejected() {
        let err = check("enum E { A, B } let e = E::C;").unwrap_err();
        assert!(err_msg(&err).contains("no variant"));
    }

    #[test]
    fn enum_match_binds_payload() {
        check(
            "enum Maybe { Nothing, Just(i64) }
             let b = Maybe::Just(42);
             match (b) {
                 Nothing => { print(0); }
                 Just(v) => { print(v); }
             }",
        )
        .unwrap();
    }

    #[test]
    fn enum_match_scrutinee_mismatch_rejected() {
        let err = check(
            "enum E { A, B }
             let x = 1;
             match (x) { A => { print(0); } B => { print(1); } }",
        )
        .unwrap_err();
        assert!(err_msg(&err).contains("scrutinee"));
    }

    #[test]
    fn enum_as_fn_param_and_return() {
        check(
            "enum Maybe { Nothing, Just(i64) }
             fn unwrap(m: Maybe) -> i64 {
                 match (m) {
                     Nothing => { return 0; }
                     Just(v) => { return v; }
                 }
                 return -1;
             }
             let x = unwrap(Maybe::Just(7));",
        )
        .unwrap();
    }

    #[test]
    fn enum_duplicate_variant_rejected() {
        let err = check("enum E { A, A }").unwrap_err();
        assert!(err_msg(&err).contains("duplicate variant"));
    }

    #[test]
    fn enum_name_conflicts_with_struct_rejected() {
        let err = check("enum E { A } struct E { x: i64 }").unwrap_err();
        assert!(err_msg(&err).contains("already defined"));
    }

    // ---------- trait system ----------

    #[test]
    fn trait_definition_collected() {
        let (hir, _) = check(
            "trait Drawable { fn draw(s: Square); }
             struct Square { side: i64, }",
        )
        .unwrap();
        assert_eq!(hir.traits.len(), 1);
        assert_eq!(hir.traits[0].name, "Drawable");
        assert_eq!(hir.traits[0].methods.len(), 1);
        assert_eq!(hir.traits[0].methods[0].name, "draw");
    }

    #[test]
    fn trait_impl_resolved() {
        let (hir, _) = check(
            "trait Drawable { fn draw(s: Square); }
             struct Square { side: i64, }
             impl Drawable for Square { fn draw(s: Square) { print(1); } }
             let s = Square { side: 1 };
             s.draw();",
        )
        .unwrap();
        assert_eq!(hir.impls.len(), 1);
        assert_eq!(hir.impls[0].type_name, "Square");
        assert!(hir
            .method_map
            .contains_key(&("Square".to_string(), "draw".to_string())));
    }

    #[test]
    fn trait_inherent_method_resolved() {
        let (hir, _) = check(
            "struct Rect { w: i64, h: i64, }
             impl Rect { fn area(r: Rect) -> i64 { return r.w * r.h; } }
             let r = Rect { w: 3, h: 4 };
             print(r.area());",
        )
        .unwrap();
        assert!(hir
            .method_map
            .contains_key(&("Rect".to_string(), "area".to_string())));
    }

    #[test]
    fn trait_impl_missing_method_rejected() {
        let err = check(
            "trait Drawable { fn draw(s: Square); fn name(s: Square); }
             struct Square { side: i64, }
             impl Drawable for Square { fn draw(s: Square) { print(1); } }",
        )
        .unwrap_err();
        assert!(err_msg(&err).contains("not implemented"));
    }

    #[test]
    fn trait_impl_signature_mismatch_rejected() {
        let err = check(
            "trait Drawable { fn draw(s: Square) -> i64; }
             struct Square { side: i64, }
             impl Drawable for Square { fn draw(s: Square) { print(1); } }",
        )
        .unwrap_err();
        assert!(err_msg(&err).contains("return type mismatch"));
    }

    #[test]
    fn trait_duplicate_impl_rejected() {
        let err = check(
            "trait Drawable { fn draw(s: Square); }
             struct Square { side: i64, }
             impl Drawable for Square { fn draw(s: Square) { print(1); } }
             impl Drawable for Square { fn draw(s: Square) { print(2); } }",
        )
        .unwrap_err();
        assert!(err_msg(&err).contains("already implemented"));
    }

    #[test]
    fn trait_bound_satisfied() {
        check(
            "trait Drawable { fn draw(s: Square); }
             struct Square { side: i64, }
             impl Drawable for Square { fn draw(s: Square) { print(1); } }
             fn draw_area<T: Drawable>(d: T) { d.draw(); }
             let s = Square { side: 1 };
             draw_area(s);",
        )
        .unwrap();
    }

    #[test]
    fn trait_bound_not_satisfied_rejected() {
        let err = check(
            "trait Drawable { fn draw(s: Square); }
             struct Square { side: i64, }
             impl Drawable for Square { fn draw(s: Square) { print(1); } }
             fn draw_area<T: Drawable>(d: T) { d.draw(); }
             draw_area(42);",
        )
        .unwrap_err();
        assert!(err_msg(&err).contains("does not implement trait"));
    }

    #[test]
    fn trait_unknown_method_rejected() {
        let err = check(
            "struct Point { x: i64, y: i64, }
             let p = Point { x: 1, y: 2 };
             p.unknown();",
        )
        .unwrap_err();
        assert!(err_msg(&err).contains("has no method"));
    }

    #[test]
    fn trait_unbounded_generic_method_rejected() {
        let err = check(
            "fn f<T>(x: T) { x.method(); }",
        )
        .unwrap_err();
        assert!(err_msg(&err).contains("has no trait bound"));
    }

    // Allow parse_source to return an AST directly (for span utility tests)
    #[allow(dead_code)]
    fn _span_smoke(span: Span) -> Span {
        span
    }
}
