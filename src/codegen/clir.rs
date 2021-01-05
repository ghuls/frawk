//! Cranelift code generation for frawk programs.
use cranelift::prelude::*;
use cranelift_module::{FuncId, Linkage, Module};
use cranelift_simplejit::{SimpleJITBuilder, SimpleJITModule};
use hashbrown::{HashMap, HashSet};

use crate::builtins;
use crate::codegen::{Backend, CodeGenerator, Op, Ref, Sig, StrReg};
use crate::common::{CompileError, FileSpec, NumTy, Result};
use crate::compile;
use crate::runtime::UniqueStr;

// TODO:
// * do `mov`
// * set up iterators
// * do print_all
// * do printf, sprintf
// * figure out declarations, stack slots for strings, and frame setup.
// * control flow, drops, etc.
//
// TODO (cleanup; after tests are passing):
// * move floatfunc/bitwise stuff into llvm module
// * move llvm module under codegen
// * make sure cargo doc builds
// * doc fixups

/// Information about a user-defined function needed by callers.
struct FuncInfo {
    globals: HashSet<Ref>,
    func_id: FuncId,
}

/// Function-independent data used in compilation
struct Shared {
    codegen_ctx: codegen::Context,
    module: SimpleJITModule,
    func_ids: Vec<FuncInfo>,
    external_funcs: HashMap<*const u8, FuncId>,
    // We need cranelift Signatures for declaring external functions. We put them here to reuse
    // them across calls to `register_external_fn`.
    sig: Signature,
}

/// A cranelift [`Variable`] with frawk-specific metadata
struct VarRef {
    var: Variable,
    is_global: bool,
    skip_drop: bool,
}

/// Function-level state
struct Frame {
    // Used for most lookups of variable information.
    vars: HashMap<Ref, VarRef>,
    runtime: Variable,
    n_params: usize,
    n_vars: usize,
}

/// Toplevel information
struct GlobalContext {
    shared: Shared,
    ctx: FunctionBuilderContext,
    funcs: Vec<Frame>,
}

/// The state required for generating code for the function at `f`.
struct View<'a> {
    f: &'a mut Frame,
    builder: FunctionBuilder<'a>,
    shared: &'a mut Shared,
}

macro_rules! external {
    ($name:ident) => {
        crate::codegen::intrinsics::$name as *const u8
    };
}

impl<'a> View<'a> {
    /// Increment the refcount of the value `v` of type `ty`.
    ///
    /// If `ty` is not an array or string type, this method is a noop.
    fn ref_val(&mut self, ty: compile::Ty, v: Value) {
        use compile::Ty::*;
        let func = match ty {
            MapIntInt | MapIntFloat | MapIntStr | MapStrInt | MapStrFloat | MapStrStr => {
                external!(ref_map)
            }
            Str => external!(ref_str),
            Null | Int | Float | IterInt | IterStr => return,
        };
        self.call_external_void(func, &[v]);
    }

    /// Decrement the refcount of the value `v` of type `ty`.
    ///
    /// If `ty` is not an array or string type, this method is a noop.
    fn drop_val(&mut self, ty: compile::Ty, v: Value) {
        use compile::Ty::*;
        let func = match ty {
            MapIntInt => external!(drop_intint),
            MapIntFloat => external!(drop_intfloat),
            MapIntStr => external!(drop_intstr),
            MapStrInt => external!(drop_strint),
            MapStrFloat => external!(drop_strfloat),
            MapStrStr => external!(drop_strstr),
            Str => external!(drop_str),
            Null | Int | Float | IterInt | IterStr => return,
        };
        self.call_external_void(func, &[v]);
    }
    /// Call and external function that returns a value.
    ///
    /// Panics if `func` has not been registered as an external function, or if it was not
    /// registered as returning a single value.
    fn call_external(&mut self, func: *const u8, args: &[Value]) -> Value {
        let inst = self.call_inst(func, args);
        let mut iter = self.builder.inst_results(inst).iter().cloned();
        let ret = iter.next().expect("expected return value");
        // For now, we expect all functions to have a single return value.
        debug_assert!(iter.next().is_none());
        ret
    }

    /// Call and external function that does not return a value.
    ///
    /// Panics if `func` has not been registered as an external function, or if it was not
    /// registered as returning a single value.
    fn call_external_void(&mut self, func: *const u8, args: &[Value]) {
        let _inst = self.call_inst(func, args);
        debug_assert!(self.builder.inst_results(_inst).iter().next().is_none());
    }

    fn call_inst(&mut self, func: *const u8, args: &[Value]) -> cranelift_codegen::ir::Inst {
        let id = self.shared.external_funcs[&func];
        let fref = self
            .shared
            .module
            .declare_func_in_func(id, self.builder.func);
        self.builder.ins().call(fref, args)
    }

    /// frawk does not have booleans, so for now we always convert the results of comparison
    /// operations back to integers.
    ///
    /// NB: It would be interesting and likely useful to add a "bool" type (with consequent
    /// coercions).
    fn bool_to_int(&mut self, b: Value) -> Value {
        let int_ty = self.get_ty(compile::Ty::Int);
        self.builder.ins().bint(int_ty, b)
    }

    /// Generate a new value according to the comparison instruction, applied to `l` and `r`, which
    /// are assumed to be floating point values if `is_float` and (signed, as is the case in frawk)
    /// integer values otherwise.
    ///
    /// As with the LLVM, we use the "ordered" variants on comparsion: the ones that return false
    /// if either operand is NaN.
    fn cmp(&mut self, op: crate::codegen::Cmp, is_float: bool, l: Value, r: Value) -> Value {
        use crate::codegen::Cmp::*;
        let res = if is_float {
            match op {
                EQ => self.builder.ins().fcmp(FloatCC::Equal, l, r),
                LTE => self.builder.ins().fcmp(FloatCC::LessThanOrEqual, l, r),
                LT => self.builder.ins().fcmp(FloatCC::LessThan, l, r),
                GTE => self.builder.ins().fcmp(FloatCC::GreaterThanOrEqual, l, r),
                GT => self.builder.ins().fcmp(FloatCC::GreaterThan, l, r),
            }
        } else {
            match op {
                EQ => self.builder.ins().icmp(IntCC::Equal, l, r),
                LTE => self.builder.ins().icmp(IntCC::SignedLessThanOrEqual, l, r),
                LT => self.builder.ins().icmp(IntCC::SignedLessThan, l, r),
                GTE => self
                    .builder
                    .ins()
                    .icmp(IntCC::SignedGreaterThanOrEqual, l, r),
                GT => self.builder.ins().icmp(IntCC::SignedGreaterThan, l, r),
            }
        };
        self.bool_to_int(res)
    }

    /// Generate a new value according to the operation specified in `op`.
    ///
    /// We assume that `args` contains floating point or signed integer values depending on the
    /// value of `is_float`. Panics if args has the wrong arity.
    fn arith(&mut self, op: crate::codegen::Arith, is_float: bool, args: &[Value]) -> Value {
        use crate::codegen::Arith::*;
        if is_float {
            match op {
                Mul => self.builder.ins().fmul(args[0], args[1]),
                Minus => self.builder.ins().fsub(args[0], args[1]),
                Add => self.builder.ins().fadd(args[0], args[1]),
                // No floating-point modulo in cranelift?
                Mod => self.call_external(external!(_frawk_fprem), args),
                Neg => self.builder.ins().fneg(args[0]),
            }
        } else {
            match op {
                Mul => self.builder.ins().imul(args[0], args[1]),
                Minus => self.builder.ins().isub(args[0], args[1]),
                Add => self.builder.ins().iadd(args[0], args[1]),
                Mod => self.builder.ins().srem(args[0], args[1]),
                Neg => self.builder.ins().ineg(args[0]),
            }
        }
    }

    /// Apply the bitwise operation specified in `op` to args.
    ///
    /// Panics if args has the wrong arity (2 for all bitwise operations except for `Complement`).
    /// All of the entries in `args` should be integer values.
    fn bitwise(&mut self, op: builtins::Bitwise, args: &[Value]) -> Value {
        use builtins::Bitwise::*;
        match op {
            Complement => self.builder.ins().bnot(args[0]),
            And => self.builder.ins().band(args[0], args[1]),
            Or => self.builder.ins().bor(args[0], args[1]),
            LogicalRightShift => self.builder.ins().ushr(args[0], args[1]),
            ArithmeticRightShift => self.builder.ins().ushr(args[0], args[1]),
            LeftShift => self.builder.ins().ishl(args[0], args[1]),
            Xor => self.builder.ins().bxor(args[0], args[1]),
        }
    }

    fn floatfunc(&mut self, op: builtins::FloatFunc, args: &[Value]) -> Value {
        use builtins::FloatFunc::*;
        match op {
            Cos => self.call_external(external!(_frawk_cos), args),
            Sin => self.call_external(external!(_frawk_sin), args),
            Atan => self.call_external(external!(_frawk_atan), args),
            Atan2 => self.call_external(external!(_frawk_atan2), args),
            Log => self.call_external(external!(_frawk_log), args),
            Log2 => self.call_external(external!(_frawk_log2), args),
            Log10 => self.call_external(external!(_frawk_log10), args),
            Sqrt => self.builder.ins().sqrt(args[0]),
            Exp => self.call_external(external!(_frawk_exp), args),
        }
    }
}

// For Cranelift, we need to register function names in a lookup table before constructing a
// module, so we actually implement `Backend` twice for each registration step.

struct RegistrationState {
    builder: SimpleJITBuilder,
}

impl Backend for RegistrationState {
    type Ty = ();
    fn void_ptr_ty(&self) -> () {
        ()
    }
    fn ptr_to(&self, (): ()) -> () {
        ()
    }
    fn usize_ty(&self) -> () {
        ()
    }
    fn u32_ty(&self) -> () {
        ()
    }
    fn get_ty(&self, _ty: compile::Ty) -> () {
        ()
    }

    fn register_external_fn(
        &mut self,
        name: &'static str,
        _name_c: *const u8,
        addr: *const u8,
        _sig: Sig<Self>,
    ) -> Result<()> {
        self.builder.symbol(name, addr);
        Ok(())
    }
}

impl<'a> Backend for View<'a> {
    type Ty = Type;
    // mappings from compile::Ty to Self::Ty
    fn void_ptr_ty(&self) -> Self::Ty {
        self.shared.module.target_config().pointer_type()
    }
    fn ptr_to(&self, _ty: Self::Ty) -> Self::Ty {
        // Cranelift pointers are all a single type, though we may eventually need to care more
        // about "references", which cranelift uses to compute stack maps.
        self.void_ptr_ty()
    }
    fn usize_ty(&self) -> Self::Ty {
        // assume pointers are 64 bits
        types::I64
    }
    fn u32_ty(&self) -> Self::Ty {
        types::I32
    }
    fn get_ty(&self, ty: compile::Ty) -> Self::Ty {
        use compile::Ty::*;
        match ty {
            Null | Int => types::I64,
            Float => types::F64,
            Str => types::I128,
            MapIntInt | MapIntFloat | MapIntStr => self.void_ptr_ty(),
            MapStrInt | MapStrFloat | MapStrStr => self.void_ptr_ty(),
            IterInt | IterStr => panic!("taking the type of an iterator"),
        }
    }

    fn register_external_fn(
        &mut self,
        name: &'static str,
        _name_c: *const u8,
        addr: *const u8,
        sig: Sig<Self>,
    ) -> Result<()> {
        let cl_sig = &mut self.shared.sig;
        cl_sig.params.clear();
        cl_sig.returns.clear();
        cl_sig
            .params
            .extend(sig.args.iter().cloned().map(AbiParam::new));
        cl_sig
            .returns
            .extend(sig.ret.as_ref().into_iter().cloned().map(AbiParam::new));
        let id = self
            .shared
            .module
            .declare_function(name, Linkage::Import, cl_sig)
            .map_err(|e| {
                CompileError(format!(
                    "error declaring {} in module: {}",
                    name,
                    e.to_string()
                ))
            })?;
        self.shared.external_funcs.insert(addr, id);
        Ok(())
    }
}

impl<'a> CodeGenerator for View<'a> {
    type Val = Value;

    // mappings to and from bytecode-level registers to IR-level values
    fn bind_val(&mut self, r: Ref, v: Self::Val) -> Result<()> {
        use compile::Ty::*;
        let (var, is_global) = if let Some(VarRef { var, is_global, .. }) = self.f.vars.get(&r) {
            (*var, *is_global)
        } else {
            return err!("unbound reference in current frame: {:?}", r);
        };
        match r.1 {
            Int | Float => {
                if is_global {
                    let p = self.builder.use_var(var);
                    self.builder.ins().store(MemFlags::trusted(), v, p, 0);
                } else {
                    self.builder.def_var(var, v);
                }
            }
            Str => {
                // NB: we assume that `v` is a string, not a pointer to a string.

                // For now, we treat globals and locals the same for strings.
                // TODO: Hopefully the stack slot mechanics don't ruin all of that...

                // first, drop the value currently in the pointer
                let p = self.builder.use_var(var);
                self.drop_val(Str, p);
                self.builder.ins().store(MemFlags::trusted(), v, p, 0);
            }
            MapIntInt | MapIntFloat | MapIntStr | MapStrInt | MapStrFloat | MapStrStr => {
                // first, ref the new value
                // TODO: can we skip the ref here?
                self.ref_val(r.1, v);
                if is_global {
                    // then, drop the value currently in the pointer
                    let p = self.builder.use_var(var);
                    let pointee = self
                        .builder
                        .ins()
                        // TODO: should this be a pointer type?
                        .load(types::I64, MemFlags::trusted(), p, 0);
                    self.drop_val(r.1, pointee);

                    // And slot the new value in
                    self.builder.ins().store(MemFlags::trusted(), v, p, 0);
                } else {
                    let cur = self.builder.use_var(var);
                    self.drop_val(r.1, cur);
                    self.builder.def_var(var, v);
                }
            }
            Null => {}
            IterInt | IterStr => return err!("attempting to store an iterator value"),
        }
        Ok(())
    }
    fn get_val(&mut self, r: Ref) -> Result<Self::Val> {
        use compile::Ty::*;
        if let Null = r.1 {
            return Ok(self.const_int(0));
        }
        let (var, is_global) = if let Some(VarRef { var, is_global, .. }) = self.f.vars.get(&r) {
            (*var, *is_global)
        } else {
            return err!("loading an unbound variable: {:?}", r);
        };
        let val = self.builder.use_var(var);

        match r.1 {
            MapIntInt | MapIntFloat | MapIntStr | MapStrInt | MapStrFloat | MapStrStr | Int
            | Float => {
                if is_global {
                    let ty = self.get_ty(r.1);
                    Ok(self.builder.ins().load(ty, MemFlags::trusted(), val, 0))
                } else {
                    Ok(val)
                }
            }
            Str => Ok(val),
            IterInt | IterStr => err!("attempting to load an iterator pointer"),
            Null => unreachable!(),
        }
    }

    // backend-specific handling of constants and low-level operations.
    fn runtime_val(&mut self) -> Self::Val {
        self.builder.use_var(self.f.runtime)
    }
    fn const_int(&mut self, i: i64) -> Self::Val {
        self.builder.ins().iconst(types::I64, i)
    }
    fn const_float(&mut self, f: f64) -> Self::Val {
        self.builder.ins().f64const(f)
    }
    fn const_str<'b>(&mut self, s: &UniqueStr<'b>) -> Self::Val {
        // iconst does not support I128, so we concatenate two I64 constants.
        let bits: u128 = s.clone_str().into_bits();
        let low = bits as i64;
        let high = (bits >> 64) as i64;
        let low_v = self.builder.ins().iconst(types::I64, low);
        let high_v = self.builder.ins().iconst(types::I64, high);
        self.builder.ins().iconcat(low_v, high_v)
    }
    fn const_ptr<'b, T>(&'b mut self, c: &'b T) -> Self::Val {
        self.const_int(c as *const _ as i64)
    }

    fn call_void(&mut self, func: *const u8, args: &mut [Self::Val]) -> Result<()> {
        Ok(self.call_external_void(func, args))
    }

    // TODO if all goes well, remove the Result<..> wrapper and migrate the callers.
    fn call_intrinsic(&mut self, func: Op, args: &mut [Self::Val]) -> Result<Self::Val> {
        use Op::*;
        match func {
            Cmp { is_float, op } => Ok(self.cmp(op, is_float, args[0], args[1])),
            Arith { is_float, op } => Ok(self.arith(op, is_float, args)),
            Bitwise(bw) => Ok(self.bitwise(bw, args)),
            Math(ff) => Ok(self.floatfunc(ff, args)),
            Div => Ok(self.builder.ins().fdiv(args[0], args[1])),
            Pow => Ok(self.call_external(external!(_frawk_pow), args)),
            FloatToInt => {
                let ty = self.get_ty(compile::Ty::Int);
                Ok(self.builder.ins().fcvt_to_sint_sat(ty, args[0]))
            }
            IntToFloat => {
                let ty = self.get_ty(compile::Ty::Float);
                Ok(self.builder.ins().fcvt_from_sint(ty, args[0]))
            }
            Intrinsic(e) => Ok(self.call_external(e, args)),
        }
    }

    // var-arg printing functions. The arguments here directly parallel the instruction
    // definitions.

    fn printf(
        &mut self,
        output: &Option<(StrReg, FileSpec)>,
        fmt: &StrReg,
        args: &Vec<Ref>,
    ) -> Result<()> {
        unimplemented!()
    }

    fn sprintf(&mut self, dst: &StrReg, fmt: &StrReg, args: &Vec<Ref>) -> Result<()> {
        unimplemented!()
    }

    fn print_all(&mut self, output: &Option<(StrReg, FileSpec)>, args: &Vec<StrReg>) -> Result<()> {
        unimplemented!()
    }

    fn mov(&mut self, ty: compile::Ty, dst: NumTy, src: NumTy) -> Result<()> {
        unimplemented!()
    }

    fn iter_begin(&mut self, dst: Ref, map: Ref) -> Result<()> {
        unimplemented!()
    }

    fn iter_hasnext(&mut self, dst: Ref, iter: Ref) -> Result<()> {
        unimplemented!()
    }

    fn iter_getnext(&mut self, dst: Ref, iter: Ref) -> Result<()> {
        unimplemented!()
    }

    // The plumbing for builtin variable manipulation is mostly pretty wrote ... anything we can do
    // here?

    fn var_loaded(&mut self, dst: Ref) -> Result<()> {
        unimplemented!()
    }
}