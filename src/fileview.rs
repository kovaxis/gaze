//! Handles user input and drawing of a file into a rectangle of the screen.

use gl::winit::event::MouseScrollDelta;

use crate::{
    cfg::Cfg,
    elem2bool,
    filebuf::{CharLayout, FileLock, FilePos, FileRect},
    mouse2id,
    prelude::*,
    ScreenRect, WindowState,
};

pub mod drawing;

#[derive(Default)]
struct ScrollManager {
    pos: FilePos,
    last_view: FileRect,
    last_bounds: FileRect,
}
impl ScrollManager {
    /// Check whether to draw the vertical scrollbar.
    fn ydraw(&self, _k: &Cfg) -> bool {
        self.hcoef() < 1.
    }

    /// Check whether to draw the horizontal scrollbar.
    fn xdraw(&self, _k: &Cfg) -> bool {
        self.wcoef() < 1.
    }

    /// Compute a float value between 0 and 1 indicating where along
    /// the file is the current vertical scroll
    fn ycoef(&self) -> f32 {
        let mut ycoef =
            (self.pos.delta_y - self.last_bounds.corner.delta_y) / self.last_bounds.size.y;
        if ycoef.is_nan() || ycoef < 0. {
            ycoef = 0.;
        } else if ycoef > 1. {
            ycoef = 1.;
        }
        ycoef as f32
    }

    /// Compute a float representing the fraction of the file that the screen takes up.
    /// Note that the scrollhandle may be larger if a lower limit is reached.
    fn hcoef(&self) -> f32 {
        let mut hcoef = self.last_view.size.y / self.last_bounds.size.y;
        if hcoef.is_nan() || hcoef > 1. {
            hcoef = 1.;
        }
        hcoef as f32
    }

    /// Get the scrollbar rect as origin and size.
    fn y_scrollbar_bounds(&self, k: &Cfg, view: ScreenRect) -> ScreenRect {
        let gap = if self.xdraw(k) {
            k.g.scrollbar_width
        } else {
            0.
        };
        ScreenRect {
            min: vec2(view.max.x - k.g.scrollbar_width, view.min.y),
            max: vec2(view.max.x, view.max.y - gap),
        }
    }

    /// Get the scrollhandle rect as origin and size.
    fn y_scrollhandle_bounds(&self, k: &Cfg, view: ScreenRect) -> ScreenRect {
        let b = self.y_scrollbar_bounds(k, view);
        let sh = (self.hcoef() as f32 * b.size().y).max(k.g.scrollhandle_min_size);
        let sy = self.ycoef() as f32 * (b.size().y - sh);
        ScreenRect {
            min: vec2(b.min.x, b.min.y + sy),
            max: vec2(b.max.x, b.min.y + sy + sh),
        }
    }

    /// Compute a float value between 0 and 1 indicating where along
    /// the file is the current horizontal scroll
    fn xcoef(&self) -> f32 {
        let mut xcoef =
            (self.pos.delta_x - self.last_bounds.corner.delta_x) / self.last_bounds.size.x;
        if xcoef.is_nan() || xcoef < 0. {
            xcoef = 0.;
        } else if xcoef > 1. {
            xcoef = 1.;
        }
        xcoef as f32
    }

    /// Compute a float representing the fraction of the file that the screen takes up.
    /// Note that the scrollhandle may be larger if a lower limit is reached.
    fn wcoef(&self) -> f32 {
        let mut wcoef = self.last_view.size.x / self.last_bounds.size.x;
        if wcoef.is_nan() || wcoef > 1. {
            wcoef = 1.;
        }
        wcoef as f32
    }

    /// Get the scrollbar rect as origin and size.
    fn x_scrollbar_bounds(&self, k: &Cfg, view: ScreenRect) -> ScreenRect {
        let gap = if self.ydraw(k) {
            k.g.scrollbar_width
        } else {
            0.
        };
        ScreenRect {
            min: vec2(view.min.x, view.max.y - k.g.scrollbar_width),
            max: vec2(view.max.x - gap, view.max.y),
        }
    }

    /// Get the scrollhandle rect as origin and size.
    fn x_scrollhandle_bounds(&self, k: &Cfg, view: ScreenRect) -> ScreenRect {
        let b = self.x_scrollbar_bounds(k, view);
        let sw = (self.wcoef() as f32 * b.size().x).max(k.g.scrollhandle_min_size);
        let sx = self.xcoef() as f32 * (b.size().x - sw);
        ScreenRect {
            min: vec2(b.min.x + sx, b.min.y),
            max: vec2(b.min.x + sx + sw, b.max.y),
        }
    }

    /// Convert a mouse cursor position to a file position, based on the last scroll and other factors.
    pub fn screen_to_file_pos(&self, k: &Cfg, view: ScreenRect, pos: Vec2) -> FilePos {
        let text_view = FileView::text_view(k, view);
        let d = (pos - vec2(0., k.g.selection_offset) - text_view.min) / k.g.font_height;
        self.last_view.corner.offset(d.as_dvec2())
    }
}

pub struct Selected {
    /// The first selected offset.
    pub first: i64,
    /// The second selected offset.
    /// Note that this position might be before the first position,
    /// the "second" is chronological, not physical.
    pub second: i64,
    /// The last modification time of the selection.
    /// Used for cursor blink.
    pub mod_time: Instant,
    /// Non-authoritative physical position of the selection endpoints.
    pub last_positions: [Option<FilePos>; 2],
}
impl Selected {
    /// Mark the selection as modified.
    /// This resets the cursor blink modifier.
    fn touch(&mut self) {
        self.mod_time = Instant::now();
    }

    /// Returns whether the cursor is visible right now, and the next
    /// instant at which its visibility will change if it is not modified.
    fn check_blink(&self, k: &Cfg) -> (bool, Instant) {
        let half = k.g.cursor_blink;
        let time = self.mod_time.elapsed().as_secs_f64() / half;
        let visible = (time * 0.5).fract() < 0.5;
        let next = Instant::now() + Duration::from_secs_f64((time.ceil() - time) * half);
        (visible, next)
    }
}

enum Drag {
    None,
    ScrollbarY {
        cut: f32,
    },
    ScrollbarX {
        cut: f32,
    },
    Slide {
        screen_base: Vec2,
        last_update: Cell<Instant>,
    },
    Grab {
        screen_base: Vec2,
        scroll_base: FilePos,
    },
}
impl Drag {
    fn is_none(&self) -> bool {
        match self {
            Drag::None => true,
            _ => false,
        }
    }

    fn is_scrollbar(&self) -> bool {
        match self {
            Drag::ScrollbarX { .. } | Drag::ScrollbarY { .. } => true,
            _ => false,
        }
    }

    fn requires_refresh(&self) -> bool {
        match self {
            Drag::Slide { .. } => true,
            _ => false,
        }
    }

    fn hold(&self, k: &Cfg) -> bool {
        match self {
            Drag::ScrollbarX { .. } | Drag::ScrollbarY { .. } => k.ui.scrollbar_button.hold,
            Drag::Grab { .. } => k.ui.grab_button.hold,
            Drag::Slide { .. } => k.ui.slide_button.hold,
            Drag::None => true,
        }
    }
}

enum MoveKind {
    /// Move directly to the character boundary closest to the
    /// given file position.
    Absolute(FilePos),
    /// Move directly to an offset within the file.
    Raw(i64),
    /// Move a number of characters to the left/right.
    CharDelta(i16),
    /// Move a number of lines up/down.
    LineDelta(i64),
    /// Move a certain spacial distance left/right.
    HorizontalDelta(f64),
}

/// Cursor movement commands.
/// When the key is typed, these commands are queued so that they can all
/// be executed at the same time reusing the same file lock.
struct MoveCmd {
    /// Whether to reset the selection range.
    /// Otherwise, only move the second selection edge.
    reset: bool,
    /// The different kinds of movement.
    kind: MoveKind,
}

pub struct FileTab {
    pub file: FileBuffer,
    pub view: FileView,
}
impl FileTab {
    pub fn new(k: &Cfg, font: &FontArc, path: &Path) -> Result<FileTab> {
        Ok(Self {
            file: FileBuffer::new(path.into(), CharLayout::new(font), k.clone())?,
            view: FileView::new(),
        })
    }
}

pub struct FileView {
    view: ScreenRect,
    send_sel_copy: Cell<bool>,
    scroll: ScrollManager,
    selected: Selected,
    move_queue: Vec<MoveCmd>,
    drag: Drag,
    selecting: bool,
}
impl FileView {
    pub fn new() -> FileView {
        Self {
            view: ScreenRect {
                min: vec2(0., 0.),
                max: vec2(1., 1.),
            },
            scroll: default(),
            drag: Drag::None,
            selected: Selected {
                first: default(),
                second: default(),
                mod_time: Instant::now(),
                last_positions: [Some(default()); 2],
            },
            selecting: false,
            move_queue: vec![],
            send_sel_copy: false.into(),
        }
    }

    fn move_selection(&mut self, cmd: MoveCmd) {
        self.move_queue.push(cmd);
        self.selected.touch();
    }

    fn text_view(k: &Cfg, view: ScreenRect) -> ScreenRect {
        ScreenRect {
            min: view.min + vec2(k.g.left_bar, 0.),
            max: view.max,
        }
    }

    fn handle_drag(&mut self, state: &mut WindowState, button: u16, down: bool) {
        if self.drag.is_none() && down {
            if self.view.is_inside(state.last_mouse_pos) {
                if button == state.k.ui.scrollbar_button.button {
                    // Maybe start dragging one of the scrollbars
                    let pos = state.last_mouse_pos;
                    let by = self.scroll.y_scrollbar_bounds(&state.k, self.view);
                    let bx = self.scroll.x_scrollbar_bounds(&state.k, self.view);
                    let hy = self.scroll.y_scrollhandle_bounds(&state.k, self.view);
                    let hx = self.scroll.x_scrollhandle_bounds(&state.k, self.view);
                    if by.is_inside(pos) {
                        // Start dragging through vertical scrollbar
                        let mut cut = (pos.y - hy.min.y) / hy.size().y;
                        if !state.k.ui.drag_scrollbar && (cut < 0. || cut > 1.) {
                            cut = 0.5;
                            state.redraw();
                        }
                        self.drag = Drag::ScrollbarY { cut };
                        return;
                    }
                    if bx.is_inside(pos) {
                        // Start dragging through horizontal scrollbar
                        let mut cut = (pos.x - hx.min.x) / hx.size().x;
                        if !state.k.ui.drag_scrollbar && (cut < 0. || cut > 1.) {
                            cut = 0.5;
                            state.redraw();
                        }
                        self.drag = Drag::ScrollbarX { cut };
                        return;
                    }
                }
                if button == state.k.ui.slide_button.button {
                    // Start slide-scrolling
                    self.drag = Drag::Slide {
                        screen_base: state.last_mouse_pos,
                        last_update: Instant::now().into(),
                    };
                    return;
                }
                if button == state.k.ui.grab_button.button {
                    // Start grab-scrolling
                    self.drag = Drag::Grab {
                        screen_base: state.last_mouse_pos,
                        scroll_base: self.scroll.pos,
                    };
                    return;
                }
            }
        } else if down == !self.drag.hold(&state.k) {
            // Stop dragging
            // Whether the press or release event triggers this is
            // configurable per drag-type
            self.drag = Drag::None;
        }
        if button == state.k.ui.select_button {
            if down {
                // Start selecting text
                if self.view.is_inside(state.last_mouse_pos) {
                    let pos =
                        self.scroll
                            .screen_to_file_pos(&state.k, self.view, state.last_mouse_pos);
                    self.move_selection(MoveCmd {
                        reset: true,
                        kind: MoveKind::Absolute(pos),
                    });
                    self.selecting = true;
                    state.redraw();
                    return;
                }
            } else {
                // Stop selecting text
                self.selecting = false;
            }
        }
    }

    fn tick_drag(&mut self, state: &mut WindowState, pos: Vec2, synthetic: bool) {
        // Tick any form of scrolling
        match &self.drag {
            Drag::None => {}
            Drag::Grab {
                screen_base,
                scroll_base,
            } => {
                let d = (*screen_base - pos) / state.k.g.font_height;
                self.scroll.pos = scroll_base.offset(d.as_dvec2());
                state.redraw();
            }
            Drag::ScrollbarY { cut } => {
                let bar = self.scroll.y_scrollbar_bounds(&state.k, self.view);
                let handle = self.scroll.y_scrollhandle_bounds(&state.k, self.view);
                // Coefficient between 0 and 1
                let mut y = (pos.y as f32 - handle.size().y * *cut - bar.min.y)
                    / (bar.size().y - handle.size().y);
                if y.is_nan() || y < 0. {
                    y = 0.;
                } else if y > 1. {
                    y = 1.;
                }
                self.scroll.pos.delta_y = self.scroll.last_bounds.corner.delta_y
                    + self.scroll.last_bounds.size.y * y as f64;
                state.redraw();
            }
            Drag::ScrollbarX { cut } => {
                let bar = self.scroll.x_scrollbar_bounds(&state.k, self.view);
                let handle = self.scroll.x_scrollhandle_bounds(&state.k, self.view);
                // Coefficient between 0 and 1
                let mut x = (pos.x as f32 - handle.size().x * *cut - bar.min.x)
                    / (bar.size().x - handle.size().x);
                if x.is_nan() || x < 0. {
                    x = 0.;
                } else if x > 1. {
                    x = 1.;
                }
                if self.scroll.last_bounds.size.x.is_finite() {
                    self.scroll.pos.delta_x = self.scroll.last_bounds.corner.delta_x
                        + self.scroll.last_bounds.size.x * x as f64;
                }
                state.redraw();
            }
            Drag::Slide {
                screen_base,
                last_update,
            } => {
                let now = Instant::now();
                let mut d = (pos - *screen_base).as_dvec2();
                let s = self.view.size().min_element() as f64;
                for k in 0..2 {
                    if d[k].abs() > state.k.ui.slide_dead_area / 2. {
                        d[k] = ((d[k].abs() / s - state.k.ui.slide_base_dist)
                            / state.k.ui.slide_double_dist)
                            .exp2()
                            .copysign(d[k])
                            * state.k.ui.slide_speed;
                    } else {
                        d[k] = 0.;
                    }
                }
                d *= (now - last_update.get()).as_secs_f64();
                last_update.set(now);
                self.scroll.pos = self.scroll.pos.offset(d);
                state.redraw();
            }
        }
        // Tick selection moves
        if self.selecting && !synthetic {
            let newpos = self
                .scroll
                .screen_to_file_pos(&state.k, self.view, state.last_mouse_pos);
            self.move_selection(MoveCmd {
                reset: false,
                kind: MoveKind::Absolute(newpos),
            });
            state.redraw();
        }
    }

    pub fn reposition(&mut self, view: ScreenRect) {
        self.view = view;
    }

    /// Call this to notify the file view that the user switched to another tab.
    pub fn unfocus(&mut self) {
        self.drag = Drag::None;
        self.selecting = false;
    }

    /// Ran periodically.
    /// Called by the draw code, but is not really tied to anything graphical.
    /// Ideally it would not be entangled with the draw code, but if it was
    /// separate then it would need to lock the file, and then we would lock
    /// the file twice per frame, and that is very suboptimal.
    /// The file manager might take single-digit amount of milliseconds to
    /// release the lock, so we *really* don't want to incur this cost twice.
    fn bookkeep_file(&mut self, state: &mut WindowState, file: &mut FileLock) {
        // Apply selection movements
        for cmd in self.move_queue.drain(..) {
            // Move offset depending on the command type
            match cmd.kind {
                MoveKind::Absolute(pos) => {
                    // Select based on a spacial position
                    let (base, y, x) = pos.floor();
                    if let Some(at) = file.lookup_pos(base, y, x, 0.5) {
                        self.selected.second = at.offset;
                    }
                }
                MoveKind::Raw(off) => {
                    // Select based on a raw file offset
                    self.selected.second = off;
                }
                MoveKind::CharDelta(delta) => {
                    // Move the current selection by this amount of characters
                    // TODO: This may leave the cursor in the middle of a UTF-8 character
                    // if we are at the edge of loaded data
                    // Figure out what to do about it
                    let off = file
                        .char_delta(self.selected.second, delta)
                        .unwrap_or_else(|e| e);
                    self.selected.second = off;
                }
                MoveKind::LineDelta(delta) => {
                    // Move the current selection by this amount of lines
                    if let Some(at) = file.lookup_offset(self.selected.second, self.selected.second)
                    {
                        if let Some(at_target) =
                            file.lookup_pos(self.selected.second, at.dy + delta, at.dx, 0.5)
                        {
                            self.selected.second = at_target.offset;
                        }
                    }
                }
                MoveKind::HorizontalDelta(delta) => {
                    // Move the current selection by this distance
                    if let Some(at) = file.lookup_offset(self.selected.second, self.selected.second)
                    {
                        if let Some(at_target) =
                            file.lookup_pos(self.selected.second, at.dy, at.dx + delta, 0.5)
                        {
                            self.selected.second = at_target.offset;
                        }
                    }
                }
            }
            // Figure out spacial position based on offset
            let pos = file
                .lookup_offset(self.scroll.pos.base_offset, self.selected.second)
                .map(|at| FilePos {
                    base_offset: self.scroll.pos.base_offset,
                    delta_x: at.dx,
                    delta_y: at.dy as f64,
                });
            self.selected.last_positions[1] = pos;
            // Move scroll position to fit cursor within bounds
            if let Some(pos) = pos {
                let sz = self.scroll.last_view.size;
                let ylo = pos.delta_y + 1. + state.k.ui.cursor_padding - sz.y;
                let yhi = pos.delta_y - state.k.ui.cursor_padding;
                let xlo = pos.delta_x + state.k.ui.cursor_padding - sz.x;
                let xhi = pos.delta_x - state.k.ui.cursor_padding;
                self.scroll.pos.delta_y = self.scroll.pos.delta_y.clamp(ylo, yhi.max(ylo));
                self.scroll.pos.delta_x = self.scroll.pos.delta_x.clamp(xlo, xhi.max(xlo));
            }
            // Conditionally reset the selection
            if cmd.reset {
                self.selected.first = self.selected.second;
                self.selected.last_positions[0] = self.selected.last_positions[1];
            }
        }
        // Inform the backend about what area of the file to load (and keep loaded)
        let mut selection = self.selected.first..self.selected.second;
        if selection.start > selection.end {
            mem::swap(&mut selection.start, &mut selection.end);
        }
        file.set_hot_area(self.scroll.last_view, Some(selection));
        // Send a copy command if requested
        if self.send_sel_copy.get() {
            file.copy_selection();
            self.send_sel_copy.set(false);
        }
    }

    pub fn handle_event(
        &mut self,
        file: &FileBuffer,
        state: &mut WindowState,
        ev: &gl::winit::event::Event<()>,
    ) {
        use gl::winit::event::{Event, WindowEvent};
        match ev {
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::KeyboardInput { input, .. } => {
                    use gl::glutin::event::VirtualKeyCode::*;
                    let down = elem2bool(input.state);
                    match input.virtual_keycode {
                        Some(C) if down && state.keys.ctrl() => {
                            self.send_sel_copy.set(true);
                            state.redraw();
                        }
                        Some(A) if down && state.keys.ctrl() => {
                            self.move_selection(MoveCmd {
                                reset: true,
                                kind: MoveKind::Raw(0),
                            });
                            self.move_selection(MoveCmd {
                                reset: false,
                                kind: MoveKind::Raw(file.file_size()),
                            });
                            state.redraw();
                        }
                        Some(Home) if down => {
                            let kind = if state.keys.ctrl() {
                                MoveKind::Raw(0)
                            } else {
                                MoveKind::HorizontalDelta(f64::NEG_INFINITY)
                            };
                            self.move_selection(MoveCmd {
                                reset: !state.keys.shift(),
                                kind,
                            });
                            state.redraw();
                        }
                        Some(End) if down => {
                            let kind = if state.keys.ctrl() {
                                MoveKind::Raw(file.file_size())
                            } else {
                                MoveKind::HorizontalDelta(f64::INFINITY)
                            };
                            self.move_selection(MoveCmd {
                                reset: !state.keys.shift(),
                                kind,
                            });
                            state.redraw();
                        }
                        Some(PageUp) if down => {
                            self.move_selection(MoveCmd {
                                reset: !state.keys.shift(),
                                kind: MoveKind::LineDelta(
                                    -self.scroll.last_view.size.y.floor().max(1.) as i64,
                                ),
                            });
                            state.redraw();
                        }
                        Some(PageDown) if down => {
                            self.move_selection(MoveCmd {
                                reset: !state.keys.shift(),
                                kind: MoveKind::LineDelta(
                                    self.scroll.last_view.size.y.floor().max(1.) as i64,
                                ),
                            });
                            state.redraw();
                        }
                        Some(Left) if down => {
                            self.move_selection(MoveCmd {
                                reset: !state.keys.shift(),
                                kind: MoveKind::CharDelta(-1),
                            });
                            state.redraw();
                        }
                        Some(Right) if down => {
                            self.move_selection(MoveCmd {
                                reset: !state.keys.shift(),
                                kind: MoveKind::CharDelta(1),
                            });
                            state.redraw();
                        }
                        Some(Up) if down => {
                            self.move_selection(MoveCmd {
                                reset: !state.keys.shift(),
                                kind: MoveKind::LineDelta(-1),
                            });
                            state.redraw();
                        }
                        Some(Down) if down => {
                            self.move_selection(MoveCmd {
                                reset: !state.keys.shift(),
                                kind: MoveKind::LineDelta(1),
                            });
                            state.redraw();
                        }
                        _ => {}
                    }
                }
                WindowEvent::MouseWheel { delta, .. } => {
                    if self.view.is_inside(state.last_mouse_pos) {
                        // Scroll directly using mouse/trackpad wheel
                        let mut d = match delta {
                            MouseScrollDelta::LineDelta(x, y) => dvec2(-x as f64, -y as f64),
                            MouseScrollDelta::PixelDelta(d) => {
                                dvec2(-d.x, -d.y) / state.k.g.font_height as f64
                            }
                        };
                        if state.k.ui.invert_wheel_x {
                            d.x *= -1.;
                        }
                        if state.k.ui.invert_wheel_y {
                            d.y *= -1.;
                        }
                        self.scroll.pos = self.scroll.pos.offset(d);
                        state.redraw();
                    }
                }
                WindowEvent::MouseInput {
                    state: st, button, ..
                } => {
                    let button = mouse2id(*button);
                    let down = elem2bool(*st);
                    self.handle_drag(state, button, down);
                }
                WindowEvent::CursorMoved { position, .. } => {
                    let pos = dvec2(position.x, position.y).as_vec2();
                    self.tick_drag(state, pos, false);
                    {
                        use gl::winit::window::CursorIcon;
                        let icon = if self.view.is_inside(pos)
                            && !self
                                .scroll
                                .y_scrollbar_bounds(&state.k, self.view)
                                .is_inside(pos)
                            && !self
                                .scroll
                                .x_scrollbar_bounds(&state.k, self.view)
                                .is_inside(pos)
                        {
                            CursorIcon::Text
                        } else {
                            CursorIcon::Default
                        };
                        state.display.gl_window().window().set_cursor_icon(icon);
                    }
                }
                _ => {}
            },
            Event::RedrawRequested(_) => {
                self.tick_drag(state, state.last_mouse_pos, true);
            }
            _ => {}
        }
    }
}
