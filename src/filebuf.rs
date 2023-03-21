use glyph_brush::ab_glyph::FontArc;

use crate::{
    filebuf::linemap::LineMap,
    filebuf::{linemap::decode_utf8, sparse::SparseData},
    prelude::*,
};

use self::linemap::LineMapper;

mod linemap;
mod sparse;

#[cfg(test)]
mod test;

/*
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
*/

pub struct LoadedData {
    pub linemap: LineMap,
    pub data: SparseData,
}

struct Shared {
    file_size: i64,
    stop: AtomicCell<bool>,
    lineloading: AtomicCell<bool>,
    hot_offset: AtomicCell<i64>,
    loaded: Mutex<LoadedData>,
    linemapper: LineMapper,
}

struct FileManager {
    shared: Arc<Shared>,
    file: File,
    read_size: usize,
    load_radius: i64,
}
impl FileManager {
    fn new(shared: Arc<Shared>, file: File) -> Self {
        Self {
            file,
            shared,
            read_size: 64 * 1024,
            load_radius: 1000,
        }
    }

    fn run(self) -> Result<()> {
        while !self.shared.stop.load() {
            let hot_offset = self.shared.hot_offset.load();
            let (l, r) = {
                let loaded = self.shared.loaded.lock();
                let start = Instant::now();
                // load data around the hot offset
                let read_size = self.read_size as i64;
                let lr = match loaded.data.find_segment(hot_offset) {
                    Ok(i) => {
                        // the hot offset itself is already loaded
                        // load either just before or just after the loaded segment
                        let lside = loaded.data[i].offset;
                        let rside = loaded.data[i].offset + loaded.data[i].data.len() as i64;
                        if hot_offset - lside < rside - hot_offset && lside > 0 {
                            // load left side
                            (
                                loaded
                                    .data
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
                                    .data
                                    .get(i + 1)
                                    .map(|s| s.offset)
                                    .unwrap_or(self.shared.file_size)
                                    .min(rside + read_size),
                            )
                        }
                    }
                    Err(i) => (
                        loaded
                            .data
                            .get(i.wrapping_sub(1))
                            .map(|s| s.offset + s.data.len() as i64)
                            .unwrap_or(0)
                            .max(hot_offset - read_size / 2),
                        loaded
                            .data
                            .get(i)
                            .map(|s| s.offset)
                            .unwrap_or(self.shared.file_size)
                            .min(hot_offset + read_size / 2),
                    ),
                };
                let segn = loaded.data.segments.len();
                drop(loaded);

                let lockt = start.elapsed();
                if lockt > Duration::from_millis(10) {
                    eprintln!(
                        "took {}ms to find segment to load within {} segments",
                        lockt.as_secs_f64() * 1000.,
                        segn,
                    );
                }
                lr
            };
            // Load the found segment
            if l < r {
                self.load_segment(l, (r - l) as usize)?;
                continue;
            }
            // Nothing to load, make sure to idle respectfully
            thread::park();
        }
        Ok(())
    }

    fn load_segment(&self, offset: i64, len: usize) -> Result<()> {
        let mut read_buf = vec![0; len];
        (&self.file).seek(io::SeekFrom::Start(offset as u64))?;
        (&self.file).read_exact(&mut read_buf)?;
        self.shared
            .linemapper
            .process_data(&self.shared.loaded, offset, &read_buf);
        SparseData::insert_data(&self.shared.loaded, offset, read_buf);

        if false {
            let loaded = self.shared.loaded.lock();
            eprintln!("loaded segment [{}, {})", offset, offset + len as i64);
            eprintln!("new sparse segments: {:?}", loaded.data);
            eprintln!("new linemap segments: {:?}", loaded.linemap);
        }
        Ok(())
    }
}

pub struct FileBuffer {
    manager: JoinHandle<Result<()>>,
    shared: Arc<Shared>,
    textbuf: Cell<String>,
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
    pub fn open(path: &Path, font: FontArc) -> Result<FileBuffer> {
        // TODO: Do not do file IO on the main thread
        // This requires the file size to be set to 0 for a while
        let mut file = File::open(path)?;
        let file_size = file
            .seek(io::SeekFrom::End(0))
            .context("failed to determine length of file")?
            .try_into()
            .context("file too large")?;
        let max_linemap_memory = 1024 * 1024;
        let shared = Arc::new(Shared {
            file_size,
            stop: false.into(),
            lineloading: true.into(),
            hot_offset: 0.into(),
            linemapper: LineMapper::new(font, max_linemap_memory, file_size),
            loaded: LoadedData {
                data: SparseData::new(file_size),
                linemap: LineMap::new(),
            }
            .into(),
        });
        let manager = {
            let shared = shared.clone();
            thread::spawn(move || {
                FileManager::new(shared, file).run()?;
                eprintln!("manager thread finishing");
                Ok(())
            })
        };
        Ok(Self {
            manager,
            shared,
            textbuf: default(),
        })
    }

    pub fn iter_lines(
        &self,
        base_offset: i64,
        min: DVec2,
        max: DVec2,
        mut f: impl FnMut(f64, i64, &str),
    ) {
        let mut textbuf = self.textbuf.take();
        let loaded = self.shared.loaded.lock();
        let y0 = min.y.floor() as i64;
        let y1 = max.y.ceil() as i64;
        for y in y0..y1 {
            if let Some([lo, hi, base]) =
                loaded
                    .linemap
                    .scanline_to_anchors(base_offset, y, (min.x, max.x))
            {
                let mut linedata = loaded.data.longest_prefix(lo.offset, hi.offset);
                // NOTE: This subtraction makes no sense if one is relative and the other is absolute
                let mut dx = lo.x_offset - base.x_offset;
                let mut dy = lo.y_offset - base.y_offset;
                // Remove excess data at the beggining of the line
                while !linedata.is_empty() && (dy < y || dy == y && dx < min.x) {
                    let (c, adv) = decode_utf8(linedata);
                    match c.unwrap_or(LineMapper::REPLACEMENT_CHAR) {
                        '\n' => {
                            if dy == y {
                                break;
                            }
                            dy += 1;
                            dx = 0.;
                        }
                        c => {
                            let hadv = self.shared.linemapper.advance_for(c);
                            if dy == y && dx + hadv > min.x {
                                break;
                            }
                            dx += hadv;
                        }
                    }
                    linedata = &linedata[adv..];
                }
                // Take readable text
                textbuf.clear();
                {
                    let mut dx_end = dx;
                    while !linedata.is_empty() && dx_end < max.x {
                        let (c, adv) = decode_utf8(linedata);
                        match c.unwrap_or(LineMapper::REPLACEMENT_CHAR) {
                            '\n' => {
                                break;
                            }
                            c => {
                                textbuf.push(c);
                                dx_end += self.shared.linemapper.advance_for(c);
                            }
                        }
                        linedata = &linedata[adv..];
                    }
                }
                f(dx, dy, &textbuf);
            }
        }
        self.textbuf.replace(textbuf);
    }

    pub fn set_hot_pos(&self, pos: &mut ScrollPos) {
        let old = self.shared.hot_offset.swap(pos.base_offset);
        if pos.base_offset != old {
            self.manager.thread().unpark();
        }
    }

    pub fn file_size(&self) -> i64 {
        self.shared.file_size
    }
}

/// Represents a position within the file, in such a way that as the file gets
/// loaded the scroll position stays as still as possible.
///
/// The scrolling position represented is the top-left corner of the screen.
pub struct ScrollPos {
    /// The base offset within the file used as a reference.
    /// Using this offset, the backend can load the file around this
    /// offset and build a local map of how the file looks like.
    /// Changes to this offset can be "seamless", like automatic rebasing
    /// of the offset, or can be "jagged", like forcefully scrolling to the
    /// end of the file.
    /// Moving the base offset across two separate segments in the line map
    /// is always a jagged movement, and should not be triggerable with smooth
    /// scrolling methods like the mouse wheel or dragging, only through the
    /// scroll bar.
    pub base_offset: i64,
    /// A difference in visual X position from the base offset, in standard font height units.
    pub delta_x: f64,
    /// A difference in visual Y position from the base offset, as a fractional number of lines.
    pub delta_y: f64,
}

type LoadedDataHandle<'a> = &'a Mutex<LoadedData>;

struct LoadedDataGuard<'a> {
    start: Instant,
    guard: MutexGuard<'a, LoadedData>,
}
impl Drop for LoadedDataGuard<'_> {
    fn drop(&mut self) {
        self.check_time();
    }
}
impl LoadedDataGuard<'_> {
    fn lock(handle: LoadedDataHandle) -> LoadedDataGuard {
        LoadedDataGuard {
            guard: handle.lock(),
            start: Instant::now(),
        }
    }

    fn bump(&mut self) {
        self.check_time();
        MutexGuard::bump(&mut self.guard);
        self.start = Instant::now();
    }

    fn check_time(&self) {
        let t = self.start.elapsed();
        if t > Duration::from_millis(5) {
            eprintln!(
                "operation locked common data for {}ms",
                t.as_secs_f64() * 1000.
            );
        }
    }
}
