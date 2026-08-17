use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn run_aero(source: &str) -> std::process::Output {
    let path = std::env::temp_dir().join(format!(
        "aero_test_{}_{}.aero",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::write(&path, source).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_aero"))
        .args(["run", path.to_str().unwrap()])
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&path);
    out
}

fn stdout(out: &std::process::Output) -> String {
    // Windows uses \r\n; normalize to \n before comparing
    String::from_utf8_lossy(&out.stdout)
        .into_owned()
        .replace("\r\n", "\n")
}

fn stderr(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn bench_runs_and_reports_table() {
    // A tiny computational benchmark; runs once (samples=1) with a small
    // iteration count so the test is fast, and verifies the report table.
    let src = "fn bench_add() { let n2 = 0; let i = 0; while (i < 100) { n2 = n2 + i; i = i + 1; } }\n";
    let path = std::env::temp_dir().join(format!(
        "aero_bench_cli_{}_{}.aero",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::write(&path, src).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_aero"))
        .args(["bench", path.to_str().unwrap(), "--iterations", "1000", "--samples", "1"])
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&path);
    assert!(out.status.success(), "bench stderr: {}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("benchmark"), "expected header:\n{text}");
    assert!(text.contains("bench_add"), "expected bench row:\n{text}");
    assert!(text.contains("op/s"), "expected throughput column:\n{text}");
}

#[test]
fn prints_sum() {
    let out = run_aero("print(1 + 2);");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "3\n");
}

#[test]
fn prints_string_literal() {
    // FFI bonus: strings print via printf (with \n escape decoding)
    let out = run_aero("print(\"hello from C\\n\");");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "hello from C\n");
}

// ---------- 12.2 aero fmt formatter ----------

#[test]
fn fmt_formats_and_output_still_runs() {
    // Messy source: tight operators, no indentation, one-liners. `aero fmt`
    // should reflow it and the result must still compile + run identically.
    let messy = "struct Point{x:i64,y:i64}\n// compute norm\nfn norm(p:Point)->i64{if(p.x>0){return p.x*p.x+p.y*p.y;}else{return 0;}}\nlet p=Point{x:3,y:4};print(\"%lld\\n\",norm(p));";
    let path = std::env::temp_dir().join(format!(
        "aero_fmt_test_{}_{}.aero",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::write(&path, messy).unwrap();

    let fmt_out = Command::new(env!("CARGO_BIN_EXE_aero"))
        .args(["fmt", path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(fmt_out.status.success(), "fmt stderr: {}", stderr(&fmt_out));

    let out = Command::new(env!("CARGO_BIN_EXE_aero"))
        .args(["run", path.to_str().unwrap()])
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&path);
    assert!(out.status.success(), "run stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "25\n");
}

#[test]
fn fmt_check_reports_unformatted() {
    let messy = "let x=1;";
    let path = std::env::temp_dir().join(format!(
        "aero_fmt_check_{}_{}.aero",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::write(&path, messy).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_aero"))
        .args(["fmt", "--check", path.to_str().unwrap()])
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&path);
    // Unformatted input should fail --check (exit code 1).
    assert!(!out.status.success());
}

#[test]
fn fmt_preserves_block_trailing_and_blank_lines_and_aligns() {
    // Exercises the enhanced formatter: block comments, trailing comments,
    // blank-line preservation between functions, and struct field alignment.
    // The formatted output must still lex + parse + run identically.
    let messy = "struct Config{a:i64,name:str,long_name:f64}\n/* area */\nfn area(c:Config)->i64{return c.a*c.a;}\n\nlet cfg=Config{a:3,name:\"x\",long_name:1.5}; // ok\nprint(\"%lld\\n\",area(cfg));";
    let path = std::env::temp_dir().join(format!(
        "aero_fmt_enh_{}_{}.aero",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::write(&path, messy).unwrap();

    let fmt_out = Command::new(env!("CARGO_BIN_EXE_aero"))
        .args(["fmt", path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(fmt_out.status.success(), "fmt stderr: {}", stderr(&fmt_out));

    // Formatted file carries the features we claim.
    let formatted = std::fs::read_to_string(&path).unwrap();
    assert!(formatted.contains("/* area */"), "formatted:\n{formatted}");
    // Blank line preserved between the fn and the let statement.
    assert!(formatted.contains("}\n\nlet cfg"), "formatted:\n{formatted}");
    // Trailing comment stays on the same line as its statement.
    assert!(formatted.contains("; // ok"), "formatted:\n{formatted}");
    // Alignment: a/name colons padded under `long_name`'s colon (col 13).
    assert!(formatted.contains("    a        : i64,"), "formatted:\n{formatted}");
    assert!(formatted.contains("    long_name: f64"), "formatted:\n{formatted}");

    // The reformatted program still compiles and runs to the same result.
    let out = Command::new(env!("CARGO_BIN_EXE_aero"))
        .args(["run", path.to_str().unwrap()])
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&path);
    assert!(out.status.success(), "run stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "9\n");
}

// ---------- aero clippy static linter ----------

#[test]
fn clippy_flags_issues_on_clean_returns_zero() {
    // A file full of deliberate issues must be flagged (non-zero exit), while a
    // clean file passes with no findings.
    let bad = "fn BadName(x: i64) -> i64 {\n    let unusedVar = 5;\n    let z = 10 / 0;\n    return x;\n}\nfn main() { print(\"%lld\\n\", BadName(1)); }";
    let path = std::env::temp_dir().join(format!(
        "aero_clippy_bad_{}_{}.aero",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::write(&path, bad).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_aero"))
        .args(["clippy", path.to_str().unwrap()])
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&path);
    let err = stderr(&out);
    assert!(
        err.contains("fn_non_snake_case"),
        "expected fn_non_snake_case in:\n{err}"
    );
    assert!(
        err.contains("division_by_zero"),
        "expected division_by_zero in:\n{err}"
    );
    // division_by_zero is severity Error => non-zero exit code.
    assert!(!out.status.success());

    // A genuinely clean source (descriptive names, trailing newline, no risky
    // patterns) should exit 0 and report no issues.
    let clean = "fn main() { let value = 1; print(\"%lld\\n\", value); }\n";
    let path2 = std::env::temp_dir().join(format!(
        "aero_clippy_ok_{}_{}.aero",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::write(&path2, clean).unwrap();
    let out2 = Command::new(env!("CARGO_BIN_EXE_aero"))
        .args(["clippy", path2.to_str().unwrap()])
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&path2);
    assert!(out2.status.success());
    assert!(stdout(&out2).contains("no issues found"));
}

// ---------- M1.1 string system ----------

#[test]
fn string_concat_and_len() {
    let src = r#"
let s1 = "hello";
let s2 = "world";
print("%s\n", s1 + s2);
print("%s\n", "foo" + "bar");
print("%lld\n", len(s1));
print("%lld\n", len("abc"));
print("%lld\n", s1[1]);
if (s1 == "hello") { print("eq!\n"); }
if (s1 != s2) { print("ne!\n"); }
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "helloworld\nfoobar\n5\n3\n101\neq!\nne!\n");
}

#[test]
fn string_int_to_str_and_free() {
    let src = r#"
let n = int_to_str(42);
print("%s\n", n);
let combined = "n=" + int_to_str(123);
print("%s\n", combined);
str_free(combined);
str_free(n);
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "42\nn=123\n");
}

#[test]
fn string_library_extensions() {
    // substr / str_to_int / str_contains / str_find / str_cmp + ordered comparison
    let src = r#"
let s = "hello world";
print("%s\n", substr(s, 0, 5));
print("%s\n", substr(s, 6, 11));
print("%lld\n", str_to_int("123"));
print("%lld\n", str_to_int("-42"));
print("%lld\n", str_to_int("abc"));
print("%lld\n", str_contains(s, "world"));
print("%lld\n", str_contains(s, "xyz"));
print("%lld\n", str_find(s, "world"));
print("%lld\n", str_find(s, "xyz"));
if ("apple" < "banana") { print("lt!\n"); }
if ("b" > "a") { print("gt!\n"); }
if ("a" <= "a") { print("le!\n"); }
print("%lld\n", str_cmp("a", "b"));
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "hello\nworld\n123\n-42\n0\n1\n0\n6\n-1\nlt!\ngt!\nle!\n-1\n"
    );
}


#[test]
fn string_variable_as_format_arg() {
    // Regression: a string value (not a literal) as a %s format argument used to crash
    // codegen via the coerce error path (LLVM 22.1.8 print_type_to_string crash bug)
    let out = run_aero("let s = \"abc\"; print(\"%s\\n\", s);");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "abc\n");
}

// ---------- Native String (phase 2b) ----------

#[test]
fn string_build_modify_and_print() {
    // String::new + push(char) + len/is_empty + %s print
    let src = r#"
let s = String::new();
s.push('h');
s.push('i');
print("len=%lld\n", s.len());
print("is_empty=%lld\n", s.is_empty());
print("s=%s\n", s);
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "len=2\nis_empty=0\ns=hi\n");
}

#[test]
fn string_from_push_str_index_and_pop() {
    let src = r#"
let t = String::from("aero");
t.push_str("lang");
print("t=%s\n", t);
print("tlen=%lld\n", t.len());
print("byte=%lld\n", t[0]);
print("byte2=%lld\n", t.at(2));
t[0] = 'A';
print("t2=%s\n", t);
let last = t.pop();
print("last=%lld\n", last);
print("after_pop=%s\n", t);
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "t=aerolang\ntlen=8\nbyte=97\nbyte2=114\nt2=Aerolang\nlast=103\nafter_pop=Aerolan\n");
}

#[test]
fn string_clear_and_with_cap_growth() {
    let src = r#"
let t = String::from("x");
t.clear();
print("cleared_empty=%lld\n", t.is_empty());
print("cleared_len=%lld\n", t.len());
let c = String::with_cap(2);
c.push('x');
c.push('y');
c.push('z');
print("c=%s\n", c);
print("clen=%lld\n", c.len());
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "cleared_empty=1\ncleared_len=0\nc=xyz\nclen=3\n");
}

#[test]
fn string_as_value_and_data() {
    // String passed/returned by value, moved to a second binding, and data() -> str
    let src = r#"
fn greet(name: String) -> String {
    let out = String::from("hello, ");
    out.push_str(name.data());
    return out;
}
let g = greet(String::from("world"));
print("g=%s\n", g);
let b = g;
print("b=%s\n", b);
print("blen=%lld\n", b.len());
let d = String::from("payload");
print("data_len=%lld\n", len(d.data()));
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "g=hello, world\nb=hello, world\nblen=12\ndata_len=7\n");
}

#[test]
fn string_utf8_char_ops() {
    // String 2.0 (stdlib Phase 1): Unicode code-point ops over a NUL-terminated buffer.
    // utf8_len counts characters (not bytes); utf8_at indexes by character; utf8_push
    // appends a code point; utf8_pop removes the last code point.
    let src = r#"
let s = String::from("aero");
s.utf8_push('你');
s.utf8_push('好');
print("chars=%lld\n", s.len());
print("utf8=%lld\n", utf8_len(s.data()));
print("c0=%lld\n", utf8_at(s.data(), 0));
print("c4=%lld\n", utf8_at(s.data(), 4));
print("c9=%lld\n", utf8_at(s.data(), 9));
let pop = s.utf8_pop();
print("pop=%lld\n", pop);
print("after=%s\n", s);
print("chars2=%lld\n", utf8_len(s.data()));
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    // "aero" = 4 ASCII bytes; 你 = U+4F60 (0x4F60, 3 bytes), 好 = U+597D (0x597D, 3 bytes).
    // s.len() is byte length (10); utf8_len counts characters (6).
    assert_eq!(stdout(&out), "chars=10\nutf8=6\nc0=97\nc4=20320\nc9=-1\npop=22909\nafter=aero你\nchars2=5\n");
}

#[test]
fn string_utf8_push_pop_roundtrip() {
    // utf8_push appends a code point (multi-byte) and utf8_pop removes it; verify
    // the byte length and remaining characters.
    let src = r#"
let s = String::new();
s.utf8_push('🎉');
s.utf8_push('A');
print("s=%s\n", s);
print("chars=%lld\n", utf8_len(s.data()));
s.utf8_pop();
print("after=%s\n", s);
print("chars2=%lld\n", utf8_len(s.data()));
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    // 🎉 = U+1F389 (4 bytes), then 'A' (1 byte). Pop removes 'A'.
    assert_eq!(stdout(&out), "s=🎉A\nchars=2\nafter=🎉\nchars2=1\n");
}

#[test]
fn string_starts_ends_with() {
    // String 2.0 (stdlib Phase 1): starts_with / ends_with use memcmp over the
    // NUL-terminated buffer. C-style booleans (1 / 0) are printed via print.
    let src = r#"
let s = String::from("aero-lang");
print("sw0=%lld\n", s.starts_with("aero"));
print("sw1=%lld\n", s.starts_with("lang"));
print("ew0=%lld\n", s.ends_with("lang"));
print("ew1=%lld\n", s.ends_with("aero"));
print("ew2=%lld\n", s.ends_with("xxx"));
print("ew3=%lld\n", s.ends_with("aero-lang"));
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "sw0=1\nsw1=0\new0=1\new1=0\new2=0\new3=1\n");
}

#[test]
fn string_wrong_method_rejected() {
    let out = run_aero("let s = String::new(); s.nope();");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("no method"));
}

#[test]
fn path_operations() {
    // stdlib Phase 1 (paths): path_join / path_basename / path_dirname /
    // path_extension operate on NUL-terminated strings and return owned Strings.
    let src = r#"
let j = path_join("a/b", "c");
print("j=%s\n", j);
let j2 = path_join("a/", "/b");
print("j2=%s\n", j2);
let j3 = path_join("", "x");
print("j3=%s\n", j3);
print("base=%s\n", path_basename("/usr/local/bin/aero.txt"));
print("base2=%s\n", path_basename("plain"));
print("dir=%s\n", path_dirname("/usr/local/bin/aero.txt"));
print("dir2=%s\n", path_dirname("aero.txt"));
print("ext=%s\n", path_extension("/srv/app.tar.gz"));
print("ext2=%s\n", path_extension("noext"));
print("ext3=%s\n", path_extension("dir.hidden/file"));
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "j=a/b/c\nj2=a/b\nj3=x\nbase=aero.txt\nbase2=plain\ndir=/usr/local/bin\ndir2=\next=.gz\next2=\next3=\n",
    );
}


#[test]
fn json_builtin() {
    // stdlib Phase 1 (serialization): json_string / json_escape / json_number_* /
    // json_bool / json_null / json_join for encoding, and json_find_key /
    // json_parse_i64 / json_parse_bool / json_unescape for parsing a JSON object.
    let src = r#"
print("%s\n", json_string("he\"llo\n"));
print("%s\n", json_number_i64(-42));
print("%s\n", json_number_f64(3.14));
print("%s\n", json_bool(true));
print("%s\n", json_null());
let obj = json_join(json_join(json_string("name"), ":", json_string("Aero")), ",", json_join(json_string("ver"), ":", json_number_i64(1)));
print("%s\n", obj);
print("%s\n", json_escape("a\"b\\c\nd"));
let doc = "{\"name\":\"Aero\",\"year\":2026,\"ok\":true}";
let at = json_find_key(doc, "year");
print("%lld\n", json_parse_i64(substr(doc, at, 999)));
let at2 = json_find_key(doc, "name");
print("%s\n", json_unescape(substr(doc, at2, 999)));
let at3 = json_find_key(doc, "ok");
print("%lld\n", json_parse_bool(substr(doc, at3, 999)));
print("%lld\n", json_find_key(doc, "missing"));
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "\"he\\\"llo\\n\"\n-42\n3.140000\ntrue\nnull\n\"name\":\"Aero\",\"ver\":1\na\\\"b\\\\c\\nd\n2026\nAero\n1\n-1\n",
    );
}


#[test]
fn prints_formatted_args() {
    // Multi-argument print: format string + i64 integers (%d)
    let out = run_aero("print(\"x = %d\\n\", 42);");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "x = 42\n");
}

#[test]
fn prints_multiple_formatted_args() {
    let out = run_aero("print(\"%d + %d = %d\\n\", 1, 2, 3);");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "1 + 2 = 3\n");
}

#[test]
fn prints_string_arg_with_percent_s() {
    let out = run_aero("print(\"hi %s\\n\", \"aero\");");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "hi aero\n");
}

#[test]
fn if_taken_branch() {
    let out = run_aero("if (1 < 2) { print(1); } else { print(2); }");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "1\n");
}

#[test]
fn else_branch() {
    let out = run_aero("if (3 < 2) { print(1); } else { print(2); }");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "2\n");
}

#[test]
fn if_without_else() {
    let out = run_aero("if (false) { print(1); }\nprint(2);");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "2\n");
}

#[test]
fn while_loop() {
    // Assignment advances the loop (0 1 2)
    let out = run_aero("let i = 0;\nwhile (i < 3) { print(i); i = i + 1; }");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "0\n1\n2\n");
}

#[test]
fn logic_short_circuit() {
    let out = run_aero(
        "if (false && true) { print(1); } else { print(0); }\nif (true || false) { print(1); } else { print(0); }",
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "0\n1\n");
}

#[test]
fn comparison_ops() {
    let out = run_aero(
        "print(1 < 2);\nprint(2 <= 2);\nprint(2 > 1);\nprint(2 >= 2);\nprint(1 == 1);\nprint(1 != 2);",
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "1\n1\n1\n1\n1\n1\n");
}

#[test]
fn assign_undefined_variable_reported() {
    let out = run_aero("x = 1;");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("undefined variable"));
}

#[test]
fn full_program() {
    let out = run_aero(
        "let x = 1;\n\
         let y = 2;\n\
         print(x + y);\n\
         print(1 + 1);\n\
         print((2 + 3) * 4);\n\
         print(-5 + 10);\n\
         print(8 / 2 - 1);",
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "3\n2\n20\n5\n3\n");
}

#[test]
fn multiplication_and_division() {
    let out = run_aero("print(7 * 6);\nprint(20 / 4);");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "42\n5\n");
}

#[test]
fn syntax_error_reported() {
    let out = run_aero("let x = 1");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("parsing"));
    assert!(stderr(&out).contains(';'));
}

#[test]
fn lex_error_reported() {
    let out = run_aero("print(@);");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("lex"));
}

#[test]
fn undefined_variable_reported() {
    let out = run_aero("print(x);");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("undefined variable"));
}

#[test]
fn function_call_and_return() {
    let out = run_aero("fn add(a: i64, b: i64) -> i64 { return a + b; }\nprint(add(1, 2));");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "3\n");
}

#[test]
fn recursion_factorial() {
    let out = run_aero(
        "fn fact(n: i64) -> i64 { if (n <= 1) { return 1; } return n * fact(n - 1); }\nprint(fact(5));",
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "120\n");
}

#[test]
fn void_function_statement() {
    let out = run_aero("fn noop(x: i64) {}\nnoop(1);\nprint(7);");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "7\n");
}

#[test]
fn forward_call_before_definition() {
    let out = run_aero("print(inc(41));\nfn inc(x: i64) -> i64 { return x + 1; }");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "42\n");
}

#[test]
fn array_literal_and_index() {
    let out = run_aero("let arr = [10, 20, 30];\nprint(arr[1]);\nprint(arr[0] + arr[2]);");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "20\n40\n");
}

#[test]
fn tuple_literal_and_index() {
    let out = run_aero("let t = (10, true);\nprint(t[0]);\nprint(t[1]);");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "10\n1\n");
}

#[test]
fn type_annotation_narrowing_rejected() {
    // x infers to i64; assigning to an i32 annotation is implicit narrowing and must error
    let out = run_aero("let x = 1 + 2;\nlet y: i32 = x;");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("type mismatch") || stderr(&out).contains("type"));
}

#[test]
fn bool_in_arithmetic_rejected() {
    let out = run_aero("let x = 1 + true;");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("type"));
}

#[test]
fn unknown_type_rejected() {
    let out = run_aero("let x: float = 1;");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("unknown type"));
}

#[test]
fn heterogeneous_array_rejected() {
    let out = run_aero("let a = [1, true];");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("type"));
}

// ---------- Campaign 2: ownership / borrows / Arena ----------

#[test]
fn ref_deref_read() {
    // Immutable reference: take &x, read via *r
    let out = run_aero("let x = 5; let r = &x; print(*r);");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "5\n");
}

#[test]
fn mut_ref_write_back() {
    // Mutable reference: *r = v writes back to the source variable
    let out = run_aero("let x = 5; let r = &mut x; *r = 10; print(x);");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "10\n");
}

#[test]
fn two_immut_borrows_coexist() {
    // Multiple immutable borrows may coexist
    let out = run_aero("let x = 5; let r1 = &x; let r2 = &x; print(*r1 + *r2);");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "10\n");
}

#[test]
fn nll_borrow_ends_after_last_use() {
    // NLL: after r last use the borrow ends, so the source can be written
    let out = run_aero("let x = 5; let r = &mut x; print(*r); x = 6; print(x);");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "5\n6\n");
}

#[test]
fn borrow_write_conflict_rejected() {
    // Writing the source while a borrow is live -> borrow check error
    let out = run_aero("let x = 5; let r = &x; x = 6;");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("borrow"));
}

#[test]
fn double_mut_borrow_rejected() {
    // Double mutable borrow -> exclusivity check error
    let out = run_aero("let x = 5; let r = &mut x; let s = &mut x;");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("borrow"));
}

#[test]
fn mut_borrow_after_immut_rejected() {
    // Requesting a mutable borrow while an immutable one is live -> mutual exclusion error
    let out = run_aero("let x = 5; let r = &x; let s = &mut x;");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("borrow"));
}

#[test]
fn borrow_used_after_write_rejected() {
    // Using the reference after writing the source -> borrow check error (r borrow not ended)
    let out = run_aero("let x = 5; let r = &mut x; print(*r); x = 6; print(*r);");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("borrow"));
}

#[test]
fn arena_alloc_and_index_write() {
    // Arena: alloc two slots, index-write, then read back and sum
    let out = run_aero(
        "let a = arena(64); let p = a.alloc(2); p[0] = 7; p[1] = 9; print(p[0] + p[1]);",
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "16\n");
}

#[test]
fn arena_reset_reuses_memory() {
    // After reset the offset is zeroed; allocation works again
    let out = run_aero(
        "let a = arena(32); let p = a.alloc(1); p[0] = 5; a.reset(); let q = a.alloc(1); q[0] = 8; print(q[0]);",
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "8\n");
}

#[test]
fn arena_in_block_auto_reset() {
    // Auto-reset at block end: arena lifetime is limited to its scope
    let out = run_aero("if (true) { let a = arena(32); let p = a.alloc(1); p[0] = 1; print(p[0]); } print(9);");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "1\n9\n");
}

#[test]
fn arena_assign_rejected() {
    // Arenas cannot be copied/moved/reassigned
    let out = run_aero("let a = arena(32); let b = a;");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("Arena"));
}

#[test]
fn array_index_write() {
    // Array index write (Campaign 2 lvalue extension)
    let out = run_aero("let a = [1, 2, 3]; a[1] = 9; print(a[1]);");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "9\n");
}

// ---------- Campaign 4: move semantics ----------

#[test]
fn copy_scalars_reusable() {
    // Scalars are Copy: both bindings may be used after a let
    let out = run_aero("let a = 5; let b = a; print(a); print(b);");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "5\n5\n");
}

#[test]
fn copy_struct_reusable() {
    // A struct is Copy only when explicitly `impl Copy for X {}`; then both
    // bindings may be used after a let (bitwise copy, no move).
    let src = r#"
struct Pt { x: i64, y: i64 }
impl Copy for Pt { }
let p = Pt { x: 1, y: 2 };
let q = p;
print("p.x=%lld q.y=%lld\n", p.x, q.y);
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "p.x=1 q.y=2\n");
}

#[test]
fn struct_without_copy_is_moved_rejected() {
    // Without `impl Copy`, a struct value is moved on `let`; the old binding is dead.
    let src = r#"
struct Pt { x: i64, y: i64 }
let p = Pt { x: 1, y: 2 };
let q = p;
print("p.x=%lld q.y=%lld\n", p.x, q.y);
"#;
    let out = run_aero(src);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("moved"));
}

#[test]
fn derive_copy_struct_reusable() {
    // `#[derive(Copy)]` is sugar for an empty `impl Copy for X {}`.
    let src = r#"
#[derive(Copy)]
struct Pt { x: i64, y: i64 }
let p = Pt { x: 1, y: 2 };
let q = p;
print("p.x=%lld q.y=%lld\n", p.x, q.y);
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "p.x=1 q.y=2\n");
}

#[test]
fn derive_copy_enum_reusable() {
    // Enums can also derive Copy (unit + payload variants of Copy fields).
    // Without Copy, the second `let e = c` would fail: `c` was already moved.
    let src = r#"
#[derive(Copy)]
enum Color { Red, Green, Blue }
let c = Color::Red;
let d = c;
let e = c;
print("ok\n");
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "ok\n");
}

#[test]
fn impl_copy_on_string_field_rejected() {
    // A struct holding a String cannot be Copy (bitwise copy would double-free).
    let src = r#"
struct S { name: String }
impl Copy for S { }
let s = S { name: String::from("x") };
"#;
    let out = run_aero(src);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("Copy"));
}

#[test]
fn string_move_allows_use_via_new_binding() {
    // Non-Copy String moves: the new binding owns the value, the old one is dead
    let out = run_aero("let a = String::from(\"hi\"); let b = a; print(\"%s\\n\", b);");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "hi\n");
}

#[test]
fn string_use_after_move_rejected() {
    // Reading a moved String is a use-after-move error
    let out = run_aero("let a = String::from(\"hi\"); let b = a; print(\"%s\\n\", a);");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("moved"));
}

#[test]
fn string_method_call_after_move_rejected() {
    // Calling a method on a moved String is a use-after-move error
    let out = run_aero("let a = String::from(\"hi\"); let b = a; a.push('x');");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("moved"));
}

#[test]
fn string_assignment_moves() {
    // Assignment transfers ownership: the source is moved, the target takes over
    let out = run_aero("let a = String::from(\"x\"); let b = String::new(); b = a; print(\"%s\\n\", b);");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "x\n");
}

#[test]
fn string_move_then_reassign_ok() {
    // Re-assigning a moved variable re-initializes it, so it may be used again
    let out = run_aero(
        "let a = String::from(\"x\"); let b = a; a = String::from(\"y\"); print(\"%s %s\\n\", a, b);",
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "y x\n");
}

#[test]
fn string_pass_to_fn_moves() {
    // Passing a String by value moves it into the function
    let out = run_aero("fn take(s: String) { print(\"%s\\n\", s); } let a = String::from(\"z\"); take(a);");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "z\n");
}

#[test]
fn string_pass_to_fn_then_use_rejected() {
    // Using the String after passing it by value is a use-after-move error
    let out = run_aero("fn take(s: String) { print(\"%s\\n\", s); } let a = String::from(\"z\"); take(a); print(\"%s\\n\", a);");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("moved"));
}

#[test]
fn vec_is_not_copy() {
    // Vec<T> is heap-owning: moving it then using the old binding is rejected
    let out = run_aero("let v: Vec<i64> = Vec::new(); let w = v; print(w.len());");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "0\n");
    let out = run_aero("let v: Vec<i64> = Vec::new(); let w = v; print(v.len());");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("moved"));
}

#[test]
fn string_return_moves() {
    // Returning a String moves it out of the function; caller owns it
    let out = run_aero(
        "fn mk(s: String) -> String { let t = s; return t; }
let r = mk(String::from(\"ok\"));
print(\"%s\\n\", r);",
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "ok\n");
}

#[test]
fn string_return_then_use_after_rejected() {
    // Returning a String then reading the old binding is a use-after-move error
    let out = run_aero(
        "fn mk(s: String) -> String { return s; }
let r = mk(String::from(\"ok\"));
print(\"%s\\n\", r);",
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "ok\n");
    let out = run_aero(
        "fn mk(s: String) -> String { let t = s; return t; print(\"%s\\n\", s); }
let r = mk(String::from(\"ok\"));",
    );
    assert!(!out.status.success());
    assert!(stderr(&out).contains("moved"));
}

// ---------- Campaign 3: Tensor / matmul / GPU kernel ----------

#[test]
fn tensor_literal_zero_initialized() {
    // tensor(2, 3) is zero-initialized; any element reads 0
    let out = run_aero("let a = tensor(2, 3); print(a[0][0]); print(a[1][2]);");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "0\n0\n");
}

#[test]
fn tensor_index_write_and_read() {
    // Multi-dim index write: a[0][1] = 5, read back
    let out = run_aero("let a = tensor(2, 2); a[0][1] = 5; print(a[0][1]); print(a[1][1]);");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "5\n0\n");
}

#[test]
fn tensor_1d_index_is_scalar() {
    // Indexing a 1-D tensor yields a scalar
    let out = run_aero("let a = tensor(4); a[2] = 7; print(a[2]); print(a[0]);");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "7\n0\n");
}

#[test]
fn tensor_subtensor_index() {
    // Single-level indexing yields a sub-tensor (indexable again)
    let out = run_aero("let a = tensor(2, 3); a[1][2] = 9; let row = a[1]; print(row[2]);");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "9\n");
}

#[test]
fn matmul_square_result() {
    // 2x2 x 2x2 = 2x2; verify element-wise
    let out = run_aero(
        "let a = tensor(2, 2); let b = tensor(2, 2);\n\
         a[0][0] = 1; a[0][1] = 2; a[1][0] = 3; a[1][1] = 4;\n\
         b[0][0] = 5; b[0][1] = 6; b[1][0] = 7; b[1][1] = 8;\n\
         let c = matmul(a, b);\n\
         print(c[0][0]); print(c[0][1]); print(c[1][0]); print(c[1][1]);",
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "19\n22\n43\n50\n");
}

#[test]
fn matmul_non_square_dims_propagate() {
    // 1x3 x 3x2 = 1x2: result dims propagate as a.rows x b.cols
    let out = run_aero(
        "let a = tensor(1, 3); let b = tensor(3, 2);\n\
         a[0][0] = 1; a[0][1] = 2; a[0][2] = 3;\n\
         b[0][0] = 4; b[0][1] = 5; b[1][0] = 6; b[1][1] = 7; b[2][0] = 8; b[2][1] = 9;\n\
         let c = matmul(a, b);\n\
         print(c[0][0]); print(c[0][1]);",
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "40\n46\n");
}

#[test]
fn matmul_dim_mismatch_rejected() {
    // a.shape[1] != b.shape[0] -> compile-time dimension error
    let out = run_aero("let a = tensor(2, 3); let b = tensor(2, 3); let c = matmul(a, b);");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("dimension"));
}

#[test]
fn matmul_scalar_args_rejected() {
    // matmul arguments must be tensors
    let out = run_aero("let c = matmul(1, 2);");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("tensor"));
}

#[test]
fn matmul_wrong_arg_count_rejected() {
    // matmul requires exactly 2 arguments
    let out = run_aero("let a = tensor(2, 2); let c = matmul(a);");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("2") || stderr(&out).contains("matmul"));
}

#[test]
fn tensor_f64_literal_and_element_ops() {
    // tensor<f64>(N) creates a zero-initialized f64 tensor; element-wise
    // + - * / operate on its scalar elements.
    let out = run_aero(
        "let a = tensor<f64>(3);\n\
         a[0] = 1.0; a[1] = 2.0; a[2] = 3.0;\n\
         let s = a[0] + a[1];\n\
         let p = a[0] * a[2];\n\
         let d = a[2] / a[1];\n\
         let m = a[1] - a[0];\n\
         print(s); print(p); print(d); print(m);",
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "3.000000\n3.000000\n1.500000\n1.000000\n");
}

#[test]
fn matmul_f64_square_result() {
    // f64 matmul: 2x2 x 2x2 = 2x2, verified element-wise.
    let out = run_aero(
        "let a = tensor<f64>(2, 2);\n\
         a[0][0] = 1.5; a[0][1] = 2.5; a[1][0] = 3.5; a[1][1] = 4.5;\n\
         let b = tensor<f64>(2, 2);\n\
         b[0][0] = 2.0; b[0][1] = 0.0; b[1][0] = 1.0; b[1][1] = 1.0;\n\
         let c = matmul(a, b);\n\
         print(c[0][0]); print(c[0][1]); print(c[1][0]); print(c[1][1]);",
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "5.500000\n2.500000\n11.500000\n4.500000\n");
}

#[test]
fn matmul_f32_supported() {
    // f32 element type is supported end-to-end (zero-init, index, matmul).
    let out = run_aero(
        "let a = tensor<f32>(1, 2);\n\
         a[0][0] = 1.0; a[0][1] = 2.0;\n\
         let b = tensor<f32>(2, 1);\n\
         b[0][0] = 3.0; b[1][0] = 4.0;\n\
         let c = matmul(a, b);\n\
         print(c[0][0]);",
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "11.000000\n");
}

#[test]
fn tensor_unsupported_element_rejected() {
    // Only numeric element types are supported for tensor literals.
    let out = run_aero("let a = tensor<str>(2);");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("tensor") || stderr(&out).contains("element"));
}

#[test]
fn tensor_bad_element_count_rejected() {
    // tensor<...> takes exactly one element type.
    let out = run_aero("let a = tensor<i64, f64>(2);");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("tensor") || stderr(&out).contains("element"));
}

#[test]
fn reduce_sum_integer_2d() {
    let out = run_aero(
        "let a = tensor(2, 3);\n\
         a[0][0] = 1; a[0][1] = 2; a[0][2] = 3;\n\
         a[1][0] = 4; a[1][1] = 5; a[1][2] = 6;\n\
         print(sum(a));",
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "21\n");
}

#[test]
fn reduce_mean_integer_2d() {
    let out = run_aero(
        "let a = tensor(2, 3);\n\
         a[0][0] = 1; a[0][1] = 2; a[0][2] = 3;\n\
         a[1][0] = 4; a[1][1] = 5; a[1][2] = 6;\n\
         print(mean(a));",
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "3\n"); // 21 / 6 = 3 (integer division)
}

#[test]
fn reduce_max_min_integer() {
    let out = run_aero(
        "let a = tensor(2, 2);\n\
         a[0][0] = -3; a[0][1] = 7;\n\
         a[1][0] = 2; a[1][1] = -5;\n\
         print(max(a)); print(min(a));",
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "7\n-5\n");
}

#[test]
fn reduce_sum_float_2d() {
    let out = run_aero(
        "let a = tensor<f64>(2, 2);\n\
         a[0][0] = 1.5; a[0][1] = 2.5;\n\
         a[1][0] = 3.5; a[1][1] = 4.5;\n\
         print(sum(a)); print(mean(a)); print(max(a)); print(min(a));",
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "12.000000\n3.000000\n4.500000\n1.500000\n");
}

#[test]
fn reduce_1d_tensor() {
    let out = run_aero(
        "let a = tensor<f64>(4);\n\
         a[0] = 2.0; a[1] = 4.0; a[2] = 6.0; a[3] = 8.0;\n\
         print(sum(a)); print(mean(a));",
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "20.000000\n5.000000\n");
}

#[test]
fn reduce_non_tensor_rejected() {
    let out = run_aero("print(sum(5));");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("sum") || stderr(&out).contains("tensor"));
}

#[test]
fn builtin_reduce_redefinition_rejected() {
    let out = run_aero("fn sum(a: i64, b: i64) -> i64 { return a; }");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("sum"));
}

#[test]
fn elemwise_add_integer_2d() {
    let out = run_aero(
        "let a = tensor(2, 2);\n\
         a[0][0] = 1; a[0][1] = 2; a[1][0] = 3; a[1][1] = 4;\n\
         let b = tensor(2, 2);\n\
         b[0][0] = 10; b[0][1] = 20; b[1][0] = 30; b[1][1] = 40;\n\
         let c = tensor_add(a, b);\n\
         print(c[0][0]); print(c[0][1]); print(c[1][0]); print(c[1][1]);",
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "11\n22\n33\n44\n");
}

#[test]
fn elemwise_sub_mul_div_neg_integer() {
    let out = run_aero(
        "let a = tensor(2, 2);\n\
         a[0][0] = 10; a[0][1] = 6; a[1][0] = 3; a[1][1] = 1;\n\
         let b = tensor(2, 2);\n\
         b[0][0] = 4; b[0][1] = 2; b[1][0] = 3; b[1][1] = 1;\n\
         let s = tensor_sub(a, b);\n\
         let m = tensor_mul(a, b);\n\
         let d = tensor_div(a, b);\n\
         let n = tensor_neg(a);\n\
         print(s[0][1]); print(m[1][1]); print(d[1][0]); print(n[0][0]);",
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "4\n1\n1\n-10\n");
}

#[test]
fn elemwise_float_add() {
    let out = run_aero(
        "let a = tensor<f64>(2, 2);\n\
         a[0][0] = 1.5; a[0][1] = 2.5; a[1][0] = 3.5; a[1][1] = 4.5;\n\
         let b = tensor<f64>(2, 2);\n\
         b[0][0] = 0.5; b[0][1] = 1.0; b[1][0] = 1.5; b[1][1] = 2.0;\n\
         let c = tensor_mul(a, b);\n\
         print(c[0][0]); print(c[0][1]); print(c[1][0]); print(c[1][1]);",
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "0.750000\n2.500000\n5.250000\n9.000000\n");
}

#[test]
fn elemwise_shape_mismatch_rejected() {
    let out = run_aero(
        "let a = tensor(2, 2);\n\
         let b = tensor(2, 3);\n\
         let c = tensor_add(a, b);",
    );
    assert!(!out.status.success());
    assert!(stderr(&out).contains("shape"));
}

#[test]
fn elemwise_non_tensor_rejected() {
    let out = run_aero("let c = tensor_add(1, 2);");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("tensor"));
}

#[test]
fn gpu_kernel_declaration_accepted() {
    // extern "gpu" fn declaration is legal; the kernel itself does not run
    let out = run_aero("extern \"gpu\" fn add_kernel(a: i64) {}\nprint(1);");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "1\n");
}

#[test]
fn gpu_kernel_must_return_void() {
    // GPU kernels cannot return a value
    let out = run_aero("extern \"gpu\" fn bad_kernel(a: i64) -> i64 { return a; }");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("void") || stderr(&out).contains("GPU"));
}

#[test]
fn gpu_kernel_call_from_cpu_rejected() {
    // Calling a GPU kernel from CPU code is forbidden (isolation)
    let out = run_aero("extern \"gpu\" fn add_kernel(a: i64) {}\nadd_kernel(1);");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("GPU"));
}

#[test]
fn builtin_matmul_redefinition_rejected() {
    // matmul is a builtin; it cannot be redefined
    let out = run_aero("fn matmul(a: i64, b: i64) -> i64 { return a; }");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("matmul"));
}

// ---------- BLAS Level-1 builtins (BLAS binding, CPU backend) ----------

#[test]
fn blas_dot_integer_returns_scalar() {
    let out = run_aero(
        "let a = tensor(3); a[0] = 1; a[1] = 2; a[2] = 3;\n\
         let b = tensor(3); b[0] = 4; b[1] = 5; b[2] = 6;\n\
         print(blas_dot(a, b));",
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "32\n"); // 1*4+2*5+3*6
}

#[test]
fn blas_dot_float_returns_scalar() {
    let out = run_aero(
        "let a = tensor<f64>(3); a[0] = 1.5; a[1] = 2.0; a[2] = 0.5;\n\
         let b = tensor<f64>(3); b[0] = 2.0; b[1] = 1.0; b[2] = 4.0;\n\
         print(blas_dot(a, b));",
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "7.000000\n"); // 3.0+2.0+2.0
}

#[test]
fn blas_nrm2_float() {
    let out = run_aero(
        "let x = tensor<f64>(2); x[0] = 3.0; x[1] = 4.0;\n\
         print(blas_nrm2(x));",
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "5.000000\n"); // sqrt(3^2+4^2)
}

#[test]
fn blas_asum_absolute_sum() {
    let out = run_aero(
        "let x = tensor(4); x[0] = -3; x[1] = 7; x[2] = 2; x[3] = -5;\n\
         print(blas_asum(x));",
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "17\n"); // 3+7+2+5
}

#[test]
fn blas_amax_returns_index() {
    let out = run_aero(
        "let x = tensor(4); x[0] = -3; x[1] = 7; x[2] = 2; x[3] = -5;\n\
         print(blas_amax(x));",
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "1\n"); // index 1 holds the largest |value|=7
}

#[test]
fn blas_scal_scales_in_place() {
    let out = run_aero(
        "let x = tensor(3); x[0] = 1; x[1] = -2; x[2] = 3;\n\
         let y = blas_scal(2, x);\n\
         print(y[0]); print(y[1]); print(y[2]);",
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "2\n-4\n6\n");
}

#[test]
fn blas_axpy_alpha_x_plus_y() {
    let out = run_aero(
        "let x = tensor(3); x[0] = 1; x[1] = 2; x[2] = 3;\n\
         let y = tensor(3); y[0] = 10; y[1] = 20; y[2] = 30;\n\
         let z = blas_axpy(2, x, y);\n\
         print(z[0]); print(z[1]); print(z[2]);",
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "12\n24\n36\n"); // y + 2x
}

#[test]
fn blas_builtin_redefinition_rejected() {
    let out = run_aero("fn blas_dot(a: i64) -> i64 { return a; }");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("blas_dot"));
}

#[test]
fn blas_non_tensor_arg_rejected() {
    let out = run_aero("print(blas_dot(1, 2));");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("tensor") || stderr(&out).contains("BLAS"));
}

#[test]
fn blas_wrong_arg_count_rejected() {
    let out = run_aero("let x = tensor(2); let y = blas_axpy(1, x);");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("axpy"));
}

// ---------- Reverse-mode automatic differentiation (AD) ----------

#[test]
fn ad_reverse_mode_linear_gradients() {
    // f = x*y + x  =>  df/dx = y+1, df/dy = x
    let out = run_aero(
        "let t = grad_new();\n\
         let x = g_leaf(t, 3.0); let y = g_leaf(t, 4.0);\n\
         let p = g_mul(t, x, y);\n\
         let f = g_add(t, p, x);\n\
         print(g_val(t, f));\n\
         g_backward(t, f);\n\
         print(g_grad(t, x)); print(g_grad(t, y));",
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "15.000000\n5.000000\n3.000000\n");
}

#[test]
fn ad_reverse_mode_division_quotient_rule() {
    // q = x / y  =>  dq/dx = 1/y, dq/dy = -x/y^2
    let out = run_aero(
        "let t = grad_new();\n\
         let a = g_leaf(t, 3.0); let b = g_leaf(t, 4.0);\n\
         let q = g_div(t, a, b);\n\
         print(g_val(t, q));\n\
         g_backward(t, q);\n\
         print(g_grad(t, a)); print(g_grad(t, b));",
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "0.750000\n0.250000\n-0.187500\n");
}

#[test]
fn ad_reverse_mode_shared_subgraph() {
    // h = (x*x) + (x*x) with a single shared node s=x*x => dh/dx = 4x
    let out = run_aero(
        "let t = grad_new();\n\
         let x = g_leaf(t, 2.0);\n\
         let s = g_mul(t, x, x);\n\
         let h = g_add(t, s, s);\n\
         g_backward(t, h);\n\
         print(g_val(t, h)); print(g_grad(t, x));",
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    // 2*(2*2)=8 ; d/dx = 2*(2x) = 8
    assert_eq!(stdout(&out), "8.000000\n8.000000\n");
}

#[test]
fn ad_sub_and_neg() {
    // g(x) = -(x - 1) => g' = -1
    let out = run_aero(
        "let t = grad_new();\n\
         let x = g_leaf(t, 5.0);\n\
         let one = g_leaf(t, 1.0);\n\
         let s = g_sub(t, x, one);\n\
         let n = g_neg(t, s);\n\
         g_backward(t, n);\n\
         print(g_val(t, n)); print(g_grad(t, x));",
    );
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "-4.000000\n-1.000000\n");
}

// ---------- Campaign 4: assert builtins / test runner / package manager ----------

#[test]
fn assert_passes_and_continues() {
    let out = run_aero("assert(true);\nassert_eq(1 + 2, 3);\nprint(7);");
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "7\n");
}

#[test]
fn assert_failure_aborts() {
    // Assertion failure: prints a diagnostic + non-zero exit code
    let out = run_aero("assert_eq(1, 2);\nprint(7);");
    assert!(!out.status.success());
    assert!(stdout(&out).contains("assertion failed"));
}

#[test]
fn assert_requires_bool() {
    // assert argument must be boolean
    let out = run_aero("assert(1);");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("type"));
}

#[test]
fn builtin_assert_redefinition_rejected() {
    // assert is a builtin; it cannot be redefined
    let out = run_aero("fn assert(x: bool) {}");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("duplicate definition") || stderr(&out).contains("assert"));
}

#[test]
fn builtin_assert_not_usable_as_expr() {
    // Builtin asserts have no return value; cannot be used as expressions
    let out = run_aero("let x = assert(true);");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("void"));
}

#[test]
fn test_command_collects_and_runs() {
    // aero test: collect test_-prefixed functions and run each in its own subprocess
    let dir = std::env::temp_dir().join(format!("aero_cli_test_cmd_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("cases.aero");
    std::fs::write(
        &file,
        "fn test_add() { assert_eq(1 + 2, 3); }\nfn test_fail() { assert_eq(1, 2); }\nfn helper() {}\n",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_aero"))
        .args(["test", file.to_str().unwrap()])
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let text = stdout(&out) + &stderr(&out);
    assert!(text.contains("PASS  test_add"), "output: {text}");
    assert!(text.contains("FAIL  test_fail"), "output: {text}");
    assert!(!out.status.success()); // non-zero exit when any case fails
}

#[test]
fn package_new_build_run_flow() {
    // aero new + build + run (directory-package flow)
    let root = std::env::temp_dir().join(format!("aero_pkg_e2e_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let bin = env!("CARGO_BIN_EXE_aero");
    let new_out = Command::new(bin)
        .current_dir(&root)
        .args(["new", "demo"])
        .output()
        .unwrap();
    assert!(new_out.status.success(), "new failed: {}", String::from_utf8_lossy(&new_out.stderr));
    let pkg = root.join("demo");
    // build (inside the package)
    let build_out = Command::new(bin)
        .current_dir(&pkg)
        .args(["build"])
        .output()
        .unwrap();
    assert!(build_out.status.success(), "build failed: {}", String::from_utf8_lossy(&build_out.stderr));
    assert!(String::from_utf8_lossy(&build_out.stdout).contains("build succeeded"));
    // run (directory)
    let run_out = Command::new(bin)
        .current_dir(&pkg)
        .args(["run", "."])
        .output()
        .unwrap();
    assert!(run_out.status.success(), "run failed: {}", String::from_utf8_lossy(&run_out.stderr));
    assert_eq!(stdout(&run_out), "Hello, Aero!\n");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn package_with_dependency_runs() {
    // Dependency merge: the lib provides functions, the root package calls them
    let root = std::env::temp_dir().join(format!("aero_pkg_dep_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let lib = root.join("libmath");
    let app = root.join("app");
    std::fs::create_dir_all(lib.join("src")).unwrap();
    std::fs::create_dir_all(app.join("src")).unwrap();
    std::fs::write(
        lib.join("Aero.toml"),
        "[package]\nname = \"libmath\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::write(
        lib.join("src/lib.aero"),
        "fn double(x: i64) -> i64 { return x * 2; }\n",
    )
    .unwrap();
    std::fs::write(
        app.join("Aero.toml"),
        "[package]\nname = \"app\"\nversion = \"0.1.0\"\n\n[dependencies]\nlibmath = { path = \"../libmath\" }\n",
    )
    .unwrap();
    std::fs::write(app.join("src/main.aero"), "print(double(21));\n").unwrap();
    let bin = env!("CARGO_BIN_EXE_aero");
    let out = Command::new(bin)
        .current_dir(&app)
        .args(["run", "."])
        .output()
        .unwrap();
    let _ = std::fs::remove_dir_all(&root);
    assert!(out.status.success(), "run failed: {}", stderr(&out));
    assert_eq!(stdout(&out), "42\n");
}

// ---------- Campaign 5: AOT compilation (standalone executables) ----------

#[test]
fn aero_build_produces_independent_exe() {
    // aero build produces a standalone exe run directly (no aero toolchain / JIT)
    let root = std::env::temp_dir().join(format!("aero_aot_e2e_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let bin = env!("CARGO_BIN_EXE_aero");
    let new_out = Command::new(bin)
        .current_dir(&root)
        .args(["new", "demo"])
        .output()
        .unwrap();
    assert!(new_out.status.success(), "new failed: {}", String::from_utf8_lossy(&new_out.stderr));
    let pkg = root.join("demo");
    let build_out = Command::new(bin)
        .current_dir(&pkg)
        .args(["build"])
        .output()
        .unwrap();
    assert!(build_out.status.success(), "build failed: {}", String::from_utf8_lossy(&build_out.stderr));
    assert!(String::from_utf8_lossy(&build_out.stdout).contains("build succeeded"));
    // The standalone exe exists and runs directly
    let exe = pkg.join("target").join("aero").join("demo.exe");
    assert!(exe.exists(), "exe not produced: {}", exe.display());
    let run_out = Command::new(&exe).output().unwrap();
    assert!(run_out.status.success(), "exe run failed: {}", String::from_utf8_lossy(&run_out.stderr));
    assert_eq!(stdout(&run_out), "Hello, Aero!\n");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn aero_build_single_file_produces_exe() {
    // Single-file AOT: aero build <file.aero> produces <name>.exe next to it
    let root = std::env::temp_dir().join(format!("aero_aot_file_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let src = root.join("prog.aero");
    std::fs::write(&src, "print(1 + 2);\n").unwrap();
    let bin = env!("CARGO_BIN_EXE_aero");
    let out = Command::new(bin)
        .args(["build", src.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success(), "build failed: {}", String::from_utf8_lossy(&out.stderr));
    let exe = root.join("prog.exe");
    assert!(exe.exists(), "exe not produced: {}", exe.display());
    let run_out = Command::new(&exe).output().unwrap();
    assert!(run_out.status.success());
    assert_eq!(stdout(&run_out), "3\n");
    let _ = std::fs::remove_dir_all(&root);
}
#[test]
fn aero_file_io_and_cli_args_in_aot_exe() {
    // M1.2: read_file/write_file + arg_count/arg in a standalone AOT exe run
    // with command-line arguments.
    let root = std::env::temp_dir().join(format!("aero_m12_io_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let src = root.join("io.aero");
    std::fs::write(
        &src,
        "print(\"argc=%lld\\n\", arg_count());\n\
if (arg_count() > 1) {\n\
    print(\"arg1=%s\\n\", arg(1));\n\
}\n\
let w = write_file(\"out.txt\", \"hello file\\n\");\n\
print(\"wrote=%lld\\n\", w);\n\
let contents = read_file(\"out.txt\");\n\
print(\"len=%lld\\n\", len(contents));\n\
print(\"content=%s\\n\", contents);\n",
    )
    .unwrap();
    let bin = env!("CARGO_BIN_EXE_aero");
    let out = Command::new(bin).args(["build", src.to_str().unwrap()]).output().unwrap();
    assert!(out.status.success(), "build failed: {}", String::from_utf8_lossy(&out.stderr));
    let exe = root.join("io.exe");
    assert!(exe.exists(), "exe not produced: {}", exe.display());
    let run_out = Command::new(&exe)
        .current_dir(&root)
        .args(["alpha", "beta"])
        .output()
        .unwrap();
    assert!(run_out.status.success(), "exe run failed: {}", String::from_utf8_lossy(&run_out.stderr));
    let expected = "argc=3\narg1=alpha\nwrote=11\nlen=11\ncontent=hello file\n\n";
    assert_eq!(stdout(&run_out), expected);
    // the file was actually written by the exe
    let file = root.join("out.txt");
    assert!(file.exists(), "out.txt was not written");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello file\n");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn aero_cli_args_via_jit_are_zero() {
    // JIT runs main(argc=0, argv=null): arg_count() must report 0.
    let out = run_aero("print(\"%lld\\n\", arg_count());\n");
    assert!(out.status.success(), "run failed: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(stdout(&out), "0\n");
}

// ---------- Campaign 5: FFI (extern "C" + [link] linking external libs) ----------

#[test]
fn extern_c_calls_libc() {
    // FFI: aliased and same-name libc calls (the AOT artifact runs standalone)
    let root = std::env::temp_dir().join(format!("aero_ffi_libc_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let src = root.join("prog.aero");
    std::fs::write(
        &src,
        "extern \"C\" fn string_len(s: str) -> i64 = \"strlen\";\n\
         extern \"C\" fn abs_c(x: i32) -> i32 = \"abs\";\n\
         extern \"C\" fn putchar(c: i32) -> i32;\n\
         print(string_len(\"hello\"));\n\
         print(abs_c(-42));\n\
         let c = putchar(65);\n\
         print(c);\n",
    )
    .unwrap();
    let bin = env!("CARGO_BIN_EXE_aero");
    let out = Command::new(bin)
        .args(["build", src.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success(), "build failed: {}", String::from_utf8_lossy(&out.stderr));
    let exe = root.join("prog.exe");
    let run_out = Command::new(&exe).output().unwrap();
    assert!(run_out.status.success());
    assert_eq!(stdout(&run_out), "5\n42\nA65\n");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn ffi_link_section_links_c_library() {
    // [link] section: link a custom C static library (lib_paths + libs)
    let root = std::env::temp_dir().join(format!("aero_ffi_clib_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let pkg = root.join("clibdemo");
    std::fs::create_dir_all(pkg.join("src")).unwrap();
    std::fs::write(
        pkg.join("mylib.c"),
        "int aero_add(int a, int b) { return a + b; }\nint aero_mul3(int a) { return a * 3; }\n",
    )
    .unwrap();
    // Compile the C library into a static library with gcc
    let cc = Command::new("gcc")
        .current_dir(&pkg)
        .args(["-c", "mylib.c", "-o", "mylib.o"])
        .output()
        .unwrap();
    assert!(cc.status.success(), "gcc -c failed: {}", String::from_utf8_lossy(&cc.stderr));
    let ar = Command::new("ar")
        .current_dir(&pkg)
        .args(["rcs", "libmylib.a", "mylib.o"])
        .output()
        .unwrap();
    assert!(ar.status.success(), "ar failed: {}", String::from_utf8_lossy(&ar.stderr));
    std::fs::write(
        pkg.join("Aero.toml"),
        "[package]\nname = \"clibdemo\"\nversion = \"0.1.0\"\n\n[link]\nlibs = [\"mylib\"]\nlib_paths = [\".\"]\n",
    )
    .unwrap();
    std::fs::write(
        pkg.join("src/main.aero"),
        "extern \"C\" fn aero_add(a: i32, b: i32) -> i32;\nextern \"C\" fn aero_mul3(a: i32) -> i32;\n\
         print(aero_add(2, 3));\nprint(aero_mul3(7));\nprint(aero_add(aero_mul3(4), 1));\n",
    )
    .unwrap();
    let bin = env!("CARGO_BIN_EXE_aero");
    let out = Command::new(bin)
        .current_dir(&pkg)
        .args(["build"])
        .output()
        .unwrap();
    assert!(out.status.success(), "build failed: {}", String::from_utf8_lossy(&out.stderr));
    let exe = pkg.join("target/aero/clibdemo.exe");
    let run_out = Command::new(&exe).output().unwrap();
    assert!(run_out.status.success());
    assert_eq!(stdout(&run_out), "5\n21\n13\n");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn ffi_link_windows_api() {
    // [link] links a system lib: GetTickCount from kernel32 (Windows API)
    let root = std::env::temp_dir().join(format!("aero_ffi_win_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let pkg = root.join("winpkg");
    std::fs::create_dir_all(pkg.join("src")).unwrap();
    std::fs::write(
        pkg.join("Aero.toml"),
        "[package]\nname = \"winpkg\"\nversion = \"0.1.0\"\n\n[link]\nlibs = [\"kernel32\"]\n",
    )
    .unwrap();
    std::fs::write(
        pkg.join("src/main.aero"),
        "extern \"C\" fn GetTickCount() -> i32;\nlet t = GetTickCount();\nprint(t);\nif (t > 0) { print(\"tick works!\"); }\n",
    )
    .unwrap();
    let bin = env!("CARGO_BIN_EXE_aero");
    let out = Command::new(bin)
        .current_dir(&pkg)
        .args(["build"])
        .output()
        .unwrap();
    assert!(out.status.success(), "build failed: {}", String::from_utf8_lossy(&out.stderr));
    let exe = pkg.join("target/aero/winpkg.exe");
    let run_out = Command::new(&exe).output().unwrap();
    assert!(run_out.status.success());
    assert!(stdout(&run_out).contains("tick works!"));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn extern_c_bad_types_rejected() {
    // bool parameters are not C-ABI compatible -> compile rejection
    let out = run_aero("extern \"C\" fn bad(x: bool) -> i64;\nprint(1);");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("C ABI") || stderr(&out).contains("compatible"));
}

#[test]
fn ffi_sqlite_end_to_end_demo() {
    // Full SQLite demo: project with [link] + mod sqlite { ... } + prepared
    // statements, transactions, and DELETE. Requires libsqlite3 on the system.
    let root = std::env::temp_dir().join(format!("aero_ffi_sqlite_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let pkg = root.join("ffi_sqlite");
    std::fs::create_dir_all(pkg.join("src")).unwrap();
    std::fs::write(
        pkg.join("Aero.toml"),
        "[package]\nname = \"ffi_sqlite\"\nversion = \"0.1.0\"\n\n[link]\nlibs = [\"sqlite3\"]\n",
    )
    .unwrap();
    std::fs::write(
        pkg.join("src/main.aero"),
        r#"
mod sqlite {
    extern "C" fn sqlite3_libversion() -> *i32;
    extern "C" fn sqlite3_open(filename: str, db: * *i32) -> i32;
    extern "C" fn sqlite3_close(db: *i32) -> i32;
    extern "C" fn sqlite3_errmsg(db: *i32) -> *i32;
    extern "C" fn sqlite3_exec(db: *i32, sql: str, cb: *i32, arg: *i32, err: * *i32) -> i32;
    extern "C" fn sqlite3_prepare_v2(db: *i32, sql: str, n: i32, stmt: * *i32, tail: * *i32) -> i32;
    extern "C" fn sqlite3_step(stmt: *i32) -> i32;
    extern "C" fn sqlite3_finalize(stmt: *i32) -> i32;
    extern "C" fn sqlite3_column_int(stmt: *i32, col: i32) -> i32;
    extern "C" fn sqlite3_column_text(stmt: *i32, col: i32) -> *i32;
    extern "C" fn sqlite3_changes(db: *i32) -> i32;
}
fn exec(db: *i32, sql: str) -> i32 {
    let rc = sqlite::sqlite3_exec(db, sql, 0, 0, 0);
    if (rc != 0) { print("SQL error: %s\n", sqlite::sqlite3_errmsg(db)); }
    return rc;
}
fn scalar(db: *i32, sql: str) -> i32 {
    let stmt: *i32 = 0;
    let prc = sqlite::sqlite3_prepare_v2(db, sql, -1, &stmt, 0);
    if (prc != 0) { return -1; }
    let r = sqlite::sqlite3_step(stmt);
    if (r != 100) { sqlite::sqlite3_finalize(stmt); return -1; }
    let v = sqlite::sqlite3_column_int(stmt, 0);
    sqlite::sqlite3_finalize(stmt);
    return v;
}
fn main_test() {
    let db: *i32 = 0;
    let rc = sqlite::sqlite3_open(":memory:", &db);
    if (rc != 0) { print("open failed"); return; }
    sqlite::sqlite3_exec(db, "CREATE TABLE t (v INTEGER)", 0, 0, 0);
    sqlite::sqlite3_exec(db, "INSERT INTO t VALUES (10)", 0, 0, 0);
    sqlite::sqlite3_exec(db, "INSERT INTO t VALUES (20)", 0, 0, 0);
    sqlite::sqlite3_exec(db, "INSERT INTO t VALUES (30)", 0, 0, 0);
    let v = scalar(db, "SELECT sum(v) FROM t");
    print(v);
    sqlite::sqlite3_exec(db, "BEGIN", 0, 0, 0);
    sqlite::sqlite3_exec(db, "INSERT INTO t VALUES (40)", 0, 0, 0);
    sqlite::sqlite3_exec(db, "ROLLBACK", 0, 0, 0);
    let r1 = scalar(db, "SELECT count(*) FROM t");
    print(r1);
    sqlite::sqlite3_close(db);
}
main_test();
"#,
    )
    .unwrap();
    let bin = env!("CARGO_BIN_EXE_aero");
    let out = Command::new(bin)
        .current_dir(&pkg)
        .args(["build"])
        .output()
        .unwrap();
    assert!(out.status.success(), "build failed: {}", String::from_utf8_lossy(&out.stderr));
    let exe = pkg.join("target/aero/ffi_sqlite.exe");
    let run_out = Command::new(&exe).output().unwrap();
    assert!(run_out.status.success(), "run failed: {}", String::from_utf8_lossy(&run_out.stderr));
    let text = stdout(&run_out);
    assert!(text.contains("60"), "expected sum=60, got:\n{text}");
    assert!(text.contains("3"), "expected count=3 after rollback, got:\n{text}");
    let _ = std::fs::remove_dir_all(&root);
}

// ---------- Algebraic data types (enum) ----------

#[test]
fn enum_adts_end_to_end() {
    // Enum defs, tagged-union layout, construction (explicit Enum::Variant),
    // match with payload binding, bare-variant patterns, enum passing/return.
    let src = r#"
struct Rect {
    w: i64,
    h: i64,
}

enum Maybe {
    Nothing,
    Just(i64),
}

enum Shape {
    Circle(f64),
    Box(Rect),
}

fn describe(m: Maybe) -> i64 {
    match (m) {
        Nothing => { return 0; }
        Just(v) => { return v; }
    }
    return -1;
}

fn area(s: Shape) -> i64 {
    match (s) {
        Circle(r) => { return 1; }
        Box(b) => { return b.w * b.h; }
    }
    return -1;
}

let a = Maybe::Nothing;
let b = Maybe::Just(42);
print("a = %lld\n", describe(a));
print("b = %lld\n", describe(b));

let c = Maybe::Just(7);
match (c) {
    Nothing => { print("c is nothing\n"); }
    Just(x) => { print("c = %lld\n", x); }
}

let s = Shape::Box(Rect { w: 3, h: 4 });
print("area = %lld\n", area(s));

let t = Shape::Circle(2.5);
match (t) {
    Circle(r) => { print("circle\n"); }
    Box(b) => { print("box %lld %lld\n", b.w, b.h); }
}
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "a = 0\nb = 42\nc = 7\narea = 12\ncircle\n");
}

#[test]
fn enum_match_wildcard_in_middle_no_crash() {
    // Regression: a wildcard/binding arm placed BEFORE a trailing variant arm
    // previously emitted instructions on an already-terminated basic block,
    // crashing codegen with an access violation (0xC0000005) / stack overflow.
    // The wildcard must match immediately and suppress all later arms.
    let src = r#"
enum W {
    X,
    Y,
    Z,
}

match (W::Y) {
    X => { print("x\n"); }
    _ => { print("wildcard\n"); }
    Z => { print("z\n"); }
}

match (W::Z) {
    _ => { print("first\n"); }
    X => { print("x\n"); }
}

match (W::X) {
    X => { print("x\n"); }
    _ => { print("wildcard\n"); }
}
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "wildcard\nfirst\nx\n");
}

#[test]
fn question_mark_propagates_result() {
    // `?` unwraps Result<T, E>: on Ok the payload continues the expression;
    // on Err the error is returned from the enclosing function immediately.
    let src = r#"
fn checked(a: i64) -> Result<i64, i64> {
    if (a < 0) { return Result::Err(a); }
    return Result::Ok(a * 2);
}

fn run(a: i64) -> Result<i64, i64> {
    let x = checked(a)?;
    let y = checked(a + 1)?;
    return Result::Ok(x + y);
}

let ok = run(10);
match (ok) {
    Ok(v) => { print("ok=%lld\n", v); }
    Err(e) => { print("err=%lld\n", e); }
}

let bad = run(-3);
match (bad) {
    Ok(v) => { print("ok=%lld\n", v); }
    Err(e) => { print("err=%lld\n", e); }
}
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "ok=42\nerr=-3\n");
}

#[test]
fn question_mark_on_non_result_rejected() {
    // `?` can only be applied to a `Result<T, E>`
    let out = run_aero("fn f() -> i64 { let x = 5?; return x; }");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("Result"));
}

#[test]
fn question_mark_requires_result_return_rejected() {
    // `?` requires the enclosing function to return a `Result<_, E>`
    let out = run_aero("fn f() -> i64 { let x = Result::Ok(5)?; return x; }");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("Result"));
}

#[test]
fn enum_unknown_variant_compile_rejected() {
    // Enum::Variant must name a variant of a declared enum
    let out = run_aero("enum E { A, B(i64) }\nlet x = E::C(1);");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("variant"));
}

#[test]
fn enum_duplicate_variant_compile_rejected() {
    let out = run_aero("enum E { A, A }\nlet x = E::A;");
    assert!(!out.status.success());
}

// ---------- Trait system (static dispatch) ----------

#[test]
fn trait_basic_end_to_end() {
    // Trait def, trait impl, inherent impl, method calls, and a generic function
    // with a trait bound (method calls on a generic param dispatch statically).
    let src = r#"
struct Rect {
    w: i64,
    h: i64,
}

struct Square {
    side: i64,
}

trait Drawable {
    fn draw(s: Square);
}

impl Drawable for Square {
    fn draw(s: Square) {
        print("square %lld\n", s.side);
    }
}

impl Rect {
    fn area(r: Rect) -> i64 {
        return r.w * r.h;
    }
}

fn draw_area<T: Drawable>(d: T) {
    d.draw();
}

let r = Rect { w: 3, h: 4 };
print("area = %lld\n", r.area());

let s = Square { side: 5 };
s.draw();
draw_area(s);
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "area = 12\nsquare 5\nsquare 5\n");
}

#[test]
fn trait_bound_violation_compile_rejected() {
    // i64 does not implement Drawable; the bound on `T` is checked at instantiation
    let src = r#"
struct Square { side: i64, }
trait Drawable { fn draw(s: Square); }
impl Drawable for Square { fn draw(s: Square) { print(1); } }
fn draw_area<T: Drawable>(d: T) { d.draw(); }
draw_area(42);
"#;
    let out = run_aero(src);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("does not implement trait"));
}

#[test]
fn trait_missing_impl_method_compile_rejected() {
    let src = r#"
struct Square { side: i64, }
trait Drawable { fn draw(s: Square); fn name(s: Square); }
impl Drawable for Square { fn draw(s: Square) { print(1); } }
"#;
    let out = run_aero(src);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("not implemented"));
}

#[test]
fn trait_unknown_method_compile_rejected() {
    let out = run_aero("struct P { x: i64, } let p = P { x: 1 }; p.nope();");
    assert!(!out.status.success());
    assert!(stderr(&out).contains("has no method"));
}

#[test]
fn extern_c_body_rejected() {
    // extern "C" declarations cannot have a body (semicolon expected)
    let out = run_aero("extern \"C\" fn bad() -> i64 { return 1; }\nprint(1);");
    assert!(!out.status.success());
}

// ---------- Phase 6: Drop / RAII ----------

#[test]
fn drop_at_block_end_reverse_order() {
    // Values implementing `Drop` are dropped when they go out of scope, in reverse
    // declaration order (last declared drops first).
    let src = r#"
struct File { id: i64, }
impl Drop for File { fn drop(x: &mut File) { print("drop(%lld)\n", (*x).id); } }
let f = File { id: 1 };
let g = File { id: 2 };
print("end\n");
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "end\ndrop(2)\ndrop(1)\n");
}

#[test]
fn drop_skipped_after_move() {
    // A moved value's source must not be dropped (ownership transferred to the new
    // binding, which drops it exactly once).
    let src = r#"
struct File { id: i64, }
impl Drop for File { fn drop(x: &mut File) { print("drop(%lld)\n", (*x).id); } }
let f = File { id: 1 };
let h = f;
print("end\n");
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "end\ndrop(1)\n");
}

#[test]
fn drop_param_and_local_at_function_end() {
    // A function drops its locals first (reverse decl order), then its params.
    let src = r#"
struct File { id: i64, }
impl Drop for File { fn drop(x: &mut File) { print("drop(%lld)\n", (*x).id); } }
fn take(f: File) {
    let g = File { id: 7 };
    print("in take\n");
}
take(File { id: 5 });
print("end\n");
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "in take\ndrop(7)\ndrop(5)\nend\n");
}

#[test]
fn drop_returned_value_not_dropped_in_fn() {
    // `return f` moves the value out of the function: `f` is not dropped inside;
    // the caller's binding drops it at its own scope end.
    let src = r#"
struct File { id: i64, }
impl Drop for File { fn drop(x: &mut File) { print("drop(%lld)\n", (*x).id); } }
fn give_back(f: File) -> File { return f; }
let r = give_back(File { id: 9 });
print("end\n");
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "end\ndrop(9)\n");
}

// ---------- Phase 7: iterators + `for x in iter` ----------

#[test]
fn for_vec_native_iteration() {
    // `for x in v` over a native `Vec<T>` lowers to an index loop over the heap buffer.
    let src = r#"
let v: Vec<i64> = Vec::new();
v.push(10);
v.push(20);
v.push(30);
for (x in v) {
    print("%lld\n", x);
}
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "10\n20\n30\n");
}

#[test]
fn for_user_intoiterator_protocol() {
    // A user type implements `IntoIterator` + `Iterator`; `for x in it` desugars to
    // `let mut it = it.into_iter(); loop { match it.next() { Some(x) => .., None => break } }`.
    let src = r#"
struct Counter { cur: i64 }
impl Iterator for Counter {
    type Item = i64;
    fn next(c: &mut Counter) -> Option<i64> {
        if (c.cur < 3) {
            let v = c.cur;
            c.cur = c.cur + 1;
            return Option::Some(v);
        }
        return Option::None;
    }
}
impl IntoIterator for Counter {
    type IntoIter = Counter;
    fn into_iter(c: Counter) -> Counter { return c; }
}
let c = Counter { cur: 0 };
for (n in c) {
    print("counter %lld\n", n);
}
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "counter 0\ncounter 1\ncounter 2\n");
}

#[test]
fn for_generic_iterator_protocol() {
    // Generic user type iterator: the protocol methods are monomorphized from the
    // receiver's type args (`WrapIter<i64>` → `T = i64`).
    let src = r#"
struct WrapIter<T> { items: [T; 2], idx: i64 }
impl<T> Iterator for WrapIter<T> {
    type Item = T;
    fn next(w: &mut WrapIter<T>) -> Option<T> {
        let a = w.items;
        if (w.idx < 2) {
            let v = w.idx;
            w.idx = w.idx + 1;
            return Option::Some(a[v]);
        }
        return Option::None;
    }
}
impl<T> IntoIterator for WrapIter<T> {
    type IntoIter = WrapIter<T>;
    fn into_iter(w: WrapIter<T>) -> WrapIter<T> { return w; }
}
let w = WrapIter { items: [7, 9], idx: 0 };
for (n in w) {
    print("wrap %lld\n", n);
}
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "wrap 7\nwrap 9\n");
}

#[test]
fn for_non_iterable_rejected() {
    // Iterating a type with no `IntoIterator`/`Iterator` impl is a type error.
    let src = r#"
struct NoIter { x: i64 }
let n = NoIter { x: 1 };
for (k in n) {
    print("%lld\n", k);
}
"#;
    let out = run_aero(src);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("into_iter"));
}

// ---------- Phase 8: operator overloading (Add / Eq / Ord traits) ----------

#[test]
fn op_overload_arith_and_cmp() {
    // A user type overloads `+` via `Add`, `==`/`!=` via `Eq`, `<`/`>`/`<=`/`>=`
    // via `Ord`. Codegen desugars each operator to the corresponding trait call,
    // deriving `>`, `<=`, `>=` from `lt` by operand swap / negation.
    let src = r#"
#[derive(Copy)]
struct Point { x: i64, y: i64 }

impl Add<Point, Point> for Point {
    fn add(lhs: Point, rhs: Point) -> Point {
        return Point { x: lhs.x + rhs.x, y: lhs.y + rhs.y };
    }
}

impl Eq<Point> for Point {
    fn eq(lhs: Point, rhs: Point) -> bool {
        return (lhs.x == rhs.x) && (lhs.y == rhs.y);
    }
}

impl Ord<Point> for Point {
    fn lt(lhs: Point, rhs: Point) -> bool {
        if (lhs.x != rhs.x) {
            return lhs.x < rhs.x;
        }
        return lhs.y < rhs.y;
    }
}

let a = Point { x: 1, y: 2 };
let b = Point { x: 3, y: 4 };
let c = a + b;
print("c.x=%lld c.y=%lld\n", c.x, c.y);

if (a == b) { print("a==b\n"); } else { print("a!=b\n"); }
if (a != b) { print("a!=b2\n"); }
if (a == a) { print("a==a\n"); }

if (a < b) { print("a<b\n"); }
if (b > a) { print("b>a\n"); }
if (a <= a) { print("a<=a\n"); }
if (a >= b) { print("a>=b\n"); } else { print("a<b2\n"); }
print("done\n");
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "c.x=4 c.y=6\na!=b\na!=b2\na==a\na<b\nb>a\na<=a\na<b2\ndone\n"
    );
}

#[test]
fn op_overload_missing_trait_rejected() {
    // Using `+` on a type that has no `Add` impl is a compile-time error with a
    // hint pointing at the missing trait.
    let src = r#"
struct Plain { x: i64 }
let p = Plain { x: 1 };
let q = Plain { x: 2 };
let r = p + q;
"#;
    let out = run_aero(src);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("Add"));
}

#[test]
fn op_overload_missing_eq_rejected() {
    // Using `==` on a type that has no `Eq` impl is a compile-time error with a
    // hint pointing at the missing trait.
    let src = r#"
struct Plain { x: i64 }
let p = Plain { x: 1 };
let q = Plain { x: 1 };
if (p == q) { print("eq\n"); }
"#;
    let out = run_aero(src);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("Eq"));
}

#[test]
fn dyn_dispatch_end_to_end() {
    // `expr as dyn Trait` boxes a (Copy) concrete value onto the heap and produces a
    // fat pointer `{ data, vtable }`. Method calls on the `dyn` receiver dispatch
    // virtually through the vtable, so two different concrete types sharing one trait
    // resolve to their own impl at runtime. The probe also verifies the source value
    // remains usable afterwards (Copy semantics).
    let src = r#"
#[derive(Copy)]
struct Animal { id: i64 }
#[derive(Copy)]
struct Robot { sn: i64 }
trait Greet {
    fn greet(s: Self, times: i64) -> i64;
    fn tag(s: Self) -> i64;
}
impl Greet for Animal {
    fn greet(s: Animal, times: i64) -> i64 { return s.id * times; }
    fn tag(s: Animal) -> i64 { return s.id + 100; }
}
impl Greet for Robot {
    fn greet(s: Robot, times: i64) -> i64 { return s.sn * times; }
    fn tag(s: Robot) -> i64 { return s.sn + 200; }
}
let a = Animal { id: 7 };
let d = a as dyn Greet;
print("animal greet(3)=%lld tag=%lld\n", d.greet(3), d.tag());
let r = Robot { sn: 5 };
let e = r as dyn Greet;
print("robot greet(4)=%lld tag=%lld\n", e.greet(4), e.tag());
print("a.id=%lld r.sn=%lld\n", a.id, r.sn);
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "animal greet(3)=21 tag=107\nrobot greet(4)=20 tag=205\na.id=7 r.sn=5\n"
    );
}

#[test]
fn dyn_cast_missing_trait_impl_rejected() {
    // Casting a type that does not implement the trait to `dyn Trait` is a
    // compile-time error.
    let src = r#"
struct Box2 { x: i64 }
trait Greet { fn tag(s: Self) -> i64; }
impl Greet for Animal { fn tag(s: Animal) -> i64 { return s.id; } }
struct Animal { id: i64 }
let b = Box2 { x: 1 };
let d = b as dyn Greet;
"#;
    let out = run_aero(src);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("does not implement trait"));
}

#[test]
fn dyn_cast_non_copy_rejected() {
    // Boxing a non-Copy payload is rejected at compile time (a heap copy requires a
    // Copy payload in this phase).
    let src = r#"
struct Person { name: String }
trait Greet { fn tag(s: Self) -> i64; }
impl Greet for Person { fn tag(s: Person) -> i64 { return 0; } }
let p = Person { name: String::from("x") };
let d = p as dyn Greet;
"#;
    let out = run_aero(src);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("Copy"));
}

// ---------- Phase 10: 'a lifetime syntax + reference returns ----------

#[test]
fn lifetime_ref_return_end_to_end() {
    // A function may declare a lifetime `'a` and return a reference derived from
    // a parameter. `get_first<'a>(x: &'a i64) -> &'a i64 { return x; }` copies the
    // parameter reference back; codegen returns the pointer and the caller derefs it.
    let src = r#"
fn get_first<'a>(x: &'a i64) -> &'a i64 {
    return x;
}
let v = 42;
let r = get_first(&v);
print("r=%lld v=%lld\n", *r, v);
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "r=42 v=42\n");
}

#[test]
fn lifetime_ref_local_return_rejected() {
    // Returning a reference to a *local* variable must be rejected as a dangling
    // reference — the borrow checker verifies the returned reference is derived
    // from a parameter.
    let src = r#"
fn bad<'a>(x: &'a i64) -> &'a i64 {
    let v = 99;
    return &v;
}
let a = 1;
let r = bad(&a);
print("%lld\n", *r);
"#;
    let out = run_aero(src);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("cannot return a reference to a local variable"));
}

#[test]
fn lifetime_ref_alias_return_allowed() {
    // A local that aliases a parameter reference retains the parameter origin, so
    // returning it is safe. `let r = x; return r;` copies the parameter reference.
    let src = r#"
fn alias<'a>(x: &'a i64) -> &'a i64 {
    let r = x;
    return r;
}
let v = 7;
let r = alias(&v);
print("r=%lld v=%lld\n", *r, v);
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "r=7 v=7\n");
}

#[test]
fn lifetime_mut_ref_return() {
    // `&mut` references may also be returned when derived from a `&mut` parameter.
    let src = r#"
fn bump<'a>(x: &'a mut i64) -> &'a mut i64 {
    *x = *x + 1;
    return x;
}
let v = 10;
let r = bump(&mut v);
print("r=%lld v=%lld\n", *r, v);
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "r=11 v=11\n");
}

// ---------- Phase 11 simple items: modulo `/` `%` operator ----------

#[test]
fn modulo_integer_operator() {
    let src = r#"
print("%lld\n", 17 % 5);
print("%lld\n", 100 % 7);
print("%lld\n", (10 + 3) % 4);
print("%lld\n", -7 % 3);
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "2\n2\n1\n-1\n");
}

#[test]
fn modulo_float_operator() {
    let src = r#"
print("%f\n", 7.5 % 2.0);
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "1.500000\n");
}

#[test]
fn modulo_precedence_matches_mul_div() {
    // `%` binds as tightly as `*` and `/` (left-associative).
    let src = r#"
print("%lld\n", 100 / 10 % 3);
print("%lld\n", 100 % 30 / 5);
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "1\n2\n");
}

// ---------- Phase 11 simple items: rand() / time() builtins ----------

#[test]
fn builtin_time_returns_epoch_seconds() {
    let src = r#"
let t = time();
print("t>=1700000000=%lld\n", t >= 1700000000);
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "t>=1700000000=1\n");
}

#[test]
fn builtin_rand_returns_varying_values() {
    let src = r#"
let a = rand();
let b = rand();
print("%lld\n", a != b);
let m = a % 1000;
print("mod=%lld\n", m);
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let s = stdout(&out);
    assert!(s.starts_with("1\nmod="), "unexpected output: {s}");
}

// ---------- Phase 1 stdlib: environment variables + path probe ----------

#[test]
fn env_set_get_has_roundtrip() {
    let src = r#"
set_env("AERO_TEST_KEY", "hello env");
print("has=%lld\n", has_env("AERO_TEST_KEY"));
print("val_ok=%lld\n", str_cmp(get_env("AERO_TEST_KEY"), "hello env") == 0);
print("val_len=%lld\n", len(get_env("AERO_TEST_KEY")));
print("unset_has=%lld\n", has_env("AERO_NEVER_DEFINED_XYZ"));
print("unset_val_len=%lld\n", len(get_env("AERO_NEVER_DEFINED_XYZ")));
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "has=1\nval_ok=1\nval_len=9\nunset_has=0\nunset_val_len=0\n");
}

#[test]
fn env_set_env_returns_bool() {
    let src = r#"
let ok = set_env("AERO_TEST_KEY_BOOL", "v");
print("%lld\n", ok);
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "1\n");
}

#[test]
fn file_exists_probes_paths() {
    let src = r#"
print("win=%lld\n", file_exists("C:/Windows/win.ini"));
print("missing=%lld\n", file_exists("C:/definitely/not/a/file.xyz"));
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "win=1\nmissing=0\n");
}

// ---------- Phase 1 stdlib: LinkedList<T> ----------

#[test]
fn linked_list_push_pop_reverse() {
    let src = r#"
let l = linked_list_new();
l.push_back(1);
l.push_back(2);
l.push_back(3);
l.push_front(0);
print("len=%lld\n", l.len());
print("front=%lld\n", l.front().unwrap_or(-1));
print("back=%lld\n", l.back().unwrap_or(-1));
print("get1=%lld\n", l.get(1, -1));
l.reverse();
print("rev_front=%lld\n", l.front().unwrap_or(-1));
print("rev_get1=%lld\n", l.get(1, -1));
print("pop=%lld\n", l.pop_front().unwrap_or(-1));
print("pop=%lld\n", l.pop_back().unwrap_or(-1));
print("len=%lld\n", l.len());
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "len=4\nfront=0\nback=3\nget1=1\nrev_front=3\nrev_get1=2\npop=3\npop=0\nlen=2\n"
    );
}

#[test]
fn linked_list_set_remove_clear() {
    let src = r#"
let l = linked_list_new();
l.push_back(10);
l.push_back(20);
l.push_back(30);
let ok = l.set(1, 99);
print("set_ok=%lld get=%lld\n", ok, l.get(1, -1));
let rm = l.remove(0);
print("rm=%lld len=%lld front=%lld\n", rm, l.len(), l.front().unwrap_or(-1));
l.clear();
print("cleared=%lld\n", l.len());
print("empty=%lld\n", l.is_empty());
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "set_ok=1 get=99\nrm=1 len=2 front=99\ncleared=0\nempty=1\n");
}

// ---------- Phase 1 stdlib: BTreeMap / BTreeSet ----------

#[test]
fn btree_map_insert_get_ordered() {
    let src = r#"
let m = btree_map_new();
m.insert(5, 50);
m.insert(1, 10);
m.insert(3, 30);
m.insert(3, 99);
print("len=%lld\n", m.len());
print("get1=%lld\n", m.get(1, -1));
print("get3=%lld\n", m.get(3, -1));
print("get5=%lld\n", m.get(5, -1));
print("get9=%lld\n", m.get(9, -1));
print("has2=%lld\n", m.contains(2));
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "len=3\nget1=10\nget3=99\nget5=50\nget9=-1\nhas2=0\n");
}

#[test]
fn btree_map_keys_ascending_remove() {
    let src = r#"
let m = btree_map_new();
m.insert(9, 9);
m.insert(2, 2);
m.insert(7, 7);
m.insert(4, 4);
let ks = m.keys();
print("ordered=%lld %lld %lld %lld\n", ks.get(0), ks.get(1), ks.get(2), ks.get(3));
let rm = m.remove(7);
print("rm=%lld len=%lld has7=%lld\n", rm, m.len(), m.contains(7));
let ks2 = m.keys();
print("after=%lld %lld %lld\n", ks2.get(0), ks2.get(1), ks2.get(2));
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "ordered=2 4 7 9\nrm=1 len=3 has7=0\nafter=2 4 9\n");
}

#[test]
fn btree_set_insert_contains() {
    let src = r#"
let s = btree_set_new();
s.insert(42);
s.insert(7);
s.insert(42);
print("len=%lld\n", s.len());
print("has42=%lld\n", s.contains(42));
print("has7=%lld\n", s.contains(7));
print("has1=%lld\n", s.contains(1));
let v = s.to_vec();
print("vec=%lld %lld\n", v.get(0), v.get(1));
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "len=2\nhas42=1\nhas7=1\nhas1=0\nvec=7 42\n");
}

// ---------- Phase 1 stdlib: sort / binary_search / reverse ----------

#[test]
fn sort_quicksort_full_order() {
    let src = r#"
let v = Vec::new();
v.push(5); v.push(2); v.push(9); v.push(1); v.push(7); v.push(3);
sort(v);
print("%lld %lld %lld %lld %lld %lld\n", v.get(0), v.get(1), v.get(2), v.get(3), v.get(4), v.get(5));
print("find1=%lld find7=%lld find9=%lld miss=%lld\n", binary_search(v, 1), binary_search(v, 7), binary_search(v, 9), binary_search(v, 4));
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "1 2 3 5 7 9\nfind1=0 find7=4 find9=5 miss=-1\n");
}

#[test]
fn reverse_in_place() {
    let src = r#"
let v = Vec::new();
v.push(1); v.push(2); v.push(3); v.push(4);
reverse(v);
print("%lld %lld %lld %lld\n", v.get(0), v.get(1), v.get(2), v.get(3));
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "4 3 2 1\n");
}

// ---------- Phase 1 stdlib: higher-order iterator algorithms ----------

#[test]
fn filter_map_reduce_composition() {
    let src = r#"
fn is_even(x: i64) -> bool { return x % 2 == 0; }
fn square(x: i64) -> i64 { return x * x; }
fn add(a: i64, b: i64) -> i64 { return a + b; }
fn mul(a: i64, b: i64) -> i64 { return a * b; }

let v = Vec::new();
v.push(1); v.push(2); v.push(3); v.push(4); v.push(5); v.push(6);
let evens = _filter_impl(v, is_even);
print("evens=%lld %lld %lld len=%lld\n", evens.get(0), evens.get(1), evens.get(2), evens.len());

let v2 = Vec::new();
v2.push(1); v2.push(2); v2.push(3); v2.push(4); v2.push(5); v2.push(6);
let sq = _map_impl(v2, square);
print("sq=%lld %lld %lld %lld %lld %lld\n", sq.get(0), sq.get(1), sq.get(2), sq.get(3), sq.get(4), sq.get(5));

let v3 = Vec::new();
v3.push(1); v3.push(2); v3.push(3); v3.push(4); v3.push(5); v3.push(6);
let sum = _reduce_impl(v3, add, 0);
let v4 = Vec::new();
v4.push(1); v4.push(2); v4.push(3); v4.push(4); v4.push(5); v4.push(6);
let prod = _reduce_impl(v4, mul, 1);
print("sum=%lld product=%lld\n", sum, prod);
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "evens=2 4 6 len=3\nsq=1 4 9 16 25 36\nsum=21 product=720\n"
    );
}

// ---------- Phase 11 simple items: Box<T> smart pointer ----------

#[test]
fn box_scalar_deref_and_free() {
    let src = r#"
let b = Box::new(42);
print("%lld\n", b.deref());
b.free();
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "42\n");
}

#[test]
fn box_struct_deref_fields() {
    let src = r#"
struct Point { x: i64, y: i64 }
let bp = Box::new(Point { x: 3, y: 4 });
let q = bp.deref();
print("%lld %lld\n", q.x, q.y);
bp.free();
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "3 4\n");
}

#[test]
fn box_move_semantics_rejected() {
    // Box is heap-owning (non-Copy): using it after a move must be rejected.
    let src = r#"
let b = Box::new(10);
let c = b;
print("%lld\n", b.deref());
"#;
    let out = run_aero(src);
    assert!(!out.status.success(), "expected move-after-use to be rejected");
    let e = stderr(&out);
    assert!(e.contains("move") || e.contains("moved"), "stderr: {e}");
}

#[test]
fn box_unknown_method_rejected() {
    let src = r#"
let b = Box::new(1);
b.bogus();
"#;
    let out = run_aero(src);
    assert!(!out.status.success(), "expected unknown Box method to be rejected");
    assert!(stderr(&out).contains("Box"), "stderr: {}", stderr(&out));
}

// ---------- 11.1 HashMap / HashSet (stdlib) ----------

#[test]
fn hashmap_insert_get_contains_remove() {
    let src = r#"
let m: HashMap<str> = hash_map_new();
m.insert(1, "one");
m.insert(2, "two");
m.insert(3, "three");
print("%lld %s %s %s\n", m.len(), m.get(1, "?"), m.get(2, "?"), m.get(3, "?"));
print("%s %lld %lld\n", m.get(4, "?"), m.contains(4), m.contains(2));
m.insert(2, "TWO");
print("%s %lld\n", m.get(2, "?"), m.len());
m.remove(2);
print("%lld %lld %s\n", m.contains(2), m.len(), m.get(1, "?"));
m.insert(2, "two-again");
print("%s %lld\n", m.get(2, "?"), m.len());
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "3 one two three\n? 0 1\nTWO 3\n0 2 one\ntwo-again 3\n"
    );
}

#[test]
fn hashmap_growth_rehashes() {
    // 100 entries force several load-factor rehash cycles.
    let src = r#"
let big: HashMap<i64> = hash_map_new();
let i = 0;
while (i < 100) {
    big.insert(i * 17 % 100, i * 3);
    i = i + 1;
}
print("%lld %lld %lld %lld\n", big.len(), big.get(0, -1), big.get(17, -1), big.get(99, -1));
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "100 0 3 141\n");
}

#[test]
fn hashset_insert_contains_remove() {
    let src = r#"
let s: HashSet = hash_set_new();
s.insert(5);
s.insert(10);
s.insert(15);
s.insert(10);
print("%lld %lld %lld %lld\n", s.len(), s.contains(5), s.contains(10), s.contains(7));
s.remove(5);
print("%lld %lld %lld\n", s.len(), s.contains(5), s.contains(15));
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "3 1 1 0\n2 0 1\n");
}

#[test]
fn hashmap_holds_i64_values() {
    let src = r#"
let score: HashMap<i64> = hash_map_new();
score.insert(1, 100);
score.insert(2, 200);
score.insert(3, 300);
score.insert(2, 250);
print("%lld %lld %lld %lld\n", score.len(), score.get(1, -1), score.get(2, -1), score.get(3, -1));
print("%lld\n", score.get(9, -1));
score.remove(2);
print("%lld %lld\n", score.contains(2), score.get(2, -1));
"#;
    let out = run_aero(src);
    // NOTE: struct-valued HashMap (e.g. HashMap<MyType>) currently hits a
    // pre-existing codegen limitation (generic struct + Vec<V> field with an
    // aggregate element type corrupts the heap). Scalar values (i64/f64/str/bool)
    // are fully supported; the aggregate limitation is tracked in 难度任务.txt.
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "3 100 250 300\n-1\n0 -1\n");
}

// ---------- P0-2 union type ----------

#[test]
fn union_literal_and_field_access() {
    // A union literal sets exactly one field; all fields share storage and the
    // union's size is that of the largest field. Reading back the set field.
    let src = r#"
union Value { i: i64, f: f64, c: i64 }
let u = Value { i: 42 };
print("%lld\n", u.i);
let v = Value { f: 2.5 };
print("%f\n", v.f);
let w = Value { c: 7 };
print("%lld\n", w.c);
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "42\n2.500000\n7\n");
}

#[test]
fn union_fields_share_storage() {
    // Writing a large field then reading it as a smaller overlapping field
    // observes the shared bytes (union semantics), not a separate slot.
    let src = r#"
union U { a: i64, b: i64 }
let u = U { a: 100 };
print("%lld\n", u.b);
u.b = 200;
print("%lld\n", u.a);
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "100\n200\n");
}

#[test]
fn union_is_copy() {
    // Unions are POD (all fields bitwise-copyable), so a union value is Copy
    // even without `impl Copy` — the second `let` does not move.
    let src = r#"
union P { x: i64, y: f64 }
let p = P { x: 5 };
let q = p;
let r = p;
print("%lld %lld\n", q.x, r.x);
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "5 5\n");
}

#[test]
fn union_exactly_one_field_enforced() {
    // A union literal must set exactly one field.
    let src = r#"
union V { a: i64, b: i64 }
let v = V { a: 1, b: 2 };
"#;
    let out = run_aero(src);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("exactly one field"));
}

#[test]
fn union_unknown_field_rejected() {
    let src = r#"
union V { a: i64 }
let v = V { nope: 1 };
"#;
    let out = run_aero(src);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("no field"));
}

// ---------- P0-3 top-level const ----------

#[test]
fn top_level_const_scalar() {
    // A top-level const is evaluated at compile time and filled into references.
    let src = r#"
const answer: i64 = 42;
const pi: f64 = 3.14;
const flag: bool = true;
print("%lld\n", answer);
print("%f\n", pi);
print("%lld\n", flag);
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "42\n3.140000\n1\n");
}

#[test]
fn top_level_const_type_inferred() {
    // Without an annotation, the const's type is inferred from the value.
    let src = r#"
const n = 7 + 3;
const f = 2.5 * 2.0;
print("%lld\n", n);
print("%f\n", f);
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "10\n5.000000\n");
}

#[test]
fn top_level_const_references_other_const() {
    // const-to-const reference resolves at compile time and can chain.
    let src = r#"
const base: i64 = 100;
const doubled: i64 = base * 2;
const quadrupled: i64 = doubled * 2;
print("%lld\n", quadrupled);
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "400\n");
}

#[test]
fn top_level_const_calls_const_fn() {
    // A const may initialize from a const fn call, folded at compile time.
    let src = r#"
const fn triple(x: i64) -> i64 { return x * 3; }
const value: i64 = triple(5);
print("%lld\n", value);
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "15\n");
}

#[test]
fn top_level_const_used_in_let() {
    // Consts can initialize local variables and participate in runtime logic.
    let src = r#"
const LIMIT: i64 = 10;
let i = 0;
while (i < LIMIT) {
    i = i + 1;
}
print("%lld\n", i);
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "10\n");
}

#[test]
fn top_level_const_type_mismatch_rejected() {
    let src = r#"
const x: i64 = 3.14;
"#;
    let out = run_aero(src);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("const `x`"));
}

#[test]
fn top_level_const_duplicate_rejected() {
    let src = r#"
const x: i64 = 1;
const x: i64 = 2;
"#;
    let out = run_aero(src);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("duplicate definition of const"));
}

// ---------- 12.1 minimal diagnostics LSP ----------

/// Frame a JSON-RPC message as a Content-Length body.
fn frame(msg: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", msg.len()).as_bytes());
    out.extend_from_slice(msg.as_bytes());
    out
}

/// Run `aero lsp`, feed it framed messages on stdin, and return its raw stdout.
fn run_lsp(msgs: &[&str]) -> String {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};

    let mut child = Command::new(env!("CARGO_BIN_EXE_aero"))
        .args(["lsp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let mut input = child.stdin.take().unwrap();
    for m in msgs {
        let _ = input.write_all(&frame(m));
    }
    drop(input);

    let mut out = String::new();
    child.stdout.take().unwrap().read_to_string(&mut out).unwrap();
    let _ = child.wait();
    out
}

#[test]
fn lsp_publishes_diagnostics_on_open_and_change() {
    // Open a document with a syntax error, then fix it via didChange. The server
    // must publish a diagnostic pointing at the error, then clear it on the change.
    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    let open = r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///t.aero","languageId":"aero","version":1,"text":"let x = 1 + ;"}}}"#;
    let change = r#"{"jsonrpc":"2.0","method":"textDocument/didChange","params":{"textDocument":{"uri":"file:///t.aero","version":2},"contentChanges":[{"text":"print(1 + 2);"}]}}"#;
    let exit = r#"{"jsonrpc":"2.0","method":"exit","params":{}}"#;

    let raw = run_lsp(&[init, open, change, exit]);

    // initialize response
    assert!(raw.contains("\"id\":1") && raw.contains("\"capabilities\""), "raw: {raw}");
    // didOpen publishes a diagnostic for `let x = 1 + ;` at line 0
    assert!(raw.contains("textDocument/publishDiagnostics"), "raw: {raw}");
    assert!(raw.contains("\"severity\":1"), "raw: {raw}");
    assert!(raw.contains("expected an expression"), "raw: {raw}");
    // After didChange the buffer is valid -> an empty-diagnostics publish clears it.
    assert!(raw.contains("\"diagnostics\":[]"), "raw: {raw}");
}

#[test]
fn lsp_ignores_unknown_method_but_keeps_running() {
    // A request for an unsupported method must not crash the server; it should
    // respond with method-not-found and still handle the next didOpen.
    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    let unknown = r#"{"jsonrpc":"2.0","id":2,"method":"textDocument/rename","params":{}}"#;
    let open = r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///t.aero","languageId":"aero","version":1,"text":"let z = ;"}}}"#;
    let exit = r#"{"jsonrpc":"2.0","method":"exit","params":{}}"#;

    let raw = run_lsp(&[init, unknown, open, exit]);

    assert!(raw.contains("method not found"), "raw: {raw}");
    assert!(raw.contains("publishDiagnostics"), "raw: {raw}");
}

// ---------- LSP semantic features: completion / hover / definition ----------

#[test]
fn lsp_completion_returns_symbols_and_keywords() {
    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    let open = r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///t.aero","languageId":"aero","version":1,"text":"fn greet(name: str) -> str { return name; }\nlet side = 1;"}}}"#;
    // Ask for completion at the start of line 0 (empty prefix => all items).
    let complete = r#"{"jsonrpc":"2.0","id":2,"method":"textDocument/completion","params":{"textDocument":{"uri":"file:///t.aero"},"position":{"line":0,"character":0}}}"#;
    let exit = r#"{"jsonrpc":"2.0","method":"exit","params":{}}"#;

    let raw = run_lsp(&[init, open, complete, exit]);

    assert!(raw.contains("\"id\":2"), "raw: {raw}");
    assert!(raw.contains("greet"), "raw: {raw}"); // user function completion
    assert!(raw.contains("print"), "raw: {raw}"); // std builtin completion
    assert!(raw.contains("\"kind\":3"), "raw: {raw}"); // function kind = 3
}

#[test]
fn lsp_hover_returns_signature_markdown() {
    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    let open = r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///t.aero","languageId":"aero","version":1,"text":"fn add(a: i64, b: i64) -> i64 { return a + b; }\nprint(add(1, 2));"}}}"#;
    // Hover over `add` in the call on line 1 (offset of 'a' in add is col 6).
    let hover = r#"{"jsonrpc":"2.0","id":2,"method":"textDocument/hover","params":{"textDocument":{"uri":"file:///t.aero"},"position":{"line":1,"character":6}}}"#;
    let exit = r#"{"jsonrpc":"2.0","method":"exit","params":{}}"#;

    let raw = run_lsp(&[init, open, hover, exit]);

    assert!(raw.contains("\"id\":2"), "raw: {raw}"); // response
    assert!(raw.contains("fn add(a: i64, b: i64) -> i64"), "raw: {raw}");
    assert!(raw.contains("markdown"), "raw: {raw}");
}

#[test]
fn lsp_definition_returns_declaration_location() {
    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    let open = r#"{"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":"file:///t.aero","languageId":"aero","version":1,"text":"let pi = 3.14;\nprint(pi);"}}}"#;
    // Go-to-definition on `pi` in line 1 col 7 -> declaration is line 0 col 4.
    let def = r#"{"jsonrpc":"2.0","id":2,"method":"textDocument/definition","params":{"textDocument":{"uri":"file:///t.aero"},"position":{"line":1,"character":7}}}"#;
    let exit = r#"{"jsonrpc":"2.0","method":"exit","params":{}}"#;

    let raw = run_lsp(&[init, open, def, exit]);

    assert!(raw.contains("\"id\":2"), "raw: {raw}");
    // Decl location: let pi -> col 4 (i starts at 0-based 4); line 0.
    assert!(raw.contains("\"uri\":\"file:///t.aero\""), "raw: {raw}");
    assert!(raw.contains("\"line\":0"), "raw: {raw}");
    assert!(raw.contains("\"character\":4"), "raw: {raw}");
}

// ---------- 12.6 const fn compile-time evaluation ----------

#[test]
fn const_fn_folds_arithmetic_recursion_and_loop() {
    // add / factorial(fact) / fib are all const fns; called with constant
    // arguments they must be folded at compile time and produce the same result
    // as the runtime equivalent.
    let src = r#"
const fn add(a: i64, b: i64) -> i64 { return a + b; }
const fn fact(n: i64) -> i64 { if (n <= 1) { return 1; } return n * fact(n - 1); }
const fn fib(n: i64) -> i64 {
    let a = 0; let b = 1; let i = 0;
    while (i < n) { let t = a + b; a = b; b = t; i = i + 1; }
    return a;
}
print("%lld\n", add(2, 3));
print("%lld\n", fact(5));
print("%lld\n", fib(10));
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "5\n120\n55\n");
}

#[test]
fn const_fn_bool_and_float_results() {
    let src = r#"
const fn is_even(n: i64) -> bool { return n % 2 == 0; }
const fn half(x: f64) -> f64 { return x / 2.0; }
if (is_even(7)) { print("EVEN\n"); } else { print("ODD\n"); }
if (is_even(8)) { print("EVEN\n"); } else { print("ODD\n"); }
print("%f\n", half(9.0));
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "ODD\nEVEN\n4.500000\n");
}

#[test]
fn const_fn_falls_back_to_runtime_on_non_constant_args() {
    // A const fn called with a non-constant argument must not be mis-folded; it
    // falls back to a normal runtime call and still produces the right value.
    let src = r#"
const fn double(n: i64) -> i64 { return n * 2; }
let x = 21;
print("%lld\n", double(x));
print("%lld\n", double(100));
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "42\n200\n");
}

#[test]
fn non_const_call_is_not_folded() {
    // A regular (non-const) fn must always be emitted as a runtime call; calling
    // it with constants must not be treated as a const fold (and still runs).
    let src = r#"
fn plus_one(n: i64) -> i64 { return n + 1; }
print("%lld\n", plus_one(41));
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "42\n");
}

// ---------- P0-1 loop keyword ----------

#[test]
fn loop_with_break_and_continue() {
    let src = r#"
let i = 0;
loop {
    if (i >= 5) { break; }
    print("%lld", i);
    i = i + 1;
}
print("\n");
let j = 0;
let sum = 0;
loop {
    j = j + 1;
    if (j > 10) { break; }
    if (j % 2 == 0) { continue; }
    sum = sum + j;
}
print("%lld\n", sum);
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    // 0..5 via break; sum of odd 1..10 = 25 (continue skips evens)
    assert_eq!(stdout(&out), "01234\n25\n");
}

#[test]
fn loop_inside_function_with_break() {
    let src = r#"
fn count_to(n: i64) -> i64 {
    let c = 0;
    loop {
        if (c >= n) { break; }
        c = c + 1;
    }
    return c;
}
print("%lld\n", count_to(7));
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "7\n");
}

// ---------- Module system (P0-4) ----------

#[test]
fn module_inline_same_module_call() {
    // mod { ... } inline: a function in the module can call another in the same module,
    // and top-level code can call the module's function via its qualified name.
    let src = r#"
mod math {
    fn double(x: i64) -> i64 {
        return x * 2;
    }
    fn plus_one(x: i64) -> i64 {
        return double(x) + 1;
    }
}
print("%lld\n", math::plus_one(10));
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "21\n");
}

#[test]
fn module_nested_and_qualified_call() {
    // Nested modules flatten to `outer::inner::name`.
    let src = r#"
mod outer {
    mod inner {
        fn greet() -> i64 {
            return 99;
        }
    }
}
print("%lld\n", outer::inner::greet());
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "99\n");
}

#[test]
fn module_use_import_brings_names() {
    // `use math::double;` brings `double` into the current scope so it can be called bare.
    let src = r#"
mod math {
    fn double(x: i64) -> i64 {
        return x * 2;
    }
}
use math::double;
print("%lld\n", double(5));
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "10\n");
}

#[test]
fn module_pub_exposes_otherwise_private() {
    // Without `pub`, definitions are module-private; `pub` makes them reachable outside.
    let src = r#"
mod api {
    pub fn visible() -> i64 {
        return 42;
    }
    fn hidden() -> i64 {
        return 1;
    }
}
print("%lld\n", api::visible());
"#;
    let out = run_aero(src);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(stdout(&out), "42\n");
}

// ---------- Optimization levels (compiler) ----------

#[test]
fn build_accepts_opt_level_flags() {
    // `aero run <file> -O0/-O1/-O2/-O3` must compile, run, and print the same result
    // regardless of the optimization level.
    let src = r#"
fn fib(n: i64) -> i64 {
    if (n < 2) { return n; }
    return fib(n - 1) + fib(n - 2);
}
print("%lld\n", fib(10));
"#;
    let path = std::env::temp_dir().join(format!("aero_opt_{}.aero", std::process::id()));
    std::fs::write(&path, src).unwrap();
    for flag in ["-O0", "-O1", "-O2", "-O3"] {
        let out = Command::new(env!("CARGO_BIN_EXE_aero"))
            .args(["run", path.to_str().unwrap(), flag])
            .output()
            .unwrap();
        assert!(out.status.success(), "flag {flag} failed: {}", stderr(&out));
        assert_eq!(stdout(&out), "55\n", "wrong output for {flag}");
    }
    let _ = std::fs::remove_file(&path);
}

