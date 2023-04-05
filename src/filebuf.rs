use ab_glyph::{Font, FontArc};

use crate::{
    cfg::Cfg,
    filebuf::linemap::LineMap,
    filebuf::{linemap::decode_utf8, sparse::SparseData},
    prelude::*,
};

use self::linemap::{decode_utf8_rev, LineMapper};

mod linemap;
mod sparse;

#[cfg(test)]
mod test;

pub struct LoadedData {
    /// TODO: Keep a dense linemap and a sparse linemap, to be able to
    /// seek large files quickly but also find precise characters quickly.
    pub linemap: LineMap,
    pub data: SparseData,
    pub hot: FileRect,
    pub sel: Option<ops::Range<i64>>,
    pub pending_sel_copy: bool,
    pub warn_time: Option<Duration>,
}
impl LoadedData {
    fn new(
        max_loaded: usize,
        merge_batch_size: usize,
        realloc_threshold: usize,
        warn_time: Option<Duration>,
    ) -> Self {
        Self {
            linemap: LineMap::new(),
            data: SparseData::new(max_loaded, merge_batch_size, realloc_threshold),
            hot: default(),
            sel: None,
            pending_sel_copy: false,
            warn_time,
        }
    }

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
        let l = self.linemap.pos_to_anchor(base, y0, x0)?.1;
        let m = self.linemap.pos_to_anchor(base, ym, xm)?.1;
        let r = self.linemap.pos_to_anchor_upper(base, y1, x1)?.1;
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
    fn get_range_to_load(
        &self,
        max_len: i64,
        load_radius: i64,
        max_sel: i64,
    ) -> ((i64, i64), bool) {
        let (lscreen, m, rscreen) = self.get_hot_range();
        let lm = self.linemap.find_surroundings(m);
        let sd = self.data.find_surroundings(m);
        let guide = lm.min(sd);
        let (l, r) = self.surroundings_to_range(guide, (lscreen, m, rscreen), max_len);
        // Load the visible screen if not loaded yet
        if lscreen - r < load_radius && l - rscreen <= load_radius {
            return ((l, r), true);
        }
        // Finished loading the local range, now attempt to load the selected range
        if let Some(sel) = self.sel.as_ref() {
            if sel.end - sel.start <= max_sel {
                let l = match self.data.find_surroundings(sel.start) {
                    Surroundings::In(_l, r) => r,
                    Surroundings::Out(_, _) => sel.start,
                };
                if l < sel.end {
                    return ((l, sel.end.min(l + max_len)), true);
                }
            }
        }
        // Nothing more to load, proceed to linemap farther out
        let (l, r) = self.surroundings_to_range(lm, (lscreen, m, rscreen), max_len);
        ((l, r), false)
    }
}

#[derive(Clone, Copy, Debug)]
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
    path: PathBuf,
    friendly_name: String,
    stop: AtomicCell<bool>,
    sleeping: AtomicCell<bool>,
    last_file_size: AtomicCell<i64>,
    loaded: Mutex<LoadedData>,
    k: Cfg,
    layout: CharLayout,
}

struct FileManager {
    shared: Arc<Shared>,
    file: File,
    read_buf: Vec<u8>,
    linemapper: LineMapper,
}
impl FileManager {
    fn new(shared: Arc<Shared>) -> Result<Self> {
        let mut file = File::open(&shared.path)?;
        let file_size: i64 = file
            .seek(io::SeekFrom::End(0))
            .context("failed to determine length of file")?
            .try_into()
            .context("file way too large")?; // can only fail for files larger than 2^63-1
        shared.last_file_size.store(file_size);
        let memk = &shared.k.f.linemap_mem;
        let max_linemap_memory = ((file_size as f64 * memk.fract)
            .clamp(memk.min_mb * 1024. * 1024., memk.max_mb * 1024. * 1024.)
            as i64)
            .clamp(0, isize::MAX as i64) as usize;
        {
            let mut loaded = shared.loaded.lock();
            loaded.linemap.file_size = file_size;
            loaded.data.file_size = file_size;
        }
        Ok(Self {
            linemapper: LineMapper::new(
                shared.layout.clone(),
                file_size,
                max_linemap_memory,
                shared.k.f.migrate_batch_size,
            ),
            read_buf: default(),
            file,
            shared,
        })
    }

    fn run(mut self) -> Result<()> {
        while !self.shared.stop.load() {
            // Find something to do
            let keep;
            let ((l, r), store_data) = {
                let mut loaded = self.shared.loaded.lock();

                // Process clipboard copy operations
                if let (true, Some(sel)) = (loaded.pending_sel_copy, loaded.sel.as_ref()) {
                    let data = loaded.data.longest_prefix(sel.start);
                    if data.len() as i64 >= sel.end - sel.start {
                        let data = &data[..(sel.end - sel.start) as usize];
                        match set_clipboard(data) {
                            Ok(()) => println!("put {} bytes into clipboard", data.len()),
                            Err(err) => println!("error setting clipboard: {:#}", err),
                        }
                        loaded.pending_sel_copy = false;
                        MutexGuard::bump(&mut loaded);
                    }
                }

                let start = Instant::now();
                // Load data around the hot offset
                let load_radius = self.shared.k.f.load_radius as i64;
                let (keepl, _keepm, keepr) = loaded.get_hot_range();
                keep = keepl - load_radius..keepr + load_radius;
                let out = loaded.get_range_to_load(
                    self.shared.k.f.read_size as i64,
                    load_radius,
                    self.shared.k.f.max_selection_copy as i64,
                );
                let segn = loaded.data.segments.len();
                drop(loaded);

                let lockt = start.elapsed();
                if lockt > Duration::from_millis(10) {
                    println!(
                        "took {}ms to find segment to load within {} segments",
                        lockt.as_secs_f64() * 1000.,
                        segn,
                    );
                }
                out
            };
            // Load segment
            if l < r {
                if l % (16 * 1024 * 1024) > r % (16 * 1024 * 1024) {
                    eprintln!("loaded {:.2}MB", l as f64 / 1024. / 1024.);
                }
                self.load_segment(l, (r - l) as usize, keep, store_data)?;
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

    fn load_segment(
        &mut self,
        offset: i64,
        len: usize,
        keep: ops::Range<i64>,
        store_data: bool,
    ) -> Result<()> {
        let read_start = Instant::now();

        if self.read_buf.len() < len {
            self.read_buf.resize(len, 0);
        }
        (&self.file).seek(io::SeekFrom::Start(offset as u64))?;
        (&self.file).read_exact(&mut self.read_buf[..len])?;

        let lmap_start = Instant::now();
        self.linemapper
            .process_data(&self.shared.loaded, offset, &self.read_buf[..len]);

        let data_start = Instant::now();
        if store_data {
            let mut read_buf = mem::take(&mut self.read_buf);
            read_buf.truncate(len);
            SparseData::insert_data(&self.shared.loaded, offset, read_buf);
            SparseData::cleanup(&self.shared.k, &self.shared.loaded, keep);
        }

        let finish = Instant::now();

        if self.shared.k.log.segment_load {
            let loaded = self.shared.loaded.lock();
            println!("loaded segment [{}, {})", offset, offset + len as i64);
            if self.shared.k.log.segment_timing {
                println!("  timing:");
                println!(
                    "    io read: {:.2}ms",
                    (lmap_start - read_start).as_secs_f64() * 1000.
                );
                println!(
                    "    linemap store: {:.2}ms",
                    (data_start - lmap_start).as_secs_f64() * 1000.
                );
                println!(
                    "    data store: {:.2}ms",
                    (finish - data_start).as_secs_f64() * 1000.
                );
            }
            if self.shared.k.log.segment_details {
                println!("  new sparse segments:");
                for s in loaded.data.segments.iter() {
                    println!("    [{}, {})", s.offset, s.offset + s.data.len() as i64);
                }
                println!("  new linemap segments:");
                for s in loaded.linemap.segments.iter() {
                    let start = s.anchors.front().unwrap();
                    let end = s.anchors.back().unwrap();
                    println!(
                        "    [{}, {}) from {}:{} to {}:{}",
                        s.start,
                        s.end,
                        start.y(s),
                        start.x(s),
                        end.y(s),
                        end.x(s),
                    );
                    for a in s.anchors.iter() {
                        println!("      {} at {}:{}", a.offset, a.y(s), a.x(s));
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct CharLayout {
    char_adv: FxHashMap<u32, f32>,
    default_adv: f32,
}
impl CharLayout {
    pub fn new(font: &FontArc) -> Self {
        let font_h = font.height_unscaled();
        let mut char_adv: FxHashMap<u32, f32> = default();
        char_adv.reserve(font.glyph_count());
        for (glyph, c) in font.codepoint_ids() {
            char_adv.insert(c as u32, font.h_advance_unscaled(glyph) / font_h);
        }
        println!("got {} char -> hadvance mappings", char_adv.len());
        Self {
            default_adv: font.h_advance_unscaled(font.glyph_id('\0')) / font_h,
            char_adv,
        }
    }

    /// Get the horizontal advance distance for the given unicode codepoint.
    pub fn advance_for(&self, codepoint: u32) -> f64 {
        *self.char_adv.get(&codepoint).unwrap_or(&self.default_adv) as f64
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
    pub fn new(path: PathBuf, layout: CharLayout, k: Cfg) -> Result<FileBuffer> {
        let shared = Arc::new(Shared {
            friendly_name: path
                .file_name()
                .unwrap_or("?".as_ref())
                .to_string_lossy()
                .into_owned(),
            path,
            stop: false.into(),
            sleeping: false.into(),
            last_file_size: 0.into(),
            layout,
            loaded: Mutex::new(LoadedData::new(
                (k.f.max_loaded_mb * 1024. * 1024.).ceil() as usize,
                k.f.merge_batch_size,
                k.f.realloc_threshold,
                if k.log.lock_warn_ms < 0. {
                    None
                } else {
                    Some(Duration::from_secs_f64(k.log.lock_warn_ms / 1000.))
                },
            )),
            k,
        });
        // TOOD: Check and display errors on the frontend
        let manager = {
            let shared = shared.clone();
            thread::spawn(move || {
                FileManager::new(shared)?.run()?;
                println!("manager thread finishing");
                Ok(())
            })
        };
        Ok(Self { manager, shared })
    }

    pub fn lock(&self) -> FileLock {
        FileLock::new(self)
    }

    pub fn layout(&self) -> &CharLayout {
        &self.shared.layout
    }

    pub fn friendly_name(&self) -> &str {
        &self.shared.friendly_name
    }

    pub fn file_size(&self) -> i64 {
        self.shared.last_file_size.load()
    }
}

pub struct DataAt<'a> {
    /// Y position relative to the reference anchor.
    pub dy: i64,
    /// X position relative to the reference anchor.
    pub dx: f64,
    /// Absolute position of the data.
    pub offset: i64,
    /// As much data as it could be collected starting at `offset`.
    pub data: &'a [u8],
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

    /// Get the bounding rectangle of the loaded area around a given offset.
    pub fn bounding_rect(&mut self, around_offset: i64) -> FileRect {
        self.loaded.linemap.bounding_rect(around_offset)
    }

    /// Look up a file position (by line Y and fractional X coordinate) and map
    /// it to the last offset that is before or at the given position.
    ///
    /// `hdiv` specifies how to round fractional X positions to exact character boundaries.
    /// `hdiv = 1` rounds like `floor`, to the last boundary before `x`
    /// `hdiv = 0.5` rounds like `round`, to the closest boundary to `x`
    /// `hdiv = 0` rounds like `ceil`, to the first boundary after `x`
    pub fn lookup_pos(&self, base_offset: i64, y: i64, x: f64, hdiv: f64) -> Option<DataAt> {
        let loaded = &*self.loaded;
        let (base, lo) = loaded.linemap.pos_to_anchor(base_offset, y, x)?;
        let mut offset = lo.offset;
        let mut data = loaded.data.longest_prefix(offset);
        // NOTE: This subtraction makes no sense if one is relative and the other is absolute
        let mut dx = lo.x_offset - base.x_offset;
        let mut dy = lo.y_offset - base.y_offset;
        // Remove excess data before the target position
        while !data.is_empty() && (dy < y || dy == y && dx < x) {
            let (c, adv) = decode_utf8(data);
            match c.unwrap_or(LineMapper::REPLACEMENT_CHAR) {
                LineMapper::NEWLINE => {
                    if dy == y {
                        break;
                    }
                    dy += 1;
                    // TODO: This is broken for relative-x bases
                    // If the base was relative, reaching this point means bailing
                    // with a `None` result
                    dx = -base.x_offset;
                }
                c => {
                    let hadv = self.filebuf.layout().advance_for(c);
                    if dy == y && dx + hadv * hdiv > x {
                        break;
                    }
                    dx += hadv;
                }
            }
            data = &data[adv..];
            offset += adv as i64;
        }
        Some(DataAt {
            dy,
            dx,
            offset,
            data,
        })
    }

    /// Looks up the character at `precise_offset`, returning its position
    /// relative to `base_offset`.
    /// If `precise_offset` is in the middle of a character, returns the next
    /// character boundary.
    pub fn lookup_offset(&self, base_offset: i64, precise_offset: i64) -> Option<DataAt> {
        let loaded = &*self.loaded;
        let (base, anchor) = loaded
            .linemap
            .offset_to_anchor(base_offset, precise_offset)?;
        let mut offset = anchor.offset;
        let mut data = loaded.data.longest_prefix(offset);
        // Parse data before target position, accumulating x/y changes
        let mut dx = anchor.x_offset - base.x_offset;
        let mut dy = anchor.y_offset - base.y_offset;
        while !data.is_empty() && offset < precise_offset {
            let (c, adv) = decode_utf8(data);
            match c.unwrap_or(LineMapper::REPLACEMENT_CHAR) {
                LineMapper::NEWLINE => {
                    dy += 1;
                    // TODO: This is broken for relative-x bases
                    // If the base was relative, reaching this point means bailing
                    // with a `None` result
                    dx = -base.x_offset;
                }
                c => {
                    let hadv = self.filebuf.layout().advance_for(c);
                    dx += hadv;
                }
            }
            data = &data[adv..];
            offset += adv as i64;
        }
        Some(DataAt {
            dy,
            dx,
            offset,
            data,
        })
    }

    /// Iterate over all lines and characters contained in the given rectangle.
    pub fn visit_rect(
        &self,
        view: FileRect,
        mut on_char_or_line: impl FnMut(i64, f64, i64, Option<(u32, f64)>),
    ) {
        let y0 = view.corner.delta_y.floor() as i64;
        let y1 = (view.corner.delta_y + view.size.y).ceil() as i64;
        let x0 = view.corner.delta_x;
        let x1 = view.corner.delta_x + view.size.x;
        for y in y0..y1 {
            // Look up the start of this line
            let mut data = match self.lookup_pos(view.corner.base_offset, y, x0, 1.) {
                Some(d) => d,
                None => continue,
            };
            // Process readable text
            on_char_or_line(data.offset, data.dx, data.dy, None);
            while !data.data.is_empty() && (data.dy < y || data.dx < x1) {
                let (c, adv) = decode_utf8(data.data);
                match c.unwrap_or(LineMapper::REPLACEMENT_CHAR) {
                    LineMapper::NEWLINE => {
                        break;
                    }
                    c => {
                        let hadv = self.filebuf.layout().advance_for(c);
                        on_char_or_line(data.offset, data.dx, data.dy, Some((c, hadv)));
                        data.dx += hadv;
                    }
                }
                data.data = &data.data[adv..];
                data.offset += adv as i64;
            }
        }
    }

    pub fn set_hot_area(&mut self, area: FileRect, selection: Option<ops::Range<i64>>) {
        // Set hot area
        let loaded = &mut *self.loaded;
        let prev = (loaded.hot, loaded.sel.clone());
        loaded.hot = area;
        loaded.sel = selection.clone();
        if prev != (area, selection) {
            self.filebuf.manager.thread().unpark();
        }
    }

    pub fn is_backend_idle(&self) -> bool {
        // Let the frontend know whether the entire text is loaded or not
        self.filebuf.shared.sleeping.load()
    }

    /// Moves the given offset by a certain amount of characters.
    ///
    /// O(n) in the amount of characters due to UTF-8.
    /// May not have enough data to complete the offset.
    /// In this case, it fails but returns the farthest it could get.
    pub fn char_delta(&self, mut offset: i64, delta: i16) -> StdResult<i64, i64> {
        if delta < 0 {
            // Move backwards
            let mut data = self.loaded.data.longest_suffix(offset);
            for _ in 0..-delta {
                if data.is_empty() {
                    return Err(offset);
                }
                let (_c, rev) = decode_utf8_rev(data);
                data = &data[..data.len() - rev];
                offset -= rev as i64;
            }
        } else {
            // Move forwards
            let mut data = self.loaded.data.longest_prefix(offset);
            for _ in 0..delta {
                if data.is_empty() {
                    return Err(offset);
                }
                let (_c, adv) = decode_utf8(data);
                data = &data[adv..];
                offset += adv as i64;
            }
        }
        Ok(offset)
    }

    /// Request the backend to copy the selected text.
    pub fn copy_selection(&mut self) {
        self.loaded.pending_sel_copy = true;
        self.filebuf.manager.thread().unpark();
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
#[derive(Copy, Clone, PartialEq, Default, Debug)]
pub struct FilePos {
    /// A reference offset within the file.
    /// This offset is only modified when scrolling jaggedly (ie. jumping directly
    /// to a file offset).
    pub base_offset: i64,
    /// A difference in visual X position from the base offset, in standard font height units.
    pub delta_x: f64,
    /// A difference in visual Y position from the base offset, as a fractional number of lines.
    pub delta_y: f64,
}
impl FilePos {
    /// Returns the base offset, the Y position and X position.
    /// Floors the Y position (which is usually what you want to do).
    pub fn floor(&self) -> (i64, i64, f64) {
        (self.base_offset, self.delta_y.floor() as i64, self.delta_x)
    }

    pub fn offset(&self, off: DVec2) -> Self {
        Self {
            base_offset: self.base_offset,
            delta_x: self.delta_x + off.x,
            delta_y: self.delta_y + off.y,
        }
    }
}

/// Represents a rectangle of the file.
/// Does **NOT** represent a linear start-end range, it literally represents
/// a rectangle view into the file.
#[derive(Copy, Clone, PartialEq, Default, Debug)]
pub struct FileRect {
    /// The top-left corner.
    pub corner: FilePos,
    /// The size of this view in line units.
    pub size: DVec2,
}
impl FileRect {
    pub fn clamp_pos(&self, mut pos: FilePos) -> FilePos {
        pos.delta_y = pos
            .delta_y
            .clamp(self.corner.delta_y, self.corner.delta_y + self.size.y);
        pos.delta_x = pos
            .delta_x
            .clamp(self.corner.delta_x, self.corner.delta_x + self.size.x);
        pos
    }
}

type LoadedDataHandle<'a> = &'a Mutex<LoadedData>;

struct LoadedDataGuard<'a> {
    file: &'static str,
    line: u32,
    start: Instant,
    guard: MutexGuard<'a, LoadedData>,
}
impl Drop for LoadedDataGuard<'_> {
    fn drop(&mut self) {
        self.check_time();
    }
}
impl<'a> LoadedDataGuard<'a> {
    fn lock(handle: LoadedDataHandle<'a>, file: &'static str, line: u32) -> Self {
        LoadedDataGuard {
            file,
            line,
            guard: handle.lock(),
            start: Instant::now(),
        }
    }

    fn bump(&mut self, file: &'static str, line: u32) {
        self.check_time();
        MutexGuard::bump(&mut self.guard);
        self.start = Instant::now();
        self.file = file;
        self.line = line;
    }

    fn check_time(&self) {
        if let Some(maxt) = self.guard.warn_time {
            let t = self.start.elapsed();
            if t > maxt {
                println!(
                    "WARNING: locked common data for {:.3}ms at {}:{}",
                    t.as_secs_f64() * 1000.,
                    self.file,
                    self.line,
                );
            }
        }
    }
}

fn set_clipboard(data: &[u8]) -> Result<()> {
    let text = std::str::from_utf8(data).context("invalid utf-8 data")?;
    gl::clipboard::set(text).map_err(|e| anyhow!("{}", e))?;
    Ok(())
}
