//! CVPixelBuffers as the backend reads and fills them: IOSurface-backed
//! buffers of a format and size, and their planes, locked for the length
//! of a closure.

use std::marker::PhantomData;

use reims_vgpu_video_core::backend::{BackendError, PixelFormat};
use reims_vgpu_video_core::surfaces::PlaneSource;

use super::cf::{self, Cf, Owned, constant};
use super::ffi::core_foundation::OSType;
use super::ffi::core_video::{
    CVPixelBufferCreate, CVPixelBufferGetBaseAddress, CVPixelBufferGetBaseAddressOfPlane,
    CVPixelBufferGetBytesPerRow, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeight,
    CVPixelBufferGetHeightOfPlane, CVPixelBufferGetPixelFormatType, CVPixelBufferGetPlaneCount,
    CVPixelBufferGetWidth, CVPixelBufferIsPlanar, CVPixelBufferLockBaseAddress, CVPixelBufferRef,
    CVPixelBufferUnlockBaseAddress, kCVPixelBufferLock_ReadOnly, kCVReturnSuccess,
};

/// One plane of a locked buffer: its rows, `bytes_per_row` apart.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PlaneGeometry {
    /// The bytes from one row to the next.
    pub(crate) bytes_per_row: usize,
    /// The plane's rows.
    pub(crate) height: usize,
}

/// A CVPixelBuffer the backend holds.
pub(crate) struct PixelBuffer(Owned);

impl PixelBuffer {
    /// Holds `buffer`, which the caller lends: a reference of its own is
    /// taken.
    ///
    /// # Safety
    ///
    /// `buffer` is NULL or a live CVPixelBuffer.
    pub(crate) unsafe fn retain(buffer: CVPixelBufferRef) -> Option<Self> {
        // SAFETY: NULL or live for this call, as the caller says; the
        // reference taken keeps it past the call.
        unsafe { Cf::borrow(buffer.cast_const()) }.map(|buffer| Self(Owned::retain(buffer)))
    }

    /// Takes `buffer`, whose reference the caller owns and hands over.
    ///
    /// # Safety
    ///
    /// `buffer` is NULL or a CVPixelBuffer the caller owns a reference to.
    pub(crate) unsafe fn take(buffer: CVPixelBufferRef) -> Option<Self> {
        // SAFETY: as the caller says.
        unsafe { Owned::take(buffer.cast_const()) }.map(Self)
    }

    /// A new IOSurface-backed buffer of `format` and size, as VideoToolbox
    /// makes its own.
    pub(crate) fn new(format: OSType, width: usize, height: usize) -> Result<Self, BackendError> {
        let attributes = io_surface_attributes(format);
        let mut buffer: CVPixelBufferRef = std::ptr::null_mut();
        // SAFETY: the attributes are a live dictionary and the out pointer
        // a live local.
        let status = unsafe {
            CVPixelBufferCreate(
                std::ptr::null(),
                width,
                height,
                format,
                attributes.as_ptr(),
                &raw mut buffer,
            )
        };
        // SAFETY: a Create call's result, or NULL.
        let buffer = unsafe { Self::take(buffer) };
        if status != kCVReturnSuccess {
            return Err(BackendError::Unsupported(format!(
                "CVPixelBufferCreate of {} {width}x{height} answered {status}",
                fourcc_text(format)
            )));
        }
        buffer.ok_or_else(|| BackendError::Malfunction("CVPixelBufferCreate gave none".into()))
    }

    /// The buffer, for the calls that take it.
    pub(crate) fn as_ptr(&self) -> CVPixelBufferRef {
        self.0.as_mut_ptr()
    }

    /// Its CoreVideo pixel format.
    pub(crate) fn format(&self) -> OSType {
        // SAFETY: a live CVPixelBuffer.
        unsafe { CVPixelBufferGetPixelFormatType(self.as_ptr()) }
    }

    /// Its width and height in pixels.
    pub(crate) fn size(&self) -> (usize, usize) {
        // SAFETY: a live CVPixelBuffer.
        unsafe {
            (
                CVPixelBufferGetWidth(self.as_ptr()),
                CVPixelBufferGetHeight(self.as_ptr()),
            )
        }
    }

    /// Each plane's geometry: one for a buffer without planes.
    pub(crate) fn planes(&self) -> Vec<PlaneGeometry> {
        let buffer = self.as_ptr();
        // SAFETY: a live CVPixelBuffer, asked of its own planes.
        unsafe {
            if CVPixelBufferIsPlanar(buffer) == 0 {
                return vec![PlaneGeometry {
                    bytes_per_row: CVPixelBufferGetBytesPerRow(buffer),
                    height: self.size().1,
                }];
            }
            (0..CVPixelBufferGetPlaneCount(buffer))
                .map(|plane| PlaneGeometry {
                    bytes_per_row: CVPixelBufferGetBytesPerRowOfPlane(buffer, plane),
                    height: CVPixelBufferGetHeightOfPlane(buffer, plane),
                })
                .collect()
        }
    }

    /// Locks the buffer for reading and lends `read` each plane's bytes,
    /// `bytes_per_row` times its rows, with its geometry.
    pub(crate) fn read<T>(
        &self,
        read: impl FnOnce(&[(&[u8], PlaneGeometry)]) -> T,
    ) -> Result<T, BackendError> {
        let locked = self.lock_for_reading()?;
        Ok(read(&locked.planes()))
    }

    /// Locks the buffer for reading until the result drops, which lends
    /// each plane's bytes as [`PixelBuffer::read`] does. The result
    /// borrows the buffer, so that nothing writes it through this
    /// [`PixelBuffer`] while it is read.
    pub(crate) fn lock_for_reading(&self) -> Result<LockedForReading<'_>, BackendError> {
        let lock = Lock::new(self, true)?;
        let planes = self.locked_planes()?;
        Ok(LockedForReading {
            _lock: lock,
            planes,
        })
    }

    /// Locks the buffer for writing and lends `write` each plane's bytes,
    /// as [`PixelBuffer::read`] lends them for reading. It takes the buffer
    /// mutably, so that no other read or write of it through this
    /// [`PixelBuffer`] overlaps the slices it lends.
    pub(crate) fn write<T>(
        &mut self,
        write: impl FnOnce(&mut [(&mut [u8], PlaneGeometry)]) -> T,
    ) -> Result<T, BackendError> {
        let _lock = Lock::new(self, false)?;
        let mut slices = Vec::new();
        for (base, length, geometry) in self.locked_planes()? {
            // SAFETY: the buffer is locked for writing until `_lock` drops,
            // after the slices do; a locked plane's memory is its
            // `length` bytes from its base, its own, so the slices do not
            // overlap each other; and `&mut self` keeps every other slice
            // of the buffer's memory this crate makes away while they live.
            let bytes = unsafe { std::slice::from_raw_parts_mut(base, length) };
            slices.push((bytes, geometry));
        }
        Ok(write(&mut slices))
    }

    /// Copies a 4:2:0 picture `width` samples wide, in two planes of
    /// `format`, into the buffer's two planes: each of the rows both planes
    /// have, as many of its bytes as both rows hold. The planes are a
    /// guest's, which core checked hold the picture; a row or plane they do
    /// not hold ends the copy of that plane, which no guest value can make
    /// reach past either plane.
    pub(crate) fn fill_420(
        &mut self,
        planes: &[PlaneSource<'_>],
        format: PixelFormat,
        width: u32,
    ) -> Result<(), BackendError> {
        let sample = format.bytes_per_sample();
        let width = usize::try_from(width).unwrap_or(usize::MAX);
        // The luma's samples, then the chroma's CbCr pairs, one for every
        // two luma samples, rounded up.
        let row_bytes = [
            width.saturating_mul(sample),
            width.div_ceil(2).saturating_mul(sample.saturating_mul(2)),
        ];
        self.write(|destination| {
            if destination.len() < 2 || planes.len() < 2 {
                return Err(BackendError::BadData(format!(
                    "a 4:2:0 picture of {} planes into {}",
                    planes.len(),
                    destination.len()
                )));
            }
            for (((to, geometry), from), row_bytes) in
                destination.iter_mut().zip(planes).zip(row_bytes)
            {
                let rows = geometry
                    .height
                    .min(usize::try_from(from.rows).unwrap_or(usize::MAX));
                let length = row_bytes
                    .min(geometry.bytes_per_row)
                    .min(from.bytes_per_row);
                for row in 0..rows {
                    let source =
                        (row.checked_mul(from.bytes_per_row)).and_then(|at| from.bytes.get(at..));
                    let target =
                        (row.checked_mul(geometry.bytes_per_row)).and_then(|at| to.get_mut(at..));
                    let (Some(source), Some(target)) = (source, target) else {
                        break;
                    };
                    let length = length.min(source.len()).min(target.len());
                    if let (Some(source), Some(target)) =
                        (source.get(..length), target.get_mut(..length))
                    {
                        target.copy_from_slice(source);
                    }
                }
            }
            Ok(())
        })?
    }

    /// Each plane of the locked buffer: its base address, its length (its
    /// bytes per row times its rows) and its geometry.
    fn locked_planes(&self) -> Result<Vec<(*mut u8, usize, PlaneGeometry)>, BackendError> {
        let buffer = self.as_ptr();
        // SAFETY: a live CVPixelBuffer, asked of its own layout.
        let planar = unsafe { CVPixelBufferIsPlanar(buffer) } != 0;
        let mut planes = Vec::new();
        for (index, geometry) in self.planes().into_iter().enumerate() {
            // SAFETY: a live, locked CVPixelBuffer, asked of a plane under
            // its count, or of its one plane.
            let base: *mut u8 = unsafe {
                if planar {
                    CVPixelBufferGetBaseAddressOfPlane(buffer, index).cast()
                } else {
                    CVPixelBufferGetBaseAddress(buffer).cast()
                }
            };
            let length = geometry.bytes_per_row.checked_mul(geometry.height);
            match length {
                Some(length) if !base.is_null() => planes.push((base, length, geometry)),
                _ => {
                    return Err(BackendError::Malfunction(format!(
                        "plane {index} has no memory for {geometry:?}"
                    )));
                }
            }
        }
        Ok(planes)
    }
}

/// A buffer's lock, undone on drop, for as long as the borrow of the
/// buffer it was made of.
struct Lock<'a> {
    buffer: CVPixelBufferRef,
    flags: u64,
    _buffer: PhantomData<&'a PixelBuffer>,
}

impl<'a> Lock<'a> {
    fn new(buffer: &'a PixelBuffer, read_only: bool) -> Result<Self, BackendError> {
        let flags = if read_only {
            kCVPixelBufferLock_ReadOnly
        } else {
            0
        };
        // SAFETY: a live CVPixelBuffer.
        let status = unsafe { CVPixelBufferLockBaseAddress(buffer.as_ptr(), flags) };
        if status != kCVReturnSuccess {
            return Err(BackendError::Malfunction(format!(
                "CVPixelBufferLockBaseAddress answered {status}"
            )));
        }
        Ok(Self {
            buffer: buffer.as_ptr(),
            flags,
            _buffer: PhantomData,
        })
    }
}

impl Drop for Lock<'_> {
    fn drop(&mut self) {
        // SAFETY: the buffer, live for the borrow, was locked with these
        // flags by this lock, and is unlocked once.
        unsafe { CVPixelBufferUnlockBaseAddress(self.buffer, self.flags) };
    }
}

/// A buffer locked for reading ([`PixelBuffer::lock_for_reading`]): each
/// plane's base address, length and geometry, until it drops.
pub(crate) struct LockedForReading<'a> {
    _lock: Lock<'a>,
    planes: Vec<(*mut u8, usize, PlaneGeometry)>,
}

impl LockedForReading<'_> {
    /// Each plane's bytes, `bytes_per_row` times its rows, with its
    /// geometry.
    pub(crate) fn planes(&self) -> Vec<(&[u8], PlaneGeometry)> {
        (self.planes.iter())
            .map(|&(base, length, geometry)| {
                // SAFETY: the buffer is locked until this drops, and a
                // locked plane's memory is its `length` bytes from its base;
                // the borrow of the buffer this holds keeps every write of
                // it through its PixelBuffer away while the slices live.
                let bytes = unsafe { std::slice::from_raw_parts(base.cast_const(), length) };
                (bytes, geometry)
            })
            .collect()
    }
}

/// `{PixelFormatType: format, IOSurfaceProperties: {}}`: the attributes of
/// an IOSurface-backed buffer of `format`.
pub(crate) fn io_surface_attributes(format: OSType) -> Owned {
    let format = cf::number_i64(i64::from(format));
    let properties = cf::dictionary(&[]);
    cf::dictionary(&[
        (
            constant!(core_video::kCVPixelBufferPixelFormatTypeKey),
            format.cf(),
        ),
        (
            constant!(core_video::kCVPixelBufferIOSurfacePropertiesKey),
            properties.cf(),
        ),
    ])
}

/// A FourCC as text, for logs: `420v`, or its number when it does not
/// print.
pub(crate) fn fourcc_text(code: OSType) -> String {
    let bytes = code.to_be_bytes();
    if bytes
        .iter()
        .all(|byte| byte.is_ascii_graphic() || *byte == b' ')
    {
        String::from_utf8_lossy(&bytes).into_owned()
    } else {
        format!("{code:#x}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::macos::ffi::core_video::kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;

    #[test]
    fn a_buffer_writes_and_reads_back_plane_by_plane() -> Result<(), BackendError> {
        let mut buffer = PixelBuffer::new(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, 66, 34)?;
        let planes = buffer.planes();
        assert_eq!(planes.len(), 2);
        assert_eq!((planes[0].height, planes[1].height), (34, 17));
        assert!(planes[0].bytes_per_row >= 66 && planes[1].bytes_per_row >= 66);
        buffer.write(|planes| {
            for (index, (bytes, _)) in planes.iter_mut().enumerate() {
                bytes.fill(u8::try_from(index).unwrap_or(0) + 7);
            }
        })?;
        let firsts =
            buffer.read(|planes| planes.iter().map(|(bytes, _)| bytes[0]).collect::<Vec<_>>())?;
        assert_eq!(firsts, vec![7, 8]);
        assert_eq!(fourcc_text(buffer.format()), "420v");
        Ok(())
    }
}
