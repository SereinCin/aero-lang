use inkwell::module::Module;
use inkwell::OptimizationLevel;

/// Compile and execute the module's `main` in memory via MCJIT.
pub fn run_jit(module: &Module) -> Result<(), String> {
    let engine = module
        .create_jit_execution_engine(OptimizationLevel::Aggressive)
        .map_err(|e| format!("failed to create JIT execution engine: {e}"))?;

    unsafe {
        // main now has the standard C signature `main(argc, argv)` (M1.2 CLI
        // arguments). JIT runs without arguments: argc = 0, argv = null.
        type MainFn = unsafe extern "C" fn(i32, *const *const std::os::raw::c_char) -> i64;
        let main = engine
            .get_function::<MainFn>("main")
            .map_err(|e| format!("failed to get main function: {e}"))?;
        main.call(0, std::ptr::null());
    }
    Ok(())
}
