use crate::prelude::*;

/// Indicates that the `line`th line (0-based) starts at file offset `offset`.
#[derive(Clone, Copy)]
pub struct LineMapping {
    pub line: i64,
    pub offset: i64,
}

pub struct LineMap {
    pub anchors: Vec<LineMapping>,
    pub file_size: i64,
}
impl LineMap {
    fn new(file_size: i64) -> Self {
        Self {
            anchors: default(),
            file_size,
        }
    }

    pub fn find_lower(&self, line: i64) -> usize {
        self.anchors
            .partition_point(|m| m.line <= line)
            .saturating_sub(1)
    }

    /// Map a line number to the last known byte offset that is
    /// at or before the start of this line.
    pub fn map_lower_bound(&self, line: i64) -> LineMapping {
        self.anchors
            .get(self.find_lower(line))
            .copied()
            .unwrap_or(LineMapping { line: 0, offset: 0 })
    }

    pub fn find_upper(&self, line: i64) -> usize {
        self.anchors.partition_point(|m| m.line < line)
    }

    /// Map a line number to the first known byte offset that is
    /// at or after the start of this line.
    ///
    /// Might return the end of the file if the line number is beyond the
    /// currently loaded lines.
    pub fn map_upper_bound(&self, line: i64) -> LineMapping {
        self.anchors
            .get(self.find_upper(line))
            .copied()
            .unwrap_or(LineMapping {
                line,
                offset: self.file_size,
            })
    }

    pub fn map_approx(&self, line: i64) -> i64 {
        let lo = self.map_lower_bound(line);
        let hi = self.map_upper_bound(line);
        if lo.line == hi.line {
            return (lo.offset + hi.offset) / 2;
        }
        let x = (line - lo.line) as f64 / (hi.line - lo.line) as f64;
        lo.offset + ((hi.offset - lo.offset) as f64 * x) as i64
    }

    pub fn iter(&self, lo: i64, hi: i64) -> LineMappingIter {
        let i = self.find_lower(lo);
        LineMappingIter { lines: self, i, hi }
    }
}

pub struct LineMappingIter<'a> {
    lines: &'a LineMap,
    i: usize,
    hi: i64,
}
impl<'a> Iterator for LineMappingIter<'a> {
    type Item = ops::Range<LineMapping>;
    fn next(&mut self) -> Option<ops::Range<LineMapping>> {
        let cur = *self.lines.anchors.get(self.i)?;
        if cur.line >= self.hi {
            return None;
        }
        self.i += 1;
        let nxt = self
            .lines
            .anchors
            .get(self.i)
            .copied()
            .unwrap_or(LineMapping {
                line: self.hi,
                offset: self.lines.file_size,
            });
        Some(cur..nxt)
    }
}

struct SparseSegment {
    offset: i64,
    data: Vec<u8>,
}

pub struct SparseData {
    segments: Vec<SparseSegment>,
    file_size: i64,
}
impl SparseData {
    fn new(file_size: i64) -> Self {
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
    fn find_segment(&self, offset: i64) -> StdResult<usize, usize> {
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

    fn insert_segment(&mut self, mut offset: i64, mut data: Vec<u8>) {
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

    /// Iterate over all available segments of data in the range `[lo, hi)`.
    pub fn iter(&self, mut lo: i64, hi: i64) -> SparseIter {
        let mut i = self
            .segments
            .partition_point(|s| (s.offset + s.data.len() as i64) <= lo);
        let mut next_data = &[][..];
        if let Some(s) = self.segments.get(i) {
            if s.offset > lo && s.offset < hi {
                lo = s.offset;
            }
            if s.offset <= lo {
                // this segment contains a prefix!
                next_data = &s.data
                    [(lo - s.offset) as usize..(hi - s.offset).min(s.data.len() as i64) as usize];
                i += 1;
            }
        }
        SparseIter {
            data: self,
            cur_offset: lo,
            cur_data: next_data,
            next_seg: i,
            hi,
        }
    }
}

pub struct SparseIter<'a> {
    data: &'a SparseData,
    cur_offset: i64,
    cur_data: &'a [u8],
    next_seg: usize,
    hi: i64,
}
impl<'a> Iterator for SparseIter<'a> {
    type Item = (i64, &'a [u8]);
    fn next(&mut self) -> Option<(i64, &'a [u8])> {
        if self.cur_data.is_empty() {
            return None;
        }
        let out = (self.cur_offset, self.cur_data);
        self.cur_data = &[][..];
        if let Some(s) = self.data.segments.get(self.next_seg) {
            if s.offset < self.hi {
                self.cur_offset = s.offset;
                self.cur_data = &s.data[..(self.hi - s.offset).min(s.data.len() as i64) as usize];
                self.next_seg += 1;
            }
        }
        Some(out)
    }
}

struct Shared {
    file_size: i64,
    read_size: usize,
    stop: AtomicCell<bool>,
    lineloading: AtomicCell<bool>,
    hot_line: AtomicCell<i64>,
    linemap: Mutex<LineMap>,
    loaded: Mutex<SparseData>,
}

fn run_lineloader(mut file: File, shared: Arc<Shared>) -> Result<()> {
    let start = Instant::now();
    let mut buf = vec![0; 64 * 1024];
    let mut offset = 0;
    let mut linenum = 0;
    shared
        .linemap
        .lockf()
        .anchors
        .push(LineMapping { line: 0, offset: 0 });
    loop {
        let count = file.read(&mut buf)?;
        if count == 0 {
            break;
        }
        if shared.stop.load() {
            break;
        }
        let mut linemap = shared.linemap.lockf();
        if shared.stop.load() {
            break;
        }
        for (i, &b) in buf.iter().enumerate().take(count) {
            if b == b'\n' {
                linenum += 1;
                linemap.anchors.push(LineMapping {
                    line: linenum,
                    offset: offset + i as i64 + 1,
                });
            }
        }
        offset += count as i64;
    }
    shared.linemap.lockf().anchors.push(LineMapping {
        line: linenum + 1,
        offset,
    });
    shared.lineloading.store(false);
    eprintln!(
        "finished lineloading file in {:2}s",
        start.elapsed().as_secs_f64()
    );
    Ok(())
}

struct FileManager {
    shared: Arc<Shared>,
    file: File,
    lineloader: JoinHandle<Result<()>>,
}
impl FileManager {
    fn new(shared: Arc<Shared>, file: [File; 2]) -> Self {
        let [file, file2] = file;
        let lineloader = {
            let shared = shared.clone();
            thread::spawn(move || run_lineloader(file2, shared))
        };
        Self {
            shared,
            file,
            lineloader,
        }
    }

    fn run(self) -> Result<()> {
        while !self.shared.stop.load() {
            let hot_line = self.shared.hot_line.load();
            // map hot line to hot offset
            let hot_offset = self.shared.linemap.lockf().map_approx(hot_line);
            // load data around the hot offset
            {
                let mut loaded = self.shared.loaded.lockf();
                let read_size = self.shared.read_size as i64;
                let (l, r) = match loaded.find_segment(hot_offset) {
                    Ok(i) => {
                        // the hot offset itself is already loaded
                        // load either just before or just after the loaded segment
                        let lside = loaded.segments[i].offset;
                        let rside =
                            loaded.segments[i].offset + loaded.segments[i].data.len() as i64;
                        if hot_offset - lside < rside - hot_offset {
                            // load left side
                            (
                                loaded
                                    .segments
                                    .get(i.wrapping_sub(1))
                                    .map(|s| s.offset + s.data.len() as i64)
                                    .unwrap_or(0)
                                    .max(lside - read_size),
                                lside,
                            )
                        } else {
                            // load right side
                            (
                                rside,
                                loaded
                                    .segments
                                    .get(i + 1)
                                    .map(|s| s.offset)
                                    .unwrap_or(self.shared.file_size)
                                    .min(rside + read_size),
                            )
                        }
                    }
                    Err(i) => (
                        loaded
                            .segments
                            .get(i.wrapping_sub(1))
                            .map(|s| s.offset + s.data.len() as i64)
                            .unwrap_or(0)
                            .max(hot_offset - read_size / 2),
                        loaded
                            .segments
                            .get(i)
                            .map(|s| s.offset)
                            .unwrap_or(self.shared.file_size)
                            .min(hot_offset + read_size / 2),
                    ),
                };
                if l < r {
                    self.load_segment(&mut loaded, l, (r - l) as usize)?;
                    continue;
                }
            }
            // nothing to load, make sure to idle respectfully
            thread::park();
        }
        self.lineloader
            .join()
            .expect("lineloader thread panicked")?;
        Ok(())
    }

    fn load_segment(&self, loaded: &mut SparseData, offset: i64, len: usize) -> Result<()> {
        let mut read_buf = vec![0; len];
        (&self.file).seek(io::SeekFrom::Start(offset as u64))?;
        (&self.file).read_exact(&mut read_buf)?;
        loaded.insert_segment(offset, read_buf);

        eprintln!("loaded segment [{}, {})", offset, offset + len as i64);
        eprint!("new segments:");
        for seg in loaded.segments.iter() {
            eprint!(" [{}, {})", seg.offset, seg.offset + seg.data.len() as i64);
        }
        eprintln!();

        Ok(())
    }
}

pub struct FileBuffer {
    manager: JoinHandle<Result<()>>,
    shared: Arc<Shared>,
}
impl Drop for FileBuffer {
    fn drop(&mut self) {
        self.shared.stop.store(true);
        self.manager.thread().unpark();
        // do not join the thread, remember we want to avoid
        // blocking operations like the plague!
        // the manager thread might be busy for a while dropping data
    }
}
impl FileBuffer {
    pub fn open(path: &Path) -> Result<FileBuffer> {
        // TODO: Do not do file IO on the main thread
        // This requires the file size to be set to 0 for a while
        let mut file = File::open(path)?;
        let file_size = file
            .seek(io::SeekFrom::End(0))
            .context("failed to determine length of file")?
            .try_into()
            .context("file too large")?;
        let file2 = File::open(path).context("failed to reopen file for parallel lineloading")?;
        let shared = Arc::new(Shared {
            file_size,
            read_size: 64 * 1024,
            stop: false.into(),
            lineloading: true.into(),
            hot_line: 0.into(),
            linemap: LineMap::new(file_size).into(),
            loaded: SparseData::new(file_size).into(),
        });
        let manager = {
            let shared = shared.clone();
            thread::spawn(move || {
                FileManager::new(shared, [file, file2]).run()?;
                eprintln!("manager thread finishing");
                Ok(())
            })
        };
        Ok(Self { manager, shared })
    }

    pub fn access_data(&self, f: impl FnOnce(&LineMap, &SparseData)) {
        let linemap = self.shared.linemap.lockf();
        let data = self.shared.loaded.lockf();
        f(&*linemap, &*data)
    }

    pub fn set_hot_line(&self, hot_line: i64) {
        let old = self.shared.hot_line.swap(hot_line);
        if hot_line != old {
            self.manager.thread().unpark();
        }
    }

    pub fn file_size(&self) -> i64 {
        self.shared.file_size
    }
}
