//! The guest buffers an AV1 or VP9 session holds, and when it lets each
//! go, as Apple's AV1 and VP9 decoders do.
//!
//! Apple's AVD asks the guest for a buffer (`Dbuf`) for every frame of a
//! temporal unit: each frame decoded, hidden or shown, of every spatial
//! layer, and each frame shown again (`show_existing_frame`). A frame
//! decoded is decoded into its buffer, and a frame shown again is emitted
//! in the buffer it was decoded into; the buffer lent for showing it again
//! is never written. Measured on Apple's host with libaom's, libvpx's and
//! SVT-AV1's streams with forward key frames, frames shown again, and two
//! and three spatial layers (macOS 27.0.1, 26A434, 2026-10-24), each
//! buffer is let go at the start of step k, the steps numbered in the
//! order the frames came, where k is the latest of:
//! - one past the step after which no reference slot held its frame;
//! - two past the step that decoded into it, or one past the step that
//!   lent it to show a frame again;
//! - two past the last step that loaded its frame's state
//!   (`primary_ref_frame`).
//!
//! Its `rels` then comes with the next sweep, the guest's next message's
//! or the next step's, and a buffer no `Demt` named is given back in the
//! `Dfub` after step k. The rule gave the step of every buffer the nine
//! streams' runs let go, 794 of them, synchronously, and of every buffer
//! of three runs that sent a frame each 33 ms after its reply,
//! asynchronously. Asynchronously AVD asks for a unit's buffers before it
//! decodes the unit's frames, so it holds more at once.
//!
//! So the most it holds when each step is over before the next is asked
//! for is AV1's eight reference slots, the frame of the last step out of
//! every slot, the last step's primary reference, and the step's own
//! buffer, 11; asynchronously, one more for each step asked for ahead.
//!
//! Apple's VP9 decoder follows the same rule, a frame having no primary
//! reference, with a step for every frame a sample carries (a superframe's
//! hidden frames, each spatial layer, each frame shown again) and for every
//! frame it refuses: one with another size than the description's, and
//! each after it, those later in its sample and those of the samples
//! before the next key frame. A refused frame's buffer is never written
//! and goes one past its step, as a buffer lent to show a frame again does,
//! and after it no slot holds a frame: the buffers the slots held go one
//! past it too. Measured on Apple's host session on the M3 Max (macOS
//! 27.0.1, 2026-10-25), with buffers lent from a pool of each `Dpba`'s
//! attributes and kept until their `rels`, synchronously and with each
//! frame sent 33 ms after the last one's reply, on 25 libvpx streams:
//! alternate references (vpxenc's `--auto-alt-ref` 1 and 6), shown existing
//! and intra-only frames (`vp9-craft`), error resilience, frame-parallel
//! and realtime encodes, 8 and 10 bits, odd sizes, dynamic resize, and two
//! and three spatial layers (`vp9-svc`), whole and one frame a sample. The
//! rule gave the step of every one of the 5,628 buffers let go.

use crate::backend::FrameDecoded;

/// The most buffers a session holds at once (ours, for host safety): AV1's
/// eight slots twice over. The rule above holds at most 11 where each step
/// is over before the next is asked for, and Apple's AVD held 13 at most
/// asynchronously, where it asks ahead. Past this an asynchronous unit's
/// next step waits for the steps asked before it, and then a buffer is let
/// go early.
pub(super) const MAX_LENDS: usize = 2 * 8;

/// One buffer lent for a frame.
pub(super) struct Lend<B> {
    pub(super) buffer: B,
    /// The frame it was lent for, by number ([`FrameDecoded::picture`]).
    picture: u64,
    /// Whether it was lent to show a frame again, and is never written.
    shown_again: bool,
    /// Whether it was lent for a frame refused, and is never written.
    refused: bool,
    /// The step that lent it.
    lent_at: u64,
    /// The last step that loaded its frame's state.
    primary_at: Option<u64>,
    /// The step after which no slot held its frame.
    out_at: Option<u64>,
    /// Whether its frame is in it.
    pub(super) written: bool,
    /// Whether a `Demt` has named it.
    pub(super) emitted: bool,
}

impl<B> Lend<B> {
    /// The step at whose start it is let go, once that is known: once no
    /// slot holds its frame.
    fn released_at(&self) -> Option<u64> {
        let out = self.out_at?;
        let own = (self.lent_at).saturating_add(if self.shown_again || self.refused {
            1
        } else {
            2
        });
        let primary = self.primary_at.map_or(0, |at| at.saturating_add(2));
        Some(out.saturating_add(1).max(own).max(primary))
    }
}

/// The buffers a session holds for its frames, in the order they were
/// lent.
pub(super) struct Lends<B> {
    lends: Vec<Lend<B>>,
    /// The steps taken so far: the next step's number.
    steps: u64,
}

impl<B> Default for Lends<B> {
    fn default() -> Self {
        Self {
            lends: Vec::new(),
            steps: 0,
        }
    }
}

impl<B> Lends<B> {
    /// Starts a step: its number.
    pub(super) fn begin(&mut self) -> u64 {
        let at = self.steps;
        self.steps = self.steps.saturating_add(1);
        at
    }

    /// Holds `buffer`, lent at step `at` for `step`'s frame. Where that
    /// would pass [`MAX_LENDS`], the buffer due first is let go first, and
    /// returned.
    pub(super) fn lend(&mut self, at: u64, step: &FrameDecoded, buffer: B) -> Option<Lend<B>> {
        let early = (self.lends.len() >= MAX_LENDS)
            .then(|| {
                let (first, _) = (self.lends.iter().enumerate())
                    .min_by_key(|(i, lend)| (lend.released_at().unwrap_or(u64::MAX), *i))?;
                Some(self.lends.remove(first))
            })
            .flatten();
        self.lends.push(Lend {
            buffer,
            picture: step.picture,
            shown_again: step.shown_again,
            refused: step.refused,
            lent_at: at,
            primary_at: None,
            out_at: None,
            written: false,
            emitted: false,
        });
        early
    }

    /// Follows step `at`: its primary reference, and the frames no slot
    /// holds after it. A buffer lent to show a frame again, or for a frame
    /// refused, is out at once. The buffers of the steps asked for after it
    /// are not yet followed.
    pub(super) fn follow(&mut self, at: u64, step: &FrameDecoded) {
        for lend in self.lends.iter_mut().filter(|lend| lend.lent_at <= at) {
            let written = !lend.shown_again && !lend.refused;
            if written && step.primary_reference == Some(lend.picture) {
                lend.primary_at = Some(at);
            }
            let held = written && step.references.contains(&Some(lend.picture));
            if lend.out_at.is_none() && !held {
                lend.out_at = Some(at);
            }
        }
    }

    /// The buffer step `at`'s frame is emitted in: for a frame shown
    /// again, the buffer it was decoded into, where the session holds it;
    /// else the buffer the step lent, where the guest gave one.
    pub(super) fn target(&mut self, at: u64, step: &FrameDecoded) -> Option<&mut Lend<B>> {
        let decoded = step
            .shown_again
            .then(|| {
                (self.lends.iter()).position(|lend| {
                    !lend.shown_again && !lend.refused && lend.picture == step.picture
                })
            })
            .flatten();
        let index = decoded.or_else(|| self.lends.iter().position(|lend| lend.lent_at == at))?;
        self.lends.get_mut(index)
    }

    /// Lets go, after step `at`, of every buffer due at the next step's
    /// start, in the order they were lent.
    pub(super) fn release(&mut self, at: u64) -> Vec<Lend<B>> {
        let next = at.saturating_add(1);
        let (released, kept) = std::mem::take(&mut self.lends)
            .into_iter()
            .partition(|lend| lend.released_at().is_some_and(|due| due <= next));
        self.lends = kept;
        released
    }

    /// Lets go of every buffer, as the decoder goes: they are returned, for
    /// the caller to let go of when it says.
    pub(super) fn clear(&mut self) -> Vec<Lend<B>> {
        std::mem::take(&mut self.lends)
    }

    /// How many buffers it holds.
    pub(super) fn len(&self) -> usize {
        self.lends.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A step of `picture` with `slots` after it.
    fn step(picture: u64, slots: [u64; 8]) -> FrameDecoded {
        FrameDecoded {
            picture,
            shown_again: false,
            refused: false,
            width: 64,
            height: 48,
            references: slots.map(|slot| (slot != 0).then_some(slot)),
            primary_reference: None,
            output: true,
            largest: (64, 48),
        }
    }

    /// Takes `steps` in order, lending each step a buffer numbered by the
    /// step, and gives each buffer's release step.
    fn releases(steps: &[FrameDecoded]) -> Vec<(u64, u64)> {
        let mut lends = Lends::default();
        let mut released = Vec::new();
        for step in steps {
            let at = lends.begin();
            assert!(lends.lend(at, step, at).is_none());
            lends.follow(at, step);
            released.extend(
                lends
                    .release(at)
                    .into_iter()
                    .map(|lend| (lend.buffer, at + 1)),
            );
        }
        released
    }

    #[test]
    fn a_frame_no_slot_keeps_is_held_through_the_next_step() {
        // A key frame in every slot, then two frames that refresh none.
        let steps = [step(1, [1; 8]), step(2, [1; 8]), step(3, [1; 8])];
        assert_eq!(releases(&steps), [(1, 3)]);
    }

    #[test]
    fn a_frame_is_let_go_one_step_after_it_leaves_the_slots() {
        // Frame 1 in slot 0 alone, replaced by frame 3 at step 2.
        let steps = [
            step(1, [1, 0, 0, 0, 0, 0, 0, 0]),
            step(2, [1, 2, 0, 0, 0, 0, 0, 0]),
            step(3, [3, 2, 0, 0, 0, 0, 0, 0]),
            step(4, [3, 2, 4, 0, 0, 0, 0, 0]),
        ];
        assert_eq!(releases(&steps), [(0, 3)]);
    }

    #[test]
    fn a_buffer_lent_to_show_a_frame_again_goes_at_the_next_step() {
        let hidden = step(1, [1; 8]);
        let again = FrameDecoded {
            shown_again: true,
            ..step(1, [1; 8])
        };
        let mut lends = Lends::default();
        let at = lends.begin();
        lends.lend(at, &hidden, "decoded");
        lends.follow(at, &hidden);
        assert!(lends.release(at).is_empty());
        let at = lends.begin();
        lends.lend(at, &again, "again");
        lends.follow(at, &again);
        // The frame shown again is emitted in its own buffer.
        let target = lends.target(at, &again).map(|lend| lend.buffer);
        assert_eq!(target, Some("decoded"));
        let released: Vec<_> = lends.release(at).into_iter().map(|l| l.buffer).collect();
        assert_eq!(released, ["again"]);
        assert_eq!(lends.len(), 1);
    }

    #[test]
    fn a_refused_frame_goes_at_the_next_step_with_every_frame_the_slots_held() {
        // A key frame in every slot, a frame into slot 0, then a VP9 frame
        // refused and one after it: the refused frames' buffers go a step
        // past each, and the slots' with the first.
        let refused = |picture| FrameDecoded {
            refused: true,
            output: false,
            ..step(picture, [0; 8])
        };
        let steps = [
            step(1, [1; 8]),
            step(2, [2, 1, 1, 1, 1, 1, 1, 1]),
            refused(3),
            refused(4),
        ];
        assert_eq!(releases(&steps), [(0, 3), (1, 3), (2, 3), (3, 4)]);
    }

    #[test]
    fn a_primary_reference_is_held_two_steps_past_its_last_load() {
        // Frame 1 in slot 0, which step 1 loads its state from and
        // replaces: held through step 2.
        let first = step(1, [1, 0, 0, 0, 0, 0, 0, 0]);
        let second = FrameDecoded {
            primary_reference: Some(1),
            ..step(2, [2, 0, 0, 0, 0, 0, 0, 0])
        };
        let third = step(3, [2, 0, 0, 0, 0, 0, 0, 0]);
        let fourth = step(4, [2, 0, 0, 0, 0, 0, 0, 0]);
        let released = releases(&[first, second, third, fourth]);
        assert_eq!(released, [(0, 3), (2, 4)]);
    }

    #[test]
    fn steps_asked_for_ahead_are_let_go_as_steps_taken_in_turn() {
        // A unit of four frames asked for before any is followed, as an
        // asynchronous unit's are, after a key frame in every slot.
        let steps = [
            step(1, [1; 8]),
            step(2, [1, 2, 1, 1, 1, 1, 1, 1]),
            step(3, [1, 2, 3, 1, 1, 1, 1, 1]),
            step(4, [4, 2, 3, 1, 1, 1, 1, 1]),
            step(5, [4, 5, 3, 1, 1, 1, 1, 1]),
        ];
        // Frame 2, lent at step 1, leaves the slots at step 4.
        let in_turn = releases(&steps);
        assert_eq!(in_turn, [(1, 5)]);
        let mut lends = Lends::default();
        let first = lends.begin();
        lends.lend(first, &steps[0], first);
        lends.follow(first, &steps[0]);
        assert!(lends.release(first).is_empty());
        let ats: Vec<u64> = steps[1..]
            .iter()
            .map(|step| {
                let at = lends.begin();
                assert!(lends.lend(at, step, at).is_none());
                at
            })
            .collect();
        let mut ahead = Vec::new();
        for (at, step) in ats.into_iter().zip(&steps[1..]) {
            lends.follow(at, step);
            ahead.extend(
                lends
                    .release(at)
                    .into_iter()
                    .map(|lend| (lend.buffer, at + 1)),
            );
        }
        assert_eq!(ahead, in_turn);
    }

    #[test]
    fn no_more_than_the_bound_is_held() {
        let mut lends = Lends::default();
        let mut early = 0;
        for picture in 1..40 {
            let at = lends.begin();
            // Every frame kept in a slot by a stream that lies: nothing
            // is ever due.
            let step = FrameDecoded {
                references: [Some(picture); 8],
                ..step(picture, [0; 8])
            };
            early += usize::from(lends.lend(at, &step, picture).is_some());
            assert!(lends.len() <= MAX_LENDS);
        }
        assert_eq!(early, 39 - MAX_LENDS);
    }
}
