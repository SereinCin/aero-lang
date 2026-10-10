pub mod aot;
pub mod codegen;
pub mod const_eval;
pub mod cpp;
pub mod jit;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use inkwell::context::Context;
use inkwell::module::Module;

pub use codegen::{compile, CodegenError};
pub use jit::run_jit;

/// Stack reserve (bytes) for the worker thread that JIT-executes `main`. Allows
/// deep recursion without overflowing the default ~1MB thread stack.
const JIT_STACK_SIZE: usize = 64 * 1024 * 1024;

/// A complete compile/execution error with phase information.
#[derive(Debug)]
pub struct AeroError {
    pub phase: &'static str,
    pub line: u32,
    pub col: u32,
    pub msg: String,
}

impl std::fmt::Display for AeroError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.line > 0 {
            write!(
                f,
                "line {} col {} [{}] {}",
                self.line, self.col, self.phase, self.msg
            )
        } else {
            write!(f, "[{}] {}", self.phase, self.msg)
        }
    }
}

/// Resolve a `mod foo;` file-module path relative to `base_dir`.
/// Checks `<base_dir>/foo.aero` first, then `<base_dir>/foo/mod.aero`.
fn resolve_mod_path(base_dir: &Path, name: &str, span: aero_parse::Span) -> Result<PathBuf, AeroError> {
    // path join safety: forbid `..` / absolute paths in mod names
    if name.starts_with('/') || name.starts_with('\\') || name.contains("..") {
        return Err(AeroError {
            phase: "module resolution",
            line: span.line,
            col: span.col,
            msg: format!("invalid module name `{name}`: must be a simple identifier, no `..` or absolute paths"),
        });
    }
    let p1 = base_dir.join(format!("{name}.aero"));
    if p1.is_file() {
        return Ok(p1);
    }
    let p2 = base_dir.join(name).join("mod.aero");
    if p2.is_file() {
        return Ok(p2);
    }
    Err(AeroError {
        phase: "module resolution",
        line: span.line,
        col: span.col,
        msg: format!(
            "file not found for `mod {name};`: tried `{}` and `{}`",
            p1.display(),
            p2.display()
        ),
    })
}

/// Recursively expand `Stmt::ModFile { name, .. }` nodes in place. Each ModFile
/// is replaced by the parsed statements from the referenced file (the entire
/// file's AST is spliced in at the point of the `mod` declaration).
///
/// - `source_dir`: working directory for resolution. `None` → any ModFile is an
///   error (no multi-file without a dir hint).
/// - `visited`: absolute paths already loaded in this pipeline run. Used to
///   prevent the same file from being expanded twice (idempotency).
fn expand_mod_files(
    stmts: &mut Vec<aero_parse::ast::Stmt>,
    source_dir: Option<&Path>,
    visited: &mut HashSet<PathBuf>,
) -> Result<(), AeroError> {
    // Walk the list; at each index we may splice (replace ModFile with file's stmts).
    let mut i = 0usize;
    while i < stmts.len() {
        // First, recurse into this single stmt's nested Vec<Stmt> fields
        // (ModDef.items, ImplBlock.methods, Pub(inner: Stmt), FnDef.body, etc.)
        expand_one_stmt(&mut stmts[i], source_dir, visited)?;

        if matches!(stmts[i], aero_parse::ast::Stmt::ModFile { .. }) {
            let stolen = stmts.remove(i);
            let (name, span) = match stolen {
                aero_parse::ast::Stmt::ModFile { name, span } => (name, span),
                _ => unreachable!(),
            };
            let dir = match source_dir {
                Some(d) => d,
                None => {
                    return Err(AeroError {
                        phase: "module resolution",
                        line: span.line,
                        col: span.col,
                        msg: format!(
                            "`mod {name};` requires a source directory hint — run via `aero run <file>` instead"
                        ),
                    });
                }
            };
            let resolved = resolve_mod_path(dir, &name, span)?;
            let abs = match std::fs::canonicalize(&resolved) {
                Ok(p) => p,
                Err(_) => resolved, // fall back to non-canonicalised path
            };
            if visited.contains(&abs) {
                continue; // idempotent skip
            }
            visited.insert(abs.clone());
            let contents = match std::fs::read_to_string(&abs) {
                Ok(c) => c,
                Err(e) => {
                    return Err(AeroError {
                        phase: "module resolution",
                        line: span.line,
                        col: span.col,
                        msg: format!("cannot read mod file `{}`: {e}", abs.display()),
                    });
                }
            };
            let file_tokens = aero_lex::lex(&contents).map_err(|e| AeroError {
                phase: "lexing",
                line: e.line,
                col: e.col,
                msg: format!("while loading mod `{name}`: {}", e.msg),
            })?;
            let file_program = aero_parse::parse(&file_tokens).map_err(|e| AeroError {
                phase: "parsing",
                line: e.line,
                col: e.col,
                msg: format!("while loading mod `{name}`: {}", e.msg),
            })?;
            let file_dir = abs.parent();
            let mut nested_stmts = file_program.stmts;
            // Recursively expand nested ModFile from this file's own directory.
            if let Some(fd) = file_dir {
                expand_mod_files(&mut nested_stmts, Some(fd), visited)?;
            }
            // Wrap into an inline ModDef so `flatten_modules` mangles names as
            // `{name}::{item}` — user code calls `mod_name.func()` for free.
            let wrapped = aero_parse::ast::Stmt::ModDef {
                name: name.clone(),
                items: nested_stmts,
                span,
            };
            // Replace the ModFile node with the wrapped ModDef.
            stmts.insert(i, wrapped);
        }
        i = i + 1;
    }
    Ok(())
}

/// Recurse into a single Stmt's nested Vec<Stmt> fields (ModDef.items,
/// ImplBlock.methods, FnDef.body, If branches, etc.) to find and expand any
/// ModFile declarations that might appear inside them.
fn expand_one_stmt(
    stmt: &mut aero_parse::ast::Stmt,
    source_dir: Option<&Path>,
    visited: &mut HashSet<PathBuf>,
) -> Result<(), AeroError> {
    use aero_parse::ast::Stmt;
    match stmt {
        Stmt::ModDef { items, .. } => {
            expand_mod_files(items, source_dir, visited)?;
        }
        Stmt::Pub(inner, _) => {
            expand_one_stmt(inner.as_mut(), source_dir, visited)?;
        }
        Stmt::FnDef { body, .. } => {
            expand_mod_files(body, source_dir, visited)?;
        }
        Stmt::If { then_body, else_body, .. } => {
            expand_mod_files(then_body, source_dir, visited)?;
            expand_mod_files(else_body, source_dir, visited)?;
        }
        Stmt::While { body, .. } | Stmt::Loop { body, .. } | Stmt::For { body, .. } => {
            expand_mod_files(body, source_dir, visited)?;
        }
        Stmt::Match { arms, .. } => {
            for arm in arms.iter_mut() {
                expand_mod_files(&mut arm.body, source_dir, visited)?;
            }
        }
        Stmt::ImplBlock { methods, .. } => {
            expand_mod_files(methods, source_dir, visited)?;
        }
        _ => {}
    }
    Ok(())
}

/// Full compilation pipeline: source -> lex -> parse -> [mod expansion] -> HIR
/// (name resolution + type checking) -> borrow check -> LLVM IR. Shared by JIT
/// ([`run_source`]) and AOT builders.
///
/// When `source_dir` is provided, `mod foo;` file-modules are resolved relative
/// to that directory (finds `foo.aero` or `foo/mod.aero`). When `None`, `ModFile`
/// nodes in the AST trigger an error — use [`compile_pipeline_from_dir`] instead.
///
/// The standard library prelude ([`aero_std::std_tokens`]) is injected ahead of the
/// user source, so `Option`/`Result` are always in scope.
pub fn compile_pipeline<'ctx>(
    context: &'ctx Context,
    source: &str,
) -> Result<Module<'ctx>, AeroError> {
    compile_pipeline_emit(context, source, None, true, None, false)
}

/// [`compile_pipeline`] with an explicit working directory for `mod foo;` resolution.
pub fn compile_pipeline_from_dir<'ctx>(
    context: &'ctx Context,
    source: &str,
    source_dir: &Path,
) -> Result<Module<'ctx>, AeroError> {
    compile_pipeline_emit(context, source, Some(source_dir), true, None, false)
}

/// Python-extension build spec (`aero build --pyext`): the module name plus the
/// `PYTHON_API_VERSION` passed to `PyModule_Create2` (e.g. 1013 for CPython
/// 3.13). The version must match the interpreter the extension links against.
pub struct PyExtSpec<'a> {
    pub module: &'a str,
    pub api_version: u32,
    /// Windows COFF target: DLL-imported data (e.g. `_Py_NoneStruct`) must be
    /// accessed through the `__imp_` indirection slot (C `dllimport` semantics);
    /// ELF/Mach-O use the plain symbol.
    pub windows: bool,
}

/// [`compile_pipeline`] with an explicit `emit_main` flag. `emit_main=false`
/// builds a shared-library module: the top-level `main` is kept but hidden from
/// the dynamic symbol table (used by `aero build --shared` / Python extensions).
/// `py_ext=Some(spec)` additionally emits the CPython glue for every
/// `#[py_export]` function (`aero build --pyext`).
pub(crate) fn compile_pipeline_emit<'ctx>(
    context: &'ctx Context,
    source: &str,
    source_dir: Option<&Path>,
    emit_main: bool,
    py_ext: Option<&PyExtSpec>,
    freestanding: bool,
) -> Result<Module<'ctx>, AeroError> {
    let mut tokens = aero_std::std_tokens().to_vec();
    let user_tokens = aero_lex::lex(source).map_err(|e| AeroError {
        phase: "lexing",
        line: e.line,
        col: e.col,
        msg: e.msg,
    })?;
    tokens.extend(user_tokens);
    let mut program = aero_parse::parse(&tokens).map_err(|e| AeroError {
        phase: "parsing",
        line: e.line,
        col: e.col,
        msg: e.msg,
    })?;
    // Expand `mod foo;` file-modules. Skips silently when no source_dir is set
    // and the program happens to contain none; errors when it contains ModFile
    // but source_dir is None.
    let mut visited: HashSet<PathBuf> = HashSet::new();
    expand_mod_files(&mut program.stmts, source_dir, &mut visited)?;
    let (hir, result) = aero_hir::lower_and_check(&program).map_err(|e| AeroError {
        phase: e.phase(),
        line: e.line(),
        col: e.col(),
        msg: e.msg().to_string(),
    })?;
    let moved_by_scope = aero_hir::check_borrows(&hir, &result.var_tys).map_err(|e| AeroError {
        phase: "borrow checking",
        line: e.line,
        col: e.col,
        msg: e.msg,
    })?;
    codegen::compile(
        context,
        &hir,
        &result.var_tys,
        &moved_by_scope,
        &result.instances,
        &result.call_types,
        &result.struct_lit_types,
        &result.enum_lit_types,
        emit_main,
        py_ext,
        freestanding,
    )
    .map_err(|e| AeroError {
        phase: "codegen",
        line: e.line,
        col: e.col,
        msg: e.msg,
    })
}

/// One-stop: source -> lex -> parse -> HIR -> borrow check -> LLVM IR -> JIT execution.
pub fn run_source(source: &str) -> Result<(), AeroError> {
    run_source_opt(source, aot::OptLevel::default())
}

/// [`run_source`] with an explicit JIT optimization level.
///
/// Execution happens on a dedicated thread with a large stack (64MB) so that
/// deeply recursive programs don't overflow the default ~1MB thread stack and
/// hard-crash the process.
pub fn run_source_opt(source: &str, opt: aot::OptLevel) -> Result<(), AeroError> {
    run_source_opt_from_dir(source, opt, None)
}

/// [`run_source_opt`] with a working directory hint for `mod foo;` resolution.
/// When `source_dir` is `None`, any `mod foo;` declarations in the source will
/// produce a compile-time error.
pub fn run_source_opt_from_dir(
    source: &str,
    opt: aot::OptLevel,
    source_dir: Option<&Path>,
) -> Result<(), AeroError> {
    let source_owned = source.to_owned();
    let dir_owned = source_dir.map(|p| p.to_path_buf());
    std::thread::Builder::new()
        .stack_size(JIT_STACK_SIZE)
        .spawn(move || run_jit_pipeline_with_dir(&source_owned, opt, dir_owned.as_deref()))
        .map_err(|e| AeroError {
            phase: "execution",
            line: 0,
            col: 0,
            msg: format!("failed to spawn JIT execution thread: {e}"),
        })?
        .join()
        .map_err(|_| AeroError {
            phase: "execution",
            line: 0,
            col: 0,
            msg: "JIT execution thread panicked".to_string(),
        })?
}

/// Convenience: run a file from disk with automatic `mod` resolution relative
/// to the file's containing directory.
pub fn run_file(path: &str, opt: aot::OptLevel) -> Result<(), AeroError> {
    let source = std::fs::read_to_string(path).map_err(|e| AeroError {
        phase: "IO",
        line: 0,
        col: 0,
        msg: format!("cannot read {path}: {e}"),
    })?;
    let dir = Path::new(path).parent();
    run_source_opt_from_dir(&source, opt, dir)
}

/// Compile and run `source` in the current thread (Context/Module/Engine all
/// live here so nothing crosses the thread boundary). Used by the big-stack
/// worker thread in [`run_source_opt_from_dir`].
fn run_jit_pipeline_with_dir(source: &str, opt: aot::OptLevel, source_dir: Option<&Path>) -> Result<(), AeroError> {
    let context = Context::create();
    let module = match source_dir {
        Some(d) => compile_pipeline_from_dir(&context, source, d)?,
        None => compile_pipeline(&context, source)?,
    };
    if std::env::var("AERO_DUMP_IR").is_ok() {
        println!("{}", module.print_to_string());
    }
    jit::run_jit(&module, opt).map_err(|msg| AeroError {
        phase: "execution",
        line: 0,
        col: 0,
        msg,
    })
}

/// Compile and run `source` without a `mod` resolution hint (backward-compat).
fn run_jit_pipeline(source: &str, opt: aot::OptLevel) -> Result<(), AeroError> {
    run_jit_pipeline_with_dir(source, opt, None)
}

/// Compile-check only (lex -> parse -> HIR -> types -> borrows -> LLVM IR + verify),
/// without executing. Used by the package manager's `aero build`.
pub fn check_source(source: &str) -> Result<(), AeroError> {
    let context = Context::create();
    let _module = compile_pipeline(&context, source)?;
    Ok(())
}
