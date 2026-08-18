//! Minimal LSP server: `aero lsp`.
//!
//! Implements enough of the Language Server Protocol to back the Aero IDE
//! integration. It speaks JSON-RPC over stdin/stdout (Content-Length framed),
//! tracks open documents, recompiles them on edits, and publishes compiler
//! `AeroError`s as `Diagnostic`s. A lightweight AST-based symbol index provides
//! completion, go-to-definition and hover signature docs.
//!
//! Supported methods:
//!   initialize / initialized / shutdown / exit
//!   textDocument/didOpen, didChange, didSave, didClose
//!   textDocument/hover, textDocument/definition, textDocument/completion

use std::collections::HashMap;
use std::io::{BufRead, Write};

use aero_parse::ast::{Stmt, TypeExpr};

// ---------------------------------------------------------------------------
// Minimal JSON value + parser (self-contained, no external deps)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(HashMap<String, Json>),
}

impl Json {
    fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(m) => m.get(key),
            _ => None,
        }
    }
    fn at(&self, idx: usize) -> Option<&Json> {
        match self {
            Json::Arr(a) => a.get(idx),
            _ => None,
        }
    }
    fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }
    fn as_str_or(&self, def: &str) -> String {
        self.as_str().unwrap_or(def).to_string()
    }
    fn as_u64(&self) -> Option<u64> {
        match self {
            Json::Num(n) => Some(*n as u64),
            _ => None,
        }
    }
    fn as_bool(&self) -> bool {
        matches!(self, Json::Bool(true))
    }
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Parser<'a> {
    fn new(b: &'a [u8]) -> Self {
        Parser { b, i: 0 }
    }
    fn skip_ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }
    fn parse(&mut self) -> Result<Json, String> {
        self.skip_ws();
        let c = self.peek().ok_or("unexpected end of JSON")?;
        match c {
            b'n' => {
                self.expect(b"null")?;
                Ok(Json::Null)
            }
            b't' => {
                self.expect(b"true")?;
                Ok(Json::Bool(true))
            }
            b'f' => {
                self.expect(b"false")?;
                Ok(Json::Bool(false))
            }
            b'"' => Ok(Json::Str(self.parse_string()?)),
            b'{' => self.parse_object(),
            b'[' => self.parse_array(),
            _ if c == b'-' || c.is_ascii_digit() => self.parse_number(),
            _ => Err(format!("unexpected character `{}` in JSON", c as char)),
        }
    }
    fn expect(&mut self, lit: &[u8]) -> Result<(), String> {
        if self.b.get(self.i..self.i + lit.len()) == Some(lit) {
            self.i += lit.len();
            Ok(())
        } else {
            Err("unexpected token".to_string())
        }
    }
    fn parse_string(&mut self) -> Result<String, String> {
        // assumes opening quote already consumed
        self.i += 1;
        let mut out = String::new();
        loop {
            let c = self.b.get(self.i).copied().ok_or("unterminated string")?;
            self.i += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let e = self.b.get(self.i).copied().ok_or("bad escape")?;
                    self.i += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let h = self.read_hex4()?;
                            out.push(char::from_u32(h).unwrap_or('\u{FFFD}'));
                        }
                        _ => return Err("bad escape".to_string()),
                    }
                }
                c if c < 0x20 => return Err("control char in string".to_string()),
                _ => out.push(c as char),
            }
        }
        Ok(out)
    }
    fn read_hex4(&mut self) -> Result<u32, String> {
        if self.i + 4 > self.b.len() {
            return Err("bad unicode escape".to_string());
        }
        let s = std::str::from_utf8(&self.b[self.i..self.i + 4])
            .map_err(|_| "bad unicode escape".to_string())?;
        self.i += 4;
        u32::from_str_radix(s, 16).map_err(|_| "bad unicode escape".to_string())
    }
    fn parse_number(&mut self) -> Result<Json, String> {
        let start = self.i;
        while self.i < self.b.len()
            && (self.b[self.i].is_ascii_digit()
                || matches!(self.b[self.i], b'-' | b'+' | b'.' | b'e' | b'E'))
        {
            self.i += 1;
        }
        let s = std::str::from_utf8(&self.b[start..self.i]).map_err(|_| "bad number")?;
        let n: f64 = s.parse().map_err(|_| format!("bad number `{s}`"))?;
        Ok(Json::Num(n))
    }
    fn parse_object(&mut self) -> Result<Json, String> {
        self.i += 1; // consume {
        let mut m = HashMap::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(Json::Obj(m));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err("expected string key".to_string());
            }
            let key = self.parse_string()?;
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err("expected `:`".to_string());
            }
            self.i += 1;
            let val = self.parse()?;
            m.insert(key, val);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.i += 1;
                }
                Some(b'}') => {
                    self.i += 1;
                    break;
                }
                _ => return Err("expected `,` or `}`".to_string()),
            }
        }
        Ok(Json::Obj(m))
    }
    fn parse_array(&mut self) -> Result<Json, String> {
        self.i += 1; // consume [
        let mut a = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(Json::Arr(a));
        }
        loop {
            let v = self.parse()?;
            a.push(v);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.i += 1;
                }
                Some(b']') => {
                    self.i += 1;
                    break;
                }
                _ => return Err("expected `,` or `]`".to_string()),
            }
        }
        Ok(Json::Arr(a))
    }
}

fn parse_json(s: &str) -> Result<Json, String> {
    let mut p = Parser::new(s.as_bytes());
    let v = p.parse()?;
    p.skip_ws();
    if p.i != p.b.len() {
        return Err("trailing characters".to_string());
    }
    Ok(v)
}

// JSON emission (for building responses).
fn jstr(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ---------------------------------------------------------------------------
// LSP messages over stdio
// ---------------------------------------------------------------------------

struct Message {
    id: Option<i64>,
    method: String,
    params: Json,
}

/// Read one Content-Length framed message from stdin.
fn read_message(reader: &mut impl BufRead) -> Option<Message> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).ok()?;
        if n == 0 {
            // EOF
            return None;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            if k.trim().eq_ignore_ascii_case("Content-Length") {
                content_length = v.trim().parse().ok();
            }
        }
    }
    let len = content_length?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).ok()?;
    let body = String::from_utf8_lossy(&buf).into_owned();
    let json = parse_json(&body).ok()?;
    let id = json.get("id").and_then(|j| match j {
        Json::Num(n) if *n >= 0.0 => Some(*n as i64),
        _ => None,
    });
    let method = json.get("method").map(|j| j.as_str_or("")).unwrap_or_default();
    let params = json.get("params").cloned().unwrap_or(Json::Null);
    Some(Message { id, method, params })
}

/// Write a raw JSON body as a Content-Length framed message.
fn write_message(out: &mut impl Write, body: &str) {
    let _ = write!(out, "Content-Length: {}\r\n\r\n{}", body.len(), body);
    let _ = out.flush();
}

fn jstr_str(s: &str) -> String {
    jstr(s)
}

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

/// Compile `source` and turn the first reported [`aero_ir::AeroError`] (if any)
/// into an LSP diagnostic. Line/col are 1-based in Aero -> 0-based in LSP.
fn diagnostics_for(source: &str) -> Vec<String> {
    match aero_ir::check_source(source) {
        Ok(()) => Vec::new(),
        Err(e) => {
            let line = e.line.saturating_sub(1);
            let col = e.col.saturating_sub(1);
            let mut diag = String::new();
            diag.push_str("{\"range\":{\"start\":{\"line\":");
            diag.push_str(&line.to_string());
            diag.push_str(",\"character\":");
            diag.push_str(&col.to_string());
            diag.push_str("},\"end\":{\"line\":");
            diag.push_str(&line.to_string());
            diag.push_str(",\"character\":");
            diag.push_str(&(col + 1).to_string());
            diag.push('}');
            diag.push_str(",\"severity\":1");
            diag.push_str(",\"source\":\"aero\"");
            diag.push_str(",\"message\":");
            diag.push_str(&jstr_str(&format!("[{}] {}", e.phase, e.msg)));
            diag.push('}');
            vec![diag]
        }
    }
}

fn publish_diagnostics(out: &mut impl Write, uri: &str, source: &str) {
    let diags = diagnostics_for(source);
    let mut body = String::new();
    body.push_str("{\"jsonrpc\":\"2.0\",\"method\":\"textDocument/publishDiagnostics\",\"params\":{\"uri\":");
    body.push_str(&jstr_str(uri));
    body.push_str(",\"diagnostics\":[");
    body.push_str(&diags.join(","));
    body.push_str("]}}");
    write_message(out, &body);
}

// ---------------------------------------------------------------------------
// Symbol indexing (completion / go-to-definition / hover)
// ---------------------------------------------------------------------------

/// A single indexed symbol: its name, declaration span (byte offsets + 1-based
/// line/col from the parser `Span`), a short signature and a human doc line.
#[derive(Clone)]
struct Sym {
    name: String,
    kind: SymKind,
    start: usize,
    end: usize,
    line: u32,
    col: u32,
    detail: String,
    doc: String,
}

#[derive(Clone, Copy, PartialEq)]
enum SymKind {
    Function,
    Struct,
    Union,
    Enum,
    Trait,
    Const,
    Variable,
    Module,
    Variant,
}

fn type_to_str(t: &TypeExpr) -> String {
    match t {
        TypeExpr::Named(s, _) => s.clone(),
        TypeExpr::Generic { name, args, .. } => {
            let a: Vec<String> = args.iter().map(type_to_str).collect();
            format!("{name}<{}>", a.join(", "))
        }
        TypeExpr::Array(inner, n, _) => format!("[{}; {}]", type_to_str(inner), n),
        TypeExpr::Tuple(ts, _) => {
            let a: Vec<String> = ts.iter().map(type_to_str).collect();
            format!("({})", a.join(", "))
        }
        TypeExpr::Ref { mut_, inner, .. } => {
            format!("&{}{}", if *mut_ { "mut " } else { "" }, type_to_str(inner))
        }
        TypeExpr::Ptr(inner, _) => format!("*{}", type_to_str(inner)),
        TypeExpr::Path { root, name, .. } => format!("{root}::{name}"),
        TypeExpr::Dyn { name, .. } => format!("dyn {name}"),
    }
}

fn fn_detail(name: &str, type_params: &[String], params: &[(String, TypeExpr)], ret: &Option<TypeExpr>) -> String {
    let mut s = String::from("fn ");
    s.push_str(name);
    if !type_params.is_empty() {
        s.push('<');
        s.push_str(&type_params.join(", "));
        s.push('>');
    }
    s.push('(');
    let ps: Vec<String> = params.iter().map(|(n, t)| format!("{n}: {}", type_to_str(t))).collect();
    s.push_str(&ps.join(", "));
    s.push(')');
    if let Some(r) = ret {
        s.push_str(" -> ");
        s.push_str(&type_to_str(r));
    }
    s
}

/// Index every symbol in `source` visible for completion / navigation: top-level
/// and module items (functions, structs, enums, unions, traits, consts), impl
/// methods, and local variables/parameters/loop variables inside function bodies.
fn index_source(source: &str) -> Vec<Sym> {
    let mut syms = Vec::new();
    if let Ok(program) = aero_parse::parse_source(source) {
        walk_stmts(&program.stmts, &mut syms, source);
    }
    syms
}

fn walk_stmts(stmts: &[Stmt], syms: &mut Vec<Sym>, src: &str) {
    for s in stmts {
        walk_stmt(s, syms, src);
    }
}

fn sym(name: &str, kind: SymKind, start: usize, end: usize, line: u32, col: u32, detail: String, doc: &str) -> Sym {
    Sym { name: name.to_string(), kind, start, end, line, col, detail, doc: doc.to_string() }
}

/// Skip runs of ASCII spaces/tabs from `i`.
fn skip_ws(src: &str, mut i: usize) -> usize {
    let b = src.as_bytes();
    while i < b.len() && matches!(b[i], b' ' | b'\t') {
        i += 1;
    }
    i
}

/// 1-based (line, col) of byte `off` in `src`.
fn line_col_of(src: &str, off: usize) -> (u32, u32) {
    let mut line = 1u32;
    let mut last_nl = 0usize;
    for (i, b) in src.as_bytes().iter().enumerate() {
        if i >= off {
            break;
        }
        if *b == b'\n' {
            line += 1;
            last_nl = i + 1;
        }
    }
    (line, (off.saturating_sub(last_nl) + 1) as u32)
}

fn walk_stmt(s: &Stmt, syms: &mut Vec<Sym>, src: &str) {
    match s {
        Stmt::Pub(inner, _) => walk_stmt(inner, syms, src),
        Stmt::FnDef { name, type_params, params, ret, span, body, .. } => {
            syms.push(sym(name, SymKind::Function, span.start, span.end, span.line, span.col, fn_detail(name, type_params, params, ret), "function"));
            for (p, pt) in params {
                let ps = pt.span();
                syms.push(sym(p, SymKind::Variable, ps.start, ps.end, ps.line, ps.col, format!("param {p}: {}", type_to_str(pt)), "parameter"));
            }
            collect_body_locals(body, syms, src);
        }
        Stmt::StructDef { name, span, .. } => {
            syms.push(sym(name, SymKind::Struct, span.start, span.end, span.line, span.col, format!("struct {name}"), "struct"));
        }
        Stmt::UnionDef { name, span, .. } => {
            syms.push(sym(name, SymKind::Union, span.start, span.end, span.line, span.col, format!("union {name}"), "union"));
        }
        Stmt::EnumDef { name, variants, span, .. } => {
            syms.push(sym(name, SymKind::Enum, span.start, span.end, span.line, span.col, format!("enum {name}"), "enum"));
            for v in variants {
                syms.push(sym(&v.name, SymKind::Variant, v.span.start, v.span.end, v.span.line, v.span.col, format!("{name}::{}", v.name), "enum variant"));
            }
        }
        Stmt::TraitDef { name, methods, span, .. } => {
            syms.push(sym(name, SymKind::Trait, span.start, span.end, span.line, span.col, format!("trait {name}"), "trait"));
            for m in methods {
                let ps: Vec<String> = m.params.iter().map(|(n, t)| format!("{n}: {}", type_to_str(t))).collect();
                let detail = format!(
                    "fn {}({}) -> {}",
                    m.name,
                    ps.join(", "),
                    m.ret.as_ref().map(type_to_str).unwrap_or_else(|| "()".to_string())
                );
                syms.push(sym(&m.name, SymKind::Function, m.span.start, m.span.end, m.span.line, m.span.col, detail, &format!("trait method of {name}")));
            }
        }
        Stmt::ImplBlock { type_name, methods, .. } => {
            for mstmt in methods {
                if let Stmt::FnDef { name, type_params, params, ret, span, .. } = mstmt {
                    syms.push(sym(name, SymKind::Function, span.start, span.end, span.line, span.col, fn_detail(name, type_params, params, ret), &format!("method of {type_name}")));
                }
            }
        }
        Stmt::ConstDef { name, span, .. } => {
            syms.push(sym(name, SymKind::Const, span.start, span.end, span.line, span.col, format!("const {name}"), "constant"));
        }
        Stmt::ModDef { name, items, span, .. } => {
            syms.push(sym(name, SymKind::Module, span.start, span.end, span.line, span.col, format!("mod {name}"), "module"));
            walk_stmts(items, syms, src);
        }
        Stmt::Let { name, ty_ann, span, .. } => {
            // The `let` statement's span points at the keyword; locate the bound
            // identifier's real position (after `let` + whitespace) so go-to-definition
            // and hover land on the name itself.
            let name_start = skip_ws(src, span.start + 3);
            let (nl, nc) = line_col_of(src, name_start);
            let detail = ty_ann
                .as_ref()
                .map(|t| format!("let {name}: {}", type_to_str(t)))
                .unwrap_or_else(|| format!("let {name}"));
            syms.push(sym(name, SymKind::Variable, span.start, span.end, nl, nc, detail, "variable"));
        }
        _ => {}
    }
}

fn collect_body_locals(stmts: &[Stmt], syms: &mut Vec<Sym>, src: &str) {
    for s in stmts {
        match s {
            Stmt::Let { name, ty_ann, span, .. } => {
                let name_start = skip_ws(src, span.start + 3);
                let (nl, nc) = line_col_of(src, name_start);
                let detail = ty_ann
                    .as_ref()
                    .map(|t| format!("let {name}: {}", type_to_str(t)))
                    .unwrap_or_else(|| format!("let {name}"));
                syms.push(sym(name, SymKind::Variable, span.start, span.end, nl, nc, detail, "variable"));
            }
            Stmt::If { then_body, else_body, .. } => {
                collect_body_locals(then_body, syms, src);
                collect_body_locals(else_body, syms, src);
            }
            Stmt::While { body, .. } | Stmt::Loop { body, .. } => collect_body_locals(body, syms, src),
            Stmt::For { var, body, .. } => {
                syms.push(sym(var, SymKind::Variable, 0, 0, 0, 0, format!("for {var}"), "loop variable"));
                collect_body_locals(body, syms, src);
            }
            Stmt::Match { arms, .. } => {
                for a in arms {
                    collect_body_locals(&a.body, syms, src);
                }
            }
            _ => {}
        }
    }
}

// --- position helpers (LSP line/char are 0-based) ---

fn pos_to_offset(source: &str, line: u32, character: u32) -> usize {
    let bytes = source.as_bytes();
    let mut off = 0usize;
    let mut cur = 0u32;
    while off < bytes.len() && cur < line {
        if bytes[off] == b'\n' {
            cur += 1;
        }
        off += 1;
    }
    let line_end = source[off..].find('\n').map(|i| off + i).unwrap_or(source.len());
    (off + character as usize).min(line_end)
}

fn is_id_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn word_at(source: &str, offset: usize) -> Option<String> {
    let bytes = source.as_bytes();
    if offset >= bytes.len() || !is_id_char(bytes[offset]) {
        return None;
    }
    let mut start = offset;
    while start > 0 && is_id_char(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = offset;
    while end < bytes.len() && is_id_char(bytes[end]) {
        end += 1;
    }
    Some(source[start..end].to_string())
}

/// Language keywords offered for completion.
const KEYWORDS: &[&str] = &[
    "let", "print", "if", "else", "while", "loop", "for", "in", "fn", "return", "break",
    "continue", "true", "false", "mut", "arena", "tensor", "extern", "gpu", "match", "struct",
    "union", "enum", "trait", "impl", "type", "dyn", "as", "const", "mod", "use", "pub", "crate",
    "Self",
];

/// Common prelude/std builtins offered alongside user symbols.
const STD_ITEMS: &[(&str, &str)] = &[
    ("String", "builtin string type"),
    ("Vec", "builtin vector type"),
    ("Box", "builtin box type"),
    ("Option", "builtin option type"),
    ("Result", "builtin result type"),
    ("Some", "option constructor"),
    ("None", "option constant"),
    ("Ok", "result constructor"),
    ("Err", "result constructor"),
    ("assert", "assert builtin"),
    ("len", "length builtin"),
    ("print", "print builtin"),
    ("tensor_add", "tensor elementwise add"),
    ("matmul", "tensor matmul builtin"),
    ("sum", "tensor reduce sum"),
    ("blas_dot", "BLAS dot builtin"),
];

fn kind_num(k: SymKind) -> u32 {
    match k {
        SymKind::Function => 3,
        SymKind::Variable => 6,
        SymKind::Struct => 7,
        SymKind::Union => 7,
        SymKind::Enum => 10,
        SymKind::Trait => 8,
        SymKind::Const => 5,
        SymKind::Module => 9,
        SymKind::Variant => 19,
    }
}

/// Build a `textDocument/completion` result array for the identifier prefix ending
/// at `(line, character)`. LSP `CompletionItemKind`: 3 fn, 5 const, 6 var, 7 struct,
/// 8 interface, 9 module, 10 enum, 14 keyword, 19 enum member.
fn handle_completion(source: &str, line: u32, character: u32) -> String {
    let off = pos_to_offset(source, line, character);
    let mut start = off;
    let bytes = source.as_bytes();
    while start > 0 && is_id_char(bytes[start - 1]) {
        start -= 1;
    }
    let prefix = &source[start..off];
    let syms = index_source(source);

    let mut seen: Vec<String> = Vec::new();
    let mut items: Vec<String> = Vec::new();
    let push = |name: &str, kind: u32, detail: &str, seen: &mut Vec<String>, items: &mut Vec<String>| {
        if name.starts_with(prefix) && !seen.iter().any(|s| s == name) {
            seen.push(name.to_string());
            items.push(format!(
                "{{\"label\":{},\"kind\":{},\"detail\":{}}}",
                jstr(name),
                kind,
                jstr(detail)
            ));
        }
    };
    for s in &syms {
        push(&s.name, kind_num(s.kind), &s.detail, &mut seen, &mut items);
    }
    for k in KEYWORDS {
        push(k, 14, "keyword", &mut seen, &mut items);
    }
    for (n, d) in STD_ITEMS {
        push(n, 6, d, &mut seen, &mut items);
    }
    format!("[{}]", items.join(","))
}

/// Build the markdown hover body for the symbol under `(line, character)`.
fn handle_hover(source: &str, line: u32, character: u32) -> String {
    let off = pos_to_offset(source, line, character);
    let syms = index_source(source);
    for s in &syms {
        if s.end > 0 && s.start <= off && off <= s.end {
            return format!("```aero\n{}\n```\n_{}_", s.detail, s.doc);
        }
    }
    if let Some(w) = word_at(source, off) {
        if let Some(s) = syms.iter().find(|x| x.name == w) {
            return format!("```aero\n{}\n```\n_{}_", s.detail, s.doc);
        }
    }
    String::new()
}

/// Build a single `Location` object for the declaration of the identifier under
/// `(line, character)`, or an empty string when no target resolves.
fn handle_definition(source: &str, line: u32, character: u32, uri: &str) -> String {
    let off = pos_to_offset(source, line, character);
    let syms = index_source(source);
    if let Some(w) = word_at(source, off) {
        if let Some(s) = syms.iter().find(|x| x.name == w) {
            let start_char = s.col.saturating_sub(1);
            let end_char = start_char + s.name.len() as u32;
            return format!(
                "\"uri\":{},\"range\":{{\"start\":{{\"line\":{},\"character\":{}}},\"end\":{{\"line\":{},\"character\":{}}}}}",
                jstr(uri),
                s.line.saturating_sub(1),
                start_char,
                s.line.saturating_sub(1),
                end_char
            );
        }
    }
    String::new()
}

// ---------------------------------------------------------------------------
// Server loop
// ---------------------------------------------------------------------------

struct Server {
    docs: HashMap<String, String>,
}

impl Server {
    fn new() -> Self {
        Server {
            docs: HashMap::new(),
        }
    }

    fn handle(&mut self, msg: &Message, out: &mut impl Write) -> bool {
        match msg.method.as_str() {
            "initialize" => {
                let body = format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{{\"capabilities\":{{\"textDocumentSync\":1,\"hoverProvider\":true,\"diagnosticsProvider\":true,\"definitionProvider\":true,\"completionProvider\":{{\"triggerCharacters\":[\".\",\":\"]}}}}}}}}",
                    json_id(&msg.id)
                );
                write_message(out, &body);
                true
            }
            "initialized" | "textDocument/didSave" => true,
            "shutdown" => {
                let body = format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":null}}",
                    json_id(&msg.id)
                );
                write_message(out, &body);
                true
            }
            "exit" => false,
            "textDocument/didOpen" => {
                let uri = msg.params.get("textDocument").and_then(|d| d.get("uri")).map(|j| j.as_str_or("")).unwrap_or_default();
                let text = msg.params.get("textDocument").and_then(|d| d.get("text")).map(|j| j.as_str_or("")).unwrap_or_default();
                self.docs.insert(uri.clone(), text);
                if let Some(src) = self.docs.get(&uri) {
                    publish_diagnostics(out, &uri, src);
                }
                true
            }
            "textDocument/didChange" => {
                if let Some(uri) = msg.params.get("textDocument").and_then(|d| d.get("uri")).map(|j| j.as_str_or("")) {
                    // Take the full text if the change is a whole-document replace,
                    // otherwise fall back to the current buffer.
                    let new_text = msg
                        .params
                        .get("contentChanges")
                        .and_then(|c| c.at(0))
                        .and_then(|c| c.get("text"))
                        .map(|j| j.as_str_or(""))
                        .unwrap_or_default();
                    if !new_text.is_empty() {
                        self.docs.insert(uri.to_string(), new_text);
                        if let Some(src) = self.docs.get(&uri) {
                            publish_diagnostics(out, &uri, src);
                        }
                    }
                }
                true
            }
            "textDocument/didClose" => {
                if let Some(uri) = msg.params.get("textDocument").and_then(|d| d.get("uri")).map(|j| j.as_str_or("")) {
                    self.docs.remove(&uri);
                    // Clear diagnostics on close.
                    let body = format!("{{\"jsonrpc\":\"2.0\",\"method\":\"textDocument/publishDiagnostics\",\"params\":{{\"uri\":{},\"diagnostics\":[]}}}}", jstr_str(&uri));
                    write_message(out, &body);
                }
                true
            }
            "textDocument/hover" => {
                let uri = msg.params.get("textDocument").and_then(|d| d.get("uri")).map(|j| j.as_str_or("")).unwrap_or_default();
                let line = msg.params.get("position").and_then(|p| p.get("line")).and_then(|j| j.as_u64()).unwrap_or(0) as u32;
                let character = msg.params.get("position").and_then(|p| p.get("character")).and_then(|j| j.as_u64()).unwrap_or(0) as u32;
                let value = self.docs.get(&uri).map(|src| handle_hover(src, line, character)).unwrap_or_default();
                let body = format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{}}}",
                    json_id(&msg.id),
                    if value.is_empty() {
                        "null".to_string()
                    } else {
                        format!("{{\"contents\":{{\"kind\":\"markdown\",\"value\":{}}}}}", jstr(&value))
                    }
                );
                write_message(out, &body);
                true
            }
            "textDocument/definition" => {
                let uri = msg.params.get("textDocument").and_then(|d| d.get("uri")).map(|j| j.as_str_or("")).unwrap_or_default();
                let line = msg.params.get("position").and_then(|p| p.get("line")).and_then(|j| j.as_u64()).unwrap_or(0) as u32;
                let character = msg.params.get("position").and_then(|p| p.get("character")).and_then(|j| j.as_u64()).unwrap_or(0) as u32;
                let loc = self.docs.get(&uri).map(|src| handle_definition(src, line, character, &uri)).unwrap_or_default();
                let body = format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{}}}",
                    json_id(&msg.id),
                    if loc.is_empty() { "null".to_string() } else { format!("{{{loc}}}") }
                );
                write_message(out, &body);
                true
            }
            "textDocument/completion" => {
                let uri = msg.params.get("textDocument").and_then(|d| d.get("uri")).map(|j| j.as_str_or("")).unwrap_or_default();
                let line = msg.params.get("position").and_then(|p| p.get("line")).and_then(|j| j.as_u64()).unwrap_or(0) as u32;
                let character = msg.params.get("position").and_then(|p| p.get("character")).and_then(|j| j.as_u64()).unwrap_or(0) as u32;
                let items = self.docs.get(&uri).map(|src| handle_completion(src, line, character)).unwrap_or_else(|| "[]".to_string());
                let body = format!(
                    "{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":{}}}",
                    json_id(&msg.id),
                    items
                );
                write_message(out, &body);
                true
            }
            _ => {
                if msg.id.is_some() {
                    let body = format!(
                        "{{\"jsonrpc\":\"2.0\",\"id\":{},\"error\":{{\"code\":-32601,\"message\":\"method not found: {}\"}}}}",
                        json_id(&msg.id),
                        msg.method
                    );
                    write_message(out, &body);
                }
                true
            }
        }
    }
}

fn json_id(id: &Option<i64>) -> String {
    match id {
        Some(n) => n.to_string(),
        None => "null".to_string(),
    }
}

/// Run the LSP server until `exit` or EOF on stdin.
pub fn run_lsp() -> u8 {
    let stdin = std::io::stdin();
    let mut reader = std::io::BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut server = Server::new();
    loop {
        match read_message(&mut reader) {
            Some(msg) => {
                let continue_loop = server.handle(&msg, &mut out);
                if !continue_loop {
                    break;
                }
            }
            None => break,
        }
    }
    0
}