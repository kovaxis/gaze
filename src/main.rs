use crate::prelude::*;
use cfg::Cfg;
use drawing::DrawState;
use filebuf::{ScrollPos, ScrollRect};
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
    pos: ScrollPos,
    last_view: ScrollRect,
    last_bounds: ScrollRect,
}
impl ScrollManager {
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
        (
            vec2(w as f32 - k.g.scrollbar_width, 0.),
            vec2(k.g.scrollbar_width, h as f32 - k.g.scrollbar_width),
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
        (
            vec2(0., h as f32 - k.g.scrollbar_width),
            vec2(w as f32 - k.g.scrollbar_width, k.g.scrollbar_width),
        )
    }

    /// Get the scrollhandle rect as origin and size.
    fn x_scrollhandle_bounds(&self, k: &Cfg, w: u32, h: u32) -> (Vec2, Vec2) {
        let (p, s) = self.x_scrollbar_bounds(k, w, h);
        let sw = (self.wcoef() as f32 * s.x).max(k.g.scrollhandle_min_size);
        let sx = self.xcoef() as f32 * (s.x - sw);
        (vec2(p.x + sx, p.y), vec2(sw, s.y))
    }
}

enum Drag {
    None,
    Grab {
        screen_base: DVec2,
        scroll_base: ScrollPos,
    },
    ScrollbarY {
        cut: f32,
    },
    ScrollbarX {
        cut: f32,
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
}

pub struct WindowState {
    display: Display,
    draw: DrawState,
    file: Option<FileBuffer>,
    k: Cfg,
    drag: Drag,
    last_mouse_pos: DVec2,
    last_size: (u32, u32),
    focused: bool,
    scroll: ScrollManager,
}
impl WindowState {
    fn redraw(&self) {
        self.display.gl_window().window().request_redraw();
    }

    fn pixel_to_lines(&self, pix: DVec2) -> DVec2 {
        pix / self.k.g.font_height as f64
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
            cb,
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
        k,
        scroll: default(),
        drag: Drag::None,
        last_mouse_pos: DVec2::ZERO,
        last_size: (1, 1),
        focused: false,
        draw: DrawState::new(&display, &font)?,
        display,
    };

    gl_run_loop(
        evloop,
        Box::new(move |ev, flow| {
            use glutin::event::{Event, WindowEvent};
            *flow = ControlFlow::Wait;
            match ev {
                Event::WindowEvent { event, .. } => match event {
                    WindowEvent::CloseRequested => *flow = ControlFlow::Exit,
                    WindowEvent::KeyboardInput { input, .. } => match input.virtual_keycode {
                        Some(glutin::event::VirtualKeyCode::Escape) => *flow = ControlFlow::Exit,
                        _ => {}
                    },
                    WindowEvent::MouseWheel { delta, .. } => {
                        let mut d = match delta {
                            MouseScrollDelta::LineDelta(x, y) => dvec2(-x as f64, -y as f64),
                            MouseScrollDelta::PixelDelta(d) => {
                                state.pixel_to_lines(dvec2(-d.x, -d.y))
                            }
                        };
                        if state.k.ui.invert_wheel_x {
                            d.x *= -1.;
                        }
                        if state.k.ui.invert_wheel_y {
                            d.y *= -1.;
                        }
                        state.scroll.pos.delta_x += d.x;
                        state.scroll.pos.delta_y += d.y;
                        state.redraw();
                    }
                    WindowEvent::MouseInput {
                        state: st, button, ..
                    } => {
                        let down = state2bool(st);
                        if !down {
                            state.drag = Drag::None;
                        } else if state.drag.is_none() {
                            match button {
                                MouseButton::Left => {
                                    let pos = state.last_mouse_pos.as_vec2();
                                    let (w, h) = state.last_size;
                                    let (byp, bys) =
                                        state.scroll.y_scrollbar_bounds(&state.k, w, h);
                                    let (bxp, bxs) =
                                        state.scroll.x_scrollbar_bounds(&state.k, w, h);
                                    let (yp, ys) =
                                        state.scroll.y_scrollhandle_bounds(&state.k, w, h);
                                    let (xp, xs) =
                                        state.scroll.x_scrollhandle_bounds(&state.k, w, h);
                                    if pos.x >= byp.x
                                        && pos.x < byp.x + bys.x
                                        && pos.y >= byp.y
                                        && pos.y < byp.y + bys.y
                                    {
                                        // Start dragging through vertical scrollbar
                                        let mut cut = (pos.y - yp.y) / ys.y;
                                        if !state.k.ui.drag_scrollbar {
                                            cut = cut.clamp(0., 1.);
                                        }
                                        state.drag = Drag::ScrollbarY { cut };
                                    } else if pos.x >= bxp.x
                                        && pos.x <= bxp.x + bxs.x
                                        && pos.y >= bxp.y
                                        && pos.y < bxp.y + bxs.y
                                    {
                                        // Start dragging through horizontal scrollbar
                                        let mut cut = (pos.x - xp.x) / xs.x;
                                        if !state.k.ui.drag_scrollbar {
                                            cut = cut.clamp(0., 1.);
                                        }
                                        state.drag = Drag::ScrollbarX { cut };
                                    }
                                }
                                MouseButton::Right => {
                                    state.drag = Drag::Grab {
                                        screen_base: state.last_mouse_pos,
                                        scroll_base: state.scroll.pos,
                                    };
                                }
                                _ => {}
                            }
                        }
                    }
                    WindowEvent::CursorMoved { position, .. } => {
                        let pos = dvec2(position.x, position.y);
                        match &state.drag {
                            Drag::None => {}
                            &Drag::Grab {
                                screen_base,
                                scroll_base,
                            } => {
                                let d = state.pixel_to_lines(screen_base - pos);
                                state.scroll.pos.delta_x = scroll_base.delta_x + d.x;
                                state.scroll.pos.delta_y = scroll_base.delta_y + d.y;
                                state.redraw();
                            }
                            &Drag::ScrollbarY { cut } => {
                                let (w, h) = state.last_size;
                                let (bp, bs) = state.scroll.y_scrollbar_bounds(&state.k, w, h);
                                let (_p, s) = state.scroll.y_scrollhandle_bounds(&state.k, w, h);
                                // Coefficient between 0 and 1
                                let mut y = (pos.y as f32 - s.y * cut - bp.y) / (bs.y - s.y);
                                if y.is_nan() || y < 0. {
                                    y = 0.;
                                } else if y > 1. {
                                    y = 1.;
                                }
                                state.scroll.pos.delta_y = state.scroll.last_bounds.corner.delta_y
                                    + state.scroll.last_bounds.size.y * y as f64;
                                state.redraw();
                            }
                            &Drag::ScrollbarX { cut } => {
                                let (w, h) = state.last_size;
                                let (bp, bs) = state.scroll.x_scrollbar_bounds(&state.k, w, h);
                                let (_p, s) = state.scroll.x_scrollhandle_bounds(&state.k, w, h);
                                // Coefficient between 0 and 1
                                let mut x = (pos.x as f32 - s.x * cut - bp.x) / (bs.x - s.x);
                                if x.is_nan() || x < 0. {
                                    x = 0.;
                                } else if x > 1. {
                                    x = 1.;
                                }
                                if state.scroll.last_bounds.size.x.is_finite() {
                                    state.scroll.pos.delta_x =
                                        state.scroll.last_bounds.corner.delta_x
                                            + state.scroll.last_bounds.size.x * x as f64;
                                }
                                state.redraw();
                            }
                        }
                        state.last_mouse_pos = pos;
                    }
                    WindowEvent::Focused(f) => state.focused = f,
                    _ => {}
                },
                Event::DeviceEvent { event, .. } => {
                    if state.focused {
                        match event {
                            _ => {}
                        }
                    }
                }
                Event::RedrawRequested(_) => match drawing::draw(&mut state) {
                    Ok(redraw_soon) => {
                        if redraw_soon {
                            state.redraw();
                        }
                    }
                    Err(err) => {
                        println!("error drawing frame: {:#}", err);
                    }
                },
                _ => {}
            }
        }),
    )
}
