//! Cranelift code generation for frawk programs.
use cranelift::prelude::*;
use cranelift_module::{FuncId, Module};
use cranelift_simplejit::SimpleJITModule;
use hashbrown::HashMap;

use crate::codegen::{CodeGenerator, Op, Ref, Sig, StrReg};
use crate::common::{FileSpec, NumTy, Result};
use crate::compile;
use crate::runtime::UniqueStr;

// TODO: IntrinsicMap equivalent (convert signatures)
// TODO: locals => Variable map
//       globals referenced as loads to a global variable.
//       stores of ref-counted type are refs
//          (except for eliminated phi nodes)
//      things should, on the whole, be simpler (knock on wood)
//
//
// so, concretely. Let's keep the same Generator/View structure, but this time encapsulate all the
// shit into a Shared struct
//
// * implement type maps. (or perhapss we don't need them?)
// * declare all functions.
// * fill in CodeGenerator trait impl for View
//
// Notes:
// * do iterators, printf last. See if we can get something compiling without all those.
// * inline drop_str manually, or not at all (?)
// * strings need to be "allocad" i.e. pointers on the stack, and then stores into pointers, loads
// when doing get_val. Do we need to do the same w/maps (hoping "no"; we do this with maps in llvm
// because ... why, excactly? Something about doing refs)
//
// We'll probably want to do stack slots directly for prints

/// Function-independent data used in compilation
struct Shared {
    codegen_ctx: codegen::Context,
    module: SimpleJITModule,
    func_ids: Vec<FuncId>,
    external_funcs: HashMap<*const u8, FuncId>,
}

/// A cranelift [`Variable`] with frawk-specific metadata
struct VarRef {
    var: Variable,
    is_global: bool,
    skip_drop: bool,
}

/// Function-level state
struct Frame {
    // TODO: initialize these two at construction time. Unlike LLVM, we don't really have to worry
    // about constructing these things "just in time", we can take "bind_val" to behave just like
    // assignment.
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
}

impl<'a> CodeGenerator for View<'a> {
    type Ty = Type;
    type Val = Value;

    fn register_external_fn(
        &mut self,
        name: &'static str,
        name_c: *const u8,
        addr: *const u8,
        sig: Sig<Self>,
    ) -> Result<()> {
        unimplemented!()
    }

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
                // and ref the new value
                self.ref_val(Str, p);
            }
            MapIntInt | MapIntFloat | MapIntStr | MapStrInt | MapStrFloat | MapStrStr => {
                // first, ref the new value
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
        // * check locals, if it's there, return the val directly,
        //   (for strings, do we return the pointer or value itself? I think we return the pointer)
        // * if it is not, then check globals and issue the load (unless it's a string, in that
        // case return the pointer)
        unimplemented!()
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

    fn call_intrinsic(&mut self, func: Op, args: &mut [Self::Val]) -> Result<Self::Val> {
        unimplemented!()
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

    /// Moves the contents of `src` into `dst`, taking refcounts into consideration if necessary.
    fn mov(&mut self, ty: compile::Ty, dst: NumTy, src: NumTy) -> Result<()> {
        unimplemented!()
    }

    /// Constructs an iterator over the keys of `map` and stores it in `dst`.
    fn iter_begin(&mut self, dst: Ref, map: Ref) -> Result<()> {
        unimplemented!()
    }

    /// Queries the iterator in `iter` as to whether any elements remain, stores the result in the
    /// `dst` register.
    fn iter_hasnext(&mut self, dst: Ref, iter: Ref) -> Result<()> {
        unimplemented!()
    }

    /// Advances the iterator in `iter` to the next element and stores the current element in `dst`
    fn iter_getnext(&mut self, dst: Ref, iter: Ref) -> Result<()> {
        unimplemented!()
    }

    // The plumbing for builtin variable manipulation is mostly pretty wrote ... anything we can do
    // here?

    /// Method called after loading a builtin variable into `dst`.
    ///
    /// This is included to help clean up ref-counts on string or map builtins, if necessary.
    fn var_loaded(&mut self, dst: Ref) -> Result<()> {
        unimplemented!()
    }
}
