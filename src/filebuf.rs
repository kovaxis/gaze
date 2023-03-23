use ab_glyph::FontArc;

use crate::{
    cfg::Cfg,
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
    pub hot: ScrollRect,
}
impl LoadedData {
    fn try_get_hot_range(&self) -> Option<(i64, i64, i64)> {
        // Get bounds
        let base = self.hot.corner.base_offset;
        let y0 = self.hot.corner.delta_y.floor() as i64;
        let ym = (self.hot.corner.delta_y + self.hot.size.y / 2.).floor() as i64;
        let y1 = (self.hot.corner.delta_y + self.hot.size.y).ceil() as i64;
        let x0 = self.hot.corner.delta_x;
        let x1 = self.hot.corner.delta_x + self.hot.size.x;
        let xm = (x0 + x1) / 2.;
        // Get offsets
        let l = self
            .linemap
            .scanline_to_anchors(base, y0, (x0, x0))
            .map(|a| a[0])?;
        let m = self
            .linemap
            .scanline_to_anchors(base, ym, (xm, xm))
            .map(|a| a[0])?;
        let r = self
            .linemap
            .scanline_to_anchors(base, y1, (x1, x1))
            .map(|a| a[0])?;
        Some((l.offset, m.offset, r.offset))
    }

    fn get_hot_range(&self) -> (i64, i64, i64) {
        let m = self.hot.corner.base_offset;
        self.try_get_hot_range().unwrap_or((m, m, m))
    }

    /// Get the next logical range to load, based on the hot scrollpos and the
    /// currently loaded data.
    /// May return invalid (negative) ranges if there is no more data to load.
    fn get_range_to_load(&self, max_len: i64, load_radius: i64) -> (i64, i64) {
        let (lscreen, m, rscreen) = self.get_hot_range();
        let lm = self.linemap.find_surroundings(m);
        let sd = self.data.find_surroundings(m);
        let guide = match (lm, sd) {
            (Err(lm), Err(sd)) => Err((lm.0.min(sd.0), lm.1.max(sd.1))),
            (Ok(lm), Ok(sd)) => Ok((lm.0.max(sd.0), lm.1.min(sd.1))),
            (Err(lm), Ok(_)) => Err(lm),
            (Ok(_), Err(sd)) => Err(sd),
        };
        let (l, r) = match guide {
            Ok((l, r)) => {
                if lscreen - l < r - rscreen && l > 0 || r >= self.linemap.file_size {
                    // Load left side
                    if l > lscreen - load_radius {
                        (l - max_len, l)
                    } else {
                        (l, l)
                    }
                } else {
                    // Load right side
                    if r <= rscreen + load_radius {
                        (r, r + max_len)
                    } else {
                        (r, r)
                    }
                }
            }
            Err((mut l, mut r)) => {
                // If [l, r) is too long, shorten it to max_len attempting to keep it centered
                if r - l > max_len {
                    if l > m - max_len / 2 {
                        r = l + max_len;
                    } else if r < m + (max_len + 1) / 2 {
                        l = r - max_len;
                    } else {
                        l = m - max_len / 2;
                        r = m + (max_len + 1) / 2;
                    }
                }
                (l, r)
            }
        };
        (l.max(0), r.min(self.linemap.file_size))
    }
}

struct Shared {
    file_size: i64,
    stop: AtomicCell<bool>,
    loaded: Mutex<LoadedData>,
    linemapper: LineMapper,
    k: Cfg,
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
            read_size: 512,
            load_radius: 1000,
        }
    }

    fn run(self) -> Result<()> {
        while !self.shared.stop.load() {
            let (l, r) = {
                let loaded = self.shared.loaded.lock();
                let start = Instant::now();
                // Load data around the hot offset
                let (l, r) = loaded.get_range_to_load(self.read_size as i64, self.load_radius);
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
                (l, r)
            };
            // Load segment
            if r - l >= 4 {
                eprintln!("loading segment [{}, {})", l, r);
                self.load_segment(l, (r - l) as usize)?;
                continue;
            }
            // Nothing to load, make sure to idle respectfully
            // The frontend will notify us if there is any relevant change
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

        if self.shared.k.log_segment_load {
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
    pub fn open(path: &Path, font: FontArc, k: Cfg) -> Result<FileBuffer> {
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
            k,
            linemapper: LineMapper::new(font, max_linemap_memory, file_size),
            loaded: LoadedData {
                data: SparseData::new(file_size),
                linemap: LineMap::new(file_size),
                hot: default(),
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
        view: ScrollRect,
        mut on_char_or_line: impl FnMut(f64, i64, Option<char>),
    ) {
        let loaded = &mut *self.loaded;
        let pos = view.corner;
        let y0 = pos.delta_y.floor() as i64;
        let y1 = (pos.delta_y + view.size.y).ceil() as i64;
        for y in y0..y1 {
            if let Some([lo, hi, base]) = loaded.linemap.scanline_to_anchors(
                pos.base_offset,
                y,
                (pos.delta_x, pos.delta_x + view.size.x),
            ) {
                let mut offset = lo.offset;
                let mut linedata = loaded.data.longest_prefix(offset, hi.offset);
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
                    offset += adv as i64;
                }
                // Process readable text
                on_char_or_line(dx, dy, None);
                while !linedata.is_empty() && dx < pos.delta_x + view.size.x {
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
        // Set hot area
        let prev = loaded.hot;
        loaded.hot = view;
        if prev != view {
            self.filebuf.manager.thread().unpark();
        }
    }
}

/// Represents a position within the file, in such a way that as the file gets
/// loaded the scroll position stays as still as possible.
///
/// Scrolling model:
/// There are 3 scrolling methods available to the user:
/// 1. Scrolling through the mouse wheel or dragging the screen.
///     This method is very smooth, and maps simply to modifying the scroll deltas.
///     This scrolling is clamped to the range of the loaded segment that contains
///     `base_offset`.
/// 2. Scrolling through the scroll bar.
///     This method can perform long scroll jumps, but is still considered "smooth"
///     in the sense that it can only jump within the currently loaded segment.
///     In fact, the beggining of scroll bar is mapped to the beggining of the current
///     segment, and the end of the scroll bar to the end of the current segment.
///     To maintain good UX, the area represented by the scroll bar may continuously
///     grow as more file is being loaded, but while the user drags the scroll handle
///     the scrollbar is frozen. The loaded area may continue to grow, but the scroll
///     bar will not reflect this until the user releases the scroll handle.
/// 3. Scrolling directly to an offset.
///     This is the roughest method to scroll, as it may exit the currently loaded
///     segment and start loading another segment.
///     This allows the user to jump to the end of the file or the middle of the file,
///     without even knowing wether the file is all a single line or thousands of lines.
///     This is similar to a "go to line" feature.
#[derive(Copy, Clone, PartialEq, Default)]
pub struct ScrollPos {
    /// A reference offset within the file.
    /// This offset is only modified when scrolling jaggedly (ie. jumping directly
    /// to a file offset).
    pub base_offset: i64,
    /// A difference in visual X position from the base offset, in standard font height units.
    pub delta_x: f64,
    /// A difference in visual Y position from the base offset, as a fractional number of lines.
    pub delta_y: f64,
}

#[derive(Copy, Clone, PartialEq, Default)]
pub struct ScrollRect {
    /// The top-left corner.
    pub corner: ScrollPos,
    /// The size of this view in line units.
    pub size: DVec2,
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
