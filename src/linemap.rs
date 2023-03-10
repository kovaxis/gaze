use glyph_brush::ab_glyph::{Font, FontArc, GlyphId};

use crate::prelude::*;

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
    segments: Vec<MappedSegment>,
    bytes_per_anchor: usize,
    file_size: i64,
    font: FontArc,
    replacement_width: f32,
}
impl LineMap {
    pub const REPLACEMENT_CHAR: char = char::REPLACEMENT_CHARACTER;

    pub fn new(font: FontArc, max_memory: usize, file_size: i64) -> Self {
        let max_anchors = max_memory / mem::size_of::<Anchor>();
        let bytes_per_anchor =
            usize::try_from(file_size / max_anchors as i64).expect("file too large");
        Self {
            segments: default(),
            bytes_per_anchor,
            file_size,
            replacement_width: font.h_advance_unscaled(font.glyph_id(Self::REPLACEMENT_CHAR))
                / font.line_gap_unscaled(),
            font,
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

    fn create_segment(&self, offset: i64, data: &[u8]) -> MappedSegment {
        let mut seg = {
            let mut first_bytes_buf = [0; 4];
            first_bytes_buf[..4.min(data.len())].copy_from_slice(&data[..4.min(data.len())]);
            let mut last_bytes_buf = [0; 4];
            last_bytes_buf[4 - 4.min(data.len())..]
                .copy_from_slice(&data[data.len().saturating_sub(4)..]);
            MappedSegment {
                start: offset,
                end: offset + data.len() as i64,
                last_bytes_buf,
                first_bytes_buf,
                y_absolute: offset == 0,
                end_x: PosX::Rel(0.),
                anchors: Vec::with_capacity(data.len() / self.bytes_per_anchor + 1),
            }
        };
        let scale = self.font.line_gap_unscaled().recip();
        let mut anchor_acc = self.bytes_per_anchor;
        let mut i = 0;
        let mut cur_x = 0.;
        let mut cur_y = 0;
        let mut abs_x = offset == 0;
        while i < data.len() {
            let c = decode_utf8(&data[i..]);
            let place_anchor = anchor_acc >= self.bytes_per_anchor;
            let c_i = i;
            let c = match c {
                None => {
                    // not a valid UTF-8 character
                    i += 1;
                    anchor_acc += 1;
                    None
                }
                Some((c, adv)) => {
                    i += adv;
                    anchor_acc += adv;
                    Some(c)
                }
            };
            if place_anchor {
                seg.anchors
                    .push(Anchor::new(offset + c_i as i64, cur_x, cur_y, abs_x));
                anchor_acc -= self.bytes_per_anchor;
            }
            match c {
                None => {
                    cur_x += self.replacement_width as f64;
                }
                Some('\n') => {
                    cur_x = 0.;
                    cur_y += 1;
                    abs_x = true;
                }
                Some(c) => {
                    let glyph = self.font.glyph_id(c);
                    cur_x += (self.font.h_advance_unscaled(glyph) * scale) as f64;
                }
            }
        }
        seg.end_x = if abs_x {
            PosX::Abs(cur_x)
        } else {
            PosX::Rel(cur_x)
        };
        seg
    }

    /// Merge two segments that are touching exactly at their edges.
    /// Place the result into the left segment.
    fn merge_segments(&self, left: &mut MappedSegment, right: &MappedSegment) {
        // Add all anchors on the right side to the left side
        let mut anchors = mem::take(&mut left.anchors);
        let mut n = anchors.len();
        anchors.extend_from_slice(&right.anchors);

        // One important issue we can have when merging segments is that
        // the segment creation algorithm interpreted UTF-8 sequences
        // at the edges incorrectly because of missing context.
        // This means that all we need to do is identify these sequences,
        // undo the incorrect spacing, and then redo the correct spacing.
        // The six potential sequences in particular are:
        //  N-3 N-2 N-1| 0   1   2
        //         [ S | C ]
        //         [ S | C   C ]
        //         [ S | C   C   C ]
        //     [ S   C | C ]
        //     [ S   C | C   C ]
        // [ S   C   C | C ]
        let mut buf = [0; 8];
        buf[..4].copy_from_slice(&left.last_bytes_buf);
        buf[4..].copy_from_slice(&right.first_bytes_buf);
        let mut check = None;
        for i in (1..4).rev() {
            let len = utf8_seq_len(buf[i]);
            if len > 4 - i {
                check = Some((i, len));
                // we can quit eagerly, because if this check does
                // succeed, we know for sure that further checks
                // will not succeed either
                break;
            }
        }
        let mut x_nudge = 0.;
        if let Some((buf_i, clen)) = check {
            if let Some((c, _clen)) = decode_utf8(&buf[buf_i..]) {
                // This character was interpreted incorrectly.
                // In particular, each and all of its bytes were interpreted as
                // replacement characters.
                // Therefore, if the character starts at absolute byte offset `j`,
                // all anchors with offset `j < offset < j+clen` should be moved back
                // to offset `j`, and their X coordinates shifted by
                // `-(offset - j) * replacement_width`.
                // Also, all anchors with offset `j+clen <= offset` should have their
                // X coordinate shifted by `width(c) - clen * replacement_width`.
                let i = left.end + (buf_i as i64 - 4);
                for anchor in anchors[..n].iter_mut().rev() {
                    if anchor.offset > i {
                        anchor
                            .nudge_x(((i - anchor.offset) as f32 * self.replacement_width) as f64);
                        anchor.offset = i;
                    } else {
                        break;
                    }
                }
                for anchor in anchors[n..].iter_mut() {
                    if anchor.offset < i + clen as i64 {
                        anchor
                            .nudge_x(((i - anchor.offset) as f32 * self.replacement_width) as f64);
                        anchor.offset = i;
                        // These anchors are actually part of the left side
                        n += 1;
                    } else {
                        break;
                    }
                }
                x_nudge += (self.font.h_advance_unscaled(self.font.glyph_id(c))
                    * self.font.line_gap_unscaled().recip()) as f64
                    - clen as f64 * self.replacement_width as f64;
            }
        }

        // Nudge all relative-x anchors on the right side by whatever was the last X position
        // Also, if the last segment ends on an absolute X position, make the relative-x positions
        // on the right side absolute
        let (x_nudge, make_abs) = match left.end_x {
            PosX::Rel(x) => (x_nudge + x, false),
            PosX::Abs(x) => (x_nudge + x, true),
        };
        for anchor in anchors[n..].iter_mut() {
            if let PosX::Rel(x) = anchor.x() {
                anchor.set_x(x + x_nudge, make_abs);
            } else {
                break;
            }
        }

        // Merge the start bytes
        let mut bytes_prefix = left.first_bytes_buf;
        let ln = (left.end - left.start) as usize;
        if ln < 4 {
            bytes_prefix[ln..].copy_from_slice(&right.first_bytes_buf[..4 - ln]);
        }

        // Merge the end bytes
        let mut bytes_suffix = right.last_bytes_buf;
        let rn = (right.end - right.start) as usize;
        if rn < 4 {
            bytes_suffix[..4 - rn].copy_from_slice(&left.last_bytes_buf[rn..]);
        }

        *left = MappedSegment {
            start: left.start,
            end: right.end,
            last_bytes_buf: bytes_suffix,
            first_bytes_buf: bytes_prefix,
            y_absolute: left.y_absolute,
            end_x: match (right.end_x, make_abs) {
                (PosX::Rel(x), false) => PosX::Rel(x),
                (PosX::Rel(x), true) => PosX::Abs(x),
                (PosX::Abs(x), _) => PosX::Abs(x),
            },
            anchors,
        };
    }

    /// Process a segment of data, adding anchors for it to the loaded segments.
    pub fn process_segment(&mut self, offset: i64, data: &[u8]) {
        // process the data into a self-contained segment
        let mut seg = self.create_segment(offset, data);
        // check if this segment merges into a segment to the left
        let mut i = self.find_before(offset);
        let mut prefix = None;
        if let Some(s) = self.segments.get_mut(i) {
            if s.end >= offset {
                // merge with this segment
                assert!(
                    s.end == offset,
                    "attempt to insert partially overlapping segment"
                );
                prefix = Some(mem::replace(s, MappedSegment::placeholder()));
            } else {
                i += 1;
            }
        }
        if let Some(mut prefix) = prefix {
            self.merge_segments(&mut prefix, &mut seg);
            mem::swap(&mut seg, &mut prefix);
        }
        // check if this segment merges with a following segment
        let mut j = self.find_after(offset + data.len() as i64);
        if let Some(s) = self.segments.get(j) {
            if s.start <= offset + data.len() as i64 {
                // merge this suffix into the segment
                assert!(
                    s.start == offset + data.len() as i64,
                    "attempt to insert partially overlapping segment"
                );
                self.merge_segments(&mut seg, s);
                j += 1;
            }
        }
        // replace previous segments with new merged segment
        self.segments.splice(i..j, std::iter::once(seg));
    }

    /// Process a piece of data, adding any missing line mappings from it.
    pub fn process_data(&mut self, mut offset: i64, mut data: &[u8]) {
        // iterate over the "holes" that are contained in the received range
        let end = offset + data.len() as i64;
        let mut i = self.find_after(offset);
        if let Some(s) = self.segments.get(i) {
            if s.start <= offset {
                data = &data[(s.end - offset).min(data.len() as i64) as usize..];
                offset = s.end;
                i += 1;
            }
        }
        loop {
            let l = offset;
            let r = self
                .segments
                .get(i)
                .map(|s| s.start)
                .unwrap_or(end)
                .min(end);
            // we have a hole from `l` to `r`
            self.process_segment(l, &data[..(r - l) as usize]);
            // advance to the next hole
            if r >= end {
                break;
            } else {
                data = &data[(self.segments[i].end - offset).min(data.len() as i64) as usize..];
                offset = self.segments[i].end;
                i += 1;
            }
        }
    }
}

pub struct MappedSegment {
    /// Inclusive start of this segment in absolute bytes.
    start: i64,
    /// Exclusive end of this segment in absolute bytes.
    end: i64,
    /// The last few bytes of the segment.
    /// Used for merging segments.
    /// Note that it might contain fewer than 4 bytes if `end - start` is less than 4.
    last_bytes_buf: [u8; 4],
    /// The first few bytes of the segment.
    first_bytes_buf: [u8; 4],
    /// `true` if this segment starts at the beggining of the file, and therefore line
    /// numbers are actually absolute.
    y_absolute: bool,
    /// The X coordinate of the segment's end edge.
    /// Might be absolute or relative.
    end_x: PosX,
    /// A set of anchor points, representing known reference points with X and Y coordinates.
    anchors: Vec<Anchor>,
}
impl MappedSegment {
    /// Create a somewhat invalid segment, to act as a discardable placeholder.
    fn placeholder() -> Self {
        Self {
            start: 0,
            end: 0,
            last_bytes_buf: [0; 4],
            first_bytes_buf: [0; 4],
            y_absolute: false,
            end_x: PosX::Abs(0.),
            anchors: vec![],
        }
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
fn decode_utf8(b: &[u8]) -> Option<(char, usize)> {
    let len = utf8_seq_len(b[0]);
    if b.len() < len {
        return None;
    }
    match std::str::from_utf8(&b[..len]) {
        Ok(s) => s.chars().next().map(|c| (c, len)),
        Err(_err) => None,
    }
}

#[derive(Clone, Copy)]
enum PosX {
    Abs(f64),
    Rel(f64),
}

#[derive(Clone, Copy)]
struct Anchor {
    /// The absolute byte offset that this anchor marks.
    pub offset: i64,
    /// The line number relative to the start of the containing segment.
    pub relative_y: i64,
    /// The `f64` X position of this anchor, may be relative (if this anchor is before any newline
    /// in its segment), or absolute.
    /// This is packed into the sign bit, which is why this is stored as a `u64`.
    pub relabs_x: u64,
}
impl Anchor {
    fn new(offset: i64, x: f64, rel_y: i64, absolute_x: bool) -> Self {
        Self {
            offset,
            relative_y: rel_y,
            relabs_x: if absolute_x {
                x.to_bits()
            } else {
                x.to_bits() ^ (1u64 << 63)
            },
        }
    }

    pub fn set_x(&mut self, x: f64, absolute: bool) {
        self.relabs_x = if absolute {
            x.to_bits()
        } else {
            x.to_bits() ^ (1u64 << 63)
        };
    }

    pub fn x(&self) -> PosX {
        let x = f64::from_bits(self.relabs_x & (u64::MAX >> 1));
        if self.relabs_x >> 63 == 0 {
            PosX::Abs(x)
        } else {
            PosX::Rel(x)
        }
    }

    pub fn nudge_x(&mut self, d: f64) {
        self.relabs_x = (f64::from_bits(self.relabs_x & (u64::MAX >> 1)) + d).to_bits()
            | (self.relabs_x & (1u64 << 63));
    }
}
