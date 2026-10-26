//! Zero copy on NVIDIA: a decoded frame copied by the GPU straight into
//! the guest's buffer, with no staging and no copy by the CPU
//! (core's `DecodedFrame::write_to_guest`).
//!
//! The host lends a view of the guest buffer's pages, contiguous in its
//! address space (reims-vgpu's view of a surface over the guest's RAM).
//! The view is registered with CUDA once per mapping
//! (`cuMemHostRegister`, portable to every context), which pins its pages
//! and makes them memory the GPU copies into by DMA, and kept by the
//! mapping's identity in core's [`DeviceImports`], process-wide, since a
//! range of host memory is registered once in a process whichever GPU
//! registered it. The first decoders made make the process's, on their
//! runtime, whose thread drops a registration once its mapping goes. It is
//! unregistered when the mapping goes, before the hold that keeps the view
//! mapped is dropped. Each frame is then the
//! picture's map, one `cuMemcpy2D` a plane from the mapped surface into
//! the guest's plane at its offset and pitch, and the unmap: the staged
//! read back's calls, into the guest's pages instead of the staging memory,
//! and without the CPU's copy after.
//!
//! The copies are waited for: when [`write()`] returns
//! [`DeviceWrite::Written`], the frame is in the guest's pages, while core
//! still holds the buffer. Copies the GPU has not done by their deadline
//! fail the frame, and keep the buffer's registration and core's hold on
//! it until it has (`crate::wait`), so that no late write lands in pages
//! the guest has been given back. They write each
//! plane's rows of the frame's samples alone, as the CPU's copy writes
//! them, within the rows core checked against the buffer's memory
//! ([`GuestTarget`]). A registration CUDA refuses, or a copy that fails,
//! is declined with the reason, and core copies that frame on the CPU.

use std::any::Any;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::{Arc, OnceLock};

use reims_vgpu_video_core::backend::{BackendError, DecodedFrame, DeviceWrite};
use reims_vgpu_video_core::device_memory::{DeviceImports, DeviceMemory, GuestTarget, HostView};
use reims_vgpu_video_core::runtime::Runtime;

use crate::cuda::Gpu;
use crate::cuda::ffi::CU_MEMHOSTREGISTER_PORTABLE;
use crate::decode::NvidiaFrame;
use crate::nvdec::decoder::{Crop, Destination};

/// The most registrations kept at once: a guest's decoders use a few pools
/// of 10 to 30 buffers each, and each registration pins its buffer's pages
/// (core's [`DeviceImports`] drops the one used longest ago past it).
const MOST_REGISTRATIONS: usize = 96;

/// The process's registrations of guest buffers, from the first decoders'
/// [`make_registrations`].
static REGISTRATIONS: OnceLock<DeviceImports<Registration>> = OnceLock::new();

/// Makes the process's registrations, dropped on a thread of `runtime`'s,
/// where no decoders have made them yet.
pub(crate) fn make_registrations(runtime: &Arc<dyn Runtime>) {
    REGISTRATIONS.get_or_init(|| DeviceImports::new(runtime, "nvidia", MOST_REGISTRATIONS));
}

/// A guest buffer's view registered with CUDA: unregistered as it drops,
/// with the GPU that registered it current, and the view's hold dropped
/// after.
pub(crate) struct Registration {
    gpu: Arc<Gpu>,
    /// The view, held mapped while this lives; dropped after the
    /// unregistration.
    view: HostView,
}

impl Registration {
    /// `memory`'s view registered on `gpu`; `Err` says why not, for the
    /// log.
    fn new(gpu: &Arc<Gpu>, memory: &dyn DeviceMemory) -> Result<Self, String> {
        let view = memory
            .host_view()
            .map_err(|error| format!("no view of the buffer: {error}"))?;
        let fits = (view.offset.checked_add(memory.length())).is_some_and(|end| end <= view.size);
        if !fits {
            return Err("the host's view is shorter than the buffer".to_owned());
        }
        let cuda = gpu.cuda();
        let registration = (cuda.registration.as_ref())
            .ok_or("this driver registers no host memory (no cuMemHostRegister_v2)")?;
        let _current = gpu.push().map_err(|error| error.to_string())?;
        // SAFETY: the view's whole pages, mapped readable and writable
        // while its hold lives, which this registration keeps until it has
        // unregistered them.
        let result = unsafe {
            (registration.cuMemHostRegister_v2)(
                view.first.as_ptr().cast::<c_void>(),
                view.size,
                CU_MEMHOSTREGISTER_PORTABLE,
            )
        };
        cuda.check("cuMemHostRegister", result)
            .map_err(|error| format!("CUDA refused to register the buffer: {error}"))?;
        log::debug!(
            "a guest buffer registered: {} bytes, its first {} into its first page",
            view.size,
            view.offset
        );
        Ok(Self {
            gpu: Arc::clone(gpu),
            view,
        })
    }

    /// The host address of the buffer's first byte, which the GPU writes
    /// by DMA.
    fn first(&self) -> NonNull<u8> {
        // SAFETY: the offset is inside the view (checked at registration).
        unsafe { self.view.first.add(self.view.offset) }
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        let cuda = self.gpu.cuda();
        let Some(registration) = cuda.registration.as_ref() else {
            return;
        };
        let Ok(_current) = self.gpu.push() else {
            log::warn!("a guest buffer stays registered: its GPU's context is not to be had");
            return;
        };
        // SAFETY: registered by `new`, unregistered once; the view is still
        // mapped, its hold dropped after this.
        let result = unsafe {
            (registration.cuMemHostUnregister)(self.view.first.as_ptr().cast::<c_void>())
        };
        if let Err(error) = cuda.check("cuMemHostUnregister", result) {
            log::warn!("{error}");
        }
    }
}

/// Writes `frame`, cropped, into `target` by the GPU's copies (module
/// docs): [`DeviceWrite::Declined`] where the buffer cannot be registered or
/// a copy fails; an `Err` where the frame did not decode.
pub(crate) fn write(
    frame: &NvidiaFrame,
    target: &GuestTarget<'_>,
) -> Result<DeviceWrite, BackendError> {
    let declined = |why: String| Ok(DeviceWrite::Declined(why));
    let format = frame.pixel_format();
    if target.pixel_format().bytes_per_sample() != format.bytes_per_sample() {
        return declined(format!(
            "a {} buffer for {} frames",
            target.pixel_format().fourcc(),
            format.fourcc()
        ));
    }
    let (width, height) = target.written_size(frame.width(), frame.height());
    if width == 0 || height == 0 {
        return declined("an empty frame".to_owned());
    }
    let memory = target.memory();
    let gpu = frame.picture.gpu();
    // Made with the decoders whose frame this is.
    let Some(registrations) = REGISTRATIONS.get() else {
        return declined("no decoders made the process's registrations".to_owned());
    };
    let registration = match registrations.get_or_make(
        &memory.identity(),
        0,
        || Registration::new(gpu, memory),
        || "the buffer's mapping went while it was held".to_owned(),
    ) {
        Ok(registration) => registration,
        Err(why) => return declined(why),
    };
    let length = memory.length();
    let mut destinations = Vec::with_capacity(2);
    for plane in target.planes() {
        // The plane's own rows, which core checked lie inside the buffer.
        let Some(len) = (plane.end_of(plane.rows, plane.row_bytes))
            .filter(|&end| end <= length)
            .and_then(|end| end.checked_sub(plane.offset))
        else {
            return declined(format!(
                "a plane at {} past the buffer's {length} bytes",
                plane.offset
            ));
        };
        destinations.push(Destination {
            // SAFETY: the offset is inside the buffer (just checked), which
            // lies inside the registered view from `first` on.
            at: unsafe { registration.first().as_ptr().add(plane.offset) },
            len,
            bytes_per_row: plane.bytes_per_row,
            rows: plane.rows,
        });
    }
    let [luma, chroma] = destinations[..] else {
        return declined("a buffer of other than two planes".to_owned());
    };
    let crop = Crop {
        width,
        height,
        ..frame.crop
    };
    // Copies not done by their deadline may still write the buffer: its
    // registration, which keeps the view mapped and the pages pinned, and
    // the session's hold on it, which keeps them the buffer's, are kept
    // with them until the GPU is done (`crate::wait`).
    let written: Vec<Box<dyn Any + Send>> =
        vec![Box::new(Arc::clone(&registration)), Box::new(target.hold())];
    // SAFETY: each destination is the buffer's plane, its rows alone, in
    // the view the registration keeps mapped and writable; core holds the
    // buffer, and writes it no other way while the device writes it, and
    // `written` keeps both for as long as the copies may still write it.
    match unsafe {
        frame
            .picture
            .write_into(crop, luma, chroma, written, &frame.deadline)
    } {
        Ok(Ok(())) => Ok(DeviceWrite::Written),
        Ok(Err(error)) => declined(format!("the GPU's copy into the buffer failed: {error}")),
        Err(error) => Err(crate::decode::backend_error(frame.noted(error))),
    }
}
