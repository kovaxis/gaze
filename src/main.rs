use crate::prelude::*;
use cfg::Cfg;
use drawing::DrawState;
use fileview::FileView;
use gl::{
    glutin::event_loop::ControlFlow,
    winit::event::{ElementState, MouseButton},
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
mod fileview;

pub struct WindowState {
    display: Display,
    draw: DrawState,
    cur_tab: usize,
    tabs: Vec<Box<FileView>>,
    k: Cfg,
    last_mouse_pos: Vec2,
    screen: ScreenRect,
    ctrl_down: bool,
    focused: bool,
}
impl WindowState {
    fn redraw(&self) {
        self.display.gl_window().window().request_redraw();
    }

    fn tab_bar_bounds(k: &Cfg, screen: ScreenRect) -> ScreenRect {
        ScreenRect {
            min: screen.min,
            max: vec2(screen.max.x, screen.min.y + k.g.tab_height),
        }
    }

    fn tab_bounds(k: &Cfg, i: usize, n: usize, screen: ScreenRect) -> ScreenRect {
        let bar = Self::tab_bar_bounds(k, screen);
        let w = ((bar.size().x + k.g.tab_gap) / n as f32 - k.g.tab_gap)
            .clamp(k.g.tab_width[0], k.g.tab_width[1]);
        let x = i as f32 * (w + k.g.tab_gap);
        ScreenRect {
            min: bar.min + vec2(x, 0.),
            max: vec2(bar.min.x + x + w, bar.max.y),
        }
    }

    fn fileview_bounds(k: &Cfg, screen: ScreenRect) -> ScreenRect {
        ScreenRect {
            min: screen.min + vec2(0., k.g.tab_height),
            max: screen.max,
        }
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
        self.screen = ScreenRect {
            min: vec2(0., 0.),
            max: vec2(w as f32, h as f32),
        };
        let bounds = Self::fileview_bounds(&self.k, self.screen);
        for i in 0..self.tabs.len() {
            let mut fview = self.take_fview(i).unwrap();
            fview.reposition(bounds);
            self.put_fview(i, fview);
        }
    }

    fn select_tab(&mut self, i: usize) {
        if i == self.cur_tab {
            return;
        }
        if let Some(tab) = self.tabs.get_mut(self.cur_tab) {
            tab.unfocus();
        }
        self.cur_tab = i;
        self.redraw();
    }

    fn kill_tab(&mut self, i: usize) {
        if i < self.tabs.len() {
            self.tabs.remove(i);
            if self.cur_tab > 0 && self.cur_tab == self.tabs.len() {
                self.cur_tab -= 1;
            }
            self.redraw();
        }
    }

    fn handle_tab_click(&mut self, button: u16, down: bool) {
        for i in 0..self.tabs.len() {
            let tab_bounds = Self::tab_bounds(&self.k, i, self.tabs.len(), self.screen);
            if tab_bounds.is_inside(self.last_mouse_pos) {
                // Clicked this tab
                if down && button == self.k.ui.tab_select_button {
                    self.select_tab(i);
                } else if down && button == self.k.ui.tab_kill_button {
                    self.kill_tab(i);
                }
            }
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
                WindowEvent::MouseInput {
                    state: st, button, ..
                } => {
                    let button = mouse2id(button);
                    let down = elem2bool(st);
                    let tabs_bounds = Self::tab_bar_bounds(&self.k, self.screen);
                    if tabs_bounds.is_inside(self.last_mouse_pos) {
                        self.handle_tab_click(button, down);
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

fn mouse2id(m: MouseButton) -> u16 {
    match m {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
        MouseButton::Other(b) => b,
    }
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

    let mut tabs = vec![];
    for path in std::env::args_os().skip(1) {
        let path = Path::new(&path);
        tabs.push(Box::new(FileView::new(&k, &font, path)?));
    }
    let mut state = WindowState {
        tabs,
        cur_tab: 0,
        last_mouse_pos: Vec2::ZERO,
        screen: ScreenRect {
            min: vec2(0., 0.),
            max: vec2(1., 1.),
        },
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
