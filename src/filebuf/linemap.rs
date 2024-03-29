use std::collections::VecDeque;

use crate::prelude::*;

use super::{CharLayout, FilePos, FileRect, LoadedData, LoadedDataGuard, Surroundings};

/// There are two diferent "coordinate systems" in a text file:
/// - Raw byte offset
/// - Normalized screen position (screen coordinates divided by font height)
///
/// Because the editor supports scrolling to the end of huge files, it must support a
/// mixed coordinate system. Instead of specifying an absolute line number and X
/// position, we specify a reference byte offset, and use line numbers relative to
/// that offset.
///
/// In order to support this mixed system, we need a system that keeps track of
/// positional reference data in a certain byte offset range.
///
/// Note that this type implements a merged-segment-list just like `SparseData`, but
/// the line map of a file and the sparse data of a file can cover completely different,
/// partially overlapping ranges.
pub struct LineMap {
    /// A linear list of segments.
    /// This list should be kept short!
    /// In particular, it must be defragmented on insert, and
    /// should be cleaned up if it becomes too long.
    ///
    /// TODO: Set an upper limit on the amount of linemap segments before
    /// dropping small/old segments.
    pub(super) segments: Vec<MappedSegment>,
    /// If set to another value, it should only increase!
    pub(super) file_size: i64,
}
impl LineMap {
    pub fn new() -> Self {
        Self {
            segments: default(),
            file_size: 0,
        }
    }

    /// Find the first segment that ends at or after the given offset.
    /// Returns the amount of segments if there is no segment after the given offset.
    fn find_after(&self, offset: i64) -> usize {
        for (i, s) in self.segments.iter().enumerate() {
            if s.end >= offset {
                return i;
            }
        }
        self.segments.len()
    }

    /// Find the last segment that starts at or before the given offset.
    /// Returns the amount of segments if there is no segment before the given offset.
    fn find_before(&self, offset: i64) -> usize {
        for (i, s) in self.segments.iter().enumerate().rev() {
            if s.start <= offset {
                return i;
            }
        }
        self.segments.len()
    }

    /// Find the segment that contains the given offset, if any.
    fn find_segment(&self, offset: i64) -> Option<&MappedSegment> {
        self.segments
            .get(self.find_after(offset))
            .filter(|s| s.start <= offset)
    }

    /// If the given offset is contained in a segment, yield its left and right edges.
    /// If it's not, yield the inner edges of the surrounding segments.
    /// If there is no segment to a given side, yield the start/end of the file.
    pub fn find_surroundings(&self, offset: i64) -> Surroundings {
        let offset = offset.min(self.file_size - 1);
        for (i, s) in self.segments.iter().enumerate() {
            if s.end > offset {
                if s.start <= offset {
                    // Offset is contained in this segment
                    return Surroundings::In(s.start, s.end);
                } else {
                    // This segment is the first segment after the given offset
                    let prev = match i {
                        0 => 0,
                        i => self.segments[i - 1].end,
                    };
                    return Surroundings::Out(prev, s.start);
                }
            }
        }
        let prev = self.segments.last().map(|s| s.end).unwrap_or(0);
        Surroundings::Out(prev, self.file_size)
    }

    /// Maps the given screen file position to an absolute offset that is at or before
    /// the given position.
    /// Returns a base anchor and the nearest anchor before the position.
    /// May return `None` if the base offset is not loaded or in other edge cases.
    pub fn pos_to_anchor(&self, base_offset: i64, dy: i64, dx: f64) -> Option<(Anchor, Anchor)> {
        let (s, base) = self.offset_to_base(base_offset)?;
        let is_x_abs = s.is_x_absolute(base);
        if !is_x_abs && dy != 0 {
            // When we use a non-absolute base, it means we haven't loaded before
            // the start of the current line.
            // Additionally, we don't know the relationship between the X coordinates
            // of following lines and the base line, therefore if we draw the following
            // lines it would involve a large amount of dizzy moving text
            return None;
        }
        let y = base.y(s) + dy;
        let x = base.x_with(s.base_x_relative, is_x_abs) + dx;
        Some((base, s.locate_lower(y, x)))
    }

    /// Maps the given screen file position to an absolute offset that is at or after
    /// the given position.
    /// Returns a base anchor and the nearest anchor after the position.
    /// May return `None` if the base offset is not loaded or in other edge cases.
    pub fn pos_to_anchor_upper(
        &self,
        base_offset: i64,
        dy: i64,
        dx: f64,
    ) -> Option<(Anchor, Anchor)> {
        let (s, base) = self.offset_to_base(base_offset)?;
        let is_x_abs = s.is_x_absolute(base);
        if !is_x_abs && dy != 0 {
            // When we use a non-absolute base, it means we haven't loaded before
            // the start of the current line.
            // Additionally, we don't know the relationship between the X coordinates
            // of following lines and the base line, therefore if we draw the following
            // lines it would involve a large amount of dizzy moving text
            return None;
        }
        let y = base.y(s) + dy;
        let x = base.x_with(s.base_x_relative, is_x_abs) + dx;
        Some((base, s.locate_upper(y, x)))
    }

    /// If the two offsets are comparable (that is, they reside in the same segment
    /// and are both x-absolute or x-relative), return anchors for both offsets.
    pub fn offset_to_anchor(&self, base_offset: i64, offset: i64) -> Option<(Anchor, Anchor)> {
        let (s1, base) = self.offset_to_base(base_offset)?;
        let (s2, lo) = self.offset_to_base(offset)?;
        if s1 as *const MappedSegment != s2 as *const MappedSegment {
            // The offsets do not reside in the same segment
            return None;
        }
        if s1.is_x_absolute(base) != s1.is_x_absolute(lo) {
            // The offsets are in different absoluteness contexts
            return None;
        }
        Some((base, lo))
    }

    fn offset_to_base(&self, base_offset: i64) -> Option<(&MappedSegment, Anchor)> {
        self.find_segment(base_offset)
            .and_then(|s| s.find_lower(base_offset).map(|a| (s, a)))
    }

    /// Get the bounding rectangle of the loaded area around a given offset.
    pub fn bounding_rect(&self, around_offset: i64) -> FileRect {
        match self.offset_to_base(around_offset) {
            Some((s, base)) => {
                // Confine to the limits of loaded data
                if s.is_x_absolute(base) {
                    let lo = s.anchors[s.first_absolute];
                    let hi = s.anchors.back().unwrap();
                    FileRect {
                        corner: FilePos {
                            base_offset: around_offset,
                            delta_x: -base.x_abs(),
                            delta_y: (lo.y_offset - base.y_offset) as f64,
                        },
                        size: dvec2(s.widest_line, (hi.y_offset - lo.y_offset) as f64),
                    }
                } else {
                    // NOTE: This clamps rendering to the Y of the relative-x line
                    // Rendering mixed relative and absolute lines in the same screen
                    // is kind of hard and messy
                    let lo = s.anchors.front().unwrap();
                    let hi = s
                        .anchors
                        .get(s.first_absolute)
                        .unwrap_or(s.anchors.back().unwrap());
                    FileRect {
                        corner: FilePos {
                            base_offset: around_offset,
                            delta_x: lo.x_offset - base.x_offset,
                            delta_y: 0.,
                        },
                        size: dvec2(s.rel_width, (hi.y_offset - lo.y_offset) as f64),
                    }
                }
            }
            None => {
                // Cannot scroll if the data is not yet loaded
                FileRect {
                    corner: FilePos {
                        base_offset: around_offset,
                        delta_x: 0.,
                        delta_y: 0.,
                    },
                    size: DVec2::ZERO,
                }
            }
        }
    }

    /// Dump the linemap data for debugging.
    pub(super) fn dump_anchors(&self) {
        eprintln!("dumping anchors...");
        println!("{} segments", self.segments.len());
        for seg in self.segments.iter() {
            println!("segment [{}, {})", seg.start, seg.end);
            println!("  base_x: {}, base_y: {}", seg.base_x_relative, seg.base_y);
            println!(
                "  rel_width: {}, widest_line: {}",
                seg.rel_width, seg.widest_line
            );
            println!(
                "  first_absolute: {} ({:?})",
                seg.first_absolute,
                seg.anchors.get(seg.first_absolute)
            );
            println!("  {} anchors:", seg.anchors.len());
            for anchor in seg.anchors.iter() {
                println!(
                    "    off: {}, dy: {} ({}), dx: {} ({})",
                    anchor.offset,
                    anchor.y(seg),
                    anchor.y_offset,
                    anchor.x(seg),
                    anchor.x_offset,
                );
            }
        }
        eprintln!("dumped anchors...");
    }
}
impl fmt::Debug for LineMap {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&self.segments, f)
    }
}

pub type LineMapHandle<'a> = &'a Mutex<LoadedData>;
macro_rules! lock_linemap {
    ($handle:expr, $ref:ident) => {
        let mut $ref = LoadedDataGuard::lock($handle, file!(), line!());
        #[allow(unused_mut)]
        let mut $ref = &mut $ref.guard.linemap;
    };
    ($handle:expr, $lock:ident, $ref:ident) => {
        let mut $lock = LoadedDataGuard::lock($handle, file!(), line!());
        #[allow(unused_mut)]
        let mut $ref = &mut $lock.guard.linemap;
    };
    ($handle:expr, $lock:ident, $ref:ident => unlocked $code:block) => {{
        drop($ref);
        drop($lock);
        $code
        $lock = LoadedDataGuard::lock($handle);
        $ref = &mut $lock.guard.linemap;
    }};
    ($handle:expr, $lock:ident, $ref:ident => bump) => {{
        drop($ref);
        $lock.bump(file!(), line!());
        $ref = &mut $lock.guard.linemap;
    }};
}

pub struct LineMapper {
    pub(super) bytes_per_anchor: usize,
    pub(super) migrate_batch_size: usize,
    pub(super) layout: CharLayout,
}
impl LineMapper {
    pub const REPLACEMENT_CHAR: u32 = char::REPLACEMENT_CHARACTER as u32;
    pub const NEWLINE: u32 = '\n' as u32;

    pub fn new(
        layout: CharLayout,
        file_size: i64,
        max_memory: usize,
        migrate_batch_size: usize,
    ) -> Self {
        let max_anchors = max_memory / mem::size_of::<Anchor>();
        let bytes_per_anchor = usize::try_from(file_size / max_anchors as i64)
            .expect("file too large")
            .max(mem::size_of::<Anchor>()); // reasonable minimum
        println!("spreading anchors {} bytes apart", bytes_per_anchor);

        Self {
            layout,
            bytes_per_anchor,
            migrate_batch_size,
        }
    }

    /// Note: A prefix and/or suffix of at most length 3 may be discarded from the given
    /// segment to align with UTF-8 character boundaries.
    /// They will not be discarded on the edges if the `rigid` flags are set.
    fn create_segment(
        &self,
        mut offset: i64,
        mut data: &[u8],
        rigid_left: bool,
        rigid_right: bool,
    ) -> MappedSegment {
        // Try our best to align the beginning and end of the segment to UTF-8 boundaries
        // Always works for valid UTF-8
        if !rigid_left {
            for _ in 0..3.min(data.len()) {
                if is_utf8_cont(data[0]) {
                    offset += 1;
                    data = &data[1..];
                } else {
                    break;
                }
            }
        }
        if !rigid_right {
            for i in 0..3.min(data.len()) {
                if utf8_seq_len(data[data.len() - i - 1]) > i + 1 {
                    data = &data[..data.len() - i - 1];
                    break;
                }
            }
        }

        let end = offset + data.len() as i64;
        let rand = ((offset + 143) as u32).wrapping_mul(0x9e3779b9);
        let mut seg = {
            MappedSegment {
                start: offset,
                end,
                base_y: (rand & 0xFFFF) as i64,
                base_x_relative: (rand >> 16) as f64,
                first_absolute: 0,
                widest_line: 0.,
                rel_width: 0.,
                anchors: VecDeque::with_capacity(data.len() / self.bytes_per_anchor + 2),
            }
        };
        let mut anchor_acc = self.bytes_per_anchor;
        let mut i = 0;
        let mut cur_y = -seg.base_y;
        let mut abs_x = offset == 0;
        let mut cur_x = if abs_x { 0. } else { -seg.base_x_relative };
        while i < data.len() {
            let (c, adv) = decode_utf8(&data[i..]);
            let place_anchor = anchor_acc >= self.bytes_per_anchor;
            let c_i = i;
            let c = c.unwrap_or(Self::REPLACEMENT_CHAR);
            i += adv;
            anchor_acc += adv;

            if place_anchor {
                anchor_acc -= self.bytes_per_anchor;
                seg.anchors.push_back(Anchor {
                    offset: offset + c_i as i64,
                    y_offset: cur_y,
                    x_offset: cur_x,
                });
                if !abs_x {
                    seg.first_absolute += 1;
                }
            }
            match c {
                Self::NEWLINE => {
                    // Newline
                    if abs_x {
                        seg.widest_line = seg.widest_line.max(cur_x);
                    } else {
                        seg.rel_width = cur_x + seg.base_x_relative;
                    }
                    cur_x = 0.;
                    cur_y += 1;
                    abs_x = true;
                }
                c => {
                    cur_x += self.layout.advance_for(c);
                }
            }
        }
        if abs_x {
            seg.widest_line = seg.widest_line.max(cur_x);
        } else {
            seg.rel_width = cur_x + seg.base_x_relative;
        }
        if anchor_acc != 0 {
            seg.anchors.push_back(Anchor {
                offset: end,
                y_offset: cur_y,
                x_offset: cur_x,
            });
            if !abs_x {
                seg.first_absolute += 1;
            }
        }
        seg
    }

    /// Merge two exactly adjacent segments.
    fn merge_segments(&self, linemap: LineMapHandle, l_idx: usize) {
        lock_linemap!(linemap, lmap_store, lmap);
        let into_left =
            lmap.segments[l_idx].anchors.len() >= lmap.segments[l_idx + 1].anchors.len();
        fn get_two(lmap: &mut LineMap, l: usize) -> (&mut MappedSegment, &mut MappedSegment) {
            let (a, b) = lmap.segments.split_at_mut(l + 1);
            (&mut a[l], &mut b[0])
        }
        {
            // NOTE: The maximum width of the segments will temporarily be wrong, but
            // doing this correctly is way too expensive with the current implementation
            // The length of the relative line will also be temporarily wrong
            let (l, r) = get_two(lmap, l_idx);
            let mut wide = l.widest_line.max(r.widest_line);
            if l.first_absolute < l.anchors.len() {
                // Factor the absolute line that is created by tacking the relative
                // line onto an absolute line
                let w = l.anchors.back().unwrap().x_abs() + r.rel_width;
                wide = wide.max(w);
            } else {
                l.rel_width += r.rel_width;
            }
            if into_left {
                l.widest_line = wide;
            } else {
                r.widest_line = wide;
            }
            r.rel_width = l.rel_width;
        }
        if !into_left {
            // There is a very special case when merging a segment into the right
            // If the left segment ends with absolute X coordinates but the right
            // segment has a relative-X prefix, we *must* update the entire prefix
            // to be absolute before merging
            let (mut lsrc, mut rdst) = get_two(lmap, l_idx);
            if lsrc.first_absolute < lsrc.anchors.len() {
                let lsrc_end_anchor = *lsrc.anchors.back().unwrap();
                let end_x = lsrc_end_anchor.x_abs();
                // Slowly bring the `first_absolute` line to the left
                while rdst.first_absolute > 0 {
                    let l = rdst.first_absolute.saturating_sub(self.migrate_batch_size);
                    for i in l..rdst.first_absolute {
                        let a = &mut rdst.anchors[i];
                        a.x_offset = a.x_offset + (rdst.base_x_relative + end_x);
                    }
                    rdst.first_absolute = l;
                    // Keep bumping the linemap to not block the main thread
                    drop(lsrc);
                    drop(rdst);
                    lock_linemap!(linemap, lmap_store, lmap => bump);
                    let (l, r) = get_two(lmap, l_idx);
                    lsrc = l;
                    rdst = r;
                }
            }
        }
        // TODO: Make sure we don't stall while growing the anchor `VecDeque`
        // In cases where a large grow needs to be done, this entails allocating a separate clone
        // and slowly copying the data over while regularly bumping the mutex
        loop {
            if into_left {
                // Move anchors from the RIGHT segment to the LEFT segment
                let (ldst, rsrc) = get_two(lmap, l_idx);
                let batch_size = self.migrate_batch_size.min(rsrc.anchors.len() - 1);
                if batch_size == 0 {
                    break;
                }
                // Remove the end anchor because it is duplicated with the
                // start anchor of the next segment
                let og_ldst_len = ldst.anchors.len();
                let dst_end_anchor = *ldst.anchors.back().unwrap();
                let end_y = dst_end_anchor.y(ldst);
                let end_x = dst_end_anchor.x(ldst);
                ldst.anchors.pop_back();
                // Map the absolute index from the right segment to the left segment
                let og_rsrc_first_absolute = rsrc.first_absolute;
                if ldst.first_absolute >= og_ldst_len {
                    ldst.first_absolute = og_ldst_len - 1 + rsrc.first_absolute.min(batch_size + 1);
                }
                rsrc.first_absolute = rsrc.first_absolute.saturating_sub(batch_size);
                // Determine how much to shift down the right segment
                let rsrc_old_start = *rsrc.anchors.front().unwrap();
                let rsrc_new_start = rsrc.anchors[batch_size];
                let shift_y = rsrc_old_start.y_offset - rsrc_new_start.y_offset;
                // TODO: This is actually all wrong
                // Because we want to keep the first anchor at position zero,
                // when removing a prefix we might convert some absolute anchors
                // into relative ones, and this has no easy fix.
                // Just give up in this case
                // This requires better data-structures, and is part of the
                // reason I'm planning a refactor
                let shift_x = if og_rsrc_first_absolute > batch_size {
                    rsrc_old_start.x_offset - rsrc_new_start.x_offset
                } else {
                    0.
                };
                // Migrate anchor-by-anchor
                for i in 0..batch_size + 1 {
                    let mut a = *rsrc.anchors.front().unwrap();
                    if i != batch_size {
                        // Do not remove the last anchor
                        // It is the start anchor of the right segment and
                        // the end anchor of the left segment, so it must
                        // be duplicated
                        rsrc.anchors.pop_front();
                    };
                    // Convert between coordinate bases
                    a.y_offset = a.y_offset + (rsrc.base_y - ldst.base_y + end_y);
                    // Whether this anchor will be absolute in the destination segment
                    let dst_abs = og_ldst_len - 1 + i >= ldst.first_absolute;
                    // Whether this anchor was absolute in the source segment
                    let src_abs = i >= og_rsrc_first_absolute;
                    match (dst_abs, src_abs) {
                        (true, true) => {} // No conversion
                        (true, false) => {
                            // Remove the base offset, then nudge by the end x
                            a.x_offset = a.x_offset + (rsrc.base_x_relative + end_x);
                        }
                        (false, false) => {
                            // Convert between bases and nudge by the end x
                            a.x_offset =
                                a.x_offset + (rsrc.base_x_relative - ldst.base_x_relative + end_x);
                        }
                        (false, true) => {
                            // Should never happen, because an absolute anchor
                            // will never become relative while loading data before it
                            unreachable!();
                        }
                    }
                    ldst.anchors.push_back(a);
                }
                // Shift the right segment down by whatever was removed
                rsrc.base_y += shift_y;
                rsrc.base_x_relative += shift_x;
                // Keep the end and start offsets in sync with the endpoint anchors
                let src_start_anchor = rsrc.anchors.front().unwrap();
                ldst.end = src_start_anchor.offset;
                rsrc.start = src_start_anchor.offset;
            } else {
                // Move anchors FROM THE LEFT segment TO THE RIGHT segment
                let (lsrc, rdst) = get_two(lmap, l_idx);
                let batch_size = self.migrate_batch_size.min(lsrc.anchors.len() - 1);
                if batch_size == 0 {
                    break;
                }
                // Remove the end anchor of the left segment because it is redundant with
                // the start anchor of the right segment
                let og_lsrc_len = lsrc.anchors.len();
                let lsrc_cap_anchor = lsrc.anchors.pop_back().unwrap();
                // Get the anchor that will be the end of the left segment/start of the right segment
                let src_end_idx = og_lsrc_len - 1 - batch_size;
                let src_end_anchor = lsrc.anchors[src_end_idx];
                // Shift the right segment by the width/height that we are migrating
                let shift_y = lsrc_cap_anchor.y_offset - src_end_anchor.y_offset;
                let shift_x = if lsrc.first_absolute >= og_lsrc_len {
                    lsrc_cap_anchor.x_offset - src_end_anchor.x_offset
                } else {
                    0.
                };
                // Map the absolute index from the left segment to the right segment
                let og_lsrc_first_absolute = lsrc.first_absolute;
                rdst.first_absolute += batch_size;
                if lsrc.first_absolute < og_lsrc_len {
                    rdst.first_absolute = rdst
                        .first_absolute
                        .min(lsrc.first_absolute.max(src_end_idx) - src_end_idx);
                }
                lsrc.first_absolute = lsrc.first_absolute.min(src_end_idx);
                // Shift all Y coordinates in the right segment by the end Y of the left segment
                rdst.base_y += shift_y;
                // Shift all relative X coordinates in the right segment by the end of the left segment
                rdst.base_x_relative += shift_x;
                for i in (0..batch_size).rev() {
                    let mut a = *lsrc.anchors.back().unwrap();
                    if i != 0 {
                        // Do not remove the last anchor, because it is both the end anchor
                        // of the left segment and the start anchor of the right segment,
                        // so it must be duplicated
                        lsrc.anchors.pop_back();
                    }
                    // Convert between coordinate bases
                    let src_abs = og_lsrc_len - 1 - batch_size + i >= og_lsrc_first_absolute;
                    match src_abs {
                        false => {
                            // Old position was relative to the start of the left segment
                            // New position needs to be relative to `src_end_anchor`,
                            // because that will be the new start of the right segment
                            a.x_offset =
                                a.x_offset + (-src_end_anchor.x_offset - rdst.base_x_relative);
                        }
                        true => {} // No conversion
                    }
                    a.y_offset = a.y_offset + (-src_end_anchor.y_offset - rdst.base_y);
                    rdst.anchors.push_front(a);
                }
                // Keep the end and start offsets in sync with the endpoint anchors
                let rdst_start_anchor = rdst.anchors.front().unwrap();
                lsrc.end = rdst_start_anchor.offset;
                rdst.start = rdst_start_anchor.offset;
            }
            // Bump the linemap mutex to keep latency low
            // Safe to do because at this point the segments are in
            // a valid state
            lock_linemap!(linemap, lmap_store, lmap => bump);
        }
        // Finally, remove the empty source segment
        let empty = lmap.segments.remove(l_idx + if into_left { 1 } else { 0 });
        // DEBUG: Sanity check
        if false {
            println!(
                "doing sanity check after merging (into_left = {:?})",
                into_left
            );
            struct DumpOnPanic<'a>(&'a LineMap);
            impl Drop for DumpOnPanic<'_> {
                fn drop(&mut self) {
                    self.0.dump_anchors();
                }
            }
            let dump = DumpOnPanic(lmap);
            let mut touching = false;
            for s in lmap.segments.iter() {
                if s.start >= s.end {
                    touching = true;
                }
                assert_eq!(s.start, s.anchors.front().unwrap().offset);
                assert_eq!(s.end, s.anchors.back().unwrap().offset);
                assert!(s.first_absolute <= s.anchors.len());
                assert_eq!(s.anchors.front().unwrap().y_offset + s.base_y, 0);
                for i in 1..s.anchors.len() {
                    let a = s.anchors[i - 1];
                    let b = s.anchors[i];
                    assert!(a.offset < b.offset);
                    assert!(a.y_offset <= b.y_offset);
                }
            }
            for i in 1..lmap.segments.len() {
                let s = &lmap.segments[i];
                let p = &lmap.segments[i - 1];
                if p.end >= s.start {
                    touching = true;
                }
            }
            mem::forget(dump);
            if touching {
                println!("adjacent/empty segments are present, dumping...");
                eprintln!("adjacent/empty segments are present, dumping...");
                lmap.dump_anchors();
            }
        }
        // Drop the empty segment while the lock is released
        // (Dropping large buffers takes time)
        drop(lmap_store);
        drop(empty);
    }

    fn insert_segment(&self, linemap: LineMapHandle, seg: MappedSegment) {
        if seg.start == seg.end {
            return;
        }
        // check if this segment merges into a segment to the left
        lock_linemap!(linemap, lmap_store, lmap);
        let mut i = lmap.find_before(seg.start);
        let mut merge_left = false;
        if i == lmap.segments.len() {
            i = 0;
        } else if let Some(s) = lmap.segments.get_mut(i) {
            if s.end >= seg.start {
                // merge with this segment
                assert!(
                    s.end == seg.start,
                    "attempt to insert partially overlapping segment"
                );
                merge_left = true;
            }
            i += 1;
        }
        // check if this segment merges with a following segment
        let j = lmap.find_after(seg.end as i64);
        let mut merge_right = false;
        if let Some(s) = lmap.segments.get_mut(j) {
            if s.start <= seg.end {
                // merge this suffix into the segment
                assert!(
                    s.start == seg.end,
                    "attempt to insert partially overlapping segment"
                );
                merge_right = true;
            }
        }
        // insert segment, possibly adjacent to the nearby segments
        lmap.segments.splice(i..j, std::iter::once(seg));
        // slowly merge the segments, regularly unlocking the linemap
        drop(lmap_store);
        if merge_left {
            self.merge_segments(linemap, i - 1);
            i -= 1;
        }
        if merge_right {
            self.merge_segments(linemap, i);
        }
    }

    /// Process a piece of data, adding any missing line mappings from it.
    ///
    /// Note: A prefix and/or suffix of at most length 3 may be discarded from the given
    /// segment to align with UTF-8 character boundaries.
    pub fn process_data<'a>(&self, linemap: LineMapHandle, offset: i64, mut data: &[u8]) {
        // iterate over the "holes" that are contained in the received range
        let end = offset + data.len() as i64;
        let mut i;
        let mut l;
        let mut rigid_left = offset == 0;
        {
            lock_linemap!(linemap, lmap);
            i = lmap.find_after(offset);
            l = offset;
            if let Some(s) = lmap.segments.get(i) {
                if s.start <= offset {
                    l = s.end.min(end);
                    data = &data[(l - offset) as usize..];
                    i += 1;
                    rigid_left = true;
                }
            }
        }
        loop {
            // we have a hole from `l` to `r`
            let (r, next_l, rigid_right) = {
                lock_linemap!(linemap, lmap);
                lmap.segments
                    .get(i)
                    .map(|s| (s.start.min(end), s.end.min(end), s.start <= end))
                    .unwrap_or((end, end, end == lmap.file_size))
            };
            // process data first without locking the linemap
            let seg = self.create_segment(l, &data[..(r - l) as usize], rigid_left, rigid_right);
            rigid_left = true;
            // insert the data into the linemap
            self.insert_segment(linemap, seg);
            // advance to the next hole
            if r >= end {
                break;
            } else {
                data = &data[(next_l - l) as usize..];
                l = next_l;
                i += 1;
            }
        }
    }
}

#[derive(Debug)]
pub struct MappedSegment {
    /// Inclusive start of this segment in absolute bytes.
    /// This information is redundant with the offset of the first anchor.
    /// If the start is `0`, Y coordinates are relative to the start of the
    /// file, so they can be considered absolute.
    pub(super) start: i64,
    /// Exclusive end of this segment in absolute bytes.
    /// This information is redundant with the offset of the last anchor.
    pub(super) end: i64,
    /// The index of the first anchor that has an absolute X coordinate.
    /// If there are no absolute anchors, it is the amount of anchors.
    pub(super) first_absolute: usize,
    /// Base line number.
    /// The coordinates in anchors must be added with this value to have any meaning.
    /// This allows to shift the X coordinate of the entire segment quickly.
    pub(super) base_y: i64,
    /// Base X coordinate, only for **relative** X coordinates.
    /// The X coordinate is special in that absolute X coordinates do not use a base.
    /// The coordinates in relative-X anchors must be added with this value to have any meaning.
    /// This allows to shift the X coordinate of the entire relative prefix of a segment quickly.
    pub(super) base_x_relative: f64,
    /// The widest *absolute* line that this segment contains.
    /// Does not include the first relative line, if any.
    /// Includes the last line, which may not end in a newline!
    /// If there are no absolute lines, is zero.
    /// This value may overestimate if segments are currently being merged!
    pub(super) widest_line: f64,
    /// The X width of the single relative line.
    /// If there is no relative line, this is zero.
    /// This value may be completely wrong if segments are currently being merged!
    pub(super) rel_width: f64,
    /// A set of anchor points, representing known reference points with X and Y coordinates.
    /// There is always an anchor at the start of the segment and at the end of the segment.
    pub(super) anchors: VecDeque<Anchor>,
}
impl MappedSegment {
    /// Check if the given anchor has an absolute X coordinate.
    fn is_x_absolute(&self, anchor: Anchor) -> bool {
        match self.anchors.get(self.first_absolute) {
            None => false,
            Some(abs) => anchor.offset >= abs.offset,
        }
    }

    /// Find the last anchor before or at the given offset.
    fn find_lower(&self, offset: i64) -> Option<Anchor> {
        match self.anchors.partition_point(|a| a.offset <= offset) {
            0 => None,
            i => Some(self.anchors[i - 1]),
        }
    }

    /// Find the first anchor at or after the given offset.
    fn _find_upper(&self, offset: i64) -> Option<Anchor> {
        self.anchors
            .get(self.anchors.partition_point(|a| a.offset < offset))
            .copied()
    }

    /// Find the last anchor before or at the given relative Y, breaking ties by
    /// choosing the largest relative/absolute X before the given relative/absolute X.
    /// If there is no such anchor, returns the first anchor.
    fn locate_lower(&self, y: i64, x: f64) -> Anchor {
        // Ugly hack because `partition_point` does not provide the index
        let rel_offset = self
            .anchors
            .get(self.first_absolute)
            .map(|a| a.offset)
            .unwrap_or(self.end + 1);
        match self.anchors.partition_point(|a| {
            a.y(self) < y
                || a.y(self) == y && a.x_with(self.base_x_relative, a.offset >= rel_offset) <= x
        }) {
            0 => self.anchors[0],
            i => self.anchors[i - 1],
        }
    }

    /// Find the first anchor at or after the given relative Y, breaking ties by
    /// choosing the smallest relative/absolute X after the given relative/absolute X.
    /// If there is no such anchor, returns the last anchor.
    fn locate_upper(&self, y: i64, x: f64) -> Anchor {
        // Ugly hack because `partition_point` does not provide the index
        let rel_offset = self
            .anchors
            .get(self.first_absolute)
            .map(|a| a.offset)
            .unwrap_or(self.end + 1);
        *self
            .anchors
            .get(self.anchors.partition_point(|a| {
                a.y(self) < y
                    || a.y(self) == y && a.x_with(self.base_x_relative, a.offset >= rel_offset) < x
            }))
            .unwrap_or(self.anchors.back().unwrap())
    }
}

/// Check if the given byte is a UTF-8 continuation byte.
fn is_utf8_cont(b: u8) -> bool {
    b & 0b1100_0000 == 0b1000_0000
}

/// Get the length of the byte sequence started by the `b` byte.
/// If `b` does not start a byte sequence (ie. it is a continuation
/// byte), returns 0.
/// Does not handle invalid UTF-8, this must be handled while
/// parsing the sequence.
fn utf8_seq_len(b: u8) -> usize {
    if b & 0b1000_0000 == 0 {
        1
    } else if b & 0b0100_0000 == 0 {
        0
    } else if b & 0b0010_0000 == 0 {
        2
    } else if b & 0b0001_0000 == 0 {
        3
    } else {
        4
    }
}

/// Decode a single UTF-8 character from the given non-empty byte slice.
/// Returns the length of the character.
/// If given malformed UTF-8 it may not raise an error but produce incorrect
/// results.
/// However, it still checks the validity of continuation bytes, a feature
/// that is necessary for synchronizing with UTF-8 streams starting from an
/// arbitrary position.
pub fn decode_utf8(b: &[u8]) -> (Result<u32, u8>, usize) {
    assert!(!b.is_empty());
    if b[0] & 0b1000_0000 == 0 {
        // Single byte
        return (Ok(b[0] as u32), 1);
    } else if b[0] & 0b0100_0000 == 0 {
        // Continuation byte
    } else if b[0] & 0b0010_0000 == 0 {
        // Two bytes
        if b.len() >= 2 && is_utf8_cont(b[1]) {
            return (
                Ok((b[0] as u32 & 0b0001_1111) << 6 | (b[1] as u32 & 0b0011_1111)),
                2,
            );
        }
    } else if b[0] & 0b0001_0000 == 0 {
        // Three bytes
        if b.len() >= 3 && is_utf8_cont(b[1]) && is_utf8_cont(b[2]) {
            return (
                Ok((b[0] as u32 & 0b1111) << 12
                    | (b[1] as u32 & 0b0011_1111) << 6
                    | (b[2] as u32 & 0b0011_1111)),
                3,
            );
        }
    } else if b[0] & 0b0000_1000 == 0 {
        // Four bytes
        if b.len() >= 4 && is_utf8_cont(b[1]) && is_utf8_cont(b[2]) && is_utf8_cont(b[3]) {
            return (
                Ok((b[0] as u32 & 0b0111) << 18
                    | (b[1] as u32 & 0b0011_1111) << 12
                    | (b[2] as u32 & 0b0011_1111) << 6
                    | (b[3] as u32 & 0b0011_1111)),
                4,
            );
        }
    }
    // Invalid UTF-8 character, fall back to reading a single byte
    (Err(b[0]), 1)
}

/// Similar to `decode_utf8` but in reverse.
/// Reads a single character from the end of the given slice.
pub fn decode_utf8_rev(b: &[u8]) -> (Result<u32, u8>, usize) {
    assert!(!b.is_empty());
    let n = b.len();
    if b[n - 1] & 0b1000_0000 == 0 {
        // Single byte
        return (Ok(b[n - 1] as u32), 1);
    } else if is_utf8_cont(b[n - 1]) && n >= 2 {
        // At least 2-byte
        if b[n - 2] & 0b1110_0000 == 0b1100_0000 {
            // Two bytes
            return (
                Ok((b[n - 2] as u32 & 0b0001_1111) << 6 | (b[n - 1] as u32 & 0b0011_1111)),
                2,
            );
        } else if is_utf8_cont(b[n - 2]) && n >= 3 {
            // At least 3-byte
            if b[n - 3] & 0b1111_0000 == 0b1110_0000 {
                // Three bytes
                return (
                    Ok((b[n - 3] as u32 & 0b1111) << 12
                        | (b[n - 2] as u32 & 0b0011_1111) << 6
                        | (b[n - 1] as u32 & 0b0011_1111)),
                    3,
                );
            } else if is_utf8_cont(b[n - 3]) && n >= 4 && b[n - 4] & 0b1111_1000 == 0b1111_0000 {
                // Four bytes
                return (
                    Ok((b[n - 4] as u32 & 0b0111) << 18
                        | (b[n - 3] as u32 & 0b0011_1111) << 12
                        | (b[n - 2] as u32 & 0b0011_1111) << 6
                        | (b[n - 1] as u32 & 0b0011_1111)),
                    4,
                );
            }
        }
    }
    // Invalid UTF-8, fall back to reading 1 byte
    (Err(b[n - 1]), 1)
}

#[derive(Clone, Copy, Debug)]
pub struct Anchor {
    /// The absolute byte offset that this anchor marks.
    pub offset: i64,
    /// The line number relative to the `base_y` value of the containing segment.
    pub y_offset: i64,
    /// The `f64` X position of this anchor, may be relative or absolute.
    pub x_offset: f64,
}
impl Anchor {
    /// Only correct for relative-X anchors.
    pub fn _x_rel(&self, base_x: f64) -> f64 {
        self.x_offset + base_x
    }

    /// Only correct for absolute-X anchors.
    pub fn x_abs(&self) -> f64 {
        self.x_offset
    }

    pub fn x(&self, s: &MappedSegment) -> f64 {
        self.x_with(s.base_x_relative, s.is_x_absolute(*self))
    }

    pub fn x_with(&self, base_x: f64, is_abs: bool) -> f64 {
        if is_abs {
            self.x_offset
        } else {
            self.x_offset + base_x
        }
    }

    pub fn y(&self, s: &MappedSegment) -> i64 {
        self.y_with(s.base_y)
    }

    pub fn y_with(&self, base_y: i64) -> i64 {
        self.y_offset + base_y
    }
}
