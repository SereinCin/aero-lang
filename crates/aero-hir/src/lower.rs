/// AST → HIR lowering: name resolution + scope binding + type annotations
/// lowered to `Ty`.
///
/// Two-pass flow:
/// 1. First pass scans top-level `fn` definitions and collects all function
///    signatures (supports forward calls and recursion)
/// 2. Second pass lowers the main block and every function body (parameters
///    bound to the function scope)
use aero_parse::ast::{Expr, MatchPattern, Program, Stmt, TypeExpr};
use aero_parse::span::Span;

use crate::hir::{
    BlasOp, DefId, ElemOp, HirBlock, HirConstDef, HirEnumDef, HirExpr, HirFn, HirImplBlock,
    HirMatchArm, HirMatchPattern, HirProgram, HirStmt, HirStructDef, HirTraitDef,
    HirTraitMethodSig, HirUnionDef, ReduceOp, ScopeId,
};
use crate::ty::Ty;

/// Map a builtin tensor-reduction name (`sum`/`mean`/`max`/`min`) to its op.
fn reduce_op(name: &str) -> Option<ReduceOp> {
    match name {
        "sum" => Some(ReduceOp::Sum),
        "mean" => Some(ReduceOp::Mean),
        "max" => Some(ReduceOp::Max),
        "min" => Some(ReduceOp::Min),
        _ => None,
    }
}

/// Whether `name` is a reserved tensor-reduction builtin.
fn is_reduce_name(name: &str) -> bool {
    reduce_op(name).is_some()
}

/// Map a builtin element-wise tensor op name to its op.
fn elem_op(name: &str) -> Option<ElemOp> {
    match name {
        "tensor_add" => Some(ElemOp::Add),
        "tensor_sub" => Some(ElemOp::Sub),
        "tensor_mul" => Some(ElemOp::Mul),
        "tensor_div" => Some(ElemOp::Div),
        "tensor_neg" => Some(ElemOp::Neg),
        _ => None,
    }
}

/// Whether `name` is a reserved element-wise tensor builtin.
fn is_elem_name(name: &str) -> bool {
    elem_op(name).is_some()
}

/// Map a builtin BLAS Level-1 tensor op name (`blas_dot`/`blas_nrm2`/
/// `blas_asum`/`blas_amax`/`blas_scal`/`blas_axpy`) to its op.
fn blas_op(name: &str) -> Option<BlasOp> {
    match name {
        "blas_dot" => Some(BlasOp::Dot),
        "blas_nrm2" => Some(BlasOp::Nrm2),
        "blas_asum" => Some(BlasOp::Asum),
        "blas_amax" => Some(BlasOp::Amax),
        "blas_scal" => Some(BlasOp::Scal),
        "blas_axpy" => Some(BlasOp::Axpy),
        _ => None,
    }
}

/// Whether `name` is a reserved BLAS tensor builtin.
fn is_blas_name(name: &str) -> bool {
    blas_op(name).is_some()
}

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
    /// Struct definitions (collected in pass 1)
    structs: Vec<HirStructDef>,
    /// Struct name → index into `structs`
    struct_by_name: std::collections::HashMap<String, usize>,
    /// Enum definitions (collected in pass 1)
    enums: Vec<HirEnumDef>,
    /// Enum name → index into `enums`
    enum_by_name: std::collections::HashMap<String, usize>,
    /// Union definitions (collected in pass 1)
    unions: Vec<HirUnionDef>,
    /// Union name → index into `unions`
    union_by_name: std::collections::HashMap<String, usize>,
    /// Const definitions (collected in pass 1, Phase P0-3)
    consts: Vec<HirConstDef>,
    /// Const name → index into `consts`
    const_by_name: std::collections::HashMap<String, usize>,
    /// Trait definitions (collected in pass 1)
    traits: Vec<HirTraitDef>,
    /// Trait name → index into `traits`
    trait_by_name: std::collections::HashMap<String, usize>,
    /// Impl blocks (collected in pass 1)
    impls: Vec<HirImplBlock>,
    /// Method resolution table: (type_name, method_name) → function DefId
    method_map: std::collections::HashMap<(String, String), DefId>,
    /// Variable scope stack: outer scopes first
    scopes: Vec<std::collections::HashMap<String, DefId>>,
    /// Generic type parameter names of the current function (used by lower_type
/// to recognize `T` as a generic parameter)
    gen_params: Vec<String>,
    /// Current `Self` type. Set while lowering trait method signatures
    /// (`Generic("Self")`) and impl method signatures (the impl target type).
    /// Used by `lower_type` to resolve `TypeExpr::Named("Self")`.
    self_type: Option<Ty>,
    /// Current associated-type context. While lowering trait method signatures it
    /// maps each trait assoc name → `Ty::Assoc(name)`; while lowering impl method
    /// signatures it maps each assoc name → the impl's concrete binding. Used by
    /// `lower_type` to resolve `TypeExpr::Path { root: "Self", name }`.
    self_assoc: Option<Vec<(String, Ty)>>,
    /// Next variable DefId
    next_var: DefId,
    /// Next scope id
    next_scope: ScopeId,
    /// Current module path (empty = crate root). Names inside a module are
    /// mangled to `module::name`; references resolved against this prefix.
    module_path: String,
    /// `use` aliases per module path: module_path → list of import paths
    /// (e.g. `use a::b::C` → target `a::b::C`, imported as `C`).
    uses: std::collections::HashMap<String, Vec<Vec<String>>>,
}

/// Signatures collected in pass 1.
struct FuncSig {
    name: String,
    def_id: DefId,
    type_params: Vec<String>,
    lifetimes: Vec<String>,
    trait_bounds: Vec<(String, String)>,
    params: Vec<(String, Ty, Span)>,
    ret: Option<Ty>,
    is_gpu: bool,
    is_const: bool,
    is_extern: bool,
    extern_symbol: Option<String>,
    builtin: bool,
    span: Span,
}

/// Substitute generic type parameters (e.g. `Self`, trait type params) in a type.
fn subst_ty(ty: &Ty, map: &std::collections::HashMap<String, Ty>) -> Ty {
    match ty {
        Ty::Generic(name) => map.get(name).cloned().unwrap_or_else(|| ty.clone()),
        Ty::Assoc(name) => map.get(name).cloned().unwrap_or_else(|| ty.clone()),
        Ty::Ref {
                mut_,
                lifetime,
                inner,
            } => Ty::Ref {
                mut_: *mut_,
                lifetime: lifetime.clone(),
                inner: Box::new(subst_ty(inner, map)),
            },
        Ty::Tuple(elems) => Ty::Tuple(elems.iter().map(|e| subst_ty(e, map)).collect()),
        Ty::Array(elem, n) => Ty::Array(Box::new(subst_ty(elem, map)), *n),
        Ty::Ptr(inner) => Ty::Ptr(Box::new(subst_ty(inner, map))),
        Ty::StructGeneric { name, args } => Ty::StructGeneric {
            name: name.clone(),
            args: args.iter().map(|a| subst_ty(a, map)).collect(),
        },
        Ty::EnumGeneric { name, args } => Ty::EnumGeneric {
            name: name.clone(),
            args: args.iter().map(|a| subst_ty(a, map)).collect(),
        },
        Ty::Vec(elem) => Ty::Vec(Box::new(subst_ty(elem, map))),
        Ty::Box(inner) => Ty::Box(Box::new(subst_ty(inner, map))),
        Ty::Tensor { elem, shape } => Ty::Tensor {
            elem: Box::new(subst_ty(elem, map)),
            shape: shape.clone(),
        },
        _ => ty.clone(),
    }
}

/// Mangle a top-level definition's name with a module-path prefix. Non-definition
/// statements are returned unchanged (they are consumed by the main-block pass).
fn mangle_def_name(stmt: &Stmt, prefix: &str) -> Stmt {
    if prefix.is_empty() {
        return stmt.clone();
    }
    let q = |n: &String| format!("{prefix}::{n}");
    let mut s = stmt.clone();
    match &mut s {
        Stmt::FnDef { name, .. } => *name = q(name),
        Stmt::StructDef { name, .. } => *name = q(name),
        Stmt::UnionDef { name, .. } => *name = q(name),
        Stmt::ConstDef { name, .. } => *name = q(name),
        Stmt::EnumDef { name, .. } => *name = q(name),
        Stmt::TraitDef { name, .. } => *name = q(name),
        Stmt::ImplBlock { type_name, .. } => *type_name = q(type_name),
        _ => {}
    }
    s
}

/// Flatten a module-structured statement list into a flat top-level list.
///
/// - `mod name { items }` hoists `items` to the top level with definition names
///   mangled to `name::item` (nested modules extend the prefix).
/// - `pub <item>` is unwrapped (single-crate visibility; everything is reachable).
/// - `mod name;` (file-backed) is rejected until multi-file loading lands.
/// - `use path::...;` is collected into `uses` as `(module_path, segments)` and
///   removed from the statement list; the lowerer resolves them into aliases.
fn flatten_modules(
    stmts: &[Stmt],
    prefix: &str,
    uses: &mut Vec<(String, Vec<String>)>,
) -> Result<Vec<Stmt>, LowerError> {
    let mut out = Vec::new();
    for stmt in stmts {
        match stmt {
            Stmt::ModDef { name, items, .. } => {
                let new_prefix = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}::{name}")
                };
                out.extend(flatten_modules(items, &new_prefix, uses)?);
            }
            Stmt::Pub(inner, _) => {
                out.extend(flatten_modules(std::slice::from_ref(&**inner), prefix, uses)?);
            }
            Stmt::UseDecl { path, span } => {
                if path.last().map(|s| s.as_str()) != Some("*") {
                    uses.push((prefix.to_string(), path.clone()));
                } else {
                    return Err(LowerError::new(
                        "glob imports (`use path::*`) are not yet supported",
                        *span,
                    ));
                }
            }
            Stmt::ModFile { name, span } => {
                return Err(LowerError::new(
                    format!(
                        "file modules (`mod {name};`) are not yet supported; write `mod {name} {{ ... }}` inline"
                    ),
                    *span,
                ));
            }
            _ => out.push(mangle_def_name(stmt, prefix)),
        }
    }
    Ok(out)
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
// UTF-8 string builtins (stdlib Phase 1, 字符串 2.0): Unicode code point counting
// and indexing over a NUL-terminated `str`. utf8_len counts code points (not bytes);
// utf8_at returns the code point at a character index, or -1 when out of range.
("utf8_len", &[Ty::Str], Some(Ty::I64)),
("utf8_at", &[Ty::Str, Ty::I64], Some(Ty::I64)),
// File IO and command-line arguments (M1.2).
("read_file", &[Ty::Str], Some(Ty::Str)),
("write_file", &[Ty::Str, Ty::Str], Some(Ty::I64)),
("arg_count", &[], Some(Ty::I64)),
("arg", &[Ty::I64], Some(Ty::Str)),
// Time / random number helpers (stdlib extension, Phase 11 simple items).
("rand", &[], Some(Ty::I64)),
("time", &[], Some(Ty::I64)),
// printf-style formatting into a fresh `str` buffer (Phase 11.2 formatting).
// Variadic: `format(fmt, args...)` or `format(value)`; special-cased in infer.
("format", &[Ty::Str], Some(Ty::Str)),
// Hash functions (Phase 11.1 HashMap/HashSet prerequisite): FNV-1a over a
// NUL-terminated `str`, and splitmix64 mixing for integer keys.
("str_hash", &[Ty::Str], Some(Ty::I64)),
("hash_i64", &[Ty::I64], Some(Ty::I64)),
// Environment variables (stdlib, Phase 1 environment): getenv/setenv wrappers.
// get_env returns the value or "" when unset; has_env distinguishes unset.
("get_env", &[Ty::Str], Some(Ty::Str)),
("set_env", &[Ty::Str, Ty::Str], Some(Ty::Bool)),
("has_env", &[Ty::Str], Some(Ty::Bool)),
// Path probe (stdlib, Phase 1 paths): file_exists checks fopen("rb").
("file_exists", &[Ty::Str], Some(Ty::Bool)),
];

impl Lowerer {
    pub fn lower(program: &Program) -> Result<HirProgram, LowerError> {
        let mut lowerer = Lowerer {
            funcs: Vec::new(),
            func_by_name: std::collections::HashMap::new(),
            structs: Vec::new(),
            struct_by_name: std::collections::HashMap::new(),
            enums: Vec::new(),
            enum_by_name: std::collections::HashMap::new(),
            unions: Vec::new(),
            union_by_name: std::collections::HashMap::new(),
            consts: Vec::new(),
            const_by_name: std::collections::HashMap::new(),
            traits: Vec::new(),
            trait_by_name: std::collections::HashMap::new(),
            impls: Vec::new(),
            method_map: std::collections::HashMap::new(),
            scopes: Vec::new(),
            gen_params: Vec::new(),
            self_type: None,
            self_assoc: None,
            next_var: 0,
            next_scope: 0,
            module_path: String::new(),
            uses: std::collections::HashMap::new(),
        };
        // Pre-pass: flatten inline modules into a flat top-level statement list
        // (hoisting `mod m { items }` to `m::item`), collect `use` aliases, unwrap
        // `pub`, and reject file modules until multi-file loading lands.
        let mut use_collect: Vec<(String, Vec<String>)> = Vec::new();
        let flat_stmts = flatten_modules(&program.stmts, "", &mut use_collect)?;
        for (mod_path, path) in use_collect {
            lowerer.uses.entry(mod_path).or_default().push(path);
        }
        let program = Program { stmts: flat_stmts };
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
                lifetimes: Vec::new(),
                trait_bounds: Vec::new(),
                params: params
                    .iter()
                    .cloned()
                    .map(|t| (String::new(), t, dummy_span))
                    .collect(),
                ret: ret.clone(),
                is_gpu: false,
                is_const: false,
                is_extern: false,
                extern_symbol: None,
                builtin: true,
                span: dummy_span,
            });
        }
        // Pass 1: collect top-level struct definitions (before function signatures
        // so function parameter/return types can reference structs)
        for stmt in &program.stmts {
            if let Stmt::StructDef {
                name,
                type_params,
                fields,
                derives,
                span,
            } = stmt
            {
                if lowerer.struct_by_name.contains_key(name) {
                    return Err(LowerError::new(
                        format!("duplicate definition of struct `{name}`"),
                        *span,
                    ));
                }
                if lowerer.func_by_name.contains_key(name) {
                    return Err(LowerError::new(
                        format!("`{name}` is already defined as a function"),
                        *span,
                    ));
                }
                // Validate generic parameter names (no builtin-type collisions)
                for tp in type_params {
                    if matches!(tp.as_str(), "i32" | "i64" | "f32" | "f64" | "char" | "bool" | "str") {
                        return Err(LowerError::new(
                            format!("generic type parameter `{tp}` collides with a builtin type name"),
                            *span,
                        ));
                    }
                }
                // Lower field types in the struct's generic-parameter context (so
                // `struct Vec<T> { data: *T }` resolves `T` to a generic type)
                let mut hir_fields = Vec::new();
                let mut seen = std::collections::HashSet::new();
                let saved_gen = std::mem::take(&mut lowerer.gen_params);
                lowerer.gen_params = type_params.clone();
                for (fname, fty) in fields {
                    if !seen.insert(fname.clone()) {
                        lowerer.gen_params = saved_gen;
                        return Err(LowerError::new(
                            format!("duplicate field `{fname}` in struct `{name}`"),
                            *span,
                        ));
                    }
                    let ty = lowerer.lower_type(fty)?;
                    hir_fields.push((fname.clone(), ty));
                }
                lowerer.gen_params = saved_gen;
                let idx = lowerer.structs.len();
                lowerer.struct_by_name.insert(name.clone(), idx);
                lowerer.structs.push(HirStructDef {
                    name: name.clone(),
                    type_params: type_params.clone(),
                    fields: hir_fields,
                    span: *span,
                });
                // `#[derive(Copy)]` expands to an empty `impl Copy for Name {}`
                for derive in derives {
                    if derive == "Copy" {
                        lowerer.impls.push(HirImplBlock {
                            type_params: type_params.clone(),
                            trait_name: Some("Copy".to_string()),
                            trait_args: Vec::new(),
                            assoc_types: Vec::new(),
                            type_name: name.clone(),
                            methods: Vec::new(),
                            span: *span,
                        });
                    }
                }
            }
        }
        // Pass 1: collect top-level enum definitions (before function signatures so
        // function parameter/return types can reference enums)
        for stmt in &program.stmts {
            if let Stmt::EnumDef {
                name,
                type_params,
                variants,
                derives,
                span,
            } = stmt
            {
                if lowerer.enum_by_name.contains_key(name) {
                    return Err(LowerError::new(
                        format!("duplicate definition of enum `{name}`"),
                        *span,
                    ));
                }
                if lowerer.struct_by_name.contains_key(name) {
                    return Err(LowerError::new(
                        format!("`{name}` is already defined as a struct"),
                        *span,
                    ));
                }
                if lowerer.func_by_name.contains_key(name) {
                    return Err(LowerError::new(
                        format!("`{name}` is already defined as a function"),
                        *span,
                    ));
                }
                // Validate generic parameter names
                for tp in type_params {
                    if matches!(tp.as_str(), "i32" | "i64" | "f32" | "f64" | "char" | "bool" | "str") {
                        return Err(LowerError::new(
                            format!("generic type parameter `{tp}` collides with a builtin type name"),
                            *span,
                        ));
                    }
                }
                // Lower payload types in the enum's generic-parameter context
                let mut hir_variants = Vec::new();
                let mut seen = std::collections::HashSet::new();
                let saved_gen = std::mem::take(&mut lowerer.gen_params);
                lowerer.gen_params = type_params.clone();
                for v in variants {
                    if !seen.insert(v.name.clone()) {
                        lowerer.gen_params = saved_gen;
                        return Err(LowerError::new(
                            format!("duplicate variant `{}` in enum `{name}`", v.name),
                            v.span,
                        ));
                    }
                    let payload = match &v.payload {
                        Some(t) => Some(lowerer.lower_type(t)?),
                        None => None,
                    };
                    hir_variants.push((v.name.clone(), payload));
                }
                lowerer.gen_params = saved_gen;
                let idx = lowerer.enums.len();
                lowerer.enum_by_name.insert(name.clone(), idx);
                lowerer.enums.push(HirEnumDef {
                    name: name.clone(),
                    type_params: type_params.clone(),
                    variants: hir_variants,
                    span: *span,
                });
                // `#[derive(Copy)]` expands to an empty `impl Copy for Name {}`
                for derive in derives {
                    if derive == "Copy" {
                        lowerer.impls.push(HirImplBlock {
                            type_params: type_params.clone(),
                            trait_name: Some("Copy".to_string()),
                            trait_args: Vec::new(),
                            assoc_types: Vec::new(),
                            type_name: name.clone(),
                            methods: Vec::new(),
                            span: *span,
                        });
                    }
                }
            }
        }
        // Pass 1: collect top-level union definitions (before function signatures so
        // function parameter/return types can reference unions)
        for stmt in &program.stmts {
            if let Stmt::UnionDef { name, fields, span } = stmt {
                if lowerer.union_by_name.contains_key(name) {
                    return Err(LowerError::new(
                        format!("duplicate definition of union `{name}`"),
                        *span,
                    ));
                }
                if lowerer.struct_by_name.contains_key(name) {
                    return Err(LowerError::new(
                        format!("`{name}` is already defined as a struct"),
                        *span,
                    ));
                }
                if lowerer.enum_by_name.contains_key(name) {
                    return Err(LowerError::new(
                        format!("`{name}` is already defined as an enum"),
                        *span,
                    ));
                }
                if lowerer.func_by_name.contains_key(name) {
                    return Err(LowerError::new(
                        format!("`{name}` is already defined as a function"),
                        *span,
                    ));
                }
                let mut hir_fields = Vec::new();
                let mut seen = std::collections::HashSet::new();
                for (fname, fty) in fields {
                    if !seen.insert(fname.clone()) {
                        return Err(LowerError::new(
                            format!("duplicate field `{fname}` in union `{name}`"),
                            *span,
                        ));
                    }
                    let ty = lowerer.lower_type(fty)?;
                    hir_fields.push((fname.clone(), ty));
                }
                let idx = lowerer.unions.len();
                lowerer.union_by_name.insert(name.clone(), idx);
                lowerer.unions.push(HirUnionDef {
                    name: name.clone(),
                    fields: hir_fields,
                    span: *span,
                });
            }
        }
        // Pass 1: collect top-level const definitions (Phase P0-3). First pass
        // registers all names so forward references between constants resolve.
        for stmt in &program.stmts {
            if let Stmt::ConstDef { name, ty, .. } = stmt {
                if lowerer.const_by_name.contains_key(name) {
                    return Err(LowerError::new(
                        format!("duplicate definition of const `{name}`"),
                        stmt.span(),
                    ));
                }
                if lowerer.struct_by_name.contains_key(name)
                    || lowerer.enum_by_name.contains_key(name)
                    || lowerer.union_by_name.contains_key(name)
                    || lowerer.func_by_name.contains_key(name)
                {
                    return Err(LowerError::new(
                        format!("`{name}` is already defined as another item"),
                        stmt.span(),
                    ));
                }
                let ann_ty = match ty {
                    Some(t) => lowerer.lower_type(t)?,
                    None => Ty::I64, // inferred below from the value
                };
                let has_ty = ty.is_some();
                let idx = lowerer.consts.len();
                lowerer.const_by_name.insert(name.clone(), idx);
                lowerer.consts.push(HirConstDef {
                    name: name.clone(),
                    ty: ann_ty,
                    has_ty,
                    value: HirExpr::IntLit(0, stmt.span()), // placeholder, filled below
                    span: stmt.span(),
                });
            }
        }
        // Pass 1: collect top-level trait definitions (before function signatures so
        // trait bounds can be validated)
        for stmt in &program.stmts {
            if let Stmt::TraitDef {
                name,
                type_params,
                assoc_types,
                methods,
                span,
                ..
            } = stmt
            {
                if lowerer.trait_by_name.contains_key(name) {
                    return Err(LowerError::new(
                        format!("duplicate definition of trait `{name}`"),
                        *span,
                    ));
                }
                if lowerer.struct_by_name.contains_key(name) {
                    return Err(LowerError::new(
                        format!("`{name}` is already defined as a struct"),
                        *span,
                    ));
                }
                if lowerer.enum_by_name.contains_key(name) {
                    return Err(LowerError::new(
                        format!("`{name}` is already defined as an enum"),
                        *span,
                    ));
                }
                if lowerer.func_by_name.contains_key(name) {
                    return Err(LowerError::new(
                        format!("`{name}` is already defined as a function"),
                        *span,
                    ));
                }
                // Lower method signatures in the trait's generic-parameter context:
                // `RHS`/`Output` resolve as generic params, and `Self` resolves to the
                // marker `Generic("Self")` (replaced with the impl target type later).
                // `Self::Item` resolves to the marker `Ty::Assoc("Item")`.
                let mut hir_methods = Vec::new();
                let mut seen = std::collections::HashSet::new();
                let saved_gen = std::mem::take(&mut lowerer.gen_params);
                lowerer.gen_params = type_params.clone();
                let saved_self = lowerer.self_type.take();
                lowerer.self_type = Some(Ty::Generic("Self".to_string()));
                let saved_assoc = lowerer.self_assoc.take();
                lowerer.self_assoc = Some(
                    assoc_types
                        .iter()
                        .map(|a| (a.clone(), Ty::Assoc(a.clone())))
                        .collect(),
                );
                for m in methods {
                    if !seen.insert(m.name.clone()) {
                        lowerer.gen_params = saved_gen;
                        lowerer.self_type = saved_self;
                        lowerer.self_assoc = saved_assoc;
                        return Err(LowerError::new(
                            format!("duplicate method `{}` in trait `{name}`", m.name),
                            m.span,
                        ));
                    }
                    let mut hir_params = Vec::new();
                    for (pname, pty) in &m.params {
                        let ty = lowerer.lower_type(pty)?;
                        hir_params.push((pname.clone(), ty, pty.span()));
                    }
                    let ret = match &m.ret {
                        Some(t) => Some(lowerer.lower_type(t)?),
                        None => None,
                    };
                    hir_methods.push(HirTraitMethodSig {
                        name: m.name.clone(),
                        params: hir_params,
                        ret,
                        span: m.span,
                    });
                }
                lowerer.gen_params = saved_gen;
                lowerer.self_type = saved_self;
                lowerer.self_assoc = saved_assoc;
                let idx = lowerer.traits.len();
                lowerer.trait_by_name.insert(name.clone(), idx);
                lowerer.traits.push(HirTraitDef {
                    name: name.clone(),
                    type_params: type_params.clone(),
                    assoc_types: assoc_types.clone(),
                    methods: hir_methods,
                    span: *span,
                });
            }
        }
        // Pass 1: collect top-level impl blocks (register methods as functions,
        // build method resolution table)
        let mut fn_bodies: Vec<(String, &[Stmt])> = Vec::new();
        for stmt in &program.stmts {
            if let Stmt::ImplBlock {
                type_params: impl_type_params,
                trait_name,
                trait_args,
                type_name,
                assoc_types,
                methods,
                span,
            } = stmt
            {
                // Validate target type exists
                if !lowerer.struct_by_name.contains_key(type_name)
                    && !lowerer.enum_by_name.contains_key(type_name)
                {
                    return Err(LowerError::new(
                        format!("impl target type `{type_name}` is not a defined struct or enum"),
                        *span,
                    ));
                }
                // The impl's module context (derived from its mangled target type)
                // so `trait_name` and associated types resolve within the module.
                let saved_module =
                    std::mem::replace(&mut lowerer.module_path, Self::module_path_of(type_name));
                // Resolve the trait name to its mangled form for lookups below.
                let resolved_trait = trait_name.as_ref().map(|tn| {
                    lowerer
                        .name_candidates(tn)
                        .into_iter()
                        .find(|c| lowerer.trait_by_name.contains_key(c))
                        .unwrap_or_else(|| tn.clone())
                });
                // A generic impl (`impl<T> Vec<T>`) requires the target type to be
                // generic with matching parameter count.
                if !impl_type_params.is_empty() {
                    let target_is_generic = {
                        let is_generic_struct = lowerer
                            .struct_by_name
                            .get(type_name)
                            .map(|&i| !lowerer.structs[i].type_params.is_empty())
                            .unwrap_or(false);
                        let is_generic_enum = lowerer
                            .enum_by_name
                            .get(type_name)
                            .map(|&i| !lowerer.enums[i].type_params.is_empty())
                            .unwrap_or(false);
                        is_generic_struct || is_generic_enum
                    };
                    if !target_is_generic {
                        return Err(LowerError::new(
                            format!("impl `impl<{}> {type_name}` but `{type_name}` is not generic", impl_type_params.join(", ")),
                            *span,
                        ));
                    }
                }
                // If trait_name is present, validate trait exists and trait args count
                let mut trait_type_params: Vec<String> = Vec::new();
                let mut hir_trait_args: Vec<Ty> = Vec::new();
                if let Some(tn) = resolved_trait.as_ref() {
                    if !lowerer.trait_by_name.contains_key(tn) {
                        return Err(LowerError::new(
                            format!("trait `{tn}` is not defined"),
                            *span,
                        ));
                    }
                    let trait_idx = lowerer.trait_by_name[tn];
                    trait_type_params = lowerer.traits[trait_idx].type_params.clone();
                    // Lower the trait's concrete type arguments (`impl Add<i64, Complex> for Complex`)
                    // in the impl's generic context so they may reference impl type params.
                    let saved_gen = std::mem::take(&mut lowerer.gen_params);
                    lowerer.gen_params = impl_type_params.clone();
                    for a in trait_args {
                        hir_trait_args.push(lowerer.lower_type(a)?);
                    }
                    lowerer.gen_params = saved_gen;
                    if hir_trait_args.len() != trait_type_params.len() {
                        return Err(LowerError::new(
                            format!(
                                "trait `{tn}` takes {} type argument(s), but {} were given",
                                trait_type_params.len(),
                                hir_trait_args.len()
                            ),
                            *span,
                        ));
                    }
                    // Check for duplicate impl (same trait for same type)
                    for existing in &lowerer.impls {
                        if existing.trait_name.as_deref() == Some(tn.as_str())
                            && existing.type_name == *type_name
                        {
                            return Err(LowerError::new(
                                format!("trait `{tn}` is already implemented for `{type_name}`"),
                                *span,
                            ));
                        }
                    }
                    // Verify every trait associated type is bound (and no extras).
                    let trait_idx = lowerer.trait_by_name[tn];
                    let trait_assoc: &[String] = &lowerer.traits[trait_idx].assoc_types;
                    for a in trait_assoc {
                        if !assoc_types.iter().any(|(n, _)| n == a) {
                            return Err(LowerError::new(
                                format!(
                                    "trait `{tn}` requires associated type `type {a};` but impl for `{type_name}` does not bind it"
                                ),
                                *span,
                            ));
                        }
                    }
                    for (an, _) in assoc_types {
                        if !trait_assoc.iter().any(|a| a == an) {
                            return Err(LowerError::new(
                                format!(
                                    "impl for `{type_name}` binds associated type `type {an}` but trait `{tn}` does not declare it"
                                ),
                                *span,
                            ));
                        }
                    }
                }
                // Lower the impl's associated-type bindings (`type Item = i64;`) in the
                // impl's generic context; used to resolve `Self::Item` in method sigs.
                let mut hir_impl_assoc: Vec<(String, Ty)> = Vec::new();
                let saved_gen = std::mem::take(&mut lowerer.gen_params);
                lowerer.gen_params = impl_type_params.clone();
                for (an, aty) in assoc_types {
                    let t = lowerer.lower_type(aty)?;
                    if hir_impl_assoc.iter().any(|(n, _)| n == an) {
                        lowerer.gen_params = saved_gen;
                        return Err(LowerError::new(
                            format!("duplicate associated type binding `type {an};` in impl for `{type_name}`"),
                            *span,
                        ));
                    }
                    hir_impl_assoc.push((an.clone(), t));
                }
                lowerer.gen_params = saved_gen;
                // Register each method as a function with mangled name. Methods of a
                // generic impl are themselves generic functions (type_params = the
                // impl's type params); they monomorphize per concrete instance.
                let mut hir_methods = Vec::new();
                for m_stmt in methods {
                    if let Stmt::FnDef {
                        name: mname,
                        type_params,
                        trait_bounds: _,
                        lifetimes: _,
                        params,
                        ret,
                        body,
                        is_gpu,
                        is_const: _,
                        is_extern,
                        extern_symbol: _,
                        span: m_span,
                    } = m_stmt
                    {
                        // Methods cannot declare their own type params (the impl's
                        // header owns them) and cannot be extern
                        if !type_params.is_empty() {
                            return Err(LowerError::new(
                                format!("impl method `{mname}` cannot have generic type parameters (declare them on the impl: `impl<T> {type_name}<T>`)"),
                                *m_span,
                            ));
                        }
                        if *is_extern {
                            return Err(LowerError::new(
                                format!("impl method `{mname}` cannot be extern"),
                                *m_span,
                            ));
                        }
                        // Mangled function name: {type_name}__{method_name}
                        let mangled = format!("{type_name}__{mname}");
                        if lowerer.func_by_name.contains_key(&mangled) {
                            return Err(LowerError::new(
                                format!("duplicate method `{mname}` on type `{type_name}`"),
                                *m_span,
                            ));
                        }
                        // Lower params and return type in the impl's generic context,
                        // with `Self` resolved to the impl target type.
                        let self_ty = if impl_type_params.is_empty() {
                            if lowerer.struct_by_name.contains_key(type_name) {
                                Ty::Struct(type_name.clone())
                            } else {
                                Ty::Enum(type_name.clone())
                            }
                        } else {
                            let args: Vec<Ty> = impl_type_params
                                .iter()
                                .map(|p| Ty::Generic(p.clone()))
                                .collect();
                            if lowerer.struct_by_name.contains_key(type_name) {
                                Ty::StructGeneric {
                                    name: type_name.clone(),
                                    args,
                                }
                            } else {
                                Ty::EnumGeneric {
                                    name: type_name.clone(),
                                    args,
                                }
                            }
                        };
                        let mut hir_params = Vec::new();
                        let saved_gen = std::mem::take(&mut lowerer.gen_params);
                        lowerer.gen_params = impl_type_params.clone();
                        let saved_self = lowerer.self_type.take();
                        lowerer.self_type = Some(self_ty.clone());
                        let saved_assoc = lowerer.self_assoc.take();
                        lowerer.self_assoc = Some(hir_impl_assoc.clone());
                        for (pname, pty) in params {
                            let ty = lowerer.lower_type(pty)?;
                            hir_params.push((pname.clone(), ty, pty.span()));
                        }
                        let ret_ty = match ret {
                            Some(t) => Some(lowerer.lower_type(t)?),
                            None => None,
                        };
                        lowerer.gen_params = saved_gen;
                        lowerer.self_type = saved_self;
                        lowerer.self_assoc = saved_assoc;
                        // References are now allowed as return types (Phase 10: the
                        // borrow checker validates the returned reference derives from a
                        // parameter). Pointers/arenas remain forbidden as return types.
                        if let Some(rt) = &ret_ty {
                            if !(rt.is_borrowable() || matches!(rt, Ty::Ref { .. })) {
                                return Err(LowerError::new(
                                    format!("method `{mname}` cannot return type `{rt}` (pointers/arenas are forbidden as return types)"),
                                    *m_span,
                                ));
                            }
                        }
                        let def_id = lowerer.funcs.len() as DefId;
                        lowerer.func_by_name.insert(mangled.clone(), def_id);
                        lowerer.method_map.insert((type_name.clone(), mname.clone()), def_id);
                        lowerer.funcs.push(FuncSig {
                            name: mangled.clone(),
                            def_id,
                            type_params: impl_type_params.clone(),
                            lifetimes: Vec::new(),
                            trait_bounds: Vec::new(),
                            params: hir_params,
                            ret: ret_ty.clone(),
                            is_gpu: *is_gpu,
                            is_const: false,
                            is_extern: false,
                            extern_symbol: None,
                            builtin: false,
                            span: *m_span,
                        });
                        fn_bodies.push((mangled.clone(), body));
                        hir_methods.push(HirFn {
                            name: mangled,
                            def_id,
                            type_params: impl_type_params.clone(),
                            lifetimes: Vec::new(),
                            trait_bounds: Vec::new(),
                            params: Vec::new(), // filled in pass 2
                            param_defs: Vec::new(),
                            ret: ret_ty,
                            is_gpu: *is_gpu,
                            is_const: false,
                            is_extern: false,
                            extern_symbol: None,
                            builtin: false,
                            body: HirBlock { stmts: Vec::new(), scope_id: 0 }, // filled in pass 2
                            span: *m_span,
                        });
                    } else {
                        return Err(LowerError::new(
                            "only function definitions are allowed in impl blocks",
                            *span,
                        ));
                    }
                }
                // If trait_name is present, verify all trait methods are implemented.
                // Trait signatures use `Generic("Self")` and the trait's type params;
                // substitute them with the impl target type and the concrete trait args
                // before comparing with the impl method signatures.
                let mut trait_subst: std::collections::HashMap<String, Ty> = std::collections::HashMap::new();
                if let Some(tn) = resolved_trait.as_ref() {
                    for (tp, arg) in trait_type_params.iter().zip(hir_trait_args.iter()) {
                        trait_subst.insert(tp.clone(), arg.clone());
                    }
                    let self_ty = if impl_type_params.is_empty() {
                        if lowerer.struct_by_name.contains_key(type_name) {
                            Ty::Struct(type_name.clone())
                        } else {
                            Ty::Enum(type_name.clone())
                        }
                    } else {
                        let args: Vec<Ty> = impl_type_params
                            .iter()
                            .map(|p| Ty::Generic(p.clone()))
                            .collect();
                        if lowerer.struct_by_name.contains_key(type_name) {
                            Ty::StructGeneric { name: type_name.clone(), args }
                        } else {
                            Ty::EnumGeneric { name: type_name.clone(), args }
                        }
                    };
                    trait_subst.insert("Self".to_string(), self_ty);
                    // Associated types: `Self::Item` in the trait signature resolves to
                    // the impl's binding (`type Item = i64;`).
                    for (an, aty) in &hir_impl_assoc {
                        trait_subst.insert(an.clone(), aty.clone());
                    }
                    let trait_idx = lowerer.trait_by_name[tn];
                    let trait_def = &lowerer.traits[trait_idx];
                    for trait_method in &trait_def.methods {
                        let found = hir_methods.iter().any(|m| {
                            let mangled_name = &m.name;
                            let suffix = format!("__{}", trait_method.name);
                            mangled_name.ends_with(&suffix)
                        });
                        if !found {
                            return Err(LowerError::new(
                                format!("trait `{tn}` method `{}` is not implemented for `{type_name}`",
                                    trait_method.name),
                                *span,
                            ));
                        }
                        // Verify signature matches (param count and types)
                        let impl_method = hir_methods.iter().find(|m| {
                            m.name == format!("{type_name}__{}", trait_method.name)
                        }).unwrap();
                        let impl_sig = &lowerer.funcs[impl_method.def_id as usize];
                        if impl_sig.params.len() != trait_method.params.len() {
                            return Err(LowerError::new(
                                format!("method `{}` has {} parameters but trait `{tn}` declares {}",
                                    trait_method.name, impl_sig.params.len(), trait_method.params.len()),
                                *span,
                            ));
                        }
                        for (i, ((_, impl_ty, _), (_, trait_ty, _))) in
                            impl_sig.params.iter().zip(trait_method.params.iter()).enumerate()
                        {
                            let expected = subst_ty(trait_ty, &trait_subst);
                            if *impl_ty != expected {
                                return Err(LowerError::new(
                                    format!("method `{}` parameter {} type mismatch: expected `{}`, found `{}`",
                                        trait_method.name, i, expected, impl_ty),
                                    *span,
                                ));
                            }
                        }
                        let expected_ret = trait_method
                            .ret
                            .as_ref()
                            .map(|t| subst_ty(t, &trait_subst));
                        if impl_sig.ret != expected_ret {
                            return Err(LowerError::new(
                                format!("method `{}` return type mismatch: expected `{}`, found `{}`",
                                    trait_method.name,
                                    expected_ret.as_ref().map(|t| t.to_string()).unwrap_or("void".to_string()),
                                    impl_sig.ret.as_ref().map(|t| t.to_string()).unwrap_or("void".to_string())),
                                *span,
                            ));
                        }
                    }
                }
                lowerer.impls.push(HirImplBlock {
                    type_params: impl_type_params.clone(),
                    trait_name: resolved_trait.clone(),
                    trait_args: hir_trait_args,
                    assoc_types: hir_impl_assoc,
                    type_name: type_name.clone(),
                    methods: hir_methods,
                    span: *span,
                });
                lowerer.module_path = saved_module;
            }
        }
        // Pass 1: collect top-level function signatures (forward calls and recursion)
        for stmt in &program.stmts {
            if let Stmt::FnDef {
                name,
                type_params,
                lifetimes,
                trait_bounds,
                params,
                ret,
                is_gpu,
                is_const,
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
                if lowerer.enum_by_name.contains_key(name) {
                    return Err(LowerError::new(
                        format!("`{name}` is already defined as an enum"),
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
                // Validate trait bounds: each bound's trait must exist and the type param must be declared
                for (tp, tn) in trait_bounds {
                    if !type_params.contains(tp) {
                        return Err(LowerError::new(
                            format!("trait bound references unknown type parameter `{tp}`"),
                            *span,
                        ));
                    }
                    let saved_module = std::mem::replace(
                        &mut lowerer.module_path,
                        Self::module_path_of(name),
                    );
                    let found = lowerer
                        .name_candidates(tn)
                        .iter()
                        .any(|c| lowerer.trait_by_name.contains_key(c));
                    lowerer.module_path = saved_module;
                    if !found {
                        return Err(LowerError::new(
                            format!("trait `{tn}` is not defined"),
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
                    // References are now allowed as return types (Phase 10: the borrow
                    // checker validates the returned reference derives from a parameter).
                    // Pointers/arenas remain forbidden (prevents dangling pointers escaping).
                    if !(rt.is_borrowable() || matches!(rt, Ty::Ref { .. })) {
                        return Err(LowerError::new(
                            format!("function `{name}` cannot return type `{rt}` (pointers/arenas are forbidden as return types)"),
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
                    lifetimes: lifetimes.clone(),
                    trait_bounds: trait_bounds.clone(),
                    params: hir_params,
                    ret: ret_ty,
                    is_gpu: *is_gpu,
                    is_const: *is_const,
                    is_extern: *is_extern,
                    extern_symbol: extern_symbol.clone(),
                    builtin: false,
                    span: *span,
                });
                fn_bodies.push((name.clone(), stmt_body(stmt)));
            }
        }
        // Const value pass: lower each const's value expression. Runs *after* the
        // top-level function signatures are collected, so a const initialized from a
        // const fn call (e.g. `const v: i64 = triple(5);`) can resolve the callee.
        // Const references between consts resolve because names were registered in the
        // earlier const-definition pass.
        for stmt in &program.stmts {
            if let Stmt::ConstDef { name, value, span, .. } = stmt {
                let idx = *lowerer
                    .const_by_name
                    .get(name)
                    .ok_or_else(|| LowerError::new(format!("internal: const `{name}` not registered"), *span))?;
                let v = lowerer.lower_expr(value)?;
                lowerer.consts[idx].value = v;
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
            Vec<String>,
            Vec<(String, String)>,
            Vec<(String, Ty, Span)>,
            Option<Ty>,
            bool,
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
                    s.lifetimes.clone(),
                    s.trait_bounds.clone(),
                    s.params.clone(),
                    s.ret.clone(),
                    s.is_gpu,
                    s.is_const,
                    s.is_extern,
                    s.extern_symbol.clone(),
                    s.builtin,
                    s.span,
                )
            })
            .collect();
        let mut hir_funcs = Vec::new();
        for (name, def_id, type_params, lifetimes, trait_bounds, params, ret, is_gpu, is_const, is_extern, extern_symbol, builtin, span) in
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
                    .find(|(n, _)| *n == name)
                    .map(|(_, body)| *body)
                    .expect("same-name function body collected in pass 1");
                lowerer.gen_params = type_params.clone();
                let saved_module = std::mem::replace(
                    &mut lowerer.module_path,
                    Self::module_path_of(&name),
                );
                let lowered = lowerer.lower_block_stmts(body_ast, Some(FnCtx { ret: ret.clone() }));
                lowerer.module_path = saved_module;
                lowerer.gen_params.clear();
                // Give the body a real scope id (not the placeholder 0 used by
                // `lower_block_stmts`), so borrowck's per-scope moved-set recording
                // does not collide with the main scope (also id 0).
                let mut body = lowered?;
                body.scope_id = lowerer.new_scope();
                body
            };
            lowerer.scopes.pop();
            hir_funcs.push(HirFn {
                name,
                def_id,
                type_params,
                lifetimes,
                trait_bounds,
                params,
                param_defs,
                ret,
                is_gpu,
                is_const,
                is_extern,
                extern_symbol,
                builtin,
                body,
                span,
            });
        }
        Ok(HirProgram {
            funcs: hir_funcs,
            structs: lowerer.structs,
            unions: lowerer.unions,
            consts: lowerer.consts,
            enums: lowerer.enums,
            traits: lowerer.traits,
            impls: lowerer.impls,
            method_map: lowerer.method_map,
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
                "f32" => Ok(Ty::F32),
                "f64" => Ok(Ty::F64),
                "char" => Ok(Ty::Char),
                "bool" => Ok(Ty::Bool),
                "str" => Ok(Ty::Str),
                "String" => Ok(Ty::String),
                // Magic higher-order parameter placeholders used in the stdlib's
                // iterator-algorithm stubs (see std.aero `_filter_impl`, `_map_impl`,
                // `_reduce_impl`). Each one lowers to a concrete `Ty::Fn` so Aero's
                // normal first-class-function pointer type inference, borrow check,
                // and codegen paths work end-to-end without full polymorphism.
                "FnPred" => Ok(Ty::Fn(vec![Ty::I64], Box::new(Ty::Bool))),
                "FnTrans" => Ok(Ty::Fn(vec![Ty::I64], Box::new(Ty::I64))),
                "FnRed" => Ok(Ty::Fn(vec![Ty::I64, Ty::I64], Box::new(Ty::I64))),
                // Legacy alias kept for tests: behaves as the predicate form.
                "Fn" => Ok(Ty::Fn(vec![Ty::I64], Box::new(Ty::Bool))),
                "Self" => match &self.self_type {
                    Some(ty) => Ok(ty.clone()),
                    None => Err(LowerError::new(
                        "`Self` can only be used inside a trait or impl method signature",
                        *span,
                    )),
                },
                _ => {
                    // Current function's generic type parameter names → generic type
                    if self.gen_params.iter().any(|p| p == name) {
                        Ok(Ty::Generic(name.clone()))
                    } else {
                        let resolved = self
                            .name_candidates(name)
                            .into_iter()
                            .find(|c| {
                                self.struct_by_name.contains_key(c)
                                    || self.union_by_name.contains_key(c)
                                    || self.enum_by_name.contains_key(c)
                            })
                            .unwrap_or_else(|| name.clone());
                        if self.struct_by_name.contains_key(&resolved) {
                            // Named struct type
                            Ok(Ty::Struct(resolved))
                        } else if self.union_by_name.contains_key(&resolved) {
                            // Named union type
                            Ok(Ty::Union(resolved))
                        } else if self.enum_by_name.contains_key(&resolved) {
                            // Named enum type
                            Ok(Ty::Enum(resolved))
                        } else {
                            Err(LowerError::new(format!("unknown type `{name}`"), *span))
                        }
                    }
                }
            },
            TypeExpr::Path { root, name, span } => {
                // Qualified associated type reference: `Self::Item`.
                if root != "Self" {
                    return Err(LowerError::new(
                        format!("unsupported qualified type path `{root}::{name}` (only `Self::<assoc>` is supported)"),
                        *span,
                    ));
                }
                match &self.self_assoc {
                    Some(bindings) => bindings
                        .iter()
                        .find(|(n, _)| n == name)
                        .map(|(_, t)| Ok(t.clone()))
                        .unwrap_or_else(|| {
                            Err(LowerError::new(
                                format!("`Self::{name}` is not a declared associated type in this context"),
                                *span,
                            ))
                        }),
                    None => Err(LowerError::new(
                        "`Self::<assoc>` can only be used inside a trait or impl method signature",
                        *span,
                    )),
                }
            }
            TypeExpr::Generic { name, args, span } => {
                // Lower the type arguments first
                let mut arg_tys = Vec::new();
                for a in args {
                    arg_tys.push(self.lower_type(a)?);
                }
                // Native `Vec<T>`: a compiler-provided growable heap vector.
                if name == "Vec" {
                    if arg_tys.len() != 1 {
                        return Err(LowerError::new(
                            format!(
                                "`Vec` takes exactly 1 type argument, but {} were given",
                                arg_tys.len()
                            ),
                            *span,
                        ));
                    }
                    return Ok(Ty::Vec(Box::new(arg_tys.pop().unwrap())));
                }
                // Native `Box<T>`: a compiler-provided heap smart pointer.
                if name == "Box" {
                    if arg_tys.len() != 1 {
                        return Err(LowerError::new(
                            format!(
                                "`Box` takes exactly 1 type argument, but {} were given",
                                arg_tys.len()
                            ),
                            *span,
                        ));
                    }
                    return Ok(Ty::Box(Box::new(arg_tys.pop().unwrap())));
                }
                let resolved_gen = self
                    .name_candidates(name)
                    .into_iter()
                    .find(|c| {
                        self.struct_by_name.contains_key(c) || self.enum_by_name.contains_key(c)
                    })
                    .unwrap_or_else(|| name.clone());
                if self.struct_by_name.contains_key(&resolved_gen) {
                    let idx = self.struct_by_name[&resolved_gen];
                    let def = &self.structs[idx];
                    if def.type_params.is_empty() {
                        return Err(LowerError::new(
                            format!("struct `{name}` is not generic and takes no type arguments"),
                            *span,
                        ));
                    }
                    if def.type_params.len() != arg_tys.len() {
                        return Err(LowerError::new(
                            format!(
                                "struct `{name}` takes {} type argument(s), but {} were given",
                                def.type_params.len(),
                                arg_tys.len()
                            ),
                            *span,
                        ));
                    }
                    Ok(Ty::StructGeneric {
                        name: resolved_gen,
                        args: arg_tys,
                    })
                } else if self.enum_by_name.contains_key(&resolved_gen) {
                    let idx = self.enum_by_name[&resolved_gen];
                    let def = &self.enums[idx];
                    if def.type_params.is_empty() {
                        return Err(LowerError::new(
                            format!("enum `{name}` is not generic and takes no type arguments"),
                            *span,
                        ));
                    }
                    if def.type_params.len() != arg_tys.len() {
                        return Err(LowerError::new(
                            format!(
                                "enum `{name}` takes {} type argument(s), but {} were given",
                                def.type_params.len(),
                                arg_tys.len()
                            ),
                            *span,
                        ));
                    }
                    Ok(Ty::EnumGeneric {
                        name: resolved_gen,
                        args: arg_tys,
                    })
                } else {
                    Err(LowerError::new(
                        format!("unknown generic type `{name}`"),
                        *span,
                    ))
                }
            }
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
            TypeExpr::Ref {
                mut_,
                lifetime,
                inner,
                ..
            } => {
                let inner = self.lower_type(inner)?;
                if !inner.is_borrowable() {
                    return Err(LowerError::new(
                        format!("cannot create a reference to type `{inner}`"),
                        te.span(),
                    ));
                }
                Ok(Ty::Ref {
                    mut_: *mut_,
                    lifetime: lifetime.clone(),
                    inner: Box::new(inner),
                })
            }
            TypeExpr::Ptr(inner, _) => {
                let inner = self.lower_type(inner)?;
                Ok(Ty::Ptr(Box::new(inner)))
            }
            TypeExpr::Dyn { name, span } => {
                // `dyn Trait`: the trait must exist and be a valid trait object.
                if !self.trait_by_name.contains_key(name) {
                    return Err(LowerError::new(
                        format!("`dyn {name}`: `{name}` is not a defined trait"),
                        *span,
                    ));
                }
                Ok(Ty::Dyn {
                    trait_name: name.clone(),
                })
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
            if let Stmt::StructDef { span, .. } = stmt {
                if fn_ctx.is_some() {
                    return Err(LowerError::new("struct definitions cannot be nested inside function bodies", *span));
                }
                continue; // top-level structs were collected in pass 1
            }
            if let Stmt::UnionDef { span, .. } = stmt {
                if fn_ctx.is_some() {
                    return Err(LowerError::new("union definitions cannot be nested inside function bodies", *span));
                }
                continue; // top-level unions were collected in pass 1
            }
            if let Stmt::ConstDef { span, .. } = stmt {
                if fn_ctx.is_some() {
                    return Err(LowerError::new("const definitions cannot be nested inside function bodies", *span));
                }
                continue; // top-level consts were collected in pass 1
            }
            if let Stmt::EnumDef { span, .. } = stmt {
                if fn_ctx.is_some() {
                    return Err(LowerError::new("enum definitions cannot be nested inside function bodies", *span));
                }
                continue; // top-level enums were collected in pass 1
            }
            if let Stmt::TraitDef { span, .. } = stmt {
                if fn_ctx.is_some() {
                    return Err(LowerError::new("trait definitions cannot be nested inside function bodies", *span));
                }
                continue; // top-level traits were collected in pass 1
            }
            if let Stmt::ImplBlock { span, .. } = stmt {
                if fn_ctx.is_some() {
                    return Err(LowerError::new("impl blocks cannot be nested inside function bodies", *span));
                }
                continue; // top-level impls were collected in pass 1
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
                mut_,
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
                    mut_: *mut_,
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
            Stmt::AssignField { target, field, value, span } => {
                let t = self.lower_expr(target)?;
                let v = self.lower_expr(value)?;
                Ok(HirStmt::AssignField {
                    target: Box::new(t),
                    field: field.clone(),
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
            Stmt::Loop { body, span } => {
                let body = self.lower_block(body, fn_ctx)?;
                Ok(HirStmt::Loop { body, span: *span })
            }
            Stmt::Return(value, span) => {
                let v = match value {
                    Some(e) => Some(self.lower_expr(e)?),
                    None => None,
                };
                Ok(HirStmt::Return(v, *span))
            }
            Stmt::For { var, iter, body, span } => {
                let iter = self.lower_expr(iter)?;
                // Create a new scope for the loop body
                let scope_id = self.new_scope();
                self.scopes.push(std::collections::HashMap::new());
                let var_def = self.bind_var(var, *span)?;
                let body = self.lower_block_stmts(body, fn_ctx.clone());
                self.scopes.pop();
                match body {
                    Ok(HirBlock { stmts, .. }) => Ok(HirStmt::For {
                        var: var.clone(),
                        var_def,
                        iter,
                        body: HirBlock { stmts, scope_id },
                        span: *span,
                    }),
                    Err(e) => Err(e),
                }
            }
            Stmt::Break(span) => Ok(HirStmt::Break(*span)),
            Stmt::Continue(span) => Ok(HirStmt::Continue(*span)),
            Stmt::Match { scrutinee, arms, span } => {
                let scrutinee = self.lower_expr(scrutinee)?;
                let mut hir_arms = Vec::new();
                for arm in arms {
                    let pattern = match &arm.pattern {
                        MatchPattern::Wildcard => HirMatchPattern::Wildcard,
                        MatchPattern::IntLit(v) => HirMatchPattern::IntLit(*v),
                        MatchPattern::BoolLit(b) => HirMatchPattern::BoolLit(*b),
                        MatchPattern::CharLit(c) => HirMatchPattern::CharLit(*c),
                        MatchPattern::StrLit(s) => HirMatchPattern::StrLit(s.clone()),
                        MatchPattern::Bind(name) => {
                            // Create a new scope for the arm body and bind the variable
                            let scope_id = self.new_scope();
                            self.scopes.push(std::collections::HashMap::new());
                            let def_id = self.bind_var(name, arm.span)?;
                            let body = self.lower_block_stmts(&arm.body, fn_ctx.clone());
                            self.scopes.pop();
                            let body = match body {
                                Ok(HirBlock { stmts, .. }) => HirBlock { stmts, scope_id },
                                Err(e) => return Err(e),
                            };
                            hir_arms.push(HirMatchArm {
                                pattern: HirMatchPattern::Bind(name.clone(), def_id),
                                body,
                                span: arm.span,
                            });
                            continue;
                        }
                        MatchPattern::EnumVariant {
                            enum_name,
                            variant,
                            bind,
                            span: pat_span,
                        } => {
                            // Resolve the enum type: explicit `Enum::Variant`, or the unique
                            // enum that defines a bare `Variant` (ambiguous names must be explicit).
                            let resolved_enum = match enum_name {
                                Some(n) => {
                                    let resolved = self
                                        .name_candidates(n)
                                        .into_iter()
                                        .find(|c| self.enum_by_name.contains_key(c))
                                        .unwrap_or_else(|| n.clone());
                                    if !self.enum_by_name.contains_key(&resolved) {
                                        return Err(LowerError::new(
                                            format!("undefined enum `{n}`"),
                                            *pat_span,
                                        ));
                                    }
                                    resolved
                                }
                                None => {
                                    let matches: Vec<&str> = self
                                        .enums
                                        .iter()
                                        .filter(|e| e.find_variant(variant).is_some())
                                        .map(|e| e.name.as_str())
                                        .collect();
                                    match matches.len() {
                                        1 => matches[0].to_string(),
                                        0 => {
                                            return Err(LowerError::new(
                                                format!("no enum defines a variant named `{variant}`"),
                                                *pat_span,
                                            ));
                                        }
                                        _ => {
                                            return Err(LowerError::new(
                                                format!(
                                                    "variant `{variant}` is ambiguous (defined by {}); use `Enum::{variant}` to disambiguate",
                                                    matches.join(", ")
                                                ),
                                                *pat_span,
                                            ));
                                        }
                                    }
                                }
                            };
                            // The variant must exist; its payload (if any) decides whether a
                            // binding is required/allowed.
                            let payload = {
                                let idx = self.enum_by_name[&resolved_enum];
                                match self.enums[idx].find_variant(variant) {
                                    Some((_, p)) => p.clone(),
                                    None => {
                                        return Err(LowerError::new(
                                            format!("enum `{resolved_enum}` has no variant `{variant}`"),
                                            *pat_span,
                                        ));
                                    }
                                }
                            };
                            // Create a new scope for the arm body; bind the payload variable
                            let scope_id = self.new_scope();
                            self.scopes.push(std::collections::HashMap::new());
                            let bind_def = match (bind, &payload) {
                                (Some(b), Some(_)) => Some((b.clone(), self.bind_var(b, *pat_span)?)),
                                (None, None) => None,
                                (Some(_), None) => {
                                    self.scopes.pop();
                                    return Err(LowerError::new(
                                        format!("variant `{resolved_enum}::{variant}` has no payload; write the pattern without `(...)`"),
                                        *pat_span,
                                    ));
                                }
                                (None, Some(_)) => {
                                    self.scopes.pop();
                                    return Err(LowerError::new(
                                        format!("variant `{resolved_enum}::{variant}` carries a payload; bind it, e.g. `{variant}(x)`"),
                                        *pat_span,
                                    ));
                                }
                            };
                            let body = self.lower_block_stmts(&arm.body, fn_ctx.clone());
                            self.scopes.pop();
                            let body = match body {
                                Ok(HirBlock { stmts, .. }) => HirBlock { stmts, scope_id },
                                Err(e) => return Err(e),
                            };
                            hir_arms.push(HirMatchArm {
                                pattern: HirMatchPattern::EnumVariant {
                                    enum_name: resolved_enum,
                                    variant: variant.clone(),
                                    bind: bind_def,
                                    span: *pat_span,
                                },
                                body,
                                span: arm.span,
                            });
                            continue;
                        }
                    };
                    // Non-binding patterns: just lower the body in the current scope
                    let scope_id = self.new_scope();
                    self.scopes.push(std::collections::HashMap::new());
                    let body = self.lower_block_stmts(&arm.body, fn_ctx.clone());
                    self.scopes.pop();
                    let body = match body {
                        Ok(HirBlock { stmts, .. }) => HirBlock { stmts, scope_id },
                        Err(e) => return Err(e),
                    };
                    hir_arms.push(HirMatchArm { pattern, body, span: arm.span });
                }
                Ok(HirStmt::Match { scrutinee, arms: hir_arms, span: *span })
            }
            Stmt::FnDef { span, .. } => Err(LowerError::new(
                "function definitions are not allowed in this position",
                *span,
            )),
            Stmt::StructDef { name, span, .. } => Err(LowerError::new(
                format!("struct definition `{name}` is not allowed in this position"),
                *span,
            )),
            Stmt::UnionDef { name, span, .. } => Err(LowerError::new(
                format!("union definition `{name}` is not allowed in this position"),
                *span,
            )),
            Stmt::ConstDef { name, span, .. } => Err(LowerError::new(
                format!("const definition `{name}` is not allowed in this position"),
                *span,
            )),
            Stmt::EnumDef { name, span, .. } => Err(LowerError::new(
                format!("enum definition `{name}` is not allowed in this position"),
                *span,
            )),
            Stmt::TraitDef { name, span, .. } => Err(LowerError::new(
                format!("trait definition `{name}` is not allowed in this position"),
                *span,
            )),
            Stmt::ImplBlock { type_name, span, .. } => Err(LowerError::new(
                format!("impl block for `{type_name}` is not allowed in this position"),
                *span,
            )),
            Stmt::ModDef { name, span, .. } => Err(LowerError::new(
                format!("module `{name}` is not allowed in this position"),
                *span,
            )),
            Stmt::ModFile { name, span, .. } => Err(LowerError::new(
                format!("file module `{name}` is not allowed in this position"),
                *span,
            )),
            Stmt::UseDecl { span, .. } => Err(LowerError::new(
                "`use` is only allowed at the top level of a module",
                *span,
            )),
            Stmt::Pub(_, span) => Err(LowerError::new(
                "`pub` is only allowed at the top level of a module",
                *span,
            )),
        }
    }

    fn lower_expr(&mut self, expr: &Expr) -> Result<HirExpr, LowerError> {
        match expr {
            Expr::Int(v, span) => Ok(HirExpr::IntLit(*v, *span)),
            Expr::Float(v, span) => Ok(HirExpr::FloatLit(*v, *span)),
            Expr::Char(c, span) => Ok(HirExpr::CharLit(*c, *span)),
            Expr::Bool(v, span) => Ok(HirExpr::BoolLit(*v, *span)),
            Expr::Str(s, span) => Ok(HirExpr::StrLit(s.clone(), *span)),
            Expr::Var(name, span) => {
                // A top-level const reference becomes a ConstRef (filled in at
                // compile time by codegen). Prefer locals when a shadowing local
                // exists in scope.
                if !self.is_local(name) {
                    let resolved_const = self
                        .name_candidates(name)
                        .into_iter()
                        .find(|c| self.const_by_name.contains_key(c));
                    if let Some(const_name) = resolved_const {
                        let idx = self.const_by_name[&const_name];
                        let ty = self.consts[idx].ty.clone();
                        return Ok(HirExpr::ConstRef {
                            name: const_name,
                            ty,
                            span: *span,
                        });
                    }
                }
                // A bare function name in value position becomes a first-class
                // function reference `FnRef` (used e.g. to build a function pointer
                // for `filter`/`map`/`reduce`). Checked after locals/consts so a
                // shadowing local takes precedence.
                if !self.is_local(name) {
                    let resolved = self
                        .name_candidates(name)
                        .into_iter()
                        .find(|c| self.func_by_name.contains_key(c));
                    if let Some(func_name) = resolved {
                        let def_id = self.func_by_name[&func_name];
                        return Ok(HirExpr::FnRef {
                            def_id,
                            span: *span,
                        });
                    }
                }
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
            Expr::Try { target, span } => {
                let t = self.lower_expr(target)?;
                Ok(HirExpr::Try {
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
            Expr::TensorLit(dims, elem, span) => {
                // Element type defaults to i64 if not annotated (backward compatible).
                let ty = match elem {
                    Some(te) => self.lower_type(te)?,
                    None => Ty::I64,
                };
                Ok(HirExpr::TensorLit { dims: dims.clone(), elem: ty, span: *span })
            }
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
                // Builtin tensor reductions: sum/mean/max/min(t) → scalar.
                // Only dispatch when called with exactly 1 argument (a tensor);
                // otherwise fall through to regular function call resolution.
                if let Some(op) = reduce_op(callee) {
                    if args.len() == 1 {
                        let input = self.lower_expr(&args[0])?;
                        return Ok(HirExpr::Reduce {
                            op,
                            input: Box::new(input),
                            span: *span,
                        });
                    }
                }
                // Builtin element-wise tensor ops: tensor_add/sub/mul/div(a, b),
                // tensor_neg(a) → tensor of same shape.
                // Only dispatch when the argument count matches; otherwise fall through.
                if let Some(op) = elem_op(callee) {
                    let is_binary = op != ElemOp::Neg;
                    let want = if is_binary { 2 } else { 1 };
                    if args.len() == want {
                        let lhs = self.lower_expr(&args[0])?;
                        let rhs = if is_binary {
                            Some(Box::new(self.lower_expr(&args[1])?))
                        } else {
                            None
                        };
                        return Ok(HirExpr::ElemWise {
                            op,
                            lhs: Box::new(lhs),
                            rhs,
                            span: *span,
                        });
                    }
                }
                // Builtin BLAS Level-1 tensor ops (BLAS binding, CPU backend):
                // blas_dot/blas_nrm2/blas_asum/blas_amax/blas_scal/blas_axpy.
                // Only dispatch when the argument count matches; otherwise fall through.
                if let Some(op) = blas_op(callee) {
                    let want = match op {
                        BlasOp::Dot | BlasOp::Nrm2 | BlasOp::Asum | BlasOp::Amax => {
                            if op == BlasOp::Dot { 2 } else { 1 }
                        }
                        BlasOp::Scal => 2,
                        BlasOp::Axpy => 3,
                    };
                    if args.len() == want {
                        let mut hir_args = Vec::with_capacity(want);
                        for a in args {
                            hir_args.push(self.lower_expr(a)?);
                        }
                        return Ok(HirExpr::Blas {
                            op,
                            args: hir_args,
                            span: *span,
                        });
                    }
                }
                let def_id = match self
                    .name_candidates(callee)
                    .iter()
                    .find_map(|c| self.func_by_name.get(c))
                {
                    Some(&id) => id,
                    None => {
                        // Not a named function: the callee may be a local variable
                        // holding a first-class function pointer (`let f = foo; f(x)`).
                        // Lower to an indirect call through that variable.
                        if self.is_local(callee) {
                            let var_def = self.resolve_var(callee, *span)?;
                            let mut hir_args = Vec::new();
                            for a in args {
                                hir_args.push(self.lower_expr(a)?);
                            }
                            return Ok(HirExpr::CallPtr {
                                callee: Box::new(HirExpr::Var(var_def, *span)),
                                args: hir_args,
                                span: *span,
                            });
                        }
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
            Expr::StructLit { name, fields, span } => {
                // Validate the type exists (a struct or a union; both use the same
                // literal syntax `Name { field: value }`). For a union only one field
                // may be set — enforced during inference.
                let resolved = self
                    .name_candidates(name)
                    .into_iter()
                    .find(|c| self.struct_by_name.contains_key(c) || self.union_by_name.contains_key(c))
                    .unwrap_or_else(|| name.clone());
                if !self.struct_by_name.contains_key(&resolved)
                    && !self.union_by_name.contains_key(&resolved)
                {
                    return Err(LowerError::new(
                        format!("undefined struct or union `{name}`"),
                        *span,
                    ));
                }
                let mut hir_fields = Vec::new();
                let mut seen = std::collections::HashSet::new();
                for (fname, fval) in fields {
                    if !seen.insert(fname.clone()) {
                        return Err(LowerError::new(
                            format!("duplicate field `{fname}` in struct literal `{name}`"),
                            *span,
                        ));
                    }
                    let v = self.lower_expr(fval)?;
                    hir_fields.push((fname.clone(), v));
                }
                Ok(HirExpr::StructLit {
                    name: resolved,
                    fields: hir_fields,
                    span: *span,
                })
            }
            Expr::Field { target, field, span } => {
                let t = self.lower_expr(target)?;
                Ok(HirExpr::Field {
                    target: Box::new(t),
                    field: field.clone(),
                    span: *span,
                })
            }
            Expr::PathCall {
                path,
                args,
                span,
            } => {
                // A two-segment path may be a native (`String`/`Vec`/`Box`) or user-enum
                // constructor `T::Variant(...)`; re-dispatch to the EnumLit lowering.
                if path.len() == 2 {
                    let name = &path[0];
                    let variant = &path[1];
                    let is_native = matches!(name.as_str(), "String" | "Vec" | "Box");
                    let is_enum = self
                        .name_candidates(name)
                        .iter()
                        .any(|c| self.enum_by_name.contains_key(c));
                    if is_native || is_enum {
                        if args.len() > 1 {
                            return Err(LowerError::new(
                                format!("`{name}::{variant}` takes at most one argument"),
                                *span,
                            ));
                        }
                        let enum_expr = Expr::EnumLit {
                            name: name.clone(),
                            variant: variant.clone(),
                            arg: args.first().cloned().map(|a| Box::new(a)),
                            span: *span,
                        };
                        return self.lower_expr(&enum_expr);
                    }
                }
                // Otherwise it is a module-qualified function call `a::b::c(args...)`.
                let mangled = path.join("::");
                let def_id = match self.func_by_name.get(&mangled) {
                    Some(&id) => id,
                    None => {
                        return Err(LowerError::new(
                            format!("undefined function `{mangled}`"),
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
            Expr::EnumLit {
                name,
                variant,
                arg,
                span,
            } => {
                // Native `Vec<T>` construction (`Vec::new` / `Vec::with_cap(n)`): not a
                // user enum, but a compiler-provided heap vector constructor.
                if name == "Vec" {
                    let arg = match arg {
                        Some(a) => Some(Box::new(self.lower_expr(a)?)),
                        None => None,
                    };
                    return Ok(HirExpr::EnumLit {
                        name: name.clone(),
                        variant: variant.clone(),
                        arg,
                        span: *span,
                    });
                }
                // Native `String` construction (`String::new` / `String::with_cap(n)` /
                // `String::from(s)`): a compiler-provided heap string constructor.
                if name == "String" {
                    let arg = match arg {
                        Some(a) => Some(Box::new(self.lower_expr(a)?)),
                        None => None,
                    };
                    return Ok(HirExpr::EnumLit {
                        name: name.clone(),
                        variant: variant.clone(),
                        arg,
                        span: *span,
                    });
                }
                // Native `Box<T>` construction (`Box::new(value)`): a compiler-provided
                // heap smart-pointer constructor.
                if name == "Box" {
                    let arg = match arg {
                        Some(a) => Some(Box::new(self.lower_expr(a)?)),
                        None => None,
                    };
                    return Ok(HirExpr::EnumLit {
                        name: name.clone(),
                        variant: variant.clone(),
                        arg,
                        span: *span,
                    });
                }
                // Validate the enum and variant exist; clone the payload type so the
                // immutable borrow of `self.enums` ends before `lower_expr` needs `&mut self`.
                let resolved_name = self
                    .name_candidates(name)
                    .into_iter()
                    .find(|c| self.enum_by_name.contains_key(c))
                    .unwrap_or_else(|| name.clone());
                let payload = match self.enum_by_name.get(&resolved_name) {
                    Some(&idx) => match self.enums[idx].find_variant(variant) {
                        Some((_, p)) => p.clone(),
                        None => {
                            return Err(LowerError::new(
                                format!("enum `{resolved_name}` has no variant `{variant}`"),
                                *span,
                            ));
                        }
                    },
                    None => {
                        return Err(LowerError::new(
                            format!("undefined enum `{name}`"),
                            *span,
                        ));
                    }
                };
                match (arg, payload) {
                    (Some(a), Some(_)) => {
                        let a = self.lower_expr(a)?;
                        Ok(HirExpr::EnumLit {
                            name: resolved_name.clone(),
                            variant: variant.clone(),
                            arg: Some(Box::new(a)),
                            span: *span,
                        })
                    }
                    (None, None) => Ok(HirExpr::EnumLit {
                        name: resolved_name,
                        variant: variant.clone(),
                        arg: None,
                        span: *span,
                    }),
                    (None, Some(_)) => Err(LowerError::new(
                        format!("variant `{resolved_name}::{variant}` carries a payload; use `{resolved_name}::{variant}(...)`"),
                        *span,
                    )),
                    (Some(_), None) => Err(LowerError::new(
                        format!("variant `{resolved_name}::{variant}` has no payload; pass no argument"),
                        *span,
                    )),
                }
            }
            Expr::Cast { target, ty, span } => {
                let target = self.lower_expr(target)?;
                let ty = self.lower_type(ty)?;
                Ok(HirExpr::Cast {
                    target: Box::new(target),
                    ty,
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

    /// Whether `name` is bound as a local variable in any active scope.
    fn is_local(&self, name: &str) -> bool {
        self.scopes.iter().rev().any(|s| s.contains_key(name))
    }

    /// The module path component of a mangled name (`a::b::foo` → `a::b`,
    /// `foo` → `""`).
    fn module_path_of(mangled: &str) -> String {
        match mangled.rfind("::") {
            Some(idx) => mangled[..idx].to_string(),
            None => String::new(),
        }
    }

    /// Candidate names to try when resolving a bare `name` from within the
    /// current module, in priority order: qualified same-module name, the bare
    /// name itself, then any `use`-imported alias whose last segment matches.
    fn name_candidates(&self, name: &str) -> Vec<String> {
        let mut v = Vec::new();
        if !self.module_path.is_empty() {
            v.push(format!("{}::{name}", self.module_path));
        }
        v.push(name.to_string());
        if let Some(uses) = self.uses.get(&self.module_path) {
            for path in uses {
                if let Some(last) = path.last() {
                    if last == name {
                        let target = path.join("::");
                        if !v.contains(&target) {
                            v.push(target);
                        }
                    }
                }
            }
        }
        v
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
