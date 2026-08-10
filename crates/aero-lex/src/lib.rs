pub mod token;

use logos::Logos;
use token::{LexError, Token, TokenKind};

/// Tokenize source code into a stream of tokens with line/column info.
pub fn lex(source: &str) -> Result<Vec<Token>, LexError> {
    // Strip UTF-8 BOM (common with Windows editors), replacing it with
    // equal-length spaces so byte offsets stay unchanged.
    let cleaned = strip_bom(source);
    // Remove `//` line comments (replace with spaces to preserve offsets).
    let cleaned = strip_comments(&cleaned);
    let starts = line_starts(&cleaned);
    let mut tokens = Vec::new();

    for (result, range) in TokenKind::lexer(&cleaned).spanned() {
        let kind = match result {
            Ok(kind) => kind,
            Err(()) => {
                let (line, col) = locate(&starts, range.start);
                let text = &source[range.start..range.end.min(source.len())];
                return Err(LexError {
                    msg: format!("unrecognized character `{}`", escape(text)),
                    line,
                    col,
                });
            }
        };
        let (line, col) = locate(&starts, range.start);
        tokens.push(Token {
            kind,
            start: range.start,
            end: range.end,
            line,
            col,
        });
    }
    Ok(tokens)
}

/// Byte offset of the start of every line, for fast line/col lookup.
fn line_starts(source: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(
            source
                .char_indices()
                .filter_map(|(i, c)| (c == '\n').then_some(i + 1)),
        )
        .collect()
}

/// Compute 1-based (line, column) from a byte offset.
fn locate(starts: &[usize], pos: usize) -> (u32, u32) {
    let line = starts.partition_point(|&s| s <= pos).saturating_sub(1);
    (line as u32 + 1, (pos - starts[line]) as u32 + 1)
}

fn escape(text: &str) -> String {
    text.chars().flat_map(|c| c.escape_default()).collect()
}

/// Replace every UTF-8 BOM (U+FEFF, 3 bytes) in the source with 3 spaces,
/// preserving byte offsets so line/col positioning stays aligned.
fn strip_bom(source: &str) -> String {
    if !source.contains('\u{feff}') {
        return source.to_string();
    }
    let mut out = String::with_capacity(source.len());
    for c in source.chars() {
        if c == '\u{feff}' {
            out.push_str("   ");
        } else {
            out.push(c);
        }
    }
    out
}

/// Replace `//` line comments with equal-length spaces, preserving offsets.
fn strip_comments(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                out.push(b' ');
                i += 1;
            }
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    // Only ASCII spaces are written, so UTF-8 encoding is unchanged.
    String::from_utf8(out).expect("source remains valid UTF-8 after comment stripping")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokenizes_int_and_operators() {
        let tokens = lex("1 + 2 * 3").unwrap();
        let kinds: Vec<TokenKind> = tokens.iter().map(|t| t.kind.clone()).collect();
        assert_eq!(
            kinds,
            vec![
                TokenKind::Int(1),
                TokenKind::Plus,
                TokenKind::Int(2),
                TokenKind::Star,
                TokenKind::Int(3),
            ]
        );
    }

    #[test]
    fn tokenizes_keywords_and_idents() {
        let tokens = lex("let x = 10;").unwrap();
        let kinds: Vec<TokenKind> = tokens.iter().map(|t| t.kind.clone()).collect();
        assert_eq!(
            kinds,
            vec![
                TokenKind::Let,
                TokenKind::Ident("x".into()),
                TokenKind::Eq,
                TokenKind::Int(10),
                TokenKind::Semi,
            ]
        );
    }

    #[test]
    fn tokenizes_string_with_escapes() {
        let tokens = lex(r#"print("hi\n");"#).unwrap();
        assert_eq!(tokens[2].kind, TokenKind::Str("hi\n".into()));
        let tokens = lex(r#""a\"b\\c""#).unwrap();
        assert_eq!(tokens[0].kind, TokenKind::Str("a\"b\\c".into()));
    }

    #[test]
    fn skips_whitespace_and_comments() {
        let tokens = lex("let a = 1; // comment\nprint(a);").unwrap();
        // 10 tokens: let a = 1 ; print ( a ) ; — comments/whitespace skipped
        assert_eq!(tokens.len(), 10);
    }

    #[test]
    fn reports_line_and_col() {
        let tokens = lex("let x = 1;\nprint(x);").unwrap();
        let print_tok = &tokens[5];
        assert_eq!(print_tok.line, 2);
        assert_eq!(print_tok.col, 1);
    }

    #[test]
    fn rejects_invalid_char() {
        let err = lex("let @ = 1;").unwrap_err();
        assert!(err.msg.contains('@'));
        assert_eq!(err.line, 1);
        assert_eq!(err.col, 5);
    }
}