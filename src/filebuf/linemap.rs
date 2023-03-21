use std::collections::VecDeque;

use glyph_brush::ab_glyph::{Font, FontArc};

use crate::prelude::*;

use super::{LoadedData, LoadedDataGuard};

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
}
impl LineMap {
    pub fn new() -> Self {
        Self {
            segments: default(),
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

    /// Maps the given base offset and a delta range to a pair of anchors that contain the scanline.
    /// Note that these anchors might have Y coordinates different to `dy`.
    /// Returns as a third value the reference anchor, against which the `dy` and `dx` values were
    /// operated.
    pub fn scanline_to_anchors(
        &self,
        base_offset: i64,
        dy: i64,
        dx: (f64, f64),
    ) -> Option<[Anchor; 3]> {
        let i = self.find_after(base_offset);
        if let Some(s) = self.segments.get(i) {
            if s.start <= base_offset {
                // The base offset is contained within a loaded segment
                let base = s.find_lower(base_offset)?;
                let is_x_abs = s.is_x_absolute(base);
                if !is_x_abs && dy != 0 {
                    // When we use a non-absolute base, it means we haven't loaded before
                    // the start of the current line.
                    // Additionally, we don't know the relationship between the X coordinates
                    // of following lines and the base line, therefore if we draw the following
                    // lines it would involve a large amount of dizzy moving text
                    return None;
                }
                let y = base.y(s.base_y) + dy;
                let x0 = base.x(s.base_x_relative, is_x_abs) + dx.0;
                let x1 = base.x(s.base_x_relative, is_x_abs) + dx.1;
                let lo = s.locate_lower(y, x0.max(0.))?;
                let hi = s.locate_upper(y, x1.max(0.))?;
                return Some([lo, hi, base]);
            }
        }
        None
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
        let mut $ref = LoadedDataGuard::lock($handle);
        #[allow(unused_mut)]
        let mut $ref = &mut $ref.guard.linemap;
    };
    ($handle:expr, $lock:ident, $ref:ident) => {
        let mut $lock = LoadedDataGuard::lock($handle);
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
        $lock.bump();
        $ref = &mut $lock.guard.linemap;
    }};
}

pub struct LineMapper {
    pub(super) bytes_per_anchor: usize,
    pub(super) font: FontArc,
    pub(super) scale: f32,
    pub(super) migrate_batch_size: usize,
}
impl LineMapper {
    pub const REPLACEMENT_CHAR: char = char::REPLACEMENT_CHARACTER;

    pub fn new(font: FontArc, max_memory: usize, file_size: i64) -> Self {
        let max_anchors = max_memory / mem::size_of::<Anchor>();
        let bytes_per_anchor = usize::try_from(file_size / max_anchors as i64)
            .expect("file too large")
            .max(mem::size_of::<Anchor>()); // reasonable minimum
        Self {
            bytes_per_anchor,
            scale: font.height_unscaled().recip(),
            font,
            migrate_batch_size: 1024,
        }
    }

    pub fn advance_for(&self, c: char) -> f64 {
        (self.font.h_advance_unscaled(self.font.glyph_id(c)) * self.scale) as f64
    }

    /// Note: A prefix and/or suffix of at most length 3 may be discarded from the given
    /// segment to align with UTF-8 character boundaries.
    fn create_segment(&self, mut offset: i64, mut data: &[u8]) -> MappedSegment {
        // Try our best to align the beginning and end of the segment to UTF-8 boundaries
        // Always works for valid UTF-8
        for _ in 0..3.min(data.len()) {
            if is_utf8_cont(data[0]) {
                offset += 1;
                data = &data[1..];
            }
        }
        for i in 0..3.min(data.len()) {
            if utf8_seq_len(data[data.len() - i - 1]) > i + 1 {
                data = &data[..data.len() - i - 1];
                break;
            }
        }

        let end = offset + data.len() as i64;
        let mut seg = {
            MappedSegment {
                start: offset,
                end,
                base_y: 100,
                base_x_relative: 100.,
                first_absolute: 0,
                anchors: VecDeque::with_capacity(data.len() / self.bytes_per_anchor + 1),
            }
        };
        let mut anchor_acc = self.bytes_per_anchor;
        let mut i = 0;
        let mut cur_y = 0;
        let mut cur_x = 0.;
        let mut abs_x = offset == 0;
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
                    y_offset: cur_y - seg.base_y,
                    x_offset: if abs_x {
                        cur_x
                    } else {
                        cur_x - seg.base_x_relative
                    },
                });
                if !abs_x {
                    seg.first_absolute += 1;
                }
            }
            match c {
                '\n' => {
                    cur_x = 0.;
                    cur_y += 1;
                    abs_x = true;
                }
                c => {
                    cur_x += self.advance_for(c);
                }
            }
        }
        if anchor_acc != 0 {
            seg.anchors.push_back(Anchor {
                offset: end,
                y_offset: cur_y - seg.base_y,
                x_offset: if abs_x {
                    cur_x
                } else {
                    cur_x - seg.base_x_relative
                },
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
        loop {
            if into_left {
                // Move anchors from the right segment to the left segment
                let (ldst, rsrc) = get_two(lmap, l_idx);
                let batch_size = self.migrate_batch_size.min(rsrc.anchors.len() - 1);
                if batch_size == 0 {
                    break;
                }
                // Remove the end anchor because it is duplicated with the
                // start anchor of the next segment
                let og_ldst_len = ldst.anchors.len();
                let dst_end_anchor = ldst.anchors.pop_back().unwrap();
                let end_y = dst_end_anchor.y(ldst.base_y);
                let end_x = dst_end_anchor.x(
                    ldst.base_x_relative,
                    ldst.first_absolute <= ldst.anchors.len(),
                );
                // Map the absolute index from the right segment to the left segment
                let og_src_first_absolute = rsrc.first_absolute;
                if ldst.first_absolute >= og_ldst_len {
                    ldst.first_absolute = og_ldst_len - 1 + rsrc.first_absolute.min(batch_size + 1);
                }
                rsrc.first_absolute = rsrc.first_absolute.saturating_sub(batch_size);
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
                    let src_abs = i >= og_src_first_absolute;
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
                let src_cap_anchor = lsrc.anchors.pop_back().unwrap();
                // Get the anchor that will be the end of the left segment/start of the right segment
                let src_end_idx = lsrc.anchors.len() - batch_size;
                let src_end_anchor = lsrc.anchors[src_end_idx];
                // Shift the right segment by the width/height that we are migrating
                let shift_y = src_cap_anchor.y_offset - src_end_anchor.y_offset;
                let shift_x = if lsrc.first_absolute >= og_lsrc_len {
                    src_cap_anchor.x_offset - src_end_anchor.x_offset
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
                            // Convert between bases
                            a.x_offset = a.x_offset + (lsrc.base_x_relative - rdst.base_x_relative);
                        }
                        true => {} // No conversion
                    }
                    a.y_offset = a.y_offset + (lsrc.base_y - rdst.base_y);
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
        lmap.segments.remove(l_idx + if into_left { 1 } else { 0 });
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
        {
            lock_linemap!(linemap, lmap);
            i = lmap.find_after(offset);
            l = offset;
            if let Some(s) = lmap.segments.get(i) {
                if s.start <= offset {
                    l = s.end.min(end);
                    data = &data[(l - offset) as usize..];
                    i += 1;
                }
            }
        }
        loop {
            // we have a hole from `l` to `r`
            let (r, next_l) = {
                lock_linemap!(linemap, lmap);
                lmap.segments
                    .get(i)
                    .map(|s| (s.start.min(end), s.end.min(end)))
                    .unwrap_or((end, end))
            };
            // process data first without locking the linemap
            let seg = self.create_segment(l, &data[..(r - l) as usize]);
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
    /// A set of anchor points, representing known reference points with X and Y coordinates.
    /// There is always an anchor at the start of the segment and at the end of the segment.
    pub(super) anchors: VecDeque<Anchor>,
}
impl MappedSegment {
    /// Create an invalid segment, to act as a discardable placeholder.
    fn placeholder() -> Self {
        Self {
            start: 0,
            end: 0,
            base_y: 0,
            base_x_relative: 0.,
            first_absolute: 0,
            anchors: VecDeque::new(),
        }
    }

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
    fn find_upper(&self, offset: i64) -> Option<Anchor> {
        self.anchors
            .get(self.anchors.partition_point(|a| a.offset < offset))
            .copied()
    }

    /// Find the last anchor before or at the given relative Y, breaking ties by
    /// choosing the largest relative/absolute X before the given relative/absolute X.
    fn locate_lower(&self, y: i64, x: f64) -> Option<Anchor> {
        // Ugly hack because `partition_point` does not provide the index
        let rel_offset = self
            .anchors
            .get(self.first_absolute)
            .map(|a| a.offset)
            .unwrap_or(self.end + 1);
        match self.anchors.partition_point(|a| {
            a.y(self.base_y) < y
                || a.y(self.base_y) == y && a.x(self.base_x_relative, a.offset >= rel_offset) <= x
        }) {
            0 => None,
            i => Some(self.anchors[i - 1]),
        }
    }

    /// Find the first anchor at or after the given relative Y, breaking ties by
    /// choosing the smallest relative/absolute X after the given relative/absolute X.
    fn locate_upper(&self, y: i64, x: f64) -> Option<Anchor> {
        // Ugly hack because `partition_point` does not provide the index
        let rel_offset = self
            .anchors
            .get(self.first_absolute)
            .map(|a| a.offset)
            .unwrap_or(self.end + 1);
        self.anchors
            .get(self.anchors.partition_point(|a| {
                a.y(self.base_y) < y
                    || a.y(self.base_y) == y
                        && a.x(self.base_x_relative, a.offset >= rel_offset) < x
            }))
            .copied()
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
pub fn decode_utf8(b: &[u8]) -> (Result<char, u8>, usize) {
    let len = utf8_seq_len(b[0]);
    if b.len() < len {
        return (Err(b[0]), 1);
    }
    match std::str::from_utf8(&b[..len]) {
        Ok(s) => (Ok(s.chars().next().unwrap()), len),
        Err(_err) => (Err(b[0]), 1),
    }
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
    pub fn x_rel(&self, base_x: f64) -> f64 {
        self.x_offset + base_x
    }

    /// Only correct for absolute-X anchors.
    pub fn x_abs(&self) -> f64 {
        self.x_offset
    }

    pub fn x(&self, base_x: f64, is_abs: bool) -> f64 {
        if is_abs {
            self.x_offset
        } else {
            self.x_offset + base_x
        }
    }

    pub fn y(&self, base_y: i64) -> i64 {
        self.y_offset + base_y
    }
}