//! The scaler's kernels beside this Mac's own scalers, in one process, interleaved, on the same
//! surfaces: Apple's scaler through IOSurfaceAccelerator's TransformSurface, VideoToolbox's
//! VTPixelTransferSession, and MPS's MPSImageLanczosScale and MPSImageBilinearScale on textures
//! over the surfaces' planes. Each case runs every engine once to warm it, then 8 rounds of 3
//! transforms of each engine in turn, and prints each engine's median wall time and, where the
//! GPU says, GPU time. Run alone, in release (`cargo nextest run --release -p reims-vgpu -E
//! 'test(/against_the_macs/)' --run-ignored only --no-capture`).
//!
//! - **The surfaces** are the platform's own IOSurfaces of the process's memory, as
//!   CVPixelBuffers make them: never one over pages read-only for the scaler.
//! - **What runs where**: MPS only scales, so it runs only where the formats are the same and
//!   nothing turns, each plane of a 4:2:0 surface a texture of its own; VideoToolbox only where
//!   nothing turns and both windows are whole; Apple's scaler everywhere.
//! - **The staged kernel** runs on surfaces of the test's own memory, which the kernel reaches
//!   through buffers of the GPU's own: its GPU time beside the in-place one says what reading the
//!   surfaces where they are costs.

use std::ffi::{c_char, c_int, c_void};
use std::ptr::{self, NonNull};

use objc2::encode::{Encode, Encoding, RefEncode};
use objc2::msg_send;
use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, ProtocolObject};
use objc2_foundation::{NSMutableDictionary, NSNumber, NSString};
use objc2_io_surface::IOSurfaceRef;
use objc2_metal::{
    MTLCommandBuffer as _, MTLCommandQueue, MTLCreateSystemDefaultDevice, MTLDevice, MTLOrigin,
    MTLPixelFormat, MTLRegion, MTLSize, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
};

use super::*;

unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

const RTLD_NOW: c_int = 2;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: *const c_void);
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    fn CVPixelBufferCreateWithIOSurface(
        allocator: *const c_void,
        surface: *const c_void,
        attributes: *const c_void,
        out: *mut *mut c_void,
    ) -> i32;
}

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    fn VTPixelTransferSessionCreate(allocator: *const c_void, session: *mut *mut c_void) -> i32;
    fn VTPixelTransferSessionTransferImage(
        session: *mut c_void,
        source: *mut c_void,
        destination: *mut c_void,
    ) -> i32;
}

/// TransformSurface's windows: the source's in 16.16, the destination's in pixels.
#[repr(C)]
struct Rects {
    src_x: u64,
    src_y: u64,
    src_width: u64,
    src_height: u64,
    dst_x: u32,
    dst_y: u32,
    dst_width: u32,
    dst_height: u32,
}

type CreateFn = unsafe extern "C" fn(*const c_void, *const c_void, *mut *mut c_void) -> i32;
type TransformFn = unsafe extern "C" fn(
    *mut c_void,
    *const c_void,
    *const c_void,
    *const c_void,
    *const Rects,
    *const c_void,
) -> i32;

/// Apple's scaler, through IOSurfaceAccelerator.
struct Apple {
    accelerator: *mut c_void,
    transform: TransformFn,
}

impl Apple {
    fn open() -> Option<Apple> {
        let path =
            c"/System/Library/PrivateFrameworks/IOSurfaceAccelerator.framework/IOSurfaceAccelerator";
        // SAFETY: dlopen and dlsym of a system framework's exported functions, whose signatures
        // are as declared above.
        unsafe {
            let handle = dlopen(path.as_ptr(), RTLD_NOW);
            if handle.is_null() {
                return None;
            }
            let create = dlsym(handle, c"IOSurfaceAcceleratorCreate".as_ptr());
            let transform = dlsym(handle, c"IOSurfaceAcceleratorTransformSurface".as_ptr());
            if create.is_null() || transform.is_null() {
                return None;
            }
            let create: CreateFn = std::mem::transmute(create);
            let transform: TransformFn = std::mem::transmute(transform);
            let mut accelerator = ptr::null_mut();
            let status = create(ptr::null(), ptr::null(), &raw mut accelerator);
            (status == 0 && !accelerator.is_null()).then_some(Apple {
                accelerator,
                transform,
            })
        }
    }

    fn run(
        &self,
        source: NonNull<c_void>,
        destination: NonNull<c_void>,
        case: &AbCase,
        plan: &Plan,
    ) -> i32 {
        let options = NSMutableDictionary::<NSString, AnyObject>::new();
        let bits = case.flags & 7;
        if bits != 0 {
            options.insert(&*NSString::from_str("Transform"), &*NSNumber::new_u64(bits));
        }
        if case.flags & 0x80 != 0 {
            options.insert(
                &*NSString::from_str("UseNearestFilter"),
                &*NSNumber::new_bool(true),
            );
        }
        let (s, d) = (plan.source_rect, plan.destination_rect);
        let rects = Rects {
            src_x: s.x,
            src_y: s.y,
            src_width: s.width,
            src_height: s.height,
            dst_x: d.x,
            dst_y: d.y,
            dst_width: d.width,
            dst_height: d.height,
        };
        // SAFETY: live surfaces and dictionary for the call, which is synchronous.
        unsafe {
            (self.transform)(
                self.accelerator,
                source.as_ptr(),
                destination.as_ptr(),
                Retained::as_ptr(&options).cast(),
                &raw const rects,
                ptr::null(),
            )
        }
    }
}

/// MPS's scale transform.
#[repr(C)]
#[derive(Clone, Copy)]
struct ScaleTransform {
    scale_x: f64,
    scale_y: f64,
    translate_x: f64,
    translate_y: f64,
}

// SAFETY: MPSScaleTransform's layout, four doubles.
unsafe impl Encode for ScaleTransform {
    const ENCODING: Encoding = Encoding::Struct(
        "MPSScaleTransform",
        &[f64::ENCODING, f64::ENCODING, f64::ENCODING, f64::ENCODING],
    );
}
// SAFETY: as above.
unsafe impl RefEncode for ScaleTransform {
    const ENCODING_REF: Encoding = Encoding::Pointer(&<Self as Encode>::ENCODING);
}

/// A texture over a plane of a surface, and its chroma step: 1, or 2 for a 4:2:0 chroma plane.
type Plane = (Retained<ProtocolObject<dyn MTLTexture>>, u32);

/// MPS's two scales, on the system's default device.
struct Mps {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
    lanczos: Retained<AnyObject>,
    bilinear: Retained<AnyObject>,
}

impl Mps {
    fn open() -> Option<Mps> {
        let path =
            c"/System/Library/Frameworks/MetalPerformanceShaders.framework/MetalPerformanceShaders";
        // SAFETY: loads a system framework.
        if unsafe { dlopen(path.as_ptr(), RTLD_NOW) }.is_null() {
            return None;
        }
        let device = MTLCreateSystemDefaultDevice()?;
        let queue = device.newCommandQueue()?;
        let make = |name: &std::ffi::CStr| -> Option<Retained<AnyObject>> {
            let class = AnyClass::get(name)?;
            // SAFETY: MPSImageScale's subclasses' initWithDevice:.
            unsafe {
                let allocated: Allocated<AnyObject> = msg_send![class, alloc];
                msg_send![allocated, initWithDevice: &*device]
            }
        };
        Some(Mps {
            lanczos: make(c"MPSImageLanczosScale")?,
            bilinear: make(c"MPSImageBilinearScale")?,
            device,
            queue,
        })
    }

    /// Textures over `surface`'s planes of `format`, `width` by `height`; `None` for a format MPS
    /// scales no plane of.
    fn planes(
        &self,
        surface: &IOSurfaceRef,
        format: ScalerFormat,
        width: u32,
        height: u32,
    ) -> Option<Vec<Plane>> {
        let (w, h) = (width.to_usize(), height.to_usize());
        let planes: Vec<(MTLPixelFormat, usize, usize, u32)> = match format {
            ScalerFormat::Bgra => vec![(MTLPixelFormat::BGRA8Unorm, w, h, 1)],
            ScalerFormat::Nv12VideoRange | ScalerFormat::Nv12FullRange => vec![
                (MTLPixelFormat::R8Unorm, w, h, 1),
                (MTLPixelFormat::RG8Unorm, w.div_ceil(2), h.div_ceil(2), 2),
            ],
            ScalerFormat::P010VideoRange | ScalerFormat::P010FullRange => vec![
                (MTLPixelFormat::R16Unorm, w, h, 1),
                (MTLPixelFormat::RG16Unorm, w.div_ceil(2), h.div_ceil(2), 2),
            ],
            _ => return None,
        };
        let mut out = Vec::new();
        for (plane, (pixel_format, pw, ph, step)) in planes.into_iter().enumerate() {
            // SAFETY: a descriptor of a 2D texture.
            let descriptor = unsafe {
                MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                    pixel_format,
                    pw,
                    ph,
                    false,
                )
            };
            descriptor.setUsage(MTLTextureUsage::ShaderRead | MTLTextureUsage::ShaderWrite);
            // SAFETY: a texture over a live surface's plane, which outlives it.
            let texture: Option<Retained<ProtocolObject<dyn MTLTexture>>> = unsafe {
                msg_send![
                    &*self.device,
                    newTextureWithDescriptor: &*descriptor,
                    iosurface: surface,
                    plane: plane
                ]
            };
            let texture = texture?;
            out.push((texture, step));
        }
        Some(out)
    }

    /// Scales each plane of `source` into `destination`'s with `kernel`, the plan's windows a
    /// plane's subsampling smaller; its wall and GPU times in ms.
    #[expect(
        clippy::as_conversions,
        clippy::cast_precision_loss,
        reason = "a bench's numbers"
    )]
    fn run(
        &self,
        kernel: &AnyObject,
        source: &[Plane],
        destination: &[Plane],
        plan: &Plan,
        runtime: &dyn Runtime,
    ) -> (f64, Option<f64>) {
        let started = runtime.now();
        let cb = self.queue.commandBuffer().unwrap();
        let (s, d) = (plan.source_rect, plan.destination_rect);
        let fixed = |v: u64| v as f64 / FIXED_ONE as f64;
        for ((from, step), (to, _)) in source.iter().zip(destination) {
            let k = f64::from(*step);
            let (sx, sy, sw, sh) = (
                fixed(s.x) / k,
                fixed(s.y) / k,
                fixed(s.width) / k,
                fixed(s.height) / k,
            );
            let (dx, dy, dw, dh) = (
                f64::from(d.x) / k,
                f64::from(d.y) / k,
                f64::from(d.width) / k,
                f64::from(d.height) / k,
            );
            let transform = ScaleTransform {
                scale_x: dw / sw,
                scale_y: dh / sh,
                translate_x: dx - sx * dw / sw,
                translate_y: dy - sy * dh / sh,
            };
            let clip = MTLRegion {
                origin: MTLOrigin {
                    x: dx.floor() as usize,
                    y: dy.floor() as usize,
                    z: 0,
                },
                size: MTLSize {
                    width: dw.ceil() as usize,
                    height: dh.ceil() as usize,
                    depth: 1,
                },
            };
            // SAFETY: MPSImageScale's scaleTransform, clipRect and encode, each with live
            // objects; the transform is copied by the set.
            unsafe {
                let () = msg_send![kernel, setScaleTransform: &raw const transform];
                let () = msg_send![kernel, setClipRect: clip];
                let () = msg_send![
                    kernel,
                    encodeToCommandBuffer: &*cb,
                    sourceTexture: &**from,
                    destinationTexture: &**to
                ];
            }
        }
        cb.commit();
        cb.waitUntilCompleted();
        let wall = runtime.elapsed_since(started).as_secs_f64() * 1000.0;
        let (start, end) = (cb.GPUStartTime(), cb.GPUEndTime());
        (
            wall,
            (end >= start && start > 0.0).then_some((end - start) * 1000.0),
        )
    }
}

/// VideoToolbox's transfer session.
struct Vt {
    session: *mut c_void,
}

impl Vt {
    fn open() -> Option<Vt> {
        let mut session = ptr::null_mut();
        // SAFETY: an out-parameter.
        let status = unsafe { VTPixelTransferSessionCreate(ptr::null(), &raw mut session) };
        (status == 0 && !session.is_null()).then_some(Vt { session })
    }
}

/// A CVPixelBuffer over an IOSurface, released when dropped.
struct PixelBuffer(*mut c_void);

impl PixelBuffer {
    fn over(surface: NonNull<c_void>) -> Option<PixelBuffer> {
        let mut out = ptr::null_mut();
        // SAFETY: a live IOSurface, which the buffer retains.
        let status = unsafe {
            CVPixelBufferCreateWithIOSurface(
                ptr::null(),
                surface.as_ptr(),
                ptr::null(),
                &raw mut out,
            )
        };
        (status == 0 && !out.is_null()).then_some(PixelBuffer(out))
    }
}

impl Drop for PixelBuffer {
    fn drop(&mut self) {
        // SAFETY: the buffer's +1.
        unsafe { CFRelease(self.0) };
    }
}

/// One case: a name, the formats and sizes, the source window (`x, y, width, height`, zeros for
/// the whole source), the destination window, and the transform's flags.
struct AbCase {
    name: &'static str,
    from: ScalerFormat,
    source: (u32, u32),
    to: ScalerFormat,
    destination: (u32, u32),
    crop: (f64, f64, f64, f64),
    rect: (u32, u32, u32, u32),
    flags: u64,
}

/// The kernel's bench cases, then every case of the conformance suite that scales.
fn ab_cases() -> Vec<AbCase> {
    use ScalerFormat::{Bgra, Nv12VideoRange as N, P010VideoRange, Yuyv};
    const WHOLE: (f64, f64, f64, f64) = (0.0, 0.0, 0.0, 0.0);
    const ALL: (u32, u32, u32, u32) = (0, 0, 0, 0);
    let c = |name, from, source, to, destination, flags| AbCase {
        name,
        from,
        source,
        to,
        destination,
        crop: WHOLE,
        rect: ALL,
        flags,
    };
    let fix = 0x1000;
    vec![
        c(
            "1080p 420v to 720p BGRA",
            N,
            (1920, 1080),
            Bgra,
            (1280, 720),
            fix,
        ),
        c(
            "2160p 420v to 1080p 420v",
            N,
            (3840, 2160),
            N,
            (1920, 1080),
            fix,
        ),
        c(
            "720p BGRA to 720p 420v",
            Bgra,
            (1280, 720),
            N,
            (1280, 720),
            fix,
        ),
        c(
            "1080p BGRA turned 90",
            Bgra,
            (1920, 1080),
            Bgra,
            (1080, 1920),
            fix | 4,
        ),
        c(
            "2160p 420v to 2160p BGRA",
            N,
            (3840, 2160),
            Bgra,
            (3840, 2160),
            fix,
        ),
        c("down2-420v", N, (1280, 720), N, (640, 360), fix),
        c("down4-BGRA", Bgra, (1280, 720), Bgra, (320, 180), fix),
        c("up4-420v", N, (320, 180), N, (1280, 720), fix),
        c("up8-BGRA", Bgra, (160, 90), Bgra, (1280, 720), fix),
        c(
            "scale-csc-420v-BGRA",
            N,
            (1920, 1080),
            Bgra,
            (1280, 720),
            fix,
        ),
        c(
            "scale-csc-BGRA-420v",
            Bgra,
            (1920, 1080),
            N,
            (1280, 720),
            fix,
        ),
        c(
            "scale-x420-420v",
            P010VideoRange,
            (1920, 1080),
            N,
            (960, 540),
            fix,
        ),
        AbCase {
            crop: (160.0, 90.0, 640.0, 360.0),
            ..c("crop-420v", N, (1280, 720), N, (320, 180), fix)
        },
        AbCase {
            crop: (80.5, 40.25, 320.0, 180.0),
            ..c(
                "crop-fraction-BGRA",
                Bgra,
                (640, 360),
                Bgra,
                (320, 180),
                fix,
            )
        },
        AbCase {
            rect: (320, 180, 320, 180),
            ..c("rect-scale-BGRA", Bgra, (640, 360), Bgra, (1280, 720), fix)
        },
        c(
            "nearest-up-BGRA",
            Bgra,
            (160, 120),
            Bgra,
            (480, 360),
            fix | 0x80,
        ),
        c(
            "nearest-down-420v",
            N,
            (640, 480),
            N,
            (320, 240),
            fix | 0x80,
        ),
        c("odd-420v", N, (321, 241), N, (161, 121), fix),
        c("odd-yuvs-BGRA", Yuyv, (321, 241), Bgra, (161, 121), fix),
        c(
            "turn-aniso-BGRA",
            Bgra,
            (320, 240),
            Bgra,
            (240, 160),
            fix | 4,
        ),
        c(
            "turn-1080-half-420v",
            N,
            (1920, 1080),
            N,
            (540, 960),
            fix | 4,
        ),
        AbCase {
            crop: (13.0, 7.0, 99.0, 101.0),
            ..c(
                "odd-crop-rotate-BGRA",
                Bgra,
                (320, 240),
                Bgra,
                (101, 99),
                fix | 6,
            )
        },
        c(
            "rotate-scale-BGRA",
            Bgra,
            (320, 240),
            Bgra,
            (120, 160),
            fix | 4,
        ),
        c("transfer-up2-420v", N, (320, 180), N, (640, 360), 0),
        c("transfer-up2-BGRA", Bgra, (160, 120), Bgra, (320, 240), 0),
        c("transfer-up1.5-BGRA", Bgra, (320, 240), Bgra, (480, 360), 0),
        c("transfer-up4-420v", N, (320, 180), N, (1280, 720), 0),
        c("transfer-up8-BGRA", Bgra, (160, 90), Bgra, (1280, 720), 0),
        c("transfer-down2-BGRA", Bgra, (640, 480), Bgra, (320, 240), 0),
        c(
            "transfer-csc-up-420v-BGRA",
            N,
            (640, 360),
            Bgra,
            (1280, 720),
            0,
        ),
        c(
            "transfer-turn-up-BGRA",
            Bgra,
            (160, 120),
            Bgra,
            (240, 320),
            4,
        ),
        c(
            "transfer-odd-up-BGRA",
            Bgra,
            (321, 241),
            Bgra,
            (642, 482),
            0,
        ),
    ]
}

/// The median of `times`; NaN for none.
fn median(times: &[f64]) -> f64 {
    let mut sorted = times.to_vec();
    sorted.sort_by(f64::total_cmp);
    sorted.get(sorted.len() / 2).copied().unwrap_or(f64::NAN)
}

/// What one engine took: wall times, and GPU times where the GPU says.
#[derive(Default)]
struct Times {
    wall: Vec<f64>,
    gpu: Vec<f64>,
}

impl Times {
    fn show(&self) -> String {
        if self.wall.is_empty() {
            return "-".to_owned();
        }
        // The median wall time, the quartiles about it, and the median GPU time.
        let mut sorted = self.wall.clone();
        sorted.sort_by(f64::total_cmp);
        let q = |f: usize| sorted[(sorted.len() - 1) * f / 4];
        let spread = format!("[{:.3}-{:.3}]", q(1), q(3));
        if self.gpu.is_empty() {
            format!("{:.3} {spread}", median(&self.wall))
        } else {
            format!(
                "{:.3} {spread} ({:.3})",
                median(&self.wall),
                median(&self.gpu)
            )
        }
    }
}

/// The kernels beside this Mac's own scalers, interleaved, on the same surfaces (module docs)
#[test]
#[ignore = "an A/B bench against the Mac's own scalers: run alone, in release"]
fn the_kernel_against_the_macs_scalers() {
    let runtime = StdRuntime::shared();
    let gpu = metal::MetalGpu::process().expect("Metal opens the kernel");
    let gpu_name = gpu.name().to_owned();
    let mut ours = GpuScalerBackend::new(gpu, Arc::clone(&runtime))
        .create_accelerator()
        .unwrap();
    let apple = Apple::open();
    let mps = Mps::open();
    let vt = Vt::open();
    println!(
        "ab: Metal, {gpu_name}; Apple's scaler {}, MPS {}, VideoToolbox {}",
        apple.is_some(),
        mps.is_some(),
        vt.is_some()
    );
    println!(
        "ab columns: case | ours in place, wall (GPU) ms | ours staged | Apple's scaler | \
         VTPixelTransferSession | MPS Lanczos | MPS bilinear | ours / fastest native but MPS bilinear, wall"
    );
    for case in ab_cases() {
        let (sw, sh) = case.source;
        let (dw, dh) = case.destination;
        let (sl, dl) = (
            layout(case.from, sw, sh, 64, 0),
            layout(case.to, dw, dh, 64, 0),
        );
        let source = platform_surface(sl.clone(), &picture_bytes(&sl, None), None);
        let grey = picture_bytes(&dl, Some([0.5, 0.5, 0.5, 1.0]));
        let destination = platform_surface(dl.clone(), &grey, None);
        let staged_source = memory(sl.clone(), None);
        let staged_destination = memory(dl.clone(), Some([0.5, 0.5, 0.5, 1.0]));
        let fixed = |v: f64| {
            #[expect(
                clippy::as_conversions,
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                clippy::cast_precision_loss,
                reason = "a window's 16.16"
            )]
            let f = (v * FIXED_ONE as f64).round() as u64;
            f
        };
        let transform = Transform {
            source_rect: FixedRect {
                x: fixed(case.crop.0),
                y: fixed(case.crop.1),
                width: fixed(case.crop.2),
                height: fixed(case.crop.3),
            },
            destination_rect: Rect {
                x: case.rect.0,
                y: case.rect.1,
                width: case.rect.2,
                height: case.rect.3,
            },
            flags: TransformFlags(case.flags),
            ..Transform::default()
        };
        let Ok(plan) = Plan::new(&transform, &sl, &dl) else {
            println!("ab {}: no plan", case.name);
            continue;
        };
        let surface_of = |s: &Platform| {
            s.memory
                .as_ref()
                .and_then(|m| m.io_surface().ok())
                .map(|l| l.as_ptr())
        };
        let (Some(src_ref), Some(dst_ref)) = (surface_of(&source), surface_of(&destination)) else {
            println!("ab {}: no IOSurfaces", case.name);
            continue;
        };
        let turns = case.flags & 7 != 0;
        let whole = case.crop.2 == 0.0 && case.rect.2 == 0;
        // SAFETY: the surfaces outlive the textures and the pixel buffers made over them.
        let as_ref = |p: NonNull<c_void>| unsafe { p.cast::<IOSurfaceRef>().as_ref() };
        let textures = mps
            .as_ref()
            .filter(|_| case.from == case.to && !turns)
            .and_then(|m| {
                Some((
                    m.planes(as_ref(src_ref), case.from, sw, sh)?,
                    m.planes(as_ref(dst_ref), case.to, dw, dh)?,
                ))
            });
        let buffers = vt
            .as_ref()
            .filter(|_| !turns && whole)
            .and_then(|_| Some((PixelBuffer::over(src_ref)?, PixelBuffer::over(dst_ref)?)));
        let mut times: [Times; 6] = Default::default();
        let mut once = |engine: usize, record: bool, times: &mut [Times; 6]| -> bool {
            let (wall, gpu_ms) = match engine {
                0 | 1 => {
                    let (s, d): (&dyn GuestSurface, &dyn GuestSurface) = if engine == 0 {
                        (&source, &destination)
                    } else {
                        (&staged_source, &staged_destination)
                    };
                    let mut job = TransformJob {
                        transform: &transform,
                        plan: plan.clone(),
                        source: s,
                        destination: d,
                        hold: GuestHold::nothing(),
                        inputs: Default::default(),
                        outputs: Default::default(),
                    };
                    GPU_TIMES.lock().clear();
                    let started = runtime.now();
                    if ours.transform(&mut job) != io_return::SUCCESS {
                        return false;
                    }
                    let wall = runtime.elapsed_since(started).as_secs_f64() * 1000.0;
                    (
                        wall,
                        GPU_TIMES.lock().first().map(|t| t.as_secs_f64() * 1000.0),
                    )
                }
                2 => {
                    let Some(apple) = &apple else { return false };
                    let started = runtime.now();
                    if apple.run(src_ref, dst_ref, &case, &plan) != 0 {
                        return false;
                    }
                    (runtime.elapsed_since(started).as_secs_f64() * 1000.0, None)
                }
                3 => {
                    let (Some(vt), Some((s, d))) = (&vt, &buffers) else {
                        return false;
                    };
                    let started = runtime.now();
                    // SAFETY: live pixel buffers and session.
                    if unsafe { VTPixelTransferSessionTransferImage(vt.session, s.0, d.0) } != 0 {
                        return false;
                    }
                    (runtime.elapsed_since(started).as_secs_f64() * 1000.0, None)
                }
                _ => {
                    let (Some(m), Some((s, d))) = (&mps, &textures) else {
                        return false;
                    };
                    let kernel = if engine == 4 { &m.lanczos } else { &m.bilinear };
                    m.run(kernel, s, d, &plan, runtime.as_ref())
                }
            };
            if record {
                times[engine].wall.push(wall);
                times[engine].gpu.extend(gpu_ms);
            }
            true
        };
        let mut runs = [false; 6];
        for (engine, ran) in runs.iter_mut().enumerate() {
            *ran = once(engine, false, &mut times);
            if *ran {
                once(engine, false, &mut times);
            }
        }
        for _ in 0..8 {
            for (engine, ran) in runs.iter().enumerate() {
                if *ran {
                    for _ in 0..3 {
                        once(engine, true, &mut times);
                    }
                }
            }
        }
        // MPS's bilinear scale is shown, as the floor of a submission, and not counted.
        let fastest = (2..5)
            .filter(|e| runs[*e])
            .map(|e| median(&times[e].wall))
            .fold(f64::INFINITY, f64::min);
        println!(
            "ab {}: {} | {} | {} | {} | {} | {} | {:.2}x",
            case.name,
            times[0].show(),
            times[1].show(),
            times[2].show(),
            times[3].show(),
            times[4].show(),
            times[5].show(),
            median(&times[0].wall) / fastest
        );
    }
}
