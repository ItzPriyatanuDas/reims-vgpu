//! [`MetalSink`]: what draws a window display's frames on the product's platform, straight into
//! the window's drawable, which it presents in the same command buffer.

use std::any::Any;
use std::sync::{Arc, Weak};

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::{MTLCommandBuffer as _, MTLCommandQueue as _, MTLDevice, MTLDrawable};
use objc2_quartz_core::CAMetalDrawable;
use reims_vgpu_core::display::Display as CoreDisplay;
use reims_vgpu_core::platform::Platform;
use reims_vgpu_core::platform::device::GpuDevice as _;
use reims_vgpu_core::raise::OnFault;
use reims_vgpu_metal::{MetalPlatform, Obj, msg};
use reims_vgpu_window::{DrawnCursor, FrameSink, PixelRect};

use super::{SinkDisplay, WindowFrame};
use crate::display::no_catch;
use crate::entry;

/// The sink of a display of the product's platform ([`super::window_sink`]): a [`MetalSink`] over
/// `display`, drawn in `frame_queue`, or `None` without a frame queue. `display` and
/// `frame_queue` back, unchanged, for a display of another platform.
#[expect(
    clippy::type_complexity,
    reason = "the two values handed back, as they came"
)]
pub(super) fn sink(
    display: Box<dyn Any>,
    frame_queue: Box<dyn Any>,
) -> Result<Option<Arc<dyn FrameSink>>, (Box<dyn Any>, Box<dyn Any>)> {
    let display = match display.downcast::<Weak<CoreDisplay<MetalPlatform>>>() {
        Ok(display) => display,
        Err(display) => return Err((display, frame_queue)),
    };
    let Ok(frame_queue) =
        frame_queue.downcast::<Option<<MetalPlatform as Platform>::CommandQueue>>()
    else {
        return Ok(None);
    };
    Ok((*frame_queue).map(|queue| {
        let sink: Arc<dyn FrameSink> = Arc::new(MetalSink::new(*display, queue));
        sink
    }))
}

impl<P: Platform> SinkDisplay<P> {
    /// `device`, the Metal device the window's layer draws with, while the display lives
    /// ([`FrameSink::device`]).
    pub(crate) fn layer_device(
        &self,
        device: &<MetalPlatform as Platform>::Device,
    ) -> Option<Retained<ProtocolObject<dyn MTLDevice>>> {
        (self.display.strong_count() > 0).then(|| device.as_retained().clone())
    }
}

/// `drawable`'s texture, which both sinks' [`FrameSink::draw`] draw into, as a handle.
pub(crate) fn drawable_texture(
    drawable: &ProtocolObject<dyn CAMetalDrawable>,
) -> <MetalPlatform as Platform>::Texture {
    Obj::from_retained(msg::get("texture", || drawable.texture()))
}

/// What draws a display's current frame into its window's drawable, on Metal: the display and
/// its frame queue ([`SinkDisplay`]), and the queue's Metal device, which the window's layer
/// draws with. Neither the queue nor the device keeps the display alive.
pub(crate) struct MetalSink {
    /// The display, held only for a draw (the `window` module's doc), and the frame queue.
    display: SinkDisplay<MetalPlatform>,
    /// The queue's Metal device: the display's.
    device: <MetalPlatform as Platform>::Device,
}

impl MetalSink {
    /// A sink over `display`, drawing in command buffers of `queue`, the frame queue of the
    /// display's Metal device.
    pub(crate) fn new(
        display: Weak<CoreDisplay<MetalPlatform>>,
        queue: <MetalPlatform as Platform>::CommandQueue,
    ) -> MetalSink {
        let device = Obj::from_retained(msg::get("device", || queue.device()));
        MetalSink {
            display: SinkDisplay::new(display, queue, own_texture),
            device,
        }
    }

    /// Encodes the display's current frame into `texture` over `region`, with `cursor` over it,
    /// in a new command buffer of the frame queue, presents `present` in it when there is one,
    /// and commits ([`SinkDisplay::encode`], whose `false` and lifetimes are this one's).
    pub(crate) fn encode_into(
        &self,
        texture: &<MetalPlatform as Platform>::Texture,
        region: PixelRect,
        cursor: Option<&DrawnCursor>,
        present: Option<&ProtocolObject<dyn CAMetalDrawable>>,
    ) -> bool {
        self.display
            .encode(region, cursor, MetalFrame { texture, present })
    }
}

/// The guest's cursor glyph's texture on Metal: a plain one, a resource of Metal's as the layer's
/// drawables are (`super::cursor`'s module doc).
pub(crate) fn own_texture(
    device: &<MetalPlatform as Platform>::Device,
    descriptor: &<MetalPlatform as Platform>::TextureDescriptor,
) -> Option<<MetalPlatform as Platform>::Texture> {
    no_catch(device.new_texture(OnFault::Die, descriptor))
}

/// A frame of [`MetalSink`]'s: encoded straight into `texture`, the drawable presented in the
/// same command buffer.
struct MetalFrame<'a> {
    /// The texture the frame is encoded into: the drawable's.
    texture: &'a <MetalPlatform as Platform>::Texture,
    /// The drawable to present, when there is one.
    present: Option<&'a ProtocolObject<dyn CAMetalDrawable>>,
}

impl WindowFrame<MetalPlatform> for MetalFrame<'_> {
    fn texture(&mut self) -> Option<&<MetalPlatform as Platform>::Texture> {
        Some(self.texture)
    }

    /// Presents the drawable in the command buffer, when there is one.
    fn encoded(&mut self, command_buffer: &<MetalPlatform as Platform>::CommandBuffer) {
        if let Some(drawable) = self.present {
            let drawable: &ProtocolObject<dyn MTLDrawable> = ProtocolObject::from_ref(drawable);
            msg::get("presentDrawable:", || {
                command_buffer.get().presentDrawable(drawable);
            });
        }
    }
}

impl FrameSink for MetalSink {
    /// The frame queue's Metal device, while the display lives.
    fn device(&self) -> Option<Retained<ProtocolObject<dyn MTLDevice>>> {
        self.display.layer_device(&self.device)
    }

    /// The drawable's texture, then [`MetalSink::encode_into`] with the drawable to present.
    fn draw(
        &self,
        drawable: &ProtocolObject<dyn CAMetalDrawable>,
        region: PixelRect,
        cursor: Option<&DrawnCursor>,
    ) -> bool {
        entry::run(|_| {
            self.encode_into(&drawable_texture(drawable), region, cursor, Some(drawable))
        })
    }

    /// The display's ([`SinkDisplay::cursor_position`]).
    fn guest_cursor_position(&self) -> Option<(u16, u16)> {
        entry::run(|_| self.display.cursor_position())
    }
}
