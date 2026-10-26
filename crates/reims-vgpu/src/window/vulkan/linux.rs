//! What Linux's window asks of the Vulkan platform's sink ([`FrameSink`]): a presenter over the
//! window's swapchain ([`SwapchainSink::make_presenter`]), which SDL's window makes its surface
//! for.

use reims_vgpu_window::{FrameSink, Presenter, VulkanTarget};

use super::swapchain::SwapchainSink;

impl FrameSink for SwapchainSink {
    /// The presenter ([`SwapchainSink::make_presenter`]).
    fn presenter(&self, target: &dyn VulkanTarget) -> Option<Box<dyn Presenter>> {
        self.make_presenter(target)
    }

    /// The display's ([`SwapchainSink::cursor_position`]).
    fn guest_cursor_position(&self) -> Option<(u16, u16)> {
        self.cursor_position()
    }
}
