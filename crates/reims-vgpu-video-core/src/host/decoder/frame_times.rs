//! Where a decoder session's frames spend their time on the host, logged
//! when the session ends (ours). Each frame's time goes to the backend's
//! decode call, the wait for the guest's buffer (`Dbuf`), the copy into
//! it, and the wait for the guest's answer to its emission (`Demt`); the
//! session's thread waits for the guest's next message the rest of the
//! time. A frame decoded while the one before it was copied has its decode
//! counted in that copy's time too.
//!
//! The copy is split as the thread that copies sees it (ours): the wait for
//! the frame to be whole on its device, where the session decodes ahead
//! (`DecodedFrame::wait`); the backend's read back
//! (`DecodedFrame::read_back`), which waits for the decode itself where
//! nothing waited before; its copy on the CPU into the bytes the guest's
//! surface lends (`FrameReadBack::copy_to`); and the lend around that copy
//! (`GuestSurface::write`), the host's own cost of reaching the guest's
//! pages.

use std::time::Duration;

/// The sums over a session's frames.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FrameTimes {
    /// The `dfrm`s decoded.
    pub(crate) frames: u64,
    /// The backend's decode calls.
    pub(crate) decode: Duration,
    /// From each `Dbuf` to the guest's buffer.
    pub(crate) buffer: Duration,
    /// The backend's copies into the guest's buffers: for a backend that
    /// decodes on a GPU, the wait for the decode too.
    pub(crate) copy: Duration,
    /// From each `Demt` to the guest's answer.
    pub(crate) emit: Duration,
    /// Waiting for the guest's next message.
    pub(crate) idle: Duration,
    /// The frames decoded while the frame before them was copied.
    pub(crate) ahead: u64,
    /// The parts of the copies, as the thread that copied measured them.
    pub(crate) copies: CopyTimes,
}

/// Where the copies' time went, summed, as the thread that copies measures
/// it (module docs).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct CopyTimes {
    /// Waiting for each frame to be whole on its device.
    pub(crate) wait: Duration,
    /// The backend's read backs.
    pub(crate) read_back: Duration,
    /// The backend's copies into the lent bytes.
    pub(crate) fill: Duration,
    /// The lends, each with its backend's copy inside it.
    pub(crate) lend: Duration,
    /// The backend's device's writes into the guest's buffers, and those it
    /// declined.
    pub(crate) device: Duration,
    /// The frames its device wrote.
    pub(crate) written: u64,
    /// The frames its device declined to write.
    pub(crate) declined: u64,
    /// The frames the CPU copied.
    pub(crate) copied: u64,
}

impl CopyTimes {
    /// Adds `other`'s parts to these.
    pub(crate) fn add(&mut self, other: CopyTimes) {
        self.wait += other.wait;
        self.read_back += other.read_back;
        self.fill += other.fill;
        self.lend += other.lend;
        self.device += other.device;
        self.written += other.written;
        self.declined += other.declined;
        self.copied += other.copied;
    }
}

impl FrameTimes {
    /// Adds `took` to the part `part` picks.
    pub(crate) fn add(&mut self, part: fn(&mut Self) -> &mut Duration, took: Duration) {
        *part(self) += took;
    }

    /// The log's line, each part's mean a frame; `None` with no frame.
    pub(crate) fn summary(&self) -> Option<String> {
        if self.frames == 0 {
            return None;
        }
        let per_frame = |total: Duration| {
            total.as_secs_f64() * 1e3 / f64::from(u32::try_from(self.frames).unwrap_or(u32::MAX))
        };
        let copies = &self.copies;
        let per = |total: Duration, count: u64| {
            total.as_secs_f64() * 1e3 / f64::from(u32::try_from(count.max(1)).unwrap_or(u32::MAX))
        };
        Some(format!(
            "{} frames; a frame took {:.2} ms to decode, {:.2} ms for its buffer, {:.2} ms to \
             copy, {:.2} ms to emit, and {:.2} ms waiting for the guest; {} decoded while the \
             frame before was copied; a copy waited {:.2} ms for its frame, read it back in \
             {:.2} ms, copied it into the guest's buffer in {:.2} ms, and took {:.2} ms more to \
             lend that buffer; {} written by the device in {:.2} ms each, {} declined, and {} \
             copied by the CPU, which read back in {:.2} ms and copied in {:.2} ms each",
            self.frames,
            per_frame(self.decode),
            per_frame(self.buffer),
            per_frame(self.copy),
            per_frame(self.emit),
            per_frame(self.idle),
            self.ahead,
            per_frame(copies.wait),
            per_frame(copies.read_back),
            per_frame(copies.fill),
            per_frame(copies.lend.saturating_sub(copies.fill)),
            copies.written,
            per(copies.device, copies.written + copies.declined),
            copies.declined,
            copies.copied,
            per(copies.read_back, copies.copied),
            per(copies.fill, copies.copied),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_summary_gives_each_parts_mean() {
        assert_eq!(FrameTimes::default().summary(), None);
        let times = FrameTimes {
            frames: 4,
            decode: Duration::from_millis(2),
            buffer: Duration::from_millis(4),
            copy: Duration::from_millis(40),
            emit: Duration::from_millis(8),
            idle: Duration::from_millis(1),
            ahead: 3,
            copies: CopyTimes {
                wait: Duration::from_millis(12),
                read_back: Duration::from_millis(4),
                fill: Duration::from_millis(20),
                lend: Duration::from_millis(26),
                device: Duration::from_millis(3),
                written: 1,
                declined: 1,
                copied: 2,
            },
        };
        assert_eq!(
            times.summary().as_deref(),
            Some(
                "4 frames; a frame took 0.50 ms to decode, 1.00 ms for its buffer, 10.00 ms \
                 to copy, 2.00 ms to emit, and 0.25 ms waiting for the guest; 3 decoded while \
                 the frame before was copied; a copy waited 3.00 ms for its frame, read it back \
                 in 1.00 ms, copied it into the guest's buffer in 5.00 ms, and took 1.50 ms \
                 more to lend that buffer; 1 written by the device in 1.50 ms each, 1 \
                 declined, and 2 copied by the CPU, which read back in 2.00 ms and copied in \
                 10.00 ms each"
            )
        );
    }
}
