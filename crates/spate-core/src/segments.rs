//! Segment bookkeeping for multi-connection downloads.
//!
//! A download is split into byte ranges ("segments"). Each connection (worker)
//! owns at most one segment at a time and streams it sequentially. When a
//! worker finishes, it *steals* work by splitting the segment that is
//! projected to finish last, so all connections stay busy until the very end
//! and nobody waits on a single slow tail.
//!
//! Slots are never removed, so a slot index handed to a worker stays valid for
//! the whole download.

use serde::{Deserialize, Serialize};

/// A contiguous byte range `[start, end)` of which `done` bytes from `start`
/// have been durably written to the output file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    pub start: u64,
    pub end: u64,
    pub done: u64,
}

impl Segment {
    pub fn new(start: u64, end: u64) -> Self {
        Self { start, end, done: 0 }
    }

    pub fn pos(&self) -> u64 {
        self.start + self.done
    }

    pub fn remaining(&self) -> u64 {
        self.end.saturating_sub(self.pos())
    }

    pub fn is_complete(&self) -> bool {
        self.remaining() == 0
    }
}

/// Read-only view of a segment for progress displays.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SegmentView {
    pub start: u64,
    pub end: u64,
    /// Bytes received (written or buffered) from `start`.
    pub received: u64,
    pub active: bool,
    pub speed: f64,
}

#[derive(Clone, Debug)]
struct Slot {
    seg: Segment,
    /// Bytes handed to the owner for writing but not yet committed.
    reserved: u64,
    owner: Option<usize>,
    /// Smoothed throughput of the owning connection in bytes/second.
    speed: f64,
}

impl Slot {
    fn cursor(&self) -> u64 {
        self.seg.pos() + self.reserved
    }

    fn unclaimed_remaining(&self) -> u64 {
        self.seg.end.saturating_sub(self.cursor())
    }
}

#[derive(Clone, Debug, Default)]
pub struct SegmentTable {
    slots: Vec<Slot>,
    min_split: u64,
    /// How far ahead (seconds) to project a connection's progress when
    /// deciding which segment will finish last.
    horizon_secs: f64,
}

/// Size used for downloads whose length is unknown until the stream ends.
pub const UNKNOWN_END: u64 = u64::MAX;

/// Each half of a split must hold at least this much transfer time, which
/// comfortably exceeds the setup cost of a new connection (TCP + TLS).
const SPLIT_FLOOR_SECS: f64 = 0.5;
const SPLIT_FLOOR_MIN: u64 = 128 * 1024;
const SPLIT_FLOOR_MAX: u64 = 64 * 1024 * 1024;

impl SegmentTable {
    /// A fresh table covering `[0, total)` as one unowned segment.
    pub fn new(total: u64, min_split: u64) -> Self {
        Self::from_segments(vec![Segment::new(0, total)], min_split)
    }

    pub fn from_segments(segments: Vec<Segment>, min_split: u64) -> Self {
        let slots = segments
            .into_iter()
            .map(|seg| Slot { seg, reserved: 0, owner: None, speed: 0.0 })
            .collect();
        Self { slots, min_split: min_split.max(1), horizon_secs: 2.0 }
    }

    /// Give `worker` something to download: an unowned unfinished segment if
    /// one exists, otherwise a fresh segment carved off the busiest one.
    pub fn claim(&mut self, worker: usize) -> Option<usize> {
        let free = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.owner.is_none() && !s.seg.is_complete())
            .min_by_key(|(_, s)| s.seg.start)
            .map(|(i, _)| i);
        if let Some(i) = free {
            self.slots[i].owner = Some(worker);
            self.slots[i].reserved = 0;
            self.slots[i].speed = 0.0;
            return Some(i);
        }
        self.split_slowest(worker)
    }

    /// Smallest piece worth carving off `s`. Once its owner's speed is known
    /// this is about half a second of that throughput (a new connection
    /// needs a round trip or two before data flows, so splitting off less
    /// gains nothing), kept within [`SPLIT_FLOOR_MIN`, `SPLIT_FLOOR_MAX`].
    /// Until then it is the configured `min_split`. Huge `min_split` values
    /// (splitting disabled, e.g. no range support) are always respected.
    fn split_floor(&self, s: &Slot) -> u64 {
        if s.speed > 0.0 && self.min_split <= SPLIT_FLOOR_MAX {
            ((s.speed * SPLIT_FLOOR_SECS) as u64).clamp(SPLIT_FLOOR_MIN, SPLIT_FLOOR_MAX)
        } else {
            self.min_split
        }
    }

    fn split_slowest(&mut self, worker: usize) -> Option<usize> {
        let speeds: Vec<f64> = self
            .slots
            .iter()
            .filter(|s| s.owner.is_some() && s.speed > 0.0)
            .map(|s| s.speed)
            .collect();
        let avg_speed = if speeds.is_empty() {
            0.0
        } else {
            speeds.iter().sum::<f64>() / speeds.len() as f64
        };

        // The candidate is the segment that will still have the most bytes
        // left after its current owner keeps going for `horizon_secs`.
        let (idx, rem) = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.owner.is_some() && s.seg.end != UNKNOWN_END)
            .map(|(i, s)| {
                let rem = s.unclaimed_remaining();
                let score = rem as f64 - s.speed * self.horizon_secs;
                (i, rem, score)
            })
            .filter(|&(i, rem, _)| rem >= self.split_floor(&self.slots[i]).saturating_mul(2))
            .max_by(|a, b| a.2.total_cmp(&b.2))
            .map(|(i, rem, _)| (i, rem))?;
        let floor = self.split_floor(&self.slots[idx]);

        // Split so both connections are expected to finish together: the
        // current owner keeps a share proportional to its speed.
        let old_speed = self.slots[idx].speed;
        let new_speed = if avg_speed > 0.0 { avg_speed } else { old_speed };
        let keep = if old_speed > 0.0 && new_speed > 0.0 {
            (rem as f64 * old_speed / (old_speed + new_speed)) as u64
        } else {
            rem / 2
        };
        let keep = keep.clamp(floor, rem - floor);

        let split_at = self.slots[idx].cursor() + keep;
        let tail = Segment::new(split_at, self.slots[idx].seg.end);
        self.slots[idx].seg.end = split_at;
        self.slots.push(Slot { seg: tail, reserved: 0, owner: Some(worker), speed: 0.0 });
        Some(self.slots.len() - 1)
    }

    /// Reserve up to `want` bytes at the cursor of `slot` for `worker`.
    /// Returns `(file_offset, allowed)`; `allowed < want` means the segment
    /// ends inside this chunk, and `0` means it is finished or no longer ours.
    pub fn reserve(&mut self, slot: usize, worker: usize, want: u64) -> (u64, u64) {
        let s = &mut self.slots[slot];
        let offset = s.cursor();
        if s.owner != Some(worker) {
            return (offset, 0);
        }
        let allowed = want.min(s.seg.end.saturating_sub(offset));
        s.reserved += allowed;
        (offset, allowed)
    }

    /// Mark `n` previously reserved bytes as written.
    pub fn commit(&mut self, slot: usize, n: u64) {
        let s = &mut self.slots[slot];
        debug_assert!(n <= s.reserved);
        s.reserved -= n;
        s.seg.done += n;
    }

    /// Give a segment back (worker failed or retired). Uncommitted
    /// reservations are discarded so another worker re-downloads them.
    pub fn release(&mut self, slot: usize, worker: usize) {
        let s = &mut self.slots[slot];
        if s.owner == Some(worker) {
            s.owner = None;
            s.reserved = 0;
            s.speed = 0.0;
        }
    }

    /// Like [`release`](Self::release), but a segment that never received a
    /// byte is folded back into the segment that ends where it starts, so
    /// the connection already streaming there simply keeps going instead of
    /// someone opening a new request for it later.
    pub fn give_back(&mut self, slot: usize, worker: usize) {
        self.release(slot, worker);
        let seg = self.slots[slot].seg;
        if seg.done != 0 || seg.is_complete() || self.slots[slot].owner.is_some() {
            return;
        }
        if let Some(prev) = (0..self.slots.len()).find(|&i| i != slot && self.slots[i].seg.end == seg.start) {
            self.slots[prev].seg.end = seg.end;
            self.slots[slot].seg.end = seg.start;
        }
    }

    /// Forget all progress of a segment (used when a non-resumable stream
    /// has to start over from its beginning).
    pub fn reset(&mut self, slot: usize) -> u64 {
        let s = &mut self.slots[slot];
        let lost = s.seg.done + s.reserved;
        s.seg.done = 0;
        s.reserved = 0;
        lost
    }

    /// For streams of unknown length: the server closed the body, so the
    /// segment ends where we are.
    pub fn finish_unknown(&mut self, slot: usize) {
        let s = &mut self.slots[slot];
        s.seg.end = s.cursor();
    }

    /// How many bytes from file offset `pos` on are already committed, i.e.
    /// written by whichever connection owns that region.
    pub fn durable_from(&self, pos: u64) -> u64 {
        self.slots
            .iter()
            .find(|s| s.seg.start <= pos && pos < s.seg.pos())
            .map_or(0, |s| s.seg.pos() - pos)
    }

    /// Number of segments currently owned by workers other than `worker`.
    pub fn other_owners(&self, worker: usize) -> usize {
        self.slots.iter().filter(|s| s.owner.is_some_and(|w| w != worker)).count()
    }

    /// Whether [`claim`](Self::claim) would hand out work right now.
    pub fn can_claim(&self) -> bool {
        self.slots.iter().any(|s| {
            let free = s.owner.is_none() && !s.seg.is_complete();
            let splittable = s.owner.is_some()
                && s.seg.end != UNKNOWN_END
                && s.unclaimed_remaining() >= self.split_floor(s).saturating_mul(2);
            free || splittable
        })
    }

    pub fn set_speed(&mut self, slot: usize, speed: f64) {
        self.slots[slot].speed = speed;
    }

    pub fn owner(&self, slot: usize) -> Option<usize> {
        self.slots[slot].owner
    }

    pub fn segment(&self, slot: usize) -> Segment {
        self.slots[slot].seg
    }

    pub fn is_complete(&self) -> bool {
        self.slots.iter().all(|s| s.seg.is_complete())
    }

    pub fn written(&self) -> u64 {
        self.slots.iter().map(|s| s.seg.done).sum()
    }

    /// Total size implied by the segments (only meaningful once known).
    pub fn extent(&self) -> u64 {
        self.slots.iter().map(|s| s.seg.end).max().unwrap_or(0)
    }

    /// Durable progress, compacted: a completed segment is merged into the
    /// segment that follows it, since their written regions are contiguous.
    pub fn snapshot(&self) -> Vec<Segment> {
        let mut segs: Vec<Segment> = self.slots.iter().map(|s| s.seg).collect();
        segs.sort_by_key(|s| s.start);
        let mut out: Vec<Segment> = Vec::with_capacity(segs.len());
        for seg in segs {
            match out.last_mut() {
                Some(prev) if prev.is_complete() && prev.end == seg.start => {
                    prev.end = seg.end;
                    prev.done += seg.done;
                }
                _ => out.push(seg),
            }
        }
        out
    }

    pub fn view(&self) -> Vec<SegmentView> {
        let mut v: Vec<SegmentView> = self
            .slots
            .iter()
            .map(|s| SegmentView {
                start: s.seg.start,
                end: s.seg.end,
                received: s.seg.done + s.reserved,
                active: s.owner.is_some() && !s.seg.is_complete(),
                speed: s.speed,
            })
            .collect();
        v.sort_by_key(|s| s.start);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1 << 20;

    fn covered(table: &SegmentTable, total: u64) {
        let mut segs = table.slots.iter().map(|s| s.seg).collect::<Vec<_>>();
        segs.sort_by_key(|s| s.start);
        let mut at = 0;
        for s in segs {
            assert_eq!(s.start, at, "gap or overlap in {:?}", table.slots);
            at = s.end;
        }
        assert_eq!(at, total);
    }

    #[test]
    fn workers_partition_evenly() {
        let mut t = SegmentTable::new(64 * MB, MB);
        for w in 0..8 {
            assert!(t.claim(w).is_some());
        }
        covered(&t, 64 * MB);
        let mut sizes: Vec<u64> = t.slots.iter().map(|s| s.seg.end - s.seg.start).collect();
        sizes.sort();
        assert_eq!(sizes, vec![8 * MB; 8]);
    }

    #[test]
    fn small_files_are_not_over_split() {
        let mut t = SegmentTable::new(3 * MB, MB);
        assert!(t.claim(0).is_some());
        assert!(t.claim(1).is_some());
        assert!(t.claim(2).is_none(), "segments below 2*min_split must not split");
    }

    #[test]
    fn reserve_stops_at_segment_end_after_split() {
        let mut t = SegmentTable::new(10 * MB, MB);
        let a = t.claim(0).unwrap();
        let (off, n) = t.reserve(a, 0, MB);
        assert_eq!((off, n), (0, MB));
        t.commit(a, MB);
        let b = t.claim(1).unwrap();
        let split = t.segment(b).start;
        assert!(split > MB);
        let (off, n) = t.reserve(a, 0, 100 * MB);
        assert_eq!(off, MB);
        assert_eq!(off + n, split, "owner must stop exactly where the stolen part starts");
        covered(&t, 10 * MB);
    }

    #[test]
    fn slow_connection_is_split_and_keeps_small_share() {
        let thirds = (0..3).map(|i| Segment::new(i * 30 * MB, (i + 1) * 30 * MB)).collect();
        let mut t = SegmentTable::from_segments(thirds, MB);
        let fast = t.claim(0).unwrap();
        let slow = t.claim(1).unwrap();
        let other = t.claim(2).unwrap();
        t.set_speed(fast, 9.0 * MB as f64);
        t.set_speed(slow, 1.0 * MB as f64);
        t.set_speed(other, 9.0 * MB as f64);
        // Worker 2 finishes its segment and steals.
        let (_, n) = t.reserve(other, 2, 30 * MB);
        t.commit(other, n);
        t.release(other, 2);
        let stolen = t.claim(2).unwrap();
        assert_eq!(t.segment(stolen).end, 60 * MB, "the segment finishing last is split");
        let kept = t.segment(slow).end - t.segment(slow).start;
        let given = t.segment(stolen).end - t.segment(stolen).start;
        assert!(kept < given, "slow owner should keep less ({kept} vs {given})");
        covered(&t, 90 * MB);
    }

    #[test]
    fn released_segments_are_reclaimed_first() {
        let mut t = SegmentTable::new(32 * MB, MB);
        let a = t.claim(0).unwrap();
        let _b = t.claim(1).unwrap();
        t.reserve(a, 0, MB);
        t.commit(a, MB);
        t.reserve(a, 0, MB); // reserved but never written
        t.release(a, 0);
        let again = t.claim(2).unwrap();
        assert_eq!(again, a);
        assert_eq!(t.reserve(again, 2, 1).0, MB, "uncommitted bytes are re-downloaded");
    }

    #[test]
    fn split_floor_follows_connection_speed() {
        const KB: u64 = 1024;
        // Slow connection (200 KB/s): a 600 KB tail is still worth stealing,
        // even though it is below the cold-start minimum of 1 MiB.
        let mut t = SegmentTable::from_segments(vec![Segment::new(0, 600 * KB)], MB);
        let a = t.claim(0).unwrap();
        t.set_speed(a, 200.0 * KB as f64);
        assert!(t.can_claim());
        let b = t.claim(1).unwrap();
        assert!(t.segment(b).end - t.segment(b).start >= 128 * KB);

        // Fast connection (50 MB/s): a 40 MB tail finishes in 0.8 s, so
        // opening another connection for it is pointless.
        let mut t = SegmentTable::from_segments(vec![Segment::new(0, 40 * MB)], MB);
        let a = t.claim(0).unwrap();
        t.set_speed(a, 50.0 * MB as f64);
        assert!(!t.can_claim());
        assert!(t.claim(1).is_none());

        // Splitting disabled (no range support) stays disabled at any speed.
        let mut t = SegmentTable::from_segments(vec![Segment::new(0, 100 * MB)], u64::MAX / 4);
        let a = t.claim(0).unwrap();
        t.set_speed(a, 1.0);
        assert!(t.claim(1).is_none());
    }

    #[test]
    fn untouched_segment_is_folded_back_into_its_predecessor() {
        let mut t = SegmentTable::new(64 * MB, MB);
        let a = t.claim(0).unwrap();
        let b = t.claim(1).unwrap();
        assert_eq!(t.segment(a).end, 32 * MB);
        // Worker 1 was refused by the server before receiving anything.
        t.give_back(b, 1);
        assert_eq!(t.segment(a).end, 64 * MB, "worker 0 keeps streaming the whole file");
        assert!(t.segment(b).is_complete());
        assert!(!t.can_claim() || t.claim(2).is_some_and(|s| t.segment(s).start > 0));
        covered(&t, 64 * MB);

        // A segment with data keeps it and stays claimable.
        let mut t = SegmentTable::new(64 * MB, MB);
        let a = t.claim(0).unwrap();
        let b = t.claim(1).unwrap();
        let (_, n) = t.reserve(b, 1, MB);
        t.commit(b, n);
        t.give_back(b, 1);
        assert_eq!(t.segment(a).end, 32 * MB);
        assert_eq!(t.claim(2), Some(b));
    }

    #[test]
    fn snapshot_merges_completed_prefixes() {
        let segs = vec![
            Segment { start: 0, end: 10, done: 10 },
            Segment { start: 10, end: 20, done: 4 },
            Segment { start: 20, end: 30, done: 10 },
            Segment { start: 30, end: 40, done: 10 },
        ];
        let t = SegmentTable::from_segments(segs, 1);
        assert_eq!(
            t.snapshot(),
            vec![
                Segment { start: 0, end: 20, done: 14 },
                Segment { start: 20, end: 40, done: 20 },
            ]
        );
    }

    #[test]
    fn durable_bytes_after_an_offset() {
        let segs = vec![Segment { start: 0, end: 10, done: 4 }, Segment { start: 10, end: 20, done: 6 }];
        let t = SegmentTable::from_segments(segs, 1);
        assert_eq!(t.durable_from(0), 4);
        assert_eq!(t.durable_from(3), 1);
        assert_eq!(t.durable_from(4), 0, "uncommitted");
        assert_eq!(t.durable_from(10), 6);
        assert_eq!(t.durable_from(16), 0);
        assert_eq!(t.durable_from(99), 0);
    }

    #[test]
    fn unknown_length_never_splits() {
        let mut t = SegmentTable::new(UNKNOWN_END, MB);
        let a = t.claim(0).unwrap();
        assert!(t.claim(1).is_none());
        let (_, n) = t.reserve(a, 0, 5 * MB);
        t.commit(a, n);
        t.finish_unknown(a);
        assert!(t.is_complete());
        assert_eq!(t.extent(), 5 * MB);
    }
}
