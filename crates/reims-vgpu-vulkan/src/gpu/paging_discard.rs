//! Core's synchronize and discard of a paged resource on this platform (opcode 62,
//! `SynchronizeAndDiscardResources`), through core's own task, resource and paging code: a
//! private buffer ([`a_discarded_private_buffer_keeps_its_transfer_buffer_until_the_write_back_completed`])
//! and a multisampled `Depth32Float` texture
//! ([`a_discarded_multisampled_texture_keeps_its_transfer_buffer_until_the_write_back_completed`]).
//!
//! `TaskCore::synchronize_resources` encodes the resource's write-back into the paging command
//! buffer: a blit from the private buffer (`copyFromBuffer:toBuffer:`) or the texture
//! (`copyFromTexture:toBuffer:`, every sample) into its transfer buffer, a no-copy buffer over
//! core's mapping of the guest's pages, whose deallocator releases that mapping. It commits the
//! paging command buffer and waits only until it is scheduled; its discard loop then releases the
//! transfer buffer (`PrivateBufferResource::discard`, `PagedTexture::discard`). Core relies on
//! Metal's contract there: a command buffer retains the resources its commands use until it
//! completes, and a no-copy buffer's deallocator runs once the buffer is released, so after the
//! copy completed (Metal's headers, and measured below). So the runtime must neither run
//! core's deallocator nor free its import of the transfer buffer's pages, which the keep-alive
//! this platform gives it outlives (`backing::keep_alive`), before the paging command buffer
//! completed: an import freed while the copy is in flight leaves the copy writing through a
//! device address with no memory under it, or one the runtime has handed to a later import.
//!
//! Each case holds the paging command buffer in flight for as long as it looks. Ahead of it on the
//! paging queue, a command buffer of the test's writes the resource's bytes (a copy into the
//! private buffer, a depth clear of the texture), then runs the busy kernel ([`crate::gpu::spin`])
//! for about half a second ([`HOLD`]). A queue's command buffers complete in its order, so the
//! paging command buffer completes after the kernel's, and its copy reads what the work ahead
//! wrote, a tracked hazard. As the synchronize returns, its discard made, the test reads:
//! - whether core's deallocator ran: its mapping leaves the platform's record of core's mappings of
//!   the guest's pages as it is unmapped ([`crate::remap::guest_mappings_of`]);
//! - whether the runtime still holds its import of the pages: its live imports of host memory.
//!
//! It then makes a no-copy buffer of the same length over other guest pages, as the next import
//! after a free would, and counts the imports that buffer adds. Only then does it check that the
//! kernel's command buffer had not completed, nor the paging command buffer's completed handler
//! run: so the paging command buffer was in flight from the synchronize's return to that count,
//! and the count is the new buffer's alone. Once the paging command buffer completed it checks
//! that the other pages are untouched, and that the transfer buffer's pages hold the resource's
//! bytes.
//!
//! Metal keeps what the test reads (measured on an Apple M3 Max, macOS 27.0.1, 2026-10-26, with
//! Metal itself: a command buffer that copies a private buffer into a no-copy buffer committed
//! behind one whose kernel runs on the same queue, the no-copy buffer released once the copy's
//! command buffer was scheduled, 300 trials without other load and 300 beside 16 CPU spinners and
//! another process's kernels): the copy's `waitUntilScheduled` returned while the kernel ran,
//! within 0.14 ms and 8 ms under load; its command buffer never read `Completed`, nor ran its
//! completed handler, before the kernel's completed; the no-copy buffer's deallocator never ran
//! before the copy's completed handler; and its pages held the bytes, in all 600.
//!
//! What holds the paging command buffer in flight is the kernel's time alone, so the test's own
//! steps before its check, two imports of host memory among them, must take less, which no
//! kernel's length makes sure of: on an RTX 5080 Laptop GPU an import waits in the driver's lock
//! while other processes make their devices (measured, `.config/nextest.toml`, which runs these
//! tests alone on Linux for it). A run whose kernel's command buffer completed before the test
//! read checks the write-back's bytes, and that core's deallocator ran, and the case runs again,
//! [`RUNS`] times at most, each run timing the kernel anew.

use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use reims_vgpu_core::device::Device;
use reims_vgpu_core::device::config::DeviceConfig;
use reims_vgpu_core::fifo::wire::Records;
use reims_vgpu_core::guest::records::APV_PAGE_SIZE;
use reims_vgpu_core::host::{HostBlocks, HostPtr};
use reims_vgpu_core::memory::descriptor::{GuestPhysicalRange, MemoryMapDescriptor};
use reims_vgpu_core::num::{ToU64 as _, ToUsize as _};
use reims_vgpu_core::platform::blit::{BlitCmd, BlitEncoder as _};
use reims_vgpu_core::platform::command::{CommandBuffer as _, CommandQueue as _};
use reims_vgpu_core::platform::descriptor::RenderPassDescriptor as _;
use reims_vgpu_core::platform::device::GpuDevice as _;
use reims_vgpu_core::platform::os::vm::HostVm as _;
use reims_vgpu_core::platform::render::{RenderCmd, RenderEncoder as _};
use reims_vgpu_core::platform::resource::Buffer as _;
use reims_vgpu_core::platform::types::{CommandBufferStatus, NsErrorText};
use reims_vgpu_core::platform::values::{DepthAttachmentSet, TextureDesc};
use reims_vgpu_core::platform::{Handle as _, HostRange, Platform};
use reims_vgpu_core::raise::OnFault;
use reims_vgpu_core::resources::buffer::private::PrivateBufferResource;
use reims_vgpu_core::resources::host::ResourceHost;
use reims_vgpu_core::resources::manager::create::Resources;
use reims_vgpu_core::resources::resource::{Resource, with_platform};
use reims_vgpu_core::resources::texture::create_texture_resource;
use reims_vgpu_core::resources::texture::dimension::{
    TextureDimension, TextureDimensionRecord, TextureMipDimension, TexturePagingInfo,
};
use reims_vgpu_core::rt::{Held, Hold, Pool, enter_ours};
use reims_vgpu_core::tasks::{ExecStatus, ResourceReference, Task};

use crate::VulkanPlatform;
use crate::gpu::device::VulkanDevice;
use crate::gpu::device::tests::dev;
use crate::gpu::spin;
use crate::os::OsVm;
use crate::platform::tests::platform;
use crate::remap::guest_mappings_of;

type P = VulkanPlatform;

/// How long the busy kernel holds the GPU, from before the synchronize to well past its return
/// (the module docs), as [`spin::rounds_to_hold`] sizes it on the device.
const HOLD: Duration = Duration::from_millis(500);

/// The kernel's fewest rounds: about half a second on the M3 Max.
const FEWEST: u32 = 8_000;

/// The most runs of a case: one whose kernel's command buffer completed before the test read what
/// the paging command buffer holds checked none of that, and the case runs again (the module
/// docs).
const RUNS: usize = 3;

/// The transfer buffer's length: a few MiB, as a guest's paged resources take.
const LEN: u64 = 4 << 20;

/// The guest's page; the guest's RAM starts at page [`BASE`].
const PAGE: u64 = APV_PAGE_SIZE;
const RAM_BASE: u64 = 0x4000_0000;
const BASE: u64 = RAM_BASE / PAGE;

/// The guest's pages, from the start of its RAM: the task root, the task's page table (depth 1),
/// the task's pages, which the resource is paged from at task page 0, then the pages of the
/// no-copy buffer made after the discard.
const ROOT: u64 = 0;
const TABLE: u64 = 1;
const TASK: u64 = 2;
const OTHER: u64 = TASK + LEN / PAGE;
const RAM_LEN: u64 = (OTHER + LEN / PAGE) * PAGE;

/// The byte the other buffer's pages hold throughout.
const UNTOUCHED: u8 = 0x5a;

/// The resource's reference.
const REFERENCE: u32 = 7;

/// `MTLResourceStorageModeShared`; `MTLResourceStorageModePrivate`.
const SHARED: u64 = 0;
const PRIVATE: u64 = 0x20;

/// The multisampled texture: `MTLTextureType2DMultisample`, `MTLPixelFormatDepth32Float`,
/// `MTLTextureUsageRenderTarget`, its samples and its side, so that its guest rows (every sample
/// of a texel, then the next texel) fill [`LEN`].
const TEXTURE_2D_MULTISAMPLE: u64 = 4;
const DEPTH32_FLOAT: u64 = 252;
const RENDER_TARGET: u64 = 4;
const SAMPLES: u64 = 4;
const SIDE: u64 = 512;
const BYTES_PER_ROW: u64 = SIDE * SAMPLES * 4;

/// `MTLLoadActionClear`, `MTLStoreActionStore`, and the depth every sample is cleared to.
const LOAD_CLEAR: u64 = 2;
const STORE_STORE: u64 = 1;
const CLEAR_DEPTH: f32 = 0.25;

/// A command buffer of this platform.
type CommandBuffer = <P as Platform>::CommandBuffer;

/// Runs `body` as a host's call into the library does: marked as ours, in a pool, with a holder
/// whose hold it gets.
fn entry<T>(body: impl for<'u> FnOnce(Hold<'u>) -> T) -> T {
    let _ours = enter_ours();
    let pool = Pool::here();
    let held = Held::new();
    let r = body(Hold::of(&held));
    drop(held);
    drop(pool);
    r
}

/// The runtime's live imports of the caller's host memory: by the pages' pointer or as dma-bufs.
fn imports(d: &VulkanDevice) -> u64 {
    let counts = d.raw.work_counts();
    counts.host_imports_live + counts.dma_buf_memories_live
}

/// The guest's RAM at the host, as a memory map names it.
struct Ram {
    at: HostPtr,
}

impl Ram {
    /// The guest's page `page` from the start of its RAM, at the host.
    fn page(&self, page: u64) -> *mut u8 {
        // SAFETY: inside the `RAM_LEN` bytes the RAM was allocated with.
        unsafe { self.at.as_ptr::<u8>().add((page * PAGE).to_usize()) }
    }

    /// Writes `bytes` at the start of guest page `page`.
    fn write(&self, page: u64, bytes: &[u8]) {
        // SAFETY: inside the RAM, which is mapped writable; nothing reads it meanwhile.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.page(page), bytes.len()) }
    }

    /// The `len` bytes from the start of guest page `page`.
    fn read(&self, page: u64, len: u64) -> Vec<u8> {
        // SAFETY: inside the RAM, which is mapped; the GPU's writes to it have completed.
        unsafe { std::slice::from_raw_parts(self.page(page), len.to_usize()).to_vec() }
    }

    /// The host range of guest pages `pages`.
    fn host_range(&self, pages: std::ops::Range<u64>) -> std::ops::Range<u64> {
        let at = self.page(pages.start).addr().to_u64();
        at..at + (pages.end - pages.start) * PAGE
    }
}

/// What a case makes its resource with: the platform, the device the test's own objects are made
/// on, and the task as its resources hold it.
struct Made<'a> {
    p: &'a Arc<P>,
    d: &'a VulkanDevice,
    host: &'a Arc<dyn ResourceHost<P>>,
}

/// One case: the resource it pages from task page 0, made in the task's platform scope; the
/// work that writes the resource's host copy, encoded ahead of the paging (`write`), with what it
/// keeps until the paging completed; and the bytes the guest's pages hold once written back.
struct Case<K> {
    resource: Arc<Resource<P>>,
    write: Box<dyn FnOnce(&CommandBuffer) -> K>,
    want: Vec<u8>,
}

/// The first byte at which `got` differs from `want`, with both bytes.
fn first_difference(got: &[u8], want: &[u8]) -> Option<(usize, u8, u8)> {
    got.iter()
        .zip(want)
        .position(|(g, w)| g != w)
        .map(|at| (at, got[at], want[at]))
}

/// [`run_case`] until a run's kernel held the paging command buffer in flight through what the
/// test read, at most [`RUNS`] times (the module docs).
fn write_back_then_discard<K>(case: impl Fn(&Made<'_>) -> Case<K>) {
    let mut missed = Vec::new();
    for _ in 0..RUNS {
        match run_case(&case) {
            Ok(()) => return,
            Err(why) => missed.push(why),
        }
    }
    panic!("in each of {RUNS} runs, {}", missed.join("; "));
}

/// A device over guest RAM with a kernel task, the resource `case` makes registered in it, its
/// host copy written ahead of the paging, then core's synchronize and discard of it, checked as
/// the module docs say. `Err` where the kernel's command buffer completed before the test read
/// what the paging command buffer holds: the run checked the write-back's bytes, and that core's
/// deallocator ran, and nothing it would have read.
fn run_case<K>(case: &impl Fn(&Made<'_>) -> Case<K>) -> Result<(), String> {
    let p = Arc::new(platform());
    let d = dev();
    let ram = Ram {
        at: crate::os::allocate_aliasable(RAM_LEN).unwrap(),
    };
    // The task's root: its page table's page and depth (`TaskRoot2`), the rest zeros; every one of
    // its pages writable, each the guest page of its own index past `TASK`.
    let mut root = Vec::new();
    root.extend_from_slice(&u32::try_from(BASE + TABLE).unwrap().to_le_bytes());
    root.extend_from_slice(&1u32.to_le_bytes());
    root.resize(40, 0);
    ram.write(ROOT, &root);
    let table: Vec<u8> = (0..LEN / PAGE)
        .flat_map(|i| (u32::try_from(BASE + TASK + i).unwrap() | 1 << 31).to_le_bytes())
        .collect();
    ram.write(TABLE, &table);
    ram.write(OTHER, &vec![UNTOUCHED; LEN.to_usize()]);

    let mut map = MemoryMapDescriptor::new();
    map.add_range(GuestPhysicalRange {
        physical_address: RAM_BASE,
        physical_length: RAM_LEN,
        host_address: ram.at,
    });
    let mut config = DeviceConfig::<P>::new(Some(d.clone_ref()));
    config.memory_map_descriptor = Some(Arc::new(map));
    config.blocks = HostBlocks {
        raise_interrupt: Some(Arc::new(|_| {})),
        ..HostBlocks::default()
    };
    let device = entry(|_| Device::new(Arc::clone(&p), config)).unwrap();
    let root = u32::try_from(BASE + ROOT).unwrap();
    entry(|hold| device.create_task_id(hold, 0, root, LEN, false)).unwrap();
    let Task::Local(task) = device.get_task_id(OnFault::Return, 0).unwrap() else {
        panic!("task 0 is local")
    };
    // The task as its resources hold it, from its resource manager.
    let (_, manager, _) = task.base.parts().unwrap();
    let host = manager.task().unwrap();
    let Case {
        resource,
        write,
        want,
    } = entry(|_| {
        with_platform(&p, || {
            case(&Made {
                p: &p,
                d: &d,
                host: &host,
            })
        })
    });
    assert!(manager.register_object::<Resources>(REFERENCE, Arc::clone(&resource)));
    drop(manager);

    // Ahead of the paging command buffer on its queue: the resource's bytes written, then the
    // busy kernel.
    let queue = device.blit_down_queue().unwrap();
    let words = d
        .new_buffer_of_length(OnFault::Die, 16, SHARED)
        .unwrap()
        .unwrap();
    let state = spin::state(&d);
    let rounds = spin::rounds_to_hold(&d, &queue, &state, HOLD, FEWEST);
    // SAFETY: the kernel's rounds, in the first of the buffer's four words, before its GPU use.
    unsafe { words.contents().as_ptr::<u32>().write(rounds) };
    let ahead = queue.command_buffer(OnFault::Die).unwrap().unwrap();
    let kept = write(&ahead);
    spin::encode(&ahead, &state, &words);
    ahead.commit(OnFault::Die).unwrap();
    ahead.wait_until_scheduled().unwrap();
    // The host's copy is the one to write back, as after the guest's GPU work wrote it.
    resource.set_is_host_valid(true);
    resource.set_is_guest_valid(false);

    let (done, completed) = mpsc::channel::<(ExecStatus, Option<NsErrorText>)>();
    let done = Arc::new(move |status: ExecStatus, error: Option<NsErrorText>| {
        let _ = done.send((status, error));
    });
    let records = REFERENCE.to_le_bytes();
    let task_pages = ram.host_range(TASK..OTHER);
    let before = imports(&d);
    entry(|hold| {
        let records = Records::<ResourceReference>::new(&records[..], 1);
        records.map(|records| task.synchronize_resources(hold, records, true, done))
    })
    .unwrap()
    .unwrap();
    // The discard is made: what the runtime holds now, the paging command buffer in flight.
    let shown = guest_mappings_of(task_pages.clone());
    let during = imports(&d);
    // The next import, over other guest pages of the same length.
    // SAFETY: guest pages of the RAM, mapped until the RAM's release below, after the buffer's.
    let other_range = unsafe { HostRange::new(HostPtr::from_ptr(ram.page(OTHER)), LEN) };
    let other = d
        .new_buffer_over_host_memory(OnFault::Die, other_range, SHARED, None)
        .unwrap()
        .unwrap();
    let others = imports(&d).saturating_sub(during);
    // Read last: whether the paging command buffer was in flight through every read above.
    let ahead_status = ahead.status();
    let early = completed.try_recv().ok();
    let held = early.is_none() && ahead_status != CommandBufferStatus::COMPLETED;
    if held {
        let mapped: u64 = shown.iter().map(|run| run.end - run.start).sum();
        assert!(
            mapped == LEN && during > before,
            "the transfer buffer went before its paging command buffer completed: core's \
             deallocator {} run ({mapped:#x} of its {LEN:#x} bytes still mapped), and the \
             runtime's live imports of host memory are {during}, {before} before the synchronize",
            if mapped == LEN { "has not" } else { "has" },
        );
    }

    let (status, error) = match &early {
        Some(seen) => seen.clone(),
        None => completed.recv_timeout(Duration::from_secs(30)).unwrap(),
    };
    assert_eq!((status, error), (ExecStatus::OK, None));
    ahead.wait_until_completed(OnFault::Die).unwrap();
    assert_eq!(
        ahead.status(),
        CommandBufferStatus::COMPLETED,
        "{:?}",
        ahead.error()
    );
    // After its completion the runtime lets go of what the paging command buffer retained: core's
    // deallocator runs and the import goes. Where the paging command buffer may have completed
    // before the other buffer was made, its import's free may have landed inside that count.
    let mut released = false;
    for _ in 0..10_000 {
        if guest_mappings_of(task_pages.clone()).is_empty()
            && (!held || imports(&d) == before + others)
        {
            released = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(
        released,
        "the transfer buffer was not released after its paging command buffer completed: {:?} \
         still mapped, {} live imports",
        guest_mappings_of(task_pages),
        imports(&d)
    );
    let got = ram.read(TASK, LEN);
    assert!(
        got == want,
        "the guest's pages do not hold the resource's bytes: first difference (offset, got, want) \
         {:?}",
        first_difference(&got, &want)
    );
    let untouched = vec![UNTOUCHED; LEN.to_usize()];
    let got = ram.read(OTHER, LEN);
    assert!(
        got == untouched,
        "the write-back reached the next import's pages: first difference (offset, got, want) \
         {:?}",
        first_difference(&got, &untouched)
    );

    drop((other, kept, resource, host, task, ahead));
    drop((words, state, queue));
    entry(|_| drop(device));
    assert!(OsVm::new().deallocate(ram.at, RAM_LEN).is_success());
    if held {
        Ok(())
    } else {
        // The next run counts the imports again: this run's go first.
        for _ in 0..10_000 {
            if imports(&d) <= before {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        Err(format!(
            "the paging command buffer completed before the test read what it holds ({early:?}, \
             the kernel's command buffer {ahead_status:?}, {rounds} rounds): nothing held it in \
             flight"
        ))
    }
}

/// A private buffer's write-back, a copy into its transfer buffer, keeps the transfer buffer
/// through its discard until the paging command buffer completed, and lands in the guest's pages
/// and nowhere else
#[test]
fn a_discarded_private_buffer_keeps_its_transfer_buffer_until_the_write_back_completed() {
    write_back_then_discard(|made| {
        // `BufferResourceDescriptor`: its length, its page and the private flag.
        let mut descriptor = Vec::new();
        descriptor.extend_from_slice(&LEN.to_le_bytes());
        descriptor.extend_from_slice(&0u32.to_le_bytes());
        descriptor.extend_from_slice(&[1, 0, 0, 0]);
        let resource = PrivateBufferResource::new(Arc::downgrade(made.host), &descriptor, None)
            .unwrap()
            .unwrap();
        let private = resource.get_buffer().unwrap().unwrap();
        // Words of their own index's.
        let want: Vec<u8> = (0..LEN / 4)
            .flat_map(|i| (u32::try_from(i).unwrap().wrapping_mul(0x9e37_79b9) | 1).to_le_bytes())
            .collect();
        let source = made
            .d
            .new_buffer_of_length(OnFault::Die, LEN, SHARED)
            .unwrap()
            .unwrap();
        // SAFETY: the shared buffer's `LEN` bytes, before any GPU use.
        unsafe {
            std::ptr::copy_nonoverlapping(want.as_ptr(), source.contents().as_ptr(), want.len());
        }
        Case {
            resource,
            write: Box::new(move |ahead: &CommandBuffer| {
                let blit = ahead.blit_encoder(OnFault::Die).unwrap().unwrap();
                for cmd in [
                    BlitCmd::CopyBufferToBuffer {
                        source_buffer: Some(&source),
                        source_offset: 0,
                        destination_buffer: Some(&private),
                        destination_offset: 0,
                        size: LEN,
                    },
                    BlitCmd::EndEncoding,
                ] {
                    blit.encode(OnFault::Die, cmd).unwrap();
                }
                (source, private)
            }),
            want,
        }
    });
}

/// A multisampled `Depth32Float` texture's write-back, a copy of every sample into its transfer
/// buffer, keeps the transfer buffer through its discard until the paging command buffer
/// completed, and lands in the guest's pages and nowhere else
#[test]
fn a_discarded_multisampled_texture_keeps_its_transfer_buffer_until_the_write_back_completed() {
    write_back_then_discard(|made| {
        let texture = made
            .d
            .new_texture(
                OnFault::Die,
                &made.p.texture_descriptor(&TextureDesc {
                    texture_type: TEXTURE_2D_MULTISAMPLE,
                    pixel_format: DEPTH32_FLOAT,
                    width: SIDE,
                    height: SIDE,
                    sample_count: SAMPLES,
                    usage: RENDER_TARGET,
                    resource_options: Some(PRIVATE),
                    ..TextureDesc::default()
                }),
            )
            .unwrap()
            .unwrap();
        // The guest's record of a one-plane texture (object type 2) of one level: its rows of
        // every sample of each texel, 4 bytes a sample, filling the pages from task page 0.
        let dimension = TextureDimensionRecord {
            header: TextureDimension {
                bits: 1,
                array_length: 1,
                base_offset: 0,
                bytes_per_slice: LEN,
                block_width: 1,
                block_height: 1,
                block_depth: 1,
                block_bytes: 4,
            },
            mips: vec![TextureMipDimension {
                offset: 0,
                size: LEN,
                bytes_per_row: BYTES_PER_ROW,
                width: u32::try_from(SIDE).unwrap(),
                height: u32::try_from(SIDE).unwrap(),
                depth: 1,
            }],
        };
        let paging = TexturePagingInfo {
            page: 0,
            unknown04: [0; 4],
            length: LEN,
        };
        // The record's length: its header and one mip.
        let length = 0x18 + 0x24;
        let target = texture.clone_ref();
        let resource = create_texture_resource(
            Arc::downgrade(made.host),
            2,
            &paging,
            &dimension,
            length,
            texture,
        )
        .unwrap()
        .unwrap();
        let p = Arc::clone(made.p);
        Case {
            resource,
            write: Box::new(move |ahead: &CommandBuffer| {
                let pass = p.render_pass_descriptor();
                pass.set_depth_attachment(&DepthAttachmentSet {
                    texture: Some(Some(&target)),
                    load_action: Some(LOAD_CLEAR),
                    store_action: Some(STORE_STORE),
                    clear_depth: Some(f64::from(CLEAR_DEPTH)),
                    ..DepthAttachmentSet::default()
                });
                let render = ahead.render_encoder(OnFault::Die, &pass).unwrap().unwrap();
                render.encode(OnFault::Die, RenderCmd::EndEncoding).unwrap();
                target
            }),
            want: CLEAR_DEPTH
                .to_bits()
                .to_le_bytes()
                .repeat((LEN / 4).to_usize()),
        }
    });
}
