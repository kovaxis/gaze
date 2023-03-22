use ab_glyph::FontArc;

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

pub struct LoadedData {
    /// TODO: Keep a dense linemap and a sparse linemap, to be able to
    /// seek large files quickly but also find precise characters quickly.
    pub linemap: LineMap,
    pub data: SparseData,
    pub hot_offset: i64,
}

struct Shared {
    file_size: i64,
    stop: AtomicCell<bool>,
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
            let (l, r) = {
                let loaded = self.shared.loaded.lock();
                let start = Instant::now();
                // load data around the hot offset
                let hot_offset = loaded.hot_offset;
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
        let file_size: i64 = file
            .seek(io::SeekFrom::End(0))
            .context("failed to determine length of file")?
            .try_into()
            .context("file way too large")?; // can only fail for files larger than 2^63-1
        let max_linemap_memory = (file_size / 16).clamp(1024 * 1024, 128 * 1024 * 1024) as usize;
        let shared = Arc::new(Shared {
            file_size,
            stop: false.into(),
            linemapper: LineMapper::new(font, max_linemap_memory, file_size),
            loaded: LoadedData {
                data: SparseData::new(),
                linemap: LineMap::new(),
                hot_offset: 0,
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
        Ok(Self { manager, shared })
    }

    pub fn lock(&self) -> FileLock {
        FileLock::new(self)
    }

    pub fn file_size(&self) -> i64 {
        self.shared.file_size
    }

    pub fn advance_for(&self, c: char) -> f64 {
        self.shared.linemapper.advance_for(c)
    }
}

/// Lock the data that is shared with the manager thread.
/// The manager thread goes to
pub struct FileLock<'a> {
    pub filebuf: &'a FileBuffer,
    loaded: MutexGuard<'a, LoadedData>,
}
impl FileLock<'_> {
    fn new(buf: &FileBuffer) -> FileLock {
        FileLock {
            loaded: buf.shared.loaded.lock(),
            filebuf: buf,
        }
    }

    pub fn clamp_scroll(&mut self, scroll: &mut ScrollPos) {
        self.filebuf
            .shared
            .linemapper
            .clamp_pos(&self.loaded.linemap, scroll);
    }

    pub fn iter_lines(
        &mut self,
        pos: &ScrollPos,
        view_size: DVec2,
        mut on_char_or_line: impl FnMut(f64, i64, Option<char>),
    ) {
        let loaded = &mut *self.loaded;
        let y0 = pos.delta_y.floor() as i64;
        let y1 = (pos.delta_y + view_size.y).ceil() as i64;
        for y in y0..y1 {
            if let Some([lo, hi, base]) = loaded.linemap.scanline_to_anchors(
                pos.base_offset,
                y,
                (pos.delta_x, pos.delta_x + view_size.x),
            ) {
                let mut linedata = loaded.data.longest_prefix(lo.offset, hi.offset);
                // NOTE: This subtraction makes no sense if one is relative and the other is absolute
                let mut dx = lo.x_offset - base.x_offset;
                let mut dy = lo.y_offset - base.y_offset;
                // Remove excess data at the beggining of the line
                while !linedata.is_empty() && (dy < y || dy == y && dx < pos.delta_x) {
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
                            let hadv = self.filebuf.advance_for(c);
                            if dy == y && dx + hadv > pos.delta_x {
                                break;
                            }
                            dx += hadv;
                        }
                    }
                    linedata = &linedata[adv..];
                }
                // Process readable text
                on_char_or_line(dx, dy, None);
                while !linedata.is_empty() && dx < pos.delta_x + view_size.x {
                    let (c, adv) = decode_utf8(linedata);
                    match c.unwrap_or(LineMapper::REPLACEMENT_CHAR) {
                        '\n' => {
                            break;
                        }
                        c => {
                            on_char_or_line(dx, dy, Some(c));
                            dx += self.filebuf.advance_for(c);
                        }
                    }
                    linedata = &linedata[adv..];
                }
            }
        }
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
