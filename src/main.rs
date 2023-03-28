use crate::prelude::*;
use cfg::Cfg;
use drawing::DrawState;
use filebuf::{FileLock, FilePos, FileRect};
use gl::{
    glutin::event_loop::ControlFlow,
    winit::event::{ElementState, MouseButton, MouseScrollDelta},
    *,
};

mod prelude {
    pub(crate) use crate::filebuf::FileBuffer;
    pub use ab_glyph::FontArc;
    pub use anyhow::{anyhow, bail, ensure, Context, Error, Result};
    pub use crossbeam_channel::{self as channel, Receiver, Sender};
    pub use crossbeam_utils::atomic::AtomicCell;
    pub use gl::glium::Display;
    pub use glam::{
        dvec2, dvec3, dvec4, vec2, vec3, vec4, DVec2, DVec3, DVec4, Mat2, Mat3, Mat4, Vec2, Vec3,
        Vec4,
    };
    pub use parking_lot::{Mutex, MutexGuard};
    pub use rustc_hash::{FxHashMap, FxHashSet};
    pub use serde::{Deserialize, Serialize};
    pub use std::{
        cell::Cell,
        fmt,
        fs::{self, File},
        io::{self, Read, Seek, Write},
        mem, ops,
        path::{Path, PathBuf},
        ptr,
        result::Result as StdResult,
        sync::Arc,
        thread::{self, JoinHandle},
        time::{Duration, Instant},
    };

    pub fn default<T: Default>() -> T {
        T::default()
    }
}

mod cfg;
mod drawing;
mod filebuf;

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
    fn y_scrollbar_bounds(&self, k: &Cfg, w: u32, h: u32) -> (Vec2, Vec2) {
        let gap = if self.xdraw(k) {
            k.g.scrollbar_width
        } else {
            0.
        };
        (
            vec2(w as f32 - k.g.scrollbar_width, 0.),
            vec2(k.g.scrollbar_width, h as f32 - gap),
        )
    }

    /// Get the scrollhandle rect as origin and size.
    fn y_scrollhandle_bounds(&self, k: &Cfg, w: u32, h: u32) -> (Vec2, Vec2) {
        let (p, s) = self.y_scrollbar_bounds(k, w, h);
        let sh = (self.hcoef() as f32 * s.y).max(k.g.scrollhandle_min_size);
        let sy = self.ycoef() as f32 * (s.y - sh);
        (vec2(p.x, p.y + sy), vec2(s.x, sh))
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
    fn x_scrollbar_bounds(&self, k: &Cfg, w: u32, h: u32) -> (Vec2, Vec2) {
        let gap = if self.ydraw(k) {
            k.g.scrollbar_width
        } else {
            0.
        };
        (
            vec2(0., h as f32 - k.g.scrollbar_width),
            vec2(w as f32 - gap, k.g.scrollbar_width),
        )
    }

    /// Get the scrollhandle rect as origin and size.
    fn x_scrollhandle_bounds(&self, k: &Cfg, w: u32, h: u32) -> (Vec2, Vec2) {
        let (p, s) = self.x_scrollbar_bounds(k, w, h);
        let sw = (self.wcoef() as f32 * s.x).max(k.g.scrollhandle_min_size);
        let sx = self.xcoef() as f32 * (s.x - sw);
        (vec2(p.x + sx, p.y), vec2(sw, s.y))
    }

    /// Convert a mouse cursor position to a file position, based on the last scroll and other factors.
    pub fn screen_to_file_pos(&self, k: &Cfg, (w, h): (u32, u32), pos: DVec2) -> FilePos {
        let (p, _s) = k.file_view_bounds((w, h));
        let d = (pos.as_vec2() - vec2(0., k.g.selection_offset) - p) / k.g.font_height;
        self.last_view.corner.offset(d.as_dvec2())
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
        screen_base: DVec2,
        last_update: Cell<Instant>,
    },
    Grab {
        screen_base: DVec2,
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
            Drag::None => false,
        }
    }
}

struct Selected {
    /// The first selected position.
    first: FilePos,
    /// The second selected position.
    /// Note that this position might be before the first position,
    /// the "second" is chronological, not physical.
    second: FilePos,
    /// Whether the second position has been set down or if it is
    /// still in flux.
    second_set: bool,
}

pub struct WindowState {
    display: Display,
    draw: DrawState,
    file: Option<FileBuffer>,
    k: Cfg,
    drag: Drag,
    selected: Selected,
    last_mouse_pos: DVec2,
    last_size: (u32, u32),
    focused: bool,
    scroll: ScrollManager,
}
impl WindowState {
    fn redraw(&self) {
        self.display.gl_window().window().request_redraw();
    }

    fn handle_drag(&mut self, button: u16, down: bool) {
        if self.drag.is_none() && down {
            if button == self.k.ui.scrollbar_button.button {
                // Maybe start dragging one of the scrollbars
                let pos = self.last_mouse_pos.as_vec2();
                let (w, h) = self.last_size;
                let (byp, bys) = self.scroll.y_scrollbar_bounds(&self.k, w, h);
                let (bxp, bxs) = self.scroll.x_scrollbar_bounds(&self.k, w, h);
                let (yp, ys) = self.scroll.y_scrollhandle_bounds(&self.k, w, h);
                let (xp, xs) = self.scroll.x_scrollhandle_bounds(&self.k, w, h);
                if pos.x >= byp.x
                    && pos.x < byp.x + bys.x
                    && pos.y >= byp.y
                    && pos.y < byp.y + bys.y
                {
                    // Start dragging through vertical scrollbar
                    let mut cut = (pos.y - yp.y) / ys.y;
                    if !self.k.ui.drag_scrollbar && (cut < 0. || cut > 1.) {
                        cut = cut.clamp(0., 1.);
                        self.redraw();
                    }
                    self.drag = Drag::ScrollbarY { cut };
                    return;
                }
                if pos.x >= bxp.x
                    && pos.x <= bxp.x + bxs.x
                    && pos.y >= bxp.y
                    && pos.y < bxp.y + bxs.y
                {
                    // Start dragging through horizontal scrollbar
                    let mut cut = (pos.x - xp.x) / xs.x;
                    if !self.k.ui.drag_scrollbar && (cut < 0. || cut > 1.) {
                        cut = cut.clamp(0., 1.);
                        self.redraw();
                    }
                    self.drag = Drag::ScrollbarX { cut };
                    return;
                }
            }
            if button == self.k.ui.slide_button.button {
                // Start slide-scrolling
                self.drag = Drag::Slide {
                    screen_base: self.last_mouse_pos,
                    last_update: Instant::now().into(),
                };
                return;
            }
            if button == self.k.ui.grab_button.button {
                // Start grab-scrolling
                self.drag = Drag::Grab {
                    screen_base: self.last_mouse_pos,
                    scroll_base: self.scroll.pos,
                };
                return;
            }
        } else if down == !self.drag.hold(&self.k) {
            // Stop dragging
            // Whether the press or release event triggers this is
            // configurable per drag-type
            self.drag = Drag::None;
        }
        if button == self.k.ui.select_button {
            if down {
                // Start selecting text
                let pos =
                    self.scroll
                        .screen_to_file_pos(&self.k, self.last_size, self.last_mouse_pos);
                self.selected = Selected {
                    first: pos,
                    second: pos,
                    second_set: false,
                };
                self.redraw();
                return;
            } else {
                // Stop selecting text
                self.selected.second_set = true;
            }
        }
    }

    fn tick_drag(&mut self, pos: DVec2) {
        // Tick any form of scrolling
        match &self.drag {
            Drag::None => {}
            Drag::Grab {
                screen_base,
                scroll_base,
            } => {
                let d = (*screen_base - pos) / self.k.g.font_height as f64;
                self.scroll.pos = scroll_base.offset(d);
                self.redraw();
            }
            Drag::ScrollbarY { cut } => {
                let (w, h) = self.last_size;
                let (bp, bs) = self.scroll.y_scrollbar_bounds(&self.k, w, h);
                let (_p, s) = self.scroll.y_scrollhandle_bounds(&self.k, w, h);
                // Coefficient between 0 and 1
                let mut y = (pos.y as f32 - s.y * *cut - bp.y) / (bs.y - s.y);
                if y.is_nan() || y < 0. {
                    y = 0.;
                } else if y > 1. {
                    y = 1.;
                }
                self.scroll.pos.delta_y = self.scroll.last_bounds.corner.delta_y
                    + self.scroll.last_bounds.size.y * y as f64;
                self.redraw();
            }
            Drag::ScrollbarX { cut } => {
                let (w, h) = self.last_size;
                let (bp, bs) = self.scroll.x_scrollbar_bounds(&self.k, w, h);
                let (_p, s) = self.scroll.x_scrollhandle_bounds(&self.k, w, h);
                // Coefficient between 0 and 1
                let mut x = (pos.x as f32 - s.x * *cut - bp.x) / (bs.x - s.x);
                if x.is_nan() || x < 0. {
                    x = 0.;
                } else if x > 1. {
                    x = 1.;
                }
                if self.scroll.last_bounds.size.x.is_finite() {
                    self.scroll.pos.delta_x = self.scroll.last_bounds.corner.delta_x
                        + self.scroll.last_bounds.size.x * x as f64;
                }
                self.redraw();
            }
            Drag::Slide {
                screen_base,
                last_update,
            } => {
                let now = Instant::now();
                let mut d = pos - *screen_base;
                let s = self.last_size.0.min(self.last_size.1) as f64;
                for k in 0..2 {
                    if d[k].abs() > self.k.ui.slide_dead_area / 2. {
                        d[k] = ((d[k].abs() / s - self.k.ui.slide_base_dist)
                            / self.k.ui.slide_double_dist)
                            .exp2()
                            .copysign(d[k])
                            * self.k.ui.slide_speed;
                    } else {
                        d[k] = 0.;
                    }
                }
                d *= (now - last_update.get()).as_secs_f64();
                last_update.set(now);
                self.scroll.pos = self.scroll.pos.offset(d);
                self.redraw();
            }
        }
        // Tick selection moves
        if !self.selected.second_set {
            self.selected.second =
                self.scroll
                    .screen_to_file_pos(&self.k, self.last_size, self.last_mouse_pos);
            self.redraw();
        }
    }

    fn handle_event(&mut self, ev: gl::winit::event::Event<()>, flow: &mut ControlFlow) {
        use gl::winit::event::{Event, WindowEvent};
        match ev {
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => *flow = ControlFlow::Exit,
                WindowEvent::KeyboardInput { input, .. } => match input.virtual_keycode {
                    Some(glutin::event::VirtualKeyCode::Escape) => *flow = ControlFlow::Exit,
                    _ => {}
                },
                WindowEvent::MouseWheel { delta, .. } => {
                    // Scroll directly using mouse/trackpad wheel
                    let mut d = match delta {
                        MouseScrollDelta::LineDelta(x, y) => dvec2(-x as f64, -y as f64),
                        MouseScrollDelta::PixelDelta(d) => {
                            dvec2(-d.x, -d.y) / self.k.g.font_height as f64
                        }
                    };
                    if self.k.ui.invert_wheel_x {
                        d.x *= -1.;
                    }
                    if self.k.ui.invert_wheel_y {
                        d.y *= -1.;
                    }
                    self.scroll.pos = self.scroll.pos.offset(d);
                    self.redraw();
                }
                WindowEvent::MouseInput { state, button, .. } => {
                    let button = match button {
                        MouseButton::Left => 0,
                        MouseButton::Right => 1,
                        MouseButton::Middle => 2,
                        MouseButton::Other(b) => b,
                    };
                    let down = state2bool(state);
                    self.handle_drag(button, down);
                }
                WindowEvent::CursorMoved { position, .. } => {
                    let pos = dvec2(position.x, position.y);
                    self.tick_drag(pos);
                    {
                        use gl::winit::window::CursorIcon;
                        let (p, s) = self.k.file_view_bounds(self.last_size);
                        let pos = pos.as_vec2();
                        let icon = if pos.x >= p.x
                            && pos.x < p.x + s.x
                            && pos.y >= p.y
                            && pos.y < p.y + s.y
                        {
                            CursorIcon::Text
                        } else {
                            CursorIcon::Default
                        };
                        self.display.gl_window().window().set_cursor_icon(icon);
                    }
                    self.last_mouse_pos = pos;
                }
                WindowEvent::Focused(f) => self.focused = f,
                _ => {}
            },
            Event::DeviceEvent { event, .. } => {
                if self.focused {
                    match event {
                        _ => {}
                    }
                }
            }
            Event::RedrawRequested(_) => {
                self.tick_drag(self.last_mouse_pos);
                match drawing::draw(self) {
                    Ok(redraw_soon) => {
                        if redraw_soon || self.drag.requires_refresh() {
                            self.redraw();
                        }
                    }
                    Err(err) => {
                        println!("error drawing frame: {:#}", err);
                    }
                }
            }
            _ => {}
        }
    }

    fn selected_offsets(&self, file: &FileLock) -> Option<ops::Range<i64>> {
        let (fo, fy, fx) = self.selected.first.floor();
        let mut first = file.lookup_pos(fo, fy, fx)?;
        let (so, sy, sx) = self.selected.second.floor();
        let mut second = file.lookup_pos(so, sy, sx)?;
        if second.offset < first.offset {
            mem::swap(&mut first, &mut second);
        }
        Some(first.offset..second.offset)
    }
}

fn state2bool(e: ElementState) -> bool {
    e == ElementState::Pressed
}

fn main() -> Result<()> {
    let (evloop, display) = gl_create_display(Box::new(|wb, cb| {
        (
            wb.with_title("Gaze Text Editor")
                .with_inner_size(glutin::dpi::LogicalSize::new(800., 600.)),
            cb.with_vsync(false).with_multisampling(4),
        )
    }));

    let font = FontArc::try_from_vec(fs::read("font.ttf").context("failed to read font file")?)?;
    let k = Cfg::load_or_new();

    let mut state = WindowState {
        file: Some(FileBuffer::open(
            std::env::args_os()
                .nth(1)
                .ok_or(anyhow!("expected file to open as argument"))?
                .as_ref(),
            font.clone(),
            k.clone(),
        )?),
        scroll: default(),
        drag: Drag::None,
        selected: Selected {
            first: default(),
            second: default(),
            second_set: true,
        },
        last_mouse_pos: DVec2::ZERO,
        last_size: (1, 1),
        focused: false,
        draw: DrawState::new(&display, &font, &k)?,
        display,
        k,
    };

    gl_run_loop(
        evloop,
        Box::new(move |ev, flow| {
            *flow = ControlFlow::Wait;
            state.handle_event(ev, flow);
        }),
    )
}
