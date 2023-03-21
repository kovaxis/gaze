use crate::prelude::*;

use super::{LoadedData, LoadedDataGuard};

#[derive(Debug)]
pub struct SparseSegment {
    pub offset: i64,
    pub data: Demem,
}

pub type SparseHandle<'a> = &'a Mutex<LoadedData>;
macro_rules! lock_sparse {
    ($handle:expr, $ref:ident) => {
        let mut $ref = LoadedDataGuard::lock($handle);
        #[allow(unused_mut)]
        let mut $ref = &mut $ref.guard.data;
    };
    ($handle:expr, $lock:ident, $ref:ident) => {
        let mut $lock = LoadedDataGuard::lock($handle);
        #[allow(unused_mut)]
        let mut $ref = &mut $lock.guard.data;
    };
    ($handle:expr, $lock:ident, $ref:ident => unlocked $code:block) => {{
        drop($ref);
        drop($lock);
        $code
        $lock = LoadedDataGuard::lock($handle);
        $ref = &mut $lock.guard.data;
    }};
    ($handle:expr, $lock:ident, $ref:ident => bump) => {{
        drop($ref);
        $lock.bump();
        $ref = &mut $lock.guard.data;
    }};
}

/// Holds sparse segments of data loaded from a potentially huge file.
pub struct SparseData {
    pub(super) segments: Vec<SparseSegment>,
    pub(super) file_size: i64,
    pub(super) merge_batch_size: usize,
}
impl SparseData {
    pub fn new(file_size: i64) -> Self {
        Self {
            segments: default(),
            file_size,
            merge_batch_size: 4 * 1024,
        }
    }

    /// Find the first segment that ends at or after the given offset.
    /// Returns the amount of segments if there is no segment after the given offset.
    fn find_after(&self, offset: i64) -> usize {
        for (i, s) in self.segments.iter().enumerate() {
            if s.offset + s.data.len() as i64 >= offset {
                return i;
            }
        }
        self.segments.len()
    }

    /// Find the last segment that starts at or before the given offset.
    /// Returns the amount of segments if there is no segment before the given offset.
    fn find_before(&self, offset: i64) -> usize {
        for (i, s) in self.segments.iter().enumerate().rev() {
            if s.offset <= offset {
                return i;
            }
        }
        self.segments.len()
    }

    /// Find the index of the segment that contains the given offset.
    /// If there is no such segment, returns the next segment, or the amount
    /// of segments if there is no such segment.
    pub fn find_segment(&self, offset: i64) -> StdResult<usize, usize> {
        for (i, s) in self.segments.iter().enumerate() {
            if s.offset + s.data.len() as i64 > offset {
                if s.offset <= offset {
                    return Ok(i);
                } else {
                    return Err(i);
                }
            }
        }
        Err(self.segments.len())
    }

    /// Inserts the given data into the given offset.
    fn insert_segment(&mut self, mut offset: i64, mut data: Vec<u8>) -> usize {
        let mut i = self.find_before(offset);
        let mut j = self.find_after(offset + data.len() as i64);

        if i == self.segments.len() {
            i = 0;
        } else if let Some(s) = self.segments.get_mut(i) {
            let overlap = s.offset + s.data.len() as i64 - offset;
            // Remove duplicate data
            if overlap > 0 {
                s.data.consume_right(overlap as usize);
            }
            // Only remove segment if all data was overwritten
            if overlap < s.data.len() as i64 {
                i += 1;
            }
        }

        if let Some(s) = self.segments.get_mut(j) {
            let overlap = offset + data.len() as i64 - s.offset;
            // Remove duplicate data
            if overlap > 0 {
                s.data.consume_left(overlap as usize);
            }
            // Only remove segment if all data was overwritten
            if overlap >= s.data.len() as i64 {
                j += 1;
            }
        }

        // Remove covered segments, and replace with the new segment
        self.segments.splice(
            i..j,
            std::iter::once(SparseSegment {
                offset,
                data: data.into(),
            }),
        );

        i
    }

    fn merge_segments(handle: SparseHandle, l_idx: usize) {
        lock_sparse!(handle, store, sparse);
        fn get_two(sparse: &mut SparseData, i: usize) -> (&mut SparseSegment, &mut SparseSegment) {
            let (l, r) = sparse.segments.split_at_mut(i + 1);
            (l.last_mut().unwrap(), r.first_mut().unwrap())
        }
        let into_left = sparse.segments[l_idx].data.len() >= sparse.segments[l_idx + 1].data.len();
        loop {
            let batch_size = sparse.merge_batch_size;
            let (l, r) = get_two(sparse, l_idx);
            if into_left {
                // Merge from right to left
                let batch_size = batch_size.min(r.data.len());
                l.data.extend_right(&r.data[..batch_size]);
                r.data.consume_left(batch_size);
                r.offset += batch_size as i64;
                if r.data.is_empty() {
                    break;
                }
            } else {
                // Merge from left to right
                let batch_size = batch_size.min(l.data.len());
                r.data.extend_left(&l.data[l.data.len() - batch_size..]);
                r.offset -= batch_size as i64;
                l.data.consume_right(batch_size);
                if l.data.is_empty() {
                    break;
                }
            }
            // Bump the mutex to allow the main thread to access data with low latency
            lock_sparse!(handle, store, sparse => bump);
        }
        // Remove the empty segment
        sparse
            .segments
            .remove(l_idx + if into_left { 1 } else { 0 });
    }

    /// Inserts and merges the given data range.
    pub fn insert_data(handle: SparseHandle, offset: i64, data: Vec<u8>) {
        if data.is_empty() {
            return;
        }
        // First, insert the data
        lock_sparse!(handle, store, sparse);
        let mut i = sparse.insert_segment(offset, data);
        if i > 0
            && sparse.segments[i - 1].offset + sparse.segments[i - 1].data.len() as i64
                == sparse.segments[i].offset
        {
            lock_sparse!(handle, store, sparse => unlocked {
                Self::merge_segments(handle, i-1);
            });
            i -= 1;
        }
        if i + 1 < sparse.segments.len()
            && sparse.segments[i].offset + sparse.segments[i].data.len() as i64
                == sparse.segments[i + 1].offset
        {
            drop(sparse);
            drop(store);
            Self::merge_segments(handle, i);
        }
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

/// Represents a chunk of memory with amortized O(1) addition of memory
/// both to the left and to the right.
pub struct Demem {
    mem: Vec<u8>,
    start: usize,
}
impl ops::Deref for Demem {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        // SAFETY: The `start` field must always point to a valid start index
        unsafe { self.mem.get_unchecked(self.start..) }
    }
}
impl From<Vec<u8>> for Demem {
    fn from(v: Vec<u8>) -> Self {
        Self { mem: v, start: 0 }
    }
}
impl Demem {
    /// Add data to the left.
    fn extend_left(&mut self, data: &[u8]) {
        while data.len() > self.start {
            let old_len = self.mem.len();
            self.mem.reserve(old_len);
            // SAFETY: The copy is within the capacity of the vector, the offsets fit in an
            // `isize` because the memory reserve was successful, and all 2*old_len bytes are
            // now initialized.
            unsafe {
                ptr::copy_nonoverlapping(
                    self.mem.as_ptr(),
                    self.mem.as_mut_ptr().offset(old_len as isize),
                    old_len,
                );
                self.mem.set_len(2 * old_len);
            }
            self.start += old_len;
        }
        // SAFETY: The range is valid due to the previous `while` condition
        // There is no overlap because of Rust's aliasing rules
        // Offsets fit in an `isize` because they are valid allocated memory indices,
        // and therefore `Vec` checks them
        unsafe {
            ptr::copy_nonoverlapping(
                data.as_ptr(),
                self.mem
                    .as_mut_ptr()
                    .offset((self.start - data.len()) as isize),
                data.len(),
            );
        }
        self.start -= data.len();
    }

    /// Add data to the right.
    fn extend_right(&mut self, data: &[u8]) {
        self.mem.extend_from_slice(data);
    }

    /// Remove data from the left.
    fn consume_left(&mut self, count: usize) {
        assert!(
            count <= self.mem.len() - self.start,
            "consumed more than the length"
        );
        self.start += count;
    }

    /// Remove data from the right.
    fn consume_right(&mut self, count: usize) {
        assert!(
            count <= self.mem.len() - self.start,
            "consumed more than the length"
        );
        self.mem.truncate(self.mem.len() - count);
    }
}
impl fmt::Debug for Demem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Demem {{ spare_left: {}, memory: {}, spare_right: {} }}",
            self.start,
            self.mem.len() - self.start,
            self.mem.capacity() - self.mem.len()
        )
    }
}
