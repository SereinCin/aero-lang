pub mod aot;
pub mod codegen;
pub mod jit;

use inkwell::context::Context;
use inkwell::module::Module;

pub use codegen::{compile, CodegenError};
pub use jit::run_jit;

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
pub fn compile_pipeline<'ctx>(
    context: &'ctx Context,
    source: &str,
) -> Result<Module<'ctx>, AeroError> {
    let tokens = aero_lex::lex(source).map_err(|e| AeroError {
        phase: "lexing",
        line: e.line,
        col: e.col,
        msg: e.msg,
    })?;
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
    aero_hir::check_borrows(&hir).map_err(|e| AeroError {
        phase: "borrow checking",
        line: e.line,
        col: e.col,
        msg: e.msg,
    })?;
    codegen::compile(context, &hir, &result.var_tys, &result.instances, &result.call_types).map_err(
        |e| AeroError {
            phase: "codegen",
            line: e.line,
            col: e.col,
            msg: e.msg,
        },
    )
}

/// One-stop: source -> lex -> parse -> HIR -> borrow check -> LLVM IR -> JIT execution.
pub fn run_source(source: &str) -> Result<(), AeroError> {
    let context = Context::create();
    let module = compile_pipeline(&context, source)?;
    if std::env::var("AERO_DUMP_IR").is_ok() {
        println!("{}", module.print_to_string());
    }
    jit::run_jit(&module).map_err(|msg| AeroError {
        phase: "execution",
        line: 0,
        col: 0,
        msg,
    })?;
    Ok(())
}

/// Compile-check only (lex -> parse -> HIR -> types -> borrows -> LLVM IR + verify),
/// without executing. Used by the package manager's `aero build`.
pub fn check_source(source: &str) -> Result<(), AeroError> {
    let context = Context::create();
    let _module = compile_pipeline(&context, source)?;
    Ok(())
}
