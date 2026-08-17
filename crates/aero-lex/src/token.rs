use logos::Logos;

/// Aero token types, produced directly by the Logos lexer.
#[derive(Logos, Debug, Clone, PartialEq)]
pub enum TokenKind {
    // Keywords
    #[token("let")]
    Let,
    #[token("print")]
    Print,
    #[token("if")]
    If,
    #[token("else")]
    Else,
    #[token("while")]
    While,
    #[token("loop")]
    Loop,
    #[token("for")]
    For,
    #[token("in")]
    In,
    #[token("fn")]
    Fn,
    #[token("return")]
    Return,
    #[token("break")]
    Break,
    #[token("continue")]
    Continue,
    #[token("true")]
    True,
    #[token("false")]
    False,
    #[token("mut")]
    Mut,
    #[token("arena")]
    Arena,
    #[token("tensor")]
    Tensor,
    #[token("extern")]
    Extern,
    #[token("gpu")]
    Gpu,
    #[token("match")]
    Match,
    #[token("struct")]
    Struct,
    #[token("union")]
    Union,
    #[token("enum")]
    Enum,
    #[token("trait")]
    Trait,
    #[token("impl")]
    Impl,
    #[token("type")]
    Type,
    #[token("dyn")]
    Dyn,
    #[token("as")]
    As,
    #[token("const")]
    Const,
    #[token("mod")]
    Mod,
    #[token("use")]
    Use,
    #[token("pub")]
    Pub,
    #[token("crate")]
    Crate,

    // Literals and identifiers
    /// Float literal, e.g. `3.14`, `1.0`, `2.5e10`
    #[regex(r"[0-9]+\.[0-9]+([eE][+-]?[0-9]+)?", |lex| lex.slice().parse::<f64>().ok())]
    #[regex(r"[0-9]+[eE][+-]?[0-9]+", |lex| lex.slice().parse::<f64>().ok())]
    Float(f64),
    #[regex(r"[0-9]+", |lex| lex.slice().parse::<i64>().ok())]
    Int(i64),
    /// Char literal, e.g. `'a'`, `'\n'`
    #[regex(r"'([^'\\]|\\.)'", |lex| {
        let slice = lex.slice();
        let inner = &slice[1..slice.len() - 1];
        Some(unescape_char(inner))
    })]
    Char(char),
    /// String literal, e.g. `"hello\n"` (escape sequences already decoded)
    #[regex(r#""([^"\\]|\\.)*""#, |lex| {
        let slice = lex.slice();
        Some(unescape_str(&slice[1..slice.len() - 1]))
    })]
    Str(String),
    #[token("_")]
    Underscore,
    /// Lifetime parameter, e.g. `'a` in `fn foo<'a>(x: &'a T) -> &'a T`.
    /// A char literal `'a'` is 3 chars and wins the longest-match; a bare `'a`
    /// (no closing quote) is a lifetime.
    #[regex(r"'[a-zA-Z_][a-zA-Z0-9_]*", |lex| Some(lex.slice().to_string()))]
    Lifetime(String),
    #[regex(r"[a-zA-Z_][a-zA-Z0-9_]*", |lex| Some(lex.slice().to_string()), priority = 1)]
    Ident(String),

    // Operators and punctuation
    #[token("+")]
    Plus,
    #[token("-")]
    Minus,
    #[token("*")]
    Star,
    #[token("/")]
    Slash,
    #[token("%")]
    Percent,
    #[token("&")]
    Amp,
    #[token("^")]
    Caret,
    #[token("|")]
    Pipe,
    #[token("<<")]
    Shl,
    #[token(">>")]
    Shr,
    #[token(".")]
    Dot,
    #[token("=")]
    Eq,
    #[token(":")]
    Colon,
    #[token("::")]
    DoubleColon,
    #[token("->")]
    Arrow,
    #[token("=>")]
    FatArrow,
    // Comparison operators (longest-match first: `>=` before `>`)
    #[token(">=")]
    Ge,
    #[token("<=")]
    Le,
    #[token("==")]
    EqEq,
    #[token("!=")]
    Ne,
    #[token(">")]
    Gt,
    #[token("<")]
    Lt,
    // Logical operators
    #[token("&&")]
    AndAnd,
    #[token("||")]
    OrOr,
    #[token("(")]
    LParen,
    #[token(")")]
    RParen,
    #[token("{")]
    LBrace,
    #[token("}")]
    RBrace,
    #[token("[")]
    LBracket,
    #[token("]")]
    RBracket,
    #[token(";")]
    Semi,
    #[token(",")]
    Comma,
    #[token("?")]
    Question,
    #[token("#")]
    Hash,

    // Whitespace is skipped (comments are pre-removed in lex())
    #[regex(r"[ \t\r\n]+", logos::skip)]
    Whitespace,
}

impl TokenKind {
    /// Human-readable description for error messages.
    pub fn describe(&self) -> &'static str {
        match self {
            TokenKind::Let => "keyword `let`",
            TokenKind::Print => "keyword `print`",
            TokenKind::If => "keyword `if`",
            TokenKind::Else => "keyword `else`",
            TokenKind::While => "keyword `while`",
            TokenKind::Fn => "keyword `fn`",
            TokenKind::Return => "keyword `return`",
            TokenKind::True | TokenKind::False => "boolean literal",
            TokenKind::Mut => "keyword `mut`",
            TokenKind::Arena => "keyword `arena`",
            TokenKind::Tensor => "keyword `tensor`",
            TokenKind::Extern => "keyword `extern`",
            TokenKind::Gpu => "keyword `gpu`",
            TokenKind::For => "keyword `for`",
            TokenKind::Loop => "keyword `loop`",
            TokenKind::In => "keyword `in`",
            TokenKind::Break => "keyword `break`",
            TokenKind::Continue => "keyword `continue`",
            TokenKind::Match => "keyword `match`",
            TokenKind::Struct => "keyword `struct`",
            TokenKind::Union => "keyword `union`",
            TokenKind::Enum => "keyword `enum`",
            TokenKind::Trait => "keyword `trait`",
            TokenKind::Impl => "keyword `impl`",
            TokenKind::Type => "keyword `type`",
            TokenKind::Dyn => "keyword `dyn`",
            TokenKind::As => "keyword `as`",
            TokenKind::Const => "keyword `const`",
            TokenKind::Mod => "keyword `mod`",
            TokenKind::Use => "keyword `use`",
            TokenKind::Pub => "keyword `pub`",
            TokenKind::Crate => "keyword `crate`",
            TokenKind::Float(_) => "float",
            TokenKind::Int(_) => "integer",
            TokenKind::Char(_) => "char",
            TokenKind::Str(_) => "string",
            TokenKind::Lifetime(_) => "lifetime parameter",
            TokenKind::Ident(_) => "identifier",
            TokenKind::Plus => "operator `+`",
            TokenKind::Minus => "operator `-`",
            TokenKind::Star => "operator `*`",
            TokenKind::Slash => "operator `/`",
            TokenKind::Percent => "operator `%`",
            TokenKind::Amp => "operator `&`",
            TokenKind::Caret => "operator `^`",
            TokenKind::Pipe => "operator `|`",
            TokenKind::Shl => "operator `<<`",
            TokenKind::Shr => "operator `>>`",
            TokenKind::Dot => "dot `.`",
            TokenKind::Eq => "operator `=`",
            TokenKind::Colon => "colon `:`",
            TokenKind::DoubleColon => "double colon `::`",
            TokenKind::Arrow => "arrow `->`",
            TokenKind::FatArrow => "fat arrow `=>`",
            TokenKind::Ge => "operator `>=`",
            TokenKind::Le => "operator `<=`",
            TokenKind::EqEq => "operator `==`",
            TokenKind::Ne => "operator `!=`",
            TokenKind::Gt => "operator `>`",
            TokenKind::Lt => "operator `<`",
            TokenKind::AndAnd => "operator `&&`",
            TokenKind::OrOr => "operator `||`",
            TokenKind::LParen => "left paren `(`",
            TokenKind::RParen => "right paren `)`",
            TokenKind::LBrace => "left brace `{`",
            TokenKind::RBrace => "right brace `}`",
            TokenKind::LBracket => "left bracket `[`",
            TokenKind::RBracket => "right bracket `]`",
            TokenKind::Semi => "semicolon `;`",
            TokenKind::Comma => "comma `,`",
            TokenKind::Question => "question mark `?`",
            TokenKind::Hash => "hash `#`",
            TokenKind::Underscore => "underscore `_`",
            TokenKind::Whitespace => "whitespace",
        }
    }
}

/// Token with source position information.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    /// Byte range [start, end)
    pub start: usize,
    pub end: usize,
    /// 1-based line number
    pub line: u32,
    /// 1-based column number
    pub col: u32,
}

/// Lexing error.
#[derive(Debug, Clone, PartialEq)]
pub struct LexError {
    pub msg: String,
    pub line: u32,
    pub col: u32,
}

/// Decode a single escape sequence in a char literal.
fn unescape_char(s: &str) -> char {
    // Only treat as an escape sequence when it actually starts with a backslash.
    // Multi-byte UTF-8 chars (len > 1 bytes) are literal code points, not escapes.
    if !s.starts_with('\\') {
        return s.chars().next().unwrap();
    }
    let mut chars = s.chars();
    chars.next(); // skip backslash
    match chars.next() {
        Some('n') => '\n',
        Some('t') => '\t',
        Some('r') => '\r',
        Some('\\') => '\\',
        Some('\'') => '\'',
        Some('"') => '"',
        Some('0') => '\0',
        Some(other) => other,
        None => '\\',
    }
}

/// Decode escape sequences inside string literals: `\n`, `\t`, `\r`, `\\`, `\"`.
/// Unknown escapes are kept as-is (lenient; may be tightened later).
fn unescape_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some('"') => out.push('"'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}