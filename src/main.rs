use crate::prelude::*;
use cfg::Cfg;
use drawing::DrawState;
use filebuf::ScrollPos;
use gl::{
    glutin::event_loop::ControlFlow,
    winit::event::{DeviceEvent, ElementState, MouseButton, MouseScrollDelta},
    *,
};
use glyph_brush::ab_glyph::FontArc;

mod prelude {
    pub(crate) use crate::filebuf::FileBuffer;
    pub use anyhow::{anyhow, bail, ensure, Context, Error, Result};
    pub use crossbeam_channel::{self as channel, Receiver, Sender};
    pub use crossbeam_utils::atomic::AtomicCell;
    pub use gl::glium::Display;
    pub use glam::{
        dvec2, dvec3, dvec4, vec2, vec3, vec4, DVec2, DVec3, DVec4, Mat2, Mat3, Mat4, Vec2, Vec3,
        Vec4,
    };
    pub use parking_lot::{Mutex, MutexGuard};
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

pub struct WindowState {
    font: FontArc,
    display: Display,
    draw: DrawState,
    file: Option<FileBuffer>,
    k: Cfg,
    mouse_drag_down: bool,
    focused: bool,
    scroll: ScrollPos,
}
impl WindowState {
    fn redraw(&self) {
        self.display.gl_window().window().request_redraw();
    }

    fn scroll(&mut self, pixels_x: f64, pixels_y: f64) {
        self.scroll.delta_x += pixels_x / self.k.font_height as f64;
        self.scroll.delta_y += pixels_y / self.k.font_height as f64;
    }
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

    let mut state = WindowState {
        file: Some(FileBuffer::open(
            std::env::args_os()
                .nth(1)
                .ok_or(anyhow!("expected file to open as argument"))?
                .as_ref(),
            font.clone(),
        )?),
        k: Cfg::load_or_new(),
        scroll: ScrollPos {
            base_offset: 0,
            delta_x: 0.,
            delta_y: 0.,
        },
        mouse_drag_down: false,
        focused: false,
        draw: DrawState::new(&display, &font)?,
        display,
        font,
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
                        let (x, y) = match delta {
                            MouseScrollDelta::LineDelta(x, y) => (
                                -x as f64 * state.k.font_height as f64,
                                -y as f64 * state.k.font_height as f64,
                            ),
                            MouseScrollDelta::PixelDelta(d) => (-d.x, -d.y),
                        };
                        state.scroll(x, y);
                        state.redraw();
                    }
                    WindowEvent::MouseInput {
                        state: st, button, ..
                    } => match button {
                        MouseButton::Right => state.mouse_drag_down = st == ElementState::Pressed,
                        _ => {}
                    },
                    WindowEvent::Focused(f) => state.focused = f,
                    _ => {}
                },
                Event::DeviceEvent { event, .. } => {
                    if state.focused {
                        match event {
                            DeviceEvent::MouseMotion { delta: (dx, dy) } => {
                                if state.mouse_drag_down {
                                    state.scroll(-dx, -dy);
                                    state.redraw();
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Event::RedrawRequested(_) => {
                    if let Err(err) = drawing::draw(&mut state) {
                        eprintln!("error drawing frame: {:#}", err);
                    }
                }
                _ => {}
            }
        }),
    )
}
