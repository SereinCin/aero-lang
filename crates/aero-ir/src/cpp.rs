//! C++ bindgen (`aero build --cpp`): collect `#[export]` function signatures
//! from an Aero source file and emit a C++ header (`.hpp`) declaring them as
//! `extern "C"`, so a C++ project can `#include` the header, link against the
//! Aero-built shared library (`.dll`/`.so`/`.dylib`, see [`crate::aot`]) and
//! call the Aero functions directly.
//!
//! The exported Aero functions are plain C-ABI symbols (M0, `#[export]`), so no
//! glue layer is needed — the header is the only artifact beyond the library.
//!
//! ## Type mapping (v1)
//!
//! | Aero | C++ |
//! | --- | --- |
//! | `i64` | `int64_t` |
//! | `i32` | `int32_t` |
//! | `f64` | `double` |
//! | `bool` | `bool` |
//! | `str` | `const char*` |
//! | `()` / none | `void` |
//!
//! `String` and raw pointers (`*T`) are not mapped in v1; an exported function
//! using them is rejected with a clear error (extend this module to support
//! them, mirroring the CPython `String` ↔ bytes conversion in codegen).

use crate::AeroError;
use aero_hir::ty::Ty;

/// A single exported Aero function, mapped for C++ consumption.
#[derive(Debug, Clone)]
pub struct CppExport {
    /// C++-visible name (the Aero name, possibly escaped if it collides with a
    /// C++ keyword).
    pub cpp_name: String,
    /// The actual C symbol name in the dynamic library (what the linker sees).
    pub symbol: String,
    /// Parameters: (name, C++ type). Empty for no-arg functions.
    pub params: Vec<(String, String)>,
    /// Return type (C++): `Some("int64_t")` etc. `None` = `void`.
    pub ret: Option<String>,
}

/// C++ keywords that cannot be used as function identifiers. Only the subset
/// plausible as Aero function names is listed (avoiding false positives for
/// words like `true`/`this` that Aero could legitimately use).
const CPP_KEYWORDS: &[&str] = &[
    "alignas", "alignof", "and", "asm", "auto", "bool", "break", "case", "catch", "char",
    "char16_t", "char32_t", "class", "const", "constexpr", "continue", "delete", "do", "double",
    "dynamic_cast", "else", "enum", "explicit", "extern", "false", "float", "for", "friend",
    "goto", "if", "inline", "int", "long", "mutable", "namespace", "new", "noexcept",
    "nullptr", "operator", "private", "protected", "public", "register", "return", "short",
    "signed", "sizeof", "static", "static_cast", "struct", "switch", "template", "this",
    "throw", "true", "try", "typedef", "typename", "union", "unsigned", "using", "virtual",
    "void", "volatile", "wchar_t", "while",
];

/// Map an Aero type to its C++ header type. `None` means unsupported in v1.
fn cpp_type(ty: &Ty) -> Option<&'static str> {
    match ty {
        Ty::I32 => Some("int32_t"),
        Ty::I64 => Some("int64_t"),
        Ty::F64 => Some("double"),
        Ty::Bool => Some("bool"),
        Ty::Str => Some("const char*"),
        _ => None,
    }
}

/// Collect the `#[export]` function signatures of an Aero source file, mapped
/// to C++. Reuses the front half of the pipeline (lex → parse → lower/type
/// check) without running codegen. `#[py_export]` functions are included too
/// (they are `#[export]` first; their raw C symbol is callable from C++).
pub fn collect_cpp_exports(source: &str) -> Result<Vec<CppExport>, AeroError> {
    let mut tokens = aero_std::std_tokens().to_vec();
    let user_tokens = aero_lex::lex(source).map_err(|e| AeroError {
        phase: "lexing",
        line: e.line,
        col: e.col,
        msg: e.msg,
    })?;
    tokens.extend(user_tokens);
    let program = aero_parse::parse(&tokens).map_err(|e| AeroError {
        phase: "parsing",
        line: e.line,
        col: e.col,
        msg: e.msg,
    })?;
    let (hir, _result) = aero_hir::lower_and_check(&program).map_err(|e| AeroError {
        phase: e.phase(),
        line: e.line(),
        col: e.col(),
        msg: e.msg().to_string(),
    })?;

    let mut out = Vec::new();
    for f in &hir.funcs {
        if !f.exported || f.builtin || f.is_extern || !f.type_params.is_empty() {
            continue;
        }
        let symbol = f.name.rsplit("::").next().unwrap_or(&f.name).to_string();
        let mut params: Vec<(String, String)> = Vec::new();
        for (pname, pty, _) in &f.params {
            match cpp_type(pty) {
                Some(ct) => params.push((pname.clone(), ct.to_string())),
                None => {
                    return Err(AeroError {
                        phase: "cpp",
                        line: 0,
                        col: 0,
                        msg: format!(
                            "`#[export]` parameter `{pname}` type `{pty}` is not supported by C++ bindgen v1 (supported: i32/i64/f64/bool/str)"
                        ),
                    });
                }
            }
        }
        let ret = match &f.ret {
            Some(rt) => match cpp_type(rt) {
                Some(ct) => Some(ct.to_string()),
                None => {
                    return Err(AeroError {
                        phase: "cpp",
                        line: 0,
                        col: 0,
                        msg: format!(
                            "`#[export]` return type `{rt}` is not supported by C++ bindgen v1 (supported: i32/i64/f64/bool/str/void)"
                        ),
                    });
                }
            },
            None => None,
        };
        let cpp_name = if CPP_KEYWORDS.contains(&symbol.as_str()) {
            // `double` (C++ keyword) cannot be a C++ identifier; expose it as
            // `double_` and pin the real symbol with an `asm` label (GCC/Clang).
            format!("{symbol}_")
        } else {
            symbol.clone()
        };
        out.push(CppExport {
            cpp_name,
            symbol,
            params,
            ret,
        });
    }
    Ok(out)
}

/// Generate the C++ header text for a set of exports. `module` becomes the
/// include guard (`AERO_<MODULE>_HPP`) and the banner.
pub fn cpp_header(exports: &[CppExport], module: &str) -> String {
    let guard = format!("AERO_{}_HPP", module.to_uppercase());
    let mut s = String::new();
    s.push_str("// Generated by `aero build --cpp`. Do not edit.\n");
    s.push_str("// Aero -> C++ bindings. Link against the Aero-built shared library.\n\n");
    s.push_str(&format!("#ifndef {guard}\n#define {guard}\n\n"));
    s.push_str("#include <cstdint>\n\n");
    s.push_str("extern \"C\" {\n");
    for e in exports {
        let params: Vec<String> = e
            .params
            .iter()
            .map(|(n, t)| format!("{t} {n}"))
            .collect();
        let ret = e.ret.as_deref().unwrap_or("void");
        let params_str = if params.is_empty() {
            "void".to_string()
        } else {
            params.join(", ")
        };
        if e.cpp_name == e.symbol {
            s.push_str(&format!("{ret} {}({});\n", e.cpp_name, params_str));
        } else {
            s.push_str(&format!(
                "// symbol `{sym}` is a C++ keyword; callable as `{cpp}`\n{ret} {cpp}({params}) asm(\"{sym}\");\n",
                sym = e.symbol,
                cpp = e.cpp_name,
                params = params_str,
            ));
        }
    }
    s.push_str("} // extern \"C\"\n\n");
    s.push_str(&format!("#endif // {guard}\n"));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_and_maps_export_signatures() {
        let src = r#"
#[export]
fn add(a: i64, b: i64) -> i64 { return a + b; }
#[export]
fn double(x: f64) -> f64 { return x * 2.0; }
#[export]
fn is_even(n: i64) -> bool { return n % 2 == 0; }
#[export]
fn greet(name: str) -> str { return name; }
#[export]
fn noop() {}
fn internal(x: i64) -> i64 { return x; }
"#;
        let exports = collect_cpp_exports(src).expect("collect");
        // `internal` is not #[export] -> excluded.
        let names: Vec<&str> = exports.iter().map(|e| e.cpp_name.as_str()).collect();
        assert_eq!(names, vec!["add", "double_", "is_even", "greet", "noop"]);
        let add = &exports[0];
        assert_eq!(add.params, vec![("a".to_string(), "int64_t".to_string()), ("b".to_string(), "int64_t".to_string())]);
        assert_eq!(add.ret.as_deref(), Some("int64_t"));
        let greet = &exports[3];
        assert_eq!(greet.params, vec![("name".to_string(), "const char*".to_string())]);
        assert_eq!(greet.ret.as_deref(), Some("const char*"));
        let noop = &exports[4];
        assert_eq!(noop.params, Vec::<(String, String)>::new());
        assert_eq!(noop.ret, None);
    }

    #[test]
    fn keyword_symbol_gets_asm_label() {
        let src = "#[export]\nfn double(x: f64) -> f64 { return x * 2.0; }\n";
        let exports = collect_cpp_exports(src).expect("collect");
        assert_eq!(exports.len(), 1);
        assert_eq!(exports[0].cpp_name, "double_");
        assert_eq!(exports[0].symbol, "double");
        let header = cpp_header(&exports, "demo");
        assert!(header.contains("double double_(double x) asm(\"double\");"), "{header}");
    }

    #[test]
    fn header_has_guard_and_cstdint() {
        let exports = vec![CppExport {
            cpp_name: "add".to_string(),
            symbol: "add".to_string(),
            params: vec![("a".to_string(), "int64_t".to_string())],
            ret: Some("int64_t".to_string()),
        }];
        let header = cpp_header(&exports, "mymod");
        assert!(header.contains("#ifndef AERO_MYMOD_HPP"));
        assert!(header.contains("#define AERO_MYMOD_HPP"));
        assert!(header.contains("#endif // AERO_MYMOD_HPP"));
        assert!(header.contains("#include <cstdint>"));
        assert!(header.contains("int64_t add(int64_t a);"));
    }

    #[test]
    fn unsupported_type_is_rejected() {
        let src = "#[export]\nfn takes_string(s: String) -> i64 { return s.len(); }\n";
        let err = collect_cpp_exports(src).unwrap_err();
        assert!(err.msg.contains("not supported by C++ bindgen v1"), "{err:?}");
    }
}
