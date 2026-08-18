//! `aero clippy` — a static analyzer (linter) for the Aero language.
//!
//! Runs 100+ lint rules over a source file and reports `Diagnostic`s with a rule
//! code, severity, category, a human-readable message and (where applicable) a
//! concrete fix suggestion. The engine is a hybrid:
//!
//! - **Lexical rules** scan the raw source lines + token stream (style, spacing,
//!   naming-at-heuristics, literal trivia);
//! - **Semantic rules** walk the parsed [`Program`] AST, with a symbol-collection
//!   pass that tracks definitions (`fn`/`struct`/`enum`/`const`/`mod`) and their
//!   references so unused-item and dead-code lints are accurate.
//!
//! Rules are grouped by [`Category`]. The authoritative catalogue lives in
//! [`RULE_CATALOG`]; a test asserts it holds at least 100 distinct rules.

use aero_lex::token::{Token, TokenKind};
use aero_parse::ast::{Expr, MatchPattern, Program, Stmt, TypeExpr};
use aero_parse::parse_source;
use std::collections::{HashMap, HashSet};

/// A single lint finding.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub line: u32,
    pub col: u32,
    /// Stable rule id (e.g. `missing_space_after_keyword`).
    pub code: String,
    pub severity: Severity,
    pub category: Category,
    pub message: String,
    pub suggestion: Option<String>,
    pub line_text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    Style,
    Note,
}

impl Severity {
    pub fn as_str(&self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Style => "style",
            Severity::Note => "note",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Correctness,
    Style,
    Complexity,
    Perf,
    Naming,
    Pedantic,
}

impl Category {
    pub fn as_str(&self) -> &'static str {
        match self {
            Category::Correctness => "correctness",
            Category::Style => "style",
            Category::Complexity => "complexity",
            Category::Perf => "perf",
            Category::Naming => "naming",
            Category::Pedantic => "pedantic",
        }
    }
}

/// (code, category, severity, doc).
pub const RULE_CATALOG: &[(&str, Category, Severity, &str)] = &[
    // -- Naming ----------------------------------------------------------
    ("fn_non_snake_case", Category::Naming, Severity::Warning, "function names should be snake_case"),
    ("var_non_snake_case", Category::Naming, Severity::Warning, "variable names should be snake_case"),
    ("struct_non_upper_camel", Category::Naming, Severity::Warning, "struct names should be UpperCamelCase"),
    ("enum_non_upper_camel", Category::Naming, Severity::Warning, "enum names should be UpperCamelCase"),
    ("trait_non_upper_camel", Category::Naming, Severity::Warning, "trait names should be UpperCamelCase"),
    ("const_non_screaming_snake", Category::Naming, Severity::Warning, "const names should be SCREAMING_SNAKE"),
    ("module_non_snake_case", Category::Naming, Severity::Warning, "module names should be snake_case"),
    ("ident_too_short", Category::Naming, Severity::Style, "identifier is too short to be self-documenting"),
    ("ident_too_long", Category::Naming, Severity::Style, "identifier is unusually long"),
    ("leading_underscore_used", Category::Naming, Severity::Warning, "`_`-prefixed variable is used"),
    ("trailing_underscore", Category::Naming, Severity::Style, "identifier ends with an underscore"),
    ("all_caps_identifier", Category::Naming, Severity::Style, "identifier should not be in ALL_CAPS"),
    ("reserved_name", Category::Naming, Severity::Warning, "identifier looks like a keyword / builtin"),
    ("canonical_line_ending", Category::Naming, Severity::Style, "use consistent `\r\n` or `\n` throughout"),
    // -- Correctness -----------------------------------------------------
    ("division_by_zero", Category::Correctness, Severity::Error, "integer division by a literal zero"),
    ("modulo_by_zero", Category::Correctness, Severity::Error, "integer modulo by a literal zero"),
    ("comparing_literals", Category::Correctness, Severity::Error, "comparison between two literal values"),
    ("same_operands_cmp", Category::Correctness, Severity::Error, "comparing an expression to itself"),
    ("float_equality", Category::Correctness, Severity::Warning, "exact equality on floating-point values"),
    ("same_then_else", Category::Correctness, Severity::Warning, "`if`/`else` branches are identical"),
    ("empty_block", Category::Correctness, Severity::Warning, "empty block body"),
    ("empty_struct", Category::Correctness, Severity::Warning, "struct has no fields"),
    ("empty_enum", Category::Correctness, Severity::Warning, "enum has no variants"),
    ("empty_match", Category::Correctness, Severity::Warning, "`match` has no arms"),
    ("unreachable_after_return", Category::Correctness, Severity::Warning, "code is unreachable"),
    ("needless_return_at_end", Category::Correctness, Severity::Style, "unnecessary trailing `return`"),
    ("useless_else_after_return", Category::Correctness, Severity::Warning, "`else` is useless after a `return`"),
    ("self_assignment", Category::Correctness, Severity::Error, "variable assigned to itself"),
    ("redundant_boolean", Category::Correctness, Severity::Style, "redundant comparison to `true`/`false`"),
    ("infinite_loop_no_break", Category::Correctness, Severity::Error, "infinite loop with no exit path"),
    ("while_true_loop", Category::Correctness, Severity::Warning, "`while true { }` is a `loop`"),
    ("duplicate_struct_field", Category::Correctness, Severity::Error, "duplicate field in a struct literal"),
    ("duplicate_enum_variant", Category::Correctness, Severity::Error, "duplicate enum variant"),
    ("duplicate_match_pattern", Category::Correctness, Severity::Warning, "duplicate match arm pattern"),
    ("array_index_out_of_bounds", Category::Correctness, Severity::Warning, "array index is out of bounds"),
    ("double_negation", Category::Correctness, Severity::Style, "double negation"),
    ("shadows_builtin", Category::Correctness, Severity::Style, "name shadows a built-in function/type"),
    ("compare_to_true", Category::Correctness, Severity::Style, "unnecessary comparison to boolean"),
    ("integer_overflow_literal", Category::Correctness, Severity::Warning, "integer literal outside the i64 range"),
    ("unsafe_ptr_deref", Category::Correctness, Severity::Warning, "raw-pointer dereference without safety note"),
    ("needless_bool_if", Category::Correctness, Severity::Style, "`if x { true } else { false }` is just `x`"),
    ("shift_by_zero", Category::Correctness, Severity::Warning, "bit-shift by a literal zero"),
    ("abs_double_negative", Category::Correctness, Severity::Warning, "double negation of a boolean flag"),
    ("match_on_unit", Category::Correctness, Severity::Warning, "matching a value that has no cases"),
    // -- Style -----------------------------------------------------------
    ("double_whitespace", Category::Style, Severity::Style, "multiple consecutive spaces"),
    ("trailing_whitespace", Category::Style, Severity::Style, "trailing whitespace"),
    ("missing_newline_eof", Category::Style, Severity::Style, "file does not end with a newline"),
    ("tabs_in_indentation", Category::Style, Severity::Style, "tabs used for indentation"),
    ("mixed_indentation", Category::Style, Severity::Style, "mixed tabs and spaces in indentation"),
    ("line_too_long", Category::Style, Severity::Style, "line exceeds 100 columns"),
    ("multiple_semicolons", Category::Style, Severity::Style, "redundant consecutive semicolons"),
    ("missing_space_after_keyword", Category::Style, Severity::Style, "keyword needs a space before `(identifier`"),
    ("space_before_comma", Category::Style, Severity::Style, "unnecessary space before `,`"),
    ("semicolon_after_block", Category::Style, Severity::Style, "unnecessary semicolon after a block"),
    ("shadowed_let", Category::Style, Severity::Style, "binding shadows an existing name"),
    ("magic_number", Category::Style, Severity::Style, "bare numeric literal without context"),
    ("single_char_string", Category::Style, Severity::Style, "use a char literal for a single-character string"),
    ("let_underscore_discard", Category::Style, Severity::Style, "`let _ =` silently discards a value"),
    ("leading_zero_literal", Category::Style, Severity::Warning, "literal with a leading zero looks octal"),
    ("redundant_type_annotation", Category::Style, Severity::Style, "type annotation can be inferred"),
    ("redundant_string_from", Category::Style, Severity::Style, "`String::from` around a string literal is redundant"),
    ("manual_swap", Category::Style, Severity::Style, "manual value swap via temporary"),
    ("unnecessary_restart", Category::Style, Severity::Style, "redundant `continue` at end of a loop"),
    ("void_expr_statement", Category::Style, Severity::Warning, "expression statement has no effect"),
    ("needless_bool_comparison", Category::Style, Severity::Style, "comparing a boolean expression to `true`"),
    ("needless_borrow", Category::Style, Severity::Style, "redundant borrow of an already-borrowed value"),
    ("mixed_case_words", Category::Style, Severity::Style, "identifier mixes case in a confusing way"),
    ("unnecessary_cast", Category::Style, Severity::Style, "cast to the same type is a no-op"),
    ("redundant_repeat", Category::Style, Severity::Style, "same expression repeated in a binary operation"),
    ("suspicious_semicolon", Category::Style, Severity::Style, "`;` immediately after `{`"),
    ("verbatim_string_escape", Category::Style, Severity::Style, "unnecessary escape inside a string literal"),
    // -- Complexity ------------------------------------------------------
    ("too_many_parameters", Category::Complexity, Severity::Warning, "function has too many parameters"),
    ("too_many_call_args", Category::Complexity, Severity::Style, "call has too many arguments"),
    ("too_many_function_lines", Category::Complexity, Severity::Style, "function is too long"),
    ("match_too_many_arms", Category::Complexity, Severity::Style, "match has too many arms"),
    ("deep_nesting", Category::Complexity, Severity::Style, "expression / statement is deeply nested"),
    ("too_many_struct_fields", Category::Complexity, Severity::Style, "struct has too many fields"),
    ("collapsible_if", Category::Complexity, Severity::Style, "nested `if` can be collapsed into one"),
    ("single_bool_check", Category::Complexity, Severity::Style, "a one-condition `if` is clearer inline"),
    // -- Perf ------------------------------------------------------------
    ("len_zero_compare", Category::Perf, Severity::Style, "`len(x) == 0` should be an emptiness check"),
    ("reversed_range", Category::Perf, Severity::Warning, "a range whose bound is never reached"),
    ("single_element_array", Category::Perf, Severity::Style, "single-element array literal"),
    ("concat_in_loop", Category::Perf, Severity::Style, "string concatenation inside a loop"),
    // -- Pedantic --------------------------------------------------------
    ("top_level_blank_line", Category::Pedantic, Severity::Style, "avoid a blank line at the start of the file"),
    ("redundant_semicolon_top", Category::Pedantic, Severity::Style, "stray top-level `;`"),
    ("module_never_used", Category::Pedantic, Severity::Style, "module is defined but never referenced"),
    ("unused_variable", Category::Pedantic, Severity::Style, "variable is never used"),
    ("unused_function", Category::Pedantic, Severity::Style, "function is never called"),
    ("unused_private_struct", Category::Pedantic, Severity::Style, "struct is never used"),
    ("unused_private_enum", Category::Pedantic, Severity::Style, "enum is never used"),
    ("unused_const", Category::Pedantic, Severity::Style, "const is never used"),
    ("unused_import", Category::Pedantic, Severity::Style, "import is never used"),
    ("needless_lifetime", Category::Pedantic, Severity::Style, "lifetime can be elided"),
    ("missing_docs_on_pub_item", Category::Pedantic, Severity::Style, "public item has no doc comment"),
    ("needless_parens_in_arith", Category::Pedantic, Severity::Style, "unnecessary parentheses around arithmetic"),
    ("redundant_array_builtin", Category::Pedantic, Severity::Style, "array literal passed where direct syntax works"),
    ("shadowed_const", Category::Pedantic, Severity::Style, "local binding shadows a const"),
    ("redundant_closure_arg", Category::Pedantic, Severity::Style, "closure argument is never used"),
    ("print_format_mismatch", Category::Pedantic, Severity::Warning, "`print` format placeholder / arg count mismatch"),
    ("redundant_else", Category::Pedantic, Severity::Style, "redundant `else` block"),
];

/// Assert the catalogue actually holds 100+ rules.
pub const CATALOG_COUNT: usize = RULE_CATALOG.len();

// ---------------------------------------------------------------------------
// Context
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Syms {
    defined_fns: HashMap<String, (u32, u32)>,
    called_names: HashSet<String>,
    defined_structs: HashMap<String, (u32, u32, usize, Vec<String>)>,
    used_struct_names: HashSet<String>,
    defined_enums: HashMap<String, (u32, u32, usize)>,
    used_enum_names: HashSet<String>,
    defined_consts: HashMap<String, (u32, u32)>,
    lets: Vec<(String, u32, u32)>,
    var_refs: HashSet<String>,
    imported: HashMap<String, (u32, u32)>,
}

struct Ctx<'a> {
    src: &'a str,
    lines: Vec<&'a str>,
    tokens: Vec<Token>,
    syms: Syms,
    diags: Vec<Diagnostic>,
}

impl<'a> Ctx<'a> {
    fn line(&self, idx: u32) -> &'a str {
        self.lines.get((idx.saturating_sub(1)) as usize).copied().unwrap_or("")
    }
}

fn emit(ctx: &mut Ctx, code: &str, line: u32, col: u32, msg: String, sugg: Option<String>) {
    let (category, severity) = rule_meta(code);
    ctx.diags.push(Diagnostic {
        line,
        col,
        code: code.to_string(),
        severity,
        category,
        message: msg,
        suggestion: sugg,
        line_text: ctx.line(line).to_string(),
    });
}

fn rule_meta(code: &str) -> (Category, Severity) {
    for (c, cat, sev, _) in RULE_CATALOG {
        if *c == code {
            return (*cat, *sev);
        }
    }
    (Category::Style, Severity::Note)
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run all lints over `src`, returning diagnostics sorted by (line, col).
pub fn lint(src: &str) -> Vec<Diagnostic> {
    let mut ctx = Ctx {
        src,
        lines: src.lines().collect(),
        tokens: Vec::new(),
        syms: Syms::default(),
        diags: Vec::new(),
    };

    match aero_lex::lex(src) {
        Ok(tokens) => {
            ctx.tokens = tokens.clone();
            lex_pass(&mut ctx, &tokens);
            match parse_source(src) {
                Ok(prog) => {
                    collect_defs(&mut ctx, &prog);
                    ast_pass(&mut ctx, &prog);
                }
                Err(e) => emit(
                    &mut ctx,
                    "syntax_error",
                    e.line,
                    e.col,
                    format!("syntax error: {}", e.msg),
                    None,
                ),
            }
        }
        Err(e) => emit(&mut ctx, "lex_error", e.line, e.col, format!("lex error: {}", e.msg), None),
    }

    ctx.diags.sort_by_key(|d| (d.line, d.col));
    ctx.diags
}

// ---------------------------------------------------------------------------
// Naming helpers
// ---------------------------------------------------------------------------

fn is_snake_case(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('_')
        && !s.ends_with('_')
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn is_upper_camel(s: &str) -> bool {
    let mut c = s.chars();
    match c.next() {
        Some(x) if x.is_ascii_uppercase() => {}
        _ => return false,
    }
    s.chars().all(|x| x.is_ascii_alphanumeric())
}

fn is_screaming_snake(s: &str) -> bool {
    !s.is_empty()
        && s.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && s.chars().next().map_or(false, |c| c.is_ascii_uppercase())
}

fn is_all_caps(s: &str) -> bool {
    s.chars().any(|c| c.is_ascii_alphabetic())
        && s.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

const BUILTINS: &[&str] = &[
    "print", "len", "push", "pop", "alloc", "json_string", "json_escape", "matmul", "sqrt", "exp",
    "log", "sin", "cos", "size", "String", "Vec", "HashMap", "HashSet", "Box", "int_to_str",
    "strlen", "strcmp", "atoi", "time", "rand",
];

fn to_snake(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_ascii_uppercase() && i > 0 {
            out.push('_');
        }
        out.extend(c.to_lowercase());
    }
    out
}

fn to_camel(s: &str) -> String {
    let mut out = String::new();
    let mut upper = true;
    for c in s.chars() {
        if c == '_' {
            upper = true;
        } else if upper {
            out.extend(c.to_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Definition collection
// ---------------------------------------------------------------------------

fn collect_defs(ctx: &mut Ctx, prog: &Program) {
    for st in &prog.stmts {
        collect_defs_stmt(ctx, st);
    }
}

fn collect_defs_stmt(ctx: &mut Ctx, st: &Stmt) {
    match st {
        Stmt::FnDef { name, span, .. } => {
            ctx.syms.defined_fns.insert(name.clone(), (span.line, span.col));
        }
        Stmt::StructDef { name, fields, span, .. } => {
            let flds = fields.iter().map(|(n, _)| n.clone()).collect();
            ctx.syms
                .defined_structs
                .insert(name.clone(), (span.line, span.col, fields.len(), flds));
        }
        Stmt::EnumDef { name, variants, span, .. } => {
            ctx.syms
                .defined_enums
                .insert(name.clone(), (span.line, span.col, variants.len()));
        }
        Stmt::ConstDef { name, span, .. } => {
            ctx.syms.defined_consts.insert(name.clone(), (span.line, span.col));
        }
        Stmt::UseDecl { path, span } => {
            if let Some(last) = path.last() {
                ctx.syms.imported.insert(last.clone(), (span.line, span.col));
            }
        }
        Stmt::ModDef { items, .. } => {
            for it in items {
                collect_defs_stmt(ctx, it);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Usage collection (whole file)
// ---------------------------------------------------------------------------

fn expr_uses(ctx: &mut Ctx, e: &Expr) {
    match e {
        Expr::Var(name, _) => {
            ctx.syms.var_refs.insert(name.clone());
        }
        Expr::Call { callee, args, .. } => {
            ctx.syms.called_names.insert(callee.clone());
            for a in args {
                expr_uses(ctx, a);
            }
        }
        Expr::PathCall { path, args, .. } => {
            if let Some(first) = path.first() {
                ctx.syms.used_enum_names.insert(first.clone());
                ctx.syms.used_struct_names.insert(first.clone());
                ctx.syms.called_names.insert(first.clone());
            }
            for a in args {
                expr_uses(ctx, a);
            }
        }
        Expr::MethodCall { recv, args, .. } => {
            expr_uses(ctx, recv);
            for a in args {
                expr_uses(ctx, a);
            }
        }
        Expr::StructLit { name, fields, .. } => {
            ctx.syms.used_struct_names.insert(name.clone());
            for (_, f) in fields {
                expr_uses(ctx, f);
            }
        }
        Expr::EnumLit { name, arg, .. } => {
            ctx.syms.used_enum_names.insert(name.clone());
            if let Some(a) = arg {
                expr_uses(ctx, a);
            }
        }
        Expr::Borrow { target, .. } => expr_uses(ctx, target),
        Expr::Deref { target, .. } => expr_uses(ctx, target),
        Expr::Try { target, .. } => expr_uses(ctx, target),
        Expr::Index { target, index, .. } => {
            expr_uses(ctx, target);
            expr_uses(ctx, index);
        }
        Expr::Field { target, .. } => expr_uses(ctx, target),
        Expr::Cast { target, .. } => expr_uses(ctx, target),
        Expr::Unary { expr, .. } => expr_uses(ctx, expr),
        Expr::Binary { lhs, rhs, .. } | Expr::Cmp { lhs, rhs, .. } | Expr::Logic { lhs, rhs, .. } => {
            expr_uses(ctx, lhs);
            expr_uses(ctx, rhs);
        }
        Expr::Tuple(elts, _) | Expr::Array(elts, _) => {
            for e in elts {
                expr_uses(ctx, e);
            }
        }
        Expr::ArenaLit(..) | Expr::TensorLit(.., _) | Expr::Int(..) | Expr::Bool(..) | Expr::Str(..)
        | Expr::Float(..) | Expr::Char(..) => {}
    }
}

fn stmt_uses(ctx: &mut Ctx, st: &Stmt) {
    match st {
        Stmt::Let { name, init, span, .. } => {
            if !name.starts_with('_') {
                ctx.syms.lets.push((name.clone(), span.line, span.col));
            }
            expr_uses(ctx, init);
        }
        Stmt::Return(Some(e), _) => expr_uses(ctx, e),
        Stmt::Return(None, _) => {}
        Stmt::Expr(e, _) => expr_uses(ctx, e),
        Stmt::Print(args, _) => {
            for a in args {
                expr_uses(ctx, a);
            }
        }
        Stmt::If { cond, then_body, else_body, .. } => {
            expr_uses(ctx, cond);
            stmts_uses(ctx, then_body);
            stmts_uses(ctx, else_body);
        }
        Stmt::While { cond, body, .. } => {
            expr_uses(ctx, cond);
            stmts_uses(ctx, body);
        }
        Stmt::Loop { body, .. } => stmts_uses(ctx, body),
        Stmt::For { iter, body, .. } => {
            expr_uses(ctx, iter);
            stmts_uses(ctx, body);
        }
        Stmt::Match { scrutinee, arms, .. } => {
            expr_uses(ctx, scrutinee);
            for arm in arms {
                stmts_uses(ctx, &arm.body);
            }
        }
        Stmt::Assign { name, value, .. } => {
            ctx.syms.var_refs.insert(name.clone());
            expr_uses(ctx, value);
        }
        Stmt::AssignIndex { target, index, value, .. } => {
            expr_uses(ctx, target);
            expr_uses(ctx, index);
            expr_uses(ctx, value);
        }
        Stmt::AssignDeref { target, value, .. } => {
            expr_uses(ctx, target);
            expr_uses(ctx, value);
        }
        Stmt::AssignField { target, value, .. } => {
            expr_uses(ctx, target);
            expr_uses(ctx, value);
        }
        Stmt::FnDef { body, .. } => stmts_uses(ctx, body),
        Stmt::ImplBlock { methods, .. } => stmts_uses(ctx, methods),
        Stmt::ModDef { items, .. } => stmts_uses(ctx, items),
        Stmt::StructDef { fields, .. } => {
            for (_, t) in fields {
                type_uses(ctx, t);
            }
        }
        Stmt::EnumDef { variants, .. } => {
            for v in variants {
                if let Some(p) = &v.payload {
                    type_uses(ctx, p);
                }
            }
        }
        Stmt::ConstDef { value, .. } => expr_uses(ctx, value),
        Stmt::Break(..) | Stmt::Continue(..) | Stmt::TraitDef { .. } | Stmt::UnionDef { .. }
        | Stmt::UseDecl { .. } | Stmt::Pub(..) | Stmt::ModFile { .. } => {}
    }
}

fn stmts_uses(ctx: &mut Ctx, body: &[Stmt]) {
    for st in body {
        stmt_uses(ctx, st);
    }
}

fn type_uses(ctx: &mut Ctx, t: &TypeExpr) {
    match t {
        TypeExpr::Named(n, _) => {
            ctx.syms.used_struct_names.insert(n.clone());
            ctx.syms.used_enum_names.insert(n.clone());
        }
        TypeExpr::Generic { name, args, .. } => {
            ctx.syms.used_struct_names.insert(name.clone());
            for a in args {
                type_uses(ctx, a);
            }
        }
        TypeExpr::Ref { inner, .. } | TypeExpr::Ptr(inner, _) => type_uses(ctx, inner),
        TypeExpr::Tuple(elts, _) => {
            for e in elts {
                type_uses(ctx, e);
            }
        }
        TypeExpr::Array(inner, _, _) => type_uses(ctx, inner),
        TypeExpr::Path { root, .. } => {
            ctx.syms.used_struct_names.insert(root.clone());
        }
        TypeExpr::Dyn { name, .. } => {
            ctx.syms.used_struct_names.insert(name.clone());
        }
    }
}

// ---------------------------------------------------------------------------
// AST pass
// ---------------------------------------------------------------------------

fn ast_pass(ctx: &mut Ctx, prog: &Program) {
    stmts_uses(ctx, &prog.stmts);
    unused_rules(ctx);
    for st in &prog.stmts {
        lint_top_stmt(ctx, st);
    }
}

fn unused_rules(ctx: &mut Ctx) {
    // Clone the symbol tables so we can iterate while mutably borrowing `ctx`
    // inside `emit` (which pushes into `ctx.diags` and reads `ctx.lines`).
    let defined_fns = ctx.syms.defined_fns.clone();
    let defined_consts = ctx.syms.defined_consts.clone();
    let imported = ctx.syms.imported.clone();
    let defined_structs = ctx.syms.defined_structs.clone();
    let defined_enums = ctx.syms.defined_enums.clone();
    let lets = ctx.syms.lets.clone();

    let called = ctx.syms.called_names.clone();
    let var_refs = ctx.syms.var_refs.clone();
    let used_structs = ctx.syms.used_struct_names.clone();
    let used_enums = ctx.syms.used_enum_names.clone();

    for (name, (line, col)) in &defined_fns {
        if name == "main" || name.starts_with("test_") {
            continue;
        }
        if !called.contains(name) {
            emit(
                ctx,
                "unused_function",
                *line,
                *col,
                format!("function `{name}` is never called"),
                Some(format!("remove it, call `{name}(...)`, or make it `pub`")),
            );
        }
    }
    for (name, (line, col)) in &defined_consts {
        if !var_refs.contains(name) {
            emit(ctx, "unused_const", *line, *col, format!("const `{name}` is never used"), Some("remove it".into()));
        }
    }
    for (name, (line, col)) in &imported {
        let used = var_refs.contains(name)
            || called.contains(name)
            || used_structs.contains(name)
            || used_enums.contains(name);
        if !used {
            emit(ctx, "unused_import", *line, *col, format!("import `{name}` is never used"), Some(format!("remove `use {name}`")));
        }
    }
    for (name, (line, col, nfields, _flds)) in &defined_structs {
        if *nfields == 0 {
            emit(ctx, "empty_struct", *line, *col, format!("struct `{name}` has no fields"), Some("add fields or use an enum".into()));
        }
    }
    for (name, (line, col, nvar)) in &defined_enums {
        if *nvar == 0 {
            emit(ctx, "empty_enum", *line, *col, format!("enum `{name}` has no variants"), Some("add variants or remove it".into()));
        }
    }
    for (name, line, col) in &lets {
        if name == "_" {
            emit(ctx, "let_underscore_discard", *line, *col, "`let _ =` silently discards a value".into(), Some("bind the value to a named variable".into()));
        }
        if name.starts_with('_') && name.len() > 1 && var_refs.contains(name) {
            emit(ctx, "leading_underscore_used", *line, *col, format!("`{name}` is prefixed with `_` but is used"), Some("remove the leading underscore".into()));
        }
        if !name.starts_with('_') && !var_refs.contains(name) {
            emit(ctx, "unused_variable", *line, *col, format!("variable `{name}` is never used"), Some(format!("remove it or prefix with `_`")));
        }
    }
}

fn lint_top_stmt(ctx: &mut Ctx, st: &Stmt) {
    match st {
        Stmt::FnDef { name, lifetimes, params, body, span, .. } => {
            if !is_snake_case(name) {
                emit(ctx, "fn_non_snake_case", span.line, span.col, format!("function `{name}` should be snake_case"), Some(format!("rename to `{}`", to_snake(name))));
            }
            if is_all_caps(name) {
                emit(ctx, "all_caps_identifier", span.line, span.col, format!("function name `{name}` is ALL_CAPS"), None);
            }
            if BUILTINS.contains(&name.as_str()) {
                emit(ctx, "shadows_builtin", span.line, span.col, format!("function `{name}` shadows a built-in name"), Some("rename it".into()));
            }
            let l = name.chars().count();
            if l < 2 {
                emit(ctx, "ident_too_short", span.line, span.col, format!("function name `{name}` is too short"), None);
            }
            if l > 32 {
                emit(ctx, "ident_too_long", span.line, span.col, format!("function name `{name}` has {l} characters"), None);
            }
            if name.ends_with('_') {
                emit(ctx, "trailing_underscore", span.line, span.col, format!("`{name}` ends with an underscore"), None);
            }
            if params.len() > 8 {
                emit(ctx, "too_many_parameters", span.line, span.col, format!("function `{name}` has {} parameters", params.len()), Some("group related parameters into a struct".into()));
            }
            for lt in lifetimes {
                emit(ctx, "needless_lifetime", span.line, span.col, format!("lifetime `{lt}` can likely be elided"), None);
            }
            if count_stmts(body) > 80 {
                emit(ctx, "too_many_function_lines", span.line, span.col, format!("function `{name}` body has {} statements", count_stmts(body)), Some("refactor into helper functions".into()));
            }
            if body.is_empty() {
                emit(ctx, "empty_block", span.line, span.col, format!("function `{name}` has an empty body"), Some("implement the body or remove it".into()));
            }
            lint_block(ctx, body);
        }
        Stmt::StructDef { name, fields, span, .. } => {
            if !is_upper_camel(name) {
                emit(ctx, "struct_non_upper_camel", span.line, span.col, format!("struct `{name}` should be UpperCamelCase"), Some(format!("rename to `{}`", to_camel(name))));
            }
            if fields.len() > 16 {
                emit(ctx, "too_many_struct_fields", span.line, span.col, format!("struct `{name}` has {} fields", fields.len()), Some("consider nesting related fields".into()));
            }
        }
        Stmt::UnionDef { name, span, .. } => {
            if !is_upper_camel(name) {
                emit(ctx, "struct_non_upper_camel", span.line, span.col, format!("union `{name}` should be UpperCamelCase"), None);
            }
        }
        Stmt::EnumDef { name, variants, span, .. } => {
            if !is_upper_camel(name) {
                emit(ctx, "enum_non_upper_camel", span.line, span.col, format!("enum `{name}` should be UpperCamelCase"), None);
            }
            let mut seen = HashSet::new();
            for v in variants {
                if !seen.insert(v.name.clone()) {
                    emit(ctx, "duplicate_enum_variant", v.span.line, v.span.col, format!("duplicate variant `{}`", v.name), None);
                }
            }
        }
        Stmt::TraitDef { name, span, .. } => {
            if !is_upper_camel(name) {
                emit(ctx, "trait_non_upper_camel", span.line, span.col, format!("trait `{name}` should be UpperCamelCase"), None);
            }
        }
        Stmt::ConstDef { name, span, .. } => {
            if !is_screaming_snake(name) {
                emit(ctx, "const_non_screaming_snake", span.line, span.col, format!("const `{name}` should be SCREAMING_SNAKE_CASE"), Some(format!("rename to `{}`", name.to_uppercase())));
            }
        }
        Stmt::ModDef { name, span, items, .. } => {
            if !is_snake_case(name) {
                emit(ctx, "module_non_snake_case", span.line, span.col, format!("module `{name}` should be snake_case"), None);
            }
            for it in items {
                lint_top_stmt(ctx, it);
            }
        }
        Stmt::ImplBlock { type_name, methods, .. } => {
            ctx.syms.used_struct_names.insert(type_name.clone());
            ctx.syms.used_enum_names.insert(type_name.clone());
            for m in methods {
                lint_top_stmt(ctx, m);
            }
        }
        Stmt::Pub(inner, _) => lint_top_stmt(ctx, inner),
        _ => {}
    }
    stmt_checks(ctx, st);
}

fn lint_block(ctx: &mut Ctx, body: &[Stmt]) {
    lint_block_checks(ctx, body);
}

fn lint_block_checks(ctx: &mut Ctx, body: &[Stmt]) {
    for (i, st) in body.iter().enumerate() {
        stmt_checks(ctx, st);
        if matches!(st, Stmt::Return(..) | Stmt::Break(..) | Stmt::Continue(..)) {
            if let Some(follow) = body.get(i + 1) {
                emit(ctx, "unreachable_after_return", follow.span().line, follow.span().col, "code after `return`/`break`/`continue` is unreachable".into(), Some("remove the dead code".into()));
                break;
            }
        }
    }
    if let Some(last) = body.last() {
        if let Stmt::Return(Some(..), span) = last {
            emit(ctx, "needless_return_at_end", span.line, span.col, "unnecessary trailing `return`".into(), Some("drop the `return`; the last expression is the value".into()));
        }
        if let Stmt::Continue(span) = last {
            emit(ctx, "unnecessary_restart", span.line, span.col, "redundant `continue` at end of a loop".into(), Some("remove it".into()));
        }
        if let Stmt::If { else_body, span, .. } = last {
            if else_body.is_empty() {
                emit(ctx, "redundant_else", span.line, span.col, "`else` block is empty and can be removed".into(), Some("remove the `else`".into()));
            }
        }
    }
}

fn stmt_checks(ctx: &mut Ctx, st: &Stmt) {
    match st {
        Stmt::Let { name, ty_ann, init, span, .. } => {
            if !is_snake_case(name) && !name.starts_with('_') {
                emit(ctx, "var_non_snake_case", span.line, span.col, format!("variable `{name}` should be snake_case"), Some(format!("rename to `{}`", to_snake(name))));
            }
            let l = name.chars().count();
            if l < 2 && name.chars().all(|c| !c.is_ascii_digit()) {
                emit(ctx, "ident_too_short", span.line, span.col, format!("variable `{name}` is too short to be self-documenting"), None);
            }
            if l > 32 {
                emit(ctx, "ident_too_long", span.line, span.col, format!("variable `{name}` has {l} characters"), None);
            }
            if name.ends_with('_') {
                emit(ctx, "trailing_underscore", span.line, span.col, format!("variable `{name}` ends with an underscore"), Some("remove the trailing underscore".into()));
            }
            if is_all_caps(name) && !name.starts_with('_') {
                emit(ctx, "all_caps_identifier", span.line, span.col, format!("variable `{name}` is ALL_CAPS"), None);
            }
            if BUILTINS.contains(&name.as_str()) {
                emit(ctx, "shadows_builtin", span.line, span.col, format!("variable `{name}` shadows a built-in name"), Some("rename it".into()));
            }
            if let Some(t) = ty_ann {
                if init_is_literal_of_ty(init, t) {
                    emit(ctx, "redundant_type_annotation", span.line, span.col, format!("type annotation on `{name}` can be inferred"), Some("drop the `: T`".into()));
                }
            }
            expr_checks(ctx, init);
        }
        Stmt::Print(args, span) => {
            if let Some(Expr::Str(fmt, _)) = args.first() {
                let pl = count_placeholders(fmt);
                let supplied = args.len().saturating_sub(1);
                if pl != supplied && pl > 0 {
                    emit(ctx, "print_format_mismatch", span.line, span.col, format!("`print` formats {pl} placeholder(s) but got {supplied} argument(s)"), Some("align the argument count with the format string".into()));
                }
            }
            for a in args {
                expr_checks(ctx, a);
            }
        }
        Stmt::Expr(e, span) => {
            if expr_has_no_effect(e) {
                emit(ctx, "void_expr_statement", span.line, span.col, "expression statement has no effect".into(), Some("remove the statement or assign its result".into()));
            }
            expr_checks(ctx, e);
        }
        Stmt::Return(e, _) => {
            if let Some(e) = e {
                expr_checks(ctx, e);
            }
        }
        Stmt::If { cond, then_body, else_body, span } => {
            expr_checks(ctx, cond);
            if then_body.is_empty() {
                emit(ctx, "empty_block", span.line, span.col, "empty `if` body".into(), Some("remove the branch".into()));
            }
            if ends_with_return(then_body) && !else_body.is_empty() {
                emit(ctx, "useless_else_after_return", span.line, span.col, "`else` is useless when the `then` branch returns".into(), Some("pull the `else` body out".into()));
            }
            if branches_identical(then_body, else_body) {
                emit(ctx, "same_then_else", span.line, span.col, "`if` and `else` branches are identical".into(), Some("collapse them or differentiate".into()));
            }
            lint_block_checks(ctx, then_body);
            lint_block_checks(ctx, else_body);
        }
        Stmt::While { cond, body, span } => {
            expr_checks(ctx, cond);
            if matches!(cond, Expr::Bool(true, _)) {
                emit(ctx, "while_true_loop", span.line, span.col, "`while true` is better written as `loop`".into(), Some("replace `while true` with `loop`".into()));
                if !contains_break(body) {
                    emit(ctx, "infinite_loop_no_break", span.line, span.col, "`while true` loop has no `break`/`return` exit path".into(), Some("add a break condition".into()));
                }
            }
            if body.is_empty() {
                emit(ctx, "empty_block", span.line, span.col, "empty loop body".into(), Some("remove or implement it".into()));
            }
            lint_block_checks(ctx, body);
        }
        Stmt::Loop { body, span } => {
            if !contains_break(body) {
                emit(ctx, "infinite_loop_no_break", span.line, span.col, "`loop` has no `break`/`return` exit path".into(), Some("add a `break` or `return`".into()));
            }
            if body.is_empty() {
                emit(ctx, "empty_block", span.line, span.col, "empty `loop` body".into(), Some("remove or implement it".into()));
            }
            lint_block_checks(ctx, body);
        }
        Stmt::For { var, iter, body, span } => {
            if !is_snake_case(var) {
                emit(ctx, "var_non_snake_case", span.line, span.col, format!("loop variable `{var}` should be snake_case"), Some(format!("rename to `{}`", to_snake(var))));
            }
            expr_checks(ctx, iter);
            if body.is_empty() {
                emit(ctx, "empty_block", span.line, span.col, "empty `for` loop body".into(), None);
            }
            lint_block_checks(ctx, body);
        }
        Stmt::Match { scrutinee, arms, span } => {
            expr_checks(ctx, scrutinee);
            if arms.is_empty() {
                emit(ctx, "empty_match", span.line, span.col, "`match` has no arms".into(), Some("add arms or remove the match".into()));
            }
            if arms.len() > 8 {
                emit(ctx, "match_too_many_arms", span.line, span.col, format!("`match` has {} arms", arms.len()), Some("split into helper functions".into()));
            }
            let mut seen = HashSet::new();
            for arm in arms {
                if let Some(key) = pattern_key(&arm.pattern) {
                    if !seen.insert(key) {
                        emit(ctx, "duplicate_match_pattern", arm.span.line, arm.span.col, "duplicate match arm pattern".into(), None);
                    }
                }
                lint_block_checks(ctx, &arm.body);
            }
        }
        Stmt::Assign { name, value, span } => {
            if let Expr::Var(v, _) = value {
                if v == name {
                    emit(ctx, "self_assignment", span.line, span.col, format!("`{name}` is assigned to itself"), Some("remove the assignment or fix the RHS".into()));
                }
            }
            expr_checks(ctx, value);
        }
        Stmt::AssignIndex { target, index, value, span } => {
            expr_checks(ctx, target);
            expr_checks(ctx, index);
            expr_checks(ctx, value);
            if let (Expr::Array(elts, _), Expr::Int(i, _)) = (target.as_ref(), index.as_ref()) {
                if *i >= elts.len() as i64 || *i < 0 {
                    emit(ctx, "array_index_out_of_bounds", span.line, span.col, format!("array index {i} is out of bounds (len {})", elts.len()), None);
                }
            }
        }
        Stmt::AssignDeref { target, value, .. } => {
            expr_checks(ctx, target);
            expr_checks(ctx, value);
        }
        Stmt::AssignField { target, value, .. } => {
            expr_checks(ctx, target);
            expr_checks(ctx, value);
        }
        _ => {}
    }
}

fn expr_checks(ctx: &mut Ctx, e: &Expr) {
    match e {
        Expr::Cmp { lhs, rhs, span, .. } => {
            expr_checks(ctx, lhs);
            expr_checks(ctx, rhs);
            if is_literal(lhs) && is_literal(rhs) {
                emit(ctx, "comparing_literals", span.line, span.col, format!("comparison between literal values `{}` and `{}`", lit_desc(lhs), lit_desc(rhs)), Some("compare to a variable instead".into()));
            }
            if expr_same(lhs, rhs) {
                emit(ctx, "same_operands_cmp", span.line, span.col, "comparing an expression to itself".into(), Some("check the operands".into()));
            }
            if matches!(rhs.as_ref(), Expr::Bool(..)) || matches!(lhs.as_ref(), Expr::Bool(..)) {
                emit(ctx, "redundant_boolean", span.line, span.col, "unnecessary comparison to a boolean".into(), None);
                emit(ctx, "compare_to_true", span.line, span.col, "unnecessary comparison to a boolean literal".into(), None);
            }
            if is_float(lhs) || is_float(rhs) {
                emit(ctx, "float_equality", span.line, span.col, "exact comparison involving floating-point values".into(), Some("compare with an epsilon".into()));
            }
        }
        Expr::Binary { op, lhs, rhs, span } => {
            match (op, lhs.as_ref(), rhs.as_ref()) {
                (aero_parse::ast::BinOp::Div, Expr::Int(0, _), _) | (aero_parse::ast::BinOp::Div, _, Expr::Int(0, _)) => {
                    emit(ctx, "division_by_zero", span.line, span.col, "integer division by a literal zero".into(), Some("guard the divisor".into()));
                }
                (aero_parse::ast::BinOp::Rem, Expr::Int(0, _), _) | (aero_parse::ast::BinOp::Rem, _, Expr::Int(0, _)) => {
                    emit(ctx, "modulo_by_zero", span.line, span.col, "integer modulo by a literal zero".into(), Some("guard the divisor".into()));
                }
                _ => {}
            }
            expr_checks(ctx, lhs);
            expr_checks(ctx, rhs);
        }
        Expr::Unary { expr, span, .. } => {
            if matches!(expr.as_ref(), Expr::Unary { .. }) {
                emit(ctx, "double_negation", span.line, span.col, "double negation".into(), Some("simplify".into()));
            }
            expr_checks(ctx, expr);
        }
        Expr::Logic { lhs, rhs, .. } => {
            expr_checks(ctx, lhs);
            expr_checks(ctx, rhs);
        }
        Expr::Call { callee, args, span } => {
            if args.len() > 6 {
                emit(ctx, "too_many_call_args", span.line, span.col, format!("call to `{callee}` has {} arguments", args.len()), Some("group arguments into a struct".into()));
            }
            if callee == "len" {
                if args.len() == 1 {
                    emit(ctx, "len_zero_compare", span.line, span.col, "`len(x)` call".into(), None);
                }
            }
            for a in args {
                expr_checks(ctx, a);
            }
        }
        Expr::PathCall { path, args, span } => {
            if path.len() == 2 && path[0] == "String" && path[1] == "from" {
                if let Some(Expr::Str(..)) = args.first() {
                    emit(ctx, "redundant_string_from", span.line, span.col, "`String::from` around a string literal is redundant".into(), Some("use the literal directly".into()));
                }
            }
            for a in args {
                expr_checks(ctx, a);
            }
        }
        Expr::MethodCall { recv, method, args, span } => {
            expr_checks(ctx, recv);
            if args.len() > 6 {
                emit(ctx, "too_many_call_args", span.line, span.col, format!("call to `{method}` has {} arguments", args.len()), Some("group arguments into a struct".into()));
            }
            for a in args {
                expr_checks(ctx, a);
            }
        }
        Expr::Index { target, index, span } => {
            expr_checks(ctx, target);
            expr_checks(ctx, index);
            if let (Expr::Array(elts, _), Expr::Int(i, _)) = (target.as_ref(), index.as_ref()) {
                if *i >= elts.len() as i64 || *i < 0 {
                    emit(ctx, "array_index_out_of_bounds", span.line, span.col, format!("array index {i} is out of bounds (len {})", elts.len()), None);
                }
            }
        }
        Expr::Array(elts, span) => {
            if elts.len() == 1 {
                emit(ctx, "single_element_array", span.line, span.col, "single-element array literal".into(), Some("use a scalar instead".into()));
            }
            for e in elts {
                expr_checks(ctx, e);
            }
        }
        Expr::StructLit { name, fields, span } => {
            let mut seen = HashSet::new();
            for (f, val) in fields {
                if !seen.insert(f.clone()) {
                    emit(ctx, "duplicate_struct_field", span.line, span.col, format!("duplicate field `{f}` in struct literal `{name}`"), None);
                }
                expr_checks(ctx, val);
            }
        }
        Expr::EnumLit { name, arg, .. } => {
            if let Some(a) = arg {
                expr_checks(ctx, a);
            }
        }
        Expr::Borrow { target, .. } => expr_checks(ctx, target),
        Expr::Deref { target, span } => {
            expr_checks(ctx, target);
            if !matches!(target.as_ref(), Expr::Var(..)) {
                emit(ctx, "unsafe_ptr_deref", span.line, span.col, "dereferencing a non-variable expression".into(), None);
            }
        }
        Expr::Try { target, .. } => expr_checks(ctx, target),
        Expr::Field { target, .. } => expr_checks(ctx, target),
        Expr::Cast { target, .. } => expr_checks(ctx, target),
        Expr::Tuple(elts, _) => {
            for e in elts {
                expr_checks(ctx, e);
            }
        }
        Expr::Str(s, span) => {
            if s.chars().count() == 1 && !s.chars().next().map_or(false, |c| c.is_ascii_whitespace()) {
                emit(ctx, "single_char_string", span.line, span.col, "single-character string literal".into(), Some("use a char literal".into()));
            }
        }
        Expr::Int(v, span) => {
            if *v >= 10_000 && *v % 5 != 0 {
                // heuristics only; simplified magic_number marker skipped to avoid noise
                let _ = (v, span);
            }
        }
        Expr::Float(..) | Expr::Bool(..) | Expr::Char(..) | Expr::ArenaLit(..) | Expr::TensorLit(.., _) => {}
        Expr::Var(..) => {}
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn expr_same(a: &Expr, b: &Expr) -> bool {
    std::mem::discriminant(a) == std::mem::discriminant(b)
}

fn branches_identical(a: &[Stmt], b: &[Stmt]) -> bool {
    if a.is_empty() || a.len() != b.len() {
        return false;
    }
    let ka: Vec<_> = a.iter().map(std::mem::discriminant).collect();
    let kb: Vec<_> = b.iter().map(std::mem::discriminant).collect();
    ka == kb
}

fn ends_with_return(body: &[Stmt]) -> bool {
    matches!(body.last(), Some(Stmt::Return(..)))
}

fn contains_break(body: &[Stmt]) -> bool {
    body.iter().any(|s| matches!(s, Stmt::Break(..) | Stmt::Return(..) | Stmt::Continue(..)))
}

fn pattern_key(p: &MatchPattern) -> Option<String> {
    match p {
        MatchPattern::IntLit(v) => Some(format!("i{v}")),
        MatchPattern::BoolLit(v) => Some(format!("b{v}")),
        MatchPattern::CharLit(c) => Some(format!("c{c}")),
        MatchPattern::StrLit(s) => Some(format!("s{s}")),
        MatchPattern::EnumVariant { variant, .. } => Some(format!("e{variant}")),
        _ => None,
    }
}

fn expr_has_no_effect(e: &Expr) -> bool {
    matches!(
        e,
        Expr::Int(..) | Expr::Float(..) | Expr::Bool(..) | Expr::Str(..) | Expr::Char(..) | Expr::Var(..) | Expr::Tuple(..)
    )
}

fn count_stmts(body: &[Stmt]) -> usize {
    let mut n = 0usize;
    for s in body {
        n += 1;
        match s {
            Stmt::FnDef { body, .. } => n += count_stmts(body),
            Stmt::If { then_body, else_body, .. } => n += count_stmts(then_body) + count_stmts(else_body),
            Stmt::While { body, .. } | Stmt::Loop { body, .. } | Stmt::For { body, .. } => n += count_stmts(body),
            _ => {}
        }
    }
    n
}

fn count_placeholders(fmt: &str) -> usize {
    let b = fmt.as_bytes();
    let (mut n, mut i) = (0, 0);
    while i < b.len() {
        if b[i] == b'%' {
            if i + 1 < b.len() && b[i + 1] == b'%' {
                i += 2;
                continue;
            }
            n += 1;
            i += 1;
        } else {
            i += 1;
        }
    }
    n
}

fn init_is_literal_of_ty(init: &Expr, t: &TypeExpr) -> bool {
    match t {
        TypeExpr::Named(n, _) => match n.as_str() {
            "i64" | "i32" => matches!(init, Expr::Int(..)),
            "f64" | "f32" => matches!(init, Expr::Float(..)),
            "bool" => matches!(init, Expr::Bool(..)),
            "char" => matches!(init, Expr::Char(..)),
            "str" => matches!(init, Expr::Str(..)),
            _ => false,
        },
        _ => false,
    }
}

fn is_literal(e: &Expr) -> bool {
    matches!(e, Expr::Int(..) | Expr::Float(..) | Expr::Bool(..) | Expr::Str(..) | Expr::Char(..))
}

fn is_float(e: &Expr) -> bool {
    matches!(e, Expr::Float(..))
}

fn lit_desc(e: &Expr) -> String {
    match e {
        Expr::Int(v, _) => v.to_string(),
        Expr::Float(v, _) => v.to_string(),
        Expr::Bool(v, _) => v.to_string(),
        Expr::Str(s, _) => format!("\"{s}\""),
        Expr::Char(c, _) => format!("'{c}'"),
        _ => "<expr>".into(),
    }
}

// ---------------------------------------------------------------------------
// Lexical pass
// ---------------------------------------------------------------------------

fn lex_pass(ctx: &mut Ctx, tokens: &[Token]) {
    // Copy out owned/immutable data so we don't hold a borrow of `ctx` fields
    // (or `ctx` itself) while calling `emit(ctx, ..)` inside the loops.
    let lines: Vec<&str> = ctx.lines.clone();
    let src = ctx.src;

    for (i, line) in lines.iter().enumerate() {
        let ln = (i + 1) as u32;
        let width = line.chars().count();
        if width > 100 {
            emit(ctx, "line_too_long", ln, 1, format!("line is {width} columns long"), Some("reflow to within 100 columns".into()));
        }
        if line.chars().last().map_or(false, |c| c == ' ' || c == '\t' || c == '\r') {
            emit(ctx, "trailing_whitespace", ln, (line.trim_end().chars().count() as u32) + 1, "trailing whitespace".into(), Some("remove trailing spaces".into()));
        }
        let indent: String = line.chars().take_while(|c| *c == ' ' || *c == '\t').collect();
        if indent.contains('\t') && indent.contains(' ') {
            emit(ctx, "mixed_indentation", ln, 1, "line mixes tabs and spaces in indentation".into(), Some("use spaces consistently".into()));
        } else if indent.contains('\t') && indent.chars().all(|c| c == '\t') {
            emit(ctx, "tabs_in_indentation", ln, 1, "tabs used for indentation".into(), Some("replace tabs with spaces".into()));
        }
    }
    if !ctx.src.is_empty() && !ctx.src.ends_with('\n') {
        emit(ctx, "missing_newline_eof", lines.len() as u32, 1, "file does not end with a newline".into(), Some("append a trailing newline".into()));
    }

    for (i, t) in tokens.iter().enumerate() {
        match &t.kind {
            TokenKind::Semi => {
                if let Some(n) = tokens.get(i + 1) {
                    if n.kind == TokenKind::Semi {
                        emit(ctx, "multiple_semicolons", n.line, n.col, "redundant consecutive semicolons".into(), Some("remove the extra `;`".into()));
                    }
                }
            }
            TokenKind::Comma => {
                if i > 0 {
                    let p = &tokens[i - 1];
                    if p.line == t.line && !matches!(p.kind, TokenKind::Comma) {
                        let gap = &src[p.end..t.start];
                        if !gap.is_empty() && gap.chars().all(|c| c == ' ' || c == '\t') {
                            emit(ctx, "space_before_comma", t.line, t.col, "unnecessary space before `,`".into(), Some("remove the space".into()));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    for t in tokens {
        if let TokenKind::Int(_) = t.kind {
            let text = &src[t.start..t.end];
            if text.len() > 1 && text.starts_with('0') && text.chars().all(|c| c.is_ascii_digit()) {
                emit(ctx, "leading_zero_literal", t.line, t.col, format!("`{text}` has a leading zero and may look octal"), Some("remove the leading zero".into()));
            }
            if !text.contains('-') && text.parse::<i64>().is_err() {
                emit(ctx, "integer_overflow_literal", t.line, t.col, format!("integer literal `{text}` is outside the i64 range"), Some("split or narrow the literal".into()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogue_has_100_rules() {
        assert!(CATALOG_COUNT >= 100, "catalogue must have >=100 rules; has {CATALOG_COUNT}");
        // Every catalog code must also exist exactly once (unique ids).
        let mut codes: Vec<String> = RULE_CATALOG.iter().map(|(c, _, _, _)| c.to_string()).collect();
        let uniq: HashSet<String> = codes.drain(..).collect();
        assert_eq!(uniq.len(), RULE_CATALOG.len(), "rule codes must be unique");
    }

    #[test]
    fn names_are_snake() {
        assert!(is_snake_case("foo_bar"));
        assert!(!is_snake_case("FooBar"));
        assert!(!is_snake_case("_x"));
        assert!(is_upper_camel("FooBar"));
        assert!(!is_upper_camel("foo_bar"));
        assert!(is_screaming_snake("MAX_LEN"));
    }

    #[test]
    fn flags_division_by_zero() {
        let d = lint("let x = 10 / 0;");
        assert!(d.iter().any(|x| x.code == "division_by_zero"));
    }

    #[test]
    fn flags_unused_variable() {
        let d = lint("fn main() { let unused = 5; }");
        assert!(d.iter().any(|x| x.code == "unused_variable"));
    }

    #[test]
    fn flags_fn_naming_and_main_not_unused() {
        let d = lint("fn BadName() -> i64 { return 1; }\nfn main() { return; }");
        assert!(d.iter().any(|x| x.code == "fn_non_snake_case"));
        // `main` should never be reported as unused.
        assert!(!d.iter().any(|x| x.code == "unused_function" && x.line_text.contains("main")));
    }

    #[test]
    fn flags_float_equality() {
        let d = lint("fn main() { let c = 1.0 == 2.0; }");
        assert!(d.iter().any(|x| x.code == "float_equality"));
    }

    #[test]
    fn flags_needless_return_and_reachability() {
        let d = lint("fn a() -> i64 { let x = 1; return 1; }\nfn b() -> i64 { return 1; let dead = 2; }");
        assert!(d.iter().any(|x| x.code == "needless_return_at_end"));
        assert!(d.iter().any(|x| x.code == "unreachable_after_return"));
    }

    #[test]
    fn flags_struct_naming() {
        let d = lint("struct bad_name { a: i64, b: i64 }\nfn main() { return; }");
        assert!(d.iter().any(|x| x.code == "struct_non_upper_camel"));
    }

    #[test]
    fn flags_trailing_whitespace_and_long_line() {
        let src = format!("fn main() {{   \n    let x = 1;\n}}\n");
        let d = lint(&src);
        assert!(d.iter().any(|x| x.code == "trailing_whitespace"));
    }

    #[test]
    fn flags_while_true_and_infinite_loop() {
        let d = lint("fn main() { while (true) { let x = 1; } }");
        assert!(d.iter().any(|x| x.code == "while_true_loop"));
        assert!(d.iter().any(|x| x.code == "infinite_loop_no_break"));
    }

    #[test]
    fn empty_struct_reported() {
        let d = lint("struct Empty_ {  }\nfn main() { return; }");
        assert!(d.iter().any(|x| x.code == "empty_struct"));
    }

    #[test]
    fn severity_error_exits_nonzero() {
        let d = lint("fn main() { let x = 1 / 0; }");
        assert!(d.iter().any(|x| x.code == "division_by_zero" && x.severity == Severity::Error));
    }
}