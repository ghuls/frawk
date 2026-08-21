//! LLVM-specific functionality for the `FunctionAttr` construct in the `codegen` module.
use crate::codegen::FunctionAttr;
use llvm_sys::core::*;
use llvm_sys::prelude::*;
use std::sync::OnceLock;

// The LLVM  C API exposes `memory` as an enum attribute, but does not expose a
// MemoryEffects constructor. The definitions below mirror the representation in
// llvm/include/llvm/Support/ModRef.h. This is an LLVM-internal representation,
// so layouts are selected from the version of the linked LLVM library.

/// Flags indicating whether a memory access modifies or references memory.
#[repr(u8)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
enum ModRefInfo {
    /// Does not read or write memory.
    NoModRef = 0,
    /// May read memory.
    Ref = 1,
    /// May write memory.
    Mod = 2,
    /// May both read and write memory.
    ModRef = 3,
}

/// The locations at which a function might access memory.
///
/// Only locations that frawk addresses explicitly are represented here. The
/// complete, version-dependent `IRMemLocation` layouts are documented on
/// `LLVMMemoryLayout` below.
#[repr(u32)]
#[derive(Clone, Copy)]
enum IRMemLocation {
    /// Access to memory via argument pointers.
    ArgMem = 0,
}

/// Groups LLVM major versions that share the same `IRMemLocation` layout used
/// by `MemoryEffectsBase`.
///
/// In LLVM, `MemoryEffectsBase(ModRefInfo)` iterates from
/// `IRMemLocation::First` through `IRMemLocation::Last` and stores two bits per
/// location. frawk only needs to name `ArgMem` directly; this enum records how
/// many locations exist for whole-memory effects such as `memory(read)`.
#[derive(Clone, Copy)]
enum LLVMMemoryLayout {
    /// LLVM 16-20:
    ///   ArgMem          = 0  // Access to memory via argument pointers.
    ///   InaccessibleMem = 1  // Memory that is inaccessible via LLVM IR.
    ///   Other           = 2  // Any other memory.
    ///   First           = ArgMem
    ///   Last            = Other
    LLVM16To20,

    /// LLVM 21:
    ///   ArgMem          = 0  // Access to memory via argument pointers.
    ///   InaccessibleMem = 1  // Memory that is inaccessible via LLVM IR.
    ///   ErrnoMem        = 2  // Errno memory.
    ///   Other           = 3  // Any other memory.
    ///   First           = ArgMem
    ///   Last            = Other
    LLVM21,

    /// LLVM 22:
    ///   ArgMem          = 0  // Access to memory via argument pointers.
    ///   InaccessibleMem = 1  // Memory that is inaccessible via LLVM IR.
    ///   ErrnoMem        = 2  // Errno memory.
    ///   Other           = 3  // Any other memory.
    ///   TargetMem0      = 4  // Represents target specific state.
    ///   TargetMem1      = 5
    ///   First           = ArgMem
    ///   Last            = TargetMem1
    LLVM22,
}

impl LLVMMemoryLayout {
    /// Number of values in LLVM's inclusive `IRMemLocation::First..=Last`
    /// range for this layout.
    const fn num_locations(self) -> u32 {
        match self {
            Self::LLVM16To20 => 3,
            Self::LLVM21 => 4,
            Self::LLVM22 => 6,
        }
    }

    fn from_llvm_major(major: u32) -> Self {
        match major {
            16..=20 => Self::LLVM16To20,
            21 => Self::LLVM21,
            22 => Self::LLVM22,
            _ => panic!(
                "unsupported LLVM major version {major} for memory attribute encoding"
            ),
        }
    }
}

static LLVM_MEMORY_LAYOUT: OnceLock<LLVMMemoryLayout> = OnceLock::new();

fn llvm_memory_layout() -> LLVMMemoryLayout {
    *LLVM_MEMORY_LAYOUT.get_or_init(|| unsafe {
        let mut major = 0;
        LLVMGetVersion(
            &mut major,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        LLVMMemoryLayout::from_llvm_major(major)
    })
}

// These mirror MemoryEffectsBase::BitsPerLoc, getLocationPos() and setModRef()
// in llvm/include/llvm/Support/ModRef.h.
const BITS_PER_LOC: u32 = 2;
const LOC_MASK: u32 = (1 << BITS_PER_LOC) - 1;

const fn get_location_pos(loc: u32) -> u32 {
    loc * BITS_PER_LOC
}

const fn set_mod_ref(data: u32, loc: u32, mr: ModRefInfo) -> u32 {
    let pos = get_location_pos(loc);
    (data & !(LOC_MASK << pos)) | ((mr as u32) << pos)
}

const fn memory_effect(loc: IRMemLocation, mr: ModRefInfo) -> u64 {
    set_mod_ref(0, loc as u32, mr) as u64
}

fn memory_effects_all(mr: ModRefInfo) -> u64 {
    let mut data = 0;
    let mut loc = 0;
    let num_locations = llvm_memory_layout().num_locations();
    while loc < num_locations {
        data = set_mod_ref(data, loc, mr);
        loc += 1;
    }
    data as u64
}

const MEMORY_ARGMEM_READ: u64 = memory_effect(IRMemLocation::ArgMem, ModRefInfo::Ref);
const MEMORY_ARGMEM_READ_WRITE: u64 = memory_effect(IRMemLocation::ArgMem, ModRefInfo::ModRef);

fn memory_attr_value(attrs: &[FunctionAttr]) -> Option<u64> {
    use FunctionAttr::*;

    let mut read_only = false;
    let mut argmem_only = false;
    for attr in attrs.iter().copied() {
        match attr {
            ReadOnly => read_only = true,
            ArgmemOnly => argmem_only = true,
        }
    }

    match (read_only, argmem_only) {
        (false, false) => None,
        (true, false) => Some(memory_effects_all(ModRefInfo::Ref)),
        (false, true) => Some(MEMORY_ARGMEM_READ_WRITE),
        (true, true) => Some(MEMORY_ARGMEM_READ),
    }
}

pub unsafe fn add_function_attrs(ctx: LLVMContextRef, func: LLVMValueRef, attrs: &[FunctionAttr]) {
    let Some(value) = memory_attr_value(attrs) else {
        return;
    };

    let name = c_str!("memory");
    let kind = LLVMGetEnumAttributeKindForName(name, "memory".len());
    assert_ne!(kind, 0);
    let attr = LLVMCreateEnumAttribute(ctx, kind, value);
    LLVMAddAttributeAtIndex(func, llvm_sys::LLVMAttributeFunctionIndex, attr);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;
    use std::ptr;

    fn render_function_attrs(attrs: &[FunctionAttr]) -> String {
        unsafe {
            let ctx = LLVMContextCreate();
            let module = LLVMModuleCreateWithNameInContext(c_str!("attr_test"), ctx);
            let fn_ty = LLVMFunctionType(LLVMVoidTypeInContext(ctx), ptr::null_mut(), 0, 0);
            let func = LLVMAddFunction(module, c_str!("f"), fn_ty);
            add_function_attrs(ctx, func, attrs);

            let ir = LLVMPrintModuleToString(module);
            let rendered = CStr::from_ptr(ir).to_string_lossy().into_owned();
            LLVMDisposeMessage(ir);
            LLVMDisposeModule(module);
            LLVMContextDispose(ctx);
            rendered
        }
    }

    #[test]
    fn memory_attributes_match_llvm_ir() {
        assert!(render_function_attrs(&[FunctionAttr::ReadOnly]).contains("memory(read)"));
        assert!(render_function_attrs(&[FunctionAttr::ArgmemOnly])
            .contains("memory(argmem: readwrite)"));
        assert!(render_function_attrs(&[FunctionAttr::ReadOnly, FunctionAttr::ArgmemOnly])
            .contains("memory(argmem: read)"));
        assert!(!render_function_attrs(&[]).contains("memory("));
    }
}
