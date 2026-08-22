use std::collections::{HashMap, HashSet};

use crate::const_eval;
use aero_hir::hir::{
    BlasOp, DefId, ElemOp, HirBlock, HirConstDef, HirEnumDef, HirExpr, HirFn, HirMatchArm,
    HirMatchPattern, HirProgram, HirStmt, HirStructDef, HirUnionDef, ReduceOp, ScopeId,
};
use aero_hir::hir::{HirImplBlock, HirTraitDef};
use aero_hir::infer::{substitute, GenericInstance};
use aero_hir::ty::Ty;

/// Cap on generic instantiations (prevents unbounded monomorphization).
const MAX_GENERIC_INSTANCES: usize = 128;

/// Maximum statement coverage counters (`__aero_cov` / `__aero_cov_lines` array size).
/// Triggered by `AERO_COV`; oversized sources are rejected with a clear error.
const COV_CAP: u64 = 1 << 17;

/// LLVM function name of a generic instance: `max$i64`, `max$bool`; multiple
/// type args are separated by `;`. Unique symbol per instance in the module.
fn mono_name(fn_name: &str, type_args: &[Ty]) -> String {
    let args = type_args
        .iter()
        .map(|t| t.to_string())
        .collect::<Vec<_>>()
        .join(";");
    // Flatten module-separated mangled names (`m::foo`) so the LLVM symbol is valid.
    format!("{fn_name}${args}").replace("::", "_")
}
use aero_parse::ast::{BinOp, CmpOp, LogicOp, UnOp};
use aero_parse::span::Span;
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::debug_info::{
    AsDIScope, DIFlags, DIFlagsConstants, DISubprogram, DWARFEmissionKind, DWARFSourceLanguage,
    DebugInfoBuilder, DICompileUnit, DIFile,
};
use inkwell::module::{FlagBehavior, Module};
use inkwell::types::{BasicMetadataTypeEnum, BasicType, BasicTypeEnum, IntType};
use inkwell::values::{
    BasicMetadataValueEnum, BasicValue, BasicValueEnum, FunctionValue, GlobalValue, IntValue,
    PointerValue,
};
use inkwell::{AddressSpace, DLLStorageClass, FloatPredicate, GlobalVisibility, IntPredicate};

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

/// Builds the self-contained UTF-8 String helpers as module functions
/// (stdlib Phase 1, "字符串 2.0"). They operate on a NUL-terminated byte buffer
/// plus its length and never touch the String struct layout, so both the JIT
/// and AOT link cleanly with no extra runtime symbols.
///
/// Returns `(aero_utf8_len, aero_utf8_at, aero_utf8_push, aero_utf8_pop)`:
/// - len(data, len) -> i64: number of Unicode code points (UTF-8 decode).
/// - at(data, len, index) -> i64: code point at character index, or -1.
/// - push(data, cp) -> i64: encode `cp` as UTF-8 into `data`, return bytes written (0 if invalid).
/// - pop(data, len, out_len) -> i64: return the last code point, write its byte length to `out_len`.
fn build_utf8_helpers<'ctx>(
    module: &Module<'ctx>,
    context: &'ctx Context,
    i8_ptr_ty: inkwell::types::PointerType<'ctx>,
    i64_ty: IntType<'ctx>,
) -> (
    FunctionValue<'ctx>,
    FunctionValue<'ctx>,
    FunctionValue<'ctx>,
    FunctionValue<'ctx>,
) {
    let i8_ty = context.i8_type();

    // ---- aero_utf8_len(data: i8*, len: i64) -> i64 ----
    let len_fn = module.add_function(
        "aero_utf8_len",
        i64_ty.fn_type(&[i8_ptr_ty.into(), i64_ty.into()], false),
        None,
    );
    {
        let b = context.create_builder();
        let entry = context.append_basic_block(len_fn, "entry");
        b.position_at_end(entry);
        let data = len_fn.get_nth_param(0).unwrap().into_pointer_value();
        let len = len_fn.get_nth_param(1).unwrap().into_int_value();
        let count = b.build_alloca(i64_ty, "cnt").unwrap();
        b.build_store(count, i64_ty.const_zero()).unwrap();
        let ivar = b.build_alloca(i64_ty, "i").unwrap();
        b.build_store(ivar, i64_ty.const_zero()).unwrap();
        let cond = context.append_basic_block(len_fn, "cond");
        let body = context.append_basic_block(len_fn, "body");
        let ret = context.append_basic_block(len_fn, "ret");
        b.build_unconditional_branch(cond).unwrap();
        b.position_at_end(cond);
        let cur = b.build_load(i64_ty, ivar, "i").unwrap().into_int_value();
        let c_cond = b
            .build_int_compare(IntPredicate::ULT, cur, len, "cond")
            .unwrap();
        b.build_conditional_branch(c_cond, body, ret).unwrap();
        b.position_at_end(body);
        let ptr = unsafe { b.build_in_bounds_gep(i8_ty, data, &[cur], "ptr").unwrap() };
        let byte = b.build_load(i8_ty, ptr, "b").unwrap().into_int_value();
        let byte64 = b.build_int_z_extend(byte, i64_ty, "b64").unwrap();
        let hi = b.build_and(byte64, i64_ty.const_int(0xC0, false), "hi").unwrap();
        let is_cont = b
            .build_int_compare(IntPredicate::EQ, hi, i64_ty.const_int(0x80, false), "cont")
            .unwrap();
        let cnt_bb = context.append_basic_block(len_fn, "cnt");
        let inc_bb = context.append_basic_block(len_fn, "inc");
        b.build_conditional_branch(is_cont, inc_bb, cnt_bb).unwrap();
        b.position_at_end(cnt_bb);
        let c0v = b.build_load(i64_ty, count, "c").unwrap().into_int_value();
        let c1v = b.build_int_add(c0v, i64_ty.const_int(1, false), "c1").unwrap();
        b.build_store(count, c1v).unwrap();
        b.build_unconditional_branch(inc_bb).unwrap();
        b.position_at_end(inc_bb);
        let i1 = b.build_int_add(cur, i64_ty.const_int(1, false), "i1").unwrap();
        b.build_store(ivar, i1).unwrap();
        b.build_unconditional_branch(cond).unwrap();
        b.position_at_end(ret);
        let res = b.build_load(i64_ty, count, "res").unwrap().into_int_value();
        b.build_return(Some(&res)).unwrap();
    }

    // ---- Neu空 aero_utf8_decode(data: i8*, i: i64, len: i64) -> i64 ----
    // Decodes the code point at `data[i]` (assumed a valid char start). Returns -1
    // if truncated or invalid.
    let decode_fn = module.add_function(
        "aero_utf8_decode",
        i64_ty.fn_type(&[i8_ptr_ty.into(), i64_ty.into(), i64_ty.into()], false),
        None,
    );
    {
        let b = context.create_builder();
        let entry = context.append_basic_block(decode_fn, "entry");
        b.position_at_end(entry);
        let data = decode_fn.get_nth_param(0).unwrap().into_pointer_value();
        let i = decode_fn.get_nth_param(1).unwrap().into_int_value();
        let len = decode_fn.get_nth_param(2).unwrap().into_int_value();
        let p0 = unsafe { b.build_in_bounds_gep(i8_ty, data, &[i], "p0").unwrap() };
        let b0 = b
            .build_int_z_extend(b.build_load(i8_ty, p0, "b0").unwrap().into_int_value(), i64_ty, "b0z")
            .unwrap();
        let is_ascii = b
            .build_int_compare(IntPredicate::ULT, b0, i64_ty.const_int(0x80, false), "a")
            .unwrap();
        let next_bb = context.append_basic_block(decode_fn, "next");
        let ascii_bb = context.append_basic_block(decode_fn, "ascii");
        b.build_conditional_branch(is_ascii, ascii_bb, next_bb).unwrap();
        b.position_at_end(ascii_bb);
        b.build_return(Some(&b0)).unwrap();

        b.position_at_end(next_bb);
        // 2-byte: b0 & 0xE0 == 0xC0
        let hi2 = b.build_and(b0, i64_ty.const_int(0xE0, false), "hi2").unwrap();
        let is2 = b
            .build_int_compare(IntPredicate::EQ, hi2, i64_ty.const_int(0xC0, false), "is2")
            .unwrap();
        let chk3 = context.append_basic_block(decode_fn, "chk3");
        let d2 = context.append_basic_block(decode_fn, "d2");
        b.build_conditional_branch(is2, d2, chk3).unwrap();
        b.position_at_end(d2);
        let i1 = b.build_int_add(i, i64_ty.const_int(1, false), "i1").unwrap();
        let inb = b.build_int_compare(IntPredicate::ULT, i1, len, "inb").unwrap();
        let fail2 = context.append_basic_block(decode_fn, "fail2");
        let d2ok = context.append_basic_block(decode_fn, "d2ok");
        b.build_conditional_branch(inb, d2ok, fail2).unwrap();
        b.position_at_end(d2ok);
        let p1 = unsafe { b.build_in_bounds_gep(i8_ty, data, &[i1], "p1").unwrap() };
        let b1 = b
            .build_int_z_extend(b.build_load(i8_ty, p1, "b1").unwrap().into_int_value(), i64_ty, "b1z")
            .unwrap();
        let lo = b.build_and(b0, i64_ty.const_int(0x1F, false), "lo").unwrap();
        let shl = b.build_left_shift(lo, i64_ty.const_int(6, false), "shl").unwrap();
        let b1lo = b.build_and(b1, i64_ty.const_int(0x3F, false), "b1lo").unwrap();
        let cp = b.build_or(shl, b1lo, "cp").unwrap();
        b.build_return(Some(&cp)).unwrap();
        b.position_at_end(fail2);
        let m1 = i64_ty.const_int(u64::MAX, false);
        b.build_return(Some(&m1)).unwrap();

        // 3-byte: b0 & 0xF0 == 0xE0
        b.position_at_end(chk3);
        let hi3 = b.build_and(b0, i64_ty.const_int(0xF0, false), "hi3").unwrap();
        let is3 = b
            .build_int_compare(IntPredicate::EQ, hi3, i64_ty.const_int(0xE0, false), "is3")
            .unwrap();
        let chk4 = context.append_basic_block(decode_fn, "chk4");
        let d3 = context.append_basic_block(decode_fn, "d3");
        b.build_conditional_branch(is3, d3, chk4).unwrap();
        b.position_at_end(d3);
        let need3 = i64_ty.const_int(2, false);
        let iplus = b.build_int_add(i, need3, "i3").unwrap();
        let inb3 = b.build_int_compare(IntPredicate::ULT, iplus, len, "inb3").unwrap();
        let fail3 = context.append_basic_block(decode_fn, "fail3");
        let d3ok = context.append_basic_block(decode_fn, "d3ok");
        b.build_conditional_branch(inb3, d3ok, fail3).unwrap();
        b.position_at_end(d3ok);
        let i3a = b.build_int_add(i, i64_ty.const_int(1, false), "i3a").unwrap();
        let p1b = unsafe { b.build_in_bounds_gep(i8_ty, data, &[i3a], "p1").unwrap() };
        let b1b = b
            .build_int_z_extend(b.build_load(i8_ty, p1b, "b1").unwrap().into_int_value(), i64_ty, "b1z")
            .unwrap();
        let i3b = b.build_int_add(i, i64_ty.const_int(2, false), "i3b").unwrap();
        let p2b = unsafe { b.build_in_bounds_gep(i8_ty, data, &[i3b], "p2").unwrap() };
        let b2b = b
            .build_int_z_extend(b.build_load(i8_ty, p2b, "b2").unwrap().into_int_value(), i64_ty, "b2z")
            .unwrap();
        let lo3 = b.build_and(b0, i64_ty.const_int(0x0F, false), "lo3").unwrap();
        let s1 = b.build_left_shift(lo3, i64_ty.const_int(12, false), "s1").unwrap();
        let b1lo = b.build_and(b1b, i64_ty.const_int(0x3F, false), "b1lo").unwrap();
        let s2 = b.build_left_shift(b1lo, i64_ty.const_int(6, false), "s2").unwrap();
        let acc = b.build_or(s1, s2, "acc").unwrap();
        let b2lo = b.build_and(b2b, i64_ty.const_int(0x3F, false), "b2lo").unwrap();
        let cp3 = b.build_or(acc, b2lo, "cp3").unwrap();
        b.build_return(Some(&cp3)).unwrap();
        b.position_at_end(fail3);
        b.build_return(Some(&i64_ty.const_int(u64::MAX, false))).unwrap();

        // 4-byte: b0 & 0xF8 == 0xF0
        b.position_at_end(chk4);
        let hi4 = b.build_and(b0, i64_ty.const_int(0xF8, false), "hi4").unwrap();
        let is4 = b
            .build_int_compare(IntPredicate::EQ, hi4, i64_ty.const_int(0xF0, false), "is4")
            .unwrap();
        let fail4 = context.append_basic_block(decode_fn, "fail4");
        let d4ok = context.append_basic_block(decode_fn, "d4ok");
        b.build_conditional_branch(is4, d4ok, fail4).unwrap();
        b.position_at_end(d4ok);
        let iplus4 = b.build_int_add(i, i64_ty.const_int(3, false), "i4").unwrap();
        let inb4 = b.build_int_compare(IntPredicate::ULT, iplus4, len, "inb4").unwrap();
        let fail4b = context.append_basic_block(decode_fn, "fail4b");
        let d4body = context.append_basic_block(decode_fn, "d4body");
        b.build_conditional_branch(inb4, d4body, fail4b).unwrap();
        b.position_at_end(d4body);
        let i4a = b.build_int_add(i, i64_ty.const_int(1, false), "i4a").unwrap();
        let p1c = unsafe { b.build_in_bounds_gep(i8_ty, data, &[i4a], "p1").unwrap() };
        let b1c = b
            .build_int_z_extend(b.build_load(i8_ty, p1c, "b1").unwrap().into_int_value(), i64_ty, "b1z")
            .unwrap();
        let i4b = b.build_int_add(i, i64_ty.const_int(2, false), "i4b").unwrap();
        let p2c = unsafe { b.build_in_bounds_gep(i8_ty, data, &[i4b], "p2").unwrap() };
        let b2c = b
            .build_int_z_extend(b.build_load(i8_ty, p2c, "b2").unwrap().into_int_value(), i64_ty, "b2z")
            .unwrap();
        let i4c = b.build_int_add(i, i64_ty.const_int(3, false), "i4c").unwrap();
        let p3c = unsafe { b.build_in_bounds_gep(i8_ty, data, &[i4c], "p3").unwrap() };
        let b3c = b
            .build_int_z_extend(b.build_load(i8_ty, p3c, "b3").unwrap().into_int_value(), i64_ty, "b3z")
            .unwrap();
        let lo4 = b.build_and(b0, i64_ty.const_int(0x07, false), "lo4").unwrap();
        let s1 = b.build_left_shift(lo4, i64_ty.const_int(18, false), "s1").unwrap();
        let b1lo = b.build_and(b1c, i64_ty.const_int(0x3F, false), "b1lo").unwrap();
        let s2 = b.build_left_shift(b1lo, i64_ty.const_int(12, false), "s2").unwrap();
        let acc = b.build_or(s1, s2, "acc").unwrap();
        let b2lo = b.build_and(b2c, i64_ty.const_int(0x3F, false), "b2lo").unwrap();
        let s3 = b.build_left_shift(b2lo, i64_ty.const_int(6, false), "s3").unwrap();
        let acc = b.build_or(acc, s3, "acc").unwrap();
        let b3lo = b.build_and(b3c, i64_ty.const_int(0x3F, false), "b3lo").unwrap();
        let cp4 = b.build_or(acc, b3lo, "cp4").unwrap();
        b.build_return(Some(&cp4)).unwrap();
        b.position_at_end(fail4b);
        b.build_return(Some(&i64_ty.const_int(u64::MAX, false))).unwrap();
        b.position_at_end(fail4);
        b.build_return(Some(&i64_ty.const_int(u64::MAX, false))).unwrap();
    }

    // ---- aero_utf8_at(data: i8*, len: i64, index: i64) -> i64 ----
    let at_fn = module.add_function(
        "aero_utf8_at",
        i64_ty.fn_type(&[i8_ptr_ty.into(), i64_ty.into(), i64_ty.into()], false),
        None,
    );
    {
        let b = context.create_builder();
        let entry = context.append_basic_block(at_fn, "entry");
        b.position_at_end(entry);
        let data = at_fn.get_nth_param(0).unwrap().into_pointer_value();
        let len = at_fn.get_nth_param(1).unwrap().into_int_value();
        let index = at_fn.get_nth_param(2).unwrap().into_int_value();
        let ivar = b.build_alloca(i64_ty, "i").unwrap();
        b.build_store(ivar, i64_ty.const_zero()).unwrap();
        let cvar = b.build_alloca(i64_ty, "ci").unwrap();
        b.build_store(cvar, i64_ty.const_zero()).unwrap();
        let cond = context.append_basic_block(at_fn, "cond");
        let body = context.append_basic_block(at_fn, "body");
        let nf = context.append_basic_block(at_fn, "nf");
        b.build_unconditional_branch(cond).unwrap();
        b.position_at_end(cond);
        let cur = b.build_load(i64_ty, ivar, "i").unwrap().into_int_value();
        let c_cond = b.build_int_compare(IntPredicate::ULT, cur, len, "cond").unwrap();
        b.build_conditional_branch(c_cond, body, nf).unwrap();
        b.position_at_end(body);
        let ptr = unsafe { b.build_in_bounds_gep(i8_ty, data, &[cur], "ptr").unwrap() };
        let byte64 = b
            .build_int_z_extend(b.build_load(i8_ty, ptr, "b").unwrap().into_int_value(), i64_ty, "b64")
            .unwrap();
        let hi = b.build_and(byte64, i64_ty.const_int(0xC0, false), "hi").unwrap();
        let is_cont = b
            .build_int_compare(IntPredicate::EQ, hi, i64_ty.const_int(0x80, false), "cont")
            .unwrap();
        let inc_bb = context.append_basic_block(at_fn, "inc");
        let start_bb = context.append_basic_block(at_fn, "start");
        b.build_conditional_branch(is_cont, inc_bb, start_bb).unwrap();
        // start: check ci == index
        b.position_at_end(start_bb);
        let ci = b.build_load(i64_ty, cvar, "ci").unwrap().into_int_value();
        let is_match = b.build_int_compare(IntPredicate::EQ, ci, index, "match").unwrap();
        let dec_bb = context.append_basic_block(at_fn, "dec");
        let ci_inc_bb = context.append_basic_block(at_fn, "ci_inc");
        b.build_conditional_branch(is_match, dec_bb, ci_inc_bb).unwrap();
        b.position_at_end(dec_bb);
        let cp = b.build_call(
            decode_fn,
            &[data.into(), cur.into(), len.into()],
            "dec",
        );
        let cp = cp.unwrap().try_as_basic_value().basic().unwrap().into_int_value();
        b.build_return(Some(&cp)).unwrap();
        b.position_at_end(ci_inc_bb);
        let ci1 = b.build_int_add(ci, i64_ty.const_int(1, false), "ci1").unwrap();
        b.build_store(cvar, ci1).unwrap();
        b.build_unconditional_branch(inc_bb).unwrap();
        // inc
        b.position_at_end(inc_bb);
        let i1 = b.build_int_add(cur, i64_ty.const_int(1, false), "i1").unwrap();
        b.build_store(ivar, i1).unwrap();
        b.build_unconditional_branch(cond).unwrap();
        // not found
        b.position_at_end(nf);
        b.build_return(Some(&i64_ty.const_int(u64::MAX, false))).unwrap();
    }

    // ---- aero_utf8_push(data: i8*, cp: i64) -> i64 ----
    // Encodes `cp` as UTF-8 into `data`; returns bytes written (0 if invalid cp).
    let push_fn = module.add_function(
        "aero_utf8_push",
        i64_ty.fn_type(&[i8_ptr_ty.into(), i64_ty.into()], false),
        None,
    );
    {
        let b = context.create_builder();
        let entry = context.append_basic_block(push_fn, "entry");
        b.position_at_end(entry);
        let data = push_fn.get_nth_param(0).unwrap().into_pointer_value();
        let cp = push_fn.get_nth_param(1).unwrap().into_int_value();
        let lt0 = b.build_int_compare(IntPredicate::SLT, cp, i64_ty.const_zero(), "lt0").unwrap();
        let gtmax = b.build_int_compare(IntPredicate::SGT, cp, i64_ty.const_int(0x10FFFF, false), "gtm").unwrap();
        let invalid = b.build_or(lt0, gtmax, "inv").unwrap();
        let ok_bb = context.append_basic_block(push_fn, "ok");
        let bad_bb = context.append_basic_block(push_fn, "bad");
        b.build_conditional_branch(invalid, bad_bb, ok_bb).unwrap();
        b.position_at_end(bad_bb);
        b.build_return(Some(&i64_ty.const_zero())).unwrap();

        b.position_at_end(ok_bb);
        // 1-byte: cp < 0x80
        let is1 = b
            .build_int_compare(IntPredicate::ULT, cp, i64_ty.const_int(0x80, false), "e1")
            .unwrap();
        let e2 = context.append_basic_block(push_fn, "e2");
        let w1 = context.append_basic_block(push_fn, "w1");
        b.build_conditional_branch(is1, w1, e2).unwrap();
        b.position_at_end(w1);
        let p0 = unsafe { b.build_in_bounds_gep(i8_ty, data, &[i64_ty.const_zero()], "p0").unwrap() };
        let b0 = b.build_int_truncate(cp, i8_ty, "b0").unwrap();
        b.build_store(p0, b0).unwrap();
        b.build_return(Some(&i64_ty.const_int(1, false))).unwrap();

        // 2-byte: cp < 0x800
        b.position_at_end(e2);
        let is2 = b
            .build_int_compare(IntPredicate::ULT, cp, i64_ty.const_int(0x800, false), "e2")
            .unwrap();
        let e3 = context.append_basic_block(push_fn, "e3");
        let w2 = context.append_basic_block(push_fn, "w2");
        b.build_conditional_branch(is2, w2, e3).unwrap();
        b.position_at_end(w2);
        let p0 = unsafe { b.build_in_bounds_gep(i8_ty, data, &[i64_ty.const_zero()], "p0").unwrap() };
        let p1 = unsafe { b.build_in_bounds_gep(i8_ty, data, &[i64_ty.const_int(1, false)], "p1").unwrap() };
        let r0 = b.build_right_shift(cp, i64_ty.const_int(6, false), false, "r0").unwrap();
        let b0 = b.build_and(r0, i64_ty.const_int(0x1F, false), "a0").unwrap();
        let b0 = b.build_or(b0, i64_ty.const_int(0xC0, false), "o0").unwrap();
        b.build_store(p0, b.build_int_truncate(b0, i8_ty, "s0").unwrap()).unwrap();
        let b1 = b.build_and(cp, i64_ty.const_int(0x3F, false), "a1").unwrap();
        let b1 = b.build_or(b1, i64_ty.const_int(0x80, false), "o1").unwrap();
        b.build_store(p1, b.build_int_truncate(b1, i8_ty, "s1").unwrap()).unwrap();
        b.build_return(Some(&i64_ty.const_int(2, false))).unwrap();

        // 3-byte: cp < 0x10000
        b.position_at_end(e3);
        let is3 = b
            .build_int_compare(IntPredicate::ULT, cp, i64_ty.const_int(0x10000, false), "e3")
            .unwrap();
        let e4 = context.append_basic_block(push_fn, "e4");
        let w3 = context.append_basic_block(push_fn, "w3");
        b.build_conditional_branch(is3, w3, e4).unwrap();
        b.position_at_end(w3);
        let p0 = unsafe { b.build_in_bounds_gep(i8_ty, data, &[i64_ty.const_zero()], "p0").unwrap() };
        let p1 = unsafe { b.build_in_bounds_gep(i8_ty, data, &[i64_ty.const_int(1, false)], "p1").unwrap() };
        let p2 = unsafe { b.build_in_bounds_gep(i8_ty, data, &[i64_ty.const_int(2, false)], "p2").unwrap() };
        let r0 = b.build_right_shift(cp, i64_ty.const_int(12, false), false, "r0").unwrap();
        let b0 = b.build_and(r0, i64_ty.const_int(0x0F, false), "a0").unwrap();
        let b0 = b.build_or(b0, i64_ty.const_int(0xE0, false), "o0").unwrap();
        b.build_store(p0, b.build_int_truncate(b0, i8_ty, "s0").unwrap()).unwrap();
        let r1 = b.build_right_shift(cp, i64_ty.const_int(6, false), false, "r1").unwrap();
        let b1 = b.build_and(r1, i64_ty.const_int(0x3F, false), "a1").unwrap();
        let b1 = b.build_or(b1, i64_ty.const_int(0x80, false), "o1").unwrap();
        b.build_store(p1, b.build_int_truncate(b1, i8_ty, "s1").unwrap()).unwrap();
        let b2 = b.build_and(cp, i64_ty.const_int(0x3F, false), "a2").unwrap();
        let b2 = b.build_or(b2, i64_ty.const_int(0x80, false), "o2").unwrap();
        b.build_store(p2, b.build_int_truncate(b2, i8_ty, "s2").unwrap()).unwrap();
        b.build_return(Some(&i64_ty.const_int(3, false))).unwrap();

        // 4-byte
        b.position_at_end(e4);
        let p0 = unsafe { b.build_in_bounds_gep(i8_ty, data, &[i64_ty.const_zero()], "p0").unwrap() };
        let p1 = unsafe { b.build_in_bounds_gep(i8_ty, data, &[i64_ty.const_int(1, false)], "p1").unwrap() };
        let p2 = unsafe { b.build_in_bounds_gep(i8_ty, data, &[i64_ty.const_int(2, false)], "p2").unwrap() };
        let p3 = unsafe { b.build_in_bounds_gep(i8_ty, data, &[i64_ty.const_int(3, false)], "p3").unwrap() };
        let r0 = b.build_right_shift(cp, i64_ty.const_int(18, false), false, "r0").unwrap();
        let b0 = b.build_and(r0, i64_ty.const_int(0x07, false), "a0").unwrap();
        let b0 = b.build_or(b0, i64_ty.const_int(0xF0, false), "o0").unwrap();
        b.build_store(p0, b.build_int_truncate(b0, i8_ty, "s0").unwrap()).unwrap();
        let r1 = b.build_right_shift(cp, i64_ty.const_int(12, false), false, "r1").unwrap();
        let b1 = b.build_and(r1, i64_ty.const_int(0x3F, false), "a1").unwrap();
        let b1 = b.build_or(b1, i64_ty.const_int(0x80, false), "o1").unwrap();
        b.build_store(p1, b.build_int_truncate(b1, i8_ty, "s1").unwrap()).unwrap();
        let r2 = b.build_right_shift(cp, i64_ty.const_int(6, false), false, "r2").unwrap();
        let b2 = b.build_and(r2, i64_ty.const_int(0x3F, false), "a2").unwrap();
        let b2 = b.build_or(b2, i64_ty.const_int(0x80, false), "o2").unwrap();
        b.build_store(p2, b.build_int_truncate(b2, i8_ty, "s2").unwrap()).unwrap();
        let b3 = b.build_and(cp, i64_ty.const_int(0x3F, false), "a3").unwrap();
        let b3 = b.build_or(b3, i64_ty.const_int(0x80, false), "o3").unwrap();
        b.build_store(p3, b.build_int_truncate(b3, i8_ty, "s3").unwrap()).unwrap();
        b.build_return(Some(&i64_ty.const_int(4, false))).unwrap();
    }

    // ---- aero_utf8_pop(data: i8*, len: i64, out_len: i64*) -> i64 ----
    // Returns the last code point; writes how many bytes it occupied to `out_len`.
    let pop_fn = module.add_function(
        "aero_utf8_pop",
        i64_ty.fn_type(&[i8_ptr_ty.into(), i64_ty.into(), i8_ptr_ty.into()], false),
        None,
    );
    {
        let b = context.create_builder();
        let entry = context.append_basic_block(pop_fn, "entry");
        b.position_at_end(entry);
        let data = pop_fn.get_nth_param(0).unwrap().into_pointer_value();
        let len = pop_fn.get_nth_param(1).unwrap().into_int_value();
        let out_len = pop_fn.get_nth_param(2).unwrap().into_pointer_value();
        let zero = i64_ty.const_zero();
        let m1 = i64_ty.const_int(u64::MAX, false);
        let empty = b.build_int_compare(IntPredicate::EQ, len, zero, "empty").unwrap();
        let nf_bb = context.append_basic_block(pop_fn, "nf");
        let scan_bb = context.append_basic_block(pop_fn, "scan");
        b.build_conditional_branch(empty, nf_bb, scan_bb).unwrap();
        b.position_at_end(nf_bb);
        b.build_store(out_len, zero).unwrap();
        b.build_return(Some(&m1)).unwrap();

        // Scan backwards from len-1 to find the last char start (first non-continuation).
        b.position_at_end(scan_bb);
        let start = b.build_int_sub(len, i64_ty.const_int(1, false), "start").unwrap();
        let svar = b.build_alloca(i64_ty, "s").unwrap();
        b.build_store(svar, start).unwrap();
        let cond = context.append_basic_block(pop_fn, "cond");
        let body = context.append_basic_block(pop_fn, "body");
        // loop: while s >= 0
        b.build_unconditional_branch(cond).unwrap();
        b.position_at_end(cond);
        let s = b.build_load(i64_ty, svar, "s").unwrap().into_int_value();
        let sge0 = b.build_int_compare(IntPredicate::SGE, s, zero, "sge").unwrap();
        b.build_conditional_branch(sge0, body, nf_bb).unwrap();
        b.position_at_end(body);
        let ptr = unsafe { b.build_in_bounds_gep(i8_ty, data, &[s], "ptr").unwrap() };
        let byte64 = b
            .build_int_z_extend(b.build_load(i8_ty, ptr, "b").unwrap().into_int_value(), i64_ty, "b64")
            .unwrap();
        let hi = b.build_and(byte64, i64_ty.const_int(0xC0, false), "hi").unwrap();
        let is_cont = b
            .build_int_compare(IntPredicate::EQ, hi, i64_ty.const_int(0x80, false), "cont")
            .unwrap();
        let dec_bb = context.append_basic_block(pop_fn, "dec");
        let dec_scan_bb = context.append_basic_block(pop_fn, "dec_scan");
        b.build_conditional_branch(is_cont, dec_scan_bb, dec_bb).unwrap();
        b.position_at_end(dec_scan_bb);
        let sm1 = b.build_int_sub(s, i64_ty.const_int(1, false), "sm1").unwrap();
        b.build_store(svar, sm1).unwrap();
        b.build_unconditional_branch(cond).unwrap();
        // found: decode at s, out_len = len - s
        b.position_at_end(dec_bb);
        let cp = b.build_call(
            decode_fn,
            &[data.into(), s.into(), len.into()],
            "dpop",
        );
        let cp = cp
            .unwrap()
            .try_as_basic_value()
            .basic()
            .unwrap()
            .into_int_value();
        let removed = b.build_int_sub(len, s, "removed").unwrap();
        b.build_store(out_len, removed).unwrap();
        b.build_return(Some(&cp)).unwrap();
    }

    (len_fn, at_fn, push_fn, pop_fn)
}

/// Compile a typed HIR program into an LLVM IR module.
/// The module contains `main() -> i64` and all user functions. Variables use stack
/// slots (alloca), so updates in loops/branches are visible across blocks; `print`
pub fn compile<'ctx>(
    context: &'ctx Context,
    program: &HirProgram,
    var_tys: &HashMap<DefId, Ty>,
    moved_by_scope: &HashMap<ScopeId, HashSet<DefId>>,
    instances: &[GenericInstance],
    call_types: &HashMap<usize, Vec<Ty>>,
    struct_lit_types: &HashMap<usize, Vec<Ty>>,
    enum_lit_types: &HashMap<usize, Vec<Ty>>,
    emit_main: bool,
    py_ext: Option<&crate::PyExtSpec>,
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


    // The main function: standard C entry `main(argc, argv)`. Top-level statements
    // live here; argc/argv feed the `arg_count()`/`arg(i)` builtins (M1.2).
    let main = module.add_function(
        "main",
        i64_ty.fn_type(
            &[i32_ty.into(), i8_ptr_ty.ptr_type(AddressSpace::from(0u16)).into()],
            false,
        ),
        None,
    );
    // Shared-library builds (`.so`/`.pyd`/`.dylib`) keep the top-level statements
    // but hide `main` from the dynamic symbol table (the library has no C entry).
    if !emit_main {
        main.set_linkage(inkwell::module::Linkage::Internal);
    }

    // Globals written at main entry: CLI argument count and vector.
    let aero_argc = module.add_global(i32_ty, None, "aero_argc");
    aero_argc.set_initializer(&i32_ty.const_zero());
    let aero_argv = module.add_global(i8_ptr_ty.ptr_type(AddressSpace::from(0u16)), None, "aero_argv");
    aero_argv.set_initializer(&i8_ptr_ty.ptr_type(AddressSpace::from(0u16)).const_zero());

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
        // extern "C" functions use the C symbol name (possibly aliased via `= "sym"`);
        // others use the function name. C ABI has no namespaces, so an `extern "C"`
        // symbol defaults to the *bare* name (module prefix dropped, `m::sqlite3_open`
        // links as `sqlite3_open`). Mangled module names (`m::foo`) are flattened
        // to `m_foo` so the emitted LLVM symbol has no `::`.
        let llvm_name = f
            .extern_symbol
            .as_deref()
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                if f.is_extern {
                    f.name.rsplit("::").next().unwrap_or(&f.name).to_string()
                } else {
                    f.name.clone()
                }
            })
            .replace("::", "_");
        if f.is_extern && matches!(llvm_name.as_str(), "printf" | "abort") {
            return Err(CodegenError {
                msg: format!("extern symbol name `{llvm_name}` is reserved"),
                line: f.span.line,
                col: f.span.col,
            });
        }
        let mut param_tys = Vec::new();
        for (_, ty, sp) in &f.params {
            param_tys.push(
                llvm_ty(context, ty, *sp, &empty_subst, &program.structs, &program.unions, &program.enums)?.into(),
            );
        }
        // extern aliases must not collide with symbols the codegen declares (printf/abort)
        let fn_ty = match &f.ret {
            Some(t) => llvm_ty(context, t, f.span, &empty_subst, &program.structs, &program.unions, &program.enums)?
                .fn_type(&param_tys, false),
            None => context.void_type().fn_type(&param_tys, false),
        };
        // Reuse existing LLVM function when the symbol name is already declared
        // (e.g. two extern "C" functions aliased to the same C symbol). This
        // prevents duplicate LLVM function declarations with the same name.
        let func = funcs
            .iter()
            .find(|f| f.get_name().to_str().map(|s| s == llvm_name.as_str()).unwrap_or(false))
            .copied()
            .unwrap_or_else(|| module.add_function(&llvm_name, fn_ty, None));
        // `#[export]` functions are visible C-ABI symbols: force external
        // visibility and mark them for DLL export (Windows COFF). On ELF/Mach-O
        // the DLL storage class is ignored, so the symbol is exported by default.
        if f.exported {
            func.set_linkage(inkwell::module::Linkage::External);
            func.as_global_value().set_visibility(GlobalVisibility::Default);
            func.as_global_value().set_dll_storage_class(DLLStorageClass::Export);
        }
        funcs.push(func);
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
    let memset = declared(
        "memset",
        i8_ptr_ty.fn_type(&[i8_ptr_ty.into(), i32_ty.into(), i64_ty.into()], false),
    );
    let strlen = declared("strlen", i64_ty.fn_type(&[i8_ptr_ty.into()], false));
    let memcmp = declared(
        "memcmp",
        i32_ty.fn_type(&[i8_ptr_ty.into(), i8_ptr_ty.into(), i64_ty.into()], false),
    );
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
    // File IO helpers (M1.2): standard C stdio with CRT export names.
    let fopen = declared(
        "fopen",
        i8_ptr_ty.fn_type(&[i8_ptr_ty.into(), i8_ptr_ty.into()], false),
    );
    let fclose = declared("fclose", i32_ty.fn_type(&[i8_ptr_ty.into()], false));
    let fprintf = declared(
        "fprintf",
        i32_ty.fn_type(&[i8_ptr_ty.into(), i8_ptr_ty.into()], true),
    );
    let fread = declared(
        "fread",
        i64_ty.fn_type(
            &[i8_ptr_ty.into(), i64_ty.into(), i64_ty.into(), i8_ptr_ty.into()],
            false,
        ),
    );
    let fwrite = declared(
        "fwrite",
        i64_ty.fn_type(
            &[i8_ptr_ty.into(), i64_ty.into(), i64_ty.into(), i8_ptr_ty.into()],
            false,
        ),
    );
    // On Windows, `long` is 32-bit (LLP64 model), so `fseek` takes `i32` offset
    // and `ftell` returns `i32`. On Linux, `long` is 64-bit (LP64 model).
    // Use `fseek`/`ftell` with i32 types for cross-platform compatibility
    // (the Windows CRT exports `fseek`/`ftell` with `long` = 32-bit).
    let fseek = declared(
        "fseek",
        i32_ty.fn_type(&[i8_ptr_ty.into(), i32_ty.into(), i32_ty.into()], false),
    );
    let ftell = declared("ftell", i32_ty.fn_type(&[i8_ptr_ty.into()], false));
    let strstr = declared(
        "strstr",
        i8_ptr_ty.fn_type(&[i8_ptr_ty.into(), i8_ptr_ty.into()], false),
    );
    // Time / random number helpers (stdlib extension, Phase 11 simple items).
    // `rand` returns a 32-bit pseudo-random integer; `time` returns seconds since
    // the Unix epoch (time_t = i64). Both are CRT/libc exports.
    let rand = declared("rand", i32_ty.fn_type(&[], false));
    let time = declared(
        "time",
        i64_ty.fn_type(&[i8_ptr_ty.into()], false),
    );
    // Environment helpers (stdlib Phase 1): getenv/_putenv_s from the CRT.
    // getenv(name) -> const char* (NULL when unset); _putenv_s(name, value) -> 0 ok.
    let getenv = declared("getenv", i8_ptr_ty.fn_type(&[i8_ptr_ty.into()], false));
    let putenv = declared("_putenv_s", i32_ty.fn_type(&[i8_ptr_ty.into(), i8_ptr_ty.into()], false));

    // UTF-8 String helpers (stdlib Phase 1, "字符串 2.0"): build self-contained
    // module functions so both JIT and AOT link cleanly (no extra runtime symbols).
    let (utf8_len_f, utf8_at_f, utf8_push_f, utf8_pop_f) =
        build_utf8_helpers(&module, context, i8_ptr_ty, i64_ty);

    // Source-coverage instrumentation (opt-in via AERO_COV). Two fixed-size i64
    // arrays: execution counts + the source line of each counter. __aero_cov_fini
    // is defined at the end of `compile` and registered with atexit at main entry.
    let cov_mode = std::env::var("AERO_COV").map(|v| v == "1").unwrap_or(false);
    let cov_arr_ty = context.i64_type().array_type(COV_CAP as u32);
    let (cov_counts, cov_lines, cov_fini) = if cov_mode {
        let cc = module.add_global(cov_arr_ty, None, "__aero_cov");
        cc.set_initializer(&cov_arr_ty.const_zero());
        let cl = module.add_global(cov_arr_ty, None, "__aero_cov_lines");
        cl.set_initializer(&cov_arr_ty.const_zero());
        let fini = module.add_function(
            "__aero_cov_fini",
            context.void_type().fn_type(&[], false),
            None,
        );
        (Some(cc), Some(cl), Some(fini))
    } else {
        (None, None, None)
    };

    // Debug info (opt-in via AERO_DEBUG). One DI compilation unit + per-function
    // DISubprograms + per-statement DILocation positions. The module-wide metadata
    // emitted here is lowered to DWARF for ELF objects and to PDB for COFF (Windows)
    // at object-emission time, so a single frontend implementation drives both formats.
    // Empty sysroot/sdk (LLVM 22 signature) — this toolchain has no SDK root.
    let debug_mode = std::env::var("AERO_DEBUG").map(|v| v == "1").unwrap_or(false);
    let dbg_di = if debug_mode {
        module.add_basic_value_flag(
            "Debug Info Version",
            FlagBehavior::Warning,
            context.i32_type().const_int(3, false),
        );
        let (dibuilder, cu) = module.create_debug_info_builder(
            true,
            DWARFSourceLanguage::Rust,
            "aero",
            ".",
            "aero compiler",
            false,
            "",
            0,
            "",
            DWARFEmissionKind::Full,
            0,
            false,
            false,
            "",
            "",
        );
        Some((dibuilder, cu, cu.get_file()))
    } else {
        None
    };

    let (dbg_dibuilder, dbg_cu, dbg_file) = match dbg_di {
        Some((db, cu, f)) => (Some(db), Some(cu), Some(f)),
        None => (None, None, None),
    };

    let mut cg = Codegen {
        context,
        module: &module,
        builder,
        i64_ty,
        i32_ty,
        bool_ty,
        vars: HashMap::new(),
        var_tys,
        moved_by_scope,
        decl_order: Vec::new(),
        scope_stack: Vec::new(),
        str_counter: 0,
        printf,
        abort,
        malloc,
        free,
        memcpy,
        memset,
        strlen,
        strcmp,
        snprintf,
        strtoll,
        strstr,
        memcmp,
        fopen,
        fclose,
        fprintf,
        fread,
        fwrite,
        fseek,
        ftell,
        rand,
        time,
        getenv,
        putenv,
        utf8_len_f,
        utf8_at_f,
        utf8_push_f,
        utf8_pop_f,
        aero_argc,
        aero_argv,
        cur_func: main,
        funcs,
        arenas: HashMap::new(),
        arena_stack: Vec::new(),
        hir_funcs: &program.funcs,
        hir_structs: &program.structs,
        hir_unions: &program.unions,
        hir_consts: &program.consts,
        hir_enums: &program.enums,
        method_map: &program.method_map,
        hir_traits: &program.traits,
        hir_impls: &program.impls,
        dyn_vtables: HashMap::new(),
        call_types,
        struct_lit_types,
        enum_lit_types,
        instance_funcs: HashMap::new(),
        instance_count: 0,
        type_subst: HashMap::new(),
        loop_stack: Vec::new(),
        cur_ret: None,
        cov_mode,
        cov_counts,
        cov_lines,
        cov_fini,
        cov_next: 0,
        cov_line_map: Vec::new(),
        debug_mode,
        dibuilder: dbg_dibuilder,
        di_cu: dbg_cu,
        di_file: dbg_file,
        di_scope: None,
    };

    let entry = context.append_basic_block(main, "entry");
    cg.builder.position_at_end(entry);
    // Save argc/argv for the arg_count()/arg(i) builtins (M1.2)
    let argc = main.get_nth_param(0).unwrap().into_int_value();
    let argv = main.get_nth_param(1).unwrap().into_pointer_value();
    bld(cg.builder.build_store(cg.aero_argc.as_pointer_value(), argc))?;
    bld(cg.builder.build_store(cg.aero_argv.as_pointer_value(), argv))?;
    // Register the coverage dump with atexit so it runs at every exit path.
    if cg.cov_mode {
        if let Some(fini) = cg.cov_fini {
            let atexit = cg.module.add_function(
                "atexit",
                cg.i32_ty.fn_type(&[i8_ptr_ty.into()], false),
                None,
            );
            let fini_ptr = fini.as_global_value().as_pointer_value();
            let fini_as_i8 = bld(cg.builder.build_pointer_cast(
                fini_ptr,
                i8_ptr_ty,
                "cov_fini_ptr",
            ))?;
            bld(cg.builder.build_call(atexit, &[fini_as_i8.into()], "cov_atexit"))?;
        }
    }
    cg.decl_order.clear();
    // Debug info: treat the top-level body as an anonymous `main` subprogram so its
    // statements get line positions too.
    cg.di_attach_function("main", Span { line: 1, col: 1, start: 0, end: 1 }, main);
    cg.gen_block(&program.main)?;
    if !cg.cur_block_terminated() {
        let zero = cg.i64_ty.const_zero();
        bld(cg.builder.build_return(Some(&zero)))?;
    }
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

    // Python-extension build (`aero build --pyext`): emit the CPython glue for
    // every `#[py_export]` function — wrappers + method table + `PyInit_<name>`.
    if let Some(spec) = py_ext {
        gen_python_glue(&mut cg, spec)?;
    }

    // Bake the line map into the cov_lines global (build-time constant) and emit the
    // `__aero_cov_fini` body that dumps (line, count) to aero.cov.txt at exit.
    if cg.cov_mode {
        if let (Some(cl), _) = (cg.cov_lines, cg.cov_counts) {
            let mut init: Vec<IntValue> = Vec::with_capacity(COV_CAP as usize);
            for i in 0..cg.cov_line_map.len() {
                init.push(cg.i64_ty.const_int(cg.cov_line_map[i] as u64, false));
            }
            for _ in init.len()..(COV_CAP as usize) {
                init.push(cg.i64_ty.const_zero());
            }
            cl.set_initializer(&cg.i64_ty.const_array(&init));
            if cg.cov_next > COV_CAP as usize {
                return Err(CodegenError {
                    msg: format!(
                        "source has too many statements for coverage ({} > {COV_CAP}); raise COV_CAP",
                        cg.cov_next
                    ),
                    line: 0,
                    col: 0,
                });
            }
            let fini = cg.cov_fini.expect("cov_fini present in cov mode");
            build_cov_fini(&mut cg, fini)?;
        }
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

    // Finalize debug info: LLVM stashes the DI compilation unit into `llvm.dbg.cu` and
    // flags the module as carrying debug info, which the object-file writer lowers to
    // DWARF (ELF) or PDB (COFF). Skipped when not in AERO_DEBUG mode.
    if let Some(db) = cg.dibuilder.as_ref() {
        db.finalize();
    }

    Ok(module)
}

/// Emit the CPython C-API glue for every `#[py_export]` function in the module:
///
/// 1. **Wrapper** `PyObject* <f>__pywrap(PyObject*, PyObject*)` — parses `args`
///    with `PyArg_ParseTuple`, calls the Aero function, builds the return object.
/// 2. **Method table** — one `PyMethodDef` per export (METH_VARARGS), NULL-terminated.
/// 3. **Module definition** `PyModuleDef` (name/methods, size -1, no slots).
/// 4. **Entry point** `PyMODINIT_FUNC PyInit_<module>()` → `PyModule_Create`.
///
/// The CPython API entry points are declared as extern C symbols and resolved by
/// the linker against the Python import library (`-lpython3xx`); no C headers or
/// generated C source are involved. v1 locks the full CPython ABI (no limited API).
fn gen_python_glue<'ctx>(
    cg: &mut Codegen<'_, 'ctx>,
    spec: &crate::PyExtSpec,
) -> Result<(), CodegenError> {
    let module_name = spec.module;
    let context = cg.context;
    let module = cg.module;
    let i64_ty = cg.i64_ty;
    let i32_ty = cg.i32_ty;
    let i1_ty = cg.bool_ty;
    let f64_ty = context.f64_type();
    let i8_ty = context.i8_type();
    let i8p = context.ptr_type(AddressSpace::from(0u16));

    // Collect `#[py_export]` functions (non-generic, non-extern, with bodies).
    // An empty list is fine: the module shell (PyModuleDef + PyInit_<name>)
    // is still emitted so the extension is importable.
    let exported: Vec<(usize, &HirFn)> = cg
        .hir_funcs
        .iter()
        .enumerate()
        .filter(|(_, f)| f.py_export && !f.builtin && f.type_params.is_empty() && !f.is_extern)
        .collect();

    // CPython C-API entry points (resolved at link time against python3xx).
    let pyarg_parsetuple = module.add_function(
        "PyArg_ParseTuple",
        i32_ty.fn_type(&[i8p.into(), i8p.into()], true),
        None,
    );
    let pylong_fromlonglong = module.add_function(
        "PyLong_FromLongLong",
        i8p.fn_type(&[i64_ty.into()], false),
        None,
    );
    let pyfloat_fromdouble = module.add_function(
        "PyFloat_FromDouble",
        i8p.fn_type(&[f64_ty.into()], false),
        None,
    );
    let pybool_fromlong = module.add_function(
        "PyBool_FromLong",
        i8p.fn_type(&[i64_ty.into()], false),
        None,
    );
    let pyunicode_fromstring = module.add_function(
        "PyUnicode_FromString",
        i8p.fn_type(&[i8p.into()], false),
        None,
    );
    // `PyBytes_FromStringAndSize(const char* s, Py_ssize_t n)`: builds a Python
    // `bytes` object from a byte buffer (copies). Used by the `Vec<i64>` ↔ bytes
    // conversion (M2): each Vec<i64> element is truncated to a byte.
    let pybytes_fromstringandsize = module.add_function(
        "PyBytes_FromStringAndSize",
        i8p.fn_type(&[i8p.into(), i64_ty.into()], false),
        None,
    );
    // `PyModule_Create` is a macro in modern CPython expanding to
    // `PyModule_Create2(module, PYTHON_API_VERSION)`; the DLL exports the latter.
    let pymodule_create2 = module.add_function(
        "PyModule_Create2",
        i8p.fn_type(&[i8p.into(), i32_ty.into()], false),
        None,
    );
    // `Py_None` is `&_Py_NoneStruct` (a DLL-exported data symbol on Windows,
    // a plain global elsewhere). On COFF, imported data is accessed through the
    // `__imp_` indirection slot, exactly like C's `__declspec(dllimport)`.
    let py_none_imp = if spec.windows {
        Some(module.add_global(i8p, None, "__imp__Py_NoneStruct"))
    } else {
        None
    };
    let py_none_plain = if spec.windows {
        None
    } else {
        Some(module.add_global(i8_ty, None, "_Py_NoneStruct"))
    };

    // C layout of `PyMethodDef { const char* ml_name; PyCFunction ml_meth;
    // int ml_flags; const char* ml_doc; }` — the trailing pointer is 8-aligned,
    // so LLVM's non-packed struct inserts the matching padding automatically.
    let method_def_ty = context.struct_type(
        &[i8p.into(), i8p.into(), i32_ty.into(), i8p.into()],
        false,
    );
    // `PyModuleDef`: PyModuleDef_Base (ob_refcnt, ob_type, m_init, m_index,
    // m_copy) + m_name, m_doc, m_size, m_methods, m_slots, m_traverse,
    // m_clear, m_free. Note `m_copy` was added in Python 3.5 (base = 40 bytes).
    let module_def_ty = context.struct_type(
        &[
            i64_ty.into(), // ob_refcnt (Py_ssize_t)
            i8p.into(),    // ob_type (NULL)
            i8p.into(),    // m_init (NULL)
            i64_ty.into(), // m_index (0)
            i8p.into(),    // m_copy (NULL)
            i8p.into(),    // m_name
            i8p.into(),    // m_doc
            i64_ty.into(), // m_size (-1: global state, no subinterpreters)
            i8p.into(),    // m_methods
            i8p.into(),    // m_slots (NULL)
            i8p.into(),    // m_traverse (NULL)
            i8p.into(),    // m_clear (NULL)
            i8p.into(),    // m_free (NULL)
        ],
        false,
    );

    // 1. Wrapper functions + 2. method-table entries.
    struct WrapInfo<'ctx> {
        wrap: FunctionValue<'ctx>,
        name_ptr: PointerValue<'ctx>,
    }
    let mut wraps: Vec<WrapInfo> = Vec::new();
    for (idx, f) in &exported {
        let py_name = f.name.rsplit("::").next().unwrap_or(&f.name);
        let wrap_name = format!("{py_name}__pywrap");
        let wrap = module.add_function(
            &wrap_name,
            i8p.fn_type(&[i8p.into(), i8p.into()], false),
            None,
        );
        let entry = context.append_basic_block(wrap, "entry");
        cg.builder.position_at_end(entry);
        let args_ptr = wrap.get_nth_param(1).unwrap().into_pointer_value();

        // ParseTuple format string + per-parameter out-slot types. Most types
        // produce one out-slot; `String` (bytes) uses `y#`, which writes TWO
        // out-pointers: `(const char** buf, Py_ssize_t* len)`.
        let mut fmt = String::new();
        let mut slot_tys: Vec<BasicTypeEnum> = Vec::new();
        // Per-param index range into `slot_tys`: (start, end).
        let mut param_slots: Vec<(usize, usize)> = Vec::new();
        for (_, ty, _) in &f.params {
            let start = slot_tys.len();
            match ty {
                Ty::I64 => {
                    fmt.push('l');
                    slot_tys.push(i64_ty.into());
                }
                Ty::F64 => {
                    fmt.push('d');
                    slot_tys.push(f64_ty.into());
                }
                Ty::Bool => {
                    fmt.push('p');
                    slot_tys.push(i32_ty.into());
                }
                Ty::Str => {
                    fmt.push('s');
                    slot_tys.push(i8p.into());
                }
                Ty::String => {
                    // Python bytes ↔ Aero String: ParseTuple "y#" yields the raw
                    // byte buffer + its length.
                    fmt.push('y');
                    fmt.push('#');
                    slot_tys.push(i8p.into());    // char** -> buffer
                    slot_tys.push(i64_ty.into()); // Py_ssize_t* -> length
                }
                other => {
                    return Err(CodegenError {
                        msg: format!("`#[py_export]` parameter type `{other}` has no ParseTuple mapping"),
                        line: f.span.line,
                        col: f.span.col,
                    });
                }
            }
            param_slots.push((start, slot_tys.len()));
        }
        let fmt_ptr = bld(cg.builder.build_global_string_ptr(&fmt, "pyfmt"))?;
        let mut slots = Vec::new();
        for slot_ty in &slot_tys {
            slots.push(bld(cg.builder.build_alloca(*slot_ty, "pyarg"))?);
        }
        // PyArg_ParseTuple(args, fmt, &a1, ..., &an)
        let mut parse_args = vec![args_ptr.into(), fmt_ptr.as_pointer_value().into()];
        for s in &slots {
            parse_args.push((*s).into());
        }
        let parse_res = bld(cg.builder.build_call(pyarg_parsetuple, &parse_args, "pyparse"))?;
        let parse_ok = parse_res
            .try_as_basic_value()
            .basic()
            .expect("PyArg_ParseTuple returns int")
            .into_int_value();
        let ok_bb = context.append_basic_block(wrap, "ok");
        let fail_bb = context.append_basic_block(wrap, "fail");
        let parse_cond = bld(cg.builder.build_int_compare(
            IntPredicate::NE,
            parse_ok,
            i32_ty.const_zero(),
            "pyparse_ok",
        ))?;
        bld(cg.builder.build_conditional_branch(parse_cond, ok_bb, fail_bb))?;
        // fail: PyArg_ParseTuple already set the exception; return NULL.
        cg.builder.position_at_end(fail_bb);
        bld(cg.builder.build_return(Some(&i8p.const_null())))?;
        // ok: call the Aero function and build the return object.
        cg.builder.position_at_end(ok_bb);
        let func_llvm = cg.funcs[*idx];
        let mut aero_args: Vec<BasicMetadataValueEnum> = Vec::new();
        for (pi, (_, ty, _)) in f.params.iter().enumerate() {
            let (start, _end) = param_slots[pi];
            match ty {
                Ty::Bool => {
                    let v = bld(cg.builder.build_load(slot_tys[start], slots[start], "arg"))?;
                    let t = bld(cg.builder.build_int_truncate(
                        v.into_int_value(),
                        i1_ty,
                        "argb",
                    ))?;
                    aero_args.push(t.into());
                }
                Ty::String => {
                    // bytes → String: ParseTuple "y#" wrote (buf, len) into the two
                    // slots. Build a `{ data, len, cap }` String struct by value: copy
                    // the bytes into a malloc'd buffer (the Aero String owns it).
                    let buf = bld(cg.builder.build_load(slot_tys[start], slots[start], "arg_buf"))?
                        .into_pointer_value();
                    let len = bld(cg.builder.build_load(
                        slot_tys[start + 1],
                        slots[start + 1],
                        "arg_len",
                    ))?
                    .into_int_value();
                    let str_ty = context.struct_type(
                        &[i8p.into(), i64_ty.into(), i64_ty.into()],
                        false,
                    );
                    let tmp = bld(cg.builder.build_alloca(str_ty, "py_str"))?;
                    let data = bld(cg.builder.build_call(cg.malloc, &[len.into()], "py_str_alloc"))?
                        .try_as_basic_value()
                        .basic()
                        .expect("malloc returned no value")
                        .into_pointer_value();
                    bld(cg.builder.build_call(
                        cg.memcpy,
                        &[data.into(), buf.into(), len.into()],
                        "py_str_cpy",
                    ))?;
                    let zero = i32_ty.const_zero();
                    let one = i32_ty.const_int(1, false);
                    let two = i32_ty.const_int(2, false);
                    let d = bld(unsafe {
                        cg.builder.build_in_bounds_gep(str_ty, tmp, &[zero, zero], "p_str_d")
                    })?;
                    let l = bld(unsafe {
                        cg.builder.build_in_bounds_gep(str_ty, tmp, &[zero, one], "p_str_l")
                    })?;
                    let c = bld(unsafe {
                        cg.builder.build_in_bounds_gep(str_ty, tmp, &[zero, two], "p_str_c")
                    })?;
                    bld(cg.builder.build_store(d, data))?;
                    bld(cg.builder.build_store(l, len))?;
                    bld(cg.builder.build_store(c, len))?;
                    let sv = bld(cg.builder.build_load(str_ty, tmp, "py_str_val"))?;
                    aero_args.push(sv.into());
                }
                _ => {
                    let v = bld(cg.builder.build_load(slot_tys[start], slots[start], "arg"))?;
                    aero_args.push(v.into());
                }
            }
        }
        let ret_val = bld(cg.builder.build_call(func_llvm, &aero_args, "pyret"))?;
        match &f.ret {
            Some(Ty::I64) => {
                let v = ret_val
                    .try_as_basic_value()
                    .basic()
                    .expect("i64 return")
                    .into_int_value();
                let obj = bld(cg.builder.build_call(
                    pylong_fromlonglong,
                    &[v.into()],
                    "pyobj",
                ))?
                .try_as_basic_value()
                .basic()
                .expect("PyLong_FromLongLong returns pointer");
                bld(cg.builder.build_return(Some(&obj)))?;
            }
            Some(Ty::F64) => {
                let v = ret_val
                    .try_as_basic_value()
                    .basic()
                    .expect("f64 return")
                    .into_float_value();
                let obj = bld(cg.builder.build_call(
                    pyfloat_fromdouble,
                    &[v.into()],
                    "pyobj",
                ))?
                .try_as_basic_value()
                .basic()
                .expect("PyFloat_FromDouble returns pointer");
                bld(cg.builder.build_return(Some(&obj)))?;
            }
            Some(Ty::Bool) => {
                let v = ret_val
                    .try_as_basic_value()
                    .basic()
                    .expect("bool return")
                    .into_int_value();
                let z = bld(cg.builder.build_int_z_extend(v, i64_ty, "pybool"))?;
                let obj = bld(cg.builder.build_call(pybool_fromlong, &[z.into()], "pyobj"))?
                    .try_as_basic_value()
                    .basic()
                    .expect("PyBool_FromLong returns pointer");
                bld(cg.builder.build_return(Some(&obj)))?;
            }
            Some(Ty::Str) => {
                let v = ret_val
                    .try_as_basic_value()
                    .basic()
                    .expect("str return")
                    .into_pointer_value();
                let obj = bld(cg.builder.build_call(
                    pyunicode_fromstring,
                    &[v.into()],
                    "pyobj",
                ))?
                .try_as_basic_value()
                .basic()
                .expect("PyUnicode_FromString returns pointer");
                bld(cg.builder.build_return(Some(&obj)))?;
            }
            Some(Ty::String) => {
                // String (bytes) return: the Aero function returns a
                // `{ data, len, cap }` struct by value. Extract data + len, copy
                // them into a Python `bytes` object (PyBytes_FromStringAndSize
                // copies), then free the transferred Aero buffer (ownership moved
                // to the wrapper on return).
                let sv = ret_val
                    .try_as_basic_value()
                    .basic()
                    .expect("String return")
                    .into_struct_value();
                let data = bld(cg.builder.build_extract_value(sv, 0, "py_s_data"))?
                    .into_pointer_value();
                let len = bld(cg.builder.build_extract_value(sv, 1, "py_s_len"))?
                    .into_int_value();
                let obj = bld(cg.builder.build_call(
                    pybytes_fromstringandsize,
                    &[data.into(), len.into()],
                    "pyobj",
                ))?
                .try_as_basic_value()
                .basic()
                .expect("PyBytes_FromStringAndSize returns pointer");
                bld(cg.builder.build_call(cg.free, &[data.into()], "py_s_free"))?;
                bld(cg.builder.build_return(Some(&obj)))?;
            }
            Some(Ty::Void) | None => {
                // Void return: return the immortal `Py_None` (borrowed reference).
                let none = if let Some(imp) = py_none_imp {
                    bld(cg.builder.build_load(i8p, imp.as_pointer_value(), "py_none"))?
                } else {
                    py_none_plain.expect("plain Py_None").as_pointer_value().into()
                };
                bld(cg.builder.build_return(Some(&none)))?;
            }
            Some(other) => {
                return Err(CodegenError {
                    msg: format!("`#[py_export]` return type `{other}` has no conversion"),
                    line: f.span.line,
                    col: f.span.col,
                });
            }
        }
        // Name global for the method-table `ml_name` field.
        let name_arr = context.const_string(py_name.as_bytes(), true);
        let name_gv = module.add_global(
            name_arr.get_type(),
            None,
            &format!("__aero_py_methname_{py_name}"),
        );
        name_gv.set_initializer(&name_arr);
        wraps.push(WrapInfo {
            wrap,
            name_ptr: name_gv.as_pointer_value().const_cast(i8p),
        });
    }

    // 2. Method table: one `PyMethodDef` per export, NULL-terminated.
    let mut method_inits: Vec<inkwell::values::StructValue<'ctx>> = Vec::new();
    for w in &wraps {
        let wrap_i8 = w.wrap.as_global_value().as_pointer_value().const_cast(i8p);
        let init: Vec<BasicValueEnum> = vec![
            w.name_ptr.into(),
            wrap_i8.into(),
            i32_ty.const_int(1, false).into(), // METH_VARARGS
            i8p.const_null().into(),           // ml_doc
        ];
        method_inits.push(method_def_ty.const_named_struct(&init));
    }
    // Terminator entry { NULL, NULL, 0, NULL }
    let term: Vec<BasicValueEnum> = vec![
        i8p.const_null().into(),
        i8p.const_null().into(),
        i32_ty.const_zero().into(),
        i8p.const_null().into(),
    ];
    method_inits.push(method_def_ty.const_named_struct(&term));
    let methods_gv = module.add_global(
        method_def_ty.array_type(method_inits.len() as u32),
        None,
        "__aero_py_methods",
    );
    methods_gv.set_initializer(&method_def_ty.const_array(&method_inits));
    let methods_ptr = methods_gv.as_pointer_value().const_cast(i8p);

    // 3. PyModuleDef global.
    let name_arr = context.const_string(module_name.as_bytes(), true);
    let module_name_ptr = module.add_global(name_arr.get_type(), None, "__aero_py_name");
    module_name_ptr.set_initializer(&name_arr);
    let def_inits: Vec<BasicValueEnum> = vec![
        i64_ty.const_int(1, false).into(),          // ob_refcnt
        i8p.const_null().into(),                    // ob_type
        i8p.const_null().into(),                    // m_init
        i64_ty.const_zero().into(),                 // m_index
        i8p.const_null().into(),                    // m_copy
        module_name_ptr.as_pointer_value().const_cast(i8p).into(), // m_name
        i8p.const_null().into(),                    // m_doc
        i64_ty.const_int(u64::MAX, false).into(),   // m_size = -1
        methods_ptr.into(),                         // m_methods
        i8p.const_null().into(),                    // m_slots
        i8p.const_null().into(),                    // m_traverse
        i8p.const_null().into(),                    // m_clear
        i8p.const_null().into(),                    // m_free
    ];
    let moduledef_gv = module.add_global(module_def_ty, None, "__aero_py_moduledef");
    moduledef_gv.set_initializer(&module_def_ty.const_named_struct(&def_inits));
    moduledef_gv.set_linkage(inkwell::module::Linkage::External);
    moduledef_gv.set_visibility(GlobalVisibility::Default);
    moduledef_gv.set_dll_storage_class(DLLStorageClass::Export);

    // 4. PyInit_<module> entry: PyModule_Create(&moduledef). This is the symbol
    // Python's importer looks up by GetProcAddress/dlsym, so it must be exported.
    let init_name = format!("PyInit_{module_name}");
    let init = module.add_function(&init_name, i8p.fn_type(&[], false), None);
    init.as_global_value().set_visibility(GlobalVisibility::Default);
    init.as_global_value().set_dll_storage_class(DLLStorageClass::Export);
    let entry = context.append_basic_block(init, "entry");
    cg.builder.position_at_end(entry);
    let mdef_ptr = bld(cg.builder.build_pointer_cast(
        moduledef_gv.as_pointer_value(),
        i8p,
        "mdef_i8",
    ))?;
    let ret = bld(cg.builder.build_call(
        pymodule_create2,
        &[
            mdef_ptr.into(),
            i32_ty.const_int(spec.api_version as u64, false).into(),
        ],
        "pymod",
    ))?;
    let ret = ret
        .try_as_basic_value()
        .basic()
        .expect("PyModule_Create2 returns pointer");
    bld(cg.builder.build_return(Some(&ret)))?;

    Ok(())
}

/// Source span of a statement (used for coverage line attribution).
fn stmt_span(stmt: &HirStmt) -> Span {
    match stmt {
        HirStmt::Let { span, .. }
        | HirStmt::Assign { span, .. }
        | HirStmt::AssignIndex { span, .. }
        | HirStmt::AssignDeref { span, .. }
        | HirStmt::AssignField { span, .. }
        | HirStmt::If { span, .. }
        | HirStmt::While { span, .. }
        | HirStmt::Loop { span, .. }
        | HirStmt::For { span, .. }
        | HirStmt::Match { span, .. }
        | HirStmt::StructDef { span, .. }
        | HirStmt::EnumDef { span, .. }
        | HirStmt::TraitDef { span, .. }
        | HirStmt::ImplBlock { span, .. }
        | HirStmt::Return(_, span)
        | HirStmt::Break(span)
        | HirStmt::Continue(span)
        | HirStmt::Print(_, span)
        | HirStmt::Expr(_, span) => *span,
    }
}

/// Build the `__aero_cov_fini` body: open `aero.cov.txt`, write one `line count` pair
/// per counter that was ever hit, then close it. Registered via `atexit` at main so it
/// runs at every exit path (normal return, `return`, `exit`).
fn build_cov_fini<'ctx>(
    cg: &mut Codegen<'_, 'ctx>,
    fini: FunctionValue<'ctx>,
) -> Result<(), CodegenError> {
    let i64_ty = cg.i64_ty;
    let fopen = cg.fopen;
    let fclose = cg.fclose;
    let fprintf = cg.fprintf;
    let counts = cg
        .cov_counts
        .expect("cov_counts present in cov mode")
        .as_pointer_value();
    let lines = cg
        .cov_lines
        .expect("cov_lines present in cov mode")
        .as_pointer_value();

    let entry = cg.context.append_basic_block(fini, "entry");
    let init = cg.context.append_basic_block(fini, "init");
    let head = cg.context.append_basic_block(fini, "head");
    let body = cg.context.append_basic_block(fini, "body");
    let fmt_bb = cg.context.append_basic_block(fini, "fmt");
    let inc = cg.context.append_basic_block(fini, "inc");
    let done = cg.context.append_basic_block(fini, "done");
    let ret = cg.context.append_basic_block(fini, "ret");

    cg.builder.position_at_end(entry);
    let fname = bld(cg.builder.build_global_string_ptr("aero.cov.txt", "cov_fname"))?
        .as_pointer_value();
    let mode = bld(cg.builder.build_global_string_ptr("wb", "cov_mode"))?.as_pointer_value();
    let f = bld(cg.builder.build_call(fopen, &[fname.into(), mode.into()], "cov_file"))?
        .try_as_basic_value()
        .basic()
        .ok_or_else(|| CodegenError {
            msg: "cov fopen returned no value".to_string(),
            line: 0,
            col: 0,
        })?
        .into_pointer_value();
    let is_null = bld(cg.builder.build_is_null(f, "cov_file_null"))?;
    bld(cg.builder.build_conditional_branch(is_null, ret, init))?;

    // init: i = 0
    cg.builder.position_at_end(init);
    let ivar = bld(cg.builder.build_alloca(i64_ty, "cov_i"))?;
    bld(cg.builder.build_store(ivar, i64_ty.const_zero()))?;
    bld(cg.builder.build_unconditional_branch(head))?;

    // head: i < COV_CAP ?
    cg.builder.position_at_end(head);
    let i = bld(cg.builder.build_load(i64_ty, ivar, "i"))?.into_int_value();
    let cond = bld(cg.builder.build_int_compare(
        IntPredicate::ULT,
        i,
        i64_ty.const_int(COV_CAP, false),
        "lt",
    ))?;
    bld(cg.builder.build_conditional_branch(cond, body, done))?;

    // body: read count + line for counter i; skip zero pads.
    cg.builder.position_at_end(body);
    let c_ptr = unsafe { bld(cg.builder.build_in_bounds_gep(i64_ty, counts, &[i], "cp"))? };
    let c = bld(cg.builder.build_load(i64_ty, c_ptr, "c"))?.into_int_value();
    let l_ptr = unsafe { bld(cg.builder.build_in_bounds_gep(i64_ty, lines, &[i], "lp"))? };
    let l = bld(cg.builder.build_load(i64_ty, l_ptr, "l"))?.into_int_value();
    let hit = bld(cg.builder.build_int_compare(
        IntPredicate::NE,
        c,
        i64_ty.const_zero(),
        "c_ne",
    ))?;
    let has_line = bld(cg.builder.build_int_compare(
        IntPredicate::NE,
        l,
        i64_ty.const_zero(),
        "l_ne",
    ))?;
    let any = bld(cg.builder.build_or(hit, has_line, "any"))?;
    bld(cg.builder.build_conditional_branch(any, fmt_bb, inc))?;

    // fmt: fprintf(f, "%ld %ld\n", line, count)
    cg.builder.position_at_end(fmt_bb);
    let fmt = bld(cg.builder.build_global_string_ptr("%ld %ld\n", "cov_fmt"))?.as_pointer_value();
    bld(cg.builder.build_call(
                fprintf,
                &[f.into(), fmt.into(), l.into(), c.into()],
                "cov_line",
            ))?;
    bld(cg.builder.build_unconditional_branch(inc))?;

    // inc: i = i + 1; loop
    cg.builder.position_at_end(inc);
    let nxt = bld(cg.builder.build_int_add(i, i64_ty.const_int(1, false), "i_next"))?;
    bld(cg.builder.build_store(ivar, nxt))?;
    bld(cg.builder.build_unconditional_branch(head))?;

    // done: fclose(f); ret
    cg.builder.position_at_end(done);
    bld(cg.builder.build_call(fclose, &[f.into()], "cov_close"))?;
    bld(cg.builder.build_unconditional_branch(ret))?;

    cg.builder.position_at_end(ret);
    bld(cg.builder.build_return(None))?;
    Ok(())
}

/// Build the substitution map for a generic struct/enum instance: map each type
/// parameter to its concrete argument (substituted through the outer `subst` first,
/// so relative instances inside generic bodies resolve correctly).
fn instance_subst(
    type_params: &[String],
    args: &[Ty],
    subst: &HashMap<String, Ty>,
) -> HashMap<String, Ty> {
    let mut merged = subst.clone();
    for (p, a) in type_params.iter().zip(args.iter()) {
        merged.insert(p.clone(), substitute(a, subst));
    }
    merged
}

/// Map an Aero type to an LLVM type. Arrays/tuples map to aggregate types.
/// `subst` maps the current generic instance type parameters (`Generic(name)` to concrete);
/// pass an empty map outside generic contexts.
///
/// Enum layout: a tagged union `{ i64 tag, [N x i8] payload }` — the tag holds the
/// variant index; the payload is a byte buffer sized to the widest variant payload
/// (values are memcpy'd in/out by the variant's own type).
fn llvm_ty<'ctx>(
    context: &'ctx Context,
    ty: &Ty,
    span: Span,
    subst: &HashMap<String, Ty>,
    structs: &[HirStructDef],
    unions: &[HirUnionDef],
    enums: &[HirEnumDef],
) -> Result<BasicTypeEnum<'ctx>, CodegenError> {
    match ty {
        Ty::I32 => Ok(context.i32_type().into()),
        Ty::I64 => Ok(context.i64_type().into()),
        Ty::F32 => Ok(context.f32_type().into()),
        Ty::F64 => Ok(context.f64_type().into()),
        Ty::Char => Ok(context.i32_type().into()),
        Ty::Bool => Ok(context.bool_type().into()),
        Ty::Str => Ok(context.ptr_type(AddressSpace::from(0u16)).into()),
        Ty::Array(elem, n) => {
            let elem_ty = llvm_ty(context, elem, span, subst, structs, unions, enums)?;
            Ok(elem_ty.array_type(*n as u32).into())
        }
        Ty::Tensor { elem, shape } => {
            // Multi-dim tensors map to nested arrays: tensor<3x4> -> [3 x [4 x i64]]
            let mut t = llvm_ty(context, elem, span, subst, structs, unions, enums)?;
            for d in shape.iter().rev() {
                t = t.array_type(*d as u32).into();
            }
            Ok(t)
        }
        Ty::Tuple(elems) => {
            let mut tys = Vec::new();
            for e in elems {
                tys.push(llvm_ty(context, e, span, subst, structs, unions, enums)?.into());
            }
            Ok(context.struct_type(&tys, false).into())
        }
        Ty::Struct(name) => {
            // Look up the struct definition and build an LLVM struct type from its fields
            let def = structs.iter().find(|s| s.name == *name).ok_or_else(|| CodegenError {
                msg: format!("internal error: struct `{name}` not found in definition table"),
                line: span.line,
                col: span.col,
            })?;
            let mut tys = Vec::new();
            for (_, fty) in &def.fields {
                tys.push(llvm_ty(context, fty, span, subst, structs, unions, enums)?.into());
            }
            Ok(context.struct_type(&tys, false).into())
        }
        Ty::Union(name) => {
            // Union layout: a byte buffer `[N x i8]` where N is the size of the
            // largest field. All fields live at byte offset 0; field reads/writes
            // bitcast this buffer to the field's type (see gen_field_ptr).
            let def = unions.iter().find(|u| u.name == *name).ok_or_else(|| CodegenError {
                msg: format!("internal error: union `{name}` not found in definition table"),
                line: span.line,
                col: span.col,
            })?;
            let size = union_payload_size(def, structs, unions, enums, subst);
            Ok(context.i8_type().array_type(size as u32).into())
        }
        Ty::StructGeneric { name, args } => {
            // Monomorphized struct instance: substitute the type args into the fields.
            let def = structs.iter().find(|s| s.name == *name).ok_or_else(|| CodegenError {
                msg: format!("internal error: struct `{name}` not found in definition table"),
                line: span.line,
                col: span.col,
            })?;
            if def.type_params.len() != args.len() {
                return Err(CodegenError {
                    msg: format!(
                        "internal error: struct `{name}` type-argument count mismatch (declared {}, got {})",
                        def.type_params.len(),
                        args.len()
                    ),
                    line: span.line,
                    col: span.col,
                });
            }
            let merged = instance_subst(&def.type_params, args, subst);
            let mut tys = Vec::new();
            for (_, fty) in &def.fields {
                tys.push(llvm_ty(context, fty, span, &merged, structs, unions, enums)?.into());
            }
            Ok(context.struct_type(&tys, false).into())
        }
        Ty::Enum(name) => {
            // Tagged union: `{ i64 tag, [N x i8] payload }` where N is the byte size of the
            // widest payload among the variants (8 if none carry a payload).
            let def = enums.iter().find(|e| e.name == *name).ok_or_else(|| CodegenError {
                msg: format!("internal error: enum `{name}` not found in definition table"),
                line: span.line,
                col: span.col,
            })?;
            let payload = enum_payload_ty(def, structs, unions, enums, subst);
            let payload_size = match &payload {
                Some(t) => aero_size(t, structs, unions, enums, subst),
                None => 8,
            };
            let payload_bytes = context.i8_type().array_type(payload_size as u32);
            Ok(context
                .struct_type(
                    &[context.i64_type().into(), payload_bytes.into()],
                    false,
                )
                .into())
        }
        Ty::EnumGeneric { name, args } => {
            // Monomorphized enum instance: substitute the type args into the payload types.
            let def = enums.iter().find(|e| e.name == *name).ok_or_else(|| CodegenError {
                msg: format!("internal error: enum `{name}` not found in definition table"),
                line: span.line,
                col: span.col,
            })?;
            if def.type_params.len() != args.len() {
                return Err(CodegenError {
                    msg: format!(
                        "internal error: enum `{name}` type-argument count mismatch (declared {}, got {})",
                        def.type_params.len(),
                        args.len()
                    ),
                    line: span.line,
                    col: span.col,
                });
            }
            let merged = instance_subst(&def.type_params, args, subst);
            let payload = enum_payload_ty(def, structs, unions, enums, &merged);
            let payload_size = match &payload {
                Some(t) => aero_size(t, structs, unions, enums, &merged),
                None => 8,
            };
            let payload_bytes = context.i8_type().array_type(payload_size as u32);
            Ok(context
                .struct_type(
                    &[context.i64_type().into(), payload_bytes.into()],
                    false,
                )
                .into())
        }
        Ty::Ref { inner, .. } => {
            // LLVM 15+ pointers are opaque (no inner type distinction)
            llvm_ty(context, inner, span, subst, structs, unions, enums)?;
            Ok(context.ptr_type(AddressSpace::from(0u16)).into())
        }
        Ty::Ptr(inner) => {
            llvm_ty(context, inner, span, subst, structs, unions, enums)?;
            Ok(context.ptr_type(AddressSpace::from(0u16)).into())
        }
        // Native `Vec<T>`: `{ data: i8*, len: i64, cap: i64 }` (growable heap buffer).
        Ty::Vec(_) => {
            let data = context.ptr_type(AddressSpace::from(0u16));
            let i64t = context.i64_type();
            Ok(context.struct_type(&[data.into(), i64t.into(), i64t.into()], false).into())
        }
        // Native `String`: `{ data: i8*, len: i64, cap: i64 }` (NUL-terminated heap buffer).
        Ty::String => {
            let data = context.ptr_type(AddressSpace::from(0u16));
            let i64t = context.i64_type();
            Ok(context.struct_type(&[data.into(), i64t.into(), i64t.into()], false).into())
        }
        // Native `Box<T>`: a single `i8*` to a heap-allocated `T`.
        Ty::Box(_) => Ok(context.ptr_type(AddressSpace::from(0u16)).into()),
        Ty::Arena(_) => Err(CodegenError {
            msg: "internal error: arena type used as an ordinary value type".to_string(),
            line: span.line,
            col: span.col,
        }),
        // `dyn Trait`: a fat pointer `{ data: i8*, vtable: i8* }`. `data` points to
        // a heap-allocated copy of the concrete value; `vtable` points to an array of
        // function pointers, one per trait method (in declaration order).
        Ty::Dyn { .. } => {
            let ptr = context.ptr_type(AddressSpace::from(0u16));
            Ok(context.struct_type(&[ptr.into(), ptr.into()], false).into())
        }
        Ty::Fn(params, ret) => {
            // A first-class function pointer: the LLVM type is a pointer to the
            // underlying function type `fn(params...) -> ret`.
            let mut param_tys = Vec::new();
            for p in params {
                param_tys.push(llvm_ty(context, p, span, subst, structs, unions, enums)?.into());
            }
            let fn_ty = if matches!(&**ret, Ty::Void) {
                context.void_type().fn_type(&param_tys, false)
            } else {
                let ret_ty = llvm_ty(context, ret, span, subst, structs, unions, enums)?;
                ret_ty.fn_type(&param_tys, false)
            };
            Ok(context.ptr_type(AddressSpace::from(0u16)).into())
        }
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
        Ty::Assoc(name) => match subst.get(name) {
            Some(concrete) => llvm_ty(context, concrete, span, subst, structs, unions, enums),
            None => Err(CodegenError {
                msg: format!("internal error: associated type `Self::{name}` was not substituted before codegen"),
                line: span.line,
                col: span.col,
            }),
        },
        Ty::Generic(name) => match subst.get(name) {
            // Instantiated generic param: substitute the concrete type and recurse
            Some(concrete) => llvm_ty(context, concrete, span, subst, structs, unions, enums),
            // Uninstantiated generic param: the function was compiled as ordinary
            None => Err(CodegenError {
                msg: format!("internal error: generic parameter `{name}` was not instantiated (generic functions must be called via instantiation)"),
                line: span.line,
                col: span.col,
            }),
        },
    }
}

/// The widest payload type among an enum's variants (`None` if no variant carries
/// a payload). Used as the LLVM payload field type / byte-buffer size. Generic
/// payloads are substituted through `subst` (and any instance type args).
fn enum_payload_ty(
    def: &HirEnumDef,
    structs: &[HirStructDef],
    unions: &[HirUnionDef],
    enums: &[HirEnumDef],
    subst: &HashMap<String, Ty>,
) -> Option<Ty> {
    let mut best: Option<Ty> = None;
    for (_, p) in &def.variants {
        let p = match p {
            Some(t) => substitute(t, subst),
            None => continue,
        };
        let bigger = match &best {
            Some(b) => {
                aero_size(&p, structs, unions, enums, subst)
                    > aero_size(b, structs, unions, enums, subst)
            }
            None => true,
        };
        if bigger {
            best = Some(p);
        }
    }
    best
}

/// Byte size of a union's storage: the size of its largest field (8 if empty).
fn union_payload_size(
    def: &HirUnionDef,
    structs: &[HirStructDef],
    unions: &[HirUnionDef],
    enums: &[HirEnumDef],
    subst: &HashMap<String, Ty>,
) -> u64 {
    def.fields
        .iter()
        .map(|(_, t)| aero_size(t, structs, unions, enums, subst))
        .max()
        .unwrap_or(8)
}

/// Aero-level size estimate (in bytes) of a type's LLVM layout. Mirrors how the
/// types above are laid out: scalars by width, aggregates as the sum of their parts.
/// `subst` resolves generic parameters (and nested generic instance args).
fn aero_size(
    ty: &Ty,
    structs: &[HirStructDef],
    unions: &[HirUnionDef],
    enums: &[HirEnumDef],
    subst: &HashMap<String, Ty>,
) -> u64 {
    match ty {
        Ty::I32 | Ty::F32 | Ty::Char => 4,
        Ty::I64 | Ty::F64 | Ty::Str => 8,
        Ty::Bool => 1,
        Ty::Array(elem, n) => aero_size(elem, structs, unions, enums, subst) * (*n as u64),
        Ty::Tuple(elems) => elems
            .iter()
            .map(|t| aero_size(t, structs, unions, enums, subst))
            .sum(),
        Ty::Tensor { elem, shape } => {
            let total: u64 = shape.iter().map(|d| *d as u64).product();
            aero_size(elem, structs, unions, enums, subst) * total
        }
        Ty::Struct(name) => match structs.iter().find(|s| s.name == *name) {
            Some(def) => def
                .fields
                .iter()
                .map(|(_, t)| aero_size(t, structs, unions, enums, subst))
                .sum(),
            None => 8,
        },
        Ty::Union(name) => match unions.iter().find(|u| u.name == *name) {
            Some(def) => union_payload_size(def, structs, unions, enums, subst),
            None => 8,
        },
        Ty::StructGeneric { name, args } => {
            match structs.iter().find(|s| s.name == *name) {
                Some(def) => {
                    let merged = instance_subst(&def.type_params, args, subst);
                    def.fields
                        .iter()
                        .map(|(_, t)| aero_size(t, structs, unions, enums, &merged))
                        .sum()
                }
                None => 8,
            }
        }
        Ty::Enum(name) => match enums.iter().find(|e| e.name == *name) {
            Some(def) => match enum_payload_ty(def, structs, unions, enums, subst) {
                Some(p) => 8 + aero_size(&p, structs, unions, enums, subst),
                None => 16,
            },
            None => 16,
        },
        Ty::EnumGeneric { name, args } => match enums.iter().find(|e| e.name == *name) {
            Some(def) => {
                let merged = instance_subst(&def.type_params, args, subst);
                match enum_payload_ty(def, structs, unions, enums, &merged) {
                    Some(p) => 8 + aero_size(&p, structs, unions, enums, &merged),
                    None => 16,
                }
            }
            None => 16,
        },
        // Native Vec is a { i8*, i64, i64 } struct (24 bytes on this ABI)
        Ty::Vec(_) => 24,
        // Native String is a { i8*, i64, i64 } struct (24 bytes on this ABI)
        Ty::String => 24,
        // Native Box is a single i8* (8 bytes on this ABI)
        Ty::Box(_) => 8,
        // References/pointers are 8-byte addresses; everything else defaults to 8.
        Ty::Ref { .. } | Ty::Ptr(_) | Ty::Arena(_) | Ty::Fn(..) | Ty::Void | Ty::Var(_)
        | Ty::Generic(_) | Ty::Assoc(_) => 8,
        // `dyn Trait` is a fat pointer `{ data: i8*, vtable: i8* }` (16 bytes)
        Ty::Dyn { .. } => 16,
    }
}

fn is_agg(ty: &Ty) -> bool {
    matches!(
        ty,
        Ty::Array(..)
            | Ty::Tuple(_)
            | Ty::Tensor { .. }
            | Ty::Struct(_)
            | Ty::Union(_)
            | Ty::Enum(_)
            | Ty::StructGeneric { .. }
            | Ty::EnumGeneric { .. }
            | Ty::Vec(_)
            | Ty::String
    )
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
    /// Per-scope moved-variable sets (from the borrow checker). Variables in the
    /// current scope's moved set are NOT dropped (ownership was transferred).
    moved_by_scope: &'a HashMap<ScopeId, HashSet<DefId>>,
    /// Declaration order of all variables in the current function (params first,
    /// then `let`s in source order) — used to emit drops in reverse declaration order.
    decl_order: Vec<DefId>,
    /// Enclosing block scopes of the current codegen position (for drop-at-return).
    scope_stack: Vec<ScopeId>,
    /// Counter for string global constants
    str_counter: u32,
    printf: FunctionValue<'ctx>,
    /// `abort()` (called on arena out-of-bounds)
    abort: FunctionValue<'ctx>,
    /// String-runtime libc helpers (malloc/free/memcpy/strlen/strcmp/snprintf)
    malloc: FunctionValue<'ctx>,
    free: FunctionValue<'ctx>,
    memcpy: FunctionValue<'ctx>,
    memset: FunctionValue<'ctx>,
    strlen: FunctionValue<'ctx>,
    strcmp: FunctionValue<'ctx>,
    memcmp: FunctionValue<'ctx>,
    snprintf: FunctionValue<'ctx>,
    /// strtoll (string -> integer parse) and strstr (substring search)
    strtoll: FunctionValue<'ctx>,
    strstr: FunctionValue<'ctx>,
    /// File IO helpers (fopen/fclose/fread/fwrite/fseek/ftell)
    fopen: FunctionValue<'ctx>,
    fclose: FunctionValue<'ctx>,
    fprintf: FunctionValue<'ctx>,
    fread: FunctionValue<'ctx>,
    fwrite: FunctionValue<'ctx>,
    fseek: FunctionValue<'ctx>,
    ftell: FunctionValue<'ctx>,
    /// Time/random helpers (stdlib extension): rand() -> i32, time(NULL) -> i64
    rand: FunctionValue<'ctx>,
    time: FunctionValue<'ctx>,
    /// Environment helpers (stdlib Phase 1): getenv/putenv
    getenv: FunctionValue<'ctx>,
    putenv: FunctionValue<'ctx>,
    /// UTF-8 String helpers (stdlib Phase 1, "字符串 2.0"): self-contained module
    /// functions so both JIT and AOT link cleanly. Operate on a NUL-terminated
    /// byte buffer + length.
    utf8_len_f: FunctionValue<'ctx>,
    utf8_at_f: FunctionValue<'ctx>,
    utf8_push_f: FunctionValue<'ctx>,
    utf8_pop_f: FunctionValue<'ctx>,
    /// CLI-argument globals (written at main entry)
    aero_argc: GlobalValue<'ctx>,
    aero_argv: GlobalValue<'ctx>,
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
    /// Struct definitions (field type/index lookup for codegen)
    hir_structs: &'a [HirStructDef],
    /// Union definitions (field type/index lookup for codegen)
    hir_unions: &'a [HirUnionDef],
    /// Top-level const definitions (Phase P0-3): values evaluated at compile time.
    hir_consts: &'a [HirConstDef],
    /// Enum definitions (variant index/payload lookup for codegen)
    hir_enums: &'a [HirEnumDef],
    /// Method resolution table: (type_name, method_name) → function DefId.
    /// Both trait methods and inherent methods are registered here by lowering.
    method_map: &'a HashMap<(String, String), DefId>,
    /// Trait definitions (for `dyn Trait` vtable layout: method order/arity)
    hir_traits: &'a [HirTraitDef],
    /// Impl blocks (for `dyn Trait` vtable content: which concrete method each
    /// trait method maps to for a given concrete type)
    hir_impls: &'a [HirImplBlock],
    /// Cached vtable globals for `dyn Trait`: (concrete_type_name, trait_name) →
    /// the vtable global's pointer (an array of trait-method thunk pointers).
    dyn_vtables: HashMap<(String, String), PointerValue<'ctx>>,
    /// User function table (HIR level, for expression type lookups)
    call_types: &'a HashMap<usize, Vec<Ty>>,
    /// Generic call sites span.start → type args (from inference)
    /// Generic struct literal span.start → concrete type args (from inference)
    struct_lit_types: &'a HashMap<usize, Vec<Ty>>,
    /// Generic enum literal span.start → concrete type args (from inference)
    enum_lit_types: &'a HashMap<usize, Vec<Ty>>,
    instance_funcs: HashMap<(DefId, Vec<Ty>), FunctionValue<'ctx>>,
    /// Generic instance functions: (fn DefId, type args) → LLVM fn (monomorphization registry)
    instance_count: usize,
    /// Total generic instances generated (guards against infinite monomorphization)
    type_subst: HashMap<String, Ty>,
    /// Stack of (continue_block, break_block) for break/continue inside loops
    loop_stack: Vec<(inkwell::basic_block::BasicBlock<'ctx>, inkwell::basic_block::BasicBlock<'ctx>)>,
    /// Return type of the function currently being generated (None for main/top-level).
    /// Used by `?` to build the `Err` value that is returned on propagation.
    cur_ret: Option<Ty>,
    /// Source-coverage instrumentation (enabled via `AERO_COV`). Each statement gets
    /// one counter slot in a fixed-size global array; `__aero_cov_fini` dumps the
    /// (line, count) map to `aero.cov.txt` at exit for the `aero cov` report.
    cov_mode: bool,
    /// `i64[COV_CAP]` execution-count array (indexed by statement counter id).
    cov_counts: Option<GlobalValue<'ctx>>,
    /// `i64[COV_CAP]` source line for each counter (set as a build-time initializer).
    cov_lines: Option<GlobalValue<'ctx>>,
    /// `__aero_cov_fini` function reference (registered with `atexit`).
    cov_fini: Option<FunctionValue<'ctx>>,
    /// Number of statement counters assigned so far (also the id of the next one).
    cov_next: usize,
    /// Source line recorded for each assigned counter id (`cov_next` entries).
    cov_line_map: Vec<u32>,
    /// Debug-info emission (enabled via `AERO_DEBUG`). When on, per-function
    /// `DISubprogram`s are attached and per-statement `DILocation`s are set, which
    /// LLVM lowers to DWARF (ELF) or PDB (COFF) at object-emission time.
    debug_mode: bool,
    /// DI builder + compilation unit (the unit also provides the source file).
    dibuilder: Option<DebugInfoBuilder<'ctx>>,
    /// The DI compilation unit — function subprogram scope root.
    di_cu: Option<DICompileUnit<'ctx>>,
    /// The DI source file (used by subroutine types & function attachments).
    di_file: Option<DIFile<'ctx>>,
    /// The `DISubprogram` of the function currently being generated (debug location scope).
    di_scope: Option<DISubprogram<'ctx>>,
}

impl<'a, 'ctx> Codegen<'a, 'ctx> {
    /// Type-parameter map of the current generic instance (active while generating its body)
    fn t(&self, ty: &Ty, span: Span) -> Result<BasicTypeEnum<'ctx>, CodegenError> {
        llvm_ty(
            self.context,
            ty,
            span,
            &self.type_subst,
            self.hir_structs,
            self.hir_unions,
            self.hir_enums,
        )
    }

    /// Evaluate a top-level const's value at compile time, returning its scalar
    /// `ConstVal` (Phase P0-3). Recursively resolves const-to-const references and
    /// const fn calls. Errors if the value cannot be folded to a scalar.
    fn eval_const(&self, name: &str, span: Span) -> Result<const_eval::ConstVal, CodegenError> {
        let def = self
            .hir_consts
            .iter()
            .find(|c| c.name == name)
            .ok_or_else(|| self.internal_err(span, &format!("undefined const `{name}`")))?;
        const_eval::const_fold_expr(self.hir_consts, self.hir_funcs, &def.value).ok_or_else(|| {
            self.internal_err(
                span,
                &format!(
                    "const `{name}` cannot be evaluated at compile time (only scalar values are supported)"
                ),
            )
        })
    }

    /// Convert a compile-time `ConstVal` into an LLVM constant of the function's
    /// return type (Phase 12.6). Only used for scalar (int/float/bool/char) returns.
    fn gen_const_val(
        &self,
        cv: &const_eval::ConstVal,
        ret: &Option<Ty>,
        span: Span,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let ty = ret.as_ref().ok_or_else(|| {
            self.internal_err(span, "const fn that returns a value must declare a return type")
        })?;
        let basic = match (cv, ty) {
            (const_eval::ConstVal::Int(n), Ty::I32) => self
                .i32_ty
                .const_int(*n as u64, false)
                .as_basic_value_enum(),
            (const_eval::ConstVal::Int(n), Ty::I64) => self
                .i64_ty
                .const_int(*n as u64, false)
                .as_basic_value_enum(),
            (const_eval::ConstVal::Int(n), Ty::Char) => self
                .i32_ty
                .const_int(*n as u64, false)
                .as_basic_value_enum(),
            (const_eval::ConstVal::Float(f), Ty::F32) => self
                .context
                .f32_type()
                .const_float(*f)
                .as_basic_value_enum(),
            (const_eval::ConstVal::Float(f), Ty::F64) => self
                .context
                .f64_type()
                .const_float(*f)
                .as_basic_value_enum(),
            (const_eval::ConstVal::Bool(b), Ty::Bool) => self
                .bool_ty
                .const_int(if *b { 1 } else { 0 }, false)
                .as_basic_value_enum(),
            (const_eval::ConstVal::Char(c), Ty::Char) => self
                .i32_ty
                .const_int(*c as u64, false)
                .as_basic_value_enum(),
            _ => {
                return Err(self.internal_err(
                    span,
                    &format!(
                        "const fn evaluated to `{cv:?}` which does not match return type `{ty}`"
                    ),
                ))
            }
        };
        Ok(basic)
    }

    /// Map an Aero type to an LLVM type (using the current instance `type_subst`).
    /// Attach a function-level DI subroutine for the debugger: creates a DISubprogram
    /// at the function's source line and records it as the active debug-location scope.
    fn di_attach_function(&mut self, name: &str, span: Span, func: FunctionValue<'ctx>) {
        if !self.debug_mode {
            return;
        }
        let Some(db) = self.dibuilder.as_ref() else { return };
        let Some(cu) = self.di_cu else { return };
        let Some(file) = self.di_file else { return };
        let line = span.line as u32;
        let subt = db.create_subroutine_type(file, None, &[], DIFlags::ZERO);
        let scope = db.create_function(
            cu.as_debug_info_scope(),
            name,
            None,
            file,
            line,
            subt,
            false,
            true,
            line,
            DIFlags::ZERO,
            false,
        );
        func.set_subprogram(scope);
        self.di_scope = Some(scope);
    }

    /// Generate the body of a concrete (non-generic) function.
    fn gen_function(&mut self, f: &HirFn, func_llvm: FunctionValue<'ctx>) -> Result<(), CodegenError> {
        self.cur_func = func_llvm;
        self.cur_ret = f.ret.clone();
        let entry = self.context.append_basic_block(func_llvm, "entry");
        self.builder.position_at_end(entry);
        // Debug info: attach a DISubprogram so the debugger maps this function back to
        // its source line, and remember it as the scope for this function's statements.
        self.di_attach_function(&f.name, f.span, func_llvm);
        self.vars.clear();
        self.decl_order.clear();
        for (i, (_, _, _sp)) in f.params.iter().enumerate() {
            let param_val = func_llvm.get_nth_param(i as u32).expect("parameter exists");
            let ptr = bld(self.builder.build_alloca(param_val.get_type(), "arg"))?;
            bld(self.builder.build_store(ptr, param_val))?;
            self.vars.insert(f.param_defs[i], ptr);
            self.decl_order.push(f.param_defs[i]);
        }
        self.gen_block(&f.body)?;
        // Fallback return at the end (type checking guarantees consistent return paths);
        // drop the (still live) parameters in reverse order first (an explicit `return`
        // already dropped every live variable, so the body block is terminated then).
        if !self.cur_block_terminated() {
            self.gen_drop_all_live(f.body.scope_id)?;
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
            param_tys.push(
                llvm_ty(self.context, &inst, *sp, &empty_subst, self.hir_structs, self.hir_unions, self.hir_enums)?.into(),
            );
        }
        let fn_ty = match &f.ret {
            Some(t) => {
                let inst = substitute(t, &subst);
                llvm_ty(self.context, &inst, f.span, &empty_subst, self.hir_structs, self.hir_unions, self.hir_enums)?
                    .fn_type(&param_tys, false)
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
        let saved_decl = std::mem::take(&mut self.decl_order);
        let saved_scopes = std::mem::take(&mut self.scope_stack);
        let saved_func = self.cur_func;
        let saved_di_scope = self.di_scope;
        let saved_block = self.builder.get_insert_block();
        self.type_subst = subst;
        self.gen_function(&f, func)?;
        self.type_subst = saved_subst;
        self.vars = saved_vars;
        self.decl_order = saved_decl;
        self.scope_stack = saved_scopes;
        self.cur_func = saved_func;
        self.di_scope = saved_di_scope;
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

    /// Source-coverage instrumentation for one statement (called from [`gen_block`]).
    /// Assigns a unique counter id + source line, then bumps the counter unless the
    /// statement is unreachable (current block already terminated).
    fn emit_cov(&mut self, stmt: &HirStmt) -> Result<(), CodegenError> {
        if !self.cov_mode {
            return Ok(());
        }
        if self.cov_next >= COV_CAP as usize {
            let sp = stmt_span(stmt);
            return Err(CodegenError {
                msg: format!(
                    "coverage counter overflow ({} >= {COV_CAP}); source has too many statements",
                    self.cov_next
                ),
                line: sp.line,
                col: sp.col,
            });
        }
        let sp = stmt_span(stmt);
        let id = self.cov_next;
        self.cov_next += 1;
        self.cov_line_map.push(sp.line);
        // Unreachable statements still reserve an id (keeps the map aligned) but emit
        // no counter IR, so they report 0 coverage.
        if self.cur_block_terminated() {
            return Ok(());
        }
        let counts = self
            .cov_counts
            .expect("cov_counts present in cov mode")
            .as_pointer_value();
        let idx = self.i64_ty.const_int(id as u64, false);
        let ptr = unsafe {
            bld(self.builder.build_in_bounds_gep(self.i64_ty, counts, &[idx], "cov_ptr"))?
        };
        let cur = self
            .builder
            .build_load(self.i64_ty, ptr, "cov_cur")
            .map_err(|e| self.internal_err(sp, &format!("coverage load failed: {e}")))?
            .into_int_value();
        let nv = self
            .builder
            .build_int_add(cur, self.i64_ty.const_int(1, false), "cov_nv")
            .map_err(|e| self.internal_err(sp, &format!("coverage add failed: {e}")))?;
        self.builder
            .build_store(ptr, nv)
            .map_err(|e| self.internal_err(sp, &format!("coverage store failed: {e}")))?;
        Ok(())
    }

    fn gen_block(&mut self, block: &HirBlock) -> Result<(), CodegenError> {
        let outer: Vec<DefId> = self.vars.keys().copied().collect();
        self.arena_stack.push(Vec::new());
        self.scope_stack.push(block.scope_id);
        for stmt in &block.stmts {
            // Debug info: anchor the next statement's IR at its source line. The scope
            // is the enclosing function's DISubprogram (LLVM discards locations for
            // unreachable blocks via the `NoDebug`/debug-location-on-terminator rule).
            if self.debug_mode {
                if let Some(db) = self.dibuilder.as_ref() {
                    if let Some(scope) = self.di_scope {
                        let sp = stmt_span(stmt);
                        if sp.line > 0 {
                            let loc = db.create_debug_location(
                                self.context,
                                sp.line,
                                sp.col,
                                scope.as_debug_info_scope(),
                                None,
                            );
                            self.builder.set_current_debug_location(loc);
                        }
                    }
                }
            }
            self.emit_cov(stmt)?;
            if self.cur_block_terminated() {
                // Block scope: variables declared in a block are rolled back at its end (local semantics).
            }
            self.gen_stmt(stmt)?;
        }
        // Phase 6 (Drop/RAII): drop the variables declared in this block, in reverse
        // declaration order, before the block's resources are released. Moved values
        // are skipped (their new owner drops them).
        if !self.cur_block_terminated() {
            let to_drop: Vec<DefId> = self
                .decl_order
                .iter()
                .rev()
                .copied()
                .filter(|def| self.vars.contains_key(def) && !outer.contains(def))
                .collect();
            for def in to_drop {
                self.gen_drop_var(def, block.scope_id)?;
            }
        }
        self.scope_stack.pop();
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
                self.decl_order.push(*def_id);
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
            HirStmt::AssignField { target, field, value, span } => {
                // `recv.field = value`: get the field slot inside the struct aggregate.
                let (slot, fty, _recv_ty) = self.gen_field_ptr(target, field, *span)?;
                let v = self.gen_value(value)?.scalar(*span, "field write")?;
                let v = self.coerce(v, &fty, *span, "field write")?;
                bld(self.builder.build_store(slot, v))?;
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
                        // `assert`/`assert_eq` are void (abort on failure). Other
                        // builtins return values; when used as a discarded statement,
                        // generate the value and drop it.
                        if hir_f.name == "assert" || hir_f.name == "assert_eq" || hir_f.name == "str_free" {
                            return self.gen_builtin_call(&hir_f.name, args, *span);
                        }
                        self.gen_value(expr)?;
                        return Ok(());
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
                if let HirExpr::MethodCall { .. } = expr {
                    // Method-call statement (arena reset / void trait method): generate and drop the result
                    self.gen_method_call(expr)?;
                    return Ok(());
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
            HirStmt::Loop { body, .. } => self.gen_loop(body),
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
                // Phase 6 (Drop/RAII): drop every still-live variable (params + block
                // vars) in reverse declaration order before leaving the function. The
                // moved set of the innermost enclosing scope governs which are skipped.
                let scope = *self.scope_stack.last().unwrap_or(&0);
                self.gen_drop_all_live(scope)?;
                match v {
                    Some(val) => {
                        // Coerce the returned value to the function's declared return
                        // type: integer literals default to i64, so `fn f() -> i32 { return 1; }`
                        // needs truncation before the `ret` (else the x86 backend aborts with
                        // "Cannot emit physreg copy instruction").
                        if let Some(rt) = self.cur_func.get_type().get_return_type() {
                            let val = self.coerce(val, &rt, *span, "return value")?;
                            bld(self.builder.build_return(Some(&val)))?;
                        } else {
                            bld(self.builder.build_return(Some(&val)))?;
                        }
                    }
                    None => {
                        bld(self.builder.build_return(None))?;
                    }
                }
                Ok(())
            }
            HirStmt::For {
                var_def,
                iter,
                body,
                span,
                ..
            } => {
                // For-in:
                //  - arrays and `Vec<T>` are iterated with a native index loop;
                //  - user-defined iterables follow the `IntoIterator`/`Iterator`
                //    protocol:
                //      let mut it = iter.into_iter();
                //      loop { match it.next() { Some(x) => body, None => break } }
                let iter_ty = self.expr_ty(iter)?;
                let iter_gv = self.gen_value(iter)?;

                match &iter_ty {
                    Ty::Array(elem_ty, n) => {
                        let arr_ptr = iter_gv.agg(*span, "for-in iterator")?;
                        let elem_llvm_ty = self.t(elem_ty, *span)?;

                        // Create loop blocks
                        let cond_bb = self
                            .context
                            .append_basic_block(self.cur_func, "for.cond");
                        let body_bb = self
                            .context
                            .append_basic_block(self.cur_func, "for.body");
                        let inc_bb = self
                            .context
                            .append_basic_block(self.cur_func, "for.inc");
                        let merge_bb = self
                            .context
                            .append_basic_block(self.cur_func, "for.end");

                        // Initialize index = 0
                        let idx_ptr = bld(self.builder.build_alloca(self.i64_ty, "for.idx"))?;
                        bld(self.builder.build_store(idx_ptr, self.i64_ty.const_zero()))?;

                        bld(self.builder.build_unconditional_branch(cond_bb))?;

                        // Condition: if idx < n, go to body, else merge
                        self.builder.position_at_end(cond_bb);
                        let idx = bld(self.builder.build_load(self.i64_ty, idx_ptr, "for.idx.load"))?;
                        let n_val = self.i64_ty.const_int(*n as u64, false);
                        let cond = bld(self.builder.build_int_compare(
                            IntPredicate::SLT,
                            idx.into_int_value(),
                            n_val,
                            "for.cond.cmp",
                        ))?;
                        bld(self.builder.build_conditional_branch(cond, body_bb, merge_bb))?;

                        // Body: load element, run body block
                        self.builder.position_at_end(body_bb);
                        let cur_idx =
                            bld(self.builder.build_load(self.i64_ty, idx_ptr, "for.idx.body"))?;
                        let elem_ptr = bld(unsafe {
                            self.builder.build_in_bounds_gep(
                                elem_llvm_ty.array_type(*n as u32),
                                arr_ptr,
                                &[self.i32_ty.const_zero(), cur_idx.into_int_value()],
                                "for.elem",
                            )
                        })?;
                        let elem_val =
                            bld(self.builder.build_load(elem_llvm_ty, elem_ptr, "for.elem.val"))?;
                        let elem_slot =
                            bld(self.builder.build_alloca(elem_llvm_ty, "for.elem.slot"))?;
                        bld(self.builder.build_store(elem_slot, elem_val))?;
                        self.vars.insert(*var_def, elem_slot);

                        // Push loop context for break/continue
                        self.loop_stack.push((inc_bb, merge_bb));
                        self.gen_block(body)?;
                        self.loop_stack.pop();

                        // Increment index and loop back
                        if !self.cur_block_terminated() {
                            bld(self.builder.build_unconditional_branch(inc_bb))?;
                        }

                        // Increment block: idx++, branch to cond
                        self.builder.position_at_end(inc_bb);
                        let idx = bld(self.builder.build_load(self.i64_ty, idx_ptr, "for.idx.inc"))?;
                        let next = bld(self.builder.build_int_add(
                            idx.into_int_value(),
                            self.i64_ty.const_int(1, false),
                            "for.idx.next",
                        ))?;
                        bld(self.builder.build_store(idx_ptr, next))?;
                        bld(self.builder.build_unconditional_branch(cond_bb))?;

                        self.builder.position_at_end(merge_bb);
                        Ok(())
                    }
                    Ty::Vec(elem_ty) => {
                        // Index loop over the Vec's heap buffer: for (i in 0..len) x = data[i]
                        let elem_llvm_ty = self.t(elem_ty, *span)?;
                        let vec_llvm = self.t(&Ty::Vec(Box::new((**elem_ty).clone())), *span)?;
                        let vec_ptr = iter_gv.agg(*span, "for-in vec")?;

                        // data_slot = GEP [0,0]; len_slot = GEP [0,1]
                        let data_slot = bld(unsafe {
                            self.builder.build_in_bounds_gep(
                                vec_llvm,
                                vec_ptr,
                                &[self.i32_ty.const_zero(), self.i32_ty.const_zero()],
                                "vec.data",
                            )
                        })?;
                        let len_slot = bld(unsafe {
                            self.builder.build_in_bounds_gep(
                                vec_llvm,
                                vec_ptr,
                                &[self.i32_ty.const_zero(), self.i32_ty.const_int(1, false)],
                                "vec.len",
                            )
                        })?;

                        let cond_bb = self
                            .context
                            .append_basic_block(self.cur_func, "for.cond");
                        let body_bb = self
                            .context
                            .append_basic_block(self.cur_func, "for.body");
                        let inc_bb = self
                            .context
                            .append_basic_block(self.cur_func, "for.inc");
                        let merge_bb = self
                            .context
                            .append_basic_block(self.cur_func, "for.end");

                        let idx_ptr = bld(self.builder.build_alloca(self.i64_ty, "for.idx"))?;
                        bld(self.builder.build_store(idx_ptr, self.i64_ty.const_zero()))?;
                        bld(self.builder.build_unconditional_branch(cond_bb))?;

                        // Condition: idx < len
                        self.builder.position_at_end(cond_bb);
                        let idx = bld(self.builder.build_load(self.i64_ty, idx_ptr, "for.idx.load"))?
                            .into_int_value();
                        let len =
                            bld(self.builder.build_load(self.i64_ty, len_slot, "for.len"))?
                                .into_int_value();
                        let cond = bld(self.builder.build_int_compare(
                            IntPredicate::SLT,
                            idx,
                            len,
                            "for.cond.cmp",
                        ))?;
                        bld(self.builder.build_conditional_branch(cond, body_bb, merge_bb))?;

                        // Body: load data[i], run body block
                        self.builder.position_at_end(body_bb);
                        let cur_idx =
                            bld(self.builder.build_load(self.i64_ty, idx_ptr, "for.idx.body"))?
                                .into_int_value();
                        let data = bld(self.builder.build_load(
                            self.context.ptr_type(AddressSpace::from(0u16)),
                            data_slot,
                            "for.vdata",
                        ))?
                        .into_pointer_value();
                        let data_elems = bld(self.builder.build_pointer_cast(
                            data,
                            elem_llvm_ty.ptr_type(AddressSpace::from(0u16)),
                            "vec_data_elems",
                        ))?;
                        let elem_ptr = bld(unsafe {
                            self.builder.build_in_bounds_gep(
                                elem_llvm_ty,
                                data_elems,
                                &[cur_idx],
                                "for.elem",
                            )
                        })?;
                        let elem_val =
                            bld(self.builder.build_load(elem_llvm_ty, elem_ptr, "for.elem.val"))?;
                        let elem_slot =
                            bld(self.builder.build_alloca(elem_llvm_ty, "for.elem.slot"))?;
                        bld(self.builder.build_store(elem_slot, elem_val))?;
                        self.vars.insert(*var_def, elem_slot);

                        self.loop_stack.push((inc_bb, merge_bb));
                        self.gen_block(body)?;
                        self.loop_stack.pop();

                        if !self.cur_block_terminated() {
                            bld(self.builder.build_unconditional_branch(inc_bb))?;
                        }

                        // Increment block: idx++, branch to cond
                        self.builder.position_at_end(inc_bb);
                        let idx = bld(self.builder.build_load(self.i64_ty, idx_ptr, "for.idx.inc"))?
                            .into_int_value();
                        let next = bld(self.builder.build_int_add(
                            idx,
                            self.i64_ty.const_int(1, false),
                            "for.idx.next",
                        ))?;
                        bld(self.builder.build_store(idx_ptr, next))?;
                        bld(self.builder.build_unconditional_branch(cond_bb))?;

                        self.builder.position_at_end(merge_bb);
                        Ok(())
                    }
                    _ => {
                        // User-defined iterable via the `IntoIterator`/`Iterator`
                        // protocol:
                        //   let mut it = iter.into_iter();
                        //   loop { match it.next() { Some(x) => body, None => break } }
                        let (into_iter_func, it_ty) =
                            self.resolve_method_for(&iter_ty, "into_iter", *span)?;
                        let (next_func, option_ty) =
                            self.resolve_method_for(&it_ty, "next", *span)?;
                        let (item_ty, some_idx, none_idx) = self.for_option_parts(
                            &option_ty,
                            *span,
                        )?;
                        let option_llvm = self.t(&option_ty, *span)?;
                        let item_llvm = self.t(&item_ty, *span)?;

                        // 1) `let mut it = iter.into_iter();`
                        let it_llvm = self.t(&it_ty, *span)?;
                        let it_slot = bld(self.builder.build_alloca(it_llvm, "for.it"))?;
                        let into_iter_params = into_iter_func.get_type().get_param_types();
                        let rpt: BasicTypeEnum = into_iter_params[0]
                            .try_into()
                            .map_err(|_| self.internal_err(*span, "into_iter receiver type mismatch"))?;
                        let recv_arg =
                            self.call_arg(iter_gv, &rpt, *span, "into_iter")?;
                        let out = bld(self.builder.build_call(
                            into_iter_func,
                            &[recv_arg.into()],
                            "into_iter",
                        ))?;
                        match out.try_as_basic_value().basic() {
                            Some(v) => {
                                if is_agg(&it_ty) {
                                    let tmp = bld(self.builder.build_alloca(
                                        v.get_type(),
                                        "for.it.tmp",
                                    ))?;
                                    bld(self.builder.build_store(tmp, v))?;
                                    let n = aero_size(
                                        &it_ty,
                                        self.hir_structs,
                                        self.hir_unions,
                                        self.hir_enums,
                                        &self.type_subst,
                                    );
                                    self.emit_memcpy(it_slot, tmp, n, *span, "into_iter")?;
                                } else {
                                    bld(self.builder.build_store(it_slot, v))?;
                                }
                            }
                            None => {
                                return Err(self.internal_err(*span, "`into_iter` returned no value"))
                            }
                        }

                        // 2) loop: o = it.next(); if Some(x) → body else break
                        let cond_bb = self
                            .context
                            .append_basic_block(self.cur_func, "for.cond");
                        let body_bb = self
                            .context
                            .append_basic_block(self.cur_func, "for.body");
                        let merge_bb = self
                            .context
                            .append_basic_block(self.cur_func, "for.end");

                        let o_slot = bld(self.builder.build_alloca(option_llvm, "for.opt"))?;
                        bld(self.builder.build_unconditional_branch(cond_bb))?;

                        self.builder.position_at_end(cond_bb);
                        // `next(it: &mut Self)` — the iterator slot address is the receiver.
                        let o = bld(self.builder.build_call(
                            next_func,
                            &[it_slot.into()],
                            "for.next",
                        ))?;
                        if let Some(v) = o.try_as_basic_value().basic() {
                            bld(self.builder.build_store(o_slot, v))?;
                        } else {
                            return Err(self.internal_err(*span, "`next` returned no value"));
                        }
                        let tag_ptr = bld(unsafe {
                            self.builder.build_in_bounds_gep(
                                option_llvm,
                                o_slot,
                                &[self.i32_ty.const_zero(), self.i32_ty.const_zero()],
                                "for.tag",
                            )
                        })?;
                        let tag = bld(self.builder.build_load(self.i64_ty, tag_ptr, "for.tagv"))?
                            .into_int_value();
                        let is_some = bld(self.builder.build_int_compare(
                            IntPredicate::EQ,
                            tag,
                            self.i64_ty.const_int(some_idx as u64, false),
                            "for.issome",
                        ))?;
                        bld(self.builder.build_conditional_branch(is_some, body_bb, merge_bb))?;

                        // Body: bind the Some payload and run the loop body
                        self.builder.position_at_end(body_bb);
                        let pay_ptr = bld(unsafe {
                            self.builder.build_in_bounds_gep(
                                option_llvm,
                                o_slot,
                                &[self.i32_ty.const_zero(), self.i32_ty.const_int(1, false)],
                                "for.pay",
                            )
                        })?;
                        let x_slot = bld(self.builder.build_alloca(item_llvm, "for.x"))?;
                        if is_agg(&item_ty) {
                            let n = aero_size(
                                &item_ty,
                                self.hir_structs,
                                self.hir_unions,
                                self.hir_enums,
                                &self.type_subst,
                            );
                            self.emit_memcpy(x_slot, pay_ptr, n, *span, "for item")?;
                        } else {
                            let typed = bld(self.builder.build_pointer_cast(
                                pay_ptr,
                                item_llvm.ptr_type(AddressSpace::from(0u16)),
                                "for.item.typed",
                            ))?;
                            let v =
                                bld(self.builder.build_load(item_llvm, typed, "for.item.load"))?;
                            bld(self.builder.build_store(x_slot, v))?;
                        }
                        self.vars.insert(*var_def, x_slot);

                        self.loop_stack.push((cond_bb, merge_bb));
                        self.gen_block(body)?;
                        self.loop_stack.pop();
                        if !self.cur_block_terminated() {
                            bld(self.builder.build_unconditional_branch(cond_bb))?;
                        }

                        self.builder.position_at_end(merge_bb);
                        Ok(())
                    }
                }
            }
            HirStmt::Break(span) => {
                let (_, break_bb) = *self
                    .loop_stack
                    .last()
                    .ok_or_else(|| self.internal_err(*span, "break outside of loop"))?;
                bld(self.builder.build_unconditional_branch(break_bb))?;
                Ok(())
            }
            HirStmt::Continue(span) => {
                let (continue_bb, _) = *self
                    .loop_stack
                    .last()
                    .ok_or_else(|| self.internal_err(*span, "continue outside of loop"))?;
                bld(self.builder.build_unconditional_branch(continue_bb))?;
                Ok(())
            }
            HirStmt::Match {
                scrutinee,
                arms,
                span,
            } => {
                // Enum scrutinees dispatch to the tagged-union code path
                let scrut_ty = self.expr_ty(scrutinee);
                let is_enum = matches!(
                    &scrut_ty,
                    Ok(Ty::Enum(_)) | Ok(Ty::EnumGeneric { .. })
                ) || arms
                    .iter()
                    .any(|a| matches!(a.pattern, HirMatchPattern::EnumVariant { .. }));
                if is_enum {
                    return self.gen_enum_match(scrutinee, arms, *span);
                }
                let scrut_val =
                    self.gen_value(scrutinee)?.scalar(*span, "match scrutinee")?;
                let merge_bb = self
                    .context
                    .append_basic_block(self.cur_func, "match.end");

                // Create a body block for each arm
                let body_bbs: Vec<_> = arms
                    .iter()
                    .enumerate()
                    .map(|(i, _)| {
                        self.context
                            .append_basic_block(self.cur_func, &format!("match.arm{i}"))
                    })
                    .collect();

                let mut merge_reachable = false;

                // Generate comparison chains: for each arm, compare scrutinee with pattern.
                // If match, branch to arm body; otherwise fall through to next comparison.
                for (i, arm) in arms.iter().enumerate() {
                    let is_last = i == arms.len() - 1;

                    match &arm.pattern {
                        HirMatchPattern::Wildcard | HirMatchPattern::Bind(..) => {
                            // Always matches: branch directly to arm body
                            bld(self.builder.build_unconditional_branch(body_bbs[i]))?;
                        }
                        pattern => {
                            let cond = match pattern {
                                HirMatchPattern::IntLit(v) => {
                                    let pat_val = scrut_val
                                        .into_int_value()
                                        .get_type()
                                        .const_int(*v as u64, false);
                                    bld(self.builder.build_int_compare(
                                        IntPredicate::EQ,
                                        scrut_val.into_int_value(),
                                        pat_val,
                                        "match.cmp",
                                    ))?
                                }
                                HirMatchPattern::BoolLit(b) => {
                                    let pat_val =
                                        self.bool_ty.const_int(if *b { 1 } else { 0 }, false);
                                    bld(self.builder.build_int_compare(
                                        IntPredicate::EQ,
                                        scrut_val.into_int_value(),
                                        pat_val,
                                        "match.cmp",
                                    ))?
                                }
                                HirMatchPattern::CharLit(c) => {
                                    let pat_val = self.i32_ty.const_int(*c as u64, false);
                                    bld(self.builder.build_int_compare(
                                        IntPredicate::EQ,
                                        scrut_val.into_int_value(),
                                        pat_val,
                                        "match.cmp",
                                    ))?
                                }
                                HirMatchPattern::StrLit(s) => {
                                    let pat_str = self.global_string(s)?;
                                    let cmp_result = bld(self.builder.build_call(
                                        self.strcmp,
                                        &[scrut_val.into(), pat_str.into()],
                                        "match.strcmp",
                                    ))?
                                    .try_as_basic_value()
                                    .basic()
                                    .ok_or_else(|| {
                                        self.internal_err(arm.span, "strcmp returned no value")
                                    })?;
                                    bld(self.builder.build_int_compare(
                                        IntPredicate::EQ,
                                        cmp_result.into_int_value(),
                                        self.i32_ty.const_zero(),
                                        "match.strcmp.eq",
                                    ))?
                                }
                                _ => unreachable!(),
                            };
                            if is_last {
                                // Last non-wildcard arm: no-match goes to merge
                                merge_reachable = true;
                                bld(self
                                    .builder
                                    .build_conditional_branch(cond, body_bbs[i], merge_bb))?;
                            } else {
                                let next_bb = self.context.append_basic_block(
                                    self.cur_func,
                                    &format!("match.next{i}"),
                                );
                                bld(self
                                    .builder
                                    .build_conditional_branch(cond, body_bbs[i], next_bb))?;
                                self.builder.position_at_end(next_bb);
                            }
                        }
                    }
                }

                // If the current block isn't terminated (e.g. empty arms), branch to merge
                if !self.cur_block_terminated() {
                    merge_reachable = true;
                    bld(self.builder.build_unconditional_branch(merge_bb))?;
                }

                // Generate arm bodies
                for (i, arm) in arms.iter().enumerate() {
                    self.builder.position_at_end(body_bbs[i]);
                    // Bind: copy scrutinee value to the bound variable
                    if let HirMatchPattern::Bind(_, def_id) = &arm.pattern {
                        let slot = bld(
                            self.builder
                                .build_alloca(scrut_val.get_type(), "match.bind"),
                        )?;
                        bld(self.builder.build_store(slot, scrut_val))?;
                        self.vars.insert(*def_id, slot);
                    }
                    self.gen_block(&arm.body)?;
                    if !self.cur_block_terminated() {
                        merge_reachable = true;
                        bld(self.builder.build_unconditional_branch(merge_bb))?;
                    }
                }

                self.builder.position_at_end(merge_bb);
                if !merge_reachable {
                    // All arms ended with return/break: merge is unreachable
                    bld(self.builder.build_unreachable())?;
                }
                Ok(())
            }
            // Struct definitions are collected in lowering; nothing to emit at runtime.
            HirStmt::StructDef { .. } => Ok(()),
            // Enum definitions are collected in lowering; nothing to emit at runtime.
            HirStmt::EnumDef { .. } => Ok(()),
            HirStmt::TraitDef { .. } => Ok(()),
            HirStmt::ImplBlock { .. } => Ok(()),
        }
    }

    /// Match on an enum value: dispatch on the variant tag, then run the matching
    /// arm's body. Enum payload bindings read the payload byte buffer with the
    /// variant's own payload type. The caller guarantees the scrutinee is an enum
    /// and that all non-wildcard patterns are `EnumVariant` patterns.
    fn gen_enum_match(
        &mut self,
        scrutinee: &HirExpr,
        arms: &[HirMatchArm],
        span: Span,
    ) -> Result<(), CodegenError> {
        let scrut_ptr = self.gen_value(scrutinee)?.agg(span, "match scrutinee")?;
        let enum_ty = self.expr_ty(scrutinee)?;
        let (enum_name, instance_args) = match &enum_ty {
            Ty::Enum(n) => (n.clone(), None),
            Ty::EnumGeneric { name, args } => (name.clone(), Some(args.clone())),
            other => {
                return Err(CodegenError {
                    msg: format!("internal error: enum match over non-enum type `{other}`"),
                    line: span.line,
                    col: span.col,
                });
            }
        };
        let def = self
            .hir_enums
            .iter()
            .find(|e| e.name == enum_name)
            .ok_or_else(|| self.internal_err(span, &format!("undefined enum `{enum_name}`")))?;
        let enum_llvm = self.t(&enum_ty, span)?;

        let merge_bb = self
            .context
            .append_basic_block(self.cur_func, "match.end");
        let body_bbs: Vec<_> = arms
            .iter()
            .enumerate()
            .map(|(i, _)| {
                self.context
                    .append_basic_block(self.cur_func, &format!("match.arm{i}"))
            })
            .collect();
        let mut merge_reachable = false;

        // Load the variant tag once (field 0 of the tagged union)
        let tag_ptr = bld(unsafe {
            self.builder.build_in_bounds_gep(
                enum_llvm,
                scrut_ptr,
                &[self.i32_ty.const_zero(), self.i32_ty.const_zero()],
                "enum.tag",
            )
        })?;
        let tag = bld(self.builder.build_load(self.i64_ty, tag_ptr, "enum.tagv"))?;

        // Compare the tag against each arm's variant; fall through to the next arm
        for (i, arm) in arms.iter().enumerate() {
            let is_last = i == arms.len() - 1;
            match &arm.pattern {
                HirMatchPattern::Wildcard | HirMatchPattern::Bind(..) => {
                    // Always matches: branch directly to the arm body. Any later
                    // arms are unreachable (the wildcard already matched), so stop
                    // emitting the dispatch chain here — generating more instructions
                    // on this now-terminated block would corrupt the LLVM IR.
                    bld(self.builder.build_unconditional_branch(body_bbs[i]))?;
                    break;
                }
                HirMatchPattern::EnumVariant { variant, .. } => {
                    let vidx = def.find_variant(variant).ok_or_else(|| {
                        self.internal_err(
                            arm.span,
                            &format!("enum `{enum_name}` has no variant `{variant}`"),
                        )
                    })?;
                    let pat = self.i64_ty.const_int(vidx.0 as u64, false);
                    let cond = bld(self.builder.build_int_compare(
                        IntPredicate::EQ,
                        tag.into_int_value(),
                        pat,
                        "enum.cmp",
                    ))?;
                    if is_last {
                        merge_reachable = true;
                        bld(self
                            .builder
                            .build_conditional_branch(cond, body_bbs[i], merge_bb))?;
                    } else {
                        let next_bb = self.context.append_basic_block(
                            self.cur_func,
                            &format!("match.next{i}"),
                        );
                        bld(self
                            .builder
                            .build_conditional_branch(cond, body_bbs[i], next_bb))?;
                        self.builder.position_at_end(next_bb);
                    }
                }
                other => {
                    return Err(CodegenError {
                        msg: format!("internal error: non-enum pattern `{other:?}` in enum match"),
                        line: arm.span.line,
                        col: arm.span.col,
                    });
                }
            }
        }

        // If the current block isn't terminated (e.g. empty arms), branch to merge
        if !self.cur_block_terminated() {
            merge_reachable = true;
            bld(self.builder.build_unconditional_branch(merge_bb))?;
        }

        // Generate arm bodies; payload bindings read the payload buffer
        for (i, arm) in arms.iter().enumerate() {
            self.builder.position_at_end(body_bbs[i]);
            match &arm.pattern {
                HirMatchPattern::Bind(_, def_id) => {
                    // Bind the whole enum value into a fresh slot
                    let slot = bld(self.builder.build_alloca(enum_llvm, "enum.bind"))?;
                    self.copy_agg(slot, scrut_ptr, &enum_ty, span, "enum match bind")?;
                    self.vars.insert(*def_id, slot);
                }
                HirMatchPattern::EnumVariant { variant, bind, .. } => {
                    if let Some((_, def_id)) = bind {
                        // The bound payload has the variant's own payload type,
                        // with generic instance args substituted in.
                        let raw_pt = def
                            .find_variant(variant)
                            .and_then(|(_, p)| p.clone())
                            .ok_or_else(|| {
                                self.internal_err(
                                    arm.span,
                                    &format!("variant `{enum_name}::{variant}` has no payload to bind"),
                                )
                            })?;
                        let pt = match &instance_args {
                            Some(args) => {
                                let merged = instance_subst(&def.type_params, args, &self.type_subst);
                                substitute(&raw_pt, &merged)
                            }
                            None => raw_pt,
                        };
                        let pt_llvm = self.t(&pt, arm.span)?;
                        let slot = bld(self.builder.build_alloca(pt_llvm, "enum.bind.pay"))?;
                        let pay_ptr = bld(unsafe {
                            self.builder.build_in_bounds_gep(
                                enum_llvm,
                                scrut_ptr,
                                &[self.i32_ty.const_zero(), self.i32_ty.const_int(1, false)],
                                "enum.payptr",
                            )
                        })?;
                        if is_agg(&pt) {
                            let n = aero_size(&pt, self.hir_structs, self.hir_unions, self.hir_enums, &self.type_subst);
                            self.emit_memcpy(slot, pay_ptr, n, arm.span, "enum payload bind")?;
                        } else {
                            let typed = bld(self.builder.build_pointer_cast(
                                pay_ptr,
                                pt_llvm.ptr_type(AddressSpace::from(0u16)),
                                "enum.bind.typed",
                            ))?;
                            let v = bld(
                                self.builder
                                    .build_load(pt_llvm, typed, "enum.bind.load"),
                            )?;
                            bld(self.builder.build_store(slot, v))?;
                        }
                        self.vars.insert(*def_id, slot);
                    }
                }
                _ => {}
            }
            self.gen_block(&arm.body)?;
            if !self.cur_block_terminated() {
                merge_reachable = true;
                bld(self.builder.build_unconditional_branch(merge_bb))?;
            }
        }

        self.builder.position_at_end(merge_bb);
        if !merge_reachable {
            // All arms ended with return/break: merge is unreachable
            bld(self.builder.build_unreachable())?;
        }
        Ok(())
    }

    /// `?` operator: unwrap a `Result<T, E>`. On `Ok(t)` the expression evaluates to
    /// `t` (continuing in the join block); on `Err(e)` the enclosing function
    /// immediately returns `Err(e)`. Type checking guarantees the target is a
    /// `Result` and that the enclosing function returns a `Result` with the same
    /// error type.
    fn gen_try(
        &mut self,
        target: &HirExpr,
        span: Span,
    ) -> Result<GenValue<'ctx>, CodegenError> {
        let scrut_ptr = self.gen_value(target)?.agg(span, "`?` target")?;
        let enum_ty = self.expr_ty(target)?;
        let (enum_name, instance_args) = match &enum_ty {
            Ty::Enum(n) => (n.clone(), None),
            Ty::EnumGeneric { name, args } => (name.clone(), Some(args.clone())),
            other => {
                return Err(self.internal_err(
                    span,
                    &format!("`?` target must be a `Result<T, E>`, got `{other}`"),
                ));
            }
        };
        let def = self
            .hir_enums
            .iter()
            .find(|e| e.name == enum_name)
            .ok_or_else(|| self.internal_err(span, &format!("undefined enum `{enum_name}`")))?;
        // Resolve the variant payload types through the instance type args.
        let subst = match &instance_args {
            Some(args) => instance_subst(&def.type_params, args, &self.type_subst),
            None => self.type_subst.clone(),
        };
        let (ok_idx, ok_pay) = def
            .find_variant("Ok")
            .map(|(i, p)| (i, p.clone().map(|t| substitute(&t, &subst))))
            .ok_or_else(|| self.internal_err(span, "`Result` enum has no `Ok` variant"))?;
        let (err_idx, err_pay) = def
            .find_variant("Err")
            .map(|(i, p)| (i, p.clone().map(|t| substitute(&t, &subst))))
            .ok_or_else(|| self.internal_err(span, "`Result` enum has no `Err` variant"))?;
        let ok_ty = ok_pay
            .ok_or_else(|| self.internal_err(span, "`Ok` variant of `Result` must carry a payload"))?;
        let err_ty = err_pay
            .ok_or_else(|| self.internal_err(span, "`Err` variant of `Result` must carry a payload"))?;

        let enum_llvm = self.t(&enum_ty, span)?;
        let ok_llvm = self.t(&ok_ty, span)?;
        // Slot holding the unwrapped `Ok` payload, read by the join block.
        let ok_slot = bld(self.builder.build_alloca(ok_llvm, "try.ok"))?;

        // Load the variant tag (field 0 of the tagged union).
        let tag_ptr = bld(unsafe {
            self.builder.build_in_bounds_gep(
                enum_llvm,
                scrut_ptr,
                &[self.i32_ty.const_zero(), self.i32_ty.const_zero()],
                "try.tag",
            )
        })?;
        let tag = bld(self.builder.build_load(self.i64_ty, tag_ptr, "try.tagv"))?;
        let is_ok = bld(self.builder.build_int_compare(
            IntPredicate::EQ,
            tag.into_int_value(),
            self.i64_ty.const_int(ok_idx as u64, false),
            "try.is_ok",
        ))?;

        let ok_bb = self.context.append_basic_block(self.cur_func, "try.ok");
        let err_bb = self.context.append_basic_block(self.cur_func, "try.err");
        let join_bb = self.context.append_basic_block(self.cur_func, "try.join");
        bld(self
            .builder
            .build_conditional_branch(is_ok, ok_bb, err_bb))?;

        // Ok branch: copy the `Ok` payload into `ok_slot`, then fall through to join.
        self.builder.position_at_end(ok_bb);
        let pay_ptr = bld(unsafe {
            self.builder.build_in_bounds_gep(
                enum_llvm,
                scrut_ptr,
                &[self.i32_ty.const_zero(), self.i32_ty.const_int(1, false)],
                "try.payptr",
            )
        })?;
        if is_agg(&ok_ty) {
            let n = aero_size(&ok_ty, self.hir_structs, self.hir_unions, self.hir_enums, &self.type_subst);
            self.emit_memcpy(ok_slot, pay_ptr, n, span, "try ok payload")?;
        } else {
            let typed = bld(self.builder.build_pointer_cast(
                pay_ptr,
                ok_llvm.ptr_type(AddressSpace::from(0u16)),
                "try.ok.typed",
            ))?;
            let v = bld(self.builder.build_load(ok_llvm, typed, "try.ok.load"))?;
            bld(self.builder.build_store(ok_slot, v))?;
        }
        bld(self.builder.build_unconditional_branch(join_bb))?;

        // Err branch: build `Err(e)` and return it from the enclosing function.
        self.builder.position_at_end(err_bb);
        let err_llvm = self.t(&err_ty, span)?;
        let err_ptr = bld(unsafe {
            self.builder.build_in_bounds_gep(
                enum_llvm,
                scrut_ptr,
                &[self.i32_ty.const_zero(), self.i32_ty.const_int(1, false)],
                "try.errptr",
            )
        })?;
        let err_val = if is_agg(&err_ty) {
            let slot = bld(self.builder.build_alloca(err_llvm, "try.err"))?;
            let n = aero_size(&err_ty, self.hir_structs, self.hir_unions, self.hir_enums, &self.type_subst);
            self.emit_memcpy(slot, err_ptr, n, span, "try err payload")?;
            GenValue::Agg(slot)
        } else {
            let typed = bld(self.builder.build_pointer_cast(
                err_ptr,
                err_llvm.ptr_type(AddressSpace::from(0u16)),
                "try.err.typed",
            ))?;
            let v = bld(self.builder.build_load(err_llvm, typed, "try.err.load"))?;
            GenValue::Scalar(v)
        };
        // The enclosing function returns `Result<_, E>` (type checking guarantees this).
        let ret_ty = self.cur_ret.clone().ok_or_else(|| {
            self.internal_err(span, "`?` used outside a `Result<_, E>`-returning function")
        })?;
        let ret_llvm = self.t(&ret_ty, span)?;
        let ret_slot = bld(self.builder.build_alloca(ret_llvm, "try.ret"))?;
        let ret_tag = bld(unsafe {
            self.builder.build_in_bounds_gep(
                ret_llvm,
                ret_slot,
                &[self.i32_ty.const_zero(), self.i32_ty.const_zero()],
                "try.ret.tag",
            )
        })?;
        bld(self.builder.build_store(
            ret_tag,
            self.i64_ty.const_int(err_idx as u64, false),
        ))?;
        let ret_pay = bld(unsafe {
            self.builder.build_in_bounds_gep(
                ret_llvm,
                ret_slot,
                &[self.i32_ty.const_zero(), self.i32_ty.const_int(1, false)],
                "try.ret.pay",
            )
        })?;
        match err_val {
            GenValue::Scalar(v) => {
                let typed = bld(self.builder.build_pointer_cast(
                    ret_pay,
                    err_llvm.ptr_type(AddressSpace::from(0u16)),
                    "try.ret.typed",
                ))?;
                bld(self.builder.build_store(typed, v))?;
            }
            GenValue::Agg(p) => {
                let n = aero_size(&err_ty, self.hir_structs, self.hir_unions, self.hir_enums, &self.type_subst);
                self.emit_memcpy(ret_pay, p, n, span, "try ret err payload")?;
            }
        }
        let ret_val = bld(self.builder.build_load(ret_llvm, ret_slot, "try.ret.load"))?;
        bld(self.builder.build_return(Some(&ret_val)))?;

        // Join block: resume with the unwrapped `Ok` payload.
        self.builder.position_at_end(join_bb);
        if is_agg(&ok_ty) {
            Ok(GenValue::Agg(ok_slot))
        } else {
            let v = bld(self.builder.build_load(ok_llvm, ok_slot, "try.join.load"))?;
            Ok(GenValue::Scalar(v))
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
            Ty::Struct(name) => {
                // Struct copy: per-field load/store; nested aggregates recurse.
                let def = self
                    .hir_structs
                    .iter()
                    .find(|s| s.name == *name)
                    .ok_or_else(|| self.internal_err(span, &format!("undefined struct `{name}`")))?;
                let struct_ty = self.t(ty, span)?;
                for (i, (_, fty)) in def.fields.iter().enumerate() {
                    let idx = self.i32_ty.const_int(i as u64, false);
                    let s = bld(unsafe {
                        self.builder.build_in_bounds_gep(
                            struct_ty,
                            src_ptr,
                            &[self.i32_ty.const_zero(), idx],
                            "cp_fs",
                        )
                    })?;
                    let d = bld(unsafe {
                        self.builder.build_in_bounds_gep(
                            struct_ty,
                            dst,
                            &[self.i32_ty.const_zero(), idx],
                            "cp_fd",
                        )
                    })?;
                    if is_agg(fty) {
                        self.copy_agg(d, s, fty, span, what)?;
                    } else {
                        let elem_ty = self.t(fty, span)?;
                        let v = bld(self.builder.build_load(elem_ty, s, "cp_fl"))?;
                        bld(self.builder.build_store(d, v))?;
                    }
                }
                Ok(())
            }
            Ty::StructGeneric { name, args } => {
                // Monomorphized struct copy: substitute instance args into field types.
                let def = self
                    .hir_structs
                    .iter()
                    .find(|s| s.name == *name)
                    .ok_or_else(|| self.internal_err(span, &format!("undefined struct `{name}`")))?;
                let merged = instance_subst(&def.type_params, args, &self.type_subst);
                let struct_ty = self.t(ty, span)?;
                for (i, (_, fty)) in def.fields.iter().enumerate() {
                    let idx = self.i32_ty.const_int(i as u64, false);
                    let s = bld(unsafe {
                        self.builder.build_in_bounds_gep(
                            struct_ty,
                            src_ptr,
                            &[self.i32_ty.const_zero(), idx],
                            "cp_fs",
                        )
                    })?;
                    let d = bld(unsafe {
                        self.builder.build_in_bounds_gep(
                            struct_ty,
                            dst,
                            &[self.i32_ty.const_zero(), idx],
                            "cp_fd",
                        )
                    })?;
                    let fty_subst = substitute(fty, &merged);
                    if is_agg(&fty_subst) {
                        self.copy_agg(d, s, &fty_subst, span, what)?;
                    } else {
                        let elem_ty = self.t(&fty_subst, span)?;
                        let v = bld(self.builder.build_load(elem_ty, s, "cp_fl"))?;
                        bld(self.builder.build_store(d, v))?;
                    }
                }
                Ok(())
            }
            Ty::Enum(name) => {
                // Tagged-union copy: tag (i64) field, then the payload byte buffer.
                let def = self
                    .hir_enums
                    .iter()
                    .find(|e| e.name == *name)
                    .ok_or_else(|| self.internal_err(span, &format!("undefined enum `{name}`")))?;
                let enum_ty = self.t(ty, span)?;
                // tag
                let s_tag = bld(unsafe {
                    self.builder.build_in_bounds_gep(
                        enum_ty,
                        src_ptr,
                        &[self.i32_ty.const_zero(), self.i32_ty.const_zero()],
                        "cp_etag_s",
                    )
                })?;
                let d_tag = bld(unsafe {
                    self.builder.build_in_bounds_gep(
                        enum_ty,
                        dst,
                        &[self.i32_ty.const_zero(), self.i32_ty.const_zero()],
                        "cp_etag_d",
                    )
                })?;
                let tv = bld(self.builder.build_load(self.i64_ty, s_tag, "cp_etag"))?;
                bld(self.builder.build_store(d_tag, tv))?;
                // payload bytes
                let payload_size = match enum_payload_ty(def, self.hir_structs, self.hir_unions, self.hir_enums, &self.type_subst) {
                    Some(p) => aero_size(&p, self.hir_structs, self.hir_unions, self.hir_enums, &self.type_subst),
                    None => 8,
                };
                if payload_size > 0 {
                    let s_pay = bld(unsafe {
                        self.builder.build_in_bounds_gep(
                            enum_ty,
                            src_ptr,
                            &[self.i32_ty.const_zero(), self.i32_ty.const_int(1, false)],
                            "cp_epay_s",
                        )
                    })?;
                    let d_pay = bld(unsafe {
                        self.builder.build_in_bounds_gep(
                            enum_ty,
                            dst,
                            &[self.i32_ty.const_zero(), self.i32_ty.const_int(1, false)],
                            "cp_epay_d",
                        )
                    })?;
                    self.emit_memcpy(d_pay, s_pay, payload_size, span, "enum copy")?;
                }
                Ok(())
            }
            Ty::EnumGeneric { name, args } => {
                // Monomorphized tagged-union copy: substitute instance args.
                let def = self
                    .hir_enums
                    .iter()
                    .find(|e| e.name == *name)
                    .ok_or_else(|| self.internal_err(span, &format!("undefined enum `{name}`")))?;
                let merged = instance_subst(&def.type_params, args, &self.type_subst);
                let enum_ty = self.t(ty, span)?;
                // tag
                let s_tag = bld(unsafe {
                    self.builder.build_in_bounds_gep(
                        enum_ty,
                        src_ptr,
                        &[self.i32_ty.const_zero(), self.i32_ty.const_zero()],
                        "cp_etag_s",
                    )
                })?;
                let d_tag = bld(unsafe {
                    self.builder.build_in_bounds_gep(
                        enum_ty,
                        dst,
                        &[self.i32_ty.const_zero(), self.i32_ty.const_zero()],
                        "cp_etag_d",
                    )
                })?;
                let tv = bld(self.builder.build_load(self.i64_ty, s_tag, "cp_etag"))?;
                bld(self.builder.build_store(d_tag, tv))?;
                // payload bytes
                let payload_size = match enum_payload_ty(def, self.hir_structs, self.hir_unions, self.hir_enums, &merged) {
                    Some(p) => aero_size(&p, self.hir_structs, self.hir_unions, self.hir_enums, &merged),
                    None => 8,
                };
                if payload_size > 0 {
                    let s_pay = bld(unsafe {
                        self.builder.build_in_bounds_gep(
                            enum_ty,
                            src_ptr,
                            &[self.i32_ty.const_zero(), self.i32_ty.const_int(1, false)],
                            "cp_epay_s",
                        )
                    })?;
                    let d_pay = bld(unsafe {
                        self.builder.build_in_bounds_gep(
                            enum_ty,
                            dst,
                            &[self.i32_ty.const_zero(), self.i32_ty.const_int(1, false)],
                            "cp_epay_d",
                        )
                    })?;
                    self.emit_memcpy(d_pay, s_pay, payload_size, span, "enum copy")?;
                }
                Ok(())
            }
            // Native `Vec<T>`: shallow copy of the `{ data, len, cap }` struct
            // (the heap buffer is shared between copies, like a Rust `Vec` move).
            Ty::Vec(_) => {
                let n = aero_size(ty, self.hir_structs, self.hir_unions, self.hir_enums, &self.type_subst);
                self.emit_memcpy(dst, src_ptr, n, span, what)
            }
            // Native `String`: shallow copy of the `{ data, len, cap }` struct
            // (the heap buffer is shared, like a Rust `String` move).
            Ty::String => {
                let n = aero_size(ty, self.hir_structs, self.hir_unions, self.hir_enums, &self.type_subst);
                self.emit_memcpy(dst, src_ptr, n, span, what)
            }
            // A union is a flat byte buffer `[N x i8]`; copying it is a raw
            // byte-for-byte copy (all fields share that storage, so a single
            // memcpy of the union's size preserves every field).
            Ty::Union(_) => {
                let n = aero_size(ty, self.hir_structs, self.hir_unions, self.hir_enums, &self.type_subst);
                self.emit_memcpy(dst, src_ptr, n, span, what)
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
            HirExpr::FloatLit(v, _) => {
                // Default float type is f64; if annotation says f32, the coercion in let/assign handles it
                Ok(GenValue::Scalar(self.context.f64_type().const_float(*v).into()))
            }
            HirExpr::CharLit(c, _) => Ok(GenValue::Scalar(
                self.i32_ty.const_int(*c as u64, false).into(),
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
            HirExpr::ConstRef { name, span, .. } => {
                // Top-level const: evaluate the value at compile time and fill it in.
                // The LLVM type is chosen from the constant value itself (the natural
                // wide type), so unannotated consts work regardless of the placeholder.
                let cv = self.eval_const(name, *span)?;
                let basic = match &cv {
                    const_eval::ConstVal::Int(n) => self
                        .i64_ty
                        .const_int(*n as u64, false)
                        .as_basic_value_enum(),
                    const_eval::ConstVal::Float(f) => self
                        .context
                        .f64_type()
                        .const_float(*f)
                        .as_basic_value_enum(),
                    const_eval::ConstVal::Bool(b) => self
                        .bool_ty
                        .const_int(if *b { 1 } else { 0 }, false)
                        .as_basic_value_enum(),
                    const_eval::ConstVal::Char(c) => self
                        .i32_ty
                        .const_int(*c as u64, false)
                        .as_basic_value_enum(),
                    const_eval::ConstVal::Str(_) => {
                        return Err(self.internal_err(
                            *span,
                            "string constants are not supported as expressions",
                        ));
                    }
                };
                Ok(GenValue::Scalar(basic))
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
                // Dereferencing an aggregate yields the whole value; keep it addressable
                // (Agg) so field access / indexing works on `(*ptr).field`.
                if is_agg(&inner_ty) {
                    let tmp = bld(self.builder.build_alloca(slot_ty, "deref.agg"))?;
                    bld(self.builder.build_store(tmp, loaded))?;
                    Ok(GenValue::Agg(tmp))
                } else {
                    Ok(GenValue::Scalar(loaded))
                }
            }
            HirExpr::Try { target, span } => self.gen_try(target, *span),
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
            HirExpr::TensorLit { dims, elem, span } => {
                let ty = Ty::Tensor {
                    elem: Box::new(elem.clone()),
                    shape: dims.clone(),
                };
                let llvm = self.t(&ty, *span)?;
                let tmp = bld(self.builder.build_alloca(llvm, "tensor"))?;
                self.store_zero_agg(tmp, &ty, *span)?;
                Ok(GenValue::Agg(tmp))
            }
            HirExpr::Matmul { .. } => self.gen_matmul(expr),
            HirExpr::Reduce { .. } => self.gen_reduce(expr),
            HirExpr::ElemWise { .. } => self.gen_elemwise(expr),
            HirExpr::Blas { .. } => self.gen_blas(expr),
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
            HirExpr::StructLit { name, fields, span } => {
                // Union literal: `U { field: value }`. Allocate the byte-buffer union,
                // bitcast to the field's type and store the single set field.
                if let Some(def) = self.hir_unions.iter().find(|u| u.name == *name) {
                    let union_ty = self.t(&Ty::Union(name.clone()), *span)?;
                    let tmp = bld(self.builder.build_alloca(union_ty, "union"))?;
                    debug_assert_eq!(fields.len(), 1, "infer enforces one field per union literal");
                    let (fname, fval) = &fields[0];
                    let fty = def
                        .find_field(fname)
                        .map(|(_, t)| t.clone())
                        .ok_or_else(|| self.internal_err(*span, &format!("union `{name}` has no field `{fname}`")))?;
                    let fty_llvm = self.t(&fty, *span)?;
                    let slot = bld(self.builder.build_pointer_cast(
                        tmp,
                        fty_llvm.ptr_type(AddressSpace::from(0u16)),
                        "union.fld",
                    ))?;
                    if is_agg(&fty) {
                        let gv = self.gen_value(fval)?;
                        let src = gv.agg(*span, "union field")?;
                        self.copy_agg(slot, src, &fty, *span, "union field")?;
                    } else {
                        let v = self.gen_value(fval)?.scalar(*span, "union field")?;
                        let v = self.coerce(v, &fty_llvm, *span, "union field")?;
                        bld(self.builder.build_store(slot, v))?;
                    }
                    return Ok(GenValue::Agg(tmp));
                }
                // Struct literal: build the LLVM struct type from the definition,
                // allocate a temp slot and fill each field by its declared type.
                // Generic structs build the monomorphized instance type (type args
                // recorded by inference, resolved through the current instance context).
                let def = self
                    .hir_structs
                    .iter()
                    .find(|s| s.name == *name)
                    .ok_or_else(|| self.internal_err(*span, &format!("undefined struct `{name}`")))?;
                let (struct_ty, field_subst) = if def.type_params.is_empty() {
                    (
                        self.t(&Ty::Struct(name.clone()), *span)?,
                        HashMap::new(),
                    )
                } else {
                    let raw = self
                        .struct_lit_types
                        .get(&span.start)
                        .cloned()
                        .ok_or_else(|| {
                            self.internal_err(
                                *span,
                                &format!("internal error: generic struct literal `{name}` lacks type-instance info (infer did not record it)"),
                            )
                        })?;
                    let resolved: Vec<Ty> = raw
                        .iter()
                        .map(|t| substitute(t, &self.type_subst))
                        .collect();
                    let subst: HashMap<String, Ty> = def
                        .type_params
                        .iter()
                        .cloned()
                        .zip(resolved.iter().cloned())
                        .collect();
                    (
                        self.t(
                            &Ty::StructGeneric {
                                name: name.clone(),
                                args: resolved,
                            },
                            *span,
                        )?,
                        subst,
                    )
                };
                let tmp = bld(self.builder.build_alloca(struct_ty, "struct"))?;
                for (fname, fval) in fields {
                    let (idx, fty) = def
                        .find_field(fname)
                        .ok_or_else(|| self.internal_err(*span, &format!("struct `{name}` has no field `{fname}`")))?;
                    let fty = substitute(fty, &field_subst);
                    let i32 = self.i32_ty.const_int(idx as u64, false);
                    let slot = bld(unsafe {
                        self.builder.build_in_bounds_gep(
                            struct_ty,
                            tmp,
                            &[self.i32_ty.const_zero(), i32],
                            "fld",
                        )
                    })?;
                    if is_agg(&fty) {
                        // Nested aggregate field (struct/array/tuple): build a temp
                        // and deep-copy it into the field slot.
                        let gv = self.gen_value(fval)?;
                        let src = gv.agg(*span, "struct field")?;
                        self.copy_agg(slot, src, &fty, *span, "struct field")?;
                    } else {
                        let fty_llvm = self.t(&fty, *span)?;
                        let v = self.gen_value(fval)?.scalar(*span, "struct field")?;
                        let v = self.coerce(v, &fty_llvm, *span, "struct field")?;
                        bld(self.builder.build_store(slot, v))?;
                    }
                }
                Ok(GenValue::Agg(tmp))
            }
            HirExpr::EnumLit {
                name,
                variant,
                arg,
                span,
            } => {
                // Native `Vec<T>` construction (`Vec::new` / `Vec::with_cap(n)`): not a
                // user enum but a compiler-provided heap vector constructor.
                if name == "Vec" {
                    return self.gen_vec_ctor(variant, arg.as_deref(), *span);
                }
                // Native `String` construction (`String::new` / `String::with_cap(n)` /
                // `String::from(s)`): a compiler-provided heap string constructor.
                if name == "String" {
                    return self.gen_string_ctor(variant, arg.as_deref(), *span);
                }
                // Native `Box<T>` construction (`Box::new(value)`): a compiler-provided
                // heap smart-pointer constructor.
                if name == "Box" {
                    return self.gen_box_ctor(variant, arg.as_deref(), *span);
                }
                // Tagged-union constructor: build a temp `{ tag, payload }` slot.
                // The tag is the variant index; the payload is written at byte offset 0
                // of the byte buffer (typed store for scalars, memcpy for aggregates).
                let def = self
                    .hir_enums
                    .iter()
                    .find(|e| e.name == *name)
                    .ok_or_else(|| self.internal_err(*span, &format!("undefined enum `{name}`")))?;
                let (vidx, payload) = def
                    .find_variant(variant)
                    .ok_or_else(|| self.internal_err(*span, &format!("enum `{name}` has no variant `{variant}`")))?;
                // Generic enums substitute the recorded type args into the payload type.
                let (enum_llvm, payload) = if def.type_params.is_empty() {
                    (
                        self.t(&Ty::Enum(name.clone()), *span)?,
                        payload.clone(),
                    )
                } else {
                    let raw = self
                        .enum_lit_types
                        .get(&span.start)
                        .cloned()
                        .ok_or_else(|| {
                            self.internal_err(
                                *span,
                                &format!("internal error: generic enum literal `{name}` lacks type-instance info (infer did not record it)"),
                            )
                        })?;
                    let resolved: Vec<Ty> = raw
                        .iter()
                        .map(|t| substitute(t, &self.type_subst))
                        .collect();
                    let subst: HashMap<String, Ty> = def
                        .type_params
                        .iter()
                        .cloned()
                        .zip(resolved.iter().cloned())
                        .collect();
                    let payload = payload
                        .as_ref()
                        .map(|p| substitute(p, &subst));
                    (
                        self.t(
                            &Ty::EnumGeneric {
                                name: name.clone(),
                                args: resolved,
                            },
                            *span,
                        )?,
                        payload,
                    )
                };
                let tmp = bld(self.builder.build_alloca(enum_llvm, "enum"))?;
                // Store the tag (variant index)
                let tag_ptr = bld(unsafe {
                    self.builder.build_in_bounds_gep(
                        enum_llvm,
                        tmp,
                        &[self.i32_ty.const_zero(), self.i32_ty.const_zero()],
                        "enum.tag",
                    )
                })?;
                bld(self.builder.build_store(
                    tag_ptr,
                    self.i64_ty.const_int(vidx as u64, false),
                ))?;
                // Store the payload (if the variant carries one)
                if let (Some(a), Some(pt)) = (arg, payload) {
                    let pay_ptr = bld(unsafe {
                        self.builder.build_in_bounds_gep(
                            enum_llvm,
                            tmp,
                            &[self.i32_ty.const_zero(), self.i32_ty.const_int(1, false)],
                            "enum.payload",
                        )
                    })?;
                    if is_agg(&pt) {
                        let src = self.gen_value(a)?.agg(*span, "enum payload")?;
                        let n = aero_size(&pt, self.hir_structs, self.hir_unions, self.hir_enums, &self.type_subst);
                        self.emit_memcpy(pay_ptr, src, n, *span, "enum payload")?;
                    } else {
                        let v = self.gen_value(a)?.scalar(*span, "enum payload")?;
                        let pt_llvm = self.t(&pt, *span)?;
                        let typed = bld(self.builder.build_pointer_cast(
                            pay_ptr,
                            pt_llvm.ptr_type(AddressSpace::from(0u16)),
                            "enum.payptr",
                        ))?;
                        let v = self.coerce(v, &pt_llvm, *span, "enum payload")?;
                        bld(self.builder.build_store(typed, v))?;
                    }
                }
                Ok(GenValue::Agg(tmp))
            }
            HirExpr::Field { target, field, span } => self.gen_field(target, field, *span),
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
                if v.get_type().is_float_type() {
                    let fv = v.into_float_value();
                    let neg = bld(self.builder.build_float_neg(fv, "fneg"))?;
                    return Ok(GenValue::Scalar(neg.into()));
                }
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
                // Operator overloading: a non-numeric user type lowers `a op b` to a
                // trait method call (`Add::add(a, b)` etc.) instead of an LLVM opcode.
                if self.resolve_ty(&ty).is_named_type() {
                    return self.gen_binop_trait(lhs, rhs, *op, *span);
                }
                let l = self.gen_value(lhs)?.scalar(*span, "arithmetic")?;
                let r = self.gen_value(rhs)?.scalar(*span, "arithmetic")?;

                // Check if this is float arithmetic
                let is_float = matches!(self.resolve_ty(&ty), Ty::F32 | Ty::F64);
                if is_float {
                    let l_ty = l.get_type();
                    let r = self.coerce(r, &l_ty, *span, "arithmetic operand")?;
                    let lf = l.into_float_value();
                    let rf = r.into_float_value();
                    let out = bld(match op {
                        BinOp::Add => self.builder.build_float_add(lf, rf, "fadd"),
                        BinOp::Sub => self.builder.build_float_sub(lf, rf, "fsub"),
                        BinOp::Mul => self.builder.build_float_mul(lf, rf, "fmul"),
                        BinOp::Div => self.builder.build_float_div(lf, rf, "fdiv"),
                        BinOp::Rem => self.builder.build_float_rem(lf, rf, "frem"),
                        BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr => {
                            return Err(self.internal_err(
                                *span,
                                "bitwise operator used on a float (integer-only)",
                            ));
                        }
                    })?;
                    return Ok(GenValue::Scalar(out.into()));
                }

                // Integer arithmetic
                let l_ty = l.get_type();
                let r = self.coerce(r, &l_ty, *span, "arithmetic operand")?;
                let l = l.into_int_value();
                let r = r.into_int_value();
                // Division / remainder by zero traps on x86 (SIGFPE) and would otherwise
                // crash the whole process. Guard it: check `r == 0` and emit 0 instead.
                if matches!(op, BinOp::Div | BinOp::Rem) {
                    let out_slot = bld(self.builder.build_alloca(l_ty, "div_res"))?;
                    let is_zero = bld(self.builder.build_int_compare(
                        IntPredicate::EQ,
                        r,
                        l_ty.const_zero().into_int_value(),
                        "divbyzero",
                    ))?;
                    let zero_bb = self.context.append_basic_block(self.cur_func, "div_zero");
                    let calc_bb = self.context.append_basic_block(self.cur_func, "div_calc");
                    let join_bb = self.context.append_basic_block(self.cur_func, "div_join");
                    bld(self.builder.build_conditional_branch(is_zero, zero_bb, calc_bb))?;
                    self.builder.position_at_end(zero_bb);
                    bld(self.builder.build_store(out_slot, l_ty.const_zero()))?;
                    bld(self.builder.build_unconditional_branch(join_bb))?;
                    self.builder.position_at_end(calc_bb);
                    let out = bld(match op {
                        BinOp::Div => self.builder.build_int_signed_div(l, r, "div"),
                        BinOp::Rem => self.builder.build_int_signed_rem(l, r, "rem"),
                        _ => unreachable!(),
                    })?;
                    bld(self.builder.build_store(out_slot, out))?;
                    bld(self.builder.build_unconditional_branch(join_bb))?;
                    self.builder.position_at_end(join_bb);
                    let res = bld(self.builder.build_load(l_ty, out_slot, "div_out"))?;
                    return Ok(GenValue::Scalar(res.into()));
                }
                let out = bld(match op {
                    BinOp::Add => self.builder.build_int_add(l, r, "add"),
                    BinOp::Sub => self.builder.build_int_sub(l, r, "sub"),
                    BinOp::Mul => self.builder.build_int_mul(l, r, "mul"),
                    BinOp::Div => self.builder.build_int_signed_div(l, r, "div"),
                    BinOp::Rem => self.builder.build_int_signed_rem(l, r, "rem"),
                    BinOp::BitAnd => self.builder.build_and(l, r, "and"),
                    BinOp::BitOr => self.builder.build_or(l, r, "or"),
                    BinOp::BitXor => self.builder.build_xor(l, r, "xor"),
                    BinOp::Shl => self.builder.build_left_shift(l, r, "shl"),
                    // Aero integers are signed: use an arithmetic (sign-extending)
                    // right shift, matching C's `>>` on signed types.
                    BinOp::Shr => self.builder.build_right_shift(l, r, true, "shr"),
                })?;
                Ok(GenValue::Scalar(out.into()))
            }
            HirExpr::Cmp { op, lhs, rhs, span } => {
                // Operator overloading: a non-numeric user type lowers comparisons to
                // trait method calls (`Eq::eq` / `Ord::lt`).
                let l_ty = self.expr_ty(lhs)?;
                if self.resolve_ty(&l_ty).is_named_type() {
                    return self.gen_cmp_trait(lhs, rhs, *op, *span);
                }
                let l = self.gen_value(lhs)?.scalar(*span, "comparison")?;
                let r = self.gen_value(rhs)?.scalar(*span, "comparison")?;
                if l.get_type().is_pointer_type() {
                    // A `str` value is an `i8*`: all six operators compare the
                    // strcmp result with 0 (`a < b` <=> strcmp(a, b) < 0, etc.)
                    if self.resolve_ty(&l_ty) == Ty::Str {
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
                    // Raw pointer comparison: null checks (`p == 0`) and pointer
                    // equality (`p == q`). The integer literal operand is coerced to
                    // the pointer type (inttoptr), then both are ptrtoint'd to i64
                    // and compared as integers.
                    let pred = match op {
                        CmpOp::Eq => IntPredicate::EQ,
                        CmpOp::Ne => IntPredicate::NE,
                        _ => {
                            return Err(self.internal_err(
                                *span,
                                "only `==` and `!=` are supported for pointer comparisons",
                            ));
                        }
                    };
                    let l_llvm = l.get_type();
                    let r = self.coerce(r, &l_llvm, *span, "pointer comparison operand")?;
                    let li = bld(self
                        .builder
                        .build_ptr_to_int(l.into_pointer_value(), self.i64_ty, "ptrcmp.l"))?;
                    let ri = bld(self
                        .builder
                        .build_ptr_to_int(r.into_pointer_value(), self.i64_ty, "ptrcmp.r"))?;
                    let out = bld(self.builder.build_int_compare(pred, li, ri, "ptrcmp"))?;
                    return Ok(GenValue::Scalar(out.into()));
                }
                if r.get_type().is_pointer_type() {
                    // `0 == p` (integer literal on the left, pointer on the right).
                    // The integer operand is inttoptr'd to the pointer type.
                    let r_ty = self.expr_ty(rhs)?;
                    if self.resolve_ty(&r_ty) == Ty::Str {
                        return Err(self.internal_err(
                            *span,
                            "mismatched string comparison operands",
                        ));
                    }
                    let pred = match op {
                        CmpOp::Eq => IntPredicate::EQ,
                        CmpOp::Ne => IntPredicate::NE,
                        _ => {
                            return Err(self.internal_err(
                                *span,
                                "only `==` and `!=` are supported for pointer comparisons",
                            ));
                        }
                    };
                    let r_llvm = r.get_type();
                    let l = self.coerce(l, &r_llvm, *span, "pointer comparison operand")?;
                    let li = bld(self
                        .builder
                        .build_ptr_to_int(l.into_pointer_value(), self.i64_ty, "ptrcmp.l"))?;
                    let ri = bld(self
                        .builder
                        .build_ptr_to_int(r.into_pointer_value(), self.i64_ty, "ptrcmp.r"))?;
                    let out = bld(self.builder.build_int_compare(pred, li, ri, "ptrcmp"))?;
                    return Ok(GenValue::Scalar(out.into()));
                }
                // Float comparison
                if l.get_type().is_float_type() {
                    let l_ty = l.get_type();
                    let r = self.coerce(r, &l_ty, *span, "comparison operand")?;
                    let lf = l.into_float_value();
                    let rf = r.into_float_value();
                    let pred = match op {
                        CmpOp::Eq => FloatPredicate::OEQ,
                        CmpOp::Ne => FloatPredicate::ONE,
                        CmpOp::Lt => FloatPredicate::OLT,
                        CmpOp::Gt => FloatPredicate::OGT,
                        CmpOp::Le => FloatPredicate::OLE,
                        CmpOp::Ge => FloatPredicate::OGE,
                    };
                    let out = bld(self.builder.build_float_compare(pred, lf, rf, "fcmp"))?;
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
            HirExpr::FnRef { def_id, span } => {
                // A first-class function reference: produce the LLVM function
                // pointer value for the named function.
                let hir_f = self
                    .hir_funcs
                    .get(*def_id as usize)
                    .ok_or_else(|| self.internal_err(*span, "missing function table"))?;
                if hir_f.builtin {
                    return Err(self.internal_err(
                        *span,
                        &format!(
                            "builtin `{}` cannot be used as a function pointer value",
                            hir_f.name
                        ),
                    ));
                }
                let func = *self
                    .funcs
                    .get(*def_id as usize)
                    .ok_or_else(|| self.internal_err(*span, "missing function table"))?;
                // A function value is itself a pointer in LLVM.
                Ok(GenValue::Scalar(func.as_global_value().as_pointer_value().into()))
            }
            HirExpr::CallPtr {
                callee,
                args,
                span,
            } => {
                // Indirect call through a first-class function pointer: load the
                // callee's function-pointer value, build the LLVM function type from
                // the callee's inferred `Ty::Fn(params, ret)`, and call through it.
                let callee_ty = self.resolve_ty(&self.expr_ty(callee)?);
                let (params, ret) = match &callee_ty {
                    Ty::Fn(params, ret) => (params.clone(), (**ret).clone()),
                    other => {
                        return Err(self.internal_err(
                            *span,
                            &format!("cannot call a value of type `{other}` (expected a function pointer)"),
                        ))
                    }
                };
                let mut param_tys = Vec::new();
                for p in &params {
                    param_tys.push(self.t(p, *span)?.into());
                }
                let fn_ty = if matches!(ret, Ty::Void) {
                    self.context.void_type().fn_type(&param_tys, false)
                } else {
                    let ret_basic = self.t(&ret, *span)?;
                    ret_basic.fn_type(&param_tys, false)
                };
                let callee_v = self.gen_value(callee)?.scalar(*span, "function pointer callee")?;
                let fn_ptr = bld(self.builder.build_pointer_cast(
                    callee_v.into_pointer_value(),
                    self.context.ptr_type(AddressSpace::from(0u16)).into(),
                    "fn_cast",
                ))?;
                let mut call_args = Vec::new();
                for (i, a) in args.iter().enumerate() {
                    let v = self.gen_value(a)?;
                    let pt: BasicTypeEnum = param_tys[i]
                        .try_into()
                        .map_err(|_| self.internal_err(*span, "parameter type mismatch"))?;
                    let v = self.call_arg(v, &pt, *span, "indirect call argument")?;
                    call_args.push(v.into());
                }
                let out = bld(self.builder.build_indirect_call(fn_ty, fn_ptr, &call_args, "indir"))?;
                match out.try_as_basic_value().basic() {
                    Some(v) => Ok(GenValue::Scalar(v)),
                    None => Err(self.internal_err(*span, "void indirect call used as an expression")),
                }
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
                        "arg_count" => {
                            if !args.is_empty() {
                                return Err(CodegenError {
                                    msg: "`arg_count` takes no arguments".to_string(),
                                    line: span.line,
                                    col: span.col,
                                });
                            }
                            let ac = bld(self.builder.build_load(
                                self.i32_ty,
                                self.aero_argc.as_pointer_value(),
                                "argc",
                            ))?;
                            let ac64 = bld(self.builder.build_int_z_extend(
                                ac.into_int_value(),
                                self.i64_ty,
                                "argc64",
                            ))?;
                            return Ok(GenValue::Scalar(ac64.into()));
                        }
                        "arg" => {
                            if args.len() != 1 {
                                return Err(CodegenError {
                                    msg: "`arg` requires 1 index argument".to_string(),
                                    line: span.line,
                                    col: span.col,
                                });
                            }
                            let i = self.gen_value(&args[0])?.scalar(*span, "arg index")?;
                            let i = self.coerce(i, &self.i64_ty.into(), *span, "arg index")?;
                            let i = i.into_int_value();
                            let ac = bld(self.builder.build_load(
                                self.i32_ty,
                                self.aero_argc.as_pointer_value(),
                                "argc",
                            ))?;
                            let ac64 = bld(self.builder.build_int_z_extend(
                                ac.into_int_value(),
                                self.i64_ty,
                                "argc64",
                            ))?;
                            let neg = bld(self.builder.build_int_compare(
                                IntPredicate::SLT,
                                i,
                                self.i64_ty.const_zero(),
                                "i_neg",
                            ))?;
                            let ge = bld(self.builder.build_int_compare(
                                IntPredicate::SGE,
                                i,
                                ac64,
                                "i_ge",
                            ))?;
                            let oob = bld(self.builder.build_or(neg, ge, "i_oob"))?;
                            let ptr_ty = self.context.ptr_type(AddressSpace::from(0u16));
                            let empty = self.global_string("")?;
                            let ok_bb = self.context.append_basic_block(self.cur_func, "arg_ok");
                            let merge_bb = self.context.append_basic_block(self.cur_func, "arg_merge");
                            let oob_bb = self.builder.get_insert_block().unwrap();
                            bld(self.builder.build_conditional_branch(oob, merge_bb, ok_bb))?;
                            self.builder.position_at_end(ok_bb);
                            let argv = bld(self.builder.build_load(
                                ptr_ty,
                                self.aero_argv.as_pointer_value(),
                                "argv",
                            ))?;
                            let argv = argv.into_pointer_value();
                            let slot = bld(unsafe {
                                self.builder.build_in_bounds_gep(ptr_ty, argv, &[i], "argv_i")
                            })?;
                            let s = bld(self.builder.build_load(ptr_ty, slot, "arg_s"))?;
                            let s = s.into_pointer_value();
                            bld(self.builder.build_unconditional_branch(merge_bb))?;
                            self.builder.position_at_end(merge_bb);
                            let res = self.builder.build_phi(ptr_ty, "arg_res").map_err(|e| {
                                CodegenError {
                                    msg: format!("LLVM IR construction failed: {e}"),
                                    line: span.line,
                                    col: span.col,
                                }
                            })?;
                            let e: BasicValueEnum = empty.into();
                            let sv: BasicValueEnum = s.into();
                            res.add_incoming(&[(&e, oob_bb), (&sv, ok_bb)]);
                            return Ok(GenValue::Scalar(res.as_basic_value()));
                        }
                        "read_file" => {
                            if args.len() != 1 {
                                return Err(CodegenError {
                                    msg: "`read_file` requires 1 path argument".to_string(),
                                    line: span.line,
                                    col: span.col,
                                });
                            }
                            // Read file using fopen/fread/fclose with a single 4KB read
                            let path = self.gen_value(&args[0])?.scalar(*span, "read_file path")?;
                            let ptr_ty = self.context.ptr_type(AddressSpace::from(0u16));
                            let mode = self.global_string("rb")?;
                            let fp = bld(self.builder.build_call(
                                self.fopen,
                                &[path.into(), mode.into()],
                                "fopen",
                            ))?
                            .try_as_basic_value()
                            .basic()
                            .ok_or_else(|| self.internal_err(*span, "fopen returned no value"))?
                            .into_pointer_value();
                            let empty = self.global_string("")?;
                            let zero64 = self.i64_ty.const_zero();
                            let fp_i = bld(self.builder.build_ptr_to_int(fp, self.i64_ty, "fpi"))?;
                            let is_null = bld(self.builder.build_int_compare(
                                IntPredicate::EQ,
                                fp_i,
                                zero64,
                                "fp_null",
                            ))?;
                            let ok_bb = self.context.append_basic_block(self.cur_func, "rf_ok");
                            let merge_bb = self.context.append_basic_block(self.cur_func, "rf_merge");
                            let oob_bb = self.builder.get_insert_block().unwrap();
                            bld(self.builder.build_conditional_branch(is_null, merge_bb, ok_bb))?;
                            self.builder.position_at_end(ok_bb);
                            // Read up to 4KB using fread, then NUL-terminate
                            let buf_size = self.i64_ty.const_int(4096, false);
                            let buf = bld(self.builder.build_call(
                                self.malloc,
                                &[buf_size.into()],
                                "rbuf",
                            ))?
                            .try_as_basic_value()
                            .basic()
                            .ok_or_else(|| self.internal_err(*span, "malloc returned no value"))?
                            .into_pointer_value();
                            let one64 = self.i64_ty.const_int(1, false);
                            let read_n = bld(self.builder.build_call(
                                self.fread,
                                &[buf.into(), one64.into(), buf_size.into(), fp.into()],
                                "fread",
                            ))?
                            .try_as_basic_value()
                            .basic()
                            .ok_or_else(|| self.internal_err(*span, "fread returned no value"))?
                            .into_int_value();
                            // NUL-terminate at the actual number of bytes read (fread
                            // return value), clamped to the buffer size minus one so we
                            // never write past the allocation. Without this, `len()`
                            // (strlen) scans into uninitialized malloc garbage after the
                            // file content.
                            let cap_idx = self.i64_ty.const_int(4095, false);
                            let is_gt = bld(self.builder.build_int_compare(
                                IntPredicate::UGT,
                                read_n,
                                cap_idx,
                                "rend_gt",
                            ))?;
                            let end_idx = bld(self.builder.build_select(
                                is_gt,
                                cap_idx,
                                read_n,
                                "rend_idx",
                            ))?
                            .into_int_value();
                            let endp = bld(unsafe {
                                self.builder.build_in_bounds_gep(
                                    self.context.i8_type(),
                                    buf,
                                    &[end_idx],
                                    "rend",
                                )
                            })?;
                            bld(self.builder.build_store(endp, self.context.i8_type().const_zero()))?;
                            bld(self.builder.build_call(self.fclose, &[fp.into()], "fclose"))?;
                            bld(self.builder.build_unconditional_branch(merge_bb))?;
                            self.builder.position_at_end(merge_bb);
                            let res = self.builder.build_phi(ptr_ty, "rf_res").map_err(|e| {
                                CodegenError {
                                    msg: format!("LLVM IR construction failed: {e}"),
                                    line: span.line,
                                    col: span.col,
                                }
                            })?;
                            // Cast empty string pointer to i8* to match PHI type
                            let empty_i8 = bld(self.builder.build_pointer_cast(empty, ptr_ty, "empty_i8"))?;
                            let e: BasicValueEnum = empty_i8.into();
                            let b: BasicValueEnum = buf.into();
                            res.add_incoming(&[(&e, oob_bb), (&b, ok_bb)]);
                            return Ok(GenValue::Scalar(res.as_basic_value()));
                        }
                        "write_file" => {
                            if args.len() != 2 {
                                return Err(CodegenError {
                                    msg: "`write_file` requires 2 arguments (path, contents)".to_string(),
                                    line: span.line,
                                    col: span.col,
                                });
                            }
                            let path = self.gen_value(&args[0])?.scalar(*span, "write_file path")?;
                            let contents = self.gen_value(&args[1])?.scalar(*span, "write_file contents")?;
                            let mode = self.global_string("wb")?;
                            let fp = bld(self.builder.build_call(
                                self.fopen,
                                &[path.into(), mode.into()],
                                "fopen",
                            ))?
                            .try_as_basic_value()
                            .basic()
                            .ok_or_else(|| self.internal_err(*span, "fopen returned no value"))?
                            .into_pointer_value();
                            let fail = self.i64_ty.const_int(u64::MAX, false); // -1
                            let zero64 = self.i64_ty.const_zero();
                            let fp_i = bld(self.builder.build_ptr_to_int(fp, self.i64_ty, "fpi"))?;
                            let is_null = bld(self.builder.build_int_compare(
                                IntPredicate::EQ,
                                fp_i,
                                zero64,
                                "fp_null",
                            ))?;
                            let ok_bb = self.context.append_basic_block(self.cur_func, "wf_ok");
                            let merge_bb = self.context.append_basic_block(self.cur_func, "wf_merge");
                            let oob_bb = self.builder.get_insert_block().unwrap();
                            bld(self.builder.build_conditional_branch(is_null, merge_bb, ok_bb))?;
                            self.builder.position_at_end(ok_bb);
                            let len = bld(self.builder.build_call(self.strlen, &[contents.into()], "wlen"))?
                                .try_as_basic_value()
                                .basic()
                                .ok_or_else(|| self.internal_err(*span, "strlen returned no value"))?
                                .into_int_value();
                            let one64 = self.i64_ty.const_int(1, false);
                            let written = bld(self.builder.build_call(
                                self.fwrite,
                                &[contents.into(), one64.into(), len.into(), fp.into()],
                                "fwrite",
                            ))?
                            .try_as_basic_value()
                            .basic()
                            .ok_or_else(|| self.internal_err(*span, "fwrite returned no value"))?
                            .into_int_value();
                            bld(self.builder.build_call(self.fclose, &[fp.into()], "fclose"))?;
                            bld(self.builder.build_unconditional_branch(merge_bb))?;
                            self.builder.position_at_end(merge_bb);
                            let res = self.builder.build_phi(self.i64_ty, "wf_res").map_err(|e| {
                                CodegenError {
                                    msg: format!("LLVM IR construction failed: {e}"),
                                    line: span.line,
                                    col: span.col,
                                }
                            })?;
                            let f: BasicValueEnum = fail.into();
                            let w: BasicValueEnum = written.into();
                            res.add_incoming(&[(&f, oob_bb), (&w, ok_bb)]);
                            return Ok(GenValue::Scalar(res.as_basic_value()));
                        }

                        "rand" => {
                            if !args.is_empty() {
                                return Err(CodegenError {
                                    msg: "`rand` takes no arguments".to_string(),
                                    line: span.line,
                                    col: span.col,
                                });
                            }
                            let r =
                                bld(self.builder.build_call(self.rand, &[], "rand"))?
                                    .try_as_basic_value()
                                    .basic()
                                    .ok_or_else(|| self.internal_err(*span, "rand returned no value"))?
                                    .into_int_value();
                            // zext i32 -> i64 to match the builtin's declared return type
                            let r64 =
                                bld(self.builder.build_int_z_extend(r, self.i64_ty, "rand64"))?;
                            return Ok(GenValue::Scalar(r64.into()));
                        }
                        "time" => {
                            if !args.is_empty() {
                                return Err(CodegenError {
                                    msg: "`time` takes no arguments".to_string(),
                                    line: span.line,
                                    col: span.col,
                                });
                            }
                            let null = self
                                .context
                                .ptr_type(AddressSpace::from(0u16))
                                .const_null();
                            let t =
                                bld(self.builder.build_call(self.time, &[null.into()], "time"))?
                                    .try_as_basic_value()
                                    .basic()
                                    .ok_or_else(|| self.internal_err(*span, "time returned no value"))?;
                            return Ok(GenValue::Scalar(t));
                        }
                        "get_env" => {
                            if args.len() != 1 {
                                return Err(CodegenError {
                                    msg: "`get_env` requires 1 environment-variable name".to_string(),
                                    line: span.line,
                                    col: span.col,
                                });
                            }
                            let name = self.gen_value(&args[0])?.scalar(*span, "get_env name")?;
                            let res = bld(self.builder.build_call(self.getenv, &[name.into()], "getenv"))?
                                .try_as_basic_value()
                                .basic()
                                .ok_or_else(|| self.internal_err(*span, "getenv returned no value"))?
                                .into_pointer_value();
                            let ptr_ty = self.context.ptr_type(AddressSpace::from(0u16));
                            let is_null = bld(self.builder.build_ptr_to_int(res, self.i64_ty, "ge_null"))?;
                            let nz = bld(self.builder.build_int_compare(
                                IntPredicate::EQ,
                                is_null,
                                self.i64_ty.const_zero(),
                                "ge_nullbool",
                            ))?;
                            let empty = self.global_string("")?;
                            let ok_bb = self.context.append_basic_block(self.cur_func, "ge_ok");
                            let merge_bb = self.context.append_basic_block(self.cur_func, "ge_merge");
                            let oob_bb = self.builder.get_insert_block().unwrap();
                            bld(self.builder.build_conditional_branch(nz, merge_bb, ok_bb))?;
                            self.builder.position_at_end(ok_bb);
                            bld(self.builder.build_unconditional_branch(merge_bb))?;
                            self.builder.position_at_end(merge_bb);
                            let phi = self.builder.build_phi(ptr_ty, "ge_res").map_err(|e| {
                                CodegenError { msg: format!("LLVM IR construction failed: {e}"), line: span.line, col: span.col }
                            })?;
                            let ev: BasicValueEnum = empty.into();
                            let vv: BasicValueEnum = res.into();
                            phi.add_incoming(&[(&ev, oob_bb), (&vv, ok_bb)]);
                            return Ok(GenValue::Scalar(phi.as_basic_value()));
                        }
                        "has_env" => {
                            if args.len() != 1 {
                                return Err(CodegenError {
                                    msg: "`has_env` requires 1 environment-variable name".to_string(),
                                    line: span.line,
                                    col: span.col,
                                });
                            }
                            let name = self.gen_value(&args[0])?.scalar(*span, "has_env name")?;
                            let res = bld(self.builder.build_call(self.getenv, &[name.into()], "getenv"))?
                                .try_as_basic_value()
                                .basic()
                                .ok_or_else(|| self.internal_err(*span, "getenv returned no value"))?
                                .into_pointer_value();
                            let is_null = bld(self.builder.build_ptr_to_int(res, self.i64_ty, "he_null"))?;
                            let nz = bld(self.builder.build_int_compare(
                                IntPredicate::NE,
                                is_null,
                                self.i64_ty.const_zero(),
                                "he_has",
                            ))?;
                            return Ok(GenValue::Scalar(nz.into()));
                        }
                        "set_env" => {
                            if args.len() != 2 {
                                return Err(CodegenError {
                                    msg: "`set_env` requires 2 arguments (name, value)".to_string(),
                                    line: span.line,
                                    col: span.col,
                                });
                            }
                            let name = self.gen_value(&args[0])?.scalar(*span, "set_env name")?;
                            let val = self.gen_value(&args[1])?.scalar(*span, "set_env value")?;
                            let rc = bld(self.builder.build_call(
                                self.putenv,
                                &[name.into(), val.into()],
                                "_putenv_s",
                            ))?;
                            let rc = rc.try_as_basic_value().basic().ok_or_else(|| self.internal_err(*span, "_putenv_s returned no value"))?.into_int_value();
                            let ok = bld(self.builder.build_int_compare(IntPredicate::EQ, rc, self.i32_ty.const_zero(), "se_ok"))?;
                            return Ok(GenValue::Scalar(ok.into()));
                        }
                        "file_exists" => {
                            if args.len() != 1 {
                                return Err(CodegenError {
                                    msg: "`file_exists` requires 1 path argument".to_string(),
                                    line: span.line,
                                    col: span.col,
                                });
                            }
                            let path = self.gen_value(&args[0])?.scalar(*span, "file_exists path")?;
                            let mode = self.global_string("rb")?;
                            let fp = bld(self.builder.build_call(
                                self.fopen,
                                &[path.into(), mode.into()],
                                "fe_fopen",
                            ))?
                            .try_as_basic_value().basic().ok_or_else(|| self.internal_err(*span, "fopen returned no value"))?
                            .into_pointer_value();
                            let fp_i = bld(self.builder.build_ptr_to_int(fp, self.i64_ty, "fe_fpi"))?;
                            let exists = bld(self.builder.build_int_compare(
                                IntPredicate::NE,
                                fp_i,
                                self.i64_ty.const_zero(),
                                "fe_exists",
                            ))?;
                            // If the file opened, close it (ignore the result).
                            let ok_bb = self.context.append_basic_block(self.cur_func, "fe_ok");
                            let merge_bb = self.context.append_basic_block(self.cur_func, "fe_merge");
                            let oob_bb = self.builder.get_insert_block().unwrap();
                            bld(self.builder.build_conditional_branch(exists, ok_bb, merge_bb))?;
                            self.builder.position_at_end(ok_bb);
                            bld(self.builder.build_call(self.fclose, &[fp.into()], "fe_fclose"))?;
                            bld(self.builder.build_unconditional_branch(merge_bb))?;
                            self.builder.position_at_end(merge_bb);
                            return Ok(GenValue::Scalar(exists.into()));
                        }
                        "format" => {
                            return self.gen_format(args, *span);
                        }
                        "str_hash" => {
                            if args.len() != 1 {
                                return Err(CodegenError {
                                    msg: "`str_hash` requires 1 string argument".to_string(),
                                    line: span.line,
                                    col: span.col,
                                });
                            }
                            return self.gen_str_hash(&args[0], *span);
                        }
                        "hash_i64" => {
                            if args.len() != 1 {
                                return Err(CodegenError {
                                    msg: "`hash_i64` requires 1 integer argument".to_string(),
                                    line: span.line,
                                    col: span.col,
                                });
                            }
                            return self.gen_hash_i64(&args[0], *span);
                        }

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
                        "utf8_len" => {
                            if args.len() != 1 {
                                return Err(CodegenError {
                                    msg: "`utf8_len` requires 1 string argument".to_string(),
                                    line: span.line,
                                    col: span.col,
                                });
                            }
                            let s = self.gen_value(&args[0])?.scalar(*span, "utf8_len argument")?;
                            let s = s.into_pointer_value();
                            let bytelen = bld(self.builder.build_call(self.strlen, &[s.into()], "ulen_byte"))?
                                .try_as_basic_value()
                                .basic()
                                .ok_or_else(|| self.internal_err(*span, "strlen returned no value"))?
                                .into_int_value();
                            let n = bld(self.builder.build_call(
                                self.utf8_len_f,
                                &[s.into(), bytelen.into()],
                                "utf8_len",
                            ))?
                            .try_as_basic_value()
                            .basic()
                            .ok_or_else(|| self.internal_err(*span, "aero_utf8_len returned no value"))?;
                            return Ok(GenValue::Scalar(n.into()));
                        }
                        "utf8_at" => {
                            if args.len() != 2 {
                                return Err(CodegenError {
                                    msg: "`utf8_at` requires 2 arguments (string, index)".to_string(),
                                    line: span.line,
                                    col: span.col,
                                });
                            }
                            let s = self.gen_value(&args[0])?.scalar(*span, "utf8_at string")?;
                            let s = s.into_pointer_value();
                            let idx = self.gen_value(&args[1])?.scalar(*span, "utf8_at index")?;
                            let idx = self.coerce(idx, &self.i64_ty.into(), *span, "utf8_at index")?.into_int_value();
                            let bytelen = bld(self.builder.build_call(self.strlen, &[s.into()], "uat_byte"))?
                                .try_as_basic_value()
                                .basic()
                                .ok_or_else(|| self.internal_err(*span, "strlen returned no value"))?
                                .into_int_value();
                            let cp = bld(self.builder.build_call(
                                self.utf8_at_f,
                                &[s.into(), bytelen.into(), idx.into()],
                                "utf8_at",
                            ))?
                            .try_as_basic_value()
                            .basic()
                            .ok_or_else(|| self.internal_err(*span, "aero_utf8_at returned no value"))?;
                            return Ok(GenValue::Scalar(cp.into()));
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
                // `const fn` called with compile-time-constant arguments: fold the
                // result into an LLVM constant instead of emitting a runtime call
                // (Phase 12.6). Falls back to a normal call on any non-constant arg.
                if hir_f.is_const {
                    if let Some(cv) = const_eval::try_eval_call(self.hir_consts, self.hir_funcs, hir_f, args) {
                        return Ok(GenValue::Scalar(self.gen_const_val(&cv, &hir_f.ret, *span)?));
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
            // `expr as dyn Trait` (Phase 9): box the target on the heap and build a
            // fat pointer `{ data, vtable }` — the concrete value is copied to the
            // heap (malloc+memcpy), and the vtable holds a thunk per trait method.
            HirExpr::Cast { target, ty, span } => self.gen_dyn_cast(target, ty, *span),
        }
    }

    /// Generate `expr as dyn Trait`: heap-allocate a copy of the target value and build
    /// the `{ data: i8*, vtable: i8* }` fat pointer. The vtable is a global array of
    /// thunk pointers (one per trait method, in trait declaration order).
    fn gen_dyn_cast(
        &mut self,
        target: &HirExpr,
        ty: &Ty,
        span: Span,
    ) -> Result<GenValue<'ctx>, CodegenError> {
        let Ty::Dyn { trait_name } = ty else {
            return Err(self.internal_err(span, "`as` only casts to a `dyn Trait` type"));
        };
        // The concrete payload type (a non-generic struct/enum, guaranteed by infer).
        let target_ty = self.resolve_ty(&self.expr_ty(target)?);
        let concrete_name = match &target_ty {
            Ty::Struct(n) | Ty::Enum(n) => n.clone(),
            _ => {
                return Err(self.internal_err(
                    span,
                    &format!("cannot box non-nominal type `{target_ty}` as `dyn {trait_name}`"),
                ))
            }
        };
        let i8_ptr = self.context.ptr_type(AddressSpace::from(0u16));

        // 1. Heap-allocate space for the concrete value and copy it in.
        let size = aero_size(&target_ty, self.hir_structs, self.hir_unions, self.hir_enums, &self.type_subst);
        let size64 = self.i64_ty.const_int(size, false);
        let raw = bld(self.builder.build_call(self.malloc, &[size64.into()], "dyn_alloc"))?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| self.internal_err(span, "malloc returned no value"))?
            .into_pointer_value();
        let data = bld(self.builder.build_pointer_cast(raw, i8_ptr, "dyn_data"))?;
        match self.gen_value(target)? {
            GenValue::Agg(slot) => self.emit_memcpy(data, slot, size, span, "dyn box")?,
            GenValue::Scalar(v) => {
                // Scalar payload: store into a temp slot then memcpy, so the heap
                // copy is byte-identical to the concrete type's layout.
                let slot_ty = self.t(&target_ty, span)?;
                let tmp = bld(self.builder.build_alloca(slot_ty, "dyn_tmp"))?;
                bld(self.builder.build_store(tmp, v))?;
                self.emit_memcpy(data, tmp, size, span, "dyn box")?;
            }
        }

        // 2. The vtable (cached per (concrete type, trait)).
        let vtable = self.gen_dyn_vtable(&concrete_name, trait_name, span)?;

        // 3. Build the fat pointer `{ data, vtable }`.
        let dyn_llvm = self.t(ty, span)?;
        let fat = bld(self.builder.build_alloca(dyn_llvm, "dyn_fat"))?;
        let d_slot = bld(unsafe {
            self.builder.build_in_bounds_gep(
                dyn_llvm,
                fat,
                &[self.i32_ty.const_zero(), self.i32_ty.const_zero()],
                "dyn_fat.data",
            )
        })?;
        let v_slot = bld(unsafe {
            self.builder.build_in_bounds_gep(
                dyn_llvm,
                fat,
                &[self.i32_ty.const_zero(), self.i32_ty.const_int(1, false)],
                "dyn_fat.vtable",
            )
        })?;
        bld(self.builder.build_store(d_slot, data))?;
        bld(self.builder.build_store(v_slot, vtable))?;
        let loaded = bld(self.builder.build_load(dyn_llvm, fat, "dyn"))?;
        Ok(GenValue::Scalar(loaded))
    }

    /// Build (and cache) the vtable global for a concrete type implementing a trait.
    /// The vtable is an array of function pointers, one per trait method in
    /// declaration order. Each entry is a **thunk**: `fn(i8* data, args...) -> ret`
    /// that loads the concrete value from `data` and calls the real impl method.
    fn gen_dyn_vtable(
        &mut self,
        concrete_name: &str,
        trait_name: &str,
        span: Span,
    ) -> Result<PointerValue<'ctx>, CodegenError> {
        let key = (concrete_name.to_string(), trait_name.to_string());
        if let Some(v) = self.dyn_vtables.get(&key) {
            return Ok(*v);
        }
        let trait_def = self
            .hir_traits
            .iter()
            .find(|t| t.name == *trait_name)
            .ok_or_else(|| self.internal_err(span, &format!("trait `{trait_name}` not found")))?;
        let i8_ptr = self.context.ptr_type(AddressSpace::from(0u16));
        let mut entries: Vec<FunctionValue<'ctx>> = Vec::new();
        for method in &trait_def.methods {
            let def_id = self
                .method_map
                .get(&(concrete_name.to_string(), method.name.clone()))
                .copied()
                .ok_or_else(|| {
                    self.internal_err(
                        span,
                        &format!(
                            "type `{concrete_name}` does not implement trait method `{}`",
                            method.name
                        ),
                    )
                })?;
            let impl_f = self
                .funcs
                .get(def_id as usize)
                .copied()
                .ok_or_else(|| self.internal_err(span, "missing impl method function table"))?;
            entries.push(self.gen_dyn_thunk(concrete_name, &method.name, impl_f, span)?);
        }
        // Global array of function pointers (opaque `i8*`, so the vtable is uniform
        // regardless of each thunk's concrete signature). Constant casts (no builder
        // instructions) so the initializer is a compile-time constant.
        let arr_ty = i8_ptr.array_type(entries.len() as u32);
        let global = self
            .module
            .add_global(arr_ty, None, &format!("vt${concrete_name}$.${trait_name}$"));
        global.set_linkage(inkwell::module::Linkage::Internal);
        let consts: Vec<PointerValue<'ctx>> = entries
            .iter()
            .map(|f| {
                f.as_global_value()
                    .as_pointer_value()
                    .const_cast(self.context.ptr_type(AddressSpace::from(0u16)))
            })
            .collect();
        let init = i8_ptr.const_array(&consts);
        global.set_initializer(&init);
        let ptr = global.as_pointer_value();
        self.dyn_vtables.insert(key, ptr);
        Ok(ptr)
    }

    /// Generate a thunk `fn(i8* data, args...) -> ret` that loads the concrete value
    /// `data` points to and calls the real impl method (which takes the receiver by
    /// value). The vtable stores these thunk pointers, enabling uniform virtual calls.
    fn gen_dyn_thunk(
        &mut self,
        concrete_name: &str,
        method_name: &str,
        impl_f: FunctionValue<'ctx>,
        span: Span,
    ) -> Result<FunctionValue<'ctx>, CodegenError> {
        let i8_ptr = self.context.ptr_type(AddressSpace::from(0u16));
        let impl_ty = impl_f.get_type();
        let impl_params = impl_ty.get_param_types();
        // Thunk signature: `(i8* data, param1, ..., paramN) -> ret` (param0 is the
        // receiver, replaced by the opaque `data` pointer).
        let mut thunk_params: Vec<BasicMetadataTypeEnum> = Vec::with_capacity(impl_params.len());
        thunk_params.push(i8_ptr.into());
        for p in impl_params.iter().skip(1) {
            thunk_params.push((*p).into());
        }
        let ret_ty = impl_ty.get_return_type();
        let fn_ty = match ret_ty {
            Some(r) => r.fn_type(&thunk_params, false),
            None => self.context.void_type().fn_type(&thunk_params, false),
        };
        let name = format!("dyn${concrete_name}$${method_name}");
        if let Some(existing) = self.module.get_function(&name) {
            return Ok(existing);
        }
        let thunk = self.module.add_function(&name, fn_ty, None);
        let saved_func = self.cur_func;
        let saved_block = self.builder.get_insert_block();
        self.cur_func = thunk;
        let entry = self.context.append_basic_block(thunk, "entry");
        self.builder.position_at_end(entry);
        // Load the concrete receiver by value from `data`.
        let data = thunk.get_nth_param(0).unwrap().into_pointer_value();
        let concrete_ty: BasicTypeEnum = impl_params[0]
            .try_into()
            .map_err(|_| self.internal_err(span, "dyn receiver type mismatch"))?;
        let recv_ptr = bld(self.builder.build_pointer_cast(
            data,
            self.context.ptr_type(AddressSpace::from(0u16)),
            "thunk_recv",
        ))?;
        let recv = bld(self.builder.build_load(concrete_ty, recv_ptr, "thunk_load"))?;
        let mut args: Vec<BasicMetadataValueEnum> = vec![recv.into()];
        for i in 1..impl_params.len() {
            let p = thunk.get_nth_param(i as u32).unwrap();
            args.push(p.into());
        }
        let out = bld(self.builder.build_call(impl_f, &args, "thunk_call"))?;
        let rv: Option<BasicValueEnum> = out.try_as_basic_value().basic();
        bld(self.builder.build_return(rv.as_ref().map(|v| v as &dyn BasicValue)))?;
        self.cur_func = saved_func;
        if let Some(bb) = saved_block {
            self.builder.position_at_end(bb);
        }
        Ok(thunk)
    }

    /// Virtual dispatch on a `dyn Trait` receiver: load `data` + `vtable` from the fat
    /// pointer, index the vtable to the method's thunk, and call it with `data` + args.
    fn gen_dyn_method_call(
        &mut self,
        recv: &HirExpr,
        trait_name: &str,
        method: &str,
        args: &[HirExpr],
        span: Span,
    ) -> Result<Option<GenValue<'ctx>>, CodegenError> {
        let trait_def = self
            .hir_traits
            .iter()
            .find(|t| t.name == *trait_name)
            .ok_or_else(|| self.internal_err(span, &format!("trait `{trait_name}` not found")))?;
        let method_idx = trait_def
            .methods
            .iter()
            .position(|m| m.name == method)
            .ok_or_else(|| {
                self.internal_err(
                    span,
                    &format!("trait `{trait_name}` has no method `{method}`"),
                )
            })?;
        let i8_ptr = self.context.ptr_type(AddressSpace::from(0u16));
        let dyn_llvm = self.t(&Ty::Dyn {
            trait_name: trait_name.to_string(),
        }, span)?;

        // Load the fat pointer, then its `data` (field 0) and `vtable` (field 1).
        let fat = self.gen_value(recv)?.scalar(span, "dyn receiver")?;
        let fat_slot = bld(self.builder.build_alloca(dyn_llvm, "dyn_recv"))?;
        bld(self.builder.build_store(fat_slot, fat))?;
        let data_slot = bld(unsafe {
            self.builder.build_in_bounds_gep(
                dyn_llvm,
                fat_slot,
                &[self.i32_ty.const_zero(), self.i32_ty.const_zero()],
                "dyn_recv.data",
            )
        })?;
        let vtable_slot = bld(unsafe {
            self.builder.build_in_bounds_gep(
                dyn_llvm,
                fat_slot,
                &[self.i32_ty.const_zero(), self.i32_ty.const_int(1, false)],
                "dyn_recv.vtable",
            )
        })?;
        let data = bld(self.builder.build_load(i8_ptr, data_slot, "dyn_data"))?
            .into_pointer_value();
        let vtable = bld(self.builder.build_load(i8_ptr, vtable_slot, "dyn_vt"))?
            .into_pointer_value();

        // Load the thunk pointer at `vtable[method_idx]`.
        let idx = self.i32_ty.const_int(method_idx as u64, false);
        let entry = bld(unsafe {
            self.builder.build_in_bounds_gep(
                i8_ptr,
                vtable,
                &[idx],
                "dyn_vt.entry",
            )
        })?;
        let thunk_ptr = bld(self.builder.build_load(i8_ptr, entry, "dyn_thunk"))?;

        // The thunk signature: `(i8* data, param1, ..., paramN) -> ret`, where the
        // params come from the trait method signature (receiver is the data pointer).
        let m = &trait_def.methods[method_idx];
        let mut thunk_params: Vec<BasicMetadataTypeEnum> = vec![i8_ptr.into()];
        for (_, pty, pspan) in m.params.iter().skip(1) {
            let concrete = self.resolve_ty(pty);
            thunk_params.push(self.t(&concrete, *pspan)?.into());
        }
        let ret_concrete = m.ret.as_ref().map(|t| self.resolve_ty(t));
        let fn_ty = match &ret_concrete {
            Some(r) => self.t(r, span)?.fn_type(&thunk_params, false),
            None => self.context.void_type().fn_type(&thunk_params, false),
        };
        let thunk_callee = bld(self.builder.build_pointer_cast(
            thunk_ptr.into_pointer_value(),
            self.context.ptr_type(AddressSpace::from(0u16)),
            "dyn_callee",
        ))?;

        // Build call args: `data` first, then the extra arguments.
        let mut call_args: Vec<BasicMetadataValueEnum> = vec![data.into()];
        for (i, arg) in args.iter().enumerate() {
            let pt: BasicTypeEnum = thunk_params[i + 1]
                .try_into()
                .map_err(|_| self.internal_err(span, "dyn method argument type mismatch"))?;
            let v = self.gen_value(arg)?;
            call_args.push(self.call_arg(v, &pt, span, "dyn method argument")?.into());
        }
        let out = bld(self.builder.build_indirect_call(fn_ty, thunk_callee, &call_args, "dyncall"))?;
        match out.try_as_basic_value().basic() {
            Some(v) => {
                let is_agg = matches!(
                    v.get_type(),
                    BasicTypeEnum::ArrayType(_) | BasicTypeEnum::StructType(_)
                );
                if is_agg {
                    let tmp = bld(self.builder.build_alloca(v.get_type(), "dyn_ret"))?;
                    bld(self.builder.build_store(tmp, v))?;
                    Ok(Some(GenValue::Agg(tmp)))
                } else {
                    Ok(Some(GenValue::Scalar(v)))
                }
            }
            None => Ok(None),
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
                    Ty::Vec(elem) => {
                        // v[i]: GEP into the heap buffer at `data + idx * elem_size`
                        let elem_ty = self.t(elem, span)?;
                        let vec_llvm = self.t(&ty, span)?;
                        let idx = self.gen_value(index)?.scalar(span, "index")?;
                        let idx = self.coerce(idx, &self.i64_ty.into(), span, "index")?;
                        let data_slot = bld(unsafe {
                            self.builder.build_in_bounds_gep(
                                vec_llvm,
                                ptr,
                                &[self.i32_ty.const_zero(), self.i32_ty.const_zero()],
                                "vec.data",
                            )
                        })?;
                        let data = bld(self.builder.build_load(
                            self.context.ptr_type(AddressSpace::from(0u16)),
                            data_slot,
                            "vdata",
                        ))?
                        .into_pointer_value();
                        let data_elems = bld(self.builder.build_pointer_cast(
                            data,
                            elem_ty.ptr_type(AddressSpace::from(0u16)),
                            "vec_data_elems",
                        ))?;
                        let slot = bld(unsafe {
                            self.builder.build_in_bounds_gep(
                                elem_ty,
                                data_elems,
                                &[idx.into_int_value()],
                                "vload",
                            )
                        })?;
                        let v = bld(self.builder.build_load(elem_ty, slot, "vindex"))?;
                        Ok(GenValue::Scalar(v))
                    }
                    Ty::String => {
                        // s[i]: load the byte at `data + idx`, sign-extend to i64
                        let i8t = self.context.i8_type();
                        let str_llvm = self.t(&ty, span)?;
                        let idx = self.gen_value(index)?.scalar(span, "index")?;
                        let idx = self.coerce(idx, &self.i64_ty.into(), span, "index")?;
                        let data_slot = bld(unsafe {
                            self.builder.build_in_bounds_gep(
                                str_llvm,
                                ptr,
                                &[self.i32_ty.const_zero(), self.i32_ty.const_zero()],
                                "str.data",
                            )
                        })?;
                        let data = bld(self.builder.build_load(
                            self.context.ptr_type(AddressSpace::from(0u16)),
                            data_slot,
                            "sdata",
                        ))?
                        .into_pointer_value();
                        let slot = bld(unsafe {
                            self.builder.build_in_bounds_gep(i8t, data, &[idx.into_int_value()], "sload")
                        })?;
                        let v = bld(self.builder.build_load(i8t, slot, "sindex"))?;
                        let v64 = bld(self.builder.build_int_s_extend(
                            v.into_int_value(),
                            self.i64_ty,
                            "sindex_sext",
                        ))?;
                        Ok(GenValue::Scalar(v64.into()))
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
            Ty::F64 => {
                let f64t = self.t(&Ty::F64, span)?;
                bld(self.builder.build_store(ptr, f64t.const_zero()))?;
                Ok(())
            }
            Ty::F32 => {
                let f32t = self.t(&Ty::F32, span)?;
                bld(self.builder.build_store(ptr, f32t.const_zero()))?;
                Ok(())
            }
            other => Err(CodegenError {
                msg: format!("tensor element type `{other}` not supported (only i64/f32/f64)"),
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
        // Element type comes from matmul's inferred result type (unified from both
        // operands by infer); supports i64 / f32 / f64.
        let (elem, res_shape) = match self.expr_ty(expr)? {
            Ty::Tensor { elem, shape } => ((*elem).clone(), shape),
            other => {
                return Err(self.internal_err(span, &format!("invalid matmul result type `{other}`")));
            }
        };
        let is_float = matches!(elem, Ty::F32 | Ty::F64);
        let elem_llvm = self.t(&elem, span)?;
        let res_ty = Ty::Tensor {
            elem: Box::new(elem),
            shape: res_shape,
        };
        let res_llvm = self.t(&res_ty, span)?;
        let res = bld(self.builder.build_alloca(res_llvm, "matmul"))?;

        // Result [M x [N x elem]]
        let i_slot = bld(self.builder.build_alloca(self.i64_ty, "mm_i"))?;
        let j_slot = bld(self.builder.build_alloca(self.i64_ty, "mm_j"))?;
        let k_slot = bld(self.builder.build_alloca(self.i64_ty, "mm_k"))?;
        let sum_slot = bld(self.builder.build_alloca(elem_llvm, "mm_sum"))?;
        let zero = self.i64_ty.const_zero();
        let one = self.i64_ty.const_int(1, false);
        let elem_zero = elem_llvm.const_zero();
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
        bld(self.builder.build_store(sum_slot, elem_zero))?;
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
        let av = bld(self.builder.build_load(elem_llvm, a_elem, "mm_al"))?;
        let b_elem = bld(unsafe {
            self.builder.build_in_bounds_gep(
                rhs_ty,
                rhs_ptr,
                &[self.i32_ty.const_zero(), kv2, jv2],
                "mm_b",
            )
        })?;
        let bv = bld(self.builder.build_load(elem_llvm, b_elem, "mm_bl"))?;
        let sumv = bld(self.builder.build_load(elem_llvm, sum_slot, "mm_sl"))?;
        let (_, new_sum): (BasicValueEnum<'ctx>, BasicValueEnum<'ctx>) = if is_float {
            let af = av.into_float_value();
            let bf = bv.into_float_value();
            let sf = sumv.into_float_value();
            let p = bld(self.builder.build_float_mul(af, bf, "mm_fmul"))?;
            let s = bld(self.builder.build_float_add(sf, p, "mm_fadd"))?;
            (p.into(), s.into())
        } else {
            let ai = av.into_int_value();
            let bi = bv.into_int_value();
            let si = sumv.into_int_value();
            let p = bld(self.builder.build_int_mul(ai, bi, "mm_mul"))?;
            let s = bld(self.builder.build_int_add(si, p, "mm_add"))?;
            (p.into(), s.into())
        };
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
        let sumv2 = bld(self.builder.build_load(elem_llvm, sum_slot, "mm_sl2"))?;
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
    /// Tensor reduction builtin `sum(t)` / `mean(t)` / `max(t)` / `min(t)`
    /// (Aero-Tensor IR, CPU backend): reduces a tensor over all elements to a
    /// single scalar. Iterates a flat index and decomposes it into per-dimension
    /// indices, so any rank is supported.
    fn gen_reduce(&mut self, expr: &HirExpr) -> Result<GenValue<'ctx>, CodegenError> {
        let (op, input, span) = match expr {
            HirExpr::Reduce { op, input, span } => (*op, input, *span),
            _ => return Err(self.internal_err(expr.span(), "invalid reduce node type")),
        };
        let input_ty = self.expr_ty(input)?;
        let (elem, shape) = match &input_ty {
            Ty::Tensor { elem, shape } if !shape.is_empty() => ((**elem).clone(), shape.clone()),
            other => {
                return Err(self.internal_err(span, &format!("invalid reduce input type `{other}`")));
            }
        };
        let total: u64 = shape.iter().fold(1u64, |acc, d| acc * (*d as u64));
        let elem_llvm = self.t(&elem, span)?;
        let input_ptr = self.gen_value(input)?.agg(span, "reduce input")?;
        let input_llvm = self.t(&input_ty, span)?;
        let is_float = matches!(elem, Ty::F32 | Ty::F64);

        let acc_slot = bld(self.builder.build_alloca(elem_llvm, "reduce_acc"))?;
        let f_slot = bld(self.builder.build_alloca(self.i64_ty, "reduce_f"))?;
        let zero = self.i64_ty.const_zero();
        let one = self.i64_ty.const_int(1, false);
        let total_c = self.i64_ty.const_int(total, false);

        // Initialize the accumulator: 0 for sum/mean; an extreme sentinel for
        // max/min so negative/positive values are handled correctly.
        let init: BasicValueEnum = match (op, is_float) {
            (ReduceOp::Sum | ReduceOp::Mean, _) => elem_llvm.const_zero().into(),
            (ReduceOp::Max, true) => elem_llvm.into_float_type().const_float(f64::NEG_INFINITY).into(),
            (ReduceOp::Max, false) => self.i64_ty.const_int(i64::MIN as u64, false).into(),
            (ReduceOp::Min, true) => elem_llvm.into_float_type().const_float(f64::INFINITY).into(),
            (ReduceOp::Min, false) => self.i64_ty.const_int(i64::MAX as u64, false).into(),
        };
        bld(self.builder.build_store(acc_slot, init))?;
        bld(self.builder.build_store(f_slot, zero))?;

        let cond = self.context.append_basic_block(self.cur_func, "reduce_cond");
        let body = self.context.append_basic_block(self.cur_func, "reduce_body");
        let end = self.context.append_basic_block(self.cur_func, "reduce_end");
        bld(self.builder.build_unconditional_branch(cond))?;

        self.builder.position_at_end(cond);
        let fv = bld(self.builder.build_load(self.i64_ty, f_slot, "reduce_fl"))?
            .into_int_value();
        let in_loop = bld(self.builder.build_int_compare(IntPredicate::SLT, fv, total_c, "reduce_ilt"))?;
        bld(self.builder.build_conditional_branch(in_loop, body, end))?;

        self.builder.position_at_end(body);
        // Decompose the flat index f into per-dimension indices (row-major).
        let mut idx: Vec<IntValue<'ctx>> = Vec::with_capacity(shape.len());
        let mut cur_f = fv;
        for d in shape.iter().rev() {
            let d_c = self.i64_ty.const_int(*d as u64, false);
            let di = bld(self.builder.build_int_signed_rem(cur_f, d_c, "rd_idx"))?;
            idx.push(di);
            cur_f = bld(self.builder.build_int_signed_div(cur_f, d_c, "rd_carry"))?;
        }
        idx.reverse();
        let mut gep_idx: Vec<IntValue<'ctx>> = vec![self.i32_ty.const_zero()];
        gep_idx.extend(idx);
        let elem_ptr = bld(unsafe {
            self.builder.build_in_bounds_gep(input_llvm, input_ptr, &gep_idx, "reduce_elem")
        })?;
        let val = bld(self.builder.build_load(elem_llvm, elem_ptr, "reduce_val"))?;
        let acc = bld(self.builder.build_load(elem_llvm, acc_slot, "reduce_acc_l"))?;
        let new_acc: BasicValueEnum = self.combine_reduce(op, is_float, acc, val)?;
        bld(self.builder.build_store(acc_slot, new_acc))?;
        let nf = bld(self.builder.build_int_add(fv, one, "reduce_finc"))?;
        bld(self.builder.build_store(f_slot, nf))?;
        bld(self.builder.build_unconditional_branch(cond))?;

        self.builder.position_at_end(end);
        // mean: divide the accumulated sum by the element count.
        if op == ReduceOp::Mean {
            let acc = bld(self.builder.build_load(elem_llvm, acc_slot, "reduce_acc_m"))?;
            let mean_val: BasicValueEnum = if is_float {
                let af = acc.into_float_value();
                let count_f = if elem == Ty::F32 {
                    self.context.f32_type().const_float(total as f64)
                } else {
                    self.context.f64_type().const_float(total as f64)
                };
                bld(self.builder.build_float_div(af, count_f, "reduce_div"))?.into()
            } else {
                let ai = acc.into_int_value();
                bld(self.builder.build_int_signed_div(ai, total_c, "reduce_idiv"))?.into()
            };
            bld(self.builder.build_store(acc_slot, mean_val))?;
        }
        let res = bld(self.builder.build_load(elem_llvm, acc_slot, "reduce_res"))?;
        Ok(GenValue::Scalar(res))
    }

    /// Combine an accumulator with one element for a reduction op.
    fn combine_reduce(
        &mut self,
        op: ReduceOp,
        is_float: bool,
        acc: BasicValueEnum<'ctx>,
        val: BasicValueEnum<'ctx>,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        match op {
            ReduceOp::Sum | ReduceOp::Mean => {
                if is_float {
                    let a = acc.into_float_value();
                    let v = val.into_float_value();
                    Ok(bld(self.builder.build_float_add(a, v, "reduce_add"))?.into())
                } else {
                    let a = acc.into_int_value();
                    let v = val.into_int_value();
                    Ok(bld(self.builder.build_int_add(a, v, "reduce_add"))?.into())
                }
            }
            ReduceOp::Max => {
                let gt = if is_float {
                    let a = acc.into_float_value();
                    let v = val.into_float_value();
                    bld(self.builder.build_float_compare(FloatPredicate::OGT, a, v, "reduce_gt"))?
                } else {
                    let a = acc.into_int_value();
                    let v = val.into_int_value();
                    bld(self.builder.build_int_compare(IntPredicate::SGT, a, v, "reduce_gt"))?
                };
                Ok(bld(self.builder.build_select(gt, acc, val, "reduce_max"))?.into())
            }
            ReduceOp::Min => {
                let lt = if is_float {
                    let a = acc.into_float_value();
                    let v = val.into_float_value();
                    bld(self.builder.build_float_compare(FloatPredicate::OLT, a, v, "reduce_lt"))?
                } else {
                    let a = acc.into_int_value();
                    let v = val.into_int_value();
                    bld(self.builder.build_int_compare(IntPredicate::SLT, a, v, "reduce_lt"))?
                };
                Ok(bld(self.builder.build_select(lt, acc, val, "reduce_min"))?.into())
            }
        }
    }

    /// Element-wise tensor operation `tensor_add/sub/mul/div(a, b)` and
    /// `tensor_neg(a)` (Aero-Tensor IR, CPU backend). Returns a tensor of the
    /// same shape and element type as the operands. Iterates a flat index like
    /// `gen_reduce`, so any rank is supported.
    fn gen_elemwise(&mut self, expr: &HirExpr) -> Result<GenValue<'ctx>, CodegenError> {
        let (op, lhs, rhs, span) = match expr {
            HirExpr::ElemWise { op, lhs, rhs, span } => (*op, lhs, rhs, *span),
            _ => return Err(self.internal_err(expr.span(), "invalid elemwise node type")),
        };
        let lhs_ty = self.expr_ty(lhs)?;
        let (elem, shape) = match &lhs_ty {
            Ty::Tensor { elem, shape } if !shape.is_empty() => ((**elem).clone(), shape.clone()),
            other => {
                return Err(self.internal_err(span, &format!("invalid element-wise operand type `{other}`")));
            }
        };
        let total: u64 = shape.iter().fold(1u64, |acc, d| acc * (*d as u64));
        let elem_llvm = self.t(&elem, span)?;
        let lhs_ptr = self.gen_value(lhs)?.agg(span, "elemwise lhs")?;
        let lhs_llvm = self.t(&lhs_ty, span)?;
        let is_float = matches!(elem, Ty::F32 | Ty::F64);

        // Right operand (present for binary ops).
        let rhs_ptr: Option<PointerValue<'ctx>> = match rhs {
            Some(r) => Some(self.gen_value(r)?.agg(span, "elemwise rhs")?),
            None => None,
        };

        let res_llvm = self.t(&lhs_ty, span)?;
        let res = bld(self.builder.build_alloca(res_llvm, "elemwise"))?;
        let f_slot = bld(self.builder.build_alloca(self.i64_ty, "ew_f"))?;
        let zero = self.i64_ty.const_zero();
        let one = self.i64_ty.const_int(1, false);
        let total_c = self.i64_ty.const_int(total, false);
        bld(self.builder.build_store(f_slot, zero))?;

        let cond = self.context.append_basic_block(self.cur_func, "ew_cond");
        let body = self.context.append_basic_block(self.cur_func, "ew_body");
        let end = self.context.append_basic_block(self.cur_func, "ew_end");
        bld(self.builder.build_unconditional_branch(cond))?;

        self.builder.position_at_end(cond);
        let fv = bld(self.builder.build_load(self.i64_ty, f_slot, "ew_fl"))?.into_int_value();
        let in_loop = bld(self.builder.build_int_compare(IntPredicate::SLT, fv, total_c, "ew_ilt"))?;
        bld(self.builder.build_conditional_branch(in_loop, body, end))?;

        self.builder.position_at_end(body);
        // Decompose the flat index into per-dimension indices (row-major).
        let mut idx: Vec<IntValue<'ctx>> = Vec::with_capacity(shape.len());
        let mut cur_f = fv;
        for d in shape.iter().rev() {
            let d_c = self.i64_ty.const_int(*d as u64, false);
            let di = bld(self.builder.build_int_signed_rem(cur_f, d_c, "ew_idx"))?;
            idx.push(di);
            cur_f = bld(self.builder.build_int_signed_div(cur_f, d_c, "ew_carry"))?;
        }
        idx.reverse();
        let mut gep_idx: Vec<IntValue<'ctx>> = vec![self.i32_ty.const_zero()];
        gep_idx.extend(idx);
        let a_elem = bld(unsafe {
            self.builder.build_in_bounds_gep(lhs_llvm, lhs_ptr, &gep_idx, "ew_a")
        })?;
        let av = bld(self.builder.build_load(elem_llvm, a_elem, "ew_av"))?;
        let bv: Option<BasicValueEnum<'ctx>> = match rhs_ptr {
            Some(rp) => {
                let b_elem = bld(unsafe {
                    self.builder.build_in_bounds_gep(lhs_llvm, rp, &gep_idx, "ew_b")
                })?;
                Some(bld(self.builder.build_load(elem_llvm, b_elem, "ew_bv"))?)
            }
            None => None,
        };
        let res_v = self.combine_elem(op, is_float, av, bv, span)?;
        let r_elem = bld(unsafe {
            self.builder.build_in_bounds_gep(res_llvm, res, &gep_idx, "ew_res")
        })?;
        bld(self.builder.build_store(r_elem, res_v))?;
        let nf = bld(self.builder.build_int_add(fv, one, "ew_finc"))?;
        bld(self.builder.build_store(f_slot, nf))?;
        bld(self.builder.build_unconditional_branch(cond))?;

        self.builder.position_at_end(end);
        Ok(GenValue::Agg(res))
    }

    /// Apply one element-wise tensor op to a loaded element (and, for binary
    /// ops, a second loaded element), returning the element result value.
    fn combine_elem(
        &mut self,
        op: ElemOp,
        is_float: bool,
        a: BasicValueEnum<'ctx>,
        b: Option<BasicValueEnum<'ctx>>,
        span: Span,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        if op == ElemOp::Neg {
            return if is_float {
                let af = a.into_float_value();
                Ok(bld(self.builder.build_float_neg(af, "ew_neg"))?.into())
            } else {
                let ai = a.into_int_value();
                Ok(bld(self.builder.build_int_neg(ai, "ew_neg"))?.into())
            };
        }
        let b = b.ok_or_else(|| {
            CodegenError {
                msg: format!("binary element-wise op `{op:?}` requires two operands"),
                line: span.line,
                col: span.col,
            }
        })?;
        let r: BasicValueEnum<'ctx> = if is_float {
            let af = a.into_float_value();
            let bf = b.into_float_value();
            match op {
                ElemOp::Add => bld(self.builder.build_float_add(af, bf, "ew_add"))?.into(),
                ElemOp::Sub => bld(self.builder.build_float_sub(af, bf, "ew_sub"))?.into(),
                ElemOp::Mul => bld(self.builder.build_float_mul(af, bf, "ew_mul"))?.into(),
                ElemOp::Div => bld(self.builder.build_float_div(af, bf, "ew_div"))?.into(),
                ElemOp::Neg => unreachable!(),
            }
        } else {
            let ai = a.into_int_value();
            let bi = b.into_int_value();
            match op {
                ElemOp::Add => bld(self.builder.build_int_add(ai, bi, "ew_add"))?.into(),
                ElemOp::Sub => bld(self.builder.build_int_sub(ai, bi, "ew_sub"))?.into(),
                ElemOp::Mul => bld(self.builder.build_int_mul(ai, bi, "ew_mul"))?.into(),
                ElemOp::Div => bld(self.builder.build_int_signed_div(ai, bi, "ew_div"))?.into(),
                ElemOp::Neg => unreachable!(),
            }
        };
        Ok(r)
    }

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

    /// BLAS Level-1 tensor operations (BLAS binding, CPU backend): `blas_dot`,
    /// `blas_nrm2`, `blas_asum`, `blas_amax`, `blas_scal`, `blas_axpy`. Uses the
    /// same general flat-index loop as `gen_reduce`/`gen_elemwise`, so any rank
    /// / shape is supported with no external BLAS dependency.
    #[allow(clippy::too_many_arguments)]
    fn gen_blas(&mut self, expr: &HirExpr) -> Result<GenValue<'ctx>, CodegenError> {
        let (op, args, span) = match expr {
            HirExpr::Blas { op, args, span } => (*op, args, *span),
            _ => return Err(self.internal_err(expr.span(), "invalid blas node type")),
        };
        // First argument that is a tensor; the scala's `alpha` precedes it.
        let first_tensor = match op {
            BlasOp::Dot | BlasOp::Nrm2 | BlasOp::Asum | BlasOp::Amax => 0usize,
            BlasOp::Scal | BlasOp::Axpy => 1usize,
        };
        let tensor_expr = &args[first_tensor];
        let tensor_ty = self.expr_ty(tensor_expr)?;
        let (elem, shape) = match &tensor_ty {
            Ty::Tensor { elem, shape } if !shape.is_empty() => ((**elem).clone(), shape.clone()),
            other => {
                return Err(self.internal_err(span, &format!("invalid blas tensor type `{other}`")));
            }
        };
        let total: u64 = shape.iter().fold(1u64, |acc, d| acc * (*d as u64));
        let elem_llvm = self.t(&elem, span)?;
        let is_float = matches!(elem, Ty::F32 | Ty::F64);
        let tensor_ptr = self.gen_value(tensor_expr)?.agg(span, "blas tensor")?;
        let tensor_llvm = self.t(&tensor_ty, span)?;

        // Binary dot: resolve the second tensor operand (`args[1]`).
        let (b_ptr, b_llvm): (Option<PointerValue<'ctx>>, Option<BasicTypeEnum<'ctx>>) =
            match op {
                BlasOp::Dot => {
                    let bt = self.expr_ty(&args[1])?;
                    let b = self.gen_value(&args[1])?.agg(span, "blas rhs")?;
                    (Some(b), Some(self.t(&bt, span)?))
                }
                _ => (None, None),
            };
        // Scalar `alpha` operand for scal / axpy (`args[0]`).
        let alpha: Option<BasicValueEnum<'ctx>> = match op {
            BlasOp::Scal | BlasOp::Axpy => Some(self.gen_value(&args[0])?.scalar(span, "blas alpha")?),
            _ => None,
        };

        let zero = self.i64_ty.const_zero();
        let one = self.i64_ty.const_int(1, false);
        let total_c = self.i64_ty.const_int(total, false);

        // Scalar-accumulator ops (dot / nrm2 / asum / amax): loop and reduce.
        if matches!(op, BlasOp::Dot | BlasOp::Nrm2 | BlasOp::Asum | BlasOp::Amax) {
            // amax accumulates index + current max |value|; others accumulate a scalar.
            let acc_slot = bld(self.builder.build_alloca(elem_llvm, "bl_acc"))?;
            let f_slot = bld(self.builder.build_alloca(self.i64_ty, "bl_f"))?;
            let init: BasicValueEnum = if is_float {
                elem_llvm.into_float_type().const_float(0.0).into()
            } else {
                elem_llvm.const_zero().into()
            };
            bld(self.builder.build_store(acc_slot, init))?;
            // amax tracks the best index too.
            let amax_idx_slot = if op == BlasOp::Amax {
                Some(bld(self.builder.build_alloca(self.i64_ty, "bl_best"))?)
            } else {
                None
            };
            if let Some(s) = amax_idx_slot {
                bld(self.builder.build_store(s, zero))?;
            }
            bld(self.builder.build_store(f_slot, zero))?;

            let cond = self.context.append_basic_block(self.cur_func, "bl_cond");
            let body = self.context.append_basic_block(self.cur_func, "bl_body");
            let end = self.context.append_basic_block(self.cur_func, "bl_end");
            bld(self.builder.build_unconditional_branch(cond))?;

            self.builder.position_at_end(cond);
            let fv = bld(self.builder.build_load(self.i64_ty, f_slot, "bl_fl"))?.into_int_value();
            let in_loop =
                bld(self.builder.build_int_compare(IntPredicate::SLT, fv, total_c, "bl_ilt"))?;
            bld(self.builder.build_conditional_branch(in_loop, body, end))?;

            self.builder.position_at_end(body);
            let mut idx: Vec<IntValue<'ctx>> = Vec::with_capacity(shape.len());
            let mut cur_f = fv;
            for d in shape.iter().rev() {
                let d_c = self.i64_ty.const_int(*d as u64, false);
                let di = bld(self.builder.build_int_signed_rem(cur_f, d_c, "bl_idx"))?;
                idx.push(di);
                cur_f = bld(self.builder.build_int_signed_div(cur_f, d_c, "bl_carry"))?;
            }
            idx.reverse();
            let mut gep_idx: Vec<IntValue<'ctx>> = vec![self.i32_ty.const_zero()];
            gep_idx.extend(idx);
            let elem_ptr = bld(unsafe {
                self.builder.build_in_bounds_gep(tensor_llvm, tensor_ptr, &gep_idx, "bl_elem")
            })?;
            let val = bld(self.builder.build_load(elem_llvm, elem_ptr, "bl_val"))?;
            let bv: Option<BasicValueEnum<'ctx>> = match b_ptr {
                Some(bp) => {
                    let be = bld(unsafe {
                        self.builder.build_in_bounds_gep(b_llvm.unwrap(), bp, &gep_idx, "bl_be")
                    })?;
                    Some(bld(self.builder.build_load(elem_llvm, be, "bl_bv"))?)
                }
                None => None,
            };
            let acc = bld(self.builder.build_load(elem_llvm, acc_slot, "bl_acc_l"))?;
            // Absolute value of the current element (float: select(x<0, -x, x);
            // int: -x when x<0). `absv==None` for dot (no abs) and nrm2 (uses x*x).
            let absv: Option<BasicValueEnum<'ctx>> = if op == BlasOp::Asum {
                let a: BasicValueEnum = if is_float {
                    let af = val.into_float_value();
                    let neg = bld(self.builder.build_float_neg(af, "bl_aneg"))?;
                    let lt = bld(self.builder.build_float_compare(
                        FloatPredicate::OLT,
                        af,
                        elem_llvm.into_float_type().const_float(0.0),
                        "bl_alt",
                    ))?;
                    bld(self.builder.build_select(lt, neg, af, "bl_aabs"))?
                } else {
                    let ai = val.into_int_value();
                    let neg = bld(self.builder.build_int_neg(ai, "bl_aneg"))?;
                    let lt = bld(self.builder.build_int_compare(
                        IntPredicate::SLT,
                        ai,
                        zero,
                        "bl_alt",
                    ))?;
                    bld(self.builder.build_select(lt, neg, ai, "bl_aabs"))?
                };
                Some(a)
            } else {
                None
            };
            let new_acc: BasicValueEnum = match op {
                BlasOp::Dot => {
                    // acc += a * b
                    let prod: BasicValueEnum = if is_float {
                        bld(self.builder.build_float_mul(
                            val.into_float_value(),
                            bv.unwrap().into_float_value(),
                            "bl_dotp",
                        ))?
                        .into()
                    } else {
                        bld(self.builder.build_int_mul(
                            val.into_int_value(),
                            bv.unwrap().into_int_value(),
                            "bl_dotp",
                        ))?
                        .into()
                    };
                    self.blas_addv(prod, acc, is_float, "bl_dotacc")?
                }
                BlasOp::Asum => self.blas_addv(absv.unwrap(), acc, is_float, "bl_sumacc")?,
                BlasOp::Nrm2 => {
                    // acc += x*x
                    let sq: BasicValueEnum = if is_float {
                        bld(self.builder.build_float_mul(
                            val.into_float_value(),
                            val.into_float_value(),
                            "bl_sq",
                        ))?
                        .into()
                    } else {
                        bld(self.builder.build_int_mul(
                            val.into_int_value(),
                            val.into_int_value(),
                            "bl_sq",
                        ))?
                        .into()
                    };
                    self.blas_addv(sq, acc, is_float, "bl_sqacc")?
                }
                BlasOp::Amax => {
                    // abs of current element (float: select(x<0, -x, x); int: -x if x<0).
                    let absv_int: BasicValueEnum<'ctx> = if is_float {
                        let af = val.into_float_value();
                        let neg = bld(self.builder.build_float_neg(af, "bl_aneg"))?;
                        let lt = bld(self.builder.build_float_compare(
                            FloatPredicate::OLT,
                            af,
                            elem_llvm.into_float_type().const_float(0.0),
                            "bl_alt",
                        ))?;
                        bld(self.builder.build_select(lt, neg, af, "bl_aabs"))?
                    } else {
                        let ai = val.into_int_value();
                        let neg = bld(self.builder.build_int_neg(ai, "bl_aneg"))?;
                        let lt = bld(self.builder.build_int_compare(
                            IntPredicate::SLT,
                            ai,
                            zero,
                            "bl_alt",
                        ))?;
                        bld(self.builder.build_select(lt, neg, ai, "bl_aabs"))?
                    };
                    let bi = bld(self.builder.build_load(self.i64_ty, amax_idx_slot.unwrap(), "bl_bestl"))?
                        .into_int_value();
                    let gt: IntValue<'ctx> = if is_float {
                        bld(self.builder.build_float_compare(
                            FloatPredicate::OGT,
                            absv_int.into_float_value(),
                            acc.into_float_value(),
                            "bl_gt",
                        ))?
                    } else {
                        bld(self.builder.build_int_compare(
                            IntPredicate::SGT,
                            absv_int.into_int_value(),
                            acc.into_int_value(),
                            "bl_gt",
                        ))?
                    };
                    let newbest = bld(self.builder.build_select(gt, absv_int, acc, "bl_nb"))?;
                    bld(self.builder.build_store(acc_slot, newbest))?;
                    let newidx = bld(self.builder.build_select(gt, fv, bi, "bl_ni"))?;
                    bld(self.builder.build_store(amax_idx_slot.unwrap(), newidx))?;
                    newbest
                }
                BlasOp::Scal | BlasOp::Axpy => unreachable!("handled by tensor path"),
            };
            bld(self.builder.build_store(acc_slot, new_acc))?;
            let nf = bld(self.builder.build_int_add(fv, one, "bl_inc"))?;
            bld(self.builder.build_store(f_slot, nf))?;
            bld(self.builder.build_unconditional_branch(cond))?;

            self.builder.position_at_end(end);
            // nrm2: sqrt the accumulated sum of squares.
            if op == BlasOp::Nrm2 {
                let acc = bld(self.builder.build_load(elem_llvm, acc_slot, "bl_nrmacc"))?;
                let call = bld(self.builder.build_call(
                    self.llvm_sqrt(elem_llvm, matches!(elem, Ty::F64), span)?,
                    &[acc.into()],
                    "bl_sqrt",
                ))?;
                let sq = call.try_as_basic_value().basic().unwrap();
                bld(self.builder.build_store(acc_slot, sq))?;
            }
            let res = bld(self.builder.build_load(elem_llvm, acc_slot, "bl_res"))?;
            // amax returns the best index (i64); dot/nrm2/asum return the scalar.
            if op == BlasOp::Amax {
                let i = bld(self.builder.build_load(self.i64_ty, amax_idx_slot.unwrap(), "bl_resi"))?;
                return Ok(GenValue::Scalar(i));
            }
            return Ok(GenValue::Scalar(res));
        }

        // Tensor-returning ops (scal / axpy): blas_scal(alpha, x),
        // blas_axpy(alpha, x, y) → same-shape tensor.
        let res_llvm = self.t(&tensor_ty, span)?;
        let res = bld(self.builder.build_alloca(res_llvm, "blas"))?;
        let f_slot = bld(self.builder.build_alloca(self.i64_ty, "bl_tf"))?;
        bld(self.builder.build_store(f_slot, zero))?;
        let y_ptr: Option<PointerValue<'ctx>> = match op {
            BlasOp::Axpy => {
                Some(self.gen_value(&args[2])?.agg(span, "blas y")?)
            }
            _ => None,
        };
        let y_llvm = y_ptr.map(|_| tensor_llvm);
        let alpha_val = alpha.unwrap();

        let cond = self.context.append_basic_block(self.cur_func, "blt_cond");
        let body = self.context.append_basic_block(self.cur_func, "blt_body");
        let end = self.context.append_basic_block(self.cur_func, "blt_end");
        bld(self.builder.build_unconditional_branch(cond))?;

        self.builder.position_at_end(cond);
        let fv = bld(self.builder.build_load(self.i64_ty, f_slot, "blt_fl"))?.into_int_value();
        let in_loop =
            bld(self.builder.build_int_compare(IntPredicate::SLT, fv, total_c, "blt_ilt"))?;
        bld(self.builder.build_conditional_branch(in_loop, body, end))?;

        self.builder.position_at_end(body);
        let mut idx: Vec<IntValue<'ctx>> = Vec::with_capacity(shape.len());
        let mut cur_f = fv;
        for d in shape.iter().rev() {
            let d_c = self.i64_ty.const_int(*d as u64, false);
            let di = bld(self.builder.build_int_signed_rem(cur_f, d_c, "blt_idx"))?;
            idx.push(di);
            cur_f = bld(self.builder.build_int_signed_div(cur_f, d_c, "blt_carry"))?;
        }
        idx.reverse();
        let mut gep_idx: Vec<IntValue<'ctx>> = vec![self.i32_ty.const_zero()];
        gep_idx.extend(idx);
        let xp = bld(unsafe {
            self.builder.build_in_bounds_gep(tensor_llvm, tensor_ptr, &gep_idx, "blt_x")
        })?;
        let xv = bld(self.builder.build_load(elem_llvm, xp, "blt_xv"))?;
        let alpha_x: BasicValueEnum = if is_float {
            bld(self.builder.build_float_mul(
                alpha_val.into_float_value(),
                xv.into_float_value(),
                "blt_ax",
            ))?
            .into()
        } else {
            bld(self.builder.build_int_mul(
                alpha_val.into_int_value(),
                xv.into_int_value(),
                "blt_ax",
            ))?
            .into()
        };
        let out_val: BasicValueEnum = match y_ptr {
            Some(yp) => {
                let ye = bld(unsafe {
                    self.builder.build_in_bounds_gep(y_llvm.unwrap(), yp, &gep_idx, "blt_y")
                })?;
                let yv = bld(self.builder.build_load(elem_llvm, ye, "blt_yv"))?;
                self.blas_addv(alpha_x, yv, is_float, "blt_axy")?
            }
            None => alpha_x,
        };
        let rp = bld(unsafe {
            self.builder.build_in_bounds_gep(res_llvm, res, &gep_idx, "blt_r")
        })?;
        bld(self.builder.build_store(rp, out_val))?;
        let nf = bld(self.builder.build_int_add(fv, one, "blt_inc"))?;
        bld(self.builder.build_store(f_slot, nf))?;
        bld(self.builder.build_unconditional_branch(cond))?;

        self.builder.position_at_end(end);
        Ok(GenValue::Agg(res))
    }

    /// Helper: emit `a + b` as a scalar, float or integer.
    fn blas_addv(
        &self,
        a: BasicValueEnum<'ctx>,
        b: BasicValueEnum<'ctx>,
        is_float: bool,
        name: &str,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        if is_float {
            Ok(bld(self.builder.build_float_add(
                a.into_float_value(),
                b.into_float_value(),
                name,
            ))?
            .into())
        } else {
            Ok(bld(self.builder.build_int_add(
                a.into_int_value(),
                b.into_int_value(),
                name,
            ))?
            .into())
        }
    }

    /// libc `sqrt` (f64) / `sqrtf` (f32) for the BLAS nrm2 helper. Reuses a
    /// user-declared extern or declares the CRT export, matching the string-runtime
    /// libc helper pattern (no LLVM intrinsic dependency).
    fn llvm_sqrt(
        &self,
        elem_llvm: BasicTypeEnum<'ctx>,
        is_float64: bool,
        _span: Span,
    ) -> Result<FunctionValue<'ctx>, CodegenError> {
        let name = if is_float64 { "sqrt" } else { "sqrtf" };
        match self.module.get_function(name) {
            Some(f) => Ok(f),
            None => {
                let fn_ty = elem_llvm.fn_type(&[elem_llvm.into()], false);
                Ok(self.module.add_function(name, fn_ty, None))
            }
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
                    Ty::Vec(elem) => {
                        // v[i] = x: write into the heap buffer element slot
                        let elem_ty = self.t(elem, span)?;
                        let vec_llvm = self.t(&ty, span)?;
                        let idx = self.gen_value(index)?.scalar(span, "index write")?;
                        let idx = self.coerce(idx, &self.i64_ty.into(), span, "index write")?;
                        let data_slot = bld(unsafe {
                            self.builder.build_in_bounds_gep(
                                vec_llvm,
                                ptr,
                                &[self.i32_ty.const_zero(), self.i32_ty.const_zero()],
                                "vec.data",
                            )
                        })?;
                        let data = bld(self.builder.build_load(
                            self.context.ptr_type(AddressSpace::from(0u16)),
                            data_slot,
                            "vdata",
                        ))?
                        .into_pointer_value();
                        let data_elems = bld(self.builder.build_pointer_cast(
                            data,
                            elem_ty.ptr_type(AddressSpace::from(0u16)),
                            "vec_data_elems",
                        ))?;
                        let slot = bld(unsafe {
                            self.builder.build_in_bounds_gep(
                                elem_ty,
                                data_elems,
                                &[idx.into_int_value()],
                                "vidxw",
                            )
                        })?;
                        Ok((slot, elem_ty))
                    }
                    Ty::String => {
                        // s[i] = v: write a byte into the heap buffer slot
                        let i8t = self.context.i8_type();
                        let str_llvm = self.t(&ty, span)?;
                        let idx = self.gen_value(index)?.scalar(span, "index write")?;
                        let idx = self.coerce(idx, &self.i64_ty.into(), span, "index write")?;
                        let data_slot = bld(unsafe {
                            self.builder.build_in_bounds_gep(
                                str_llvm,
                                ptr,
                                &[self.i32_ty.const_zero(), self.i32_ty.const_zero()],
                                "str.data",
                            )
                        })?;
                        let data = bld(self.builder.build_load(
                            self.context.ptr_type(AddressSpace::from(0u16)),
                            data_slot,
                            "sdata",
                        ))?
                        .into_pointer_value();
                        let slot = bld(unsafe {
                            self.builder.build_in_bounds_gep(i8t, data, &[idx.into_int_value()], "sidxw")
                        })?;
                        Ok((slot, i8t.into()))
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

    /// Field access `recv.field`: GEP into the struct aggregate. Aggregate
    /// fields return their slot pointer (`Agg`); scalar fields are loaded.
    fn gen_field(
        &mut self,
        target: &HirExpr,
        field: &str,
        span: Span,
    ) -> Result<GenValue<'ctx>, CodegenError> {
        let (ptr, fty_llvm, recv_ty) = self.gen_field_ptr(target, field, span)?;
        // The field's Aero type determines whether the value is an aggregate.
        let struct_name = match &recv_ty {
            Ty::Struct(n) => n.clone(),
            Ty::StructGeneric { name, .. } => name.clone(),
            Ty::Union(n) => n.clone(),
            other => {
                return Err(CodegenError {
                    msg: format!("cannot access field `.{field}` on type `{other}`"),
                    line: span.line,
                    col: span.col,
                });
            }
        };
        let (fty, params) = if matches!(&recv_ty, Ty::Union(_)) {
            let ft = self
                .hir_unions
                .iter()
                .find(|u| u.name == struct_name)
                .and_then(|u| u.find_field(field).map(|(_, t)| t.clone()))
                .ok_or_else(|| self.internal_err(span, &format!("union `{struct_name}` has no field `{field}`")))?;
            (ft, Vec::new())
        } else {
            let def = self
                .hir_structs
                .iter()
                .find(|s| s.name == struct_name)
                .ok_or_else(|| self.internal_err(span, &format!("undefined struct `{struct_name}`")))?;
            let ft = def
                .find_field(field)
                .map(|(_, t)| t.clone())
                .ok_or_else(|| self.internal_err(span, &format!("struct `{struct_name}` has no field `{field}`")))?;
            (ft, def.type_params.clone())
        };
        let fty_subst = match &recv_ty {
            Ty::StructGeneric { name, args } => {
                let merged = instance_subst(&params, args, &self.type_subst);
                substitute(&fty, &merged)
            }
            _ => fty.clone(),
        };
        if is_agg(&fty_subst) {
            Ok(GenValue::Agg(ptr))
        } else {
            let v = bld(self.builder.build_load(fty_llvm, ptr, "fld_load"))?;
            Ok(GenValue::Scalar(v))
        }
    }

    /// Compute the stack slot pointer of a struct field (used by field access
    /// and field writes). Returns the field slot, its LLVM type, and the
    /// (auto-deref'd) receiver type. Auto-deref: `recv.field` where `recv` is
    /// `&T` / `&mut T` accesses `(*recv).field`.
    fn gen_field_ptr(
        &mut self,
        target: &HirExpr,
        field: &str,
        span: Span,
    ) -> Result<
        (
            PointerValue<'ctx>,
            BasicTypeEnum<'ctx>,
            Ty,
        ),
        CodegenError,
    > {
        let raw_recv_ty = self.expr_ty(target)?;
        // Auto-deref: unwrap references to reach the struct type
        let recv_ty = match &raw_recv_ty {
            Ty::Ref { inner, .. } => (**inner).clone(),
            other => other.clone(),
        };
        let is_union = matches!(&recv_ty, Ty::Union(_));
        let struct_name = match &recv_ty {
            Ty::Struct(name) => name.clone(),
            Ty::StructGeneric { name, .. } => name.clone(),
            Ty::Union(name) => name.clone(),
            other => {
                return Err(CodegenError {
                    msg: format!("cannot access field `.{field}` on type `{other}` (not a struct)"),
                    line: span.line,
                    col: span.col,
                });
            }
        };
        // Union fields live in the union definition table; struct/generic fields
        // in the struct table.
        let (fidx, fty, params) = if is_union {
            let udef = self
                .hir_unions
                .iter()
                .find(|u| u.name == struct_name)
                .ok_or_else(|| self.internal_err(span, &format!("undefined union `{struct_name}`")))?;
            let ft = udef
                .find_field(field)
                .map(|(_, t)| t.clone())
                .ok_or_else(|| self.internal_err(span, &format!("union `{struct_name}` has no field `{field}`")))?;
            // Every union field lives at byte offset 0.
            (0, ft, Vec::new())
        } else {
            let def = self
                .hir_structs
                .iter()
                .find(|s| s.name == struct_name)
                .ok_or_else(|| self.internal_err(span, &format!("undefined struct `{struct_name}`")))?;
            let (fi, ft) = def
                .find_field(field)
                .ok_or_else(|| self.internal_err(span, &format!("struct `{struct_name}` has no field `{field}`")))?;
            (fi, ft.clone(), def.type_params.clone())
        };
        // Substitute generic instance args into the field type if applicable.
        let fty_subst = match &recv_ty {
            Ty::StructGeneric { name, args } => {
                let merged = instance_subst(&params, args, &self.type_subst);
                substitute(&fty, &merged)
            }
            _ => fty.clone(),
        };
        let fty_llvm = self.t(&fty_subst, span)?;

        // The receiver must be addressable: a variable slot, or a temporary
        // aggregate slot for literals / nested expressions.
        let ptr = if matches!(&raw_recv_ty, Ty::Ref { .. }) {
            // Auto-deref: the receiver holds (or is) a pointer to the struct.
            match target {
                HirExpr::Var(def_id, _) => {
                    let slot = *self
                        .vars
                        .get(def_id)
                        .ok_or_else(|| self.internal_err(span, "field target not allocated"))?;
                    let ref_llvm = self.t(&raw_recv_ty, span)?;
                    bld(self.builder.build_load(ref_llvm, slot, "deref.ptr"))?
                        .into_pointer_value()
                }
                other => self
                    .gen_value(other)?
                    .scalar(span, "field access deref")?
                    .into_pointer_value(),
            }
        } else {
            match target {
                HirExpr::Var(def_id, _) => *self
                    .vars
                    .get(def_id)
                    .ok_or_else(|| self.internal_err(span, "field target not allocated"))?,
                other => self.gen_value(other)?.agg(span, "field access")?,
            }
        };
        let struct_ty = self.t(&recv_ty, span)?;
        // Unions: every field lives at byte offset 0 of the flat byte buffer.
        // The buffer pointer is bitcast to the field's type (no GEP indexing).
        let slot = if matches!(&recv_ty, Ty::Union(_)) {
            bld(self
                .builder
                .build_pointer_cast(ptr, fty_llvm.ptr_type(AddressSpace::from(0u16)), "union.fld"))?
        } else {
            bld(unsafe {
                self.builder.build_in_bounds_gep(
                    struct_ty,
                    ptr,
                    &[
                        self.i32_ty.const_zero(),
                        self.i32_ty.const_int(fidx as u64, false),
                    ],
                    "fld_ptr",
                )
            })?
        };
        Ok((slot, fty_llvm, recv_ty))
    }

    fn expr_ty(&self, expr: &HirExpr) -> Result<Ty, CodegenError> {
        match expr {
            HirExpr::IntLit(..) => Ok(Ty::I64),
            HirExpr::FloatLit(..) => Ok(Ty::F64),
            HirExpr::CharLit(..) => Ok(Ty::Char),
            HirExpr::BoolLit(..) => Ok(Ty::Bool),
            HirExpr::StrLit(..) => Ok(Ty::Str),
            HirExpr::Var(def_id, span) => self
                .var_tys
                .get(def_id)
                .cloned()
                .ok_or_else(|| self.internal_err(*span, "missing variable type")),
            HirExpr::ConstRef { name, span, ty } => {
                // Prefer the const's resolved type (written back by inference after
                // lowering); fall back to the placeholder stored at lowering time. This
                // keeps unannotated float consts correct at use sites (Phase P0-3).
                if let Some(c) = self.hir_consts.iter().find(|c| c.name == *name) {
                    Ok(c.ty.clone())
                } else {
                    Ok(ty.clone())
                }
            }
            HirExpr::Borrow { def_id, mut_, span } => {
                let src = self
                    .var_tys
                    .get(def_id)
                    .cloned()
                    .ok_or_else(|| self.internal_err(*span, "missing variable type"))?;
                // Borrowing a raw pointer `*T` yields `**T` (see infer.rs): the borrowed
                // expression is the address of the pointer slot, used for FFI out-params.
                if matches!(&src, Ty::Ptr(_)) {
                    Ok(Ty::Ptr(Box::new(src)))
                } else {
                    Ok(Ty::Ref {
                        mut_: *mut_,
                        lifetime: None,
                        inner: Box::new(src),
                    })
                }
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
            HirExpr::Try { target, span } => {
                // `expr?` evaluates to the `Ok` payload type of the target `Result`.
                let t = self.expr_ty(target)?;
                match &t {
                    Ty::EnumGeneric { name, args } if name == "Result" && args.len() == 2 => {
                        Ok(args[0].clone())
                    }
                    other => Err(self.internal_err(
                        *span,
                        &format!("`?` target must be a `Result<T, E>`, got `{other}`"),
                    )),
                }
            }
            HirExpr::MethodCall { recv, method, span, .. } => {
                // `dyn Trait` receiver: the return type comes from the trait method
                // signature (the concrete implementation is chosen at runtime).
                if let Ty::Dyn { trait_name } = self.expr_ty(recv)? {
                    let trait_def = self
                        .hir_traits
                        .iter()
                        .find(|t| t.name == *trait_name)
                        .ok_or_else(|| {
                            self.internal_err(*span, &format!("trait `{trait_name}` not found"))
                        })?;
                    let m = trait_def.find_method(method).ok_or_else(|| {
                        self.internal_err(
                            *span,
                            &format!("trait `{trait_name}` has no method `{method}`"),
                        )
                    })?;
                    return Ok(m.ret.clone().unwrap_or(Ty::Void));
                }
                // Trait/inherent method: return type comes from the resolved function signature
                if let Some(def_id) = self.method_def(recv, method) {
                    let f = self
                        .hir_funcs
                        .get(def_id as usize)
                        .ok_or_else(|| self.internal_err(*span, "missing method function table"))?;
                    let ret = f.ret.clone().unwrap_or(Ty::Void);
                    // Generic methods (from `impl<T> Type<T>`) are monomorphized per instance:
                    // substitute the concrete type arguments into the return type so callers
                    // (e.g. `print("%s", map.get(k, def))`) see the instantiated type, not `T`.
                    if !f.type_params.is_empty() {
                        let type_args = self.resolve_call_instance(*span, f)?;
                        let subst: std::collections::HashMap<String, Ty> = f
                            .type_params
                            .iter()
                            .cloned()
                            .zip(type_args)
                            .collect();
                        return Ok(substitute(&ret, &subst));
                    }
                    return Ok(ret);
                }
                // Native `Vec<T>` methods: return type derived from the element type.
                // Auto-deref: a `&mut Vec<T>` receiver dispatches on the inner type.
                if let Ty::Vec(elem) = self.deref_native_receiver(recv, *span)? {
                    return Ok(match method.as_str() {
                        "push" | "set" | "free" => Ty::Void,
                        "pop" | "get" => (*elem).clone(),
                        "len" => Ty::I64,
                        "is_empty" => Ty::Bool,
                        other => {
                            return Err(self.internal_err(
                                *span,
                                &format!("`Vec` has no method `{other}` (supported: push/pop/len/get/set/free/is_empty)"),
                            ));
                        }
                    });
                }
                // Native `Box<T>` methods: return type derived from the inner type.
                if let Ty::Box(inner) = self.deref_native_receiver(recv, *span)? {
                    return Ok(match method.as_str() {
                        "deref" => (*inner).clone(),
                        "free" => Ty::Void,
                        other => {
                            return Err(self.internal_err(
                                *span,
                                &format!("`Box` has no method `{other}` (supported: deref/free)"),
                            ));
                        }
                    });
                }
                // Native `String` methods: return type is fixed by the method name.
                if let Ty::String = self.deref_native_receiver(recv, *span)? {
                    return Ok(match method.as_str() {
                        "push" | "push_str" | "utf8_push" | "clear" | "free" => Ty::Void,
                        "pop" | "utf8_pop" | "len" | "at" => Ty::I64,
                        "is_empty" | "starts_with" | "ends_with" => Ty::Bool,
                        "data" => Ty::Str,
                        other => {
                            return Err(self.internal_err(
                                *span,
                                &format!("`String` has no method `{other}` (supported: push/push_str/utf8_push/pop/utf8_pop/len/is_empty/clear/at/data/starts_with/ends_with/free)"),
                            ));
                        }
                    });
                }
                match method.as_str() {
                    "alloc" => Ok(Ty::Ptr(Box::new(Ty::I64))),
                    "reset" => Ok(Ty::Void),
                    other => Err(self.internal_err(*span, &format!("unknown method `{other}`"))),
                }
            }
            HirExpr::ArenaLit(n, _) => Ok(Ty::Arena(*n)),
            HirExpr::TensorLit { dims, elem, .. } => Ok(Ty::Tensor {
                elem: Box::new(elem.clone()),
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
            HirExpr::Reduce { input, span, .. } => {
                let it = self.expr_ty(input)?;
                match it {
                    Ty::Tensor { elem, shape } if !shape.is_empty() => Ok(*elem),
                    _other => Err(self.internal_err(*span, &format!("invalid reduce input type"))),
                }
            }
            HirExpr::ElemWise { lhs, span, .. } => {
                let lt = self.expr_ty(lhs)?;
                match lt {
                    Ty::Tensor { elem, shape } if !shape.is_empty() => Ok(Ty::Tensor {
                        elem,
                        shape,
                    }),
                    _other => Err(self.internal_err(*span, &format!("invalid element-wise operand type"))),
                }
            }
            HirExpr::Blas { op, args, span } => {
                // `scal`/`axpy` put a scalar `alpha` before the first tensor operand.
                let first_tensor = match op {
                    BlasOp::Scal | BlasOp::Axpy => 1,
                    _ => 0,
                };
                let elem = match self.expr_ty(&args[first_tensor])? {
                    Ty::Tensor { elem, shape } if !shape.is_empty() => (*elem, shape),
                    other => {
                        return Err(self.internal_err(
                            *span,
                            &format!("invalid BLAS tensor operand type `{other}`"),
                        ))
                    }
                };
                match op {
                    // Scal / Axpy → tensor of the same shape.
                    BlasOp::Scal | BlasOp::Axpy => {
                        let (elem_ty, shape) = elem;
                        Ok(Ty::Tensor {
                            elem: Box::new(elem_ty),
                            shape,
                        })
                    }
                    // Amax → index (i64).
                    BlasOp::Amax => Ok(Ty::I64),
                    // Dot / Nrm2 / Asum → scalar of the tensor element type.
                    _ => Ok(elem.0),
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
                    Ty::Vec(elem) => Ok(*elem),
                    Ty::Str | Ty::String => Ok(Ty::I64),
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
            HirExpr::StructLit { name, span, .. } => {
                // The struct must exist (inference already validated). Return the named type,
                // or the monomorphized generic instance (type args recorded by inference).
                let generic = self
                    .hir_structs
                    .iter()
                    .find(|s| s.name == *name)
                    .map(|s| !s.type_params.is_empty())
                    .unwrap_or(false);
                if generic {
                    let raw = self
                        .struct_lit_types
                        .get(&span.start)
                        .cloned()
                        .ok_or_else(|| {
                            self.internal_err(
                                *span,
                                &format!("internal error: generic struct literal `{name}` lacks type-instance info (infer did not record it)"),
                            )
                        })?;
                    let resolved: Vec<Ty> = raw
                        .iter()
                        .map(|t| substitute(t, &self.type_subst))
                        .collect();
                    return Ok(Ty::StructGeneric {
                        name: name.clone(),
                        args: resolved,
                    });
                }
                Ok(Ty::Struct(name.clone()))
            }
            HirExpr::EnumLit { name, span, arg, .. } => {
                // Native `Vec<T>` construction: element type recorded by inference.
                if name == "Vec" {
                    let raw = self
                        .enum_lit_types
                        .get(&span.start)
                        .cloned()
                        .ok_or_else(|| {
                            self.internal_err(
                                *span,
                                "internal error: `Vec` constructor lacks element type (infer did not record it)",
                            )
                        })?;
                    let resolved = substitute(&raw[0], &self.type_subst);
                    return Ok(Ty::Vec(Box::new(resolved)));
                }
                // Native `String` construction: fixed `String` type.
                if name == "String" {
                    return Ok(Ty::String);
                }
                // Native `Box<T>` construction: inner type inferred from the argument.
                if name == "Box" {
                    let inner = arg
                        .as_ref()
                        .map(|a| self.expr_ty(a))
                        .ok_or_else(|| self.internal_err(*span, "Box::new requires an argument"))??;
                    return Ok(Ty::Box(Box::new(inner)));
                }
                // The enum must exist (inference already validated). Return the named type,
                // or the monomorphized generic instance (type args recorded by inference).
                let generic = self
                    .hir_enums
                    .iter()
                    .find(|e| e.name == *name)
                    .map(|e| !e.type_params.is_empty())
                    .unwrap_or(false);
                if generic {
                    let raw = self
                        .enum_lit_types
                        .get(&span.start)
                        .cloned()
                        .ok_or_else(|| {
                            self.internal_err(
                                *span,
                                &format!("internal error: generic enum literal `{name}` lacks type-instance info (infer did not record it)"),
                            )
                        })?;
                    let resolved: Vec<Ty> = raw
                        .iter()
                        .map(|t| substitute(t, &self.type_subst))
                        .collect();
                    return Ok(Ty::EnumGeneric {
                        name: name.clone(),
                        args: resolved,
                    });
                }
                Ok(Ty::Enum(name.clone()))
            }
            HirExpr::Field { target, field, span } => {
                let recv_ty = self.expr_ty(target)?;
                // Auto-deref: `recv.field` on `&T` / `&mut T` accesses `(*recv).field`
                let recv_ty = match &recv_ty {
                    Ty::Ref { inner, .. } => (**inner).clone(),
                    other => other.clone(),
                };
                match &recv_ty {
                    Ty::Union(struct_name) => {
                        let def = self
                            .hir_unions
                            .iter()
                            .find(|u| u.name == *struct_name)
                            .ok_or_else(|| {
                                self.internal_err(*span, &format!("undefined union `{struct_name}`"))
                            })?;
                        match def.find_field(field) {
                            Some((_, t)) => Ok(t.clone()),
                            None => Err(self.internal_err(
                                *span,
                                &format!("union `{struct_name}` has no field `{field}`"),
                            )),
                        }
                    }
                    Ty::Struct(struct_name) => {
                        let def = self
                            .hir_structs
                            .iter()
                            .find(|s| s.name == *struct_name)
                            .ok_or_else(|| {
                                self.internal_err(*span, &format!("undefined struct `{struct_name}`"))
                            })?;
                        match def.find_field(field) {
                            Some((_, t)) => Ok(t.clone()),
                            None => Err(self.internal_err(
                                *span,
                                &format!("struct `{struct_name}` has no field `{field}`"),
                            )),
                        }
                    }
                    Ty::StructGeneric { name, args } => {
                        let def = self
                            .hir_structs
                            .iter()
                            .find(|s| s.name == *name)
                            .ok_or_else(|| self.internal_err(*span, &format!("undefined struct `{name}`")))?;
                        let subst: HashMap<String, Ty> = def
                            .type_params
                            .iter()
                            .cloned()
                            .zip(args.iter().cloned())
                            .collect();
                        let fty = match def.find_field(field) {
                            Some((_, t)) => t.clone(),
                            None => {
                                return Err(self.internal_err(
                                    *span,
                                    &format!("struct `{name}` has no field `{field}`"),
                                ));
                            }
                        };
                        Ok(substitute(&fty, &subst))
                    }
                    other => Err(self.internal_err(
                        *span,
                        &format!("cannot access field `.{field}` on type `{other}`"),
                    )),
                }
            }
            // `expr as dyn Trait` evaluates to the dyn fat-pointer type.
            HirExpr::Cast { ty, .. } => Ok(ty.clone()),
            // A named function used as a value has the function-pointer type.
            HirExpr::FnRef { def_id, span } => {
                let f = self
                    .hir_funcs
                    .get(*def_id as usize)
                    .ok_or_else(|| self.internal_err(*span, "missing function table"))?;
                let params = f.params.iter().map(|(_, t, _)| t.clone()).collect();
                let ret = f.ret.clone().unwrap_or(Ty::Void);
                Ok(Ty::Fn(params, Box::new(ret)))
            }
            // An indirect call's type is its callee's return type.
            HirExpr::CallPtr { callee, .. } => match self.expr_ty(callee)? {
                Ty::Fn(_, ret) => Ok(*ret),
                other => Err(self.internal_err(
                    callee.span(),
                    &format!("cannot call a value of type `{other}` (expected a function pointer)"),
                )),
            },
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

    /// Native `Vec<T>` constructor: `Vec::new()` (empty, no allocation) and
    /// `Vec::with_cap(n)` (pre-allocated buffer). Builds a `{ data, len, cap }`
    /// aggregate slot (the buffer is malloc-managed, released via `Vec::free`).
    fn gen_vec_ctor(
        &mut self,
        variant: &str,
        arg: Option<&HirExpr>,
        span: Span,
    ) -> Result<GenValue<'ctx>, CodegenError> {
        // Element type recorded by inference as `enum_lit_types[span.start] = [elem]`.
        let raw = self
            .enum_lit_types
            .get(&span.start)
            .cloned()
            .ok_or_else(|| {
                self.internal_err(
                    span,
                    "internal error: `Vec` constructor lacks element type (infer did not record it)",
                )
            })?;
        let elem = substitute(&raw[0], &self.type_subst);
        let vec_llvm = self.t(&Ty::Vec(Box::new(elem.clone())), span)?;
        let tmp = bld(self.builder.build_alloca(vec_llvm, "vec"))?;
        let zero = self.i32_ty.const_zero();
        let one = self.i32_ty.const_int(1, false);
        let two = self.i32_ty.const_int(2, false);
        let data_slot = bld(unsafe {
            self.builder.build_in_bounds_gep(vec_llvm, tmp, &[zero, zero], "vec.data")
        })?;
        let len_slot = bld(unsafe {
            self.builder.build_in_bounds_gep(vec_llvm, tmp, &[zero, one], "vec.len")
        })?;
        let cap_slot = bld(unsafe {
            self.builder.build_in_bounds_gep(vec_llvm, tmp, &[zero, two], "vec.cap")
        })?;
        let i8_ptr = self.context.ptr_type(AddressSpace::from(0u16));
        match variant {
            "new" => {
                // data = null, len = 0, cap = 0 (the first `push` allocates)
                bld(self.builder.build_store(data_slot, i8_ptr.const_null()))?;
                bld(self.builder.build_store(len_slot, self.i64_ty.const_zero()))?;
                bld(self.builder.build_store(cap_slot, self.i64_ty.const_zero()))?;
            }
            "with_cap" => {
                let cap_arg = arg.ok_or_else(|| {
                    CodegenError {
                        msg: "`Vec::with_cap` requires a capacity argument".to_string(),
                        line: span.line,
                        col: span.col,
                    }
                })?;
                let cap = self.gen_value(cap_arg)?.scalar(span, "Vec::with_cap capacity")?;
                let cap = self.coerce(cap, &self.i64_ty.into(), span, "Vec::with_cap capacity")?;
                let elem_size = aero_size(&elem, self.hir_structs, self.hir_unions, self.hir_enums, &self.type_subst);
                let nbytes = bld(self.builder.build_int_mul(
                    cap.into_int_value(),
                    self.i64_ty.const_int(elem_size, false),
                    "vec_nbytes",
                ))?;
                let data = bld(self.builder.build_call(self.malloc, &[nbytes.into()], "vec_alloc"))?
                    .try_as_basic_value()
                    .basic()
                    .ok_or_else(|| self.internal_err(span, "malloc returned no value"))?;
                bld(self.builder.build_store(data_slot, data))?;
                bld(self.builder.build_store(len_slot, self.i64_ty.const_zero()))?;
                bld(self.builder.build_store(cap_slot, cap))?;
            }
            other => {
                return Err(CodegenError {
                    msg: format!("`Vec` has no constructor `{other}` (supported: new/with_cap)"),
                    line: span.line,
                    col: span.col,
                });
            }
        }
        Ok(GenValue::Agg(tmp))
    }

    /// Native `String` constructor (`String::new` / `String::with_cap(n)` /
    /// `String::from(s)`). Produces a `{ data: i8*, len: i64, cap: i64 }` struct whose
    /// buffer is always NUL-terminated at `data[len]` and `data` is never null
    /// (an empty String still owns a 1-byte terminator buffer).
    fn gen_string_ctor(
        &mut self,
        variant: &str,
        arg: Option<&HirExpr>,
        span: Span,
    ) -> Result<GenValue<'ctx>, CodegenError> {
        let str_llvm = self.t(&Ty::String, span)?;
        let tmp = bld(self.builder.build_alloca(str_llvm, "str"))?;
        let i64t = self.i64_ty;
        let i8_ptr = self.context.ptr_type(AddressSpace::from(0u16));
        let i8t = self.context.i8_type();
        let zero = self.i32_ty.const_zero();
        let one = self.i32_ty.const_int(1, false);
        let two = self.i32_ty.const_int(2, false);
        let data_slot = bld(unsafe {
            self.builder.build_in_bounds_gep(str_llvm, tmp, &[zero, zero], "str.data")
        })?;
        let len_slot = bld(unsafe {
            self.builder.build_in_bounds_gep(str_llvm, tmp, &[zero, one], "str.len")
        })?;
        let cap_slot = bld(unsafe {
            self.builder.build_in_bounds_gep(str_llvm, tmp, &[zero, two], "str.cap")
        })?;
        let err = |msg: String| CodegenError {
            msg,
            line: span.line,
            col: span.col,
        };
        match variant {
            "new" => {
                // Allocate a 1-byte terminator buffer so `data()` is always valid.
                let data = bld(self.builder.build_call(self.malloc, &[i64t.const_int(1, false).into()], "str_alloc"))?
                    .try_as_basic_value()
                    .basic()
                    .ok_or_else(|| self.internal_err(span, "malloc returned no value"))?
                    .into_pointer_value();
                bld(self.builder.build_store(data_slot, data))?;
                let zero_byte = i8t.const_zero();
                bld(self.builder.build_store(data, zero_byte))?;
                bld(self.builder.build_store(len_slot, i64t.const_zero()))?;
                bld(self.builder.build_store(cap_slot, i64t.const_int(1, false)))?;
            }
            "with_cap" => {
                let cap_arg = arg.ok_or_else(|| {
                    err("`String::with_cap` requires a capacity argument".to_string())
                })?;
                let cap = self.gen_value(cap_arg)?.scalar(span, "String::with_cap capacity")?;
                let cap = self.coerce(cap, &i64t.into(), span, "String::with_cap capacity")?;
                let cap = cap.into_int_value();
                // Ensure at least 1 byte so the terminator always fits.
                let cap_zero = bld(self.builder.build_int_compare(
                    IntPredicate::EQ,
                    cap,
                    i64t.const_zero(),
                    "scapz",
                ))?;
                let cap1 = bld(self.builder.build_select::<IntValue, IntValue>(
                    cap_zero,
                    i64t.const_int(1, false),
                    cap,
                    "scap1",
                ))?;
                let data = bld(self.builder.build_call(self.malloc, &[cap1.into()], "str_alloc"))?
                    .try_as_basic_value()
                    .basic()
                    .ok_or_else(|| self.internal_err(span, "malloc returned no value"))?
                    .into_pointer_value();
                bld(self.builder.build_store(data_slot, data))?;
                let zero_byte = i8t.const_zero();
                bld(self.builder.build_store(data, zero_byte))?;
                bld(self.builder.build_store(len_slot, i64t.const_zero()))?;
                bld(self.builder.build_store(cap_slot, cap1))?;
            }
            "from" => {
                let s_arg = arg.ok_or_else(|| {
                    err("`String::from` requires a `str` argument".to_string())
                })?;
                let s = self.gen_value(s_arg)?.scalar(span, "String::from argument")?;
                let s = s.into_pointer_value();
                let slen = bld(self.builder.build_call(self.strlen, &[s.into()], "s_from_len"))?
                    .try_as_basic_value()
                    .basic()
                    .ok_or_else(|| self.internal_err(span, "strlen returned no value"))?;
                // cap = slen + 1 (room for the terminator); copy slen+1 bytes incl. '\0'.
                let slen64 = slen.into_int_value();
                let nbytes = bld(self.builder.build_int_add(
                    slen64,
                    i64t.const_int(1, false),
                    "s_from_nbytes",
                ))?;
                let data = bld(self.builder.build_call(self.malloc, &[nbytes.into()], "str_alloc"))?
                    .try_as_basic_value()
                    .basic()
                    .ok_or_else(|| self.internal_err(span, "malloc returned no value"))?;
                bld(self.builder.build_call(
                    self.memcpy,
                    &[data.into(), s.into(), nbytes.into()],
                    "str_from_copy",
                ))?;
                bld(self.builder.build_store(data_slot, data))?;
                bld(self.builder.build_store(len_slot, slen64))?;
                bld(self.builder.build_store(cap_slot, nbytes))?;
            }
            other => {
                return Err(err(format!(
                    "`String` has no constructor `{other}` (supported: new/with_cap/from)"
                )));
            }
        }
        Ok(GenValue::Agg(tmp))
    }

    /// Native `Box<T>` constructor: `Box::new(value)` allocates a `T` on the heap
    /// (via malloc), copies `value` into it, and returns a `Box<T>` (a single `i8*`).
    /// The inner type is inferred and recorded by the type checker.
    fn gen_box_ctor(
        &mut self,
        variant: &str,
        arg: Option<&HirExpr>,
        span: Span,
    ) -> Result<GenValue<'ctx>, CodegenError> {
        if variant != "new" {
            return Err(CodegenError {
                msg: format!("`Box` has no constructor `{variant}` (supported: new)"),
                line: span.line,
                col: span.col,
            });
        }
        let value = arg.ok_or_else(|| CodegenError {
            msg: "`Box::new` requires an argument".to_string(),
            line: span.line,
            col: span.col,
        })?;
        let inner_ty = self.expr_ty(value)?;
        let inner_llvm = self.t(&inner_ty, span)?;
        let nbytes = aero_size(&inner_ty, self.hir_structs, self.hir_unions, self.hir_enums, &self.type_subst);
        let alloc = bld(self.builder.build_call(
            self.malloc,
            &[self.i64_ty.const_int(nbytes, false).into()],
            "box_alloc",
        ))?
        .try_as_basic_value()
        .basic()
        .ok_or_else(|| self.internal_err(span, "malloc returned no value"))?
        .into_pointer_value();
        // Store the value into the heap slot (scalar store for scalars, memcpy for aggregates).
        let v = self.gen_value(value)?;
        let src_ptr = match v {
            GenValue::Scalar(s) => {
                let slot = bld(self.builder.build_alloca(inner_llvm, "box_tmp"))?;
                bld(self.builder.build_store(slot, s))?;
                slot
            }
            GenValue::Agg(slot) => slot,
        };
        let dst_ptr = bld(self.builder.build_pointer_cast(
            alloc,
            self.context.ptr_type(AddressSpace::from(0u16)),
            "box_dst",
        ))?;
        self.emit_memcpy(dst_ptr, src_ptr, nbytes, span, "Box::new copy")?;
        Ok(GenValue::Scalar(
            alloc.as_basic_value_enum(),
        ))
    }

    /// Resolve a method call to its function DefId. Returns `None` for arena built-in
    /// methods (alloc/reset) and for methods without a registered impl.
    fn method_def(&self, recv: &HirExpr, method: &str) -> Option<DefId> {
        let recv_ty = self.expr_ty(recv).ok()?;
        // Receiver type name: a struct/enum, unwrapping references, and resolving generic
        // parameters through the current instance substitution (static dispatch)
        let type_name = match &recv_ty {
            Ty::Struct(n) | Ty::Enum(n) => n.clone(),
            Ty::StructGeneric { name, .. } | Ty::EnumGeneric { name, .. } => name.clone(),
            Ty::Ref { inner, .. } => match &**inner {
                Ty::Struct(n) | Ty::Enum(n) => n.clone(),
                _ => return None,
            },
            Ty::Generic(tp) => match self.type_subst.get(tp) {
                Some(Ty::Struct(n)) | Some(Ty::Enum(n)) => n.clone(),
                Some(Ty::StructGeneric { name, .. }) | Some(Ty::EnumGeneric { name, .. }) => {
                    name.clone()
                }
                _ => return None,
            },
            _ => return None,
        };
        self.method_map
            .get(&(type_name, method.to_string()))
            .copied()
    }

    /// Whether a type has a registered `Drop` impl, returning the `drop` method DefId.
    fn drop_def(&self, ty: &Ty) -> Option<DefId> {
        let type_name = match ty {
            Ty::Struct(n) | Ty::Enum(n) => n.clone(),
            Ty::StructGeneric { name, .. } | Ty::EnumGeneric { name, .. } => name.clone(),
            _ => return None,
        };
        self.method_map.get(&(type_name, "drop".to_string())).copied()
    }

    /// Resolve a method on a *type* (not an expression) to its monomorphized LLVM
    /// function and concrete return type. Used by the `for`-loop `IntoIterator`/
    /// `Iterator` protocol lowering, where the `into_iter`/`next` calls are
    /// synthesized directly (no HIR method-call node exists, so infer's recorded
    /// call-site types may be absent for them).
    fn resolve_method_for(
        &mut self,
        recv_ty: &Ty,
        method: &str,
        span: Span,
    ) -> Result<(FunctionValue<'ctx>, Ty), CodegenError> {
        let type_name = match recv_ty {
            Ty::Struct(n) | Ty::Enum(n) => n.clone(),
            Ty::StructGeneric { name, .. } | Ty::EnumGeneric { name, .. } => name.clone(),
            other => {
                return Err(self.internal_err(
                    span,
                    &format!(
                        "type `{other}` has no method `{method}` (iteration requires an `IntoIterator`/`Iterator` impl)"
                    ),
                ))
            }
        };
        let def_id = self
            .method_map
            .get(&(type_name.clone(), method.to_string()))
            .copied()
            .ok_or_else(|| {
                self.internal_err(
                    span,
                    &format!("type `{type_name}` has no method `{method}`"),
                )
            })?;
        let f = self
            .hir_funcs
            .get(def_id as usize)
            .ok_or_else(|| self.internal_err(span, "internal error: method function table missing"))?;
        if !f.type_params.is_empty() {
            // Monomorphize. Prefer the receiver's own generic args (impl type params
            // align with the receiver args, e.g. `impl<T> Iterator for VecIter<T>`);
            // fall back to infer's recorded call-site types.
            let recv_args = match recv_ty {
                Ty::StructGeneric { args, .. } | Ty::EnumGeneric { args, .. } => args.clone(),
                _ => Vec::new(),
            };
            let type_args = if !recv_args.is_empty() && recv_args.len() == f.type_params.len() {
                recv_args
            } else {
                self.resolve_call_instance(span, f)?
            };
            let func = self.gen_instance(def_id, type_args.clone())?;
            let subst: HashMap<String, Ty> = f
                .type_params
                .iter()
                .cloned()
                .zip(type_args.into_iter())
                .collect();
            let ret = f
                .ret
                .as_ref()
                .map(|t| substitute(t, &subst))
                .unwrap_or(Ty::Void);
            Ok((func, ret))
        } else {
            let func = *self
                .funcs
                .get(def_id as usize)
                .ok_or_else(|| {
                    self.internal_err(span, "internal error: method function table missing")
                })?;
            let ret = f.ret.clone().unwrap_or(Ty::Void);
            Ok((func, ret))
        }
    }

    /// For an iterator `next` returning `Option<Item>`, extract the `Item` type and
    /// the tag indices of the `Some`/`None` variants (used by `for`-loop lowering).
    fn for_option_parts(
        &self,
        option_ty: &Ty,
        span: Span,
    ) -> Result<(Ty, usize, usize), CodegenError> {
        let (name, args) = match option_ty {
            Ty::Enum(n) => (n.clone(), Vec::new()),
            Ty::EnumGeneric { name, args } => (name.clone(), args.clone()),
            other => {
                return Err(self.internal_err(
                    span,
                    &format!("iterator `next` must return `Option<Item>`, got `{other}`"),
                ))
            }
        };
        if name != "Option" {
            return Err(self.internal_err(
                span,
                &format!("iterator `next` must return `Option<Item>`, got enum `{name}`"),
            ));
        }
        if args.len() != 1 {
            return Err(self.internal_err(
                span,
                "`Option` requires one type argument (iterator `next` must return `Option<Item>`)",
            ));
        }
        let item_ty = args.into_iter().next().unwrap();
        let def = self
            .hir_enums
            .iter()
            .find(|e| e.name == "Option")
            .ok_or_else(|| self.internal_err(span, "internal error: `Option` enum not defined"))?;
        let some_idx = def
            .find_variant("Some")
            .map(|(i, _)| i)
            .ok_or_else(|| self.internal_err(span, "internal error: `Option::Some` variant missing"))?;
        let none_idx = def
            .find_variant("None")
            .map(|(i, _)| i)
            .ok_or_else(|| self.internal_err(span, "internal error: `Option::None` variant missing"))?;
        Ok((item_ty, some_idx, none_idx))
    }

    /// Emit the implicit drop call for a variable (if its type implements `Drop` and
    /// the value was not moved). Skipped for moved variables — ownership has been
    /// transferred to the new owner, which drops it instead.
    fn gen_drop_var(&mut self, def_id: DefId, scope_id: ScopeId) -> Result<(), CodegenError> {
        let zero_span = Span { line: 0, col: 0, start: 0, end: 0 };
        if let Some(moved) = self.moved_by_scope.get(&scope_id) {
            if moved.contains(&def_id) {
                return Ok(());
            }
        }
        let ty = match self.var_tys.get(&def_id) {
            Some(t) => t.clone(),
            None => return Ok(()),
        };
        let drop_def = match self.drop_def(&ty) {
            Some(d) => d,
            None => return Ok(()),
        };
        let ptr = match self.vars.get(&def_id) {
            Some(p) => *p,
            None => return Ok(()),
        };
        // Resolve the (possibly generic) drop function, like `gen_method_call` does.
        let hir_f = self
            .hir_funcs
            .get(drop_def as usize)
            .ok_or_else(|| self.internal_err(zero_span, "missing drop function table"))?;
        let func = if !hir_f.type_params.is_empty() {
            let type_args = self.resolve_call_instance(zero_span, hir_f)?;
            self.gen_instance(drop_def, type_args)?
        } else {
            *self
                .funcs
                .get(drop_def as usize)
                .ok_or_else(|| self.internal_err(zero_span, "missing drop function"))?
        };
        // `drop(x: &mut Self)` takes the address of the value; the stack slot is it.
        bld(self.builder.build_call(func, &[ptr.into()], "drop"))?;
        Ok(())
    }

    /// Emit drops for every variable still live in the current function, in reverse
    /// declaration order (params first), consulting the moved set of `scope_id`.
    fn gen_drop_all_live(&mut self, scope_id: ScopeId) -> Result<(), CodegenError> {
        let to_drop: Vec<DefId> = self
            .decl_order
            .iter()
            .rev()
            .copied()
            .filter(|def| self.vars.contains_key(def))
            .collect();
        for def in to_drop {
            self.gen_drop_var(def, scope_id)?;
        }
        Ok(())
    }

    /// Operator overloading for arithmetic: `a op b` on a non-numeric user type
    /// lowers to the corresponding trait method call (`Add::add(a, b)` etc.),
    /// reusing the standard method-call path (receiver = first argument).
    fn gen_binop_trait(
        &mut self,
        lhs: &HirExpr,
        rhs: &HirExpr,
        op: BinOp,
        span: Span,
    ) -> Result<GenValue<'ctx>, CodegenError> {
        let method = match op {
            BinOp::Add => "add",
            BinOp::Sub => "sub",
            BinOp::Mul => "mul",
            BinOp::Div => "div",
            BinOp::Rem => "rem",
            BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Shr => {
                return Err(self.internal_err(
                    span,
                    "bitwise operator used on a non-integer type (integer-only)",
                ));
            }
        };
        let synthetic = HirExpr::MethodCall {
            recv: Box::new(lhs.clone()),
            method: method.to_string(),
            args: vec![rhs.clone()],
            span,
        };
        match self.gen_method_call(&synthetic)? {
            Some(v) => Ok(v),
            None => Err(self.internal_err(span, "operator method returned no value")),
        }
    }

    /// Operator overloading for comparisons: `==`/`!=` call `Eq::eq`; `<`/`>`/`<=`/`>=`
    /// call `Ord::lt`. Derived operators (`!=`, `>`, `<=`, `>=`) are produced by negation
    /// of the base call or by swapping the operands (`a > b` == `lt(b, a)`).
    fn gen_cmp_trait(
        &mut self,
        lhs: &HirExpr,
        rhs: &HirExpr,
        op: CmpOp,
        span: Span,
    ) -> Result<GenValue<'ctx>, CodegenError> {
        let (method, swap, negate) = match op {
            CmpOp::Eq => ("eq", false, false),
            CmpOp::Ne => ("eq", false, true),
            CmpOp::Lt => ("lt", false, false),
            CmpOp::Gt => ("lt", true, false),
            CmpOp::Le => ("lt", true, true),
            CmpOp::Ge => ("lt", false, true),
        };
        let (recv, arg) = if swap { (rhs, lhs) } else { (lhs, rhs) };
        let synthetic = HirExpr::MethodCall {
            recv: Box::new(recv.clone()),
            method: method.to_string(),
            args: vec![arg.clone()],
            span,
        };
        let v = match self.gen_method_call(&synthetic)? {
            Some(GenValue::Scalar(v)) => v,
            Some(GenValue::Agg(_)) => {
                return Err(self.internal_err(
                    span,
                    "comparison operator method must return `bool`",
                ))
            }
            None => {
                return Err(self.internal_err(
                    span,
                    "comparison operator method returned no value",
                ))
            }
        };
        if negate {
            let b = v.into_int_value();
            let n = bld(self.builder.build_not(b, "opnot"))?;
            Ok(GenValue::Scalar(n.into()))
        } else {
            Ok(GenValue::Scalar(v))
        }
    }

    /// Method-call codegen: trait/inherent methods become plain function calls with the
    /// receiver as the first argument; arena `alloc(n)` (returns an `i64*` to the slot)
    /// and `reset()` are handled separately. `None` means no return value (void method).
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
        // `dyn Trait` receiver (Phase 9): virtual dispatch through the vtable. The
        // receiver is a fat pointer `{ data, vtable }`; we load the vtable, index to
        // the method's thunk, and call it with `data` + the extra arguments.
        if let Ty::Dyn { trait_name } = self.expr_ty(recv)? {
            return self.gen_dyn_method_call(
                recv,
                &trait_name,
                method,
                args,
                span,
            );
        }
        // Trait/inherent method: a plain function call with the receiver as the first argument.
        // Generic methods (from `impl<T> Type<T>`) are monomorphized per instance.
        if let Some(def_id) = self.method_def(recv, method) {
            let hir_f = self
                .hir_funcs
                .get(def_id as usize)
                .ok_or_else(|| self.internal_err(span, "missing method function table"))?;
            let func = if !hir_f.type_params.is_empty() {
                let type_args = self.resolve_call_instance(span, hir_f)?;
                self.gen_instance(def_id, type_args)?
            } else {
                *self
                    .funcs
                    .get(def_id as usize)
                    .ok_or_else(|| self.internal_err(span, "missing method function table"))?
            };
            let mut call_args = Vec::new();
            let param_tys = func.get_type().get_param_types();
            // Receiver as the first implicit parameter (`self`)
            let rv = self.gen_value(recv)?;
            let rpt: BasicTypeEnum = param_tys[0]
                .try_into()
                .map_err(|_| self.internal_err(span, "method receiver type mismatch"))?;
            // Auto-reference `&self` / `&mut self` receivers: pass the receiver's
            // address instead of its value (mirrors `unify_receiver` in type checking).
            if rpt.is_pointer_type() {
                let addr = match rv {
                    GenValue::Agg(p) => p,
                    GenValue::Scalar(v) => match &**recv {
                        // Scalar receiver (`&mut i64` methods): use its stack-slot address
                        HirExpr::Var(def_id, _) | HirExpr::Borrow { def_id, .. } => *self
                            .vars
                            .get(def_id)
                            .ok_or_else(|| self.internal_err(span, "receiver has no stack slot"))?,
                        _ => v.into_pointer_value(),
                    },
                };
                call_args.push(addr.into());
            } else {
                call_args.push(self.call_arg(rv, &rpt, span, "method receiver")?.into());
            }
            for (i, arg) in args.iter().enumerate() {
                let v = self.gen_value(arg)?;
                let pt: BasicTypeEnum = param_tys[i + 1]
                    .try_into()
                    .map_err(|_| self.internal_err(span, "method argument type mismatch"))?;
                call_args.push(self.call_arg(v, &pt, span, "method argument")?.into());
            }
            let out = bld(self.builder.build_call(func, &call_args, "call"))?;
            return match out.try_as_basic_value().basic() {
                Some(v) => {
                    // Aggregate return (array/tuple): store into a temp slot and return an Agg pointer
                    let is_agg = matches!(
                        v.get_type(),
                        BasicTypeEnum::ArrayType(_) | BasicTypeEnum::StructType(_)
                    );
                    if is_agg {
                        let tmp = bld(self.builder.build_alloca(v.get_type(), "call_ret"))?;
                        bld(self.builder.build_store(tmp, v))?;
                        Ok(Some(GenValue::Agg(tmp)))
                    } else {
                        Ok(Some(GenValue::Scalar(v)))
                    }
                }
                None => Ok(None),
            };
        }
        // Native `Vec<T>` methods: implemented inline against the heap struct.
        if let Ty::Vec(elem) = self.deref_native_receiver(recv, span)? {
            return self.gen_vec_method(recv, &elem, method, args, span);
        }
        // Native `Box<T>` methods: implemented inline against the heap pointer.
        if let Ty::Box(inner) = self.deref_native_receiver(recv, span)? {
            return self.gen_box_method(recv, &inner, method, args, span);
        }
        // Native `String` methods: implemented inline against the heap struct.
        if let Ty::String = self.deref_native_receiver(recv, span)? {
            return self.gen_string_method(recv, method, args, span);
        }
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

    /// Resolve the receiver type of a native method call, auto-dereferencing a
    /// `&T` / `&mut T` / pointer receiver so native `Vec`/`Box`/`String` methods
    /// dispatch on the inner type.
    fn deref_native_receiver(&self, recv: &HirExpr, span: Span) -> Result<Ty, CodegenError> {
        let _ = span;
        let ty = self.expr_ty(recv)?;
        Ok(match ty {
            Ty::Ref { inner, .. } | Ty::Ptr(inner) => (*inner).clone(),
            other => other,
        })
    }

    /// Native `Vec<T>` methods, implemented inline against the `{ data, len, cap }`
    /// heap struct. Void methods (push/set/free) return `None`; read methods return a
    /// scalar (len/is_empty/pop/get for scalar elements, an Agg slot for aggregates).
    fn gen_vec_method(
        &mut self,
        recv: &HirExpr,
        elem: &Ty,
        method: &str,
        args: &[HirExpr],
        span: Span,
    ) -> Result<Option<GenValue<'ctx>>, CodegenError> {
        let recv_val = self.gen_value(recv)?;
        // A `&T`/`&mut T`/pointer receiver holds the Vec struct address directly;
        // otherwise (value receiver) the receiver is the Vec struct itself.
        let vec_ptr = match self.expr_ty(recv)? {
            Ty::Ref { .. } | Ty::Ptr(_) => recv_val
                .scalar(span, "Vec method receiver")?
                .into_pointer_value(),
            _ => recv_val.agg(span, "Vec method receiver")?,
        };
        let vec_llvm = self.t(&Ty::Vec(Box::new(elem.clone())), span)?;
        let i64t = self.i64_ty;
        let i8_ptr = self.context.ptr_type(AddressSpace::from(0u16));
        let zero = self.i32_ty.const_zero();
        let one = self.i32_ty.const_int(1, false);
        let two = self.i32_ty.const_int(2, false);
        let data_slot = bld(unsafe {
            self.builder.build_in_bounds_gep(vec_llvm, vec_ptr, &[zero, zero], "vec.data")
        })?;
        let len_slot = bld(unsafe {
            self.builder.build_in_bounds_gep(vec_llvm, vec_ptr, &[zero, one], "vec.len")
        })?;
        let cap_slot = bld(unsafe {
            self.builder.build_in_bounds_gep(vec_llvm, vec_ptr, &[zero, two], "vec.cap")
        })?;
        let elem_llvm = self.t(elem, span)?;
        let elem_size = aero_size(elem, self.hir_structs, self.hir_unions, self.hir_enums, &self.type_subst);
        let err = |msg: String| CodegenError {
            msg,
            line: span.line,
            col: span.col,
        };
        match method {
            "push" => {
                if args.len() != 1 {
                    return Err(err("`push` requires 1 argument (the element to append)".to_string()));
                }
                let v = self.gen_value(&args[0])?;
                let len = bld(self.builder.build_load(i64t, len_slot, "vlen"))?.into_int_value();
                let cap = bld(self.builder.build_load(i64t, cap_slot, "vcap"))?.into_int_value();
                // Grow when len >= cap: allocate cap*2 (or 1 for the first push) and migrate.
                let grow_needed = bld(self.builder.build_int_compare(
                    IntPredicate::UGE,
                    len,
                    cap,
                    "vneed_grow",
                ))?;
                let grow_bb = self.context.append_basic_block(self.cur_func, "vec_grow");
                let append_bb = self.context.append_basic_block(self.cur_func, "vec_append");
                bld(self.builder.build_conditional_branch(grow_needed, grow_bb, append_bb))?;
                self.builder.position_at_end(grow_bb);
                let cap_zero = bld(self.builder.build_int_compare(
                    IntPredicate::EQ,
                    cap,
                    i64t.const_zero(),
                    "vcapz",
                ))?;
                let cap2 = bld(self.builder.build_int_mul(cap, i64t.const_int(2, false), "vcap2"))?;
                let new_cap = bld(self.builder.build_select::<IntValue, IntValue>(
                    cap_zero,
                    i64t.const_int(1, false),
                    cap2,
                    "vnewcap",
                ))?;
                let nbytes = bld(self.builder.build_int_mul(
                    new_cap.clone().into_int_value(),
                    i64t.const_int(elem_size, false),
                    "vnewbytes",
                ))?;
                let new_data = bld(self.builder.build_call(self.malloc, &[nbytes.into()], "vec_malloc"))?
                    .try_as_basic_value()
                    .basic()
                    .ok_or_else(|| self.internal_err(span, "malloc returned no value"))?;
                let old_data = bld(self.builder.build_load(i8_ptr, data_slot, "vdata"))?
                    .into_pointer_value();
                let ncopy = bld(self.builder.build_int_mul(
                    len,
                    i64t.const_int(elem_size, false),
                    "vcopy",
                ))?;
                // Only migrate if the old buffer is non-null (first push has data == null).
                let old_null = bld(self.builder.build_is_null(old_data, "vdata_null"))?;
                let skip_bb = self.context.append_basic_block(self.cur_func, "vec_copy_skip");
                let do_bb = self.context.append_basic_block(self.cur_func, "vec_copy_do");
                let join_bb = self.context.append_basic_block(self.cur_func, "vec_copy_join");
                bld(self.builder.build_conditional_branch(old_null, skip_bb, do_bb))?;
                self.builder.position_at_end(do_bb);
                bld(self.builder.build_call(
                    self.memcpy,
                    &[new_data.into(), old_data.into(), ncopy.into()],
                    "vec_memcpy",
                ))?;
                bld(self.builder.build_call(self.free, &[old_data.into()], "vec_free_old"))?;
                bld(self.builder.build_unconditional_branch(join_bb))?;
                self.builder.position_at_end(skip_bb);
                bld(self.builder.build_unconditional_branch(join_bb))?;
                self.builder.position_at_end(join_bb);
                bld(self.builder.build_store(data_slot, new_data))?;
                bld(self.builder.build_store(cap_slot, new_cap))?;
                bld(self.builder.build_unconditional_branch(append_bb))?;
                self.builder.position_at_end(append_bb);
                // data[len] = v
                let data = bld(self.builder.build_load(i8_ptr, data_slot, "vdata2"))?
                    .into_pointer_value();
                let data_elems = bld(self.builder.build_pointer_cast(
                    data,
                    elem_llvm.ptr_type(AddressSpace::from(0u16)),
                    "vec_data_elems",
                ))?;
                let slot = bld(unsafe {
                    self.builder.build_in_bounds_gep(elem_llvm, data_elems, &[len], "velem")
                })?;
                if is_agg(elem) {
                    let src = v.agg(span, "push element")?;
                    self.copy_agg(slot, src, elem, span, "push element")?;
                } else {
                    let vv = v.scalar(span, "push element")?;
                    let vv = self.coerce(vv, &elem_llvm, span, "push element")?;
                    bld(self.builder.build_store(slot, vv))?;
                }
                let new_len = bld(self.builder.build_int_add(len, i64t.const_int(1, false), "vlen1"))?;
                bld(self.builder.build_store(len_slot, new_len))?;
                Ok(None)
            }
            "pop" => {
                if !args.is_empty() {
                    return Err(err("`pop` takes no arguments".to_string()));
                }
                // Result slot: zero-initialized on an empty vector, else the last element.
                let tmp = bld(self.builder.build_alloca(elem_llvm, "vec_pop_tmp"))?;
                let len = bld(self.builder.build_load(i64t, len_slot, "vlen"))?.into_int_value();
                let empty = bld(self.builder.build_int_compare(
                    IntPredicate::EQ,
                    len,
                    i64t.const_zero(),
                    "vpop_empty",
                ))?;
                let empty_bb = self.context.append_basic_block(self.cur_func, "vec_pop_empty");
                let load_bb = self.context.append_basic_block(self.cur_func, "vec_pop_load");
                let join_bb = self.context.append_basic_block(self.cur_func, "vec_pop_join");
                bld(self.builder.build_conditional_branch(empty, empty_bb, load_bb))?;
                self.builder.position_at_end(empty_bb);
                bld(self.builder.build_store(tmp, elem_llvm.const_zero()))?;
                bld(self.builder.build_unconditional_branch(join_bb))?;
                self.builder.position_at_end(load_bb);
                let new_len = bld(self.builder.build_int_sub(len, i64t.const_int(1, false), "vlenm1"))?;
                bld(self.builder.build_store(len_slot, new_len))?;
                let data = bld(self.builder.build_load(i8_ptr, data_slot, "vdata"))?
                    .into_pointer_value();
                let data_elems = bld(self.builder.build_pointer_cast(
                    data,
                    elem_llvm.ptr_type(AddressSpace::from(0u16)),
                    "vec_data_elems",
                ))?;
                let slot = bld(unsafe {
                    self.builder.build_in_bounds_gep(elem_llvm, data_elems, &[new_len], "velem")
                })?;
                if is_agg(elem) {
                    self.copy_agg(tmp, slot, elem, span, "pop element")?;
                } else {
                    let v = bld(self.builder.build_load(elem_llvm, slot, "vpop"))?;
                    bld(self.builder.build_store(tmp, v))?;
                }
                bld(self.builder.build_unconditional_branch(join_bb))?;
                self.builder.position_at_end(join_bb);
                if is_agg(elem) {
                    Ok(Some(GenValue::Agg(tmp)))
                } else {
                    let v = bld(self.builder.build_load(elem_llvm, tmp, "vpop_res"))?;
                    Ok(Some(GenValue::Scalar(v)))
                }
            }
            "len" => {
                if !args.is_empty() {
                    return Err(err("`len` takes no arguments".to_string()));
                }
                let len = bld(self.builder.build_load(i64t, len_slot, "vlen"))?;
                Ok(Some(GenValue::Scalar(len)))
            }
            "is_empty" => {
                if !args.is_empty() {
                    return Err(err("`is_empty` takes no arguments".to_string()));
                }
                let len = bld(self.builder.build_load(i64t, len_slot, "vlen"))?
                    .into_int_value();
                let z = bld(self.builder.build_int_compare(
                    IntPredicate::EQ,
                    len,
                    i64t.const_zero(),
                    "vis_empty",
                ))?;
                Ok(Some(GenValue::Scalar(z.into())))
            }
            "get" => {
                if args.len() != 1 {
                    return Err(err("`get` requires 1 argument (the index)".to_string()));
                }
                let idx = self.gen_value(&args[0])?.scalar(span, "get index")?;
                let idx = self.coerce(idx, &i64t.into(), span, "get index")?;
                let len = bld(self.builder.build_load(i64t, len_slot, "vlen"))?
                    .into_int_value();
                // Bounds guard: only touch the buffer when 0 <= idx < len. Out-of-range
                // (including negative indexes and empty Vec) returns a zeroed element
                // instead of dereferencing the (possibly null) data pointer.
                let idx_ge0 = bld(self.builder.build_int_compare(
                    IntPredicate::SGE,
                    idx.into_int_value(),
                    i64t.const_zero(),
                    "vidx_ge0",
                ))?;
                let idx_ltlen = bld(self.builder.build_int_compare(
                    IntPredicate::SLT,
                    idx.into_int_value(),
                    len,
                    "vidx_ltlen",
                ))?;
                let in_bounds = bld(self.builder.build_and(idx_ge0, idx_ltlen, "vin_bounds"))?;
                // Shared result slot, zero-initialized so the OOB path returns a default.
                let res = bld(self.builder.build_alloca(elem_llvm, "vec_get_res"))?;
                let res8 = bld(self.builder.build_pointer_cast(
                    res,
                    i8_ptr,
                    "vec_get_res8",
                ))?;
                bld(self.builder.build_call(
                    self.memset,
                    &[
                        res8.into(),
                        self.i32_ty.const_zero().into(),
                        i64t.const_int(elem_size, false).into(),
                    ],
                    "vec_get_zero",
                ))?;
                let hit_bb = self.context.append_basic_block(self.cur_func, "vec_get_hit");
                let skip_bb = self.context.append_basic_block(self.cur_func, "vec_get_skip");
                let join_bb = self.context.append_basic_block(self.cur_func, "vec_get_join");
                bld(self.builder.build_conditional_branch(in_bounds, hit_bb, skip_bb))?;
                self.builder.position_at_end(hit_bb);
                {
                    let data = bld(self.builder.build_load(i8_ptr, data_slot, "vdata"))?
                        .into_pointer_value();
                    let data_elems = bld(self.builder.build_pointer_cast(
                        data,
                        elem_llvm.ptr_type(AddressSpace::from(0u16)),
                        "vec_data_elems",
                    ))?;
                    let slot = bld(unsafe {
                        self.builder.build_in_bounds_gep(elem_llvm, data_elems, &[idx.into_int_value()], "velem")
                    })?;
                    if is_agg(elem) {
                        self.copy_agg(res, slot, elem, span, "get element")?;
                    } else {
                        let v = bld(self.builder.build_load(elem_llvm, slot, "vget"))?;
                        bld(self.builder.build_store(res, v))?;
                    }
                }
                bld(self.builder.build_unconditional_branch(join_bb))?;
                self.builder.position_at_end(skip_bb);
                bld(self.builder.build_unconditional_branch(join_bb))?;
                self.builder.position_at_end(join_bb);
                if is_agg(elem) {
                    Ok(Some(GenValue::Agg(res)))
                } else {
                    let v = bld(self.builder.build_load(elem_llvm, res, "vget"))?;
                    Ok(Some(GenValue::Scalar(v)))
                }
            }
            "set" => {
                if args.len() != 2 {
                    return Err(err("`set` requires 2 arguments (index, element)".to_string()));
                }
                let idx = self.gen_value(&args[0])?.scalar(span, "set index")?;
                let idx = self.coerce(idx, &i64t.into(), span, "set index")?;
                let v = self.gen_value(&args[1])?;
                let len = bld(self.builder.build_load(i64t, len_slot, "vlen"))?
                    .into_int_value();
                // Bounds guard: out-of-range writes (negative index, past end, empty Vec)
                // are ignored rather than corrupting memory.
                let idx_ge0 = bld(self.builder.build_int_compare(
                    IntPredicate::SGE,
                    idx.into_int_value(),
                    i64t.const_zero(),
                    "vidx_ge0",
                ))?;
                let idx_ltlen = bld(self.builder.build_int_compare(
                    IntPredicate::SLT,
                    idx.into_int_value(),
                    len,
                    "vidx_ltlen",
                ))?;
                let in_bounds = bld(self.builder.build_and(idx_ge0, idx_ltlen, "vin_bounds"))?;
                let hit_bb = self.context.append_basic_block(self.cur_func, "vec_set_hit");
                let skip_bb = self.context.append_basic_block(self.cur_func, "vec_set_skip");
                let join_bb = self.context.append_basic_block(self.cur_func, "vec_set_join");
                bld(self.builder.build_conditional_branch(in_bounds, hit_bb, skip_bb))?;
                self.builder.position_at_end(hit_bb);
                {
                    let data = bld(self.builder.build_load(i8_ptr, data_slot, "vdata"))?
                        .into_pointer_value();
                    let data_elems = bld(self.builder.build_pointer_cast(
                        data,
                        elem_llvm.ptr_type(AddressSpace::from(0u16)),
                        "vec_data_elems",
                    ))?;
                    let slot = bld(unsafe {
                        self.builder.build_in_bounds_gep(elem_llvm, data_elems, &[idx.into_int_value()], "velem")
                    })?;
                    if is_agg(elem) {
                        let src = v.agg(span, "set element")?;
                        self.copy_agg(slot, src, elem, span, "set element")?;
                    } else {
                        let vv = v.scalar(span, "set element")?;
                        let vv = self.coerce(vv, &elem_llvm, span, "set element")?;
                        bld(self.builder.build_store(slot, vv))?;
                    }
                }
                bld(self.builder.build_unconditional_branch(join_bb))?;
                self.builder.position_at_end(skip_bb);
                bld(self.builder.build_unconditional_branch(join_bb))?;
                self.builder.position_at_end(join_bb);
                Ok(None)
            }
            "free" => {
                if !args.is_empty() {
                    return Err(err("`free` takes no arguments".to_string()));
                }
                let data = bld(self.builder.build_load(i8_ptr, data_slot, "vdata"))?;
                bld(self.builder.build_call(self.free, &[data.into()], "vec_free"))?;
                bld(self.builder.build_store(data_slot, i8_ptr.const_null()))?;
                bld(self.builder.build_store(len_slot, i64t.const_zero()))?;
                bld(self.builder.build_store(cap_slot, i64t.const_zero()))?;
                Ok(None)
            }
            other => Err(err(format!(
                "`Vec` has no method `{other}` (supported: push/pop/len/get/set/free/is_empty)"
            ))),
        }
    }

    /// Grow a String's buffer so its capacity is at least `needed` bytes. Existing
    /// content (min(len, new_cap)) is migrated; a null old buffer (after `free`) is
    /// treated as empty. The builder is left at the join block.
    fn grow_string(
        &mut self,
        data_slot: PointerValue<'ctx>,
        len_slot: PointerValue<'ctx>,
        cap_slot: PointerValue<'ctx>,
        needed: IntValue<'ctx>,
        span: Span,
    ) -> Result<(), CodegenError> {
        let i64t = self.i64_ty;
        let i8_ptr = self.context.ptr_type(AddressSpace::from(0u16));
        let cap = bld(self.builder.build_load(i64t, cap_slot, "scap"))?.into_int_value();
        let len = bld(self.builder.build_load(i64t, len_slot, "slen"))?.into_int_value();
        let grow_needed = bld(self.builder.build_int_compare(
            IntPredicate::UGT,
            needed,
            cap,
            "sneed_grow",
        ))?;
        let grow_bb = self.context.append_basic_block(self.cur_func, "str_grow");
        let join_bb = self.context.append_basic_block(self.cur_func, "str_grow_join");
        bld(self.builder.build_conditional_branch(grow_needed, grow_bb, join_bb))?;
        self.builder.position_at_end(grow_bb);
        // new_cap = max(needed, cap * 2)
        let cap2 = bld(self.builder.build_int_mul(cap, i64t.const_int(2, false), "scap2"))?;
        let needed_gt_cap2 = bld(self.builder.build_int_compare(
            IntPredicate::UGT,
            needed,
            cap2,
            "sneed_gt",
        ))?;
        let new_cap = bld(self.builder.build_select::<IntValue, IntValue>(
            needed_gt_cap2,
            needed,
            cap2,
            "snewcap",
        ))?;
        let new_data = bld(self.builder.build_call(self.malloc, &[new_cap.into()], "str_malloc"))?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| self.internal_err(span, "malloc returned no value"))?;
        let old_data = bld(self.builder.build_load(i8_ptr, data_slot, "sdata"))?
            .into_pointer_value();
        let old_null = bld(self.builder.build_is_null(old_data, "sdata_null"))?;
        let skip_bb = self.context.append_basic_block(self.cur_func, "str_copy_skip");
        let do_bb = self.context.append_basic_block(self.cur_func, "str_copy_do");
        let copy_join_bb = self.context.append_basic_block(self.cur_func, "str_copy_join");
        bld(self.builder.build_conditional_branch(old_null, skip_bb, do_bb))?;
        self.builder.position_at_end(do_bb);
        bld(self.builder.build_call(
            self.memcpy,
            &[new_data.into(), old_data.into(), len.into()],
            "str_memcpy",
        ))?;
        bld(self.builder.build_call(self.free, &[old_data.into()], "str_free_old"))?;
        bld(self.builder.build_unconditional_branch(copy_join_bb))?;
        self.builder.position_at_end(skip_bb);
        bld(self.builder.build_unconditional_branch(copy_join_bb))?;
        self.builder.position_at_end(copy_join_bb);
        bld(self.builder.build_store(data_slot, new_data))?;
        bld(self.builder.build_store(cap_slot, new_cap))?;
        bld(self.builder.build_unconditional_branch(join_bb))?;
        self.builder.position_at_end(join_bb);
        Ok(())
    }

    /// Native `Box<T>` methods, implemented inline against the heap pointer.
    /// `deref` loads the boxed value and returns it; `free` releases the allocation.
    fn gen_box_method(
        &mut self,
        recv: &HirExpr,
        inner: &Ty,
        method: &str,
        args: &[HirExpr],
        span: Span,
    ) -> Result<Option<GenValue<'ctx>>, CodegenError> {
        let recv_val = self.gen_value(recv)?;
        let box_ptr = recv_val.scalar(span, "Box method receiver")?;
        let ptr = box_ptr.into_pointer_value();
        let err = |msg: String| CodegenError {
            msg,
            line: span.line,
            col: span.col,
        };
        match method {
            "deref" => {
                if !args.is_empty() {
                    return Err(err("`deref` takes no arguments".to_string()));
                }
                let inner_llvm = self.t(inner, span)?;
                let slot = bld(self.builder.build_pointer_cast(
                    ptr,
                    self.context.ptr_type(AddressSpace::from(0u16)),
                    "box_deref_cast",
                ))?;
                let v = bld(self.builder.build_load(inner_llvm, slot, "box_deref"))?;
                // Aggregate contents are returned through an alloca slot.
                if is_agg(inner) {
                    let tmp = bld(self.builder.build_alloca(inner_llvm, "box_deref_tmp"))?;
                    bld(self.builder.build_store(tmp, v))?;
                    Ok(Some(GenValue::Agg(tmp)))
                } else {
                    Ok(Some(GenValue::Scalar(v)))
                }
            }
            "free" => {
                if !args.is_empty() {
                    return Err(err("`free` takes no arguments".to_string()));
                }
                bld(self.builder.build_call(self.free, &[ptr.into()], "box_free"))?;
                Ok(None)
            }
            other => Err(err(format!(
                "`Box` has no method `{other}` (supported: deref/free)"
            ))),
        }
    }

    /// Native `String` methods: implemented inline against the heap struct
    /// `{ data: i8*, len: i64, cap: i64 }` (NUL-terminated at `data[len]`).
    fn gen_string_method(
        &mut self,
        recv: &HirExpr,
        method: &str,
        args: &[HirExpr],
        span: Span,
    ) -> Result<Option<GenValue<'ctx>>, CodegenError> {
        let recv_val = self.gen_value(recv)?;
        let str_ptr = recv_val.agg(span, "String method receiver")?;
        let str_llvm = self.t(&Ty::String, span)?;
        let i64t = self.i64_ty;
        let i8_ptr = self.context.ptr_type(AddressSpace::from(0u16));
        let i8t = self.context.i8_type();
        let zero = self.i32_ty.const_zero();
        let one = self.i32_ty.const_int(1, false);
        let two = self.i32_ty.const_int(2, false);
        let data_slot = bld(unsafe {
            self.builder.build_in_bounds_gep(str_llvm, str_ptr, &[zero, zero], "str.data")
        })?;
        let len_slot = bld(unsafe {
            self.builder.build_in_bounds_gep(str_llvm, str_ptr, &[zero, one], "str.len")
        })?;
        let cap_slot = bld(unsafe {
            self.builder.build_in_bounds_gep(str_llvm, str_ptr, &[zero, two], "str.cap")
        })?;
        let err = |msg: String| CodegenError {
            msg,
            line: span.line,
            col: span.col,
        };
        match method {
            "push" => {
                if args.len() != 1 {
                    return Err(err("`push` requires 1 argument (the byte to append)".to_string()));
                }
                let v = self.gen_value(&args[0])?.scalar(span, "push byte")?;
                let v = self.coerce(v, &i64t.into(), span, "push byte")?.into_int_value();
                let len = bld(self.builder.build_load(i64t, len_slot, "slen"))?.into_int_value();
                // Need room for the new byte plus the terminator.
                let needed = bld(self.builder.build_int_add(
                    len,
                    i64t.const_int(2, false),
                    "sneed",
                ))?;
                self.grow_string(data_slot, len_slot, cap_slot, needed, span)?;
                let data = bld(self.builder.build_load(i8_ptr, data_slot, "sdata"))?
                    .into_pointer_value();
                let slot = bld(unsafe {
                    self.builder.build_in_bounds_gep(i8t, data, &[len], "sbyte")
                })?;
                let v8 = bld(self.builder.build_int_truncate(v, i8t, "sbyte_trunc"))?;
                bld(self.builder.build_store(slot, v8))?;
                let len1 = bld(self.builder.build_int_add(
                    len,
                    i64t.const_int(1, false),
                    "slen1",
                ))?;
                let term = bld(unsafe {
                    self.builder.build_in_bounds_gep(i8t, data, &[len1], "sterm")
                })?;
                bld(self.builder.build_store(term, i8t.const_zero()))?;
                let new_len = bld(self.builder.build_int_add(
                    len,
                    i64t.const_int(1, false),
                    "slenp",
                ))?;
                bld(self.builder.build_store(len_slot, new_len))?;
                Ok(None)
            }
            "push_str" => {
                if args.len() != 1 {
                    return Err(err("`push_str` requires 1 argument (the string to append)".to_string()));
                }
                let s = self.gen_value(&args[0])?.scalar(span, "push_str argument")?;
                let s = s.into_pointer_value();
                let slen = bld(self.builder.build_call(self.strlen, &[s.into()], "spush_len"))?
                    .try_as_basic_value()
                    .basic()
                    .ok_or_else(|| self.internal_err(span, "strlen returned no value"))?
                    .into_int_value();
                let len = bld(self.builder.build_load(i64t, len_slot, "slen"))?.into_int_value();
                // Need room for the appended bytes plus the terminator.
                let len_plus = bld(self.builder.build_int_add(len, slen, "sneed0"))?;
                let needed = bld(self.builder.build_int_add(
                    len_plus,
                    i64t.const_int(1, false),
                    "sneed",
                ))?;
                self.grow_string(data_slot, len_slot, cap_slot, needed, span)?;
                let data = bld(self.builder.build_load(i8_ptr, data_slot, "sdata"))?
                    .into_pointer_value();
                let dest = bld(unsafe {
                    self.builder.build_in_bounds_gep(i8t, data, &[len], "sdest")
                })?;
                bld(self.builder.build_call(
                    self.memcpy,
                    &[dest.into(), s.into(), slen.into()],
                    "spush_copy",
                ))?;
                let new_len = bld(self.builder.build_int_add(len, slen, "slenp"))?;
                let term = bld(unsafe {
                    self.builder.build_in_bounds_gep(i8t, data, &[new_len], "sterm")
                })?;
                bld(self.builder.build_store(term, i8t.const_zero()))?;
                bld(self.builder.build_store(len_slot, new_len))?;
                Ok(None)
            }
            "pop" => {
                if !args.is_empty() {
                    return Err(err("`pop` takes no arguments".to_string()));
                }
                let len = bld(self.builder.build_load(i64t, len_slot, "slen"))?.into_int_value();
                let empty = bld(self.builder.build_int_compare(
                    IntPredicate::EQ,
                    len,
                    i64t.const_zero(),
                    "spop_empty",
                ))?;
                let tmp = bld(self.builder.build_alloca(i64t, "str_pop_tmp"))?;
                let empty_bb = self.context.append_basic_block(self.cur_func, "str_pop_empty");
                let load_bb = self.context.append_basic_block(self.cur_func, "str_pop_load");
                let join_bb = self.context.append_basic_block(self.cur_func, "str_pop_join");
                bld(self.builder.build_conditional_branch(empty, empty_bb, load_bb))?;
                self.builder.position_at_end(empty_bb);
                bld(self.builder.build_store(tmp, i64t.const_zero()))?;
                bld(self.builder.build_unconditional_branch(join_bb))?;
                self.builder.position_at_end(load_bb);
                let new_len = bld(self.builder.build_int_sub(
                    len,
                    i64t.const_int(1, false),
                    "slenm1",
                ))?;
                let data = bld(self.builder.build_load(i8_ptr, data_slot, "sdata"))?
                    .into_pointer_value();
                let slot = bld(unsafe {
                    self.builder.build_in_bounds_gep(i8t, data, &[new_len], "sbyte")
                })?;
                let v8 = bld(self.builder.build_load(i8t, slot, "spop"))?;
                let v64 = bld(self.builder.build_int_s_extend(v8.into_int_value(), i64t, "spop_sext"))?;
                bld(self.builder.build_store(tmp, v64))?;
                bld(self.builder.build_store(slot, i8t.const_zero()))?;
                bld(self.builder.build_store(len_slot, new_len))?;
                bld(self.builder.build_unconditional_branch(join_bb))?;
                self.builder.position_at_end(join_bb);
                let res = bld(self.builder.build_load(i64t, tmp, "spop_res"))?;
                Ok(Some(GenValue::Scalar(res)))
            }
            "utf8_push" => {
                if args.len() != 1 {
                    return Err(err("`utf8_push` requires 1 argument (the code point to append)".to_string()));
                }
                let v = self.gen_value(&args[0])?.scalar(span, "utf8_push code point")?;
                let v = self.coerce(v, &i64t.into(), span, "utf8_push code point")?.into_int_value();
                let len = bld(self.builder.build_load(i64t, len_slot, "slen"))?.into_int_value();
                // A code point encodes to at most 4 bytes; reserve room for it plus the
                // terminator before encoding (the helper writes at the buffer's base).
                let needed = bld(self.builder.build_int_add(
                    len,
                    i64t.const_int(5, false),
                    "u8push_need",
                ))?;
                self.grow_string(data_slot, len_slot, cap_slot, needed, span)?;
                let data = bld(self.builder.build_load(i8_ptr, data_slot, "sdata"))?
                    .into_pointer_value();
                // Encode at the current end: pass `data + len` as the write base.
                let target = bld(unsafe {
                    self.builder.build_in_bounds_gep(i8t, data, &[len], "u8push_target")
                })?;
                let written = bld(self.builder.build_call(
                    self.utf8_push_f,
                    &[target.into(), v.into()],
                    "u8push",
                ))?
                .try_as_basic_value()
                .basic()
                .ok_or_else(|| self.internal_err(span, "aero_utf8_push returned no value"))?
                .into_int_value();
                // Only advance len when the code point was valid (bytes written > 0).
                let valid = bld(self.builder.build_int_compare(
                    IntPredicate::SGT,
                    written,
                    i64t.const_zero(),
                    "u8push_valid",
                ))?;
                let ok_bb = self.context.append_basic_block(self.cur_func, "u8push_ok");
                let join_bb = self.context.append_basic_block(self.cur_func, "u8push_join");
                bld(self.builder.build_conditional_branch(valid, ok_bb, join_bb))?;
                self.builder.position_at_end(ok_bb);
                let new_len = bld(self.builder.build_int_add(len, written, "u8push_len"))?;
                let term = bld(unsafe {
                    self.builder.build_in_bounds_gep(i8t, data, &[new_len], "u8push_term")
                })?;
                bld(self.builder.build_store(term, i8t.const_zero()))?;
                bld(self.builder.build_store(len_slot, new_len))?;
                bld(self.builder.build_unconditional_branch(join_bb))?;
                self.builder.position_at_end(join_bb);
                Ok(None)
            }
            "utf8_pop" => {
                if !args.is_empty() {
                    return Err(err("`utf8_pop` takes no arguments".to_string()));
                }
                let len = bld(self.builder.build_load(i64t, len_slot, "slen"))?.into_int_value();
                let empty = bld(self.builder.build_int_compare(
                    IntPredicate::EQ,
                    len,
                    i64t.const_zero(),
                    "u8pop_empty",
                ))?;
                let tmp = bld(self.builder.build_alloca(i64t, "utf8_pop_tmp"))?;
                let out_len = bld(self.builder.build_alloca(i64t, "utf8_pop_outlen"))?;
                let empty_bb = self.context.append_basic_block(self.cur_func, "u8pop_empty");
                let load_bb = self.context.append_basic_block(self.cur_func, "u8pop_load");
                let join_bb = self.context.append_basic_block(self.cur_func, "u8pop_join");
                bld(self.builder.build_conditional_branch(empty, empty_bb, load_bb))?;
                self.builder.position_at_end(empty_bb);
                bld(self.builder.build_store(tmp, i64t.const_int(u64::MAX, false)))?;
                bld(self.builder.build_unconditional_branch(join_bb))?;
                self.builder.position_at_end(load_bb);
                let data = bld(self.builder.build_load(i8_ptr, data_slot, "sdata"))?
                    .into_pointer_value();
                let cp = bld(self.builder.build_call(
                    self.utf8_pop_f,
                    &[data.into(), len.into(), out_len.into()],
                    "u8pop",
                ))?
                .try_as_basic_value()
                .basic()
                .ok_or_else(|| self.internal_err(span, "aero_utf8_pop returned no value"))?
                .into_int_value();
                let removed = bld(self.builder.build_load(i64t, out_len, "u8pop_removed"))?
                    .into_int_value();
                let new_len = bld(self.builder.build_int_sub(len, removed, "u8pop_len"))?;
                let term = bld(unsafe {
                    self.builder.build_in_bounds_gep(i8t, data, &[new_len], "u8pop_term")
                })?;
                bld(self.builder.build_store(term, i8t.const_zero()))?;
                bld(self.builder.build_store(len_slot, new_len))?;
                bld(self.builder.build_store(tmp, cp))?;
                bld(self.builder.build_unconditional_branch(join_bb))?;
                self.builder.position_at_end(join_bb);
                let res = bld(self.builder.build_load(i64t, tmp, "u8pop_res"))?;
                Ok(Some(GenValue::Scalar(res)))
            }
            "len" => {
                if !args.is_empty() {
                    return Err(err("`len` takes no arguments".to_string()));
                }
                let len = bld(self.builder.build_load(i64t, len_slot, "slen"))?;
                Ok(Some(GenValue::Scalar(len)))
            }
            "is_empty" => {
                if !args.is_empty() {
                    return Err(err("`is_empty` takes no arguments".to_string()));
                }
                let len = bld(self.builder.build_load(i64t, len_slot, "slen"))?
                    .into_int_value();
                let z = bld(self.builder.build_int_compare(
                    IntPredicate::EQ,
                    len,
                    i64t.const_zero(),
                    "sis_empty",
                ))?;
                Ok(Some(GenValue::Scalar(z.into())))
            }
            "clear" => {
                if !args.is_empty() {
                    return Err(err("`clear` takes no arguments".to_string()));
                }
                let data = bld(self.builder.build_load(i8_ptr, data_slot, "sdata"))?
                    .into_pointer_value();
                bld(self.builder.build_store(data, i8t.const_zero()))?;
                bld(self.builder.build_store(len_slot, i64t.const_zero()))?;
                Ok(None)
            }
            "at" => {
                if args.len() != 1 {
                    return Err(err("`at` requires 1 argument (the index)".to_string()));
                }
                let idx = self.gen_value(&args[0])?.scalar(span, "at index")?;
                let idx = self.coerce(idx, &i64t.into(), span, "at index")?.into_int_value();
                let data = bld(self.builder.build_load(i8_ptr, data_slot, "sdata"))?
                    .into_pointer_value();
                let slot = bld(unsafe {
                    self.builder.build_in_bounds_gep(i8t, data, &[idx], "sbyte")
                })?;
                let v8 = bld(self.builder.build_load(i8t, slot, "sat"))?;
                let v64 = bld(self.builder.build_int_s_extend(v8.into_int_value(), i64t, "sat_sext"))?;
                Ok(Some(GenValue::Scalar(v64.into())))
            }
            "data" => {
                if !args.is_empty() {
                    return Err(err("`data` takes no arguments".to_string()));
                }
                let data = bld(self.builder.build_load(i8_ptr, data_slot, "sdata"))?;
                Ok(Some(GenValue::Scalar(data)))
            }
            "free" => {
                if !args.is_empty() {
                    return Err(err("`free` takes no arguments".to_string()));
                }
                let data = bld(self.builder.build_load(i8_ptr, data_slot, "sdata"))?;
                bld(self.builder.build_call(self.free, &[data.into()], "str_free"))?;
                bld(self.builder.build_store(data_slot, i8_ptr.const_null()))?;
                bld(self.builder.build_store(len_slot, i64t.const_zero()))?;
                bld(self.builder.build_store(cap_slot, i64t.const_zero()))?;
                Ok(None)
            }
            "starts_with" => {
                if args.len() != 1 {
                    return Err(err("`starts_with` requires 1 argument (the prefix string)".to_string()));
                }
                let p = self.gen_value(&args[0])?.scalar(span, "starts_with prefix")?;
                let p = p.into_pointer_value();
                let data = bld(self.builder.build_load(i8_ptr, data_slot, "sdata"))?;
                let plen = bld(self.builder.build_call(self.strlen, &[p.into()], "sw_plen"))?
                    .try_as_basic_value()
                    .basic()
                    .ok_or_else(|| self.internal_err(span, "strlen returned no value"))?
                    .into_int_value();
                let cmp = bld(self.builder.build_call(
                    self.memcmp,
                    &[data.into(), p.into(), plen.into()],
                    "sw_cmp",
                ))?;
                let cmp = cmp.try_as_basic_value().basic().unwrap().into_int_value();
                let eq = bld(self.builder.build_int_compare(IntPredicate::EQ, cmp, i64t.const_zero(), "sw_eq"))?;
                Ok(Some(GenValue::Scalar(eq.into())))
            }
            "ends_with" => {
                if args.len() != 1 {
                    return Err(err("`ends_with` requires 1 argument (the suffix string)".to_string()));
                }
                let suf = self.gen_value(&args[0])?.scalar(span, "ends_with suffix")?;
                let suf = suf.into_pointer_value();
                let data = bld(self.builder.build_load(i8_ptr, data_slot, "sdata"))?;
                let data = data.into_pointer_value();
                let len = bld(self.builder.build_load(i64t, len_slot, "slen"))?.into_int_value();
                let slen = bld(self.builder.build_call(self.strlen, &[suf.into()], "ew_slen"))?
                    .try_as_basic_value()
                    .basic()
                    .ok_or_else(|| self.internal_err(span, "strlen returned no value"))?
                    .into_int_value();
                let too_short = bld(self.builder.build_int_compare(IntPredicate::ULT, len, slen, "ew_short"))?;
                let tmp = bld(self.builder.build_alloca(i64t, "ew_tmp"))?;
                let short_bb = self.context.append_basic_block(self.cur_func, "ew_short");
                let cmp_bb = self.context.append_basic_block(self.cur_func, "ew_cmp");
                let join_bb = self.context.append_basic_block(self.cur_func, "ew_join");
                bld(self.builder.build_conditional_branch(too_short, short_bb, cmp_bb))?;
                self.builder.position_at_end(short_bb);
                bld(self.builder.build_store(tmp, i64t.const_int(1, false)))?; // 1 = not equal
                bld(self.builder.build_unconditional_branch(join_bb))?;
                self.builder.position_at_end(cmp_bb);
                let offset = bld(self.builder.build_int_sub(len, slen, "ew_off"))?;
                let tail = bld(unsafe {
                    self.builder.build_in_bounds_gep(i8t, data, &[offset], "ew_tail")
                })?;
                let cmp = bld(self.builder.build_call(
                    self.memcmp,
                    &[tail.into(), suf.into(), slen.into()],
                    "ew_cmp",
                ))?;
                let cmp = cmp.try_as_basic_value().basic().unwrap().into_int_value();
                // memcmp returns i32; sign-extend to i64 to match the `tmp` slot type.
                let cmp64 = bld(self.builder.build_int_s_extend(cmp, i64t, "ew_cmp64"))?;
                bld(self.builder.build_store(tmp, cmp64))?;
                bld(self.builder.build_unconditional_branch(join_bb))?;
                self.builder.position_at_end(join_bb);
                let res = bld(self.builder.build_load(i64t, tmp, "ew_res"))?;
                let res = res.into_int_value();
                let eq = bld(self.builder.build_int_compare(IntPredicate::EQ, res, i64t.const_zero(), "ew_eq"))?;
                Ok(Some(GenValue::Scalar(eq.into())))
            }
            other => Err(err(format!(
                "`String` has no method `{other}` (supported: push/push_str/utf8_push/pop/utf8_pop/len/is_empty/clear/at/data/starts_with/ends_with/free)"
            ))),
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
        // The unconditional branch to `merge_bb` is emitted from the *current*
        // insertion block, which is `rhs_bb` for a simple rhs but becomes the
        // inner merge block when `rhs` is itself a nested short-circuit (e.g.
        // `a && (b || c)`). That block — not `rhs_bb` — is the real predecessor
        // of `merge_bb`, so it must be the phi's incoming edge or the SSA form
        // is invalid (missing/extra predecessors) and MCJIT crashes on it.
        let rhs_exit = self
            .builder
            .get_insert_block()
            .expect("an insertion block must exist before a conditional branch");
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
            (&r.as_basic_value_enum(), rhs_exit),
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
        // Push loop context for break/continue (continue → cond, break → merge)
        self.loop_stack.push((cond_bb, merge_bb));
        self.gen_block(body)?;
        self.loop_stack.pop();
        if !self.cur_block_terminated() {
            bld(self.builder.build_unconditional_branch(cond_bb))?;
        }
        self.builder.position_at_end(merge_bb);
        Ok(())
    }

    /// Generate `loop { ... }` (infinite loop). `continue` jumps to the loop
    /// header, `break` jumps to the merge block.
    fn gen_loop(&mut self, body: &HirBlock) -> Result<(), CodegenError> {
        let body_bb = self.context.append_basic_block(self.cur_func, "loop_body");
        let merge_bb = self.context.append_basic_block(self.cur_func, "loop_merge");

        bld(self.builder.build_unconditional_branch(body_bb))?;
        self.builder.position_at_end(body_bb);
        self.loop_stack.push((body_bb, merge_bb));
        self.gen_block(body)?;
        self.loop_stack.pop();
        if !self.cur_block_terminated() {
            bld(self.builder.build_unconditional_branch(body_bb))?;
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

    /// Emit a `memcpy(dst, src, n)` call; the pointers are cast to `i8*` so the
    /// byte buffer (e.g. an enum payload) can be copied between differently typed slots.
    fn emit_memcpy(
        &mut self,
        dst: PointerValue<'ctx>,
        src: PointerValue<'ctx>,
        n_bytes: u64,
        span: Span,
        what: &str,
    ) -> Result<(), CodegenError> {
        let i8_ptr = self.context.ptr_type(AddressSpace::from(0u16));
        let dst8 = bld(self.builder.build_pointer_cast(dst, i8_ptr, "mem_dst"))?;
        let src8 = bld(self.builder.build_pointer_cast(src, i8_ptr, "mem_src"))?;
        let n = self.i64_ty.const_int(n_bytes, false);
        bld(self.builder.build_call(self.memcpy, &[dst8.into(), src8.into(), n.into()], what))?;
        let _ = span;
        Ok(())
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

    /// Auto-format specifier for a single `print`/`format` argument by type.
    fn auto_fmt(ty: &Ty) -> &'static str {
        if *ty == Ty::Str || *ty == Ty::String || matches!(ty, Ty::Ptr(_)) {
            "%s"
        } else if *ty == Ty::F32 || *ty == Ty::F64 {
            "%f"
        } else if *ty == Ty::Char {
            "%c"
        } else {
            "%lld"
        }
    }

    /// Marshal one non-literal `print`/`format` argument by type: `String`
    /// passes its NUL-terminated data pointer (`%s`); floats pass as f64;
    /// chars pass as i32; `str` passes as-is; other scalars coerce to i64.
    fn marshal_print_value(
        &mut self,
        expr: &HirExpr,
        ty: &Ty,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let span = expr.span();
        if *ty == Ty::String {
            let v = self.gen_value(expr)?;
            let ptr = v.agg(span, "print")?;
            let str_llvm = self.t(&Ty::String, span)?;
            let data_slot = bld(unsafe {
                self.builder.build_in_bounds_gep(
                    str_llvm,
                    ptr,
                    &[self.i32_ty.const_zero(), self.i32_ty.const_zero()],
                    "str.data",
                )
            })?;
            let data = bld(self.builder.build_load(
                self.context.ptr_type(AddressSpace::from(0u16)),
                data_slot,
                "sdata",
            ))?;
            return Ok(data);
        }
        let v = self.gen_value(expr)?.scalar(span, "print")?;
        if *ty == Ty::F32 || *ty == Ty::F64 {
            let vf = self.coerce(v, &self.context.f64_type().into(), span, "print")?;
            return Ok(vf);
        }
        if *ty == Ty::Char {
            let v32 = self.coerce(v, &self.i32_ty.into(), span, "print")?;
            return Ok(v32);
        }
        if *ty == Ty::Str || matches!(*ty, Ty::Ptr(_)) {
            return Ok(v);
        }
        let v64 = self.coerce(v, &self.i64_ty.into(), span, "print")?;
        Ok(v64)
    }

    /// Marshal `print`/`format` variadic arguments into a printf-style call
    /// argument list (format pointer first). A single non-literal argument is
    /// auto-formatted by type (`newline` controls the trailing `\n`, mirroring
    /// `print`'s implicit newline); with multiple arguments the first must be a
    /// string literal used verbatim as the printf format.
    fn printf_style_args(
        &mut self,
        args: &[HirExpr],
        span: Span,
        newline: bool,
    ) -> Result<Vec<BasicMetadataValueEnum<'ctx>>, CodegenError> {
        let first = args.first().ok_or_else(|| CodegenError {
            msg: "requires at least one argument".to_string(),
            line: span.line,
            col: span.col,
        })?;

        // Single argument (non-literal): auto-format by type.
        if args.len() == 1 && !matches!(first, HirExpr::StrLit(..)) {
            let ty = self.expr_ty(first)?;
            let mut fmt = Self::auto_fmt(&ty).to_string();
            if newline {
                fmt.push('\n');
            }
            let fmt_ptr = bld(self.builder.build_global_string_ptr(&fmt, "fmt"))?;
            let v = self.marshal_print_value(first, &ty)?;
            return Ok(vec![fmt_ptr.as_pointer_value().into(), v.into()]);
        }

        // Otherwise: the first argument must be a string format string.
        let fmt = match first {
            HirExpr::StrLit(s, _) => s.clone(),
            other => {
                let sp = other.span();
                return Err(CodegenError {
                    msg: "multi-argument print/format requires the first argument to be a string format".to_string(),
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
                    let ty = self.expr_ty(other)?;
                    let v = self.marshal_print_value(other, &ty)?;
                    call_args.push(v.into());
                }
            }
        }
        Ok(call_args)
    }

    /// `format(fmt, args...) -> str` (stdlib formatting, Phase 11.2):
    /// printf-style formatting into a fresh malloc'd buffer. Two-pass:
    /// probe the needed length with `snprintf(NULL, 0, ...)`, allocate
    /// len+1 bytes, then format into the buffer. Arguments are evaluated
    /// once; the same SSA values feed both snprintf calls.
    fn gen_format(&mut self, args: &[HirExpr], span: Span) -> Result<GenValue<'ctx>, CodegenError> {
        let varargs = self.printf_style_args(args, span, false)?;
        let ptr_ty = self.context.ptr_type(AddressSpace::from(0u16));
        // Pass 1: probe the formatted length (snprintf(NULL, 0, fmt, ...)).
        let mut probe: Vec<BasicMetadataValueEnum<'ctx>> = Vec::with_capacity(varargs.len() + 2);
        probe.push(ptr_ty.const_null().into());
        probe.push(self.i64_ty.const_zero().into());
        probe.extend(varargs.iter().cloned());
        let need = bld(self.builder.build_call(self.snprintf, &probe, "fmt_len"))?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| self.internal_err(span, "snprintf returned no value"))?
            .into_int_value();
        let need64 = bld(self.builder.build_int_s_extend(need, self.i64_ty, "fmt_len64"))?;
        let size = bld(self.builder.build_int_add(
            need64,
            self.i64_ty.const_int(1, false),
            "fmt_size",
        ))?;
        // Pass 2: allocate and format into the buffer.
        let buf = bld(self.builder.build_call(self.malloc, &[size.into()], "fmt_buf"))?
            .try_as_basic_value()
            .basic()
            .ok_or_else(|| self.internal_err(span, "malloc returned no value"))?
            .into_pointer_value();
        let mut real: Vec<BasicMetadataValueEnum<'ctx>> = Vec::with_capacity(varargs.len() + 2);
        real.push(buf.into());
        real.push(size.into());
        real.extend(varargs.iter().cloned());
        bld(self.builder.build_call(self.snprintf, &real, "fmt_out"))?;
        Ok(GenValue::Scalar(buf.into()))
    }

    fn gen_print(&mut self, args: &[HirExpr], span: Span) -> Result<(), CodegenError> {
        let call_args = self.printf_style_args(args, span, true)?;
        bld(self.builder.build_call(self.printf, &call_args, "printf"))?;
        Ok(())
    }

    /// `str_hash(s) -> i64` (Phase 11.1): FNV-1a over the NUL-terminated
    /// bytes of `s` (the terminator itself is not folded in).
    ///
    /// ```c
    /// u64 h = 0xcbf29ce484222325;
    /// while (*p) { h ^= (u8)*p; h *= 0x100000001b3; p++; }
    /// ```
    fn gen_str_hash(&mut self, arg: &HirExpr, span: Span) -> Result<GenValue<'ctx>, CodegenError> {
        let s = self.gen_value(arg)?.scalar(span, "str_hash argument")?;
        let s = s.into_pointer_value();
        let i8t = self.context.i8_type();
        let i8_ptr = self.context.ptr_type(AddressSpace::from(0u16));
        let pre_bb = self
            .builder
            .get_insert_block()
            .ok_or_else(|| self.internal_err(span, "no insertion point"))?;
        let cond_bb = self.context.append_basic_block(self.cur_func, "hash_cond");
        let body_bb = self.context.append_basic_block(self.cur_func, "hash_body");
        let done_bb = self.context.append_basic_block(self.cur_func, "hash_done");
        bld(self.builder.build_unconditional_branch(cond_bb))?;

        // cond: p = phi [s, pre], [p_next, body]; h = phi [h0, pre], [h_next, body]
        self.builder.position_at_end(cond_bb);
        let p = self.builder.build_phi(i8_ptr, "hash_p").map_err(|e| {
            CodegenError {
                msg: format!("LLVM IR construction failed: {e}"),
                line: span.line,
                col: span.col,
            }
        })?;
        let h = self.builder.build_phi(self.i64_ty, "hash_h").map_err(|e| {
            CodegenError {
                msg: format!("LLVM IR construction failed: {e}"),
                line: span.line,
                col: span.col,
            }
        })?;
        let h0 = self.i64_ty.const_int(0xcbf29ce484222325, false);
        let b = bld(self.builder.build_load(i8t, p.as_basic_value().into_pointer_value(), "hb"))?
            .into_int_value();
        let is_nul = bld(self.builder.build_int_compare(
            IntPredicate::EQ,
            b,
            i8t.const_zero(),
            "hb_nul",
        ))?;
        bld(self.builder.build_conditional_branch(is_nul, done_bb, body_bb))?;

        // body: h' = (h ^ zext(b)) * FNV_PRIME; p' = p + 1
        self.builder.position_at_end(body_bb);
        let b64 = bld(self.builder.build_int_z_extend(b, self.i64_ty, "hb64"))?;
        let x = bld(self.builder.build_xor(
            h.as_basic_value().into_int_value(),
            b64,
            "hx",
        ))?;
        let prime = self.i64_ty.const_int(0x100000001b3, false);
        let h_next = bld(self.builder.build_int_mul(x, prime, "hnext"))?;
        let p_next = bld(unsafe {
            self.builder.build_in_bounds_gep(
                i8t,
                p.as_basic_value().into_pointer_value(),
                &[self.i64_ty.const_int(1, false)],
                "pnext",
            )
        })?;
        bld(self.builder.build_unconditional_branch(cond_bb))?;

        // done: result = h (wire up both phis' incoming edges)
        self.builder.position_at_end(done_bb);
        p.add_incoming(&[(&s, pre_bb), (&p_next, body_bb)]);
        let h0v: IntValue = h0;
        let hnv: IntValue = h_next;
        h.add_incoming(&[(&h0v, pre_bb), (&hnv, body_bb)]);
        Ok(GenValue::Scalar(h.as_basic_value()))
    }

    /// `hash_i64(x) -> i64` (Phase 11.1): splitmix64 finalizer — good avalanche
    /// mixing for integer keys.
    fn gen_hash_i64(&mut self, arg: &HirExpr, span: Span) -> Result<GenValue<'ctx>, CodegenError> {
        let x = self.gen_value(arg)?.scalar(span, "hash_i64 argument")?;
        let x = self.coerce(x, &self.i64_ty.into(), span, "hash_i64 argument")?;
        let x = x.into_int_value();
        let sixty = self.i64_ty.const_int(30, false);
        let twoseven = self.i64_ty.const_int(27, false);
        let thirtyone = self.i64_ty.const_int(31, false);
        // z = x + GOLDEN
        let z = bld(self.builder.build_int_add(
            x,
            self.i64_ty.const_int(0x9E3779B97F4A7C15, false),
            "sm_z",
        ))?;
        let z1 = bld(self.builder.build_right_shift(z, sixty, false, "sm_z1s"))?;
        let z1 = bld(self.builder.build_xor(z, z1, "sm_z1x"))?;
        let z1 = bld(self.builder.build_int_mul(
            z1,
            self.i64_ty.const_int(0xBF58476D1CE4E5B9, false),
            "sm_z1m",
        ))?;
        let z2 = bld(self.builder.build_right_shift(z1, twoseven, false, "sm_z2s"))?;
        let z2 = bld(self.builder.build_xor(z1, z2, "sm_z2x"))?;
        let z2 = bld(self.builder.build_int_mul(
            z2,
            self.i64_ty.const_int(0x94D049BB133111EB, false),
            "sm_z2m",
        ))?;
        let z3 = bld(self.builder.build_right_shift(z2, thirtyone, false, "sm_z3s"))?;
        let res = bld(self.builder.build_xor(z2, z3, "sm_res"))?;
        Ok(GenValue::Scalar(res.into()))
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
            GenValue::Agg(p) => {
                // A `&T`/`&mut T` parameter wants the address of an aggregate value
                // (auto-borrow `f(v)` ⇒ `f(&v)`), so pass the slot pointer directly.
                if matches!(to, BasicTypeEnum::PointerType(_)) {
                    p.as_basic_value_enum()
                } else {
                    bld(self.builder.build_load(*to, p, "agg_arg"))?
                }
            }
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
        // Float-to-float conversion (f64 → f32 or f32 → f64)
        if let (BasicValueEnum::FloatValue(fv), BasicTypeEnum::FloatType(to_ty)) = (v, to) {
            let from_w = fv.get_type().get_bit_width();
            let to_w = to_ty.get_bit_width();
            let out = if from_w > to_w {
                bld(self.builder.build_float_trunc(fv, *to_ty, "ftrunc"))?
            } else {
                bld(self.builder.build_float_ext(fv, *to_ty, "fext"))?
            };
            return Ok(out.into());
        }
        // Int-to-float conversion (e.g. integer literal in a float context)
        if let (BasicValueEnum::IntValue(iv), BasicTypeEnum::FloatType(to_ty)) = (v, to) {
            let out = bld(self.builder.build_signed_int_to_float(iv, *to_ty, "sitofp"))?;
            return Ok(out.into());
        }
        // Float-to-int conversion
        if let (BasicValueEnum::FloatValue(fv), BasicTypeEnum::IntType(to_ty)) = (v, to) {
            let out = bld(self.builder.build_float_to_signed_int(fv, *to_ty, "fptosi"))?;
            return Ok(out.into());
        }
        // Pointer-to-pointer bitcast: a typed aggregate slot (e.g. `[16 x i32]*`
        // from an array variable/field decayed to `*i32`) needs a pointer cast
        // (same address, different pointee) — C array-to-pointer decay.
        if let (BasicValueEnum::PointerValue(pv), BasicTypeEnum::PointerType(to_p)) = (v, to) {
            if pv.get_type() != *to_p {
                let out = bld(self.builder.build_pointer_cast(pv, *to_p, "ptrcast"))?;
                return Ok(out.into());
            }
            return Ok(v);
        }
        // Integer-to-integer coercion (existing logic)
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
            BasicTypeEnum::PointerType(p) => {
                // Integer-to-pointer: C-FFI NULL (0) or reinterpreted-address arguments
                // (`sqlite3_exec(db, sql, 0, 0, 0)`), mirroring Rust's `0 as *mut T`.
                let out = bld(self.builder.build_int_to_ptr(iv, *p, "inttoptr"))?;
                return Ok(out.into());
            }
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

    /// Resolve a type through the current generic substitution context.
    /// Used to check whether a type is a float family member inside generic functions.
    fn resolve_ty(&self, ty: &Ty) -> Ty {
        substitute(ty, &self.type_subst)
    }

    /// Type hint for aggregate-literal elements (used for tuple temp slots).
    /// Literals use their own type; other expressions look up static types (generics via
    /// type_subst), so the temp-slot layout matches the target type.
    fn elem_ty_hint(&self, expr: &HirExpr) -> Ty {
        match expr {
            HirExpr::BoolLit(..) => Ty::Bool,
            HirExpr::StrLit(..) => Ty::Str,
            HirExpr::IntLit(..) => Ty::I64,
            HirExpr::FloatLit(..) => Ty::F64,
            HirExpr::CharLit(..) => Ty::Char,
            other => self.expr_ty(other).unwrap_or(Ty::I64),
        }
    }
}
