//! Mach VM: what protection the process's own mappings have
//! (`mach/mach_vm.h`, `mach/vm_region.h`), which the zero copy reads of a
//! guest buffer's mapping before a transfer writes it (`zero_copy.rs`), and
//! the pages its tests protect.

#![expect(non_camel_case_types, reason = "the SDK's own names")]

use super::{constants, declare};

/// `kern_return_t`: `int`.
pub(crate) type kern_return_t = i32;
/// `mach_port_t`: `__darwin_mach_port_t`, an `unsigned int`.
pub(crate) type mach_port_t = u32;
/// `vm_map_read_t`: a `mach_port_t`.
pub(crate) type vm_map_read_t = mach_port_t;
/// `vm_map_t`: a `mach_port_t`.
pub(crate) type vm_map_t = mach_port_t;
/// `boolean_t`: `int` on arm64.
pub(crate) type boolean_t = i32;
/// `mach_vm_address_t`: `uint64_t`.
pub(crate) type mach_vm_address_t = u64;
/// `mach_vm_size_t`: `uint64_t`.
pub(crate) type mach_vm_size_t = u64;
/// `vm_region_flavor_t`: `int`.
pub(crate) type vm_region_flavor_t = i32;
/// `vm_region_info_t`: `int *`.
pub(crate) type vm_region_info_t = *mut i32;
/// `mach_msg_type_number_t`: `natural_t`, an `unsigned int`.
pub(crate) type mach_msg_type_number_t = u32;
/// `vm_prot_t`: `int`.
pub(crate) type vm_prot_t = i32;

/// `vm_region_basic_info_data_64_t`, under the header's `#pragma pack(push,
/// 4)`: its `offset` follows five 4-byte fields at 20.
#[repr(C, packed(4))]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct vm_region_basic_info_data_64_t {
    pub(crate) protection: vm_prot_t,
    pub(crate) max_protection: vm_prot_t,
    pub(crate) inheritance: u32,
    pub(crate) shared: i32,
    pub(crate) reserved: i32,
    pub(crate) offset: u64,
    pub(crate) behavior: i32,
    pub(crate) user_wired_count: u16,
}

declare! {
    #[link(name = "System")]
    functions {
        fn mach_vm_region(
            target_task: vm_map_read_t,
            address: *mut mach_vm_address_t,
            size: *mut mach_vm_size_t,
            flavor: vm_region_flavor_t,
            info: vm_region_info_t,
            info_count: *mut mach_msg_type_number_t,
            object_name: *mut mach_port_t,
        ) -> kern_return_t;
        fn mach_vm_allocate(
            target: vm_map_t,
            address: *mut mach_vm_address_t,
            size: mach_vm_size_t,
            flags: i32,
        ) -> kern_return_t;
        fn mach_vm_protect(
            target_task: vm_map_t,
            address: mach_vm_address_t,
            size: mach_vm_size_t,
            set_maximum: boolean_t,
            new_protection: vm_prot_t,
        ) -> kern_return_t;
        fn mach_vm_deallocate(
            target: vm_map_t,
            address: mach_vm_address_t,
            size: mach_vm_size_t,
        ) -> kern_return_t;
    }
    statics {
        static mach_task_self_: mach_port_t;
    }
}

constants! {
    /// `KERN_SUCCESS`.
    pub(crate) const KERN_SUCCESS: kern_return_t = 0;
    /// `VM_REGION_BASIC_INFO_64`.
    pub(crate) const VM_REGION_BASIC_INFO_64: vm_region_flavor_t = 9;
    /// `VM_REGION_BASIC_INFO_COUNT_64`: the struct's size in `int`s.
    pub(crate) const VM_REGION_BASIC_INFO_COUNT_64: mach_msg_type_number_t = 9;
    /// `VM_PROT_READ`.
    pub(crate) const VM_PROT_READ: vm_prot_t = 1;
    /// `VM_PROT_WRITE`.
    pub(crate) const VM_PROT_WRITE: vm_prot_t = 2;
    /// `VM_FLAGS_ANYWHERE`.
    pub(crate) const VM_FLAGS_ANYWHERE: i32 = 1;
}
