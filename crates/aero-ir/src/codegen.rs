use std::collections::HashMap;

use aero_hir::hir::{DefId, HirBlock, HirExpr, HirFn, HirProgram, HirStmt};
use aero_hir::infer::{substitute, GenericInstance};
use aero_hir::ty::Ty;

/// Cap on generic instantiations (prevents unbounded monomorphization).
const MAX_GENERIC_INSTANCES: usize = 128;

/// LLVM function name of a generic instance: `max$i64`, `max$bool`; multiple
/// type args are separated by `;`. Unique symbol per instance in the module.
fn mono_name(fn_name: &str, type_args: &[Ty]) -> String {
    let args = type_args
        .iter()
        .map(|t| t.to_string())
        .collect::<Vec<_>>()
        .join(";");
    format!("{fn_name}${args}")
}
use aero_parse::ast::{BinOp, CmpOp, LogicOp, UnOp};
use aero_parse::span::Span;
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::Module;
use inkwell::types::{BasicType, BasicTypeEnum, IntType};
use inkwell::values::{
    BasicMetadataValueEnum, BasicValue, BasicValueEnum, FunctionValue, IntValue, PointerValue,
};
use inkwell::{AddressSpace, IntPredicate};

/// Codegen error (with line/column).
#[derive(Debug, Clone, PartialEq)]
pub struct CodegenError {
    pub msg: String,
    pub line: u32,
    pub col: u32,
}

/// Wrap an inkwell BuilderError as a CodegenError.
fn bld<T>(r: Result<T, inkwell::builder::BuilderError>) -> Result<T, CodegenError> {
    r.map_err(|e| CodegenError {
        msg: format!("LLVM IR construction failed: {e}"),
        line: 0,
        col: 0,
    })
}

/// Codegen result: a scalar value or the stack-slot pointer of an aggregate
/// (array/tuple).
enum GenValue<'ctx> {
    /// Scalar (i1/i32/i64 or i8* string pointer)
    Scalar(BasicValueEnum<'ctx>),
    /// Memory slot holding an aggregate (array/tuple)
    Agg(PointerValue<'ctx>),
}

impl<'ctx> GenValue<'ctx> {
    fn scalar(self, span: Span, what: &str) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        match self {
            GenValue::Scalar(v) => Ok(v),
            GenValue::Agg(_) => Err(CodegenError {
                msg: format!("{what} requires a scalar value, got an array/tuple"),
                line: span.line,
                col: span.col,
            }),
        }
    }

    fn agg(self, span: Span, what: &str) -> Result<PointerValue<'ctx>, CodegenError> {
        match self {
            GenValue::Agg(p) => Ok(p),
            GenValue::Scalar(_) => Err(CodegenError {
                msg: format!("{what} requires an array/tuple, got a scalar"),
                line: span.line,
                col: span.col,
            }),
        }
    }
}

/// Compile a typed HIR program into an LLVM IR module.
/// The module contains `main() -> i64` and all user functions. Variables use stack
/// slots (alloca), so updates in loops/branches are visible across blocks; `print`
pub fn compile<'ctx>(
    context: &'ctx Context,
    program: &HirProgram,
    var_tys: &HashMap<DefId, Ty>,
    instances: &[GenericInstance],
    call_types: &HashMap<usize, Vec<Ty>>,
) -> Result<Module<'ctx>, CodegenError> {
    let module = context.create_module("aero");
    let builder = context.create_builder();
    let i64_ty = context.i64_type();
    let i32_ty = context.i32_type();
    let bool_ty = context.bool_type();
    let i8_ptr_ty = context.ptr_type(AddressSpace::from(0u16));

    // calls C `printf` (variadic); AND/OR short-circuit via br + phi.
    let printf_ty = i8_ptr_ty.fn_type(&[i8_ptr_ty.into()], true);
    let printf = module.add_function("printf", printf_ty, None);

    // Declare abort() (fallback for arena out-of-bounds)
    let abort = module.add_function("abort", context.void_type().fn_type(&[], false), None);


    // The main function
    let main = module.add_function("main", i64_ty.fn_type(&[], false), None);

    // Declare user functions (DefId aligned with program.funcs; builtin slots hold placeholders)
    let empty_subst = HashMap::new();
    let mut funcs: Vec<FunctionValue<'ctx>> = Vec::with_capacity(program.funcs.len());
    for f in &program.funcs {
        if f.builtin {
            // Builtins (assert/assert_eq) have no LLVM declaration; call sites special-case them,
            // so abort placeholders keep funcs[def_id] aligned with the HirFn table.
            funcs.push(abort);
            continue;
        }
        if !f.type_params.is_empty() {
            // Generic functions: generated dynamically at instantiation; placeholder here
            funcs.push(abort);
            continue;
        }
        if f.name == "main" || f.name == "printf" {
            return Err(CodegenError {
                msg: format!("function name `{}` is reserved", f.name),
                line: f.span.line,
                col: f.span.col,
            });
        }
        let llvm_name = f.extern_symbol.as_deref().unwrap_or(&f.name);
        // extern "C" functions use the C symbol name (possibly aliased via `= "sym"`);
        // others use the function name
        if f.is_extern && matches!(llvm_name, "printf" | "abort") {
            return Err(CodegenError {
                msg: format!("extern symbol name `{llvm_name}` is reserved"),
                line: f.span.line,
                col: f.span.col,
            });
        }
        let mut param_tys = Vec::new();
        for (_, ty, sp) in &f.params {
            param_tys.push(llvm_ty(context, ty, *sp, &empty_subst)?.into());
        }
        // extern aliases must not collide with symbols the codegen declares (printf/abort)
        let fn_ty = match &f.ret {
            Some(t) => llvm_ty(context, t, f.span, &empty_subst)?.fn_type(&param_tys, false),
            None => context.void_type().fn_type(&param_tys, false),
        };
        funcs.push(module.add_function(llvm_name, fn_ty, None));
    }

    // String-runtime libc helpers: reuse user-declared extern "C" functions with the
    // same symbol names (otherwise LLVM auto-renames the duplicate to `strlen.1` and
    // linking fails with an undefined reference), else declare them for the CRT.
    // Note: _snprintf (underscore prefix) is the CRT export on Windows; gcc's link
    // alias hides this in AOT builds while MCJIT needs the real export name.
    let declared = |name: &str, ty: inkwell::types::FunctionType<'ctx>| -> FunctionValue<'ctx> {
        funcs
            .iter()
            .find(|f| f.get_name().to_str().map(|s| s == name).unwrap_or(false))
            .copied()
            .unwrap_or_else(|| module.add_function(name, ty, None))
    };
    let malloc = declared("malloc", i8_ptr_ty.fn_type(&[i64_ty.into()], false));
    let free = declared(
        "free",
        context.void_type().fn_type(&[i8_ptr_ty.into()], false),
    );
    let memcpy = declared(
        "memcpy",
        i8_ptr_ty.fn_type(&[i8_ptr_ty.into(), i8_ptr_ty.into(), i64_ty.into()], false),
    );
    let strlen = declared("strlen", i64_ty.fn_type(&[i8_ptr_ty.into()], false));
    let strcmp = declared(
        "strcmp",
        i32_ty.fn_type(&[i8_ptr_ty.into(), i8_ptr_ty.into()], false),
    );
    let snprintf = declared(
        "_snprintf",
        i32_ty.fn_type(&[i8_ptr_ty.into(), i64_ty.into(), i8_ptr_ty.into()], true),
    );
    let strtoll = declared(
        "strtoll",
        i64_ty.fn_type(&[i8_ptr_ty.into(), i8_ptr_ty.into(), i32_ty.into()], false),
    );
    let strstr = declared(
        "strstr",
        i8_ptr_ty.fn_type(&[i8_ptr_ty.into(), i8_ptr_ty.into()], false),
    );

    let mut cg = Codegen {
        context,
        module: &module,
        builder,
        i64_ty,
        i32_ty,
        bool_ty,
        vars: HashMap::new(),
        var_tys,
        str_counter: 0,
        printf,
        abort,
        malloc,
        free,
        memcpy,
        strlen,
        strcmp,
        snprintf,
        strtoll,
        strstr,
        cur_func: main,
        funcs,
        arenas: HashMap::new(),
        arena_stack: Vec::new(),
        hir_funcs: &program.funcs,
        call_types,
        instance_funcs: HashMap::new(),
        instance_count: 0,
        type_subst: HashMap::new(),
    };

    let entry = context.append_basic_block(main, "entry");
    cg.builder.position_at_end(entry);
    cg.gen_block(&program.main)?;
    if !cg.cur_block_terminated() {
        let zero = cg.i64_ty.const_zero();
        bld(cg.builder.build_return(Some(&zero)))?;
    }

    // Build the fn type separately for void and non-void return types
    // Generate the main block
    for i in 0..program.funcs.len() {
        if program.funcs[i].builtin
            || !program.funcs[i].type_params.is_empty()
            || program.funcs[i].is_extern
        {
            continue;
        }
        let func_llvm = cg.funcs[i];
        cg.gen_function(&program.funcs[i], func_llvm)?;
    }

    // Generate user function bodies (indexed iteration avoids funcs borrow conflicts;
    // builtin/generic/extern functions are skipped). extern "C" functions are external
    for inst in instances {
        let type_args = inst.type_args.clone();
        if type_args
            .iter()
            .any(|t| matches!(t, Ty::Generic(_)))
        {
            continue;
        }
        cg.gen_instance(inst.fn_def_id, type_args)?;
    }

    // symbol declarations with no body; the linker resolves them.
    // Generate generic instance functions (monomorphization). Only "absolute instances"
    // (type args without generic params) are handled here; relative instances produced
    // inside generic bodies are expanded recursively by the outer instance.
    let mut gpu_kernels = Vec::new();
    for f in program.funcs.iter() {
        if f.is_gpu {
            let node = context.metadata_node(&[
                context.metadata_string(&f.name).into(),
                context.metadata_string("kernel").into(),
                cg.i32_ty.const_int(1, false).into(),
            ]);
            gpu_kernels.push(node.into());
        }
    }
    if !gpu_kernels.is_empty() {
        let tuple = context.metadata_node(&gpu_kernels);
        module
            .add_global_metadata("aero.gpu_kernels", &tuple)
            .map_err(|e| CodegenError {
                msg: format!("failed to write GPU kernel metadata: {e}"),
                line: 0,
                col: 0,
            })?;
    }

    // Note: LLVMVerifyModule in the official LLVM 22.1.8 Windows static libs crashes
    // (0xC0000005; a minimal repro triggers it reliably, while the official opt tool
    // verifies the same IR fine). Frontend type/borrow checks guarantee IR correctness,
    // so verification is skipped by default; set AERO_VERIFY=1 to re-enable it.
    if std::env::var("AERO_VERIFY").is_ok() {
        module
            .verify()
            .map_err(|e| CodegenError {
                msg: format!("LLVM module verification failed: {e}"),
                line: 0,
                col: 0,
            })?;
    }

    Ok(module)
}

/// Map an Aero type to an LLVM type. Arrays/tuples map to aggregate types.
/// `subst` maps the current generic instance type parameters (`Generic(name)` to concrete);
/// pass an empty map outside generic contexts.
fn llvm_ty<'ctx>(
    context: &'ctx Context,
    ty: &Ty,
    span: Span,
    subst: &HashMap<String, Ty>,
) -> Result<BasicTypeEnum<'ctx>, CodegenError> {
    match ty {
        Ty::I32 => Ok(context.i32_type().into()),
        Ty::I64 => Ok(context.i64_type().into()),
        Ty::Bool => Ok(context.bool_type().into()),
        Ty::Str => Ok(context.ptr_type(AddressSpace::from(0u16)).into()),
        Ty::Array(elem, n) => {
            let elem_ty = llvm_ty(context, elem, span, subst)?;
            Ok(elem_ty.array_type(*n as u32).into())
        }
        Ty::Tensor { elem, shape } => {
            // Multi-dim tensors map to nested arrays: tensor<3x4> -> [3 x [4 x i64]]
            let mut t = llvm_ty(context, elem, span, subst)?;
            for d in shape.iter().rev() {
                t = t.array_type(*d as u32).into();
            }
            Ok(t)
        }
        Ty::Tuple(elems) => {
            let mut tys = Vec::new();
            for e in elems {
                tys.push(llvm_ty(context, e, span, subst)?.into());
            }
            Ok(context.struct_type(&tys, false).into())
        }
        Ty::Ref { inner, .. } => {
            // LLVM 15+ pointers are opaque (no inner type distinction)
            llvm_ty(context, inner, span, subst)?;
            Ok(context.ptr_type(AddressSpace::from(0u16)).into())
        }
        Ty::Ptr(inner) => {
            llvm_ty(context, inner, span, subst)?;
            Ok(context.ptr_type(AddressSpace::from(0u16)).into())
        }
        Ty::Arena(_) => Err(CodegenError {
            msg: "internal error: arena type used as an ordinary value type".to_string(),
            line: span.line,
            col: span.col,
        }),
        Ty::Fn(_, _) => Err(CodegenError {
            msg: "function types cannot be value types".to_string(),
            line: span.line,
            col: span.col,
        }),
        Ty::Void => Err(CodegenError {
            msg: "void cannot be a value type".to_string(),
            line: span.line,
            col: span.col,
        }),
        Ty::Var(_) => Err(CodegenError {
            msg: "internal error: an undefaulted type variable reached codegen".to_string(),
            line: span.line,
            col: span.col,
        }),
        Ty::Generic(name) => match subst.get(name) {
            // Instantiated generic param: substitute the concrete type and recurse
            Some(concrete) => llvm_ty(context, concrete, span, subst),
            // Uninstantiated generic param: the function was compiled as ordinary
            None => Err(CodegenError {
                msg: format!("internal error: generic parameter `{name}` was not instantiated (generic functions must be called via instantiation)"),
                line: span.line,
                col: span.col,
            }),
        },
    }
}

fn is_agg(ty: &Ty) -> bool {
    matches!(ty, Ty::Array(..) | Ty::Tuple(_) | Ty::Tensor { .. })
}

#[derive(Clone, Copy)]
struct ArenaSlots<'ctx> {
    buf: PointerValue<'ctx>,
    // Uninstantiated generic param: the function was compiled as ordinary
    offset: PointerValue<'ctx>,
    /// Byte capacity
    capacity: u64,
}

struct Codegen<'a, 'ctx> {
    context: &'ctx Context,
    /// Module reference (generic instance functions are added on demand)
    module: &'a Module<'ctx>,
    builder: Builder<'ctx>,
    i64_ty: IntType<'ctx>,
    i32_ty: IntType<'ctx>,
    bool_ty: IntType<'ctx>,
    /// Variable DefId → stack-slot pointer
    vars: HashMap<DefId, PointerValue<'ctx>>,
    /// Variable type table produced by type checking
    var_tys: &'a HashMap<DefId, Ty>,
    /// Counter for string global constants
    str_counter: u32,
    printf: FunctionValue<'ctx>,
    /// `abort()` (called on arena out-of-bounds)
    abort: FunctionValue<'ctx>,
    /// String-runtime libc helpers (malloc/free/memcpy/strlen/strcmp/snprintf)
    malloc: FunctionValue<'ctx>,
    free: FunctionValue<'ctx>,
    memcpy: FunctionValue<'ctx>,
    strlen: FunctionValue<'ctx>,
    strcmp: FunctionValue<'ctx>,
    snprintf: FunctionValue<'ctx>,
    /// strtoll (string -> integer parse) and strstr (substring search)
    strtoll: FunctionValue<'ctx>,
    strstr: FunctionValue<'ctx>,
    /// The function currently being generated
    cur_func: FunctionValue<'ctx>,
    /// User function table (indexed by DefId; generic slots hold abort placeholders,
    funcs: Vec<FunctionValue<'ctx>>,
    /// called via instantiation)
    arenas: HashMap<DefId, ArenaSlots<'ctx>>,
    /// Arena variable DefId → internal slots
    arena_stack: Vec<Vec<DefId>>,
    /// Arenas created per block (auto-reset at scope end)
    hir_funcs: &'a [HirFn],
    /// User function table (HIR level, for expression type lookups)
    call_types: &'a HashMap<usize, Vec<Ty>>,
    /// Generic call sites span.start → type args (from inference)
    instance_funcs: HashMap<(DefId, Vec<Ty>), FunctionValue<'ctx>>,
    /// Generic instance functions: (fn DefId, type args) → LLVM fn (monomorphization registry)
    instance_count: usize,
    /// Total generic instances generated (guards against infinite monomorphization)
    type_subst: HashMap<String, Ty>,
}

impl<'a, 'ctx> Codegen<'a, 'ctx> {
    /// Type-parameter map of the current generic instance (active while generating its body)
    fn t(&self, ty: &Ty, span: Span) -> Result<BasicTypeEnum<'ctx>, CodegenError> {
        llvm_ty(self.context, ty, span, &self.type_subst)
    }

    /// Map an Aero type to an LLVM type (using the current instance `type_subst`).
    fn gen_function(&mut self, f: &HirFn, func_llvm: FunctionValue<'ctx>) -> Result<(), CodegenError> {
        self.cur_func = func_llvm;
        let entry = self.context.append_basic_block(func_llvm, "entry");
        self.builder.position_at_end(entry);
        self.vars.clear();
        for (i, (_, _, _sp)) in f.params.iter().enumerate() {
            let param_val = func_llvm.get_nth_param(i as u32).expect("parameter exists");
            let ptr = bld(self.builder.build_alloca(param_val.get_type(), "arg"))?;
            bld(self.builder.build_store(ptr, param_val))?;
            self.vars.insert(f.param_defs[i], ptr);
        }
        self.gen_block(&f.body)?;
        // Fallback return at the end (type checking guarantees consistent return paths);
        if !self.cur_block_terminated() {
            match &f.ret {
                Some(t) => {
                    let zero = self.t(t, f.span)?.into_int_type().const_zero();
                    bld(self.builder.build_return(Some(&zero)))?;
                }
                None => {
                    bld(self.builder.build_return(None))?;
                }
            }
        }
        Ok(())
    }

    // skip if already terminated
    ///
    /// Generate one concrete instance of a generic function (monomorphization).
    /// Instantiate the signature with `type_args`, declare the LLVM function (mangled),
    /// then generate the body under the `type_subst` context; nested generic calls expand
    fn gen_instance(
        &mut self,
        fn_def_id: DefId,
        type_args: Vec<Ty>,
    ) -> Result<FunctionValue<'ctx>, CodegenError> {
        if let Some(func) = self.instance_funcs.get(&(fn_def_id, type_args.clone())) {
            return Ok(*func);
        }
        if self.instance_count >= MAX_GENERIC_INSTANCES {
            let f = &self.hir_funcs[fn_def_id as usize];
            return Err(CodegenError {
                msg: format!(
                    "generic function `{}` exceeded the instantiation cap ({}); possible infinite generic instantiation (e.g. self-nesting `f<[T]>`)",
                    f.name, MAX_GENERIC_INSTANCES
                ),
                line: f.span.line,
                col: f.span.col,
            });
        }
        // Clone independently (avoid borrow conflicts with &mut self)
        let f = self.hir_funcs[fn_def_id as usize].clone();
        if f.type_params.is_empty() {
            return Err(CodegenError {
                msg: format!("internal error: non-generic function `{}` must not take the instantiation path", f.name),
                line: f.span.line,
                col: f.span.col,
            });
        }
        if f.type_params.len() != type_args.len() {
            return Err(CodegenError {
                msg: format!(
                    "internal error: generic parameter count mismatch for `{}` (declared {}, instantiated {})",
                    f.name,
                    f.type_params.len(),
                    type_args.len()
                ),
                line: f.span.line,
                col: f.span.col,
            });
        }
        let subst: HashMap<String, Ty> = f
            .type_params
            .iter()
            .cloned()
            .zip(type_args.iter().cloned())
            .collect();
        // Type-parameter map: Generic(name) → concrete type
        let empty_subst = HashMap::new();
        let mut param_tys = Vec::new();
        for (_, pty, sp) in &f.params {
            let inst = substitute(pty, &subst);
            param_tys.push(llvm_ty(self.context, &inst, *sp, &empty_subst)?.into());
        }
        let fn_ty = match &f.ret {
            Some(t) => {
                let inst = substitute(t, &subst);
                llvm_ty(self.context, &inst, f.span, &empty_subst)?.fn_type(&param_tys, false)
            }
            None => self.context.void_type().fn_type(&param_tys, false),
        };
        let func = self.module.add_function(&mono_name(&f.name, &type_args), fn_ty, None);
        self.instance_funcs.insert((fn_def_id, type_args), func);
        self.instance_count += 1;
        // Instantiated signature → LLVM function type
        // Generate the body under the instance context (Generic resolved via type_subst).
        // Nested instantiation mutates vars / cur_func / builder insertion point, so they are
        let saved_subst = std::mem::take(&mut self.type_subst);
        let saved_vars = std::mem::take(&mut self.vars);
        let saved_func = self.cur_func;
        let saved_block = self.builder.get_insert_block();
        self.type_subst = subst;
        self.gen_function(&f, func)?;
        self.type_subst = saved_subst;
        self.vars = saved_vars;
        self.cur_func = saved_func;
        if let Some(bb) = saved_block {
            self.builder.position_at_end(bb);
        }
        Ok(func)
    }

    // saved and restored after the return, or the outer function (e.g. main) tables and
    // insertion point would be cleared/preempted by the instance function.
    fn resolve_call_instance(
        &self,
        span: Span,
        hir_f: &HirFn,
    ) -> Result<Vec<Ty>, CodegenError> {
        let raw = self
            .call_types
            .get(&span.start)
            .ok_or_else(|| CodegenError {
                msg: format!(
                    "internal error: generic function `{}` call site lacks type-instance info (infer did not record it)",
                    hir_f.name
                ),
                line: span.line,
                col: span.col,
            })?
            .clone();
        if raw.len() != hir_f.type_params.len() {
            return Err(CodegenError {
                msg: format!(
                    "internal error: call-site type-arg count mismatch for `{}` (expected {}, got {})",
                    hir_f.name,
                    hir_f.type_params.len(),
                    raw.len()
                ),
                line: span.line,
                col: span.col,
            });
        }
        // Apply the current instance context (type_args may contain outer generic params)
        let resolved: Vec<Ty> = raw
            .iter()
            .map(|t| substitute(t, &self.type_subst))
            .collect();
        for t in &resolved {
            if let Ty::Generic(name) = t {
                return Err(CodegenError {
                    msg: format!(
                        "generic parameter `{name}` could not be instantiated at the call site of `{}` (inference produced no concrete type)",
                        hir_f.name
                    ),
                    line: span.line,
                    col: span.col,
                });
            }
        }
        Ok(resolved)
    }

    /// Whether the current insertion block is terminated (return/unreachable, etc.).
    fn cur_block_terminated(&self) -> bool {
        self.builder
            .get_insert_block()
            .map(|bb| bb.get_terminator().is_some())
            .unwrap_or(true)
    }

    fn gen_block(&mut self, block: &HirBlock) -> Result<(), CodegenError> {
        let outer: Vec<DefId> = self.vars.keys().copied().collect();
        self.arena_stack.push(Vec::new());
        for stmt in &block.stmts {
            if self.cur_block_terminated() {
                // Block scope: variables declared in a block are rolled back at its end (local semantics).
            }
            self.gen_stmt(stmt)?;
        }
        // Arenas are auto-reset at block end (offset zeroed, bulk release).
        if !self.cur_block_terminated() {
            if let Some(defs) = self.arena_stack.pop() {
                for def in defs {
                    if let Some(slots) = self.arenas.get(&def) {
                        bld(self.builder
                            .build_store(slots.offset, self.i64_ty.const_zero()))?;
                    }
                }
            }
        } else {
            self.arena_stack.pop();
        }
        self.vars.retain(|def, _| outer.contains(def));
        Ok(())
    }

    fn gen_stmt(&mut self, stmt: &HirStmt) -> Result<(), CodegenError> {
        match stmt {
            HirStmt::Let {
                def_id,
                init,
                span,
                ..
            } => {
                let ty = self
                    .var_tys
                    .get(def_id)
                    .cloned()
                    .ok_or_else(|| self.internal_err(*span, "missing type for let variable"))?;
                if let Ty::Arena(size) = &ty {
                    // Arena init: byte pool + offset slot (offset zeroed)
                    let buf_ty = self.context.i8_type().array_type(*size as u32);
                    let buf = bld(self.builder.build_alloca(buf_ty, "arena_buf"))?;
                    let offset = bld(self.builder.build_alloca(self.i64_ty, "arena_off"))?;
                    bld(self.builder.build_store(offset, self.i64_ty.const_zero()))?;
                    self.arenas.insert(
                        *def_id,
                        ArenaSlots {
                            buf,
                            offset,
                            capacity: *size as u64,
                        },
                    );
                    self.arena_stack
                        .last_mut()
                        .expect("gen_block established the arena stack")
                        .push(*def_id);
                    self.vars.insert(*def_id, buf);
                    return Ok(());
                }
                if is_agg(&ty) {
                    // Aggregate: literals fill the target type directly; others (variable refs) deep-copy
                    let target = bld(self.builder.build_alloca(
                        self.t(&ty, *span)?,
                        "agg",
                    ))?;
                    self.vars.insert(*def_id, target);
                    self.gen_agg_store(target, init, &ty, *span, "let initializer")?;
                } else {
                    let slot_ty = self.t(&ty, *span)?;
                    let ptr = bld(self.builder.build_alloca(slot_ty, "var"))?;
                    let v = self.gen_value(init)?.scalar(*span, "let initializer")?;
                    let v = self.coerce(v, &slot_ty, *span, "let initializer")?;
                    bld(self.builder.build_store(ptr, v))?;
                    self.vars.insert(*def_id, ptr);
                }
                Ok(())
            }
            HirStmt::Assign {
                def_id,
                value,
                span,
            } => {
                let ty = self
                    .var_tys
                    .get(def_id)
                    .cloned()
                    .ok_or_else(|| self.internal_err(*span, "missing type for assignment target"))?;
                let ptr = *self
                    .vars
                    .get(def_id)
                    .ok_or_else(|| self.internal_err(*span, "assignment target not defined"))?;
                if is_agg(&ty) {
                    self.gen_agg_store(ptr, value, &ty, *span, "assignment")?;
                } else {
                    let slot_ty = self.t(&ty, *span)?;
                    let v = self.gen_value(value)?.scalar(*span, "assignment")?;
                    let v = self.coerce(v, &slot_ty, *span, "assignment")?;
                    bld(self.builder.build_store(ptr, v))?;
                }
                Ok(())
            }
            HirStmt::AssignIndex {
                target,
                index,
                value,
                span,
            } => {
                let (slot, elem_ty) = self.gen_index_ptr(target, index, *span)?;
                let v = self.gen_value(value)?.scalar(*span, "index write")?;
                let v = self.coerce(v, &elem_ty, *span, "index write")?;
                bld(self.builder.build_store(slot, v))?;
                Ok(())
            }
            HirStmt::AssignDeref { target, value, span } => {
                let v = self.gen_value(value)?.scalar(*span, "deref write")?;
                let (ptr, inner_ty) = self.deref_ptr(target, *span)?;
                let v = self.coerce(v, &self.t(&inner_ty, *span)?, *span, "deref write")?;
                // `*ptr = v`: target is the dereferenced value expression (type `&mut T`)
                bld(self.builder.build_store(ptr, v))?;
                Ok(())
            }
            HirStmt::Print(args, span) => self.gen_print(args, *span),
            HirStmt::Expr(expr, _) => {
                // Expression statement: void calls / builtin asserts / arena.reset() — generate and drop
                if let HirExpr::Call {
                    def_id, args, span,
                } = expr
                {
                    let hir_f = self
                        .hir_funcs
                        .get(*def_id as usize)
                        .ok_or_else(|| self.internal_err(*span, "missing function table"))?;
                    if hir_f.builtin {
                        // Builtin asserts: assert/assert_eq (void; abort on failure)
                        return self.gen_builtin_call(&hir_f.name, args, *span);
                    }
                    // Generic call: dispatched to the concrete instance via monomorphization
                    let func = if !hir_f.type_params.is_empty() {
                        let type_args = self.resolve_call_instance(*span, hir_f)?;
                        self.gen_instance(*def_id, type_args)?
                    } else {
                        *self
                            .funcs
                            .get(*def_id as usize)
                            .ok_or_else(|| self.internal_err(*span, "missing function table"))?
                    };
                    if func.get_type().get_return_type().is_none() {
                        let mut call_args = Vec::new();
                        let param_tys = func.get_type().get_param_types();
                        for (i, arg) in args.iter().enumerate() {
                            let v = self.gen_value(arg)?;
                            let pt: BasicTypeEnum = param_tys[i]
                                .try_into()
                                .map_err(|_| self.internal_err(*span, "parameter type mismatch"))?;
                            let v = self.call_arg(v, &pt, *span, "function argument")?;
                            call_args.push(v.into());
                        }
                        bld(self.builder.build_call(func, &call_args, "call"))?;
                        return Ok(());
                    }
                }
                if let HirExpr::MethodCall { method, .. } = expr {
                    if method == "reset" {
                        self.gen_method_call(expr)?;
                        return Ok(());
                    }
                }
                self.gen_value(expr)?;
                Ok(())
            }
            HirStmt::If {
                cond,
                then_body,
                else_body,
                ..
            } => self.gen_if(cond, then_body, else_body),
            HirStmt::While { cond, body, .. } => self.gen_while(cond, body),
            HirStmt::Return(value, span) => {
                let v = match value {
                    Some(e) => {
                        let gv = self.gen_value(e)?;
                        match gv {
                            GenValue::Scalar(v) => Some(v),
                            // Returning an aggregate (array/tuple): load the whole value from its stack slot
                            GenValue::Agg(p) => {
                                let ret_ty = self
                                    .cur_func
                                    .get_type()
                                    .get_return_type()
                                    .ok_or_else(|| {
                                        self.internal_err(*span, "returning an aggregate but the function has no return type")
                                    })?;
                                Some(bld(self.builder.build_load(ret_ty, p, "ret_agg"))?)
                            }
                        }
                    }
                    None => None,
                };
                match v {
                    Some(val) => {
                        bld(self.builder.build_return(Some(&val)))?;
                    }
                    None => {
                        bld(self.builder.build_return(None))?;
                    }
                }
                Ok(())
            }
        }
    }

    /// Aggregate assignment: literals fill the target type; variable refs deep-copy by type.
    fn gen_agg_store(
        &mut self,
        target: PointerValue<'ctx>,
        init: &HirExpr,
        ty: &Ty,
        span: Span,
        what: &str,
    ) -> Result<(), CodegenError> {
        match (init, ty) {
            (HirExpr::Array(elems, _), Ty::Array(elem, n)) => {
                if elems.len() != *n {
                    return Err(CodegenError {
                        msg: format!("{what}: array length {} does not match declared length {}", elems.len(), n),
                        line: span.line,
                        col: span.col,
                    });
                }
                let elem_ty = self.t(elem, span)?;
                let arr_ty = self.t(ty, span)?;
                for (i, e) in elems.iter().enumerate() {
                    let v = self.gen_value(e)?.scalar(span, what)?;
                    let v = self.coerce(v, &elem_ty, span, what)?;
                    let idx = self.i32_ty.const_int(i as u64, false);
                    let slot = bld(unsafe {
                        self.builder.build_in_bounds_gep(
                            arr_ty,
                            target,
                            &[self.i32_ty.const_zero(), idx],
                            "aelem",
                        )
                    })?;
                    bld(self.builder.build_store(slot, v))?;
                }
                Ok(())
            }
            (HirExpr::Tuple(elems, _), Ty::Tuple(tys)) => {
                if elems.len() != tys.len() {
                    return Err(CodegenError {
                        msg: format!("{what}: tuple element count mismatch"),
                        line: span.line,
                        col: span.col,
                    });
                }
                let struct_ty = self.t(ty, span)?;
                for (i, e) in elems.iter().enumerate() {
                    let elem_ty = self.t(&tys[i], span)?;
                    let v = self.gen_value(e)?.scalar(span, what)?;
                    let v = self.coerce(v, &elem_ty, span, what)?;
                    let idx = self.i32_ty.const_int(i as u64, false);
                    let slot = bld(unsafe {
                        self.builder.build_in_bounds_gep(
                            struct_ty,
                            target,
                            &[self.i32_ty.const_zero(), idx],
                            "telem",
                        )
                    })?;
                    bld(self.builder.build_store(slot, v))?;
                }
                Ok(())
            }
            _ => {
                // Variable ref: type checking guarantees isomorphism with the target; copy element-wise
                let src = self.gen_value(init)?.agg(span, what)?;
                self.copy_agg(target, src, ty, span, what)
            }
        }
    }

    /// Deep-copy an aggregate (source and target types are isomorphic).
    fn copy_agg(
        &mut self,
        dst: PointerValue<'ctx>,
        src_ptr: PointerValue<'ctx>,
        ty: &Ty,
        span: Span,
        what: &str,
    ) -> Result<(), CodegenError> {
        match ty {
            Ty::Array(elem, n) => {
                let elem_ty = self.t(elem, span)?;
                let arr_ty = self.t(ty, span)?;
                for i in 0..*n {
                    let idx = self.i32_ty.const_int(i as u64, false);
                    let s = bld(unsafe {
                        self.builder.build_in_bounds_gep(
                            arr_ty,
                            src_ptr,
                            &[self.i32_ty.const_zero(), idx],
                            "cp_s",
                        )
                    })?;
                    let d = bld(unsafe {
                        self.builder.build_in_bounds_gep(
                            arr_ty,
                            dst,
                            &[self.i32_ty.const_zero(), idx],
                            "cp_d",
                        )
                    })?;
                    let v = bld(self.builder.build_load(elem_ty, s, "cp_l"))?;
                    bld(self.builder.build_store(d, v))?;
                }
                Ok(())
            }
            Ty::Tuple(elems) => {
                let struct_ty = self.t(ty, span)?;
                for (i, elem) in elems.iter().enumerate() {
                    let elem_ty = self.t(elem, span)?;
                    let idx = self.i32_ty.const_int(i as u64, false);
                    let s = bld(unsafe {
                        self.builder.build_in_bounds_gep(
                            struct_ty,
                            src_ptr,
                            &[self.i32_ty.const_zero(), idx],
                            "cp_s",
                        )
                    })?;
                    let d = bld(unsafe {
                        self.builder.build_in_bounds_gep(
                            struct_ty,
                            dst,
                            &[self.i32_ty.const_zero(), idx],
                            "cp_d",
                        )
                    })?;
                    let v = bld(self.builder.build_load(elem_ty, s, "cp_l"))?;
                    bld(self.builder.build_store(d, v))?;
                }
                Ok(())
            }
            Ty::Tensor { elem, shape } if shape.is_empty() => {
                // Tensors recurse to the innermost layer: scalar element copies
                let elem_ty = self.t(elem, span)?;
                let v = bld(self.builder.build_load(elem_ty, src_ptr, "cp_tl"))?;
                bld(self.builder.build_store(dst, v))?;
                Ok(())
            }
            Ty::Tensor { elem, shape } => {
                let llvm = self.t(ty, span)?;
                for i in 0..shape[0] {
                    let idx = self.i32_ty.const_int(i as u64, false);
                    let s = bld(unsafe {
                        self.builder.build_in_bounds_gep(
                            llvm,
                            src_ptr,
                            &[self.i32_ty.const_zero(), idx],
                            "cp_ts",
                        )
                    })?;
                    let d = bld(unsafe {
                        self.builder.build_in_bounds_gep(
                            llvm,
                            dst,
                            &[self.i32_ty.const_zero(), idx],
                            "cp_td",
                        )
                    })?;
                    let sub_ty = Ty::Tensor {
                        elem: elem.clone(),
                        shape: shape[1..].to_vec(),
                    };
                    self.copy_agg(d, s, &sub_ty, span, what)?;
                }
                Ok(())
            }
            other => Err(CodegenError {
                msg: format!("{what}: cannot copy non-aggregate type `{other}`"),
                line: span.line,
                col: span.col,
            }),
        }
    }

    /// Generate an expression value.
    fn gen_value(&mut self, expr: &HirExpr) -> Result<GenValue<'ctx>, CodegenError> {
        match expr {
            HirExpr::IntLit(v, _) => Ok(GenValue::Scalar(
                self.i64_ty.const_int(*v as u64, false).into(),
            )),
            HirExpr::BoolLit(v, _) => Ok(GenValue::Scalar(
                self.bool_ty.const_int(if *v { 1 } else { 0 }, false).into(),
            )),
            HirExpr::StrLit(s, _) => {
                let p = self.global_string(s)?;
                Ok(GenValue::Scalar(p.into()))
            }
            HirExpr::Var(def_id, span) => {
                let ty = self
                    .var_tys
                    .get(def_id)
                    .ok_or_else(|| self.internal_err(*span, "missing variable type"))?
                    .clone();
                let ptr = self
                    .vars
                    .get(def_id)
                    .ok_or_else(|| self.internal_err(*span, "variable has no stack slot"))?;
                if is_agg(&ty) {
                    Ok(GenValue::Agg(*ptr))
                } else {
                    let slot_ty = self.t(&ty, *span)?;
                    let loaded = bld(self.builder.build_load(slot_ty, *ptr, "var"))?;
                    Ok(GenValue::Scalar(loaded))
                }
            }
            // Borrow &x / &mut x: return the source variable stack-slot address (the reference value)
            HirExpr::Borrow { def_id, span, .. } => {
                let ptr = self
                    .vars
                    .get(def_id)
                    .ok_or_else(|| self.internal_err(*span, "borrow target has no stack slot"))?;
                Ok(GenValue::Scalar(ptr.as_basic_value_enum()))
            }
            // Deref *p: load by the reference/pointer target type
            HirExpr::Deref { target, span } => {
                let inner_ty = self.deref_inner_ty(target, *span)?;
                let p = self.gen_value(target)?.scalar(*span, "dereference")?;
                let slot_ty = self.t(&inner_ty, *span)?;
                let loaded =
                    bld(self.builder.build_load(slot_ty, p.into_pointer_value(), "deref"))?;
                Ok(GenValue::Scalar(loaded))
            }
            HirExpr::MethodCall { .. } => match self.gen_method_call(expr)? {
                Some(v) => Ok(v),
                None => Err(self.internal_err(expr.span(), "void reset call used as an expression")),
            },
            HirExpr::ArenaLit(_, span) => Err(CodegenError {
                msg: "internal error: arena literal may only appear in a let initializer".to_string(),
                line: span.line,
                col: span.col,
            }),
            // Tensor literal: zero-initialize the nested array, return the Agg slot
            HirExpr::TensorLit(dims, span) => {
                let ty = Ty::Tensor {
                    elem: Box::new(Ty::I64),
                    shape: dims.clone(),
                };
                let llvm = self.t(&ty, *span)?;
                let tmp = bld(self.builder.build_alloca(llvm, "tensor"))?;
                self.store_zero_agg(tmp, &ty, *span)?;
                Ok(GenValue::Agg(tmp))
            }
            HirExpr::Matmul { .. } => self.gen_matmul(expr),
            // Aggregate literals are handled on the "fill by target type" path of let/assign;
            // elsewhere (indexing, arguments) they build a temporary slot by default rules.
            HirExpr::Array(elems, span) => {
                let arr_ty = self.i64_ty.array_type(elems.len() as u32);
                let tmp = bld(self.builder.build_alloca(arr_ty, "array"))?;
                for (i, e) in elems.iter().enumerate() {
                    let v = self.gen_value(e)?.scalar(*span, "array element")?;
                    let v = self.coerce(v, &self.i64_ty.into(), *span, "array element")?;
                    let idx = self.i32_ty.const_int(i as u64, false);
                    let slot = bld(unsafe {
                        self.builder.build_in_bounds_gep(
                            arr_ty,
                            tmp,
                            &[self.i32_ty.const_zero(), idx],
                            "aelem",
                        )
                    })?;
                    bld(self.builder.build_store(slot, v))?;
                }
                Ok(GenValue::Agg(tmp))
            }
            HirExpr::Tuple(elems, span) => {
                let mut tys = Vec::new();
                for e in elems {
                    tys.push(self.elem_ty_hint(e));
                }
                let struct_ty = self.context.struct_type(
                    &tys
                        .iter()
                        .map(|t| self.t(t, *span).map(|b| b.into()))
                        .collect::<Result<Vec<_>, _>>()?,
                    false,
                );
                let tmp = bld(self.builder.build_alloca(struct_ty, "tuple"))?;
                for (i, e) in elems.iter().enumerate() {
                    let elem_ty = self.t(&tys[i], *span)?;
                    let v = self.gen_value(e)?.scalar(*span, "tuple element")?;
                    let v = self.coerce(v, &elem_ty, *span, "tuple element")?;
                    let idx = self.i32_ty.const_int(i as u64, false);
                    let slot = bld(unsafe {
                        self.builder.build_in_bounds_gep(
                            struct_ty,
                            tmp,
                            &[self.i32_ty.const_zero(), idx],
                            "telem",
                        )
                    })?;
                    bld(self.builder.build_store(slot, v))?;
                }
                Ok(GenValue::Agg(tmp))
            }
            HirExpr::Index {
                target,
                index,
                span,
            } => self.gen_index(target, index, *span),
            HirExpr::Unary {
                op: UnOp::Neg,
                expr: inner,
                span,
            } => {
                let v = self.gen_value(inner)?.scalar(*span, "unary minus")?;
                let iv = v.into_int_value();
                Ok(GenValue::Scalar(bld(self.builder.build_int_neg(iv, "neg"))?.into()))
            }
            HirExpr::Binary { op, lhs, rhs, span } => {
                let ty = self.expr_ty(expr)?;
                if ty == Ty::Str {
                    // String concatenation (type checking guarantees `+` only).
                    // Fold two string literals at compile time; otherwise allocate
                    // and copy at runtime (libc malloc/memcpy, released via str_free).
                    if let (HirExpr::StrLit(s1, _), HirExpr::StrLit(s2, _)) = (&**lhs, &**rhs) {
                        let p = self.global_string(&format!("{s1}{s2}"))?;
                        return Ok(GenValue::Scalar(p.into()));
                    }
                    let a = self.gen_value(lhs)?.scalar(*span, "concatenation")?;
                    let b = self.gen_value(rhs)?.scalar(*span, "concatenation")?;
                    let buf = self.gen_str_concat(a, b, *span)?;
                    return Ok(GenValue::Scalar(buf.into()));
                }
                let l = self.gen_value(lhs)?.scalar(*span, "arithmetic")?;
                let r = self.gen_value(rhs)?.scalar(*span, "arithmetic")?;
                let l_ty = l.get_type();
                let r = self.coerce(r, &l_ty, *span, "arithmetic operand")?;
                let l = l.into_int_value();
                let r = r.into_int_value();
                let out = bld(match op {
                    BinOp::Add => self.builder.build_int_add(l, r, "add"),
                    BinOp::Sub => self.builder.build_int_sub(l, r, "sub"),
                    BinOp::Mul => self.builder.build_int_mul(l, r, "mul"),
                    BinOp::Div => self.builder.build_int_signed_div(l, r, "div"),
                })?;
                Ok(GenValue::Scalar(out.into()))
            }
            HirExpr::Cmp { op, lhs, rhs, span } => {
                let l = self.gen_value(lhs)?.scalar(*span, "comparison")?;
                let r = self.gen_value(rhs)?.scalar(*span, "comparison")?;
                if l.get_type().is_pointer_type() {
                    // String comparison via strcmp: all six operators compare the
                    // strcmp result with 0 (`a < b` <=> strcmp(a, b) < 0, etc.)
                    let cmp = bld(self.builder.build_call(
                        self.strcmp,
                        &[l.into(), r.into()],
                        "strcmp",
                    ))?
                    .try_as_basic_value()
                    .basic()
                    .ok_or_else(|| self.internal_err(*span, "strcmp returned no value"))?;
                    let zero = self.i32_ty.const_zero();
                    let out = bld(match op {
                        CmpOp::Eq => self
                            .builder
                            .build_int_compare(IntPredicate::EQ, cmp.into_int_value(), zero, "streq"),
                        CmpOp::Ne => self
                            .builder
                            .build_int_compare(IntPredicate::NE, cmp.into_int_value(), zero, "strne"),
                        CmpOp::Lt => self
                            .builder
                            .build_int_compare(IntPredicate::SLT, cmp.into_int_value(), zero, "strlt"),
                        CmpOp::Gt => self
                            .builder
                            .build_int_compare(IntPredicate::SGT, cmp.into_int_value(), zero, "strgt"),
                        CmpOp::Le => self
                            .builder
                            .build_int_compare(IntPredicate::SLE, cmp.into_int_value(), zero, "strle"),
                        CmpOp::Ge => self
                            .builder
                            .build_int_compare(IntPredicate::SGE, cmp.into_int_value(), zero, "strge"),
                    })?;
                    return Ok(GenValue::Scalar(out.into()));
                }
                let l_ty = l.get_type();
                let r = self.coerce(r, &l_ty, *span, "comparison operand")?;
                let pred = match op {
                    CmpOp::Lt => IntPredicate::SLT,
                    CmpOp::Gt => IntPredicate::SGT,
                    CmpOp::Le => IntPredicate::SLE,
                    CmpOp::Ge => IntPredicate::SGE,
                    CmpOp::Eq => IntPredicate::EQ,
                    CmpOp::Ne => IntPredicate::NE,
                };
                let c = bld(self.builder.build_int_compare(
                    pred,
                    l.into_int_value(),
                    r.into_int_value(),
                    "cmp",
                ))?;
                Ok(GenValue::Scalar(c.into()))
            }
            HirExpr::Logic { .. } => {
                let c = self.gen_cond(expr)?;
                Ok(GenValue::Scalar(c.into()))
            }
            HirExpr::Call {
                def_id, args, span,
            } => {
                let hir_f = self
                    .hir_funcs
                    .get(*def_id as usize)
                    .ok_or_else(|| self.internal_err(*span, "missing function table"))?;
                if hir_f.builtin {
                    // String builtins with return values: len / int_to_str
                    match hir_f.name.as_str() {
                        "len" => {
                            if args.len() != 1 {
                                return Err(CodegenError {
                                    msg: "`len` requires 1 string argument".to_string(),
                                    line: span.line,
                                    col: span.col,
                                });
                            }
                            let s = self.gen_value(&args[0])?.scalar(*span, "len argument")?;
                            let l = bld(self.builder.build_call(self.strlen, &[s.into()], "len"))?
                                .try_as_basic_value()
                                .basic()
                                .ok_or_else(|| self.internal_err(*span, "strlen returned no value"))?;
                            return Ok(GenValue::Scalar(l.into()));
                        }
                        "int_to_str" => {
                            if args.len() != 1 {
                                return Err(CodegenError {
                                    msg: "`int_to_str` requires 1 integer argument".to_string(),
                                    line: span.line,
                                    col: span.col,
                                });
                            }
                            let n = self.gen_value(&args[0])?.scalar(*span, "int_to_str argument")?;
                            // 32 bytes is enough for any i64 (sign + 19 digits + NUL)
                            let cap = self.i64_ty.const_int(32, false);
                            let buf = bld(self.builder.build_call(
                                self.malloc,
                                &[cap.into()],
                                "itoa_buf",
                            ))?
                            .try_as_basic_value()
                            .basic()
                            .ok_or_else(|| self.internal_err(*span, "malloc returned no value"))?
                            .into_pointer_value();
                            // snprintf(buf, 32, "%lld", n)
                            let fmt = self.global_string("%lld")?;
                            let call_args: [BasicMetadataValueEnum<'ctx>; 4] = [
                                buf.into(),
                                cap.into(),
                                fmt.into(),
                                n.into(),
                            ];
                            bld(self.builder.build_call(self.snprintf, &call_args, "itoa"))?;
                            return Ok(GenValue::Scalar(buf.into()));
                        }
                        "substr" => {
                            if args.len() != 3 {
                                return Err(CodegenError {
                                    msg: "`substr` requires 3 arguments (string, start, end)".to_string(),
                                    line: span.line,
                                    col: span.col,
                                });
                            }
                            let s = self.gen_value(&args[0])?.scalar(*span, "substr string")?;
                            let start = self.gen_value(&args[1])?.scalar(*span, "substr start")?;
                            let end = self.gen_value(&args[2])?.scalar(*span, "substr end")?;
                            let s = s.into_pointer_value();
                            let start = start.into_int_value();
                            let end = end.into_int_value();
                            let i8_ty = self.context.i8_type();
                            let zero = self.i64_ty.const_zero();
                            let one = self.i64_ty.const_int(1, false);
                            let len = bld(self.builder.build_call(self.strlen, &[s.into()], "substr_len"))?
                                .try_as_basic_value()
                                .basic()
                                .ok_or_else(|| self.internal_err(*span, "strlen returned no value"))?
                                .into_int_value();
                            // Clamp start/end into [0, len], then force start <= end
                            // (reversed bounds yield an empty slice).
                            let start0 = bld(self.builder.build_int_compare(IntPredicate::SLT, start, zero, "sc0"))?;
                            let start1 = bld(self.builder.build_select::<IntValue, IntValue>(start0, zero, start, "sc1"))?.into_int_value();
                            let start2 = bld(self.builder.build_int_compare(IntPredicate::SGT, start1, len, "sc2"))?;
                            let startc = bld(self.builder.build_select::<IntValue, IntValue>(start2, len, start1, "sc3"))?.into_int_value();
                            let end0 = bld(self.builder.build_int_compare(IntPredicate::SLT, end, zero, "sc4"))?;
                            let end1 = bld(self.builder.build_select::<IntValue, IntValue>(end0, zero, end, "sc5"))?.into_int_value();
                            let end2 = bld(self.builder.build_int_compare(IntPredicate::SGT, end1, len, "sc6"))?;
                            let endc = bld(self.builder.build_select::<IntValue, IntValue>(end2, len, end1, "sc7"))?.into_int_value();
                            let rev = bld(self.builder.build_int_compare(IntPredicate::SGT, startc, endc, "sc8"))?;
                            let startf = bld(self.builder.build_select::<IntValue, IntValue>(rev, endc, startc, "sc9"))?.into_int_value();
                            let n = bld(self.builder.build_int_sub(endc, startf, "slice_len"))?;
                            let size = bld(self.builder.build_int_add(n, one, "slice_size"))?;
                            let buf = bld(self.builder.build_call(self.malloc, &[size.into()], "slice_buf"))?
                                .try_as_basic_value()
                                .basic()
                                .ok_or_else(|| self.internal_err(*span, "malloc returned no value"))?
                                .into_pointer_value();
                            let src = bld(unsafe {
                                self.builder.build_in_bounds_gep(i8_ty, s, &[startf], "slice_src")
                            })?;
                            bld(self.builder.build_call(
                                self.memcpy,
                                &[buf.into(), src.into(), n.into()],
                                "slice_copy",
                            ))?;
                            let nul = bld(unsafe {
                                self.builder.build_in_bounds_gep(i8_ty, buf, &[n], "slice_nul")
                            })?;
                            bld(self.builder.build_store(nul, i8_ty.const_zero()))?;
                            return Ok(GenValue::Scalar(buf.into()));
                        }
                        "str_to_int" => {
                            if args.len() != 1 {
                                return Err(CodegenError {
                                    msg: "`str_to_int` requires 1 string argument".to_string(),
                                    line: span.line,
                                    col: span.col,
                                });
                            }
                            let s = self.gen_value(&args[0])?.scalar(*span, "str_to_int argument")?;
                            let s = s.into_pointer_value();
                            // strtoll(s, NULL, 10): leading whitespace + optional sign are
                            // accepted; unparseable input yields 0.
                            let null_pp = self
                                .context
                                .i8_type()
                                .ptr_type(AddressSpace::from(0u16))
                                .const_null();
                            let base = self.i32_ty.const_int(10, false);
                            let v = bld(self.builder.build_call(
                                self.strtoll,
                                &[s.into(), null_pp.into(), base.into()],
                                "strtoll",
                            ))?
                            .try_as_basic_value()
                            .basic()
                            .ok_or_else(|| self.internal_err(*span, "strtoll returned no value"))?;
                            return Ok(GenValue::Scalar(v.into()));
                        }
                        "str_contains" => {
                            if args.len() != 2 {
                                return Err(CodegenError {
                                    msg: "`str_contains` requires 2 string arguments (haystack, needle)".to_string(),
                                    line: span.line,
                                    col: span.col,
                                });
                            }
                            let h = self.gen_value(&args[0])?.scalar(*span, "str_contains haystack")?;
                            let n = self.gen_value(&args[1])?.scalar(*span, "str_contains needle")?;
                            let h = h.into_pointer_value();
                            let n = n.into_pointer_value();
                            let r = bld(self.builder.build_call(self.strstr, &[h.into(), n.into()], "strstr"))?
                                .try_as_basic_value()
                                .basic()
                                .ok_or_else(|| self.internal_err(*span, "strstr returned no value"))?
                                .into_pointer_value();
                            let r_int = bld(self.builder.build_ptr_to_int(r, self.i64_ty, "sc_ptr"))?;
                            let zero = self.i64_ty.const_zero();
                            let found = bld(self.builder.build_int_compare(
                                IntPredicate::NE,
                                r_int,
                                zero,
                                "contains",
                            ))?;
                            return Ok(GenValue::Scalar(found.into()));
                        }
                        "str_find" => {
                            if args.len() != 2 {
                                return Err(CodegenError {
                                    msg: "`str_find` requires 2 string arguments (haystack, needle)".to_string(),
                                    line: span.line,
                                    col: span.col,
                                });
                            }
                            let h = self.gen_value(&args[0])?.scalar(*span, "str_find haystack")?;
                            let n = self.gen_value(&args[1])?.scalar(*span, "str_find needle")?;
                            let h = h.into_pointer_value();
                            let n = n.into_pointer_value();
                            let r = bld(self.builder.build_call(self.strstr, &[h.into(), n.into()], "strstr"))?
                                .try_as_basic_value()
                                .basic()
                                .ok_or_else(|| self.internal_err(*span, "strstr returned no value"))?
                                .into_pointer_value();
                            let r_int = bld(self.builder.build_ptr_to_int(r, self.i64_ty, "sc_ptr"))?;
                            let h_int = bld(self.builder.build_ptr_to_int(h, self.i64_ty, "sc_hptr"))?;
                            let diff = bld(self.builder.build_int_sub(r_int, h_int, "find_diff"))?;
                            let zero = self.i64_ty.const_zero();
                            let is_null = bld(self.builder.build_int_compare(
                                IntPredicate::EQ,
                                r_int,
                                zero,
                                "find_null",
                            ))?;
                            let minus_one = self.i64_ty.const_int(u64::MAX, false);
                            let res = bld(self.builder.build_select::<IntValue, IntValue>(
                                is_null,
                                minus_one,
                                diff,
                                "find",
                            ))?;
                            return Ok(GenValue::Scalar(res.into()));
                        }
                        "str_cmp" => {
                            if args.len() != 2 {
                                return Err(CodegenError {
                                    msg: "`str_cmp` requires 2 string arguments".to_string(),
                                    line: span.line,
                                    col: span.col,
                                });
                            }
                            let a = self.gen_value(&args[0])?.scalar(*span, "str_cmp a")?;
                            let b = self.gen_value(&args[1])?.scalar(*span, "str_cmp b")?;
                            let a = a.into_pointer_value();
                            let b = b.into_pointer_value();
                            let cmp = bld(self.builder.build_call(self.strcmp, &[a.into(), b.into()], "strcmp"))?
                                .try_as_basic_value()
                                .basic()
                                .ok_or_else(|| self.internal_err(*span, "strcmp returned no value"))?;
                            let v = bld(self.builder.build_int_s_extend(
                                cmp.into_int_value(),
                                self.i64_ty,
                                "scmp",
                            ))?;
                            return Ok(GenValue::Scalar(v.into()));
                        }
                        _ => {
                            // Builtin asserts have no return value; cannot be used as expressions
                            return Err(CodegenError {
                                msg: format!("builtin function `{}` has no return value and cannot be used as an expression", hir_f.name),
                                line: span.line,
                                col: span.col,
                            });
                        }
                    }
                }
                // Generic call: dispatched to the concrete instance via monomorphization
    // (one instance per call site)
                let func = if !hir_f.type_params.is_empty() {
                    let type_args = self.resolve_call_instance(*span, hir_f)?;
                    self.gen_instance(*def_id, type_args)?
                } else {
                    *self
                        .funcs
                        .get(*def_id as usize)
                        .ok_or_else(|| self.internal_err(*span, "missing function table"))?
                };
                let mut call_args = Vec::new();
                let param_tys = func.get_type().get_param_types();
                for (i, arg) in args.iter().enumerate() {
                    let v = self.gen_value(arg)?;
                    let pt: BasicTypeEnum = param_tys[i]
                        .try_into()
                        .map_err(|_| self.internal_err(*span, "parameter type mismatch"))?;
                    let v = self.call_arg(v, &pt, *span, "function argument")?;
                    call_args.push(v.into());
                }
                let out = bld(self.builder.build_call(func, &call_args, "call"))?;
                match out.try_as_basic_value().basic() {
                    Some(v) => {
                        // Aggregate return (array/tuple): store into a temp slot and return an Agg pointer,
                        // so `let p = make_pair(...)` can keep using it as an aggregate
                        let is_agg = matches!(
                            v.get_type(),
                            BasicTypeEnum::ArrayType(_) | BasicTypeEnum::StructType(_)
                        );
                        if is_agg {
                            let tmp = bld(self.builder.build_alloca(v.get_type(), "call_ret"))?;
                            bld(self.builder.build_store(tmp, v))?;
                            Ok(GenValue::Agg(tmp))
                        } else {
                            Ok(GenValue::Scalar(v))
                        }
                    }
                    None => Err(self.internal_err(*span, "void function call used as an expression")),
                }
            }
        }
    }

    /// Index access: `target[index]`. Arrays support dynamic indices; tuples only constant
    /// indices; tensors support any dimension (sub-tensor or scalar element).
    fn gen_index(
        &mut self,
        target: &HirExpr,
        index: &HirExpr,
        span: Span,
    ) -> Result<GenValue<'ctx>, CodegenError> {
        // Tensor indexing: target static type is Tensor (incl. sub-tensors from nested indexing)
        if matches!(self.expr_ty(target), Ok(Ty::Tensor { .. })) {
            return self.gen_tensor_index(target, index, span);
        }
        match target {
            HirExpr::Var(def_id, _) => {
                let ty = self
                    .var_tys
                    .get(def_id)
                    .ok_or_else(|| self.internal_err(span, "missing type for index target"))?
                    .clone();
                let ptr = *self
                    .vars
                    .get(def_id)
                    .ok_or_else(|| self.internal_err(span, "index target not allocated"))?;
                match &ty {
                    Ty::Str => {
                        // s[i]: the stack slot holds an i8*; load it first, then index the bytes
                        let idx = self.gen_value(index)?.scalar(span, "index")?;
                        let idx = self.coerce(idx, &self.i64_ty.into(), span, "index")?;
                        let slot_ty = self.t(&ty, span)?;
                        let s_ptr = bld(self.builder.build_load(slot_ty, ptr, "sptr"))?
                            .into_pointer_value();
                        let slot = bld(unsafe {
                            self.builder.build_in_bounds_gep(
                                self.context.i8_type(),
                                s_ptr,
                                &[idx.into_int_value()],
                                "sidx",
                            )
                        })?;
                        let b = bld(self.builder.build_load(
                            self.context.i8_type(),
                            slot,
                            "sload",
                        ))?;
                        let v = bld(self.builder.build_int_z_extend(
                            b.into_int_value(),
                            self.i64_ty,
                            "szext",
                        ))?;
                        Ok(GenValue::Scalar(v.into()))
                    }
                    Ty::Array(elem, _) => {
                        let elem_ty = self.t(elem, span)?;
                        let arr_ty = self.t(&ty, span)?;
                        let v = self.gen_array_index(arr_ty, ptr, elem_ty, index, span)?;
                        Ok(GenValue::Scalar(v))
                    }
                    Ty::Ptr(elem) => {
                        let elem_ty = self.t(elem, span)?;
                        let idx = self.gen_value(index)?.scalar(span, "index")?;
                        let idx = self.coerce(idx, &self.i64_ty.into(), span, "index")?;
                        let slot = bld(unsafe {
                            self.builder.build_in_bounds_gep(
                                elem_ty,
                                ptr,
                                &[idx.into_int_value()],
                                "pidx",
                            )
                        })?;
                        let v = bld(self.builder.build_load(elem_ty, slot, "pload"))?;
                        Ok(GenValue::Scalar(v))
                    }
                    Ty::Tuple(elems) => {
                        let k = self.const_index(index, span)?;
                        if k >= elems.len() {
                            return Err(CodegenError {
                                msg: format!("tuple index {k} out of bounds (length {})", elems.len()),
                                line: span.line,
                                col: span.col,
                            });
                        }
                        let elem_ty = self.t(&elems[k], span)?;
                        let slot_ty = self.t(&ty, span)?;
                        let slot = bld(unsafe {
                            self.builder.build_in_bounds_gep(
                                slot_ty,
                                ptr,
                                &[
                                    self.i32_ty.const_zero(),
                                    self.i32_ty.const_int(k as u64, false),
                                ],
                                "tidx",
                            )
                        })?;
                        let v = bld(self.builder.build_load(elem_ty, slot, "tload"))?;
                        Ok(GenValue::Scalar(v))
                    }
                    other => Err(CodegenError {
                        msg: format!("cannot index into type `{other}`"),
                        line: span.line,
                        col: span.col,
                    }),
                }
            }
            HirExpr::Array(elems, _) => {
                // Array-literal indexing: [1,2,3][0]
                let tmp = self.gen_value(target)?.agg(span, "index")?;
                let arr_ty = self.i64_ty.array_type(elems.len() as u32);
                let v = self.gen_array_index(
                    arr_ty.into(),
                    tmp,
                    self.i64_ty.into(),
                    index,
                    span,
                )?;
                Ok(GenValue::Scalar(v))
            }
            other => Err(CodegenError {
                msg: "only variables and array literals can be indexed".to_string(),
                line: other.span().line,
                col: other.span().col,
            }),
        }
    }

    /// Array index: GEP [0, index] then load.
    fn gen_array_index(
        &mut self,
        arr_ty: BasicTypeEnum<'ctx>,
        ptr: PointerValue<'ctx>,
        elem_ty: BasicTypeEnum<'ctx>,
        index: &HirExpr,
        span: Span,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let idx = self.gen_value(index)?.scalar(span, "array index")?;
        let idx = self.coerce(idx, &self.i64_ty.into(), span, "array index")?;
        let slot = bld(unsafe {
            self.builder.build_in_bounds_gep(
                arr_ty,
                ptr,
                &[self.i32_ty.const_zero(), idx.into_int_value()],
                "aidx",
            )
        })?;
        bld(self.builder.build_load(elem_ty, slot, "aload"))
    }

    /// Tensor index: `a[i]` GEPs one layer of the current level (full tensor or sub-tensor).
    /// The last layer returns a scalar element; otherwise a sub-tensor Agg slot.
    fn gen_tensor_index(
        &mut self,
        target: &HirExpr,
        index: &HirExpr,
        span: Span,
    ) -> Result<GenValue<'ctx>, CodegenError> {
        let ty = self.expr_ty(target)?;
        let (shape, elem) = match &ty {
            Ty::Tensor { shape, elem } => (shape.clone(), (**elem).clone()),
            other => {
                return Err(CodegenError {
                    msg: format!("invalid tensor index target type `{other}`"),
                    line: span.line,
                    col: span.col,
                });
            }
        };
        let ptr = match target {
            HirExpr::Var(def_id, _) => *self
                .vars
                .get(def_id)
                .ok_or_else(|| self.internal_err(span, "tensor index target not allocated"))?,
            other => self.gen_value(other)?.agg(span, "tensor index")?,
        };
        let arr_ty = self.t(&ty, span)?;
        let idx = self.gen_value(index)?.scalar(span, "tensor index")?;
        let idx = self.coerce(idx, &self.i64_ty.into(), span, "tensor index")?;
        let slot = bld(unsafe {
            self.builder.build_in_bounds_gep(
                arr_ty,
                ptr,
                &[self.i32_ty.const_zero(), idx.into_int_value()],
                "tidx",
            )
        })?;
        if shape.len() == 1 {
            // Last layer: load the scalar element
            let elem_ty = self.t(&elem, span)?;
            let v = bld(self.builder.build_load(elem_ty, slot, "tload"))?;
            Ok(GenValue::Scalar(v))
        } else {
            // Sub-tensor: return the Agg slot pointing at the remaining dimensions
            Ok(GenValue::Agg(slot))
        }
    }

    /// Recursively zero every element of an aggregate (tensor literals are zero-initialized).
    fn store_zero_agg(
        &mut self,
        ptr: PointerValue<'ctx>,
        ty: &Ty,
        span: Span,
    ) -> Result<(), CodegenError> {
        match ty {
            Ty::Tensor { elem, shape } if shape.is_empty() => self.store_zero_agg(ptr, elem, span),
            Ty::Tensor { elem, shape } => {
                let llvm = self.t(ty, span)?;
                for i in 0..shape[0] {
                    let idx = self.i32_ty.const_int(i as u64, false);
                    let sub = bld(unsafe {
                        self.builder.build_in_bounds_gep(
                            llvm,
                            ptr,
                            &[self.i32_ty.const_zero(), idx],
                            "tz",
                        )
                    })?;
                    let sub_ty = Ty::Tensor {
                        elem: elem.clone(),
                        shape: shape[1..].to_vec(),
                    };
                    self.store_zero_agg(sub, &sub_ty, span)?;
                }
                Ok(())
            }
            Ty::I64 => {
                bld(self.builder.build_store(ptr, self.i64_ty.const_zero()))?;
                Ok(())
            }
            other => Err(CodegenError {
                msg: format!("tensor element type `{other}` not supported (only i64)"),
                line: span.line,
                col: span.col,
            }),
        }
    }

    /// Matrix multiply `matmul(a, b)`: a is [M, K], b is [K, N], result [M, N].
    /// Emits a triple loop (i rows, j columns, k reduction) computing and writing
    fn gen_matmul(&mut self, expr: &HirExpr) -> Result<GenValue<'ctx>, CodegenError> {
        let (lhs, rhs, span) = match expr {
            HirExpr::Matmul { lhs, rhs, span } => (lhs, rhs, *span),
            _ => return Err(self.internal_err(expr.span(), "invalid matmul node type")),
        };
        let lt = self.expr_ty(lhs)?;
        let rt = self.expr_ty(rhs)?;
        let (m, k) = match &lt {
            Ty::Tensor { shape, .. } => (shape[0], shape[1]),
            _ => return Err(self.internal_err(span, "invalid matmul left operand type")),
        };
        let (_, n) = match &rt {
            Ty::Tensor { shape, .. } => (shape[0], shape[1]),
            _ => return Err(self.internal_err(span, "invalid matmul right operand type")),
        };
        // Operand aggregate slots: fixed outside the loops (no repeated temp alloca inside)
        let lhs_ptr = self.gen_value(lhs)?.agg(span, "matmul left matrix")?;
        let lhs_ty = self.t(&lt, span)?;
        let rhs_ty = self.t(&rt, span)?;

        let rhs_ptr = self.gen_value(rhs)?.agg(span, "matmul right matrix")?;
        let res_ty = Ty::Tensor {
            elem: Box::new(Ty::I64),
            shape: vec![m, n],
        };
        let res_llvm = self.t(&res_ty, span)?;
        let res = bld(self.builder.build_alloca(res_llvm, "matmul"))?;

        // Result [M x [N x i64]]
        let i_slot = bld(self.builder.build_alloca(self.i64_ty, "mm_i"))?;
        let j_slot = bld(self.builder.build_alloca(self.i64_ty, "mm_j"))?;
        let k_slot = bld(self.builder.build_alloca(self.i64_ty, "mm_k"))?;
        let sum_slot = bld(self.builder.build_alloca(self.i64_ty, "mm_sum"))?;
        let zero = self.i64_ty.const_zero();
        let one = self.i64_ty.const_int(1, false);
        let m_c = self.i64_ty.const_int(m as u64, false);
        let n_c = self.i64_ty.const_int(n as u64, false);
        let k_c = self.i64_ty.const_int(k as u64, false);

        let i_cond = self.context.append_basic_block(self.cur_func, "mm_i_cond");
        let i_body = self.context.append_basic_block(self.cur_func, "mm_i_body");
        let j_cond = self.context.append_basic_block(self.cur_func, "mm_j_cond");
        let j_body = self.context.append_basic_block(self.cur_func, "mm_j_body");
        let k_cond = self.context.append_basic_block(self.cur_func, "mm_k_cond");
        let k_body = self.context.append_basic_block(self.cur_func, "mm_k_body");
        let k_end = self.context.append_basic_block(self.cur_func, "mm_k_end");
        let j_inc = self.context.append_basic_block(self.cur_func, "mm_j_inc");
        let mm_end = self.context.append_basic_block(self.cur_func, "mm_end");

        bld(self.builder.build_store(i_slot, zero))?;
        bld(self.builder.build_unconditional_branch(i_cond))?;

        // Loop variable slots
        self.builder.position_at_end(i_cond);
        let iv = bld(self.builder.build_load(self.i64_ty, i_slot, "mm_il"))?
            .into_int_value();
        let i_lt = bld(self.builder.build_int_compare(IntPredicate::SLT, iv, m_c, "mm_ilt"))?;
        bld(self.builder.build_conditional_branch(i_lt, i_body, mm_end))?;

        self.builder.position_at_end(i_body);
        bld(self.builder.build_store(j_slot, zero))?;
        bld(self.builder.build_unconditional_branch(j_cond))?;

        // i loop condition
        self.builder.position_at_end(j_cond);
        let jv = bld(self.builder.build_load(self.i64_ty, j_slot, "mm_jl"))?
            .into_int_value();
        let j_lt = bld(self.builder.build_int_compare(IntPredicate::SLT, jv, n_c, "mm_jlt"))?;
        bld(self.builder.build_conditional_branch(j_lt, j_body, j_inc))?;

        self.builder.position_at_end(j_body);
        bld(self.builder.build_store(sum_slot, zero))?;
        bld(self.builder.build_store(k_slot, zero))?;
        bld(self.builder.build_unconditional_branch(k_cond))?;

        // j loop condition
        self.builder.position_at_end(k_cond);
        let kv = bld(self.builder.build_load(self.i64_ty, k_slot, "mm_kl"))?
            .into_int_value();
        let k_lt = bld(self.builder.build_int_compare(IntPredicate::SLT, kv, k_c, "mm_klt"))?;
        bld(self.builder.build_conditional_branch(k_lt, k_body, k_end))?;

        // k loop condition
        self.builder.position_at_end(k_body);
        let iv2 = bld(self.builder.build_load(self.i64_ty, i_slot, "mm_il2"))?
            .into_int_value();
        let jv2 = bld(self.builder.build_load(self.i64_ty, j_slot, "mm_jl2"))?
            .into_int_value();
        let kv2 = bld(self.builder.build_load(self.i64_ty, k_slot, "mm_kl2"))?
            .into_int_value();
        let a_elem = bld(unsafe {
            self.builder.build_in_bounds_gep(
                lhs_ty,
                lhs_ptr,
                &[self.i32_ty.const_zero(), iv2, kv2],
                "mm_a",
            )
        })?;
        let av = bld(self.builder.build_load(self.i64_ty, a_elem, "mm_al"))?
            .into_int_value();
        let b_elem = bld(unsafe {
            self.builder.build_in_bounds_gep(
                rhs_ty,
                rhs_ptr,
                &[self.i32_ty.const_zero(), kv2, jv2],
                "mm_b",
            )
        })?;
        let bv = bld(self.builder.build_load(self.i64_ty, b_elem, "mm_bl"))?
            .into_int_value();
        let prod = bld(self.builder.build_int_mul(av, bv, "mm_mul"))?;
        let sumv = bld(self.builder.build_load(self.i64_ty, sum_slot, "mm_sl"))?
            .into_int_value();
        let new_sum = bld(self.builder.build_int_add(sumv, prod, "mm_add"))?;
        bld(self.builder.build_store(sum_slot, new_sum))?;
        let nk = bld(self.builder.build_int_add(kv2, one, "mm_kinc"))?;
        bld(self.builder.build_store(k_slot, nk))?;
        bld(self.builder.build_unconditional_branch(k_cond))?;

        // k loop body: sum += a[i][k] * b[k][j]
        self.builder.position_at_end(k_end);
        let iv3 = bld(self.builder.build_load(self.i64_ty, i_slot, "mm_il3"))?
            .into_int_value();
        let jv3 = bld(self.builder.build_load(self.i64_ty, j_slot, "mm_jl3"))?
            .into_int_value();
        let c_elem = bld(unsafe {
            self.builder.build_in_bounds_gep(
                res_llvm,
                res,
                &[self.i32_ty.const_zero(), iv3, jv3],
                "mm_c",
            )
        })?;
        let sumv2 = bld(self.builder.build_load(self.i64_ty, sum_slot, "mm_sl2"))?
            .into_int_value();
        bld(self.builder.build_store(c_elem, sumv2))?;
        let nj = bld(self.builder.build_int_add(jv3, one, "mm_jinc"))?;
        bld(self.builder.build_store(j_slot, nj))?;
        bld(self.builder.build_unconditional_branch(j_cond))?;

        // k end: c[i][j] = sum; j++
        self.builder.position_at_end(j_inc);
        let iv4 = bld(self.builder.build_load(self.i64_ty, i_slot, "mm_il4"))?
            .into_int_value();
        let ni = bld(self.builder.build_int_add(iv4, one, "mm_iinc"))?;
        bld(self.builder.build_store(i_slot, ni))?;
        bld(self.builder.build_unconditional_branch(i_cond))?;

        self.builder.position_at_end(mm_end);
        Ok(GenValue::Agg(res))
    }

    // j increment: i++
    fn const_index(&self, index: &HirExpr, span: Span) -> Result<usize, CodegenError> {
        match index {
            HirExpr::IntLit(k, _) if *k >= 0 => Ok(*k as usize),
            _ => Err(CodegenError {
                msg: "tuple index must be an integer constant".to_string(),
                line: span.line,
                col: span.col,
            }),
        }
    }

    /// Element pointer for an index write: `target[index] = v`.
    /// Returns (element slot ptr, element LLVM type). Supports variables and array literals.
    fn gen_index_ptr(
        &mut self,
        target: &HirExpr,
        index: &HirExpr,
        span: Span,
    ) -> Result<(PointerValue<'ctx>, BasicTypeEnum<'ctx>), CodegenError> {
        // Tensor index write: a[i] sub-tensor / a[i][j] element slot
        if matches!(self.expr_ty(target), Ok(Ty::Tensor { .. })) {
            let ty = self.expr_ty(target)?;
            let elem = match &ty {
                Ty::Tensor { elem, .. } => (**elem).clone(),
                _ => return Err(self.internal_err(span, "invalid tensor index-write target type")),
            };
            let ptr = match target {
                HirExpr::Var(def_id, _) => *self
                    .vars
                    .get(def_id)
                    .ok_or_else(|| self.internal_err(span, "tensor index-write target not allocated"))?,
                other => self.gen_value(other)?.agg(span, "tensor index write")?,
            };
            let arr_ty = self.t(&ty, span)?;
            let idx = self.gen_value(index)?.scalar(span, "tensor index write")?;
            let idx = self.coerce(idx, &self.i64_ty.into(), span, "tensor index write")?;
            let slot = bld(unsafe {
                self.builder.build_in_bounds_gep(
                    arr_ty,
                    ptr,
                    &[self.i32_ty.const_zero(), idx.into_int_value()],
                    "tidxw",
                )
            })?;
            let elem_ty = self.t(&elem, span)?;
            return Ok((slot, elem_ty));
        }
        match target {
            HirExpr::Var(def_id, _) => {
                let ty = self
                    .var_tys
                    .get(def_id)
                    .ok_or_else(|| self.internal_err(span, "missing type for index target"))?
                    .clone();
                let ptr = *self
                    .vars
                    .get(def_id)
                    .ok_or_else(|| self.internal_err(span, "index target not allocated"))?;
                match &ty {
                    Ty::Array(elem, _) => {
                        let elem_ty = self.t(elem, span)?;
                        let arr_ty = self.t(&ty, span)?;
                        let idx = self.gen_value(index)?.scalar(span, "index write")?;
                        let idx = self.coerce(idx, &self.i64_ty.into(), span, "index write")?;
                        let slot = bld(unsafe {
                            self.builder.build_in_bounds_gep(
                                arr_ty,
                                ptr,
                                &[self.i32_ty.const_zero(), idx.into_int_value()],
                                "aidxw",
                            )
                        })?;
                        Ok((slot, elem_ty))
                    }
                    Ty::Ptr(elem) => {
                        let elem_ty = self.t(elem, span)?;
                        let idx = self.gen_value(index)?.scalar(span, "index write")?;
                        let idx = self.coerce(idx, &self.i64_ty.into(), span, "index write")?;
                        let slot = bld(unsafe {
                            self.builder.build_in_bounds_gep(
                                elem_ty,
                                ptr,
                                &[idx.into_int_value()],
                                "pidxw",
                            )
                        })?;
                        Ok((slot, elem_ty))
                    }
                    other => Err(CodegenError {
                        msg: format!("cannot index-write into type `{other}`"),
                        line: span.line,
                        col: span.col,
                    }),
                }
            }
            HirExpr::Array(elems, _) => {
                // Array-literal index write: temp slot (elements assumed i64, matching gen_value literals)
                let tmp = self.gen_value(target)?.agg(span, "index write")?;
                let arr_ty = self.i64_ty.array_type(elems.len() as u32);
                let idx = self.gen_value(index)?.scalar(span, "index write")?;
                let idx = self.coerce(idx, &self.i64_ty.into(), span, "index write")?;
                let slot = bld(unsafe {
                    self.builder.build_in_bounds_gep(
                        arr_ty,
                        tmp,
                        &[self.i32_ty.const_zero(), idx.into_int_value()],
                        "aidxw",
                    )
                })?;
                Ok((slot, self.i64_ty.into()))
            }
            _ => Err(CodegenError {
                msg: "only variables and array literals can be index-written".to_string(),
                line: span.line,
                col: span.col,
            }),
        }
    }

    fn expr_ty(&self, expr: &HirExpr) -> Result<Ty, CodegenError> {
        match expr {
            HirExpr::IntLit(..) => Ok(Ty::I64),
            HirExpr::BoolLit(..) => Ok(Ty::Bool),
            HirExpr::StrLit(..) => Ok(Ty::Str),
            HirExpr::Var(def_id, span) => self
                .var_tys
                .get(def_id)
                .cloned()
                .ok_or_else(|| self.internal_err(*span, "missing variable type")),
            HirExpr::Borrow { def_id, mut_, span } => {
                let src = self
                    .var_tys
                    .get(def_id)
                    .cloned()
                    .ok_or_else(|| self.internal_err(*span, "missing variable type"))?;
                Ok(Ty::Ref {
                    mut_: *mut_,
                    inner: Box::new(src),
                })
            }
            HirExpr::Deref { target, span } => {
                let t = self.expr_ty(target)?;
                match t {
                    Ty::Ref { inner, .. } | Ty::Ptr(inner) => Ok(*inner),
                    other => Err(self.internal_err(
                        *span,
                        &format!("cannot dereference type `{other}`"),
                    )),
                }
            }
            HirExpr::MethodCall { method, span, .. } => match method.as_str() {
                "alloc" => Ok(Ty::Ptr(Box::new(Ty::I64))),
                "reset" => Ok(Ty::Void),
                other => Err(self.internal_err(*span, &format!("unknown method `{other}`"))),
            },
            HirExpr::ArenaLit(n, _) => Ok(Ty::Arena(*n)),
            HirExpr::TensorLit(dims, _) => Ok(Ty::Tensor {
                elem: Box::new(Ty::I64),
                shape: dims.clone(),
            }),
            HirExpr::Matmul { lhs, rhs, span } => {
                let lt = self.expr_ty(lhs)?;
                let rt = self.expr_ty(rhs)?;
                match (&lt, &rt) {
                    (Ty::Tensor { shape: s1, elem }, Ty::Tensor { shape: s2, .. })
                        if s1.len() == 2 && s2.len() == 2 =>
                    {
                        Ok(Ty::Tensor {
                            elem: elem.clone(),
                            shape: vec![s1[0], s2[1]],
                        })
                    }
                    _other => Err(self.internal_err(*span, &format!("invalid matmul operand type"))),
                }
            }
            HirExpr::Tuple(elems, _) => {
                let mut tys = Vec::new();
                for e in elems {
                    tys.push(self.expr_ty(e)?);
                }
                Ok(Ty::Tuple(tys))
            }
            HirExpr::Array(elems, _) => {
                if elems.is_empty() {
                    return Err(self.internal_err(expr.span(), "empty array cannot determine element type"));
                }
                let elem = self.expr_ty(&elems[0])?;
                Ok(Ty::Array(Box::new(elem), elems.len()))
            }
            HirExpr::Index {
                target,
                index: _,
                span,
            } => {
                let t = self.expr_ty(target)?;
                match t {
                    Ty::Array(elem, _) | Ty::Ptr(elem) => Ok(*elem),
                    Ty::Str => Ok(Ty::I64),
                    Ty::Tensor { shape, elem } => {
                        if shape.len() == 1 {
                            Ok(*elem)
                        } else {
                            Ok(Ty::Tensor {
                                elem: elem.clone(),
                                shape: shape[1..].to_vec(),
                            })
                        }
                    }
                    Ty::Tuple(elems) => Ok(elems.first().cloned().unwrap_or(Ty::I64)),
                    other => Err(self.internal_err(*span, &format!("cannot index into type `{other}`"))),
                }
            }
            HirExpr::Unary { expr, .. } => self.expr_ty(expr),
            HirExpr::Binary { lhs, .. } => self.expr_ty(lhs),
            HirExpr::Cmp { .. } => Ok(Ty::Bool),
            HirExpr::Logic { .. } => Ok(Ty::Bool),
            HirExpr::Call { def_id, span, .. } => {
                let f = self
                    .hir_funcs
                    .get(*def_id as usize)
                    .ok_or_else(|| self.internal_err(*span, "missing function table"))?;
                match &f.ret {
                    Some(ret) => {
                        // Generic call: return the instantiated type (type args from infer call_types,
                        // then apply the current instance context to resolve nested generics)
                        if !f.type_params.is_empty() {
                            if let Some(type_args) = self.call_types.get(&span.start) {
                                if type_args.len() == f.type_params.len() {
                                    let mut merged = self.type_subst.clone();
                                    for (name, concrete) in
                                        f.type_params.iter().zip(type_args.iter())
                                    {
                                        merged.insert(name.clone(), concrete.clone());
                                    }
                                    return Ok(substitute(ret, &merged));
                                }
                            }
                        }
                        Ok(ret.clone())
                    }
                    None => Ok(Ty::Void),
                }
            }
        }
    }

    /// Static type of a deref target: `*p` yields the reference/pointer inner type of `p`.
    fn deref_inner_ty(&self, expr: &HirExpr, span: Span) -> Result<Ty, CodegenError> {
        let t = self.expr_ty(expr)?;
        match t {
            Ty::Ref { inner, .. } | Ty::Ptr(inner) => Ok(*inner),
            other => Err(CodegenError {
                msg: format!("cannot dereference type `{other}`"),
                line: span.line,
                col: span.col,
            }),
        }
    }

    /// Target pointer of a deref write: `*p = v`. Returns (target ptr, inner Aero type).
    fn deref_ptr(
        &mut self,
        expr: &HirExpr,
        span: Span,
    ) -> Result<(PointerValue<'ctx>, Ty), CodegenError> {
        let inner_ty = self.deref_inner_ty(expr, span)?;
        let v = self.gen_value(expr)?.scalar(span, "deref write")?;
        Ok((v.into_pointer_value(), inner_ty))
    }

    /// Method-call codegen: arena `alloc(n)` (returns an `i64*` to the slot) and `reset()`.
    /// `None` means no return value (reset; statement-only).
    fn gen_method_call(
        &mut self,
        expr: &HirExpr,
    ) -> Result<Option<GenValue<'ctx>>, CodegenError> {
        let (recv, method, args, span) = match expr {
            HirExpr::MethodCall {
                recv,
                method,
                args,
                span,
            } => (recv, method, args, *span),
            _ => return Err(self.internal_err(expr.span(), "invalid method-call node type")),
        };
        let recv_def = match &**recv {
            HirExpr::Var(def_id, _) => *def_id,
            _ => return Err(self.internal_err(span, "method-call receiver must be an arena variable")),
        };
        let slots = self
            .arenas
            .get(&recv_def)
            .copied()
            .ok_or_else(|| self.internal_err(span, "arena variable has no internal slots"))?;
        match method.as_str() {
            "alloc" => {
                if args.len() != 1 {
                    return Err(CodegenError {
                        msg: "`alloc` requires 1 argument (slot count)".to_string(),
                        line: span.line,
                        col: span.col,
                    });
                }
                let n = self.gen_value(&args[0])?.scalar(span, "alloc slot count")?;
                let n = self.coerce(n, &self.i64_ty.into(), span, "alloc slot count")?;
                let off = bld(self.builder.build_load(self.i64_ty, slots.offset, "aoff"))?
                    .into_int_value();
                let n8 = bld(self.builder.build_int_mul(
                    n.into_int_value(),
                    self.i64_ty.const_int(8, false),
                    "an8",
                ))?;
                let new_off = bld(self.builder.build_int_add(off, n8, "anew"))?;
                // Bounds check: new_off <= capacity, otherwise abort
                let ok_bb = self.context.append_basic_block(self.cur_func, "alloc_ok");
                let abort_bb = self.context.append_basic_block(self.cur_func, "alloc_abort");
                let cap = self.i64_ty.const_int(slots.capacity, false);
                let ok = bld(self.builder.build_int_compare(
                    IntPredicate::ULE,
                    new_off,
                    cap,
                    "acap",
                ))?;
                bld(self.builder.build_conditional_branch(ok, ok_bb, abort_bb))?;
                self.builder.position_at_end(abort_bb);
                bld(self.builder.build_call(self.abort, &[], "abort"))?;
                bld(self.builder.build_unreachable())?;
                self.builder.position_at_end(ok_bb);
                // slot = buf + off (offset in the byte pool): GEP on [N x i8], then cast to i64*
                let buf_ty = self.context.i8_type().array_type(slots.capacity as u32);
                let slot = bld(unsafe {
                    self.builder.build_in_bounds_gep(
                        buf_ty,
                        slots.buf,
                        &[self.i32_ty.const_zero(), off],
                        "aslot",
                    )
                })?;
                bld(self.builder.build_store(slots.offset, new_off))?;
                let slot64 = bld(
                    self.builder
                        .build_pointer_cast(slot, self.context.ptr_type(AddressSpace::from(0u16)), "aslot64"),
                )?;
                Ok(Some(GenValue::Scalar(slot64.as_basic_value_enum())))
            }
            "reset" => {
                bld(self.builder.build_store(slots.offset, self.i64_ty.const_zero()))?;
                Ok(None)
            }
            other => Err(CodegenError {
                msg: format!("arena has no method `{other}`"),
                line: span.line,
                col: span.col,
            }),
        }
    }

    /// Generate condition code (i1): comparisons → icmp; logic → short-circuit;
    /// other scalars → compare with 0.
    fn gen_cond(&mut self, expr: &HirExpr) -> Result<IntValue<'ctx>, CodegenError> {
        match expr {
            HirExpr::Cmp { .. } => match self.gen_value(expr)? {
                GenValue::Scalar(v) => Ok(v.into_int_value()),
                _ => unreachable!("comparison result is always a scalar"),
            },
            HirExpr::Logic {
                op: LogicOp::And,
                lhs,
                rhs,
                ..
            } => self.gen_short_circuit(lhs, rhs, true),
            HirExpr::Logic {
                op: LogicOp::Or,
                lhs,
                rhs,
                ..
            } => self.gen_short_circuit(lhs, rhs, false),
            other => {
                let v = self.gen_value(other)?.scalar(expr.span(), "condition")?;
                let iv = v.into_int_value();
                let zero = iv.get_type().const_zero();
                bld(self.builder.build_int_compare(IntPredicate::NE, iv, zero, "ne_zero"))
            }
        }
    }

    fn gen_short_circuit(
        &mut self,
        lhs: &HirExpr,
        rhs: &HirExpr,
        is_and: bool,
    ) -> Result<IntValue<'ctx>, CodegenError> {
        let l = self.gen_cond(lhs)?;
        let cond_block = self
            .builder
            .get_insert_block()
            .expect("an insertion block must exist before a conditional branch");
        let rhs_bb = self.context.append_basic_block(self.cur_func, "sc_rhs");
        let merge_bb = self.context.append_basic_block(self.cur_func, "sc_merge");

        if is_and {
            bld(self.builder.build_conditional_branch(l, rhs_bb, merge_bb))?;
        } else {
            bld(self.builder.build_conditional_branch(l, merge_bb, rhs_bb))?;
        }
        self.builder.position_at_end(rhs_bb);
        let r = self.gen_cond(rhs)?;
        bld(self.builder.build_unconditional_branch(merge_bb))?;
        self.builder.position_at_end(merge_bb);

        let phi = bld(self.builder.build_phi(
            self.context.bool_type(),
            if is_and { "and" } else { "or" },
        ))?;
        let short_val = self
            .context
            .bool_type()
            .const_int(if is_and { 0 } else { 1 }, false);
        phi.add_incoming(&[
            (&short_val.as_basic_value_enum(), cond_block),
            (&r.as_basic_value_enum(), rhs_bb),
        ]);
        Ok(phi.as_basic_value().into_int_value())
    }

    fn gen_if(
        &mut self,
        cond: &HirExpr,
        then_body: &HirBlock,
        else_body: &HirBlock,
    ) -> Result<(), CodegenError> {
        let c = self.gen_cond(cond)?;
        let then_bb = self.context.append_basic_block(self.cur_func, "then");
        let merge_bb = self.context.append_basic_block(self.cur_func, "merge");
        let mut merged = false;

        if else_body.stmts.is_empty() {
            bld(self.builder.build_conditional_branch(c, then_bb, merge_bb))?;
            // The false path flows straight into merge, so it always has a live predecessor
            merged = true;
            self.builder.position_at_end(then_bb);
            self.gen_block(then_body)?;
            if !self.cur_block_terminated() {
                bld(self.builder.build_unconditional_branch(merge_bb))?;
            }
        } else {
            let else_bb = self.context.append_basic_block(self.cur_func, "else");
            bld(self.builder.build_conditional_branch(c, then_bb, else_bb))?;
            self.builder.position_at_end(then_bb);
            self.gen_block(then_body)?;
            if !self.cur_block_terminated() {
                bld(self.builder.build_unconditional_branch(merge_bb))?;
                merged = true;
            }
            self.builder.position_at_end(else_bb);
            self.gen_block(else_body)?;
            if !self.cur_block_terminated() {
                bld(self.builder.build_unconditional_branch(merge_bb))?;
                merged = true;
            }
        }
        self.builder.position_at_end(merge_bb);
        if !merged {
            // Both branches ended with return: merge is unreachable; insert unreachable terminator
            bld(self.builder.build_unreachable())?;
        }
        Ok(())
    }

    fn gen_while(&mut self, cond: &HirExpr, body: &HirBlock) -> Result<(), CodegenError> {
        let cond_bb = self.context.append_basic_block(self.cur_func, "cond");
        let body_bb = self.context.append_basic_block(self.cur_func, "body");
        let merge_bb = self.context.append_basic_block(self.cur_func, "merge");

        bld(self.builder.build_unconditional_branch(cond_bb))?;
        self.builder.position_at_end(cond_bb);
        let c = self.gen_cond(cond)?;
        bld(self.builder.build_conditional_branch(c, body_bb, merge_bb))?;
        self.builder.position_at_end(body_bb);
        self.gen_block(body)?;
        if !self.cur_block_terminated() {
            bld(self.builder.build_unconditional_branch(cond_bb))?;
        }
        self.builder.position_at_end(merge_bb);
        Ok(())
    }

    // Both branches ended with return: merge is unreachable; insert an unreachable terminator
    /// Runtime string concatenation: malloc(len(a)+len(b)+1), memcpy both parts
    /// (the second copy includes the NUL terminator). Returns the new buffer;
    /// the caller owns it and releases it with `str_free`.
    fn gen_str_concat(
        &mut self,
        a: BasicValueEnum<'ctx>,
        b: BasicValueEnum<'ctx>,
        span: Span,
    ) -> Result<PointerValue<'ctx>, CodegenError> {
        let a = a.into_pointer_value();
        let b = b.into_pointer_value();
        let i8_ty = self.context.i8_type();
        let la = bld(self.builder.build_call(self.strlen, &[a.into()], "strlen_a"))?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| self.internal_err(span, "strlen returned no value"))?;
        let lb = bld(self.builder.build_call(self.strlen, &[b.into()], "strlen_b"))?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| self.internal_err(span, "strlen returned no value"))?;
        let one = self.i64_ty.const_int(1, false);
        let len = bld(self.builder.build_int_add(la.into_int_value(), lb.into_int_value(), "concat_len"))?;
        let size = bld(self.builder.build_int_add(len, one, "concat_size"))?;
        let buf = bld(self.builder.build_call(self.malloc, &[size.into()], "concat_buf"))?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| self.internal_err(span, "malloc returned no value"))?
            .into_pointer_value();
        // memcpy(buf, a, la)
        bld(self.builder.build_call(
            self.memcpy,
            &[buf.into(), a.into(), la.into()],
            "copy_a",
        ))?;
        // memcpy(buf + la, b, lb + 1)  (includes the NUL terminator)
        let dest = bld(unsafe {
            self.builder
                .build_in_bounds_gep(i8_ty, buf, &[la.into_int_value()], "copy_dest")
        })?;
        let lb1 = bld(self.builder.build_int_add(lb.into_int_value(), one, "copy_len"))?;
        bld(self.builder.build_call(
            self.memcpy,
            &[dest.into(), b.into(), lb1.into()],
            "copy_b",
        ))?;
        Ok(buf)
    }

    /// Builtin assertion codegen: `assert(cond)` / `assert_eq(a, b)`.
    /// On failure, prints a line-numbered diagnostic and calls `abort()`; no return value.
    fn gen_builtin_call(
        &mut self,
        name: &str,
        args: &[HirExpr],
        span: Span,
    ) -> Result<(), CodegenError> {
        // str_free: release a malloc-allocated string buffer; no assertion machinery.
        if name == "str_free" {
            if args.len() != 1 {
                return Err(CodegenError {
                    msg: "`str_free` requires 1 string argument".to_string(),
                    line: span.line,
                    col: span.col,
                });
            }
            let s = self.gen_value(&args[0])?.scalar(span, "str_free argument")?;
            bld(self.builder.build_call(self.free, &[s.into()], "free"))?;
            return Ok(());
        }
        let cond: IntValue<'ctx> = match name {
            "assert" => {
                if args.len() != 1 {
                    return Err(CodegenError {
                        msg: "`assert` requires 1 boolean argument".to_string(),
                        line: span.line,
                        col: span.col,
                    });
                }
                self.gen_cond(&args[0])?
            }
            "assert_eq" => {
                if args.len() != 2 {
                    return Err(CodegenError {
                        msg: "`assert_eq` requires 2 integer arguments".to_string(),
                        line: span.line,
                        col: span.col,
                    });
                }
                let a = self.gen_value(&args[0])?.scalar(span, "assert_eq left operand")?;
                let a = self.coerce(a, &self.i64_ty.into(), span, "assert_eq left operand")?;
                let b = self.gen_value(&args[1])?.scalar(span, "assert_eq right operand")?;
                let b = self.coerce(b, &self.i64_ty.into(), span, "assert_eq right operand")?;
                bld(self.builder.build_int_compare(
                    IntPredicate::EQ,
                    a.into_int_value(),
                    b.into_int_value(),
                    "aeq",
                ))?
            }
            other => {
                return Err(self.internal_err(span, &format!("unknown builtin function `{other}`")));
            }
        };

        let fail = self.context.append_basic_block(self.cur_func, "assert_fail");
        let ok = self.context.append_basic_block(self.cur_func, "assert_ok");
        bld(self.builder.build_conditional_branch(cond, ok, fail))?;

        self.builder.position_at_end(fail);
        // fail block: print the diagnostic and terminate
        let msg = self.global_string(&format!("assertion failed (line {})\n", span.line))?;
        let call_args: [BasicMetadataValueEnum<'ctx>; 1] = [msg.into()];
        bld(self.builder.build_call(self.printf, &call_args, "assert_msg"))?;
        bld(self.builder.build_call(self.abort, &[], "abort"))?;
        // abort is noreturn; add unreachable to satisfy module verification
        bld(self.builder.build_unreachable())?;

        // ok block: continue with subsequent instructions
        self.builder.position_at_end(ok);
        Ok(())
    }

    fn gen_print(&mut self, args: &[HirExpr], span: Span) -> Result<(), CodegenError> {
        let first = args.first().ok_or_else(|| CodegenError {
            msg: "print requires at least one argument".to_string(),
            line: span.line,
            col: span.col,
        })?;

        // Single argument: string value expressions (e.g. the result of `identity("str")`)
        // use %s; other scalars (int/bool) print via printf("%lld\n", value)
        if args.len() == 1 && !matches!(first, HirExpr::StrLit(..)) {
            let ty = self.expr_ty(first)?;
            if ty == Ty::Str {
                let v = self.gen_value(first)?.scalar(span, "print")?;
                let fmt = bld(self.builder.build_global_string_ptr("%s\n", "fmt"))?;
                let call_args: [BasicMetadataValueEnum<'ctx>; 2] =
                    [fmt.as_pointer_value().into(), v.into()];
                bld(self.builder.build_call(self.printf, &call_args, "printf"))?;
                return Ok(());
            }
            let v = self.gen_value(first)?.scalar(span, "print")?;
            let v64 = self.coerce(v, &self.i64_ty.into(), span, "print")?;
            let fmt = bld(self.builder.build_global_string_ptr("%lld\n", "fmt"))?;
            let call_args: [BasicMetadataValueEnum<'ctx>; 2] =
                [fmt.as_pointer_value().into(), v64.into()];
            bld(self.builder.build_call(self.printf, &call_args, "printf"))?;
            return Ok(());
        }

        // Otherwise: the first argument must be a string format string
        let fmt = match first {
            HirExpr::StrLit(s, _) => s.clone(),
            other => {
                let sp = other.span();
                return Err(CodegenError {
                    msg: "multi-argument print requires the first argument to be a string format".to_string(),
                    line: sp.line,
                    col: sp.col,
                });
            }
        };

        let fmt_ptr = self.global_string(&fmt)?;
        let mut call_args: Vec<BasicMetadataValueEnum<'ctx>> = Vec::with_capacity(args.len());
        call_args.push(fmt_ptr.into());

        for arg in &args[1..] {
            match arg {
                HirExpr::StrLit(s, _) => {
                    let p = self.global_string(s)?;
                    call_args.push(p.into());
                }
                other => {
                    let v = self.gen_value(other)?.scalar(other.span(), "print")?;
                    // String values pass as %s; other scalars coerce to i64
                    if self.expr_ty(other)? == Ty::Str {
                        call_args.push(v.into());
                    } else {
                        let v64 = self.coerce(v, &self.i64_ty.into(), other.span(), "print")?;
                        call_args.push(v64.into());
                    }
                }
            }
        }

        bld(self.builder.build_call(self.printf, &call_args, "printf"))?;
        Ok(())
    }

    fn global_string(&mut self, s: &str) -> Result<PointerValue<'ctx>, CodegenError> {
        let name = format!("str_{}", self.str_counter);
        self.str_counter += 1;
        let gv = bld(self.builder.build_global_string_ptr(s, &name))?;
        Ok(gv.as_pointer_value())
    }

    /// Create a string global constant `[N x i8]` (auto NUL-terminated); returns `i8*`.
    /// Function arguments: scalars pass directly; aggregates (array/tuple) are loaded
    fn call_arg(
        &mut self,
        v: GenValue<'ctx>,
        to: &BasicTypeEnum<'ctx>,
        span: Span,
        what: &str,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let v = match v {
            GenValue::Scalar(v) => v,
            GenValue::Agg(p) => bld(self.builder.build_load(*to, p, "agg_arg"))?,
        };
        let v = self.coerce(v, to, span, what)?;
        Ok(v)
    }

    /// from their stack slots. The callee `gen_function` receives them via alloca+store.
    fn coerce(
        &mut self,
        v: BasicValueEnum<'ctx>,
        to: &BasicTypeEnum<'ctx>,
        span: Span,
        what: &str,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        if v.get_type() == *to {
            return Ok(v);
        }
        let (iv, from_w) = match v {
            BasicValueEnum::IntValue(iv) => (iv, iv.get_type().get_bit_width()),
            _other => {
                return Err(CodegenError {
                    msg: format!("{what}: cannot coerce the value to the target type"),
                    line: span.line,
                    col: span.col,
                });
            }
        };
        let to_ty = match to {
            BasicTypeEnum::IntType(t) => *t,
            _other => {
                return Err(CodegenError {
                    msg: format!("{what}: target type is not an integer"),
                    line: span.line,
                    col: span.col,
                });
            }
        };
        let to_w = to_ty.get_bit_width();
        let out = if from_w < to_w {
            if from_w == 1 {
                bld(self.builder.build_int_z_extend(iv, to_ty, "zext"))?
            } else {
                bld(self.builder.build_int_s_extend(iv, to_ty, "sext"))?
            }
        } else if from_w > to_w {
            if to_w == 1 {
                // 1-bit (bool) zero-extends; others sign-extend (i32 → i64)
                let zero = iv.get_type().const_zero();
                bld(self.builder.build_int_compare(IntPredicate::NE, iv, zero, "tobool"))?
            } else {
                bld(self.builder.build_int_truncate(iv, to_ty, "trunc"))?
            }
        } else {
            iv
        };
        Ok(out.into())
    }

    fn internal_err(&self, span: Span, msg: &str) -> CodegenError {
        CodegenError {
            msg: format!("internal error: {msg}"),
            line: span.line,
            col: span.col,
        }
    }

    /// Type hint for aggregate-literal elements (used for tuple temp slots).
    /// Literals use their own type; other expressions look up static types (generics via
    /// type_subst), so the temp-slot layout matches the target type.
    fn elem_ty_hint(&self, expr: &HirExpr) -> Ty {
        match expr {
            HirExpr::BoolLit(..) => Ty::Bool,
            HirExpr::StrLit(..) => Ty::Str,
            HirExpr::IntLit(..) => Ty::I64,
            other => self.expr_ty(other).unwrap_or(Ty::I64),
        }
    }
}
