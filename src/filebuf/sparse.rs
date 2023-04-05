use crate::{cfg::Cfg, prelude::*};

use super::{LoadedData, LoadedDataGuard, Surroundings};

#[derive(Debug)]
pub struct SparseSegment {
    pub offset: i64,
    pub data: Demem,
}

pub type SparseHandle<'a> = &'a Mutex<LoadedData>;
macro_rules! lock_sparse {
    ($handle:expr, $ref:ident) => {
        let mut $ref = LoadedDataGuard::lock($handle, file!(), line!());
        #[allow(unused_mut)]
        let mut $ref = &mut $ref.guard.data;
    };
    ($handle:expr, $lock:ident, $ref:ident) => {
        let mut $lock = LoadedDataGuard::lock($handle, file!(), line!());
        #[allow(unused_mut)]
        let mut $ref = &mut $lock.guard.data;
    };
    ($handle:expr, $lock:ident, $ref:ident => unlocked $code:block) => {{
        drop($ref);
        drop($lock);
        $code
        $lock = LoadedDataGuard::lock($handle, file!(), line!());
        $ref = &mut $lock.guard.data;
    }};
    ($handle:expr, $lock:ident, $ref:ident => bump) => {{
        drop($ref);
        $lock.bump(file!(), line!());
        $ref = &mut $lock.guard.data;
    }};
}

/// Holds sparse segments of data loaded from a potentially huge file.
pub struct SparseData {
    pub(super) segments: Vec<SparseSegment>,
    /// If set to another value, it should only increase!
    pub(super) file_size: i64,
    /// Start dropping far away data to keep memory usage under this amount.
    pub(super) max_loaded: usize,
    /// How many bytes to move from one segment to another in a single atomic lock.
    pub(super) merge_batch_size: usize,
    /// After a segment is this bytes long, start considering that growing a vector
    /// of this length could take way too long, and instead allocate a new copy
    /// while off the lock.
    pub(super) realloc_threshold: usize,
}
impl SparseData {
    pub fn new(max_loaded: usize, merge_batch_size: usize, realloc_threshold: usize) -> Self {
        Self {
            segments: default(),
            file_size: 0,
            max_loaded,
            merge_batch_size,
            realloc_threshold,
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

    /// If the given offset is contained in a segment, yield its left and right edges.
    /// If it's not, yield the inner edges of the surrounding segments.
    /// If there is no segment to a given side, yield the start/end of the file.
    pub fn find_surroundings(&self, offset: i64) -> Surroundings {
        let offset = offset.min(self.file_size - 1);
        for (i, s) in self.segments.iter().enumerate() {
            if s.offset + s.data.len() as i64 > offset {
                if s.offset <= offset {
                    // Offset is contained in this segment
                    return Surroundings::In(s.offset, s.offset + s.data.len() as i64);
                } else {
                    // This segment is the first segment after the given offset
                    let prev = match i {
                        0 => 0,
                        i => {
                            let p = &self.segments[i - 1];
                            p.offset + p.data.len() as i64
                        }
                    };
                    return Surroundings::Out(prev, s.offset);
                }
            }
        }
        let prev = self
            .segments
            .last()
            .map(|s| s.offset + s.data.len() as i64)
            .unwrap_or(0);
        Surroundings::Out(prev, self.file_size)
    }

    /// Inserts the given data into the given offset.
    /// If the given data overlaps with previous segments, the previous segments
    /// will be partially (or completely) discarded to make space for the new
    /// data.
    /// The data will be inserted as a fresh segment, without merging wit
    /// adjacent segments.
    /// `merge_segments` should be called afterwards to maintain the soft
    /// invariant that no segments are touching without being merged.
    fn insert_segment(&mut self, offset: i64, data: Vec<u8>) -> usize {
        let mut i = self.find_before(offset);
        let mut j = self.find_after(offset + data.len() as i64);

        if let Some(s) = self.segments.get_mut(i) {
            let overlap = s.offset + s.data.len() as i64 - offset;
            // Remove duplicate data
            if overlap > 0 {
                if overlap >= data.len() as i64 {
                    // The provided data is completely redundant
                    // In this case, just bail and keep the old data
                    return i;
                }
                s.data.consume_right(overlap as usize);
            }
            // Only remove segment if all data was overwritten
            if overlap < s.data.len() as i64 {
                i += 1;
            }
        } else {
            i = 0;
        }

        if let Some(s) = self.segments.get_mut(j) {
            let overlap = offset + data.len() as i64 - s.offset;
            // Remove duplicate data
            if overlap > 0 {
                s.offset += overlap;
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

    /// Merge two adjacent segments, assuming they touch right on the edges.
    /// Avoids locking the loaded data for long periods, even with huge segments.
    fn merge_segments(handle: SparseHandle, l_idx: usize, force_into_left: Option<bool>) {
        lock_sparse!(handle, store, sparse);
        fn get_two(sparse: &mut SparseData, i: usize) -> (&mut SparseSegment, &mut SparseSegment) {
            let (l, r) = sparse.segments.split_at_mut(i + 1);
            (l.last_mut().unwrap(), r.first_mut().unwrap())
        }
        // Determine whether it's cheaper to move into the left or right segments
        let into_left;
        let l_realloc;
        let r_realloc;
        let realloc_size;
        {
            let l = &sparse.segments[l_idx].data;
            let r = &sparse.segments[l_idx + 1].data;
            l_realloc =
                (l.capacity() + r.len()) >= sparse.realloc_threshold && l.spare_right() < r.len();
            r_realloc =
                (r.capacity() + l.len()) >= sparse.realloc_threshold && r.spare_left() < l.len();
            realloc_size = (l.len() + r.len()).next_power_of_two();
            into_left = match force_into_left {
                Some(il) => il,
                None => r.len() <= l.len(),
            };
        }
        // If we need to carry out a big reallocation, do it off the lock
        if into_left && l_realloc {
            // Create a segment with enough capacity for both datas
            let off = sparse.segments[l_idx].offset;
            let seg;
            lock_sparse!(handle, store, sparse => unlocked {
                seg = SparseSegment {
                    offset: off,
                    data: Demem::with_capacity(0, realloc_size),
                };
            });
            sparse.segments.insert(l_idx, seg);
            lock_sparse!(handle, store, sparse => unlocked {
                Self::merge_segments(handle, l_idx, Some(true));
            });
        } else if !into_left && r_realloc {
            // Create a segment with enough capacity for both, and merge right segment into it
            let off =
                sparse.segments[l_idx + 1].offset + sparse.segments[l_idx + 1].data.len() as i64;
            let seg;
            lock_sparse!(handle, store, sparse => unlocked {
                seg = SparseSegment {
                    offset: off,
                    data: Demem::with_capacity(realloc_size, 0),
                };
            });
            sparse.segments.insert(l_idx + 2, seg);
            lock_sparse!(handle, store, sparse => unlocked {
                Self::merge_segments(handle, l_idx+1, Some(false));
            });
        }
        // Copy data from one segment to the other
        // Make sure to bump the mutex regularly
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
        let empty = sparse
            .segments
            .remove(l_idx + if into_left { 1 } else { 0 });
        // Drop the segment data with the lock unheld
        // Turns out dropping large buffers takes a pretty long time
        drop(store);
        drop(empty);
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
                Self::merge_segments(handle, i-1, None);
            });
            i -= 1;
        }
        if i + 1 < sparse.segments.len()
            && sparse.segments[i].offset + sparse.segments[i].data.len() as i64
                == sparse.segments[i + 1].offset
        {
            drop(sparse);
            drop(store);
            Self::merge_segments(handle, i, None);
        }
    }

    /// Clean up memory if we are over the target memory usage.
    /// Only gurantees keeping the given offset range in memory.
    pub fn cleanup(k: &Cfg, handle: SparseHandle, keep: ops::Range<i64>) {
        lock_sparse!(handle, store, sparse);

        let mut total_cap = 0;
        for seg in sparse.segments.iter() {
            total_cap += seg.data.capacity();
        }
        if total_cap > sparse.max_loaded {
            let mut timing = TimingLog::new();

            let mut shrinked_by = 0;
            // Mark everything outside the keep range as freed
            // Don't actually free them with the lock held, though
            let mut free_later = vec![];
            sparse.segments.retain_mut(|s| {
                // Remove the prefix that is outside `keep`
                let lconsume = (keep.start - s.offset).clamp(0, s.data.len() as i64);
                s.data.consume_left(lconsume as usize);
                s.offset += lconsume;
                // Remove the suffix that is outside `keep`
                let rconsume =
                    ((s.offset + s.data.len() as i64) - keep.end).clamp(0, s.data.len() as i64);
                s.data.consume_right(rconsume as usize);
                // Drop the segment if it is empty
                if s.data.is_empty() {
                    shrinked_by += s.data.capacity();
                    free_later.push(mem::replace(&mut s.data, Demem::new()));
                }
                !s.data.is_empty()
            });

            timing.mark("clip-segments");

            // Free spare memory
            // This might require some mutex-bumping for latency
            let mut data_acc = 0;
            for i in 0..sparse.segments.len() {
                let s = &sparse.segments[i];
                shrinked_by += s.data.capacity() - s.data.len();
                if data_acc + s.data.capacity() >= sparse.realloc_threshold {
                    // Data is too large to relocate in one go
                    // First, create a new copy with the exact needed size
                    let size = s.data.len();
                    let mut new;
                    lock_sparse!(handle, store, sparse => unlocked {
                        new = Demem::with_capacity(0, size);
                    });
                    // Then, copy live data to this new location
                    loop {
                        let s = &sparse.segments[i];
                        let batch_size = sparse.merge_batch_size.min(size - new.len());
                        new.extend_right(&s.data[new.len()..new.len() + batch_size]);
                        if new.len() >= size {
                            break;
                        }
                        // Be mindful to keep bumping the mutex
                        lock_sparse!(handle, store, sparse => bump);
                    }
                    // Finally, replace the old container with the new tight container
                    // This will drop the old container and free its memory!
                    // (So do it off the lock)
                    free_later.push(mem::replace(&mut sparse.segments[i].data, new));
                    data_acc = 0;
                } else {
                    // Data is small enough to shrink in one go
                    data_acc += s.data.capacity();
                    sparse.segments[i].data.shrink_to_fit();
                }
            }

            timing.mark("shrink-to-fit");

            // Free buffers with the lock released
            if !free_later.is_empty() {
                lock_sparse!(handle, store, sparse => unlocked {
                    drop(free_later);
                });
            }

            timing.mark("free-buffers");

            // Report shrinkage
            use std::fmt::Write;
            let mut buf = String::new();
            let _ = writeln!(
                buf,
                "memory usage was at {:.3}/{:.3}MB, so {:.3}MB were freed, leaving these segments only:",
                total_cap as f64 / 1024. / 1024.,
                sparse.max_loaded as f64 / 1024. / 1024.,
                shrinked_by as f64 / 1024. / 1024.,
            );
            for s in sparse.segments.iter() {
                let _ = writeln!(buf, "  [{}, {})", s.offset, s.offset + s.data.len() as i64);
            }

            timing.mark("log-build");

            drop(store);
            print!("{}", buf);

            timing.mark("log-print");

            if k.log.mem_release {
                timing.log("mem-release");
            }
        }
    }

    /// Find the longest contiguous segment of data starting at `at`.
    pub fn longest_prefix(&self, starting_at: i64) -> &[u8] {
        for s in self.segments.iter().rev() {
            if s.offset <= starting_at {
                return &s.data[(starting_at - s.offset).min(s.data.len() as i64) as usize..];
            }
        }
        &[][..]
    }

    /// Find the longest contiguous segment of data ending at `at`.
    pub fn longest_suffix(&self, ending_at: i64) -> &[u8] {
        for s in self.segments.iter() {
            if s.offset + s.data.len() as i64 >= ending_at {
                return &s.data[..(ending_at - s.offset).max(0) as usize];
            }
        }
        &[][..]
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
    fn new() -> Self {
        Self {
            mem: Vec::new(),
            start: 0,
        }
    }

    fn with_capacity(lspare: usize, rspare: usize) -> Self {
        let mut mem = Vec::with_capacity(lspare + rspare);
        mem.resize(lspare, 0);
        Self { mem, start: lspare }
    }

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
        assert!(count <= self.len(), "consumed more than the length");
        self.start += count;
    }

    /// Remove data from the right.
    fn consume_right(&mut self, count: usize) {
        assert!(count <= self.len(), "consumed more than the length");
        self.mem.truncate(self.mem.len() - count);
    }

    /// Free any unused spare capacity.
    fn shrink_to_fit(&mut self) {
        if self.start > 0 {
            let len = self.len();
            self.mem.copy_within(self.start.., 0);
            self.start = 0;
            self.mem.truncate(len);
            self.mem.shrink_to_fit();
        }
    }

    fn spare_left(&self) -> usize {
        self.start
    }

    fn spare_right(&self) -> usize {
        self.mem.capacity() - self.mem.len()
    }

    fn capacity(&self) -> usize {
        self.mem.capacity()
    }
}
impl fmt::Debug for Demem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Demem {{ spare_left: {}, memory: {}, spare_right: {} }}",
            self.spare_left(),
            self.len(),
            self.spare_right(),
        )
    }
}
