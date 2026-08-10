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
    #[token("fn")]
    Fn,
    #[token("return")]
    Return,
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

    // Literals and identifiers
    #[regex(r"[0-9]+", |lex| lex.slice().parse::<i64>().ok())]
    Int(i64),
    /// String literal, e.g. `"hello\n"` (escape sequences already decoded)
    #[regex(r#""([^"\\]|\\.)*""#, |lex| {
        let slice = lex.slice();
        Some(unescape_str(&slice[1..slice.len() - 1]))
    })]
    Str(String),
    #[regex(r"[a-zA-Z_][a-zA-Z0-9_]*", |lex| Some(lex.slice().to_string()))]
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
    #[token("&")]
    Amp,
    #[token(".")]
    Dot,
    #[token("=")]
    Eq,
    #[token(":")]
    Colon,
    #[token("->")]
    Arrow,
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
            TokenKind::Int(_) => "integer",
            TokenKind::Str(_) => "string",
            TokenKind::Ident(_) => "identifier",
            TokenKind::Plus => "operator `+`",
            TokenKind::Minus => "operator `-`",
            TokenKind::Star => "operator `*`",
            TokenKind::Slash => "operator `/`",
            TokenKind::Amp => "operator `&`",
            TokenKind::Dot => "dot `.`",
            TokenKind::Eq => "operator `=`",
            TokenKind::Colon => "colon `:`",
            TokenKind::Arrow => "arrow `->`",
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