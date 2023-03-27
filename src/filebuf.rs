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

    fn surroundings_to_range(
        &self,
        s: Surroundings,
        (lscreen, m, rscreen): (i64, i64, i64),
        max_len: i64,
    ) -> (i64, i64) {
        let (l, r) = match s {
            Surroundings::In(l, r) => {
                if lscreen - l < r - rscreen && l > 0 || r >= self.linemap.file_size {
                    // Load left side
                    (l - max_len, l)
                } else {
                    // Load right side
                    (r, r + max_len)
                }
            }
            Surroundings::Out(mut l, mut r) => {
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

    /// Get the next logical range to load, based on the hot scrollpos and the
    /// currently loaded data.
    /// May return invalid (negative) ranges if there is no more data to load.
    /// Also returns a boolean indicating if the range is "close" and its data
    /// should be stored (as opposed to "far away" ranges that should only be
    /// linemapped but not loaded).
    fn get_range_to_load(&self, max_len: i64, load_radius: i64) -> ((i64, i64), bool) {
        let (lscreen, m, rscreen) = self.get_hot_range();
        let lm = self.linemap.find_surroundings(m);
        let sd = self.data.find_surroundings(m);
        let guide = lm.min(sd);
        let (l, r) = self.surroundings_to_range(guide, (lscreen, m, rscreen), max_len);
        if lscreen - r >= load_radius || l - rscreen > load_radius {
            // Nothing more to load, proceed to linemap farther out
            let (l, r) = self.surroundings_to_range(lm, (lscreen, m, rscreen), max_len);
            ((l, r), false)
        } else {
            ((l, r), true)
        }
    }
}

#[derive(Clone, Copy)]
pub enum Surroundings {
    In(i64, i64),
    Out(i64, i64),
}
impl Surroundings {
    fn min(self, b: Surroundings) -> Surroundings {
        use Surroundings::*;
        match (self, b) {
            (Out(al, ar), Out(bl, br)) => Out(al.min(bl), ar.max(br)),
            (In(al, ar), In(bl, br)) => In(al.max(bl), ar.min(br)),
            (Out(l, r), In(..)) => Out(l, r),
            (In(..), Out(l, r)) => Out(l, r),
        }
    }
}

struct Shared {
    file_size: i64,
    stop: AtomicCell<bool>,
    sleeping: AtomicCell<bool>,
    loaded: Mutex<LoadedData>,
    linemapper: LineMapper,
    k: Cfg,
}

struct FileManager {
    shared: Arc<Shared>,
    file: File,
    read_buf: Vec<u8>,
}
impl FileManager {
    fn new(shared: Arc<Shared>, file: File) -> Self {
        Self {
            read_buf: default(),
            file,
            shared,
        }
    }

    fn run(mut self) -> Result<()> {
        while !self.shared.stop.load() {
            let ((l, r), store_data) = {
                let loaded = self.shared.loaded.lock();
                let start = Instant::now();
                // Load data around the hot offset
                let out = loaded.get_range_to_load(
                    self.shared.k.f.read_size as i64,
                    self.shared.k.f.load_radius as i64,
                );
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
                out
            };
            // Load segment
            if r - l >= 4 || self.shared.file_size < 4 {
                if l % (16 * 1024 * 1024) > r % (16 * 1024 * 1024) {
                    eprintln!("loaded {:.2}MB", l as f64 / 1024. / 1024.);
                }
                // eprintln!("loading segment [{}, {})", l, r);
                self.load_segment(l, (r - l) as usize, store_data)?;
                continue;
            }
            // Nothing to load, make sure to idle respectfully
            // The frontend will notify us if there is any relevant change
            self.shared.sleeping.store(true);
            thread::park();
            self.shared.sleeping.store(false);
        }
        Ok(())
    }

    fn load_segment(&mut self, offset: i64, len: usize, store_data: bool) -> Result<()> {
        let read_start = Instant::now();

        if self.read_buf.len() < len {
            self.read_buf.resize(len, 0);
        }
        (&self.file).seek(io::SeekFrom::Start(offset as u64))?;
        (&self.file).read_exact(&mut self.read_buf[..len])?;

        let lmap_start = Instant::now();
        self.shared
            .linemapper
            .process_data(&self.shared.loaded, offset, &self.read_buf);

        let data_start = Instant::now();
        if store_data {
            let read_buf = mem::take(&mut self.read_buf);
            SparseData::insert_data(&self.shared.loaded, offset, read_buf);
        }

        let finish = Instant::now();

        if self.shared.k.log.segment_load {
            let loaded = self.shared.loaded.lock();
            eprintln!("loaded segment [{}, {})", offset, offset + len as i64);
            if self.shared.k.log.segment_timing {
                eprintln!("  timing:");
                eprintln!(
                    "    io read: {:.2}ms",
                    (lmap_start - read_start).as_secs_f64() * 1000.
                );
                eprintln!(
                    "    linemap store: {:.2}ms",
                    (data_start - lmap_start).as_secs_f64() * 1000.
                );
                eprintln!(
                    "    data store: {:.2}ms",
                    (finish - data_start).as_secs_f64() * 1000.
                );
            }
            if self.shared.k.log.segment_details {
                eprintln!("  new sparse segments: {:?}", loaded.data);
                eprintln!("  new linemap segments: {:?}", loaded.linemap);
            }
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
        let memk = &k.f.linemap_mem;
        let max_linemap_memory = ((file_size as f64 * memk.fract)
            .clamp(memk.min_mb * 1024. * 1024., memk.max_mb * 1024. * 1024.)
            as i64)
            .clamp(0, isize::MAX as i64) as usize;
        let shared = Arc::new(Shared {
            file_size,
            stop: false.into(),
            sleeping: false.into(),
            linemapper: LineMapper::new(
                font,
                file_size,
                max_linemap_memory,
                k.f.migrate_batch_size,
            ),
            loaded: LoadedData {
                data: SparseData::new(file_size),
                linemap: LineMap::new(file_size),
                hot: default(),
            }
            .into(),
            k,
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

    pub fn clamp_scroll(&mut self, scroll: &mut ScrollPos) -> ScrollRect {
        let rect = self
            .filebuf
            .shared
            .linemapper
            .bounding_rect(&self.loaded.linemap, *scroll);
        scroll.delta_y = scroll
            .delta_y
            .clamp(rect.corner.delta_y, rect.corner.delta_y + rect.size.y);
        scroll.delta_x = scroll
            .delta_x
            .clamp(rect.corner.delta_x, rect.corner.delta_x + rect.size.x);
        rect
    }

    /// Iterate over all lines and characters contained in the given rectangle.
    /// Returns whether the backend is idle or not.
    pub fn visit_rect(
        &mut self,
        view: ScrollRect,
        mut on_char_or_line: impl FnMut(f64, i64, Option<char>),
    ) -> bool {
        let loaded = &mut *self.loaded;
        let y0 = view.corner.delta_y.floor() as i64;
        let y1 = (view.corner.delta_y + view.size.y).ceil() as i64;
        let x0 = view.corner.delta_x;
        let x1 = view.corner.delta_x + view.size.x;
        for y in y0..y1 {
            if let Some([lo, hi, base]) =
                loaded
                    .linemap
                    .scanline_to_anchors(view.corner.base_offset, y, (x0, x1))
            {
                let mut offset = lo.offset;
                let mut linedata = loaded.data.longest_prefix(offset, hi.offset);
                // NOTE: This subtraction makes no sense if one is relative and the other is absolute
                let mut dx = lo.x_offset - base.x_offset;
                let mut dy = lo.y_offset - base.y_offset;
                // Remove excess data at the beggining of the line
                while !linedata.is_empty() && (dy < y || dy == y && dx < x0) {
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
                            if dy == y && dx + hadv > x0 {
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
                while !linedata.is_empty() && (dy < y || dx < x1) {
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
        // Let the frontend know whether the entire text is loaded or not
        self.filebuf.shared.sleeping.load()
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
