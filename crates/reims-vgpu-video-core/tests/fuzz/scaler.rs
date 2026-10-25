//! The M2 scaler device's requests under seeded fuzzing: capabilities and
//! transform requests as the guest kext lays them out, with rectangles,
//! flags, side buffers and scatter-gather lists of any kind, against
//! surfaces of every layout and guest RAM in two regions, one at the top
//! of the address space; and requests of any type and size, now and then
//! mutated. The device runs on core's CPU reference scaler, which reaches
//! no GPU, before and after its DRIVER_OK and reset.
//!
//! Each request is checked for:
//! - no panic, within the time and memory bounds;
//! - a reply of the request's size, and a capabilities reply that wrote
//!   no more than the room and the capabilities' size;
//! - an end of the VM only for a scatter-gather list past eight entries,
//!   as Apple's host aborts;
//! - no surface held once the request is answered;
//! - after a transform that succeeded, nothing of the destination changed
//!   outside what `scaler::coverage` says the transform writes.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use reims_vgpu_video_core::fourcc::FourCc;
use reims_vgpu_video_core::scaler::backend::{
    Accelerator, ReferenceBackend, ScalerBackend, TransformJob,
};
use reims_vgpu_video_core::scaler::capabilities::{self, Capabilities, FormatClass};
use reims_vgpu_video_core::scaler::coverage::{Window, covered};
use reims_vgpu_video_core::scaler::format::{Packing, ScalerFormat};
use reims_vgpu_video_core::scaler::plan::Plan;
use reims_vgpu_video_core::scaler::request::{
    CAPABILITIES_ADDRESS, CAPABILITIES_HOST_POINTER, CAPABILITIES_LENGTH, CAPABILITIES_SIZE,
    DESTINATION_MAPPING_ID, LIST_ENTRIES, LIST_LENGTH, LISTS, SIDE_BUFFER_PAIR_LENGTH,
    SIDE_BUFFERS, SIDE_BUFFERS_USED, SOURCE_MAPPING_ID, STATUS, TRANSFORM, TRANSFORM_SIZE, TYPE,
    TYPE_CAPABILITIES, TYPE_TRANSFORM, Transform, put_u32, put_u64, u32_at, u64_at,
};
use reims_vgpu_video_core::scaler::{
    GuestMemory, MAX_INPUT_SIZE, MAX_OUTPUT_SIZE, Outcome, ScalerDevice, io_return,
};
use reims_vgpu_video_core::surfaces::{
    GuestSurface, MappingId, PlaneLayout, SurfaceError, SurfaceLayout, Surfaces,
};
use reims_vgpu_video_core::sync::Mutex;

use crate::support::{Rng, Target, long, maybe_mutate, quick};

const SCALER: Target = Target::new("scaler-requests");

/// Transforms that succeeded, which says how deep the inputs get.
static TRANSFORMED: AtomicU64 = AtomicU64::new(0);

#[test]
fn scaler_requests() {
    let report = quick(&SCALER, 3000, scaler_case);
    eprintln!(
        "scaler: {} transforms done in {} inputs",
        TRANSFORMED.load(Ordering::Relaxed),
        report.inputs
    );
}

#[test]
#[ignore = "long: RVGV_FUZZ_CASES, RVGV_FUZZ_SECONDS, RVGV_FUZZ_SEED"]
fn scaler_requests_long() {
    let report = long(&SCALER, scaler_case);
    eprintln!(
        "scaler: {} transforms done in {} inputs",
        TRANSFORMED.load(Ordering::Relaxed),
        report.inputs
    );
}

/// Guest RAM: a region low in the address space, and one that ends at its
/// top, where an address and a length overflow.
const RAM: [(u64, usize); 2] = [(0x1000_0000, 0x1_0000), (0xffff_ffff_ffff_0000, 0x1_0000)];

struct Ram {
    regions: Vec<(u64, Mutex<Vec<u8>>)>,
}

impl Ram {
    fn new() -> Self {
        Self {
            regions: RAM
                .iter()
                .map(|&(base, size)| (base, Mutex::new(vec![0x5a; size])))
                .collect(),
        }
    }

    /// The region and bytes of `[address, address + length)`, a length of
    /// 0 being the address alone.
    fn find(&self, address: u64, length: u64) -> Option<(usize, Range<usize>)> {
        self.regions
            .iter()
            .enumerate()
            .find_map(|(index, (base, memory))| {
                let start = address.checked_sub(*base)?;
                let end = start.checked_add(length.max(1))?;
                let size = memory.lock().len() as u64;
                (end <= size).then(|| (index, start as usize..(start + length) as usize))
            })
    }
}

impl GuestMemory for Ram {
    fn is_ram(&self, address: u64, length: u64) -> bool {
        self.find(address, length).is_some()
    }

    fn read(&self, address: u64, bytes: &mut [u8]) -> bool {
        let Some((index, range)) = self.find(address, bytes.len() as u64) else {
            return false;
        };
        bytes.copy_from_slice(&self.regions[index].1.lock()[range]);
        true
    }

    fn write(&self, address: u64, bytes: &[u8]) -> bool {
        let Some((index, range)) = self.find(address, bytes.len() as u64) else {
            return false;
        };
        self.regions[index].1.lock()[range].copy_from_slice(bytes);
        true
    }
}

/// An address the guest could name: in either region most often, near
/// their ends, or anywhere.
fn address(rng: &mut Rng) -> u64 {
    let (base, size) = *rng.pick(&RAM);
    match rng.below(8) {
        0 => rng.interesting_u64(),
        1 => base
            .wrapping_add(size as u64)
            .wrapping_sub(rng.range(0, 16)),
        _ => base + rng.range(0, size as u64 - 1),
    }
}

/// One surface the device can hold.
struct Surface {
    layout: SurfaceLayout,
    memory: Mutex<Vec<u8>>,
    reachable: bool,
}

/// The surfaces, by mapping ID 1 to [`SURFACES`], and their holds.
struct Pool {
    surfaces: HashMap<u64, Arc<Surface>>,
    holds: Arc<AtomicUsize>,
}

const SURFACES: u64 = 4;

struct Held {
    surface: Arc<Surface>,
    holds: Arc<AtomicUsize>,
}

impl Drop for Held {
    fn drop(&mut self) {
        self.holds.fetch_sub(1, Ordering::SeqCst);
    }
}

impl GuestSurface for Held {
    fn layout(&self) -> SurfaceLayout {
        self.surface.layout.clone()
    }

    fn write(&self, write: &mut dyn FnMut(&mut [u8])) -> Result<(), SurfaceError> {
        if !self.surface.reachable {
            return Err(SurfaceError::Unreachable("the stand-in's memory".into()));
        }
        write(&mut self.surface.memory.lock());
        Ok(())
    }

    fn read(&self, read: &mut dyn FnMut(&[u8])) -> Result<(), SurfaceError> {
        if !self.surface.reachable {
            return Err(SurfaceError::Unreachable("the stand-in's memory".into()));
        }
        read(&self.surface.memory.lock());
        Ok(())
    }
}

impl Surfaces for Pool {
    fn retain(&self, mapping_id: MappingId) -> Result<Box<dyn GuestSurface>, SurfaceError> {
        let surface = self
            .surfaces
            .get(&mapping_id.0)
            .ok_or(SurfaceError::Unknown(mapping_id.0))?;
        self.holds.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(Held {
            surface: Arc::clone(surface),
            holds: Arc::clone(&self.holds),
        }))
    }
}

/// A surface in a scaler format most often, laid out as CoreVideo lays it
/// out, now and then not.
fn surface(rng: &mut Rng) -> Surface {
    let format = if rng.one_in(16) {
        FourCc::new(*rng.pick(&[*b"422v", *b"L008", *b"v210"]))
    } else {
        rng.pick(&ScalerFormat::ALL).fourcc()
    };
    let side = |rng: &mut Rng| {
        if rng.one_in(12) {
            *rng.pick(&[0, 1, 2, 3, 1 << 14, u32::MAX])
        } else {
            rng.range_u32(1, 64)
        }
    };
    let (width, height) = (side(rng), side(rng));
    let (w, h) = (u64::from(width), u64::from(height));
    let shapes: Vec<(u64, u64)> = match ScalerFormat::from_fourcc(format).map(ScalerFormat::packing)
    {
        Some(Packing::Biplanar420) => {
            let sample = if [*b"x420", *b"xf20"].contains(&format.0.to_be_bytes()) {
                2
            } else {
                1
            };
            vec![(w * sample, h), (w.div_ceil(2) * 2 * sample, h.div_ceil(2))]
        }
        Some(Packing::Packed422(_)) => vec![(w.div_ceil(2) * 4, h)],
        Some(Packing::Rgb32(_)) => vec![(w * 4, h)],
        None => vec![(w, h)],
    };
    let align = *rng.pick(&[1, 16, 64]);
    let mut planes = Vec::new();
    let mut offset = 0u64;
    for (row, rows) in shapes {
        let mut bytes_per_row = row.div_ceil(align).saturating_mul(align);
        if rng.one_in(20) {
            bytes_per_row = bytes_per_row.saturating_sub(rng.range(1, 4));
        }
        if rng.one_in(30) && !planes.is_empty() {
            offset = offset.saturating_sub(rng.range(1, 32));
        }
        planes.push(PlaneLayout {
            offset: usize::try_from(offset).unwrap_or(usize::MAX),
            bytes_per_row: usize::try_from(bytes_per_row).unwrap_or(usize::MAX),
            width: width.max(1),
            height: u32::try_from(rows).unwrap_or(u32::MAX),
        });
        offset = offset.saturating_add(bytes_per_row.saturating_mul(rows));
    }
    let mut length = offset;
    if rng.one_in(16) {
        length = length.saturating_sub(rng.range(1, 64));
    }
    if length > 1 << 20 {
        length = rng.range(0, 4096);
    }
    let (planes, bytes_per_row) = match planes.as_slice() {
        [only] if rng.bool() => (Vec::new(), only.bytes_per_row),
        _ => (planes, 0),
    };
    let mut memory = vec![rng.below(256) as u8; length as usize];
    for (byte, value) in memory.iter_mut().zip(rng.some_bytes(256)) {
        *byte = value;
    }
    Surface {
        layout: SurfaceLayout {
            pixel_format: format,
            width,
            height,
            planes,
            bytes_per_row,
        },
        memory: Mutex::new(memory),
        reachable: !rng.one_in(24),
    }
}

/// A scaler that cannot make an accelerator.
struct NoAccelerator;

impl ScalerBackend for NoAccelerator {
    fn create_accelerator(&self) -> Result<Box<dyn Accelerator>, String> {
        Err("the stand-in has none".to_owned())
    }
}

/// The most a stand-in transform scales either way, as MaxHUpscale,
/// MaxHDownscale, MaxVUpscale and MaxVDownscale.
const STAND_IN_MOST_SCALE: [u32; 4] = [16; 4];

/// Core's CPU reference scaler, which refuses a transform past
/// [`STAND_IN_MOST_SCALE`] as a driver may. Past it the reference's
/// filter grows with the ratio: input 5292 of seed 0xf950be2f5ae7fa87, a
/// 21x16384 source turned onto 14x16384, scaled 16384 to 14 across, took
/// a debug build 70 s on this Mac. The scaler's kernel offers 8, 8, 8
/// and 4 (`MAX_SCALE` in reims-vgpu's `scaler/gpu.rs`), and the device
/// refuses past what a backend offers, so it is asked for no more; the
/// capabilities the fuzzer offers still reach every bound.
struct Reference(ReferenceBackend);

impl ScalerBackend for Reference {
    fn create_accelerator(&self) -> Result<Box<dyn Accelerator>, String> {
        Ok(Box::new(Bounded(self.0.create_accelerator()?)))
    }
}

/// [`Reference`]'s accelerator.
struct Bounded(Box<dyn Accelerator>);

impl Accelerator for Bounded {
    fn capabilities(&self) -> Capabilities {
        self.0.capabilities()
    }

    fn transform(&mut self, job: &mut TransformJob<'_>) -> io_return::IoReturn {
        if job.plan.ratio_past(STAND_IN_MOST_SCALE).is_some() {
            return io_return::UNSUPPORTED;
        }
        self.0.transform(job)
    }
}

const FORMAT_CLASSES: [FormatClass; 8] = [
    FormatClass::TwoPlanes420,
    FormatClass::TwoPlanes42010,
    FormatClass::Interleaved422,
    FormatClass::Argb8888,
    FormatClass::Rgba8888,
    FormatClass::TwoPlanes444,
    FormatClass::LumaOnly8,
    FormatClass::Rgb565,
];

/// Capabilities of any kind, bounds of 0 and none among them.
fn capabilities(rng: &mut Rng) -> Capabilities {
    let bound = |rng: &mut Rng| *rng.pick(&[0, 16, 64, 4096, u32::MAX]);
    let scale = |rng: &mut Rng| {
        if rng.one_in(4) {
            *rng.pick(&[1, 2, 4, 8])
        } else {
            *rng.pick(&[0, 16, 64, u32::MAX])
        }
    };
    Capabilities {
        scaler_version: rng.interesting_u32(),
        formats: FORMAT_CLASSES
            .iter()
            .copied()
            .filter(|_| rng.bool())
            .collect(),
        min_source: *rng.pick(&[0, 1, 4, 16]),
        max_source: (bound(rng), bound(rng)),
        min_destination: *rng.pick(&[0, 1, 4, 16]),
        max_destination: (bound(rng), bound(rng)),
        max_scale: [scale(rng), scale(rng), scale(rng), scale(rng)],
        xy_offsets_with_tiling: (rng.bool(), rng.bool()),
    }
}

/// A mapping ID: one of the surfaces most often.
fn mapping_id(rng: &mut Rng) -> u64 {
    match rng.below(16) {
        0 => 0,
        1 => rng.interesting_u64(),
        _ => rng.range(1, SURFACES),
    }
}

/// A type 1 request.
fn capabilities_request(rng: &mut Rng) -> Vec<u8> {
    let mut r = vec![0u8; CAPABILITIES_SIZE];
    put_u32(&mut r, TYPE, TYPE_CAPABILITIES);
    put_u64(&mut r, CAPABILITIES_ADDRESS, address(rng));
    put_u64(&mut r, CAPABILITIES_HOST_POINTER, rng.next_u64());
    let length = *rng.pick(&[0, 1, 0x100, capabilities::LENGTH as u64, 0x289, 0x1_0000]);
    let length = if rng.one_in(8) {
        rng.interesting_u64()
    } else {
        length
    };
    put_u64(&mut r, CAPABILITIES_LENGTH, length);
    r
}

/// A scatter-gather list of `size` bytes, most often split over the
/// entries in RAM, written as list `index`.
fn list(rng: &mut Rng, r: &mut [u8], index: usize, size: u32) {
    let at = LISTS + LIST_LENGTH * index;
    let count = if rng.one_in(10) {
        *rng.pick(&[0, 9, 16, u32::MAX])
    } else {
        rng.range_u32(1, LIST_ENTRIES as u32)
    };
    let mut left = size;
    for entry in 0..LIST_ENTRIES.min(count as usize) {
        let last = entry + 1 == count as usize;
        let length = if rng.one_in(10) {
            rng.interesting_u32()
        } else if last {
            left
        } else {
            rng.range_u32(0, left)
        };
        left = left.saturating_sub(length);
        // In RAM with room for its length, most often. The range is
        // inclusive, so an entry of no bytes starts a byte before the
        // region's end: the top region's end is 2^64.
        let at_address = if rng.one_in(6) {
            address(rng)
        } else {
            let (base, ram) = *rng.pick(&RAM);
            base + rng.range(0, (ram as u64).saturating_sub(u64::from(length.max(1))))
        };
        put_u64(r, at + 16 * entry, at_address);
        put_u32(r, at + 16 * entry + 8, length);
    }
    put_u32(r, at + 0x80, count);
}

/// A type 2 request between surfaces of `pool`, with windows within them
/// most often.
fn transform_request(rng: &mut Rng, pool: &Pool) -> Vec<u8> {
    let mut r = vec![0u8; TRANSFORM_SIZE];
    put_u32(&mut r, TYPE, TYPE_TRANSFORM);
    let t = |offset: usize| TRANSFORM + offset;
    put_u32(&mut r, t(0), rng.next_u32());
    put_u32(&mut r, t(4), rng.next_u32());
    let (source, destination) = (mapping_id(rng), mapping_id(rng));
    put_u64(&mut r, SOURCE_MAPPING_ID, source);
    put_u64(&mut r, DESTINATION_MAPPING_ID, destination);
    if rng.one_in(4) {
        for offset in [8, 0x10, 0x18] {
            put_u64(&mut r, t(offset), rng.next_u64());
        }
    }
    let flags = match rng.below(10) {
        0 => rng.interesting_u64(),
        1 => (1 << 13) | (1 << rng.below(64)),
        _ => {
            let mut flags = (1u64 << 13) | rng.range(0, 7);
            if rng.bool() {
                flags |= 1 << 12;
            }
            if rng.one_in(4) {
                flags |= 0x80;
            }
            flags
        }
    };
    put_u64(&mut r, t(0x20), flags);
    let size = |id: u64| {
        pool.surfaces.get(&id).map_or((64, 64), |s| {
            (s.layout.width.max(1), s.layout.height.max(1))
        })
    };
    let (sw, sh) = size(source);
    if !rng.one_in(3) {
        let fixed = |rng: &mut Rng, side: u32| {
            if rng.one_in(10) {
                return (rng.interesting_u64(), rng.interesting_u64());
            }
            let side = u64::from(side.min(1 << 16)) << 16;
            let at = rng.range(0, side.saturating_sub(1 << 16));
            let at = if rng.bool() { at & !0xffff } else { at };
            (at, rng.range(1, side - at))
        };
        let (x, width) = fixed(rng, sw);
        let (y, height) = fixed(rng, sh);
        for (offset, value) in [(0x28, x), (0x30, y), (0x38, width), (0x40, height)] {
            put_u64(&mut r, t(offset), value);
        }
    }
    put_u32(&mut r, t(0x48), sw);
    put_u32(&mut r, t(0x4c), sh);
    let (dw, dh) = size(destination);
    if !rng.one_in(3) {
        let whole = |rng: &mut Rng, side: u32| {
            if rng.one_in(10) {
                return (rng.interesting_u32(), rng.interesting_u32());
            }
            let at = rng.range_u32(0, side - 1);
            (at, rng.range_u32(1, side - at))
        };
        let (x, width) = whole(rng, dw);
        let (y, height) = whole(rng, dh);
        for (offset, value) in [(0x60, x), (0x64, y), (0x68, width), (0x6c, height)] {
            put_u32(&mut r, t(offset), value);
        }
    }
    put_u32(&mut r, t(0x70), dw);
    put_u32(&mut r, t(0x74), dh);
    if rng.one_in(4) {
        for offset in [0x78, 0x80, 0x88, 0x90, 0x98, 0xa0] {
            put_u64(&mut r, t(offset), rng.next_u64());
        }
    }
    if rng.one_in(6) {
        for offset in (0xa8..0xdc).step_by(4) {
            put_u32(&mut r, t(offset), rng.interesting_u32());
        }
    }
    if rng.one_in(3) {
        r[SIDE_BUFFERS_USED] = if rng.one_in(16) {
            rng.below(256) as u8
        } else {
            1
        };
        for pair in 0..4 {
            if !rng.bool() {
                continue;
            }
            let at = SIDE_BUFFERS + SIDE_BUFFER_PAIR_LENGTH * pair;
            let input_size = if rng.one_in(8) {
                MAX_INPUT_SIZE + 1
            } else {
                *rng.pick(&[0, 1, 0x100, 0x1000, MAX_INPUT_SIZE])
            };
            let output_size = if rng.one_in(8) {
                MAX_OUTPUT_SIZE + 1
            } else {
                *rng.pick(&[0, 1, 0x10, 0x200, MAX_OUTPUT_SIZE])
            };
            put_u32(&mut r, at, rng.range_u32(0, 7));
            put_u64(&mut r, at + 0x8, rng.next_u64());
            put_u32(&mut r, at + 0x10, input_size);
            put_u32(&mut r, at + 0x14, rng.range_u32(0, 4));
            put_u64(&mut r, at + 0x18, rng.next_u64());
            put_u32(&mut r, at + 0x20, output_size);
            list(rng, &mut r, 2 * pair, input_size);
            list(rng, &mut r, 2 * pair + 1, output_size);
        }
    }
    r
}

/// Whether request `r` has a scatter-gather list past its eight entries:
/// the only end of the VM the device may answer.
fn has_list_past_its_entries(r: &[u8]) -> bool {
    r.len() == TRANSFORM_SIZE
        && r[SIDE_BUFFERS_USED] == 1
        && (0..8).any(|index| u32_at(r, LISTS + LIST_LENGTH * index + 0x80) as usize > LIST_ENTRIES)
}

/// The bytes of a destination of `memory_length` bytes that a successful
/// transform of `plan` may change.
fn writable_bytes(plan: &Plan, memory_length: usize) -> Vec<bool> {
    let mut mask = vec![false; memory_length];
    let rect = plan.destination_rect;
    let window = Window {
        x: rect.x as usize,
        y: rect.y as usize,
        width: rect.width as usize,
        height: rect.height as usize,
    };
    let planes = &plan.destination_format.planes;
    for run in covered(plan.destination_format.format, window) {
        let Some(plane) = planes.get(run.plane) else {
            continue;
        };
        let start = plane.offset + run.row * plane.bytes_per_row + run.start;
        let end = (start + run.length).min(memory_length);
        for byte in mask.iter_mut().take(end).skip(start) {
            *byte = true;
        }
    }
    mask
}

/// One input: a device on the reference scaler or none, its DRIVER_OK,
/// requests and resets.
fn scaler_case(rng: &mut Rng) {
    let pool = Arc::new(Pool {
        surfaces: (1..=SURFACES)
            .map(|id| (id, Arc::new(surface(rng))))
            .collect(),
        holds: Arc::new(AtomicUsize::new(0)),
    });
    let backend: Arc<dyn ScalerBackend> = if rng.one_in(16) {
        Arc::new(NoAccelerator)
    } else {
        Arc::new(Reference(ReferenceBackend {
            capabilities: capabilities(rng),
        }))
    };
    let device = ScalerDevice::new(pool.clone(), backend);
    let ram = Ram::new();
    if !rng.one_in(12) {
        let _ = device.start();
    }
    for _ in 0..rng.range(1, 6) {
        if rng.one_in(16) {
            device.reset();
            if rng.bool() {
                let _ = device.start();
            }
        }
        let mut request = match rng.below(16) {
            0..3 => capabilities_request(rng),
            3 => {
                let length = *rng.pick(&[0, 1, 7, 8, 0x20, 0x610, 0x611, 0x2000]);
                let mut r = rng.bytes(length);
                if r.len() >= 4 {
                    put_u32(&mut r, TYPE, rng.range_u32(0, 3));
                }
                r
            }
            _ => transform_request(rng, &pool),
        };
        if rng.one_in(10) {
            let donor = request.clone();
            maybe_mutate(rng, &mut request, &donor, 1);
        }
        let writable = if rng.one_in(16) {
            rng.below(request.len() + 1)
        } else {
            request.len()
        };
        let destination = pool
            .surfaces
            .get(&u64_at(&request, DESTINATION_MAPPING_ID))
            .cloned();
        let before = destination.as_ref().map(|d| d.memory.lock().clone());
        let outcome = device.process(&request, writable, &ram);
        assert_eq!(
            pool.holds(),
            0,
            "surfaces still held after the request was answered"
        );
        match &outcome {
            Outcome::Reply(reply) => {
                assert_eq!(reply.len(), request.len(), "a reply of another size");
                check_reply(&request, reply, &pool, destination.as_deref(), before);
            }
            Outcome::EndVm(reason) => assert!(
                has_list_past_its_entries(&request),
                "the VM ended ({reason}) with no list past its entries"
            ),
            Outcome::NeedsReset(_) => {}
        }
    }
}

impl Pool {
    fn holds(&self) -> usize {
        self.holds.load(Ordering::SeqCst)
    }
}

/// A reply's checks: a capabilities reply within its room, and a
/// transform that wrote only what it covers.
fn check_reply(
    request: &[u8],
    reply: &[u8],
    pool: &Pool,
    destination: Option<&Surface>,
    before: Option<Vec<u8>>,
) {
    let succeeded = u32_at(reply, STATUS) == io_return::SUCCESS;
    match u32_at(request, TYPE) {
        TYPE_CAPABILITIES if request.len() == CAPABILITIES_SIZE && succeeded => {
            let room = u64_at(request, CAPABILITIES_LENGTH);
            let written = u64_at(reply, CAPABILITIES_LENGTH);
            assert!(
                written <= room && written <= capabilities::LENGTH as u64,
                "{written:#x} bytes of capabilities in {room:#x} of room"
            );
        }
        TYPE_TRANSFORM if request.len() == TRANSFORM_SIZE && succeeded => {
            let source = pool.surfaces.get(&u64_at(request, SOURCE_MAPPING_ID));
            let (Some(source), Some(destination), Some(before)) = (source, destination, before)
            else {
                return;
            };
            let Ok(plan) = Plan::new(
                &Transform::read(request),
                &source.layout,
                &destination.layout,
            ) else {
                return;
            };
            TRANSFORMED.fetch_add(1, Ordering::Relaxed);
            let after = destination.memory.lock().clone();
            let mask = writable_bytes(&plan, after.len());
            let changed =
                (before.iter().zip(&after).zip(&mask)).position(|((b, a), &may)| b != a && !may);
            assert!(
                changed.is_none(),
                "a transform into {:?} changed byte {changed:?} outside its window",
                plan.destination_rect
            );
        }
        _ => {}
    }
}
