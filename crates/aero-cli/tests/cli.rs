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
fn extern_c_body_rejected() {
    // extern "C" declarations cannot have a body (semicolon expected)
    let out = run_aero("extern \"C\" fn bad() -> i64 { return 1; }\nprint(1);");
    assert!(!out.status.success());
}
