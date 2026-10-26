//! The Vulkan platform's window sink on macOS: the swapchain's image of a frame is what the
//! display's compose draws into a texture of the window's size, with no copy between.

use std::ffi::c_void;
use std::sync::Arc;

use ash::vk;
use objc2::rc::Retained;
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_metal::MTLTextureUsage;
use objc2_quartz_core::CAMetalLayer;
use reims_vgpu_core::display::{GammaTableArgs, MINIMUM_TEXTURE_USAGE};
use reims_vgpu_core::num::ToU64 as _;
use reims_vgpu_core::platform::descriptor::TextureDescriptor as _;
use reims_vgpu_core::platform::device::GpuDevice as _;
use reims_vgpu_core::platform::resource::Texture as _;
use reims_vgpu_core::platform::types::{MtlOrigin, MtlRegion, MtlSize};
use reims_vgpu_core::platform::{Handle as _, HostRange, Platform};
use reims_vgpu_core::raise::OnFault;
use reims_vgpu_core::rt::Hold;
use reims_vgpu_core::tasks::isolation::TaskThreads;
use reims_vgpu_vulkan::VulkanPlatform;
use reims_vgpu_vulkan::swapchain::Swapchain;
use reims_vgpu_window::{PixelRect, VulkanTarget};

use super::super::swapchain::own_texture;
use super::super::swapchain::tests::{draw_and_read_back, read_back};
use crate::PixelFormat;
use crate::display::no_catch;
use crate::entry;
use crate::platform::api_state;
use crate::window::{SinkDisplay, WindowFrame};
use crate::{Device, DeviceConfig, Display, DisplayConfig};

/// The platform's command queue.
type Queue = <VulkanPlatform as Platform>::CommandQueue;
/// Its command buffer.
type CommandBuffer = <VulkanPlatform as Platform>::CommandBuffer;
/// Its texture.
type Texture = <VulkanPlatform as Platform>::Texture;

/// `MTLResourceStorageModeShared`, with the default CPU cache and hazard tracking modes.
const SHARED: u64 = 0;

/// `MTLResourceStorageModePrivate`: a drawable's storage.
const PRIVATE: u64 = 0x20;

/// The frame queue of `device`'s Metal device.
fn frame_queue(device: &Device<VulkanPlatform>) -> Queue {
    entry::run(|_| {
        api_state(device.platform())
            .frame_queue(device.core().mtl_device_ref())
            .unwrap()
    })
}

/// A Vulkan device on a platform of its own, and a display on its port 0 that takes
/// presents: one with a new-frame callback.
fn presenting_display(serial: u32) -> (Device<VulkanPlatform>, Display<VulkanPlatform>) {
    let platform = Arc::new(VulkanPlatform::new(Box::new(TaskThreads::new())));
    let device = Device::with_platform(platform, DeviceConfig::default())
        .expect("a Vulkan device (MoltenVK)");
    let mut c = DisplayConfig::new(0, serial);
    c.callbacks.new_frame = Some(Arc::new(|| {}));
    let display = device.new_display(c).unwrap();
    (device, display)
}

/// Presents to `display` a frame of `(width, height)`, BGRA, of every colour channel a
/// gradient of its own, opaque: what the window draws next.
fn present_a_fixed_frame(
    device: &Device<VulkanPlatform>,
    display: &Display<VulkanPlatform>,
    (width, height): (u64, u64),
) {
    let bytes: Vec<u8> = (0..height)
        .flat_map(|y| (0..width).map(move |x| (x, y)))
        .flat_map(|(x, y)| {
            let cut = |v: u64| u8::try_from(v % 256).unwrap();
            [
                cut(x * 255 / (width - 1)),
                cut(y * 255 / (height - 1)),
                cut((x * 7 + y * 13) % 256),
                0xff,
            ]
        })
        .collect();
    entry::run(|_| {
        let format = u64::from(PixelFormat::Bgra8Unorm.metal());
        let td = no_catch(device.platform().texture_2d_descriptor(
            OnFault::Die,
            format,
            width,
            height,
            false,
        ));
        td.set_usage(MTLTextureUsage::ShaderRead.0.to_u64());
        td.set_resource_options(SHARED);
        let frame = no_catch(
            device
                .core()
                .mtl_device_ref()
                .new_texture(OnFault::Die, &td),
        )
        .unwrap();
        no_catch(frame.replace_region(
            MtlRegion::new(MtlOrigin::new(0, 0, 0), MtlSize::new(width, height, 1)),
            0,
            0,
            HostRange::of_slice(&bytes),
            width * 4,
            width * 4 * height,
        ));
        no_catch(display.core().present_frame(
            Hold::NONE,
            OnFault::Die,
            Some(frame),
            u32::from_be_bytes(*b"BGRA"),
            0,
            None,
            GammaTableArgs::default(),
            None,
            Box::new(|| {}),
        ));
    });
}

/// A `CAMetalLayer` of `(width, height)` pixels at a scale of 1, in no window: what a
/// swapchain over it sizes its images by.
fn a_layer((width, height): (u64, u64)) -> Retained<CAMetalLayer> {
    let layer = CAMetalLayer::new();
    layer.setBounds(CGRect::new(
        CGPoint::new(0.0, 0.0),
        CGSize::new(
            f64::from(u32::try_from(width).unwrap()),
            f64::from(u32::try_from(height).unwrap()),
        ),
    ));
    layer.setContentsScale(1.0);
    layer
}

/// A layer as the window's render thread gives it: a surface of it is made with
/// `VK_EXT_metal_surface` (as `reims_vgpu_window`'s own target makes it).
struct LayerTarget<'a>(&'a CAMetalLayer);

impl VulkanTarget for LayerTarget<'_> {
    fn create_surface(
        &self,
        get_instance_proc_addr: vk::PFN_vkGetInstanceProcAddr,
        instance: vk::Instance,
    ) -> Option<vk::SurfaceKHR> {
        // SAFETY: a live instance's look-up of an instance command by its name.
        let found =
            unsafe { get_instance_proc_addr(instance, c"vkCreateMetalSurfaceEXT".as_ptr()) }?;
        // SAFETY: the command the loader answers for the name.
        let create: vk::PFN_vkCreateMetalSurfaceEXT = unsafe { std::mem::transmute(found) };
        let layer: *const c_void = std::ptr::from_ref(self.0).cast();
        let info = vk::MetalSurfaceCreateInfoEXT::default().layer(layer);
        let mut surface = vk::SurfaceKHR::null();
        // SAFETY: a valid create info naming a live layer, which outlives the surface.
        let made = unsafe { create(instance, &info, std::ptr::null(), &mut surface) };
        (made == vk::Result::SUCCESS).then_some(surface)
    }
}

/// A frame drawn into a texture of the test's own, as a sink draws one into its target: the
/// command buffer it was encoded in, once committed.
struct Plain<'a> {
    /// The texture.
    texture: &'a Texture,
    /// The frame's command buffer.
    committed: Option<CommandBuffer>,
}

impl WindowFrame<VulkanPlatform> for &mut Plain<'_> {
    fn texture(&mut self) -> Option<&Texture> {
        Some(self.texture)
    }

    fn encoded(&mut self, command_buffer: &CommandBuffer) {
        self.committed = Some(command_buffer.clone_ref());
    }
}

/// What the display's compose draws of its current frame over `region` into a private BGRA
/// texture of `size`, a drawable's format and storage, read back.
fn composed(
    device: &Device<VulkanPlatform>,
    display: &Display<VulkanPlatform>,
    queue: &Queue,
    size: (u64, u64),
    region: PixelRect,
) -> Vec<u8> {
    let texture = entry::run(|_| {
        let format = u64::from(PixelFormat::Bgra8Unorm.metal());
        let td = no_catch(device.platform().texture_2d_descriptor(
            OnFault::Die,
            format,
            size.0,
            size.1,
            false,
        ));
        td.set_usage(MINIMUM_TEXTURE_USAGE);
        td.set_resource_options(PRIVATE);
        no_catch(
            device
                .core()
                .mtl_device_ref()
                .new_texture(OnFault::Die, &td),
        )
        .unwrap()
    });
    let sink = SinkDisplay::new(display.core().downgrade(), queue.clone_ref(), own_texture);
    let mut plain = Plain {
        texture: &texture,
        committed: None,
    };
    assert!(sink.encode(region, None, &mut plain), "the frame is drawn");
    assert!(plain.committed.is_some());
    read_back(queue, &texture, size)
}

/// The swapchain's image of a frame is, byte for byte, what the display's compose draws into a
/// texture of the window's size and format: a fixed frame scaled into a region of the window,
/// its rest cleared, so the same format, colour and scaling, with no copy between. (The staging
/// sink the swapchain replaced drew the same bytes: the comparison of the two passed before it
/// went.) The frame is one the display draws visibly, not all the clear
#[test]
fn the_swapchain_holds_the_composed_frame() {
    let (device, display) = presenting_display(0x5eed_7b70);
    present_a_fixed_frame(&device, &display, (64, 40));
    let size = (61, 37);
    let region = PixelRect {
        x: 4,
        y: 3,
        width: 50,
        height: 30,
    };
    let queue = frame_queue(&device);
    let theirs = composed(&device, &display, &queue, size, region);

    let layer = a_layer(size);
    let made = entry::run(|_| {
        // SAFETY: the target makes the surface on the instance it is given, and hands it over;
        // the layer outlives the swapchain.
        unsafe {
            Swapchain::over(&queue, |get_instance_proc_addr, instance| {
                LayerTarget(&layer).create_surface(get_instance_proc_addr, instance)
            })
        }
    });
    let mut swapchain = made.unwrap();
    let ours = draw_and_read_back(
        display.core().downgrade(),
        &queue,
        &mut swapchain,
        size,
        region,
    );
    entry::run(|_| drop(swapchain));
    drop(layer);

    let black = [0, 0, 0, 0xff];
    assert!(
        theirs.chunks(4).any(|p| p != black),
        "the frame shows: not all the clear"
    );
    assert_eq!(ours.len(), theirs.len());
    let first = (ours.chunks(4).zip(theirs.chunks(4)))
        .position(|(a, b)| a != b)
        .map(|i| {
            (
                i % 61,
                i / 61,
                &ours[i * 4..i * 4 + 4],
                &theirs[i * 4..i * 4 + 4],
            )
        });
    assert_eq!(
        first, None,
        "the first pixel that differs: (x, y, ours, theirs)"
    );
}

/// What a window frame of `size` pixels shows over `region`, read back: the display's fixed
/// frame of `frame_size` alone, then with `cursor` drawn over it, as a sink draws it
/// ([`SinkDisplay::encode`]).
fn a_frame_with_the_cursor(
    serial: u32,
    frame_size: (u64, u64),
    size: (u64, u64),
    region: PixelRect,
    cursor: &reims_vgpu_window::DrawnCursor,
) -> (Vec<u8>, Vec<u8>) {
    let (device, display) = presenting_display(serial);
    present_a_fixed_frame(&device, &display, frame_size);
    let queue = frame_queue(&device);
    let frame = composed(&device, &display, &queue, size, region);
    let texture = entry::run(|_| {
        let format = u64::from(PixelFormat::Bgra8Unorm.metal());
        let td = no_catch(device.platform().texture_2d_descriptor(
            OnFault::Die,
            format,
            size.0,
            size.1,
            false,
        ));
        td.set_usage(MINIMUM_TEXTURE_USAGE);
        td.set_resource_options(PRIVATE);
        no_catch(
            device
                .core()
                .mtl_device_ref()
                .new_texture(OnFault::Die, &td),
        )
        .unwrap()
    });
    let sink = SinkDisplay::new(display.core().downgrade(), queue.clone_ref(), own_texture);
    let mut plain = Plain {
        texture: &texture,
        committed: None,
    };
    assert!(
        sink.encode(region, Some(cursor), &mut plain),
        "the frame is drawn"
    );
    (frame, read_back(&queue, &texture, size))
}

/// Pixel `(x, y)` of `bytes`, rows of `width` BGRA pixels.
fn pixel(bytes: &[u8], width: u64, (x, y): (u64, u64)) -> [u8; 4] {
    let i = usize::try_from((y * width + x) * 4).unwrap();
    bytes[i..i + 4].try_into().unwrap()
}

/// Four rows of four: opaque red, opaque green, blue at alpha 0x80, white at alpha 0.
const ROWS: [[u8; 4]; 4] = [
    [0xff, 0, 0, 0xff],
    [0, 0xff, 0, 0xff],
    [0, 0, 0xff, 0x80],
    [0xff, 0xff, 0xff, 0],
];

/// The cursor of [`ROWS`]'s glyph, its hot spot at `hot_spot`, at `at` in a mode of 64 by 40.
fn rows_cursor(hot_spot: (u16, u16), at: (u16, u16)) -> reims_vgpu_window::DrawnCursor {
    reims_vgpu_window::DrawnCursor {
        glyph: Arc::new(reims_vgpu_window::Glyph {
            width: 4,
            height: 4,
            hot_spot,
            rgba: ROWS.iter().flat_map(|row| row.repeat(4)).collect(),
        }),
        mode: (64, 40),
        at,
    }
}

/// `drawn` is `frame`, of `size` pixels, with [`ROWS`]'s glyph over it a pixel to a pixel, its
/// top left at `(left, top)`, wherever it lies inside `region`, each pixel over the frame by the
/// glyph's alpha (opaque rows the glyph's colour, a half-transparent row half over the frame, a
/// transparent row the frame's, whatever its colour bytes); everywhere else it is `frame`.
fn assert_rows_over(
    frame: &[u8],
    drawn: &[u8],
    size: (u64, u64),
    region: PixelRect,
    (left, top): (i64, i64),
) {
    for y in 0..size.1 {
        for x in 0..size.0 {
            let (under, got) = (pixel(frame, size.0, (x, y)), pixel(drawn, size.0, (x, y)));
            let in_region = (region.x..region.x + region.width).contains(&x)
                && (region.y..region.y + region.height).contains(&y);
            let (gx, gy) = (
                i64::try_from(x).unwrap() - left,
                i64::try_from(y).unwrap() - top,
            );
            if !(in_region && (0..4).contains(&gx) && (0..4).contains(&gy)) {
                assert_eq!(got, under, "outside the glyph, ({x}, {y})");
                continue;
            }
            let [r, g, b, a] = ROWS[usize::try_from(gy).unwrap()];
            // The target is BGRA; each channel the glyph's over the frame's by the glyph's alpha.
            let over = |glyph: u8, frame: u8| {
                (f64::from(glyph) * f64::from(a) + f64::from(frame) * f64::from(255 - a)) / 255.0
            };
            for (channel, (want, have)) in [over(b, under[0]), over(g, under[1]), over(r, under[2])]
                .into_iter()
                .zip(got)
                .enumerate()
            {
                assert!(
                    (want - f64::from(have)).abs() <= 1.0,
                    "({x}, {y}) channel {channel}: {have} for {want:.1}, over {under:?}"
                );
            }
        }
    }
}

/// The guest's cursor, drawn over a frame where the window draws it: its hot spot at the guest's
/// position, a glyph pixel to a guest pixel at a 1:1 image, each pixel over the frame by its
/// alpha, and the rest of the frame untouched ([`assert_rows_over`])
#[test]
fn the_guest_s_cursor_is_drawn_over_the_frame_by_its_alpha() {
    let size = (64, 40);
    let region = PixelRect {
        x: 0,
        y: 0,
        width: size.0,
        height: size.1,
    };
    let cursor = rows_cursor((1, 1), (10, 20));
    let (frame, drawn) = a_frame_with_the_cursor(0x5eed_7b71, size, size, region, &cursor);
    // The glyph's top left is the hot spot's place less the hot spot: (9, 19).
    assert_rows_over(&frame, &drawn, size, region, (9, 19));
}

/// In an image letterboxed in the window, the glyph is placed from the image's top left, and a
/// glyph past the image's top left edge is clipped there: the bars beside it keep the frame's
/// clear, though the glyph's viewport reaches over them
#[test]
fn the_guest_s_cursor_is_clipped_to_a_letterboxed_image() {
    let size = (80, 50);
    let region = PixelRect {
        x: 6,
        y: 4,
        width: 64,
        height: 40,
    };
    let cursor = rows_cursor((1, 1), (0, 0));
    let (frame, drawn) = a_frame_with_the_cursor(0x5eed_7b73, (64, 40), size, region, &cursor);
    // The glyph's top left is (6 - 1, 4 - 1); its first row and column fall on the bars.
    assert_rows_over(&frame, &drawn, size, region, (5, 3));
    assert_ne!(
        pixel(&drawn, size.0, (6, 4)),
        pixel(&frame, size.0, (6, 4)),
        "the glyph's second row and column, inside the image"
    );
}

/// Where the image scales the glyph, its filtered edge is the glyph's coverage of the frame: at
/// 2 pixels a guest pixel, an opaque white texel beside a transparent black one gives the pixels
/// between them a quarter and three quarters of white over the frame, not the darker values
/// filtering straight alpha would give (9/16 and 1/16 of white)
#[test]
fn a_scaled_glyph_s_edges_are_its_coverage_of_white() {
    use reims_vgpu_window::{DrawnCursor, Glyph};

    let size = (128, 80);
    let region = PixelRect {
        x: 0,
        y: 0,
        width: size.0,
        height: size.1,
    };
    let cursor = DrawnCursor {
        glyph: Arc::new(Glyph {
            width: 2,
            height: 1,
            hot_spot: (0, 0),
            rgba: vec![0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0],
        }),
        mode: (64, 40),
        at: (10, 20),
    };
    let (frame, drawn) = a_frame_with_the_cursor(0x5eed_7b72, (64, 40), size, region, &cursor);
    // The glyph covers pixels 20..24 of rows 40 and 41; the samples of the pixels between its
    // texels' centres weigh the white texel 3/4 (x 21) and 1/4 (x 22).
    for y in [40, 41] {
        for (x, white) in [(20, 1.0), (21, 0.75), (22, 0.25), (23, 0.0)] {
            let (under, got) = (pixel(&frame, size.0, (x, y)), pixel(&drawn, size.0, (x, y)));
            for (channel, (&have, &frame)) in got.iter().zip(&under).take(3).enumerate() {
                let want = 255.0 * white + f64::from(frame) * (1.0 - white);
                assert!(
                    (want - f64::from(have)).abs() <= 2.0,
                    "({x}, {y}) channel {channel}: {have} for {want:.1}, over {under:?}"
                );
            }
        }
    }
    for (x, y) in [(19, 40), (24, 40), (20, 39), (20, 42)] {
        assert_eq!(
            pixel(&drawn, size.0, (x, y)),
            pixel(&frame, size.0, (x, y)),
            "outside the glyph, ({x}, {y})"
        );
    }
}
