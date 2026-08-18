//! `aero fmt` — a token-based source formatter for the Aero language.
//!
//! Approach: lex the source into a token stream (each token carries byte offsets
//! into the original source), then rebuild well-formatted code by applying
//! spacing / indentation / line-breaking rules to that stream. Token *text* is
//! sliced from the original source (`&source[start..end]`) so string/number
//! literals keep their exact spelling.
//!
//! The formatter is intentionally conservative: it only reflows whitespace and
//! line breaks. It never reorders tokens and never changes literal spellings, so
//! the output always lexes + parses identically to the input (verified by the
//! CLI tests, which re-parse the formatted output).
//!
//! Comments are re-inserted from the original source (the lexer blanks them out):
//! - `//` line comments and `/* */` block comments are both preserved.
//! - A comment that shared a line with the token to its left is kept *trailing*
//!   on that same line; a comment on its own line stays on its own line.
//! - Up to one blank `source` line is preserved between statements, so logical
//!   groupings in the source survive formatting.
//! - `struct`/`union` bodies are laid out one field per line, and consecutive
//!   `name: Type` fields are aligned so the `:` colons line up vertically.

use aero_lex::token::{Token, TokenKind};
use std::collections::{HashMap, HashSet};

/// Formatting options (mirrors the rustfmt-style knobs aero supports).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FmtOptions {
    /// Long lines are broken at safe points once this total line width is exceeded.
    pub max_width: usize,
    /// Indentation amount (spaces per level) used for block/continuation indent.
    pub indent: usize,
}

impl Default for FmtOptions {
    fn default() -> Self {
        FmtOptions { max_width: 80, indent: 4 }
    }
}

/// Format Aero source code with the default options. Returns the reformatted
/// text, or an error (with line/col) if the source fails to lex.
pub fn format(source: &str) -> Result<String, String> {
    format_with(source, &FmtOptions::default())
}

/// Format Aero source code with explicit options.
pub fn format_with(source: &str, opts: &FmtOptions) -> Result<String, String> {
    // Pre-processing: sort `use` imports (stable sort, rustfmt-level).
    let sorted = sort_imports(source)?;

    let tokens = aero_lex::lex(&sorted)
        .map_err(|e| format!("line {} col {}: {}", e.line, e.col, e.msg))?;

    let comments = extract_comments(&sorted);
    let (trailing, inline_lead, own_line, tail) = classify_comments(&sorted, &comments, &tokens);

    // Field `:` colons to align -> column-offset (relative to the entry indent).
    // token idx that start a fresh struct field line (one per line).
    let (align_cols, break_before) = collect_struct_layout(&tokens);

    let mut w = Writer::new(opts);
    let mut prev: Option<&Token> = None;

    for (i, t) in tokens.iter().enumerate() {
        // The token text doubles as the width probe for line breaking below.
        let text = &sorted[t.start..t.end];
        let tok_w = text.chars().count();

        // 1. Structural line break (or inline space) before this token.
        if let Some(p) = prev {
            // A struct field that should start on its own line wins first.
            if break_before.contains(&i) {
                w.newline();
            } else {
                match p.kind {
                    TokenKind::LBrace => w.newline(),
                    TokenKind::RBrace => match t.kind {
                        // `} else {` needs a space.
                        TokenKind::Else => w.space(),
                        // Tight closers: `};`, `},`, `})`, `}}`.
                        TokenKind::Semi | TokenKind::Comma | TokenKind::RParen
                        | TokenKind::RBrace => {}
                        _ => break_or_blank(&sorted, p.end, t.start, &mut w),
                    },
                    TokenKind::Semi => break_or_blank(&sorted, p.end, t.start, &mut w),
                    _ => {
                        if space_between(&p.kind, &t.kind) {
                            // Long-line breaking at safe operator/comma points.
                            if breakable_after(&p.kind)
                                && !w.line_start
                                && w.line_w + 1 + tok_w > w.max_width
                            {
                                w.continue_break();
                            } else {
                                w.space();
                            }
                        }
                    }
                }
            }
        }

        // 2. Standalone comments that lead this token.
        for c in own_line.get(&i).into_iter().flatten() {
            w.own_line(c);
        }
        // Comments on the same line as this token, before it (`/* c */ x`).
        for c in inline_lead.get(&i).into_iter().flatten() {
            w.inline(c);
            w.space();
        }

        // 3. Write the token (brace-indent bookkeeping inline).
        let text = &sorted[t.start..t.end];
        match t.kind {
            TokenKind::LBrace => {
                w.write("{");
                w.indent += 1;
            }
            TokenKind::RBrace => {
                if w.indent > 0 {
                    w.indent -= 1;
                }
                w.write("}");
            }
            TokenKind::Semi => w.write(";"),
            TokenKind::Comma => w.write(","),
            TokenKind::Colon => {
                if let Some(rel) = align_cols.get(&i) {
                    w.pad_to((w.indent + w.cont) * w.indent_w + *rel);
                }
                w.write(":");
            }
            _ => w.write(text),
        }

        // 4. Trailing comments stay on this line, after the token.
        if let Some(list) = trailing.get(&i) {
            for c in list {
                w.inline(c);
            }
        }

        prev = Some(t);
    }

    // Tail: any comments after the last token.
    for c in &tail {
        w.own_line(c);
    }

    // Ensure the output ends with exactly one newline.
    if !w.out.is_empty() && !w.out.ends_with('\n') {
        w.out.push('\n');
    }
    Ok(w.out)
}

/// Sort blocks of consecutive `use` / `pub use` import statements in the
/// source.  Within each blank-line-separated group, statements are sorted
/// alphabetically (stable sort, matching rustfmt behaviour).
///
/// Comments attached to a `use` line (trailing or inline-lead) stay with
/// their original line position; own-line comments between `use` statements
/// are preserved but not reordered.
fn sort_imports(source: &str) -> Result<String, String> {
    let tokens = aero_lex::lex(source)
        .map_err(|e| format!("line {} col {}: {}", e.line, e.col, e.msg))?;

    // ---- Step 1: locate every import statement (`use` or `pub use` … `;`) ----
    let mut imports: Vec<(usize, usize)> = Vec::new(); // (start_byte, end_byte)
    let mut i = 0usize;
    while i < tokens.len() {
        let start = if tokens[i].kind == TokenKind::Pub {
            // Advance past `pub` and check for `use`
            i += 1;
            if i < tokens.len() && tokens[i].kind == TokenKind::Use {
                tokens[i - 1].start // start at `pub`
            } else {
                continue;
            }
        } else if tokens[i].kind == TokenKind::Use {
            tokens[i].start
        } else {
            i += 1;
            continue;
        };

        // Scan forward to the terminating `;`
        let mut j = i + 1;
        while j < tokens.len() && tokens[j].kind != TokenKind::Semi {
            j += 1;
        }
        if j < tokens.len() {
            // Extend past `;` to include any trailing content on the same line
            // (comments, whitespace — the `;` end byte is the byte *after* `;`).
            let mut end = tokens[j].end;
            while end < source.len() && source.as_bytes()[end] != b'\n' {
                end += 1;
            }
            imports.push((start, end));
            i = j + 1;
        } else {
            i += 1;
        }
    }

    if imports.is_empty() {
        return Ok(source.to_string());
    }

    // ---- Step 2: group into blocks separated by blank lines ----
    let mut blocks: Vec<Vec<(usize, usize, &str)>> = Vec::new();
    let mut cur: Vec<(usize, usize, &str)> = Vec::new();
    for (idx, &(s, e)) in imports.iter().enumerate() {
        let text = &source[s..e];
        if idx > 0 {
            let prev_end = imports[idx - 1].1;
            let gap = &source[prev_end..s];
            // A blank line (2+ consecutive newlines) separates groups.
            if gap.matches('\n').count() >= 2 {
                if !cur.is_empty() {
                    blocks.push(std::mem::take(&mut cur));
                }
            }
        }
        cur.push((s, e, text));
    }
    if !cur.is_empty() {
        blocks.push(cur);
    }

    // ---- Step 3: rebuild source with sorted imports ----
    let mut result = String::with_capacity(source.len());
    let mut pos = 0usize;

    for block in &blocks {
        // Copy everything before this block unchanged.
        let block_start = block[0].0;
        result.push_str(&source[pos..block_start]);

        // Sort the statement texts of this block.
        let mut texts: Vec<&str> = block.iter().map(|(_, _, t)| *t).collect();
        texts.sort();

        // Join with newlines (original `;` is part of each text slice).
        for (k, t) in texts.iter().enumerate() {
            if k > 0 {
                result.push('\n');
            }
            result.push_str(t);
        }

        pos = block.last().unwrap().1;
    }

    // Copy everything after the last block.
    result.push_str(&source[pos..]);

    Ok(result)
}

/// A comment recovered from the original source.
#[derive(Debug)]
struct Comment {
    start: usize,
    end: usize,
    text: String,
}

/// Scan the source for `//` line comments and `/* */` block comments, returning
/// them in byte order. Line-comment text excludes the trailing newline; block
/// text includes its `/* ... */` markers and any internal newlines.
fn extract_comments(source: &str) -> Vec<Comment> {
    let bytes = source.as_bytes();
    let mut out: Vec<Comment> = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            let start = i;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            out.push(Comment { start, end: i, text: source[start..i].to_string() });
        } else if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            let start = i;
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len()); // consume `*/` (or run to EOF)
            out.push(Comment { start, end: i, text: source[start..i].to_string() });
        } else {
            i += 1;
        }
    }
    out
}

/// Partition comments into `(trailing_by_token, inline_lead_by_token,
/// own_line_by_token, tail)`.
///
/// - *Trailing*: on the same source line as the nearest token ending before it.
/// - *Inline-lead*: on the same source line as the token that follows it
///   (`/* c */ let x`).
/// - *Own-line*: on its own source line, attached to the following token.
fn classify_comments(
    source: &str,
    comments: &[Comment],
    tokens: &[Token],
) -> (HashMap<usize, Vec<String>>, HashMap<usize, Vec<String>>, HashMap<usize, Vec<String>>, Vec<String>) {
    let mut trailing: HashMap<usize, Vec<String>> = HashMap::new();
    let mut inline_lead: HashMap<usize, Vec<String>> = HashMap::new();
    let mut own_line: HashMap<usize, Vec<String>> = HashMap::new();
    let mut tail: Vec<String> = Vec::new();

    for c in comments {
        let same_line_left = tokens
            .iter()
            .rposition(|tk| tk.end <= c.start)
            .map(|li| !source[tokens[li].end..c.start].contains('\n'))
            .unwrap_or(false);
        if same_line_left {
            let li = tokens.iter().rposition(|tk| tk.end <= c.start).unwrap();
            trailing.entry(li).or_default().push(c.text.clone());
            continue;
        }
        match tokens.iter().position(|tk| tk.start >= c.end) {
            Some(ri) => {
                let same_line_right = !source[c.end..tokens[ri].start].contains('\n');
                let bucket = if same_line_right { &mut inline_lead } else { &mut own_line };
                bucket.entry(ri).or_default().push(c.text.clone());
            }
            None => tail.push(c.text.clone()),
        }
    }
    (trailing, inline_lead, own_line, tail)
}

/// Emit a single line break, or one blank line when `[a, b)` in the source
/// contained a blank line.
fn break_or_blank(source: &str, a: usize, b: usize, w: &mut Writer) {
    if source_blank_between(source, a, b) {
        w.blank();
    } else {
        w.newline();
    }
}

/// True when the source slice `[a, b)` contains a fully blank line (two or more
/// consecutive newlines), meaning the user separated statements with a blank.
fn source_blank_between(source: &str, a: usize, b: usize) -> bool {
    let seg = &source[a..b.min(source.len())];
    let mut nl = 0usize;
    for ch in seg.chars() {
        if ch == '\n' {
            nl += 1;
            if nl >= 2 {
                return true;
            }
        } else if ch != ' ' && ch != '\t' && ch != '\r' {
            return false;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// struct / union field layout + alignment
// ---------------------------------------------------------------------------

/// Compute, for every `struct`/`union` body:
/// - `align_cols`: map from the `:` token index to the relative output column
///   (offset from the body indent) that colons should be padded up to.
/// - `break_before`: token indices that begin a non-first field (own line).
fn collect_struct_layout(
    tokens: &[Token],
) -> (HashMap<usize, usize>, HashSet<usize>) {
    let mut align: HashMap<usize, usize> = HashMap::new();
    let mut brk: HashSet<usize> = HashSet::new();
    let mut i = 0usize;
    while i < tokens.len() {
        if !matches!(tokens[i].kind, TokenKind::Struct | TokenKind::Union) {
            i += 1;
            continue;
        }
        let Some(open) = find_body_brace(tokens, i + 1) else { i += 1; continue };
        let Some((entries, close)) = collect_body_entries(tokens, open) else {
            i += 1;
            continue;
        };
        // entries: (entry_start_idx, colon_idx, colon_rel_col)
        if entries.len() >= 2 {
            let target = entries.iter().map(|e| e.2).max().unwrap_or(0);
            // The closing brace also starts on its own line after the last field.
            brk.insert(close);
            for (start, colon, _) in entries {
                if colon != usize::MAX {
                    align.insert(colon, target);
                }
                if start != usize::MAX {
                    brk.insert(start);
                }
            }
        }
        i = close + 1;
    }
    (align, brk)
}

/// Find the `{` that opens a struct/union body, scanning forward from the
/// keyword + name (skipping the name and any generic `<...>` type parameters).
fn find_body_brace(tokens: &[Token], from: usize) -> Option<usize> {
    let mut i = from;
    while i < tokens.len() {
        match tokens[i].kind {
            TokenKind::Ident(_) | TokenKind::DoubleColon => i += 1,
            TokenKind::Lt => {
                let mut depth = 1usize;
                i += 1;
                while i < tokens.len() && depth > 0 {
                    match tokens[i].kind {
                        TokenKind::Lt => depth += 1,
                        TokenKind::Gt => depth -= 1,
                        _ => {}
                    }
                    i += 1;
                }
            }
            TokenKind::LBrace => return Some(i),
            _ => return None,
        }
    }
    None
}

/// Split the tokens strictly inside a body block into top-level fields (on
/// commas at depth 0). Returns `(entries, closing_brace_idx)`. Each entry is
/// `(start_idx, colon_idx, colon_rel_col)`; `colon_idx`/rel are `usize::MAX`
/// when the field is not of the `name: Type` shape.
fn collect_body_entries(tokens: &[Token], open: usize) -> Option<(Vec<(usize, usize, usize)>, usize)> {
    let mut depth = 0usize;
    let mut fields: Vec<(usize, usize, usize)> = Vec::new();
    let mut start = open + 1;
    let mut i = open + 1;
    while i < tokens.len() {
        match tokens[i].kind {
            TokenKind::LBrace | TokenKind::LParen | TokenKind::LBracket => depth += 1,
            TokenKind::RBrace if depth == 0 => {
                if i > start {
                    fields.push(field_entry(tokens, start, i));
                }
                return Some((fields, i));
            }
            TokenKind::RBrace | TokenKind::RParen | TokenKind::RBracket => depth = depth.saturating_sub(1),
            TokenKind::Comma if depth == 0 => {
                if i > start {
                    fields.push(field_entry(tokens, start, i));
                }
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Describe one field slice `[start, end)` (exclusive). Returns
/// `(start, colon_idx, colon_rel_col)`; colons that are not the first token at
/// depth 0 produce `(usize::MAX, usize::MAX, 0)`.
fn field_entry(tokens: &[Token], start: usize, end: usize) -> (usize, usize, usize) {
    let mut depth = 0usize;
    let mut j = start;
    while j < end {
        match tokens[j].kind {
            TokenKind::LBrace | TokenKind::LParen | TokenKind::LBracket => depth += 1,
            TokenKind::RBrace | TokenKind::RParen | TokenKind::RBracket => depth = depth.saturating_sub(1),
            TokenKind::Colon if depth == 0 && j > start => {
                // Relative column = width of `tokens[start..=j-1]` (names + any
                // leading `pub`/`mut`) with single spaces, then the `:` glues on.
                let mut rel = 0usize;
                for k in start..j {
                    rel += (tokens[k].end - tokens[k].start).max(1) + 1;
                }
                return (start, j, rel.saturating_sub(1));
            }
            _ => {}
        }
        j += 1;
    }
    (usize::MAX, usize::MAX, 0)
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

/// Accumulates formatted output, managing indentation / line-width / line-start
/// state. `indent` counts the *block nesting depth* (incremented on `{` and
/// decremented on `}`); the output column *per level* is `indent_w` (from
/// [`FmtOptions::indent`]). Once `line_w` would exceed `max_width`, long lines
/// break at a safe point and the continuation gets one extra hanging level
/// (`cont`), matching rustfmt's style.
struct Writer {
    out: String,
    /// Block nesting depth (`{`/`}` bookkeeping).
    indent: usize,
    /// Configurable spaces per block level.
    indent_w: usize,
    /// Configurable max line width (in chars).
    max_width: usize,
    /// Width of the current output line in chars (from the last newline).
    line_w: usize,
    /// Hanging-indent levels applied to continuation lines.
    cont: usize,
    /// True when the next `write` is at the start of a line.
    line_start: bool,
}

impl Writer {
    fn new(opts: &FmtOptions) -> Self {
        Writer {
            out: String::new(),
            indent: 0,
            indent_w: opts.indent,
            max_width: opts.max_width,
            line_w: 0,
            cont: 0,
            line_start: true,
        }
    }

    /// Start a new line, unless already at the start of a non-empty line.
    fn newline(&mut self) {
        if self.line_start {
            return;
        }
        self.out.push('\n');
        self.line_start = true;
        self.line_w = 0;
        self.cont = 0;
    }

    /// Insert a single blank line (never more than one).
    fn blank(&mut self) {
        if self.out.is_empty() || self.out.ends_with("\n\n") {
            return;
        }
        if !self.line_start {
            self.out.push('\n');
            self.line_start = true;
        }
        self.out.push('\n');
        self.line_w = 0;
        self.cont = 0;
    }

    /// Write a single space (never at line start, never doubling).
    fn space(&mut self) {
        if self.line_start {
            return;
        }
        if !self.out.ends_with(' ') {
            self.out.push(' ');
            self.line_w += 1;
        }
    }

    /// Write text, applying the block + continuation indent at line start.
    fn write(&mut self, s: &str) {
        if self.line_start {
            let columns = (self.indent + self.cont) * self.indent_w;
            for _ in 0..columns {
                self.out.push(' ');
            }
            self.line_w = columns;
            self.line_start = false;
        }
        self.out.push_str(s);
        self.line_w += s.chars().count();
    }

    /// Break onto a new line and apply one hanging-indent level (used when a
    /// long line is broken at an operator/comma). Only meaningful mid-line.
    fn continue_break(&mut self) {
        if self.line_start {
            self.cont = 1;
            return;
        }
        self.out.push('\n');
        self.line_w = 0;
        self.line_start = true;
        self.cont = 1;
    }

    /// Pad the current line so its visible width reaches at least `col` chars
    /// measured from the line start. No-op once past it.
    fn pad_to(&mut self, col: usize) {
        if self.line_start {
            return;
        }
        let line_start_off = self.out.rfind('\n').map(|p| p + 1).unwrap_or(0);
        let line_w = self.out[line_start_off..].chars().count();
        for _ in 0..col.saturating_sub(line_w) {
            self.out.push(' ');
        }
        if col > line_w {
            self.line_w = col;
        }
    }

    /// Write a comment on the same (current) line: ` // ...`.
    fn inline(&mut self, text: &str) {
        self.space();
        self.write(text);
    }

    /// Write a standalone comment on its own line at the current indent.
    fn own_line(&mut self, text: &str) {
        if !self.line_start {
            self.out.push('\n');
            self.line_start = true;
        }
        self.line_w = 0;
        self.cont = 0;
        self.write(text);
        self.newline();
    }
}

/// Decide whether a space is required between two adjacent tokens.
fn space_between(prev: &TokenKind, cur: &TokenKind) -> bool {
    use TokenKind::*;
    // Tight left: these must glue to the token before them.
    match cur {
        RParen | RBracket | RBrace | Comma | Semi | Dot | DoubleColon | Question | Colon => return false,
        _ => {}
    }
    // Tight right: these must glue to the token after them.
    match prev {
        LParen | LBracket | Dot | DoubleColon => return false,
        _ => {}
    }
    // `(` after a control keyword needs a space (`if (x)`); after anything else
    // it is a call/index and stays tight (`f(x)`).
    if matches!(cur, LParen) {
        return matches!(prev, If | While | For | Return | Match);
    }
    if matches!(cur, LBracket) {
        return false;
    }
    true
}

/// Whether it is safe to break the line *after* a token of kind `k`. Long lines
/// are broken only after infix operators, arrows and commas (the same points
/// rustfmt treats as soft-wrappable). Glue punctuation like `.` and `::` is
/// never a break point, so a break never separates an accessor from its target.
fn breakable_after(k: &TokenKind) -> bool {
    use TokenKind::*;
    matches!(
        k,
        Plus | Minus | Star | Slash | Percent | Amp | Caret | Pipe | Shl | Shr | Eq
            | EqEq | Ne | Le | Ge | Gt | Lt | AndAnd | OrOr | Arrow | FatArrow | Comma
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_let() {
        let src = "let x=1; let y = 2;";
        assert_eq!(format(src).unwrap(), "let x = 1;\nlet y = 2;\n");
    }

    #[test]
    fn call_and_print() {
        let src = "print(\"hi\");";
        assert_eq!(format(src).unwrap(), "print(\"hi\");\n");
    }

    #[test]
    fn control_keyword_paren_space() {
        let src = "if(x>0){print(1);}";
        assert_eq!(format(src).unwrap(), "if (x > 0) {\n    print(1);\n}\n");
    }

    #[test]
    fn struct_hoists_each_field_to_its_own_line() {
        let src = "struct Point{x:i64,y:i64}";
        assert_eq!(format(src).unwrap(), "struct Point {\n    x: i64,\n    y: i64\n}\n");
    }

    #[test]
    fn leading_and_trailing_comments() {
        let src = "// header\nlet x = 1; // trailing\n";
        assert_eq!(format(src).unwrap(), "// header\nlet x = 1; // trailing\n");
    }

    #[test]
    fn preserves_literal_spelling() {
        let src = "let s = \"a\\nb\"; let n = 3.14;";
        assert_eq!(format(src).unwrap(), "let s = \"a\\nb\";\nlet n = 3.14;\n");
    }

    #[test]
    fn else_brace() {
        let src = "if (a) { f(); } else { g(); }";
        assert_eq!(
            format(src).unwrap(),
            "if (a) {\n    f();\n} else {\n    g();\n}\n"
        );
    }

    #[test]
    fn block_comments_preserved() {
        let src = "/* top */ let x = 1; /* trailing */";
        assert_eq!(
            format(src).unwrap(),
            "/* top */ let x = 1; /* trailing */\n"
        );
    }

    #[test]
    fn block_comments_standalone() {
        let src = "fn f() {\n    /* multi\n     * line */\n    g();\n}\n";
        // Leading block comment is re-emitted verbatim, preserving inner layout.
        assert_eq!(format(src).unwrap(), "fn f() {\n    /* multi\n     * line */\n    g();\n}\n");
    }

    #[test]
    fn blank_line_preserved_between_statements() {
        let src = "let a = 1;\n\nlet b = 2;\n";
        assert_eq!(format(src).unwrap(), "let a = 1;\n\nlet b = 2;\n");
    }

    #[test]
    fn struct_field_colons_aligned() {
        let src = "struct Config{a:i64,named_long:str,t:bool}";
        assert_eq!(
            format(src).unwrap(),
            "struct Config {\n    a         : i64,\n    named_long: str,\n    t         : bool\n}\n"
        );
    }

    #[test]
    fn long_line_broken_at_operator() {
        // Narrow width forces a break at `beta` (after the interoperable `+`),
        // with a single hanging-indent level (rustfmt style). The `+` trailing
        // on the first line is the conservative single-token-lookbehind style.
        let src = "let value = alpha + beta + gamma;";
        let opts = FmtOptions { max_width: 20, indent: 4 };
        assert_eq!(
            format_with(src, &opts).unwrap(),
            "let value = alpha +\n    beta + gamma;\n"
        );
    }

    #[test]
    fn long_line_break_is_glue_safe() {
        // A member access `obj.field` and a call `f(x)` never break at the
        // glue (`.` / `(` / `::`), only at trailing operators/commas.
        let src = "let total = compute(serialized) + apply(matrices).sum() + extra_term;";
        let opts = FmtOptions { max_width: 28, indent: 2 };
        let out = format_with(src, &opts).unwrap();
        assert!(out.contains("\n  "), "expected a continuation line, got:\n{out}");
        assert!(!out.contains(".\n"), "`.` must not be a break point:\n{out}");
        assert!(!out.contains("(\n"), "`(` must not be a break point:\n{out}");
        // Indivisible glue runs (e.g. `compute(serialized)`) can exceed the
        // width on their own, so allow a small tolerance.
        for line in out.lines() {
            assert!(
                line.chars().count() <= 38,
                "line exceeds allowed width tolerance: {line:?}"
            );
        }
    }

    #[test]
    fn custom_indent_size() {
        let src = "fn f(){ let x=1; }";
        let opts = FmtOptions { max_width: 80, indent: 2 };
        assert_eq!(format_with(src, &opts).unwrap(), "fn f() {\n  let x = 1;\n}\n");
    }

    #[test]
    fn wide_max_width_keeps_line_flat() {
        let src = "let value = alpha + beta + gamma;";
        let opts = FmtOptions { max_width: 120, indent: 4 };
        assert_eq!(format_with(src, &opts).unwrap(), "let value = alpha + beta + gamma;\n");
    }

    // -----------------------------------------------------------------------
    // Import sorting
    // -----------------------------------------------------------------------

    #[test]
    fn sort_simple_uses() {
        let src = "use zeta;\nuse alpha;\nuse beta;\n";
        assert_eq!(format(src).unwrap(), "use alpha;\nuse beta;\nuse zeta;\n");
    }

    #[test]
    fn sort_with_pub_use() {
        // `pub use` texts start with `p` (< `u`), so they sort before bare `use` lines.
        let src = "pub use zeta;\nuse alpha;\npub use beta;\n";
        let out = format(src).unwrap();
        assert_eq!(out, "pub use beta;\npub use zeta;\nuse alpha;\n");
    }

    #[test]
    fn sort_blank_line_separated_group() {
        // Blank-line-separated groups are sorted independently.
        let src = "use zeta;\nuse alpha;\n\nuse gamma;\nuse beta;\n";
        let out = format(src).unwrap();
        assert_eq!(out, "use alpha;\nuse zeta;\n\nuse beta;\nuse gamma;\n");
    }

    #[test]
    fn sort_preserves_non_import_code() {
        let src = "// header\nuse b::Bar;\nuse a::Foo;\n\nfn main() {}\n";
        assert_eq!(format(src).unwrap(), "// header\nuse a::Foo;\nuse b::Bar;\n\nfn main() {\n}\n");
    }

    #[test]
    fn sort_no_imports_no_change() {
        let src = "fn main() { let x = 1; }\n";
        assert_eq!(format(src).unwrap(), "fn main() {\n    let x = 1;\n}\n");
    }

    #[test]
    fn sort_single_import_unchanged() {
        let src = "use std::collections::HashMap;\n";
        assert_eq!(format(src).unwrap(), "use std::collections::HashMap;\n");
    }

    #[test]
    fn sort_imports_with_paths() {
        let src = "use crate::zzz::last;\nuse aero::fmt;\nuse std::collections::HashMap;\n";
        let out = format(src).unwrap();
        assert_eq!(
            out,
            "use aero::fmt;\nuse crate::zzz::last;\nuse std::collections::HashMap;\n"
        );
    }

    #[test]
    fn sort_with_trailing_comment() {
        // Trailing comments stay with their use line.
        let src = "use beta; // beta docs\nuse alpha; // alpha docs\n";
        let out = format(src).unwrap();
        // After sorting: alpha first, then beta. Each comment stays on its line.
        assert_eq!(out, "use alpha; // alpha docs\nuse beta; // beta docs\n");
    }

    #[test]
    fn sort_three_groups() {
        let src = "pub use std::fmt;\n\nuse alpha;\nuse gamma;\n\npub use crate::util;\n";
        let out = format(src).unwrap();
        assert_eq!(out, "pub use std::fmt;\n\nuse alpha;\nuse gamma;\n\npub use crate::util;\n");
    }
}