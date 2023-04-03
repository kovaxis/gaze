use crate::prelude::*;
use cfg::Cfg;
use drawing::DrawState;
use fileview::FileView;
use gl::{glutin::event_loop::ControlFlow, winit::event::ElementState, *};

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
mod fileview;

pub struct WindowState {
    display: Display,
    draw: DrawState,
    cur_tab: usize,
    tabs: Vec<Box<FileView>>,
    k: Cfg,
    last_mouse_pos: Vec2,
    last_size: (u32, u32),
    ctrl_down: bool,
    focused: bool,
}
impl WindowState {
    fn redraw(&self) {
        self.display.gl_window().window().request_redraw();
    }

    fn take_fview(&mut self, idx: usize) -> Option<Box<FileView>> {
        if idx < self.tabs.len() {
            Some(self.tabs.swap_remove(idx))
        } else {
            None
        }
    }

    fn put_fview(&mut self, idx: usize, fview: Box<FileView>) {
        let last = self.tabs.len();
        self.tabs.push(fview);
        self.tabs.swap(idx, last);
    }

    fn resize(&mut self, (w, h): (u32, u32)) {
        for i in 0..self.tabs.len() {
            let mut fview = self.take_fview(i).unwrap();
            fview.reposition(ScreenRect {
                min: vec2(10., 10.),
                max: vec2(w as f32 - 10., h as f32 - 10.),
            });
            self.put_fview(i, fview);
        }
    }

    fn handle_event(&mut self, ev: gl::winit::event::Event<()>, flow: &mut ControlFlow) {
        use gl::winit::event::{Event, WindowEvent};
        // Dispatch event to active file view
        if let Some(mut fview) = self.take_fview(self.cur_tab) {
            fview.handle_event(self, &ev);
            self.put_fview(self.cur_tab, fview);
        }
        // Handle event at the window level
        match ev {
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => *flow = ControlFlow::Exit,
                WindowEvent::KeyboardInput { input, .. } => {
                    use glutin::event::VirtualKeyCode::*;
                    let down = elem2bool(input.state);
                    match input.virtual_keycode {
                        Some(Escape) => *flow = ControlFlow::Exit,
                        Some(LControl) => self.ctrl_down = down,
                        _ => {}
                    }
                }
                WindowEvent::CursorMoved { position, .. } => {
                    self.last_mouse_pos = dvec2(position.x, position.y).as_vec2();
                }
                WindowEvent::Focused(f) => self.focused = f,
                WindowEvent::Resized(sz) => self.resize((sz.width, sz.height)),
                _ => {}
            },
            Event::RedrawRequested(_) => {
                if let Err(err) = drawing::draw(self) {
                    println!("error drawing frame: {:#}", err);
                }
            }
            _ => {}
        }
    }
}

fn elem2bool(e: ElementState) -> bool {
    e == ElementState::Pressed
}

#[derive(Copy, Clone)]
pub struct ScreenRect {
    min: Vec2,
    max: Vec2,
}
impl ScreenRect {
    fn size(&self) -> Vec2 {
        self.max - self.min
    }

    fn is_inside(&self, u: Vec2) -> bool {
        let (p0, p1) = (self.min, self.max);
        u.x >= p0.x && u.x < p1.x && u.y >= p0.y && u.y < p1.y
    }

    fn as_gl_rect(&self, (_w, h): (u32, u32)) -> gl::glium::Rect {
        gl::glium::Rect {
            left: self.min.x.ceil() as u32,
            bottom: h - self.max.y.floor() as u32,
            width: self.max.x.floor() as u32 - self.min.x.ceil() as u32,
            height: self.max.y.floor() as u32 - self.min.y.ceil() as u32,
        }
    }
}

fn main() -> Result<()> {
    gl::clipboard::maybe_serve().map_err(|e| anyhow!("failed to serve clipboard: {}", e))?;

    let (evloop, display) = gl_create_display(Box::new(|wb, cb| {
        (
            wb.with_title("Gaze Text Editor")
                .with_inner_size(glutin::dpi::LogicalSize::new(800., 600.)),
            cb.with_vsync(false).with_multisampling(4),
        )
    }));

    let font = FontArc::try_from_vec(fs::read("font.ttf").context("failed to read font file")?)?;
    let k = Cfg::load_or_new();

    let path = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or(anyhow!("expected file to open as argument"))?,
    );
    let mut state = WindowState {
        tabs: vec![Box::new(FileView::new(&k, &font, path.as_path())?)],
        cur_tab: 0,
        last_mouse_pos: Vec2::ZERO,
        last_size: (1, 1),
        ctrl_down: false,
        focused: false,
        draw: DrawState::new(&display, &font, &k)?,
        display,
        k,
    };

    state.resize(state.display.get_framebuffer_dimensions());

    gl_run_loop(
        evloop,
        Box::new(move |ev, flow| {
            *flow = ControlFlow::Wait;
            state.handle_event(ev, flow);
        }),
    )
}
