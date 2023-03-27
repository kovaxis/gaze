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
    fn scrollbar_bounds(&self, k: &Cfg, w: u32, h: u32) -> (Vec2, Vec2) {
        (
            vec2(w as f32 - k.g.scrollbar_width, 0.),
            vec2(k.g.scrollbar_width, h as f32),
        )
    }

    /// Get the scrollhandle rect as origin and size.
    fn scrollhandle_bounds(&self, k: &Cfg, w: u32, h: u32) -> (Vec2, Vec2) {
        let sh = (self.hcoef() as f32 * h as f32).max(k.g.scrollhandle_min_size);
        let sy = self.ycoef() as f32 * (h as f32 - sh);
        (
            vec2(w as f32 - k.g.scrollbar_width, sy),
            vec2(k.g.scrollbar_width, sh),
        )
    }
}

pub struct WindowState {
    display: Display,
    draw: DrawState,
    file: Option<FileBuffer>,
    k: Cfg,
    scroll_drag: Option<(DVec2, ScrollPos)>,
    scrollbar_drag: Option<f32>,
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
        scroll_drag: None,
        scrollbar_drag: None,
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
                        let d = match delta {
                            MouseScrollDelta::LineDelta(x, y) => dvec2(-x as f64, -y as f64),
                            MouseScrollDelta::PixelDelta(d) => {
                                state.pixel_to_lines(dvec2(-d.x, -d.y))
                            }
                        };
                        state.scroll.pos.delta_x += d.x;
                        state.scroll.pos.delta_y += d.y;
                        state.redraw();
                    }
                    WindowEvent::MouseInput {
                        state: st, button, ..
                    } => {
                        let down = state2bool(st);
                        match button {
                            MouseButton::Left => {
                                if down {
                                    let (p, s) = state.scroll.scrollhandle_bounds(
                                        &state.k,
                                        state.last_size.0,
                                        state.last_size.1,
                                    );
                                    let pos = state.last_mouse_pos.as_vec2();
                                    if pos.x >= p.x
                                        && pos.x < p.x + s.x
                                        && pos.y >= p.y
                                        && pos.y < p.y + s.y
                                    {
                                        // Start dragging through scrollbar
                                        let cut = (pos.y - p.y) / s.y;
                                        state.scrollbar_drag.get_or_insert(cut);
                                    }
                                } else {
                                    state.scrollbar_drag = None;
                                }
                            }
                            MouseButton::Right => {
                                if down {
                                    state
                                        .scroll_drag
                                        .get_or_insert((state.last_mouse_pos, state.scroll.pos));
                                } else {
                                    state.scroll_drag = None;
                                }
                            }
                            _ => {}
                        }
                    }
                    WindowEvent::CursorMoved { position, .. } => {
                        let pos = dvec2(position.x, position.y);
                        if let Some(cut) = state.scrollbar_drag {
                            let (w, h) = state.last_size;
                            let (_p, s) = state.scroll.scrollhandle_bounds(&state.k, w, h);
                            // Coefficient between 0 and 1
                            let mut y = (pos.y as f32 - s.y * cut) / (h as f32 - s.y);
                            if y.is_nan() || y < 0. {
                                y = 0.;
                            } else if y > 1. {
                                y = 1.;
                            }
                            state.scroll.pos.delta_y = state.scroll.last_bounds.corner.delta_y
                                + state.scroll.last_bounds.size.y * y as f64;
                            state.redraw();
                        } else if let Some((ogpos, ogscr)) = state.scroll_drag {
                            let d = state.pixel_to_lines(ogpos - pos);
                            state.scroll.pos.delta_x = ogscr.delta_x + d.x;
                            state.scroll.pos.delta_y = ogscr.delta_y + d.y;
                            state.redraw();
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
                        eprintln!("error drawing frame: {:#}", err);
                    }
                },
                _ => {}
            }
        }),
    )
}
