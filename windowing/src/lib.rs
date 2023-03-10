pub use glium;
pub use glutin;
pub use winit;

pub fn gl_create_display(
    init: Box<
        dyn FnOnce(
            glutin::window::WindowBuilder,
            glutin::ContextBuilder<glutin::NotCurrent>,
        ) -> (
            glutin::window::WindowBuilder,
            glutin::ContextBuilder<glutin::NotCurrent>,
        ),
    >,
) -> (glutin::event_loop::EventLoop<()>, glium::Display) {
    let evloop = glutin::event_loop::EventLoop::new();
    let wb = glutin::window::WindowBuilder::new();
    let cb = glutin::ContextBuilder::new();
    let (wb, cb) = init(wb, cb);
    let display = glium::Display::new(wb, cb, &evloop).expect("failed to create opengl window");
    (evloop, display)
}

pub fn gl_run_loop(
    evloop: glutin::event_loop::EventLoop<()>,
    mut on_ev: Box<dyn FnMut(glutin::event::Event<()>, &mut glutin::event_loop::ControlFlow)>,
) -> ! {
    evloop.run(move |ev, _evloop, flow| on_ev(ev, flow))
}
