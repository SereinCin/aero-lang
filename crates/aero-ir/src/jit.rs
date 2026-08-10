use inkwell::module::Module;
use inkwell::OptimizationLevel;

/// Compile and execute the module's `main` in memory via MCJIT.
pub fn run_jit(module: &Module) -> Result<(), String> {
    let engine = module
        .create_jit_execution_engine(OptimizationLevel::Aggressive)
        .map_err(|e| format!("failed to create JIT execution engine: {e}"))?;

    unsafe {
        let main = engine
            .get_function::<unsafe extern "C" fn() -> i64>("main")
            .map_err(|e| format!("failed to get main function: {e}"))?;
        main.call();
    }
    Ok(())
}
