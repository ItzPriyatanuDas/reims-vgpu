//! What macOS's window asks of the Vulkan platform's sink ([`FrameSink`]): it presents itself,
//! through a presenter over a swapchain of the window's `CAMetalLayer`
//! ([`SwapchainSink::make_presenter`]), whose images are the layer's drawables. So the window's
//! render thread takes no drawable for it, and its swapchain sets the layer up (its device among
//! the rest): the sink names no device, draws into no drawable it is given, and lets the layer's
//! drawables take copies, as its swapchain's images allow (their usage beside a render target's
//! is the transfers' and sampling's, `reims_vgpu_vulkan::swapchain`).

#[cfg(test)]
mod tests;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_metal::MTLDevice;
use objc2_quartz_core::CAMetalDrawable;
use reims_vgpu_window::{DrawnCursor, FrameSink, PixelRect, Presenter, VulkanTarget};

use super::swapchain::SwapchainSink;

impl FrameSink for SwapchainSink {
    /// None: the swapchain sets the layer's device.
    fn device(&self) -> Option<Retained<ProtocolObject<dyn MTLDevice>>> {
        None
    }

    /// Nothing: the sink presents itself, and is given no drawable.
    fn draw(
        &self,
        _drawable: &ProtocolObject<dyn CAMetalDrawable>,
        _region: PixelRect,
        _cursor: Option<&DrawnCursor>,
    ) -> bool {
        false
    }

    /// False: the swapchain's images take transfers and sampling too, as the swapchain sets
    /// the layer.
    fn render_target_only(&self) -> bool {
        false
    }

    /// True: the sink presents through [`FrameSink::presenter`].
    fn presents(&self) -> bool {
        true
    }

    /// The presenter ([`SwapchainSink::make_presenter`]).
    fn presenter(&self, target: &dyn VulkanTarget) -> Option<Box<dyn Presenter>> {
        self.make_presenter(target)
    }

    /// The display's ([`SwapchainSink::cursor_position`]).
    fn guest_cursor_position(&self) -> Option<(u16, u16)> {
        self.cursor_position()
    }
}
