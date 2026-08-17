//! Aero standard library (prelude).
//!
//! `std.aero` is injected ahead of every user compilation unit by the front-end
//! (`aero_ir::compile_pipeline`). It defines the core error-handling types
//! (`Option`, `Result`) so user code can use them without declaring them.
//!
//! "Precompilation": the token stream of `std.aero` is lexed once per process and
//! cached, so the standard library is never re-lexed across compilations. It is
//! still parsed together with user code (single monolithic parse), which keeps
//! spans of std items and user items correct independently (std tokens carry
//! std-file line numbers, user tokens carry user-file line numbers).

use std::sync::OnceLock;

/// The standard library source text, embedded at build time.
pub const STD_SOURCE: &str = include_str!("std.aero");

/// Number of source lines in `std.aero` (useful for diagnostics/counting).
pub fn std_lines() -> usize {
    STD_SOURCE.lines().count()
}

/// The pre-lexed standard library token stream (cached for the process lifetime).
pub fn std_tokens() -> &'static [aero_lex::token::Token] {
    static TOKENS: OnceLock<Vec<aero_lex::token::Token>> = OnceLock::new();
    TOKENS
        .get_or_init(|| {
            aero_lex::lex(STD_SOURCE)
                .expect("the standard library must always lex without errors")
        })
        .as_slice()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn std_source_lexes_cleanly() {
        let tokens = std_tokens();
        assert!(!tokens.is_empty(), "std.aero must produce tokens");
        // The prelude must not contain any top-level side effects; it should
        // lex to a balanced set of definitions.
        assert!(std_lines() > 0);
    }

    #[test]
    fn std_source_defines_option_and_result() {
        assert!(STD_SOURCE.contains("enum Option<T>"));
        assert!(STD_SOURCE.contains("enum Result<T, E>"));
    }
}
