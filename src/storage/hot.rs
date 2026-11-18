//! The hot set: cache frames kept on purpose, for pages that are used
//! *often*, which is not the same as recently.
//!
//! The page cache's CLOCK keeps what was used recently, so a scan, another
//! database wanting memory, or a reopen sweeps away the pages a workload
//! comes back to all day. The hot set is a bounded part of the same cache,
//! at most `hot_size` bytes of its frames and never more than half of them,
//! whose pages the sweep passes over. Which pages belong there is decided
//! by *heat*:
//!
//! * **Time is lookups.** A database's clock is its own cache lookups (every
//!   page read through the cache, hit or miss), counted in *ticks* of
//!   `tick_len` lookups: as many as the cache has frames, and at least
//!   [`MIN_TICK`]. Lookup `n` (from 0, since the hot set was turned on) is
//!   in tick `n / tick_len`. An idle database's clock stands still, so its
//!   hot set does not cool while it waits.
//! * **Heat counts ticks, not lookups.** A page earns one heat in each tick
//!   it is read in, however many times it is read within it, up to
//!   [`MAX_HEAT`]. A pass over data reads each page in one stretch, and a
//!   stretch of at most `2 * tick_len + 1` lookups falls in at most three
//!   ticks: it earns at most three, one short of hot. A tree's root and
//!   upper levels are read in every tick of a workload that uses the tree,
//!   so they become hot, as they should.
//! * **Heat halves every [`HALF`] ticks**, every page's at once, rounding
//!   down: when the clock enters a tick that is a multiple of [`HALF`].
//! * **A page is hot from [`PROMOTE`] heat.** When it reaches it, it joins
//!   the hot set if there is room, or takes the place of the coolest hot
//!   page if that page now has less heat than it. A hot page leaves when its
//!   heat has halved to nothing, when a hotter page takes its place, when
//!   the page is freed, or when the process's budget takes its frame. When
//!   copy-on-write gives a page's data a new number, the copy takes the
//!   page's heat and its place.
//!
//! So, used from a cold start in ticks that are not a halving apart, a page
//! is hot after its fourth tick; one hot at the most heat, left alone, is
//! gone after five halvings (`5 * HALF` ticks), one just promoted after
//! three.
//!
//! **Constant work.** Hot pages sit in one ring per heat level. The coolest
//! is the first page of the lowest ring that is not empty, and halving
//! every heat relabels the rings (rings `2h` and `2h + 1` become ring `h`):
//! [`RINGS`] list splices per [`HALF`] ticks, however large the set. A
//! lookup advances the clock and, the first time it reads a page in a
//! tick, computes the page's heat and moves it between rings; a promotion
//! adds a look at no more than [`RINGS`] ring heads and one demotion. Each
//! lookup also retires at most one page whose heat has run out. Nothing
//! walks the cache or the set, except a cache with no cold frame left to
//! evict looking for the coolest hot frame nobody holds.
//!
//! This module only does the bookkeeping, by frame position: the pager
//! calls in whenever a frame is read, added, emptied, removed or moved, and
//! keeps one [`Heat`] here per cache frame, in the cache's frame order.

/// Heat at which a page becomes hot.
pub const PROMOTE: u8 = 4;
/// Heat stops counting here.
pub const MAX_HEAT: u8 = 31;
/// Ticks between halvings of every heat.
pub const HALF: u64 = 16;
/// The fewest lookups in a tick, whatever the size of the cache.
pub const MIN_TICK: u64 = 1024;
/// One ring per heat level.
pub const RINGS: usize = MAX_HEAT as usize + 1;

/// No slot: the frame is not hot.
const COLD: u32 = u32::MAX;
/// A frame not read since it last changed page.
const NEVER: u64 = u64::MAX;

/// What the hot set knows about one cache frame.
#[derive(Clone, Copy, Debug)]
struct Heat {
    /// Ticks the page was read in, halved every [`HALF`] ticks, as of the
    /// tick in `seen` (see [`Hot::level`]).
    heat: u8,
    /// The tick of the last read counted, or [`NEVER`].
    seen: u64,
    /// The slot holding this frame in the hot set, or [`COLD`].
    slot: u32,
}

impl Heat {
    const NONE: Heat = Heat {
        heat: 0,
        seen: NEVER,
        slot: COLD,
    };
}

/// One cache's hot set.
pub(crate) struct Hot {
    /// Most frames the set may hold.
    cap: usize,
    /// Frames it holds.
    len: usize,
    /// Lookups per tick, and lookups left in the current one.
    tick_len: u64,
    left: u64,
    tick: u64,
    /// One per cache frame, in frame order.
    heat: Vec<Heat>,
    /// Slots `0..RINGS` are the heads of the rings, one per heat level;
    /// slots from `RINGS` hold hot frames. Rings are circular, doubly
    /// linked through `prev` and `next`; `frame` maps a slot to its frame.
    frame: Vec<u32>,
    prev: Vec<u32>,
    next: Vec<u32>,
    /// Slots holding nothing.
    free: Vec<u32>,
    /// Lookups that found their page hot.
    pub hits: u64,
    pub promotions: u64,
    /// Pages that left the set, for any reason but a copy taking their place.
    pub demotions: u64,
    /// Pages made hot at open from a remembered list.
    pub prewarmed: u64,
}

impl Hot {
    /// A hot set of at most `cap` frames for a cache of `cache_frames`
    /// frames, `frames` of which exist now.
    pub(crate) fn new(cap: usize, cache_frames: usize, frames: usize) -> Hot {
        let tick_len = (cache_frames as u64).max(MIN_TICK);
        let mut h = Hot {
            cap: 0,
            len: 0,
            tick_len,
            left: tick_len,
            tick: 0,
            heat: vec![Heat::NONE; frames],
            frame: Vec::new(),
            prev: Vec::new(),
            next: Vec::new(),
            free: Vec::new(),
            hits: 0,
            promotions: 0,
            demotions: 0,
            prewarmed: 0,
        };
        h.slots(cap);
        h
    }

    /// Empty rings with room for `cap` frames.
    fn slots(&mut self, cap: usize) {
        let n = RINGS + cap;
        self.cap = cap;
        self.len = 0;
        self.frame = vec![COLD; n];
        self.prev = (0..n as u32).collect();
        self.next = (0..n as u32).collect();
        self.free = (RINGS as u32..n as u32).rev().collect();
    }

    pub(crate) fn capacity(&self) -> usize {
        self.cap
    }

    /// Ticks of `lookups` lookups from the next lookup on. A short clock
    /// makes heat come and go within a few dozen reads: for tests that want
    /// the set filling, displacing and cooling all the time.
    pub(crate) fn set_tick(&mut self, lookups: u64) {
        self.tick_len = lookups.max(1);
        self.left = self.tick_len;
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    pub(crate) fn is_hot(&self, i: usize) -> bool {
        self.heat.get(i).is_some_and(|h| h.slot != COLD)
    }

    /// A frame's heat now: its stored heat, halved once for every halving
    /// since the tick it was stored in.
    fn level(&self, h: Heat) -> u8 {
        if h.seen == NEVER {
            return 0;
        }
        let halvings = self.tick / HALF - h.seen / HALF;
        if halvings >= 8 {
            0
        } else {
            h.heat >> halvings
        }
    }

    /// A lookup of the page in frame `i`, hit or miss.
    pub(crate) fn touch(&mut self, i: usize) {
        if self.cap == 0 {
            return;
        }
        let Some(h) = self.heat.get(i).copied() else {
            return;
        };
        if h.slot != COLD {
            self.hits += 1;
        }
        if h.seen != self.tick {
            let heat = self.level(h).saturating_add(1).min(MAX_HEAT);
            self.heat[i].heat = heat;
            self.heat[i].seen = self.tick;
            if h.slot != COLD {
                self.unlink(h.slot);
                self.link(heat as usize, h.slot);
            } else if heat >= PROMOTE {
                self.promote(i, heat);
            }
        }
        self.left -= 1;
        if self.left == 0 {
            self.left = self.tick_len;
            self.tick += 1;
            if self.tick % HALF == 0 {
                self.halve();
            }
        }
        // A page whose heat has run out gives up its place, one per lookup.
        if !self.empty(0) {
            let f = self.frame[self.next[0] as usize] as usize;
            self.demote(f);
        }
    }

    /// Put frame `i`, which has `heat`, in the set: in a free slot, or in
    /// place of the coolest hot page if it is cooler.
    fn promote(&mut self, i: usize, heat: u8) {
        if self.len >= self.cap {
            let Some(ring) = (0..heat as usize).find(|&r| !self.empty(r)) else {
                return;
            };
            let f = self.frame[self.next[ring] as usize] as usize;
            self.demote(f);
        }
        let Some(s) = self.free.pop() else {
            return;
        };
        self.frame[s as usize] = i as u32;
        self.heat[i].slot = s;
        self.link(heat as usize, s);
        self.len += 1;
        self.promotions += 1;
    }

    /// Frame `i` leaves the set; it keeps its heat.
    pub(crate) fn demote(&mut self, i: usize) {
        let Some(s) = self.heat.get(i).map(|h| h.slot) else {
            return;
        };
        if s == COLD {
            return;
        }
        self.unlink(s);
        self.frame[s as usize] = COLD;
        self.heat[i].slot = COLD;
        self.free.push(s);
        self.len -= 1;
        self.demotions += 1;
    }

    /// Frame `i` no longer holds the page it did (freed, or the frame given
    /// to another page): no heat, not hot.
    pub(crate) fn forget(&mut self, i: usize) {
        self.demote(i);
        if let Some(h) = self.heat.get_mut(i) {
            *h = Heat::NONE;
        }
    }

    /// A frame was added after the last.
    pub(crate) fn push(&mut self) {
        self.heat.push(Heat::NONE);
    }

    /// Frame `i` was removed and the last frame moved into its place.
    pub(crate) fn swap_remove(&mut self, i: usize) {
        if i >= self.heat.len() {
            return;
        }
        self.demote(i);
        self.heat.swap_remove(i);
        if let Some(moved) = self.heat.get(i) {
            if moved.slot != COLD {
                self.frame[moved.slot as usize] = i as u32;
            }
        }
    }

    /// Every frame is gone.
    pub(crate) fn clear(&mut self) {
        self.demotions += self.len as u64;
        self.heat.clear();
        self.slots(self.cap);
    }

    /// Copy-on-write put the data of frame `from` in frame `to`: the copy
    /// takes its heat and, if it is hot, its place. The original keeps its
    /// heat, cold, for as long as it is still a page: a rollback makes it
    /// the data again.
    pub(crate) fn inherit(&mut self, from: usize, to: usize) {
        if from == to || from >= self.heat.len() || to >= self.heat.len() {
            return;
        }
        self.forget(to);
        let h = self.heat[from];
        self.heat[to] = h;
        if h.slot != COLD {
            self.frame[h.slot as usize] = to as u32;
            self.heat[from].slot = COLD;
        }
    }

    /// Make frame `i` hot now, with at least [`PROMOTE`] heat: for a hot set
    /// remembered from before. False if there is no room among hotter pages.
    pub(crate) fn mark(&mut self, i: usize) -> bool {
        let Some(h) = self.heat.get(i).copied() else {
            return false;
        };
        if self.cap == 0 {
            return false;
        }
        if h.slot != COLD {
            return true;
        }
        let heat = self.level(h).max(PROMOTE);
        self.heat[i].heat = heat;
        self.heat[i].seen = self.tick;
        self.promote(i, heat);
        self.heat[i].slot != COLD
    }

    /// The coolest hot frame that `ok` accepts.
    pub(crate) fn coolest(&self, ok: impl Fn(usize) -> bool) -> Option<usize> {
        for ring in 0..RINGS {
            let mut s = self.next[ring];
            while s as usize != ring {
                let f = self.frame[s as usize] as usize;
                if ok(f) {
                    return Some(f);
                }
                s = self.next[s as usize];
            }
        }
        None
    }

    /// Hot frames, hottest first (and, within one heat, most recently read
    /// first).
    pub(crate) fn frames(&self) -> Vec<usize> {
        let mut out = Vec::with_capacity(self.len);
        for ring in (0..RINGS).rev() {
            let mut s = self.prev[ring];
            while s as usize != ring {
                out.push(self.frame[s as usize] as usize);
                s = self.prev[s as usize];
            }
        }
        out
    }

    /// Room for `cap` frames; past it, the coolest leave.
    pub(crate) fn resize(&mut self, cap: usize) {
        if cap == self.cap {
            return;
        }
        let hot = self.frames();
        self.slots(cap);
        for (n, f) in hot.into_iter().enumerate() {
            self.heat[f].slot = COLD;
            if n >= cap {
                self.demotions += 1;
                continue;
            }
            let heat = self.level(self.heat[f]);
            if let Some(s) = self.free.pop() {
                self.frame[s as usize] = f as u32;
                self.heat[f].slot = s;
                self.link(heat as usize, s);
                self.len += 1;
            }
        }
    }

    /// Every heat halves: rings `2h` and `2h + 1` become ring `h`. Ring `h`
    /// is empty when it is filled, its own pages having gone to ring `h / 2`
    /// a step earlier; ring 0 keeps its own.
    fn halve(&mut self) {
        for h in 0..RINGS / 2 {
            if h > 0 {
                self.splice(h, 2 * h);
            }
            self.splice(h, 2 * h + 1);
        }
    }

    fn empty(&self, ring: usize) -> bool {
        self.next[ring] as usize == ring
    }

    /// Append slot `s` to ring `ring`.
    fn link(&mut self, ring: usize, s: u32) {
        let tail = self.prev[ring];
        self.prev[s as usize] = tail;
        self.next[s as usize] = ring as u32;
        self.next[tail as usize] = s;
        self.prev[ring] = s;
    }

    fn unlink(&mut self, s: u32) {
        let (p, n) = (self.prev[s as usize], self.next[s as usize]);
        self.next[p as usize] = n;
        self.prev[n as usize] = p;
    }

    /// Move every slot of ring `src` to the end of ring `dst`.
    fn splice(&mut self, dst: usize, src: usize) {
        if self.empty(src) {
            return;
        }
        let (first, last) = (self.next[src], self.prev[src]);
        let tail = self.prev[dst];
        self.next[tail as usize] = first;
        self.prev[first as usize] = tail;
        self.next[last as usize] = dst as u32;
        self.prev[dst] = last;
        self.next[src] = src as u32;
        self.prev[src] = src as u32;
    }

    /// The bookkeeping agrees with itself and with a cache of `frames`
    /// frames: every hot frame in exactly one ring, the ring of its heat.
    pub(crate) fn check(&self, frames: usize) -> Result<(), String> {
        if self.heat.len() != frames {
            return Err(format!(
                "{} heat records for {frames} frames",
                self.heat.len()
            ));
        }
        if self.len > self.cap {
            return Err(format!("{} hot frames, room for {}", self.len, self.cap));
        }
        let mut seen = 0usize;
        for ring in 0..RINGS {
            let mut s = self.next[ring];
            let mut back = ring as u32;
            while s as usize != ring {
                if (s as usize) < RINGS || s as usize >= self.frame.len() {
                    return Err(format!("ring {ring} holds slot {s}"));
                }
                if self.prev[s as usize] != back {
                    return Err(format!("ring {ring}: slot {s} is linked one way only"));
                }
                let f = self.frame[s as usize] as usize;
                let Some(h) = self.heat.get(f) else {
                    return Err(format!("slot {s} holds frame {f} of {frames}"));
                };
                if h.slot != s {
                    return Err(format!("frame {f} is in slot {s} but names {}", h.slot));
                }
                if self.level(*h) as usize != ring {
                    return Err(format!(
                        "frame {f} has heat {} but is in ring {ring}",
                        self.level(*h)
                    ));
                }
                seen += 1;
                if seen > self.len {
                    return Err("the rings hold more frames than the set".into());
                }
                back = s;
                s = self.next[s as usize];
            }
        }
        let marked = self.heat.iter().filter(|h| h.slot != COLD).count();
        if seen != self.len || marked != self.len {
            return Err(format!(
                "{} hot by count, {seen} in rings, {marked} marked",
                self.len
            ));
        }
        if self.free.len() + self.len != self.cap {
            return Err(format!(
                "{} free slots and {} used of {}",
                self.free.len(),
                self.len,
                self.cap
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lookups per tick for a small cache.
    const T: u64 = MIN_TICK;
    /// Frames from 40 are filler, read only to move the clock on: each tick
    /// reads a different one, and the same one only every 20 ticks, more
    /// than a halving apart, so filler never has more than one heat.
    const FILL: usize = 40;
    const FRAMES: usize = FILL + 20;

    fn hot(cap: usize) -> Hot {
        Hot::new(cap, 64, FRAMES)
    }

    /// Lookups so far.
    fn lookups(h: &Hot) -> u64 {
        h.tick * h.tick_len + (h.tick_len - h.left)
    }

    /// Read filler until the next lookup is lookup `n`.
    fn until(h: &mut Hot, n: u64) {
        while lookups(h) < n {
            h.touch(FILL + (h.tick % 20) as usize);
        }
    }

    /// Read frame `f` as lookup `tick * T + k`.
    fn read_at(h: &mut Hot, f: usize, tick: u64, k: u64) {
        until(h, tick * T + k);
        h.touch(f);
    }

    fn heat(h: &Hot, f: usize) -> u8 {
        h.level(h.heat[f])
    }

    #[test]
    fn a_page_is_hot_after_its_fourth_tick() {
        let mut h = hot(4);
        for tick in 0..3 {
            read_at(&mut h, 0, tick, 0);
            assert!(!h.is_hot(0), "hot after {} ticks", tick + 1);
        }
        read_at(&mut h, 0, 3, 0);
        assert!(h.is_hot(0), "four ticks make a page hot");
        assert_eq!(h.promotions, 1, "filler is never hot");
        h.check(FRAMES).unwrap();
    }

    #[test]
    fn reads_within_one_tick_count_once() {
        let mut h = hot(4);
        for tick in 0..3 {
            until(&mut h, tick * T);
            for _ in 0..T / 2 {
                h.touch(0);
            }
        }
        assert_eq!(heat(&h, 0), 3, "1536 reads in three ticks");
        assert!(!h.is_hot(0));
    }

    /// The edge of the scan rule: a stretch of `2T + 1` lookups of one page,
    /// started at the worst moment (the last lookup of a tick), falls in
    /// three ticks and does not make it hot; one lookup more reaches a
    /// fourth tick and does.
    #[test]
    fn one_stretch_of_reads_is_not_hot_up_to_two_ticks_and_a_lookup() {
        for (stretch, hot_after) in [(2 * T + 1, false), (2 * T + 2, true)] {
            let mut h = hot(4);
            until(&mut h, T - 1);
            for _ in 0..stretch {
                h.touch(0);
            }
            assert_eq!(h.is_hot(0), hot_after, "a stretch of {stretch} lookups");
            h.check(FRAMES).unwrap();
        }
    }

    #[test]
    fn heat_halves_as_every_sixteenth_tick_begins() {
        // Read in ticks 13, 14 and 15: heat 3, halved to 1 as tick 16
        // begins; ticks 16 and 17 make 3, tick 18 makes it hot.
        let mut h = hot(4);
        for tick in [13, 14, 15] {
            read_at(&mut h, 0, tick, 0);
        }
        assert_eq!(heat(&h, 0), 3);
        until(&mut h, 16 * T - 1);
        assert_eq!(heat(&h, 0), 3, "not halved yet");
        until(&mut h, 16 * T);
        assert_eq!(heat(&h, 0), 1, "halved as tick 16 began");
        for tick in [16, 17] {
            read_at(&mut h, 0, tick, 0);
            assert!(!h.is_hot(0));
        }
        assert_eq!(heat(&h, 0), 3);
        read_at(&mut h, 0, 18, 0);
        assert!(h.is_hot(0));
        // The same four ticks with no halving between them: hot at the
        // fourth.
        let mut h = hot(4);
        for tick in [12, 13, 14, 15] {
            read_at(&mut h, 0, tick, 0);
        }
        assert!(h.is_hot(0));
    }

    /// A page made hot and then left alone halves to 2, 1 and 0 as ticks
    /// 16, 32 and 48 begin, and leaves as it reaches 0: hot through the
    /// second to last lookup of tick 47, gone after the last. Nothing else
    /// wanted its place.
    #[test]
    fn a_hot_page_left_alone_cools_out_without_pressure() {
        let mut h = hot(4);
        for tick in 0..4 {
            read_at(&mut h, 0, tick, 0);
        }
        assert!(h.is_hot(0));
        until(&mut h, 32 * T);
        assert_eq!(heat(&h, 0), 1);
        until(&mut h, 48 * T - 1);
        assert!(h.is_hot(0), "still hot before the third halving");
        until(&mut h, 48 * T);
        assert!(!h.is_hot(0), "cooled out at the third halving");
        assert_eq!((h.promotions, h.demotions, h.len()), (1, 1, 0));
        h.check(FRAMES).unwrap();
    }

    /// Full: a page hotter than the coolest hot page takes its place, one
    /// only as hot does not.
    #[test]
    fn a_hotter_page_displaces_the_coolest_and_an_equal_one_does_not() {
        let mut h = hot(2);
        // Frame 0 read in ticks 0 to 4 (heat 5), frame 1 in 1 to 4 (heat 4).
        for tick in 0..5 {
            read_at(&mut h, 0, tick, 0);
            if tick >= 1 {
                read_at(&mut h, 1, tick, 1);
            }
        }
        assert_eq!(h.frames(), vec![0, 1], "hottest first");
        // Frame 2 reaches heat 4, as hot as frame 1: it stays out.
        for tick in 5..9 {
            read_at(&mut h, 2, tick, 0);
        }
        assert_eq!(heat(&h, 2), 4);
        assert_eq!(h.frames(), vec![0, 1]);
        // At heat 5 it is hotter than frame 1, which gives way.
        read_at(&mut h, 2, 9, 0);
        assert_eq!(h.frames(), vec![2, 0]);
        assert_eq!((h.promotions, h.demotions), (3, 1));
        h.check(FRAMES).unwrap();
    }

    #[test]
    fn the_page_displaced_is_the_coolest() {
        let mut h = hot(3);
        // Frames 0, 1 and 2 read from ticks 0, 2 and 1 to 5: heat 6, 4, 5.
        for tick in 0..6 {
            for (f, first) in [(0, 0), (1, 2), (2, 1)] {
                if tick >= first {
                    read_at(&mut h, f, tick, 1 + f as u64);
                }
            }
        }
        assert_eq!(h.frames(), vec![0, 2, 1]);
        assert_eq!(h.coolest(|_| true), Some(1));
        assert_eq!(h.coolest(|f| f != 1), Some(2));
        // Frame 3: heat 4 is not enough, heat 5 takes frame 1's place.
        for tick in 6..10 {
            read_at(&mut h, 3, tick, 0);
        }
        assert!(!h.is_hot(3));
        read_at(&mut h, 3, 10, 0);
        assert!(h.is_hot(3) && !h.is_hot(1) && h.is_hot(0) && h.is_hot(2));
        h.check(FRAMES).unwrap();
    }

    /// A copy takes the original's heat and place; the original keeps its
    /// heat, cold. A freed page loses both.
    #[test]
    fn a_copy_inherits_and_a_freed_page_forgets() {
        let mut h = hot(4);
        for tick in 0..5 {
            read_at(&mut h, 0, tick, 0);
        }
        h.inherit(0, 1);
        assert!(h.is_hot(1) && !h.is_hot(0));
        assert_eq!((heat(&h, 0), heat(&h, 1)), (5, 5));
        assert_eq!((h.promotions, h.demotions), (1, 0), "a copy is not a move");
        h.forget(1);
        assert!(!h.is_hot(1));
        assert_eq!(heat(&h, 1), 0);
        assert_eq!(h.demotions, 1);
        // The original comes back as soon as it is read again.
        read_at(&mut h, 0, 5, 0);
        assert!(h.is_hot(0));
        h.check(FRAMES).unwrap();
    }

    /// Every change of frames keeps the rings, the slots and the heat
    /// records agreeing, through random reads, frames added, removed,
    /// copied, freed, marked, a resized set and a cleared cache.
    #[test]
    fn bookkeeping_survives_every_frame_change() {
        let mut r = 0x2545_F491_4F6C_DD1Du64;
        let mut rand = move |n: u64| {
            r ^= r << 13;
            r ^= r >> 7;
            r ^= r << 17;
            r % n.max(1)
        };
        let mut h = Hot::new(6, 16, 0);
        let mut frames = 0usize;
        for step in 0..100_000u32 {
            let (op, a, b, c) = (rand(100), rand(16), rand(16), rand(500));
            let (a, b) = (a as usize, b as usize);
            match op {
                0..=2 if frames < 16 => {
                    h.push();
                    frames += 1;
                }
                3 if a < frames => {
                    h.swap_remove(a);
                    frames -= 1;
                }
                4 if a < frames && b < frames => h.inherit(a, b),
                5 if a < frames => h.forget(a),
                6 if a < frames => {
                    h.mark(a);
                }
                7 if c == 0 => {
                    h.clear();
                    frames = 0;
                }
                8 if c < 3 => h.resize(b / 2),
                _ if frames > 0 => {
                    // Skewed reads: three frames often, the rest seldom.
                    let f = if c % 4 == 0 {
                        a % frames
                    } else {
                        a % 3.min(frames)
                    };
                    for _ in 0..c % 64 {
                        h.touch(f);
                    }
                }
                _ => {}
            }
            if step % 97 == 0 {
                h.check(frames)
                    .unwrap_or_else(|e| panic!("step {step}: {e}"));
            }
            assert!(h.len() <= h.capacity());
        }
        h.check(frames).unwrap();
        assert!(h.promotions > 100 && h.demotions > 100);
    }
}
