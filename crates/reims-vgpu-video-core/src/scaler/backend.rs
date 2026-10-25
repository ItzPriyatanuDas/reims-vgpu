//! What a scaler backend (VA-API's video processing, or the CPU stand-in
//! of the tests) gives the device: an accelerator, made at the guest's
//! DRIVER_OK as Apple's host makes its IOSurfaceAccelerator, that answers
//! the capabilities and runs transforms.
//!
//! The device reads and checks the request, holds the two surfaces and
//! gathers the side buffers; a backend only transforms.

use std::sync::Arc;

use crate::device_wait::GuestHold;
use crate::scaler::capabilities::Capabilities;
use crate::scaler::io_return::{self, IoReturn};
use crate::scaler::plan::Plan;
use crate::scaler::reference;
use crate::scaler::request::Transform;
use crate::surfaces::{GuestSurface, SurfaceError};

/// A transform over bytes: the plan, the source's bytes, the destination's.
pub type RunOnBytes<'r> = dyn FnMut(&Plan, &[u8], &mut [u8]) -> IoReturn + 'r;

/// Makes accelerators.
pub trait ScalerBackend: Send + Sync {
    /// A new accelerator, at the guest's DRIVER_OK. A failure is the
    /// device's needs-reset, as Apple's host's is
    /// ("IOSurfaceAcceleratorCreate failed").
    fn create_accelerator(&self) -> Result<Box<dyn Accelerator>, String>;
}

/// One accelerator: the device's from DRIVER_OK to its reset.
pub trait Accelerator: Send {
    /// What it can do, for a type 1 request.
    fn capabilities(&self) -> Capabilities;

    /// Runs one transform. Its status is the request's: 0, or an IOReturn
    /// the guest's client gets back.
    fn transform(&mut self, job: &mut TransformJob<'_>) -> IoReturn;
}

/// One transform as a backend gets it.
pub struct TransformJob<'a> {
    /// The request's transform, as the guest sent it.
    pub transform: &'a Transform,
    /// The transform resolved against the two surfaces.
    pub plan: Plan,
    /// The source surface, held while the job runs.
    pub source: &'a dyn GuestSurface,
    /// The destination surface, held while the job runs.
    pub destination: &'a dyn GuestSurface,
    /// The device's hold on both surfaces, for a backend whose device may
    /// still write the destination past its deadline to keep until it is
    /// done ([`crate::device_wait`]): while it lives the guest sees both
    /// surfaces in use. A job a test makes over surfaces no device holds
    /// holds nothing.
    pub hold: GuestHold,
    /// The inbound side buffers, gathered from the guest, by pair: kind and
    /// bytes. None unless the guest sent them.
    pub inputs: [Option<(i32, Vec<u8>)>; 4],
    /// The outbound side buffers, zeroed, which the device scatters back to
    /// the guest after the transform: kind and bytes.
    pub outputs: [Option<(i32, Vec<u8>)>; 4],
}

impl TransformJob<'_> {
    /// Reads the source, then lends the destination, as a backend that
    /// copies in and out does: `run(plan, source bytes, destination bytes)`.
    /// A surface that cannot be lent is kIOReturnNotReadable or
    /// kIOReturnNotWritable; the layout's own checks are the plan's.
    pub fn with_bytes(&mut self, run: &mut RunOnBytes<'_>) -> IoReturn {
        let source = match self.read_source() {
            Ok(source) => source,
            Err(status) => return status,
        };
        let plan = &self.plan;
        let mut status = io_return::SUCCESS;
        let written = self
            .destination
            .write(&mut |bytes| status = run(plan, &source, bytes));
        written_status(written, status)
    }

    /// Runs a transform whose device works outside the lends, as no device
    /// call runs inside one (docs/video/core.md, "The copy into the
    /// guest's buffer"): reads the source, then `prepare(plan, source
    /// bytes)` with no surface lent, which runs the device and leaves its
    /// output where the CPU reads it, then lends the destination for
    /// `copy(plan, output, destination bytes)`, the CPU's alone. The output
    /// is dropped after the lend, so what it holds mapped or locked is let
    /// go outside it.
    ///
    /// The statuses come in [`TransformJob::with_bytes`]'s order: a source
    /// that cannot be read, a destination that cannot be lent, a surface
    /// too small for its format in the plan (kIOReturnBadArgument), then
    /// `prepare`'s failure, then `copy`'s answer. `prepare` runs only for a
    /// source that holds its format, and before the destination's lend says
    /// whether the destination can be written.
    pub fn with_bytes_off_the_lend<P>(
        &mut self,
        prepare: impl FnOnce(&Plan, &[u8]) -> Result<P, IoReturn>,
        mut copy: impl FnMut(&Plan, &P, &mut [u8]) -> IoReturn,
    ) -> IoReturn {
        let source = match self.read_source() {
            Ok(source) => source,
            Err(status) => return status,
        };
        let plan = &self.plan;
        let source_fits = plan.source_format.fits(source.len());
        let output = if source_fits {
            prepare(plan, &source)
        } else {
            Err(io_return::BAD_ARGUMENT)
        };
        let mut status = io_return::SUCCESS;
        let written = self.destination.write(&mut |bytes| {
            status = if !source_fits || !plan.destination_format.fits(bytes.len()) {
                io_return::BAD_ARGUMENT
            } else {
                match &output {
                    Ok(output) => copy(plan, output, bytes),
                    Err(status) => *status,
                }
            };
        });
        drop(output);
        written_status(written, status)
    }

    /// The source's bytes, read in its lend; kIOReturnNotReadable where it
    /// cannot be lent.
    fn read_source(&self) -> Result<Vec<u8>, IoReturn> {
        let mut source = Vec::new();
        let read = self
            .source
            .read(&mut |bytes| source.extend_from_slice(bytes));
        match read {
            Ok(()) => Ok(source),
            Err(error) => {
                log::warn!("scaler: the source surface cannot be read: {error}");
                Err(io_return::NOT_READABLE)
            }
        }
    }
}

/// A transform's status after its destination's write lend: `status`, the
/// one the lend's closure gave, where the destination was lent.
fn written_status(written: Result<(), SurfaceError>, status: IoReturn) -> IoReturn {
    match written {
        Ok(()) => status,
        Err(SurfaceError::Layout) => io_return::BAD_ARGUMENT,
        Err(error) => {
            log::warn!("scaler: the destination surface cannot be written: {error}");
            io_return::NOT_WRITABLE
        }
    }
}

/// The CPU stand-in: [`reference::run`] for every transform, with
/// capabilities the caller gives.
#[derive(Clone, Debug)]
pub struct ReferenceBackend {
    /// What its accelerators answer for type 1.
    pub capabilities: Capabilities,
}

impl ScalerBackend for ReferenceBackend {
    fn create_accelerator(&self) -> Result<Box<dyn Accelerator>, String> {
        Ok(Box::new(ReferenceAccelerator {
            capabilities: self.capabilities.clone(),
        }))
    }
}

/// [`ReferenceBackend`]'s accelerator.
#[derive(Debug)]
pub struct ReferenceAccelerator {
    capabilities: Capabilities,
}

impl Accelerator for ReferenceAccelerator {
    fn capabilities(&self) -> Capabilities {
        self.capabilities.clone()
    }

    fn transform(&mut self, job: &mut TransformJob<'_>) -> IoReturn {
        job.with_bytes(&mut |plan, source, destination| match reference::run(
            plan,
            source,
            destination,
        ) {
            Ok(()) => io_return::SUCCESS,
            Err(status) => status,
        })
    }
}

impl<T: ScalerBackend + ?Sized> ScalerBackend for Arc<T> {
    fn create_accelerator(&self) -> Result<Box<dyn Accelerator>, String> {
        (**self).create_accelerator()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::fourcc::FourCc;
    use crate::surfaces::SurfaceLayout;
    use crate::sync::Mutex;

    /// A BGRA surface in host memory that counts the lends in progress,
    /// shared with the other surfaces of a test. `bytes` may be shorter
    /// than its layout says, and it may refuse either lend.
    struct Lending {
        layout: SurfaceLayout,
        bytes: Mutex<Vec<u8>>,
        lends: Arc<AtomicUsize>,
        readable: bool,
        writable: bool,
    }

    impl Lending {
        fn new(lends: &Arc<AtomicUsize>, length: usize) -> Lending {
            Lending {
                layout: SurfaceLayout {
                    pixel_format: FourCc::new(*b"BGRA"),
                    width: 4,
                    height: 4,
                    planes: Vec::new(),
                    bytes_per_row: 16,
                },
                bytes: Mutex::new(vec![0; length]),
                lends: Arc::clone(lends),
                readable: true,
                writable: true,
            }
        }
    }

    impl GuestSurface for Lending {
        fn layout(&self) -> SurfaceLayout {
            self.layout.clone()
        }

        fn write(&self, write: &mut dyn FnMut(&mut [u8])) -> Result<(), SurfaceError> {
            if !self.writable {
                return Err(SurfaceError::Unreachable("read-only".into()));
            }
            self.lends.fetch_add(1, Ordering::SeqCst);
            write(&mut self.bytes.lock());
            self.lends.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        }

        fn read(&self, read: &mut dyn FnMut(&[u8])) -> Result<(), SurfaceError> {
            if !self.readable {
                return Err(SurfaceError::Unreachable("unmapped".into()));
            }
            self.lends.fetch_add(1, Ordering::SeqCst);
            read(&self.bytes.lock());
            self.lends.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// An output that records the lends in progress as it drops.
    struct Output {
        lends: Arc<AtomicUsize>,
        dropped_in: Arc<Mutex<Vec<usize>>>,
    }

    impl Drop for Output {
        fn drop(&mut self) {
            self.dropped_in
                .lock()
                .push(self.lends.load(Ordering::SeqCst));
        }
    }

    /// What one transform off the lends saw: its status, the lends in
    /// progress as `prepare` ran, as `copy` ran, and as the output
    /// dropped.
    #[derive(Debug, PartialEq)]
    struct Seen {
        status: IoReturn,
        prepared_in: Vec<usize>,
        copied_in: Vec<usize>,
        dropped_in: Vec<usize>,
    }

    fn off_the_lend(
        source: &Lending,
        destination: &Lending,
        prepared: Result<(), IoReturn>,
    ) -> Seen {
        let lends = Arc::clone(&source.lends);
        let transform = Transform::default();
        let plan = Plan::new(&transform, &source.layout, &destination.layout)
            .expect("a transform between two like surfaces plans");
        let mut job = TransformJob {
            transform: &transform,
            plan,
            source,
            destination,
            hold: GuestHold::nothing(),
            inputs: Default::default(),
            outputs: Default::default(),
        };
        let dropped_in = Arc::new(Mutex::new(Vec::new()));
        let (mut prepared_in, mut copied_in) = (Vec::new(), Vec::new());
        let status = job.with_bytes_off_the_lend(
            |_, _| {
                prepared_in.push(lends.load(Ordering::SeqCst));
                prepared.map(|()| Output {
                    lends: Arc::clone(&lends),
                    dropped_in: Arc::clone(&dropped_in),
                })
            },
            |_, _, bytes| {
                copied_in.push(lends.load(Ordering::SeqCst));
                bytes.fill(1);
                io_return::SUCCESS
            },
        );
        let dropped_in = dropped_in.lock().clone();
        Seen {
            status,
            prepared_in,
            copied_in,
            dropped_in,
        }
    }

    #[test]
    fn a_transform_off_the_lend_runs_its_device_with_no_surface_lent() {
        let lends = Arc::new(AtomicUsize::new(0));
        let (source, destination) = (Lending::new(&lends, 64), Lending::new(&lends, 64));
        let seen = off_the_lend(&source, &destination, Ok(()));
        assert_eq!(
            seen,
            Seen {
                status: io_return::SUCCESS,
                prepared_in: vec![0],
                copied_in: vec![1],
                dropped_in: vec![0],
            }
        );
        assert_eq!(*destination.bytes.lock(), vec![1; 64]);
    }

    #[test]
    fn a_transform_off_the_lend_answers_in_the_order_with_bytes_does() {
        let lends = Arc::new(AtomicUsize::new(0));
        let failed = Err(io_return::UNSUPPORTED);
        let status = |source: &Lending, destination: &Lending, prepared| {
            let seen = off_the_lend(source, destination, prepared);
            (seen.status, seen.prepared_in.len(), seen.copied_in.len())
        };
        let whole = || Lending::new(&lends, 64);
        let unreadable = Lending {
            readable: false,
            ..whole()
        };
        let unwritable = Lending {
            writable: false,
            ..whole()
        };
        let short = Lending::new(&lends, 63);
        // A source that cannot be read, before anything runs.
        assert_eq!(
            status(&unreadable, &unwritable, failed),
            (io_return::NOT_READABLE, 0, 0)
        );
        // A destination that cannot be lent, over the device's failure.
        assert_eq!(
            status(&whole(), &unwritable, failed),
            (io_return::NOT_WRITABLE, 1, 0)
        );
        // A source short of its format: the device does not run.
        assert_eq!(
            status(&short, &whole(), Ok(())),
            (io_return::BAD_ARGUMENT, 0, 0)
        );
        // A destination short of its format, over the device's failure.
        assert_eq!(
            status(&whole(), &short, failed),
            (io_return::BAD_ARGUMENT, 1, 0)
        );
        // The device's failure, with nothing copied.
        assert_eq!(
            status(&whole(), &whole(), failed),
            (io_return::UNSUPPORTED, 1, 0)
        );
    }
}
