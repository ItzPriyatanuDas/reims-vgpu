//! Our own declarations of the Apple framework functions, statics, types
//! and constants the backend calls, one module per framework, and the
//! private ones VideoToolbox exports without a header ([`private`]).
//!
//! Each declaration is written once, through [`declare!`], which also
//! records its C spelling. The test at the bottom compiles a C file of
//! `_Static_assert`s against the SDK's headers, one per
//! function and static, so that a parameter or return of the wrong type
//! fails to build, and compares the sizes and layouts of the types and the
//! values of the constants with the headers'.

pub(crate) mod core_foundation;
pub(crate) mod core_media;
pub(crate) mod core_video;
pub(crate) mod io_surface;
pub(crate) mod mach;
pub(crate) mod motion_estimation;
pub(crate) mod private;
pub(crate) mod video_toolbox;

/// One declaration as [`declare!`] records it, for the header check.
#[cfg(test)]
pub(crate) enum Declaration {
    /// A function: its name, its return type, and its parameters' types,
    /// each as Rust spells it.
    Function {
        name: &'static str,
        returns: &'static str,
        arguments: &'static [&'static str],
    },
    /// A static: its name and type.
    Static {
        name: &'static str,
        ty: &'static str,
    },
}

/// Declares one framework's functions and statics, and records each for
/// the header check as `DECLARATIONS`. Every function is `unsafe`: its
/// caller keeps CoreFoundation's rules for the references it passes.
macro_rules! declare {
    (
        #[link($($link:tt)*)]
        functions {
            $( fn $name:ident ( $( $arg:ident : $ty:ty ),* $(,)? ) $( -> $ret:ty )? ; )*
        }
        statics {
            $( static $static_name:ident : $static_ty:ty ; )*
        }
    ) => {
        #[link($($link)*)]
        #[allow(
            dead_code,
            reason = "a framework's declarations are checked against its headers as a set, \
                      and the probe and tests call some the backend does not"
        )]
        unsafe extern "C" {
            $( pub(crate) fn $name ( $( $arg : $ty ),* ) $( -> $ret )? ; )*
            $( pub(crate) static $static_name : $static_ty ; )*
        }

        #[cfg(test)]
        pub(crate) const DECLARATIONS: &[$crate::macos::ffi::Declaration] = &[
            $(
                $crate::macos::ffi::Declaration::Function {
                    name: stringify!($name),
                    returns: declare!(@returns $( $ret )?),
                    arguments: &[ $( stringify!($ty) ),* ],
                },
            )*
            $(
                $crate::macos::ffi::Declaration::Static {
                    name: stringify!($static_name),
                    ty: stringify!($static_ty),
                },
            )*
        ];
    };
    (@returns) => { "void" };
    (@returns $ret:ty) => { stringify!($ret) };
}
pub(crate) use declare;

/// Records named constants for the header check as `CONSTANTS`: each name
/// is the SDK's, and the check prints the header's value beside ours.
macro_rules! constants {
    ( $( $(#[$attr:meta])* pub(crate) const $name:ident : $ty:ty = $value:expr ; )* ) => {
        $(
            $(#[$attr])*
            #[allow(dead_code, reason = "checked against the headers with the rest of its framework")]
            pub(crate) const $name: $ty = $value;
        )*

        #[cfg(test)]
        pub(crate) const CONSTANTS: &[(&str, i128)] = &[
            $( (stringify!($name), $name as i128), )*
        ];
    };
}
pub(crate) use constants;

/// The size of each type the declarations use, against the header's.
#[cfg(test)]
const TYPE_SIZES: &[(&str, usize)] = {
    use core_foundation::*;
    use core_media::*;
    use core_video::*;
    use std::mem::size_of;
    use video_toolbox::*;
    &[
        ("Boolean", size_of::<Boolean>()),
        ("OSStatus", size_of::<OSStatus>()),
        ("OSType", size_of::<OSType>()),
        ("CFIndex", size_of::<CFIndex>()),
        ("CFTypeID", size_of::<CFTypeID>()),
        ("CFStringEncoding", size_of::<CFStringEncoding>()),
        ("CFNumberType", size_of::<CFNumberType>()),
        ("CMVideoCodecType", size_of::<CMVideoCodecType>()),
        ("CMItemCount", size_of::<CMItemCount>()),
        ("CMBlockBufferFlags", size_of::<CMBlockBufferFlags>()),
        ("CMTime", size_of::<CMTime>()),
        ("CMSampleTimingInfo", size_of::<CMSampleTimingInfo>()),
        ("CVReturn", size_of::<CVReturn>()),
        (
            "CVPixelBufferLockFlags",
            size_of::<CVPixelBufferLockFlags>(),
        ),
        ("CVAttachmentMode", size_of::<CVAttachmentMode>()),
        ("VTDecodeFrameFlags", size_of::<VTDecodeFrameFlags>()),
        ("VTDecodeInfoFlags", size_of::<VTDecodeInfoFlags>()),
        ("VTEncodeInfoFlags", size_of::<VTEncodeInfoFlags>()),
        (
            "VTDecompressionOutputCallbackRecord",
            size_of::<VTDecompressionOutputCallbackRecord>(),
        ),
        ("kern_return_t", size_of::<mach::kern_return_t>()),
        ("mach_port_t", size_of::<mach::mach_port_t>()),
        ("mach_vm_address_t", size_of::<mach::mach_vm_address_t>()),
        ("mach_vm_size_t", size_of::<mach::mach_vm_size_t>()),
        ("vm_region_flavor_t", size_of::<mach::vm_region_flavor_t>()),
        (
            "mach_msg_type_number_t",
            size_of::<mach::mach_msg_type_number_t>(),
        ),
        ("vm_prot_t", size_of::<mach::vm_prot_t>()),
        (
            "vm_region_basic_info_data_64_t",
            size_of::<mach::vm_region_basic_info_data_64_t>(),
        ),
    ]
};

/// The offset of each field of the structs the declarations pass by value
/// or by pointer, against the header's.
#[cfg(test)]
const FIELD_OFFSETS: &[(&str, &str, usize)] = {
    use core_media::*;
    use std::mem::offset_of;
    use video_toolbox::*;
    &[
        ("CMTime", "value", offset_of!(CMTime, value)),
        ("CMTime", "timescale", offset_of!(CMTime, timescale)),
        ("CMTime", "flags", offset_of!(CMTime, flags)),
        ("CMTime", "epoch", offset_of!(CMTime, epoch)),
        (
            "CMSampleTimingInfo",
            "duration",
            offset_of!(CMSampleTimingInfo, duration),
        ),
        (
            "CMSampleTimingInfo",
            "presentationTimeStamp",
            offset_of!(CMSampleTimingInfo, presentation_time_stamp),
        ),
        (
            "CMSampleTimingInfo",
            "decodeTimeStamp",
            offset_of!(CMSampleTimingInfo, decode_time_stamp),
        ),
        (
            "VTDecompressionOutputCallbackRecord",
            "decompressionOutputCallback",
            offset_of!(VTDecompressionOutputCallbackRecord, callback),
        ),
        (
            "VTDecompressionOutputCallbackRecord",
            "decompressionOutputRefCon",
            offset_of!(VTDecompressionOutputCallbackRecord, ref_con),
        ),
        (
            "vm_region_basic_info_data_64_t",
            "protection",
            offset_of!(mach::vm_region_basic_info_data_64_t, protection),
        ),
        (
            "vm_region_basic_info_data_64_t",
            "max_protection",
            offset_of!(mach::vm_region_basic_info_data_64_t, max_protection),
        ),
        (
            "vm_region_basic_info_data_64_t",
            "offset",
            offset_of!(mach::vm_region_basic_info_data_64_t, offset),
        ),
        (
            "vm_region_basic_info_data_64_t",
            "user_wired_count",
            offset_of!(mach::vm_region_basic_info_data_64_t, user_wired_count),
        ),
    ]
};

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    use super::{
        Declaration, core_foundation, core_media, core_video, io_surface, mach, motion_estimation,
        video_toolbox,
    };

    /// The headers the check includes.
    const HEADERS: &str = "\
#include <Block.h>
#include <CoreFoundation/CoreFoundation.h>
#include <CoreMedia/CoreMedia.h>
#include <CoreVideo/CoreVideo.h>
#include <CoreVideo/CVPixelBufferIOSurface.h>
#include <IOSurface/IOSurface.h>
#include <VideoToolbox/VideoToolbox.h>
#include <dlfcn.h>
#include <mach/mach.h>
#include <mach/mach_vm.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
";

    /// The C spelling of a type as Rust spells it: primitives by their
    /// `<stdint.h>` names, pointers inside out, and every other name (each
    /// a typedef the SDK has) as it is.
    fn c_type(rust: &str) -> String {
        let rust = rust.trim();
        if let Some(rest) = rust.strip_prefix('*') {
            let rest = rest.trim_start();
            if let Some(pointee) = rest.strip_prefix("const ") {
                return pointer_to(&c_type(pointee), true);
            }
            if let Some(pointee) = rest.strip_prefix("mut ") {
                return pointer_to(&c_type(pointee), false);
            }
        }
        match rust {
            "c_void" => "void",
            "c_char" => "char",
            "u8" => "uint8_t",
            "u16" => "uint16_t",
            "u32" => "uint32_t",
            "u64" => "uint64_t",
            "i8" => "int8_t",
            "i16" => "int16_t",
            "i32" => "int32_t",
            "i64" => "int64_t",
            "usize" => "size_t",
            "f64" => "double",
            other => other,
        }
        .to_owned()
    }

    /// `pointee *` or `const pointee *`, with the qualifier on the pointee.
    fn pointer_to(pointee: &str, constant: bool) -> String {
        if constant {
            // A pointer's own pointee is qualified after its star.
            if pointee.ends_with('*') {
                format!("{pointee} const *")
            } else {
                format!("const {pointee} *")
            }
        } else {
            format!("{pointee} *")
        }
    }

    /// One `_Static_assert` per declaration: the header's type of each name
    /// against the type ours spells.
    fn assertions(declarations: &[Declaration]) -> String {
        let mut c = String::new();
        for declaration in declarations {
            match declaration {
                Declaration::Function {
                    name,
                    returns,
                    arguments,
                } => {
                    let arguments: Vec<String> = arguments.iter().map(|a| c_type(a)).collect();
                    let arguments = if arguments.is_empty() {
                        "void".to_owned()
                    } else {
                        arguments.join(", ")
                    };
                    let _ = writeln!(
                        c,
                        "_Static_assert(__builtin_types_compatible_p(__typeof__({name}), {} ({arguments})), \"{name}\");",
                        c_type(returns),
                    );
                }
                Declaration::Static { name, ty } => {
                    let _ = writeln!(
                        c,
                        "_Static_assert(__builtin_types_compatible_p(__typeof__({name}), const {}), \"{name}\");",
                        c_type(ty),
                    );
                }
            }
        }
        c
    }

    /// The C file: the assertions, the callbacks' types, then a program
    /// that prints each type's size and layout and each constant's value,
    /// as `rust_facts` does.
    fn check_program() -> String {
        let mut c = String::from(HEADERS);
        for declarations in [
            core_foundation::DECLARATIONS,
            core_media::DECLARATIONS,
            core_video::DECLARATIONS,
            io_surface::DECLARATIONS,
            mach::DECLARATIONS,
            video_toolbox::DECLARATIONS,
            motion_estimation::DECLARATIONS,
        ] {
            c.push_str(&assertions(declarations));
        }
        c.push_str(motion_estimation::C_ASSERTIONS);
        c.push_str(
            "_Static_assert(__builtin_types_compatible_p(VTDecompressionOutputCallback, \
             void (*)(void *, void *, OSStatus, VTDecodeInfoFlags, CVImageBufferRef, CMTime, CMTime)), \
             \"VTDecompressionOutputCallback\");\n\
             _Static_assert(__builtin_types_compatible_p(VTCompressionOutputCallback, \
             void (*)(void *, void *, OSStatus, VTEncodeInfoFlags, CMSampleBufferRef)), \
             \"VTCompressionOutputCallback\");\n",
        );
        // The two declared outside `declare!`: `ffi/private.rs`'s dlsym and
        // `ffi/motion_estimation.rs`'s block class.
        c.push_str(
            "_Static_assert(__builtin_types_compatible_p(__typeof__(dlsym), \
             void *(void *, const char *)), \"dlsym\");\n\
             _Static_assert(__builtin_types_compatible_p(__typeof__(_NSConcreteStackBlock), \
             void *[32]), \"_NSConcreteStackBlock\");\n",
        );
        c.push_str("int main(void) {\n");
        for (name, _) in super::TYPE_SIZES {
            let _ = writeln!(c, "    printf(\"size {name} %zu\\n\", sizeof({name}));");
        }
        for (ty, field, _) in super::FIELD_OFFSETS {
            let _ = writeln!(
                c,
                "    printf(\"offset {ty}.{field} %zu\\n\", offsetof({ty}, {field}));"
            );
        }
        for constants in [
            core_foundation::CONSTANTS,
            core_media::CONSTANTS,
            core_video::CONSTANTS,
            video_toolbox::CONSTANTS,
            io_surface::CONSTANTS,
            mach::CONSTANTS,
            super::private::CONSTANTS,
        ] {
            for (name, _) in constants {
                let _ = writeln!(
                    c,
                    "    printf(\"const {name} %lld\\n\", (long long)({name}));"
                );
            }
        }
        c.push_str("    return 0;\n}\n");
        c
    }

    /// What the C program prints, from this side.
    fn rust_facts() -> Vec<String> {
        let mut facts = Vec::new();
        for (name, size) in super::TYPE_SIZES {
            facts.push(format!("size {name} {size}"));
        }
        for (ty, field, offset) in super::FIELD_OFFSETS {
            facts.push(format!("offset {ty}.{field} {offset}"));
        }
        for constants in [
            core_foundation::CONSTANTS,
            core_media::CONSTANTS,
            core_video::CONSTANTS,
            video_toolbox::CONSTANTS,
            io_surface::CONSTANTS,
            mach::CONSTANTS,
            super::private::CONSTANTS,
        ] {
            for (name, value) in constants {
                facts.push(format!("const {name} {value}"));
            }
        }
        facts
    }

    /// Every declaration, type and constant against the SDK's headers:
    /// compiled with `xcrun clang`, run, and compared line for line.
    #[test]
    fn declarations_match_the_sdk_headers() -> Result<(), Box<dyn std::error::Error>> {
        let dir = std::env::temp_dir().join(format!("rvgv-vt-headers-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let binary = dir.join("check");
        let mut compile = Command::new("xcrun")
            .args(["clang", "-Werror", "-x", "c", "-", "-o"])
            .arg(&binary)
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        compile
            .stdin
            .take()
            .ok_or("no stdin")?
            .write_all(check_program().as_bytes())?;
        let compiled = compile.wait_with_output()?;
        if !compiled.status.success() {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(format!(
                "the header check does not compile:\n{}",
                String::from_utf8_lossy(&compiled.stderr)
            )
            .into());
        }
        let output = Command::new(&binary).output()?;
        std::fs::remove_dir_all(&dir)?;
        let c_facts: Vec<String> = String::from_utf8(output.stdout)?
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(rust_facts(), c_facts);
        Ok(())
    }

    #[test]
    fn rust_types_spell_as_c() {
        assert_eq!(c_type("*mut *const c_void"), "const void * *");
        assert_eq!(
            c_type("*const CMSampleTimingInfo"),
            "const CMSampleTimingInfo *"
        );
        assert_eq!(c_type("*mut CVPixelBufferRef"), "CVPixelBufferRef *");
        assert_eq!(c_type("usize"), "size_t");
    }
}
