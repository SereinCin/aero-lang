pub mod aot;
pub mod codegen;
pub mod const_eval;
pub mod jit;

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

/// Full compilation pipeline: source -> lex -> parse -> HIR (name resolution + type
/// checking) -> borrow check -> LLVM IR. Shared by JIT ([`run_source`]) and AOT
///
/// The standard library prelude ([`aero_std::std_tokens`]) is injected ahead of the
/// user source (pre-compiled/cached token stream), so `Option`/`Result` are always
/// in scope. Std tokens keep their own file's line numbers and user tokens keep
/// the user file's, so diagnostics point at the right source either way.
pub fn compile_pipeline<'ctx>(
    context: &'ctx Context,
    source: &str,
) -> Result<Module<'ctx>, AeroError> {
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
    let source = source.to_owned();
    std::thread::Builder::new()
        .stack_size(JIT_STACK_SIZE)
        .spawn(move || run_jit_pipeline(&source, opt))
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

/// Compile and run `source` in the current thread (Context/Module/Engine all
/// live here so nothing crosses the thread boundary). Used by the big-stack
/// worker thread in [`run_source_opt`].
fn run_jit_pipeline(source: &str, opt: aot::OptLevel) -> Result<(), AeroError> {
    let context = Context::create();
    let module = compile_pipeline(&context, source)?;
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

/// Compile-check only (lex -> parse -> HIR -> types -> borrows -> LLVM IR + verify),
/// without executing. Used by the package manager's `aero build`.
pub fn check_source(source: &str) -> Result<(), AeroError> {
    let context = Context::create();
    let _module = compile_pipeline(&context, source)?;
    Ok(())
}
