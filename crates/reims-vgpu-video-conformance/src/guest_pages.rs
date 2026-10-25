//! The stand-in guest's buffers in pages laid out as a guest's are, so that
//! a backend's device can write them as it writes a real guest's (core's
//! `device_memory`).
//!
//! A guest's pixel buffer lies in its RAM, one memfd on the host, with
//! nearly every 4 KiB page a run of its own, and reims-vgpu maps it as one
//! view of those pages, from which it makes a udmabuf in the view's order
//! (reims-vgpu's `os/linux/dma_buf.rs`). [`GuestPages`] is the same on
//! Linux: the pages of a memfd of its own, every other one in pairs
//! swapped, so that no two follow on in the file; one view of them in the
//! buffer's order; and a dma-buf of ranges of the view's pages made by
//! `UDMABUF_CREATE_LIST` on each ask, one piece a page, refused past
//! udmabuf's `list_limit` and `size_limit_mb` as the host's are. Elsewhere, and where a
//! memfd, a mapping or `/dev/udmabuf` is not to be had, the stand-in's
//! buffers are plain memory, which no device is lent.

#[cfg(target_os = "linux")]
pub(crate) use linux::GuestPages;

#[cfg(not(target_os = "linux"))]
pub(crate) use elsewhere::GuestPages;

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::{c_char, c_int, c_uint, c_ulong, c_void};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::ptr::NonNull;

    use reims_vgpu_video_core::device_memory::{DmaBufError, SurfaceDmaBuf};

    /// The size of a page.
    const PAGE: usize = 4096;

    unsafe extern "C" {
        fn memfd_create(name: *const c_char, flags: c_uint) -> c_int;
        fn ftruncate(fd: c_int, length: i64) -> c_int;
        fn fcntl(fd: c_int, command: c_int, ...) -> c_int;
        fn mmap(
            address: *mut c_void,
            length: usize,
            protection: c_int,
            flags: c_int,
            fd: c_int,
            offset: i64,
        ) -> *mut c_void;
        fn munmap(address: *mut c_void, length: usize) -> c_int;
        fn open(path: *const c_char, flags: c_int, ...) -> c_int;
        fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    }

    const MFD_CLOEXEC: c_uint = 1;
    const MFD_ALLOW_SEALING: c_uint = 2;
    const F_ADD_SEALS: c_int = 1033;
    const F_SEAL_SHRINK: c_int = 2;
    const PROT_NONE: c_int = 0;
    const PROT_READ_WRITE: c_int = 3;
    const MAP_SHARED: c_int = 1;
    const MAP_PRIVATE: c_int = 2;
    const MAP_FIXED: c_int = 0x10;
    const MAP_ANONYMOUS: c_int = 0x20;
    const O_RDWR: c_int = 2;
    const O_CLOEXEC: c_int = 0o2_000_000;
    /// `UDMABUF_FLAGS_CLOEXEC`.
    const UDMABUF_CLOEXEC: u32 = 1;
    /// `UDMABUF_CREATE_LIST`, `_IOW('u', 0x43, struct udmabuf_create_list)`.
    const UDMABUF_CREATE_LIST: c_ulong = (1 << 30) | (8 << 16) | ((b'u' as c_ulong) << 8) | 0x43;

    fn os_error(what: &str) -> String {
        format!("{what}: {}", std::io::Error::last_os_error())
    }

    /// A buffer's bytes in a memfd's pages, scattered as a guest's are, and
    /// one view of them in the buffer's order (module docs).
    pub(crate) struct GuestPages {
        memfd: OwnedFd,
        view: NonNull<u8>,
        /// The buffer's bytes.
        len: usize,
        /// The view's pages.
        pages: usize,
        /// Each of the view's pages' page in the memfd.
        file_pages: Vec<usize>,
    }

    // SAFETY: the view is this value's own mapping, reached only through
    // it; the memfd is a descriptor.
    unsafe impl Send for GuestPages {}
    // SAFETY: as above; `&self` reads the view's address alone.
    unsafe impl Sync for GuestPages {}

    impl GuestPages {
        /// `len` bytes, every one 0; `Err` says why there are none.
        pub(crate) fn new(len: usize) -> Result<Self, String> {
            let pages = len.div_ceil(PAGE).max(1);
            // SAFETY: a NUL-terminated name.
            let raw = unsafe {
                memfd_create(
                    c"reims-vgpu-video stand-in guest".as_ptr(),
                    MFD_CLOEXEC | MFD_ALLOW_SEALING,
                )
            };
            if raw < 0 {
                return Err(os_error("memfd_create"));
            }
            // SAFETY: a descriptor just made, owned from here.
            let memfd = unsafe { OwnedFd::from_raw_fd(raw) };
            let file_length = i64::try_from(2 * pages * PAGE).map_err(|e| e.to_string())?;
            // SAFETY: the memfd above; udmabuf takes a memfd sealed against
            // shrinking alone.
            if unsafe { ftruncate(memfd.as_raw_fd(), file_length) } != 0
                || unsafe { fcntl(memfd.as_raw_fd(), F_ADD_SEALS, F_SEAL_SHRINK) } != 0
            {
                return Err(os_error("sizing and sealing the memfd"));
            }
            // Every other page, in pairs swapped: no two of the view's pages
            // follow on in the file.
            let file_pages: Vec<usize> = (0..pages)
                .map(|page| {
                    let swapped = page ^ 1;
                    2 * if swapped < pages { swapped } else { page }
                })
                .collect();
            // SAFETY: a fresh reservation of the view's pages, mapped over
            // page by page below.
            let view = unsafe {
                mmap(
                    std::ptr::null_mut(),
                    pages * PAGE,
                    PROT_NONE,
                    MAP_PRIVATE | MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            if view as usize == usize::MAX {
                return Err(os_error("reserving the view"));
            }
            let view = view.cast::<u8>();
            let made = Self {
                memfd,
                view: NonNull::new(view).ok_or("a view at 0")?,
                len,
                pages,
                file_pages,
            };
            for page in 0..pages {
                let offset =
                    i64::try_from(made.file_pages[page] * PAGE).map_err(|e| e.to_string())?;
                // SAFETY: one page of the reservation, replaced by a shared
                // mapping of the memfd's page.
                let at = unsafe {
                    mmap(
                        view.add(page * PAGE).cast(),
                        PAGE,
                        PROT_READ_WRITE,
                        MAP_SHARED | MAP_FIXED,
                        made.memfd.as_raw_fd(),
                        offset,
                    )
                };
                if at as usize == usize::MAX {
                    return Err(os_error("mapping a page of the view"));
                }
            }
            Ok(made)
        }

        /// The buffer's bytes, in the view.
        pub(crate) fn bytes(&mut self) -> &mut [u8] {
            // SAFETY: the view maps `pages` pages, readable and writable,
            // while this lives; `&mut self` keeps every other slice apart.
            unsafe { std::slice::from_raw_parts_mut(self.view.as_ptr(), self.len) }
        }

        /// The buffer's bytes, in the view, to read.
        pub(crate) fn as_bytes(&self) -> &[u8] {
            // SAFETY: as for `bytes`; `&self` keeps any `&mut` of them away.
            unsafe { std::slice::from_raw_parts(self.view.as_ptr(), self.len) }
        }

        /// The view's first byte, and its bytes: whole pages.
        pub(crate) fn view(&self) -> (NonNull<u8>, usize) {
            (self.view, self.pages * PAGE)
        }

        /// A dma-buf of `ranges` of the buffer's bytes, each an offset and
        /// a length, in ascending order and apart: the view's pages each
        /// lies in, in its order, a page two share once, one piece a page,
        /// and where each range's first byte lies in it. Refused as the
        /// host refuses (reims-vgpu's `os/linux/device_lend.rs`): with
        /// udmabuf's limits where its pages are more than they take, which
        /// are one piece each here, else with the kernel's refusal.
        pub(crate) fn dma_buf_of(
            &self,
            ranges: &[(usize, usize)],
        ) -> Result<SurfaceDmaBuf, DmaBufError> {
            let refused = |why: &str| DmaBufError::Refused(why.to_owned());
            // The view's pages each range lies in, merged where two share
            // or meet one, and each range's offset in them end to end.
            let mut runs: Vec<(usize, usize)> = Vec::new();
            let mut offsets = Vec::with_capacity(ranges.len());
            let (mut before, mut after) = (0, 0);
            for &(at, bytes) in ranges {
                let end = at
                    .checked_add(bytes)
                    .ok_or_else(|| refused("a range past the buffer"))?;
                if bytes == 0 || at < after || end > self.pages * PAGE {
                    return Err(refused(
                        "ranges empty, out of order, overlapping or past the buffer",
                    ));
                }
                after = end;
                let (first, last) = (at / PAGE, end.div_ceil(PAGE));
                match runs.last_mut() {
                    Some(run) if first <= run.1 => run.1 = run.1.max(last),
                    _ => {
                        before += runs.last().map_or(0, |run| (run.1 - run.0) * PAGE);
                        runs.push((first, last));
                    }
                }
                let run = runs.last().map_or(first, |run| run.0);
                offsets.push((before + at - run * PAGE) as u64);
            }
            let pages: Vec<usize> = runs.iter().flat_map(|&(first, last)| first..last).collect();
            let limit = |name: &str, default: usize| {
                std::fs::read_to_string(format!("/sys/module/udmabuf/parameters/{name}"))
                    .ok()
                    .and_then(|s| s.trim().parse().ok())
                    .unwrap_or(default)
            };
            let (most_pieces, most_bytes) =
                (limit("list_limit", 1024), limit("size_limit_mb", 64) << 20);
            let size = pages.len() * PAGE;
            if pages.len() > most_pieces || size > most_bytes {
                return Err(DmaBufError::TooLarge {
                    pieces: pages.len() as u64,
                    bytes: size as u64,
                    most_pieces: most_pieces as u64,
                    most_bytes: most_bytes as u64,
                    why: format!(
                        "{} pieces of {size:#x} bytes do not fit udmabuf's list_limit \
                         {most_pieces} and size_limit_mb {}",
                        pages.len(),
                        most_bytes >> 20
                    ),
                });
            }
            // SAFETY: a NUL-terminated path.
            let device = unsafe { open(c"/dev/udmabuf".as_ptr(), O_RDWR | O_CLOEXEC) };
            if device < 0 {
                return Err(DmaBufError::Refused(os_error("/dev/udmabuf")));
            }
            // SAFETY: a descriptor just opened, owned from here.
            let device = unsafe { OwnedFd::from_raw_fd(device) };
            // `struct udmabuf_create_list` as little-endian words: `flags`
            // and `count`, then each item's `memfd` and padding, `offset`
            // and `size`.
            let count = u32::try_from(pages.len()).map_err(|e| refused(&e.to_string()))?;
            let memfd =
                u64::try_from(self.memfd.as_raw_fd()).map_err(|e| refused(&e.to_string()))?;
            let mut call = vec![u64::from(UDMABUF_CLOEXEC) | (u64::from(count) << 32)];
            for &page in &pages {
                call.extend([memfd, (self.file_pages[page] * PAGE) as u64, PAGE as u64]);
            }
            // SAFETY: the list names the memfd this value holds; the kernel
            // reads the header and its items.
            let made = unsafe { ioctl(device.as_raw_fd(), UDMABUF_CREATE_LIST, call.as_ptr()) };
            if made < 0 {
                return Err(DmaBufError::Refused(format!(
                    "{} (a dma-buf of {} pieces)",
                    os_error("UDMABUF_CREATE_LIST"),
                    pages.len(),
                )));
            }
            Ok(SurfaceDmaBuf {
                // SAFETY: the dma-buf just made, owned from here.
                fd: unsafe { OwnedFd::from_raw_fd(made) },
                size: size as u64,
                offsets,
            })
        }
    }

    impl Drop for GuestPages {
        fn drop(&mut self) {
            // SAFETY: the view this value mapped, unmapped once.
            unsafe { munmap(self.view.as_ptr().cast(), self.pages * PAGE) };
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod elsewhere {
    /// No such buffers here: the stand-in's are plain memory.
    pub(crate) enum GuestPages {}

    impl GuestPages {
        pub(crate) fn new(_len: usize) -> Result<Self, String> {
            Err("guest pages are Linux's".to_owned())
        }

        pub(crate) fn bytes(&mut self) -> &mut [u8] {
            match *self {}
        }

        pub(crate) fn as_bytes(&self) -> &[u8] {
            match *self {}
        }

        pub(crate) fn view(&self) -> (std::ptr::NonNull<u8>, usize) {
            match *self {}
        }
    }
}
