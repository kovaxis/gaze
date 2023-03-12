use crate::prelude::*;

pub struct SparseSegment {
    pub offset: i64,
    pub data: Vec<u8>,
}

/// Holds sparse segments of data loaded from a potentially huge file.
pub struct SparseData {
    segments: Vec<SparseSegment>,
    file_size: i64,
}
impl SparseData {
    pub fn new(file_size: i64) -> Self {
        Self {
            segments: default(),
            file_size,
        }
    }

    /// Find the index of the loaded segment that the given offset falls into.
    /// If there is no such segment, returns the index of the next available
    /// segment.
    /// Note that in the latter case, there might be no next available segment.
    /// In this case, the index will simply be the amount of segments.
    pub fn find_segment(&self, offset: i64) -> StdResult<usize, usize> {
        let i = self
            .segments
            .partition_point(|s| (s.offset + s.data.len() as i64) <= offset);
        if let Some(s) = self.segments.get(i) {
            if s.offset <= offset {
                return Ok(i);
            }
        }
        Err(i)
    }

    pub fn insert_segment(&mut self, mut offset: i64, mut data: Vec<u8>) {
        let i = self
            .segments
            .binary_search_by_key(&offset, |s| s.offset + s.data.len() as i64)
            .unwrap_or_else(|i| i);
        let mut j = self
            .segments
            .binary_search_by_key(&(offset + data.len() as i64), |s| {
                s.offset + s.data.len() as i64
            })
            .map(|i| i + 1)
            .unwrap_or_else(|i| i);
        if let Some(s) = self.segments.get_mut(i) {
            if s.offset < offset {
                // this segment intersects with the start of the inserted segment
                s.data
                    .truncate(usize::try_from(offset - s.offset).expect("read too large"));
                s.data.extend_from_slice(&data);
                offset = s.offset;
                mem::swap(&mut data, &mut s.data);
            }
        }
        if let Some(s) = self.segments.get_mut(j) {
            if (offset + data.len() as i64) >= s.offset {
                // this segment intersects with the end of the inserted segment
                data.extend_from_slice(
                    &s.data[usize::try_from(offset + data.len() as i64 - s.offset)
                        .expect("read too large")..],
                );
                j += 1;
            }
        }
        self.segments
            .splice(i..j, std::iter::once(SparseSegment { offset, data }));
    }

    /// Finds the longest prefix of the given `[lo, hi)` range that is loaded and available.
    pub fn longest_prefix(&self, lo: i64, hi: i64) -> &[u8] {
        for s in self.segments.iter().rev() {
            if s.offset <= lo {
                return &s.data[(lo - s.offset).min(s.data.len() as i64) as usize
                    ..(hi - s.offset).min(s.data.len() as i64) as usize];
            }
        }
        &[][..]
    }

    pub fn get(&self, i: usize) -> Option<&SparseSegment> {
        self.segments.get(i)
    }
}
impl ops::Index<usize> for SparseData {
    type Output = SparseSegment;
    fn index(&self, i: usize) -> &SparseSegment {
        &self.segments[i]
    }
}
impl fmt::Debug for SparseData {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "(")?;
        let mut first = true;
        for seg in self.segments.iter() {
            if first {
                first = false;
            } else {
                write!(f, ", ")?;
            }
            write!(
                f,
                "[{}, {})",
                seg.offset,
                seg.offset + seg.data.len() as i64
            )?;
        }
        Ok(())
    }
}
