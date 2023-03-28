use crate::{cfg::Cfg, filebuf::FileRect, prelude::*, Drag, WindowState};
use ab_glyph::{Font, Glyph};
use gl::glium::{
    index::{IndicesSource, PrimitiveType},
    uniforms::{MagnifySamplerFilter, MinifySamplerFilter, Uniforms},
    vertex::VertexBufferSlice,
    Blend, DrawParameters, Frame, Program, Surface, Texture2d, VertexBuffer,
};
use glyph_brush_draw_cache::DrawCache;

#[derive(Clone, Copy, Debug)]
pub struct FlatVertex {
    pos: [f32; 2],
    color: [u8; 4],
}

gl::glium::implement_vertex!(FlatVertex,
    pos normalize(false),
    color normalize(true)
);

#[derive(Clone, Copy, Debug)]
pub struct TextVertex {
    pos: [f32; 2],
    uv: [f32; 2],
    color: [u8; 4],
}

gl::glium::implement_vertex!(TextVertex,
    pos normalize(false),
    uv normalize(false),
    color normalize(true)
);

struct VertexBuf<T: Copy> {
    buf: Vec<T>,
    vbo: VertexBuffer<T>,
    vbo_len: usize,
}
impl<T: Copy + gl::glium::Vertex> VertexBuf<T> {
    fn new(display: &Display) -> Result<Self> {
        Ok(Self {
            buf: default(),
            vbo: VertexBuffer::empty_dynamic(display, 1024)?,
            vbo_len: 0,
        })
    }

    fn clear(&mut self) {
        self.buf.clear();
        self.vbo_len = 0;
    }

    fn push(&mut self, v: T) {
        self.buf.push(v);
    }

    fn upload(&mut self, display: &Display) -> Result<()> {
        let verts = &self.buf[..];
        if verts.len() > self.vbo.len() {
            self.vbo = VertexBuffer::empty_dynamic(display, verts.len().next_power_of_two())?;
        }
        if !verts.is_empty() {
            self.vbo.slice(0..verts.len()).unwrap().write(verts);
        }
        self.vbo_len = verts.len();
        Ok(())
    }

    fn vbo(&self) -> VertexBufferSlice<T> {
        self.vbo.slice(..self.vbo_len).unwrap()
    }
}
impl VertexBuf<FlatVertex> {
    fn push_quad(&mut self, corner: Vec2, size: Vec2, color: [u8; 4]) {
        let (o, x, y) = (corner, vec2(size.x, 0.), vec2(0., size.y));
        self.push(FlatVertex {
            pos: o.to_array(),
            color,
        });
        self.push(FlatVertex {
            pos: (o + x).to_array(),
            color,
        });
        self.push(FlatVertex {
            pos: (o + x + y).to_array(),
            color,
        });
        self.push(FlatVertex {
            pos: o.to_array(),
            color,
        });
        self.push(FlatVertex {
            pos: (o + x + y).to_array(),
            color,
        });
        self.push(FlatVertex {
            pos: (o + y).to_array(),
            color,
        });
    }

    fn push_prebuilt(&mut self, prebuilt: &[FlatVertex], offset: Vec2) {
        for v in prebuilt {
            self.push(FlatVertex {
                pos: (Vec2::from_array(v.pos) + offset).to_array(),
                color: v.color,
            });
        }
    }

    fn build_slide_icon(k: &Cfg) -> Vec<FlatVertex> {
        let mut out = vec![];
        let k = &k.g.slide_icon;
        let mut poly = |v: &[Vec2], color: [u8; 4]| {
            assert!(v.len() >= 3);
            for i in 1..v.len() - 1 {
                for j in [0, i, i + 1] {
                    out.push(FlatVertex {
                        pos: v[j].to_array(),
                        color,
                    });
                }
            }
        };
        let mut circ = vec![];
        for i in 0..k.detail {
            circ.push(
                Vec2::from_angle(std::f32::consts::TAU * i as f32 / k.detail as f32) * k.radius,
            );
        }
        poly(&circ, k.bg);
        let mut tri = [vec2(0., -1.), vec2(1., 0.), vec2(0., 1.)];
        for v in tri.iter_mut() {
            *v *= k.arrow_size;
            *v += vec2(k.arrow_shift, 0.);
        }
        for _ in 0..4 {
            poly(&tri, k.fg);
            for v in tri.iter_mut() {
                *v = v.perp();
            }
        }
        out
    }
}

struct TextScope {
    queue: Vec<Glyph>,
    buf: VertexBuf<TextVertex>,
}
impl TextScope {
    fn new(display: &Display) -> Result<Self> {
        Ok(Self {
            queue: default(),
            buf: VertexBuf::new(display)?,
        })
    }

    fn clear(&mut self) {
        self.queue.clear();
        self.buf.clear();
    }

    fn push(&mut self, cache: &mut DrawCache, g: Glyph) {
        self.queue.push(g.clone());
        cache.queue_glyph(0, g);
    }

    fn upload_verts(
        &mut self,
        color: [u8; 4],
        cache: &mut DrawCache,
        display: &Display,
    ) -> Result<()> {
        // Process the glyph queue and generate vertices/indices
        for g in self.queue.iter() {
            if let Some((tex, pos)) = cache.rect_for(0, g) {
                macro_rules! vert {
                    ($x:ident, $y:ident) => {{
                        self.buf.push(TextVertex {
                            pos: [pos.$x.x, pos.$y.y],
                            uv: [tex.$x.x, tex.$y.y],
                            color,
                        });
                    }};
                }
                vert!(min, min);
                vert!(max, min);
                vert!(max, max);
                vert!(min, min);
                vert!(max, max);
                vert!(min, max);
            }
        }

        // Upload the vertices
        self.buf.upload(display)?;

        Ok(())
    }

    fn draw(
        &self,
        frame: &mut Frame,
        shader: &Program,
        uniforms: &impl Uniforms,
        draw_params: &DrawParameters,
    ) -> Result<()> {
        frame.draw(
            self.buf.vbo(),
            IndicesSource::NoIndices {
                primitives: PrimitiveType::TrianglesList,
            },
            shader,
            uniforms,
            draw_params,
        )?;
        Ok(())
    }
}

fn load_shader(display: &Display, name: &str) -> Result<Program> {
    use gl::glium::program;
    let vertex = fs::read_to_string(&format!("shader/{}.vert", name))
        .with_context(|| format!("failed to read {} vertex shader", name))?;
    let fragment = fs::read_to_string(&format!("shader/{}.frag", name))
        .with_context(|| format!("failed to read {} vertex shader", name))?;
    Ok(program!(display,
        330 => {
            vertex: &*vertex,
            fragment: &*fragment,
        }
    )?)
}

pub struct DrawState {
    font: FontArc,
    glyphs: DrawCache,
    texture: Texture2d,
    text: TextScope,
    linenums: TextScope,
    text_shader: Program,
    flat_shader: Program,
    slide_icon: Vec<FlatVertex>,
    aux_vbo: VertexBuf<FlatVertex>,
}
impl DrawState {
    pub fn new(display: &Display, font: &FontArc, k: &Cfg) -> Result<Self> {
        let cache_size = (512, 512);
        Ok(Self {
            glyphs: DrawCache::builder()
                .dimensions(cache_size.0, cache_size.0)
                .position_tolerance(1.)
                .build(),
            font: font.clone(),
            texture: Texture2d::empty(display, cache_size.0, cache_size.1)?,
            text: TextScope::new(display)?,
            linenums: TextScope::new(display)?,
            text_shader: load_shader(display, "text")?,
            flat_shader: load_shader(display, "flat")?,
            slide_icon: VertexBuf::build_slide_icon(k),
            aux_vbo: VertexBuf::new(display)?,
        })
    }
}

struct FrameWrap {
    frame: mem::ManuallyDrop<Frame>,
}
impl ops::Deref for FrameWrap {
    type Target = Frame;
    fn deref(&self) -> &Frame {
        &self.frame
    }
}
impl ops::DerefMut for FrameWrap {
    fn deref_mut(&mut self) -> &mut Frame {
        &mut self.frame
    }
}
impl FrameWrap {
    fn into_inner(mut self) -> Frame {
        // SAFETY: Safe to take out because `self` is forgotten
        unsafe {
            let frame = mem::ManuallyDrop::take(&mut self.frame);
            mem::forget(self);
            frame
        }
    }
}
impl Drop for FrameWrap {
    fn drop(&mut self) {
        // SAFETY: After dropping the frame will never be accessed
        unsafe {
            if let Err(err) = mem::ManuallyDrop::take(&mut self.frame).finish() {
                println!("frame was emergency-dropped and raised an error: {:#}", err);
            }
        }
    }
}

/// Returns `true` if the backend is still loading and it would
/// be good to redraw after a certain timeout to include newly
/// loaded data.
pub fn draw(state: &mut WindowState) -> Result<bool> {
    let start = Instant::now();

    let mut frame = FrameWrap {
        frame: mem::ManuallyDrop::new(state.display.draw()),
    };
    {
        let [r, g, b, a] = state.k.g.bg_color;
        let s = 255f32.recip();
        frame.clear_color(r as f32 * s, g as f32 * s, b as f32 * s, a as f32 * s);
    }
    let (w, h) = frame.get_dimensions();
    state.last_size = (w, h);

    // Go through file characters, queueing the text for rendering
    let prefile = Instant::now();
    let mut textqueue = Duration::ZERO;
    state.draw.text.clear();
    state.draw.linenums.clear();
    state.draw.aux_vbo.clear();
    let mut all_loaded = true;
    if let Some(filebuf) = state.file.as_ref() {
        // Lock the shared file data
        // We want to do this only once, to minimize latency
        let mut file = filebuf.lock();

        // Determine the bounds of the loaded area, and clamp the scroll position to it
        let scroll_bounds = file.bounding_rect(state.scroll.pos.base_offset);
        state.scroll.pos = scroll_bounds.clamp_pos(state.scroll.pos);
        state.scroll.last_view = FileRect {
            corner: state.scroll.pos,
            size: (state.k.file_view_bounds((w, h)).1 / state.k.g.font_height).as_dvec2(),
        };

        // Only update bounds if not dragging the scrollbar
        // This makes the scrollbar-drag experience much smoother
        // while the file is still being loaded
        if !state.drag.is_scrollbar() {
            state.scroll.last_bounds = scroll_bounds;
        }

        // Transform the selected area from position to absolute offsets
        let selected = state.selected_offsets(&file);

        // Iterate over all characters on the screen and queue them up for rendering
        let mut linenum_buf = String::new();
        let mut sel_box = (0., 0., 0.);
        file.visit_rect(state.scroll.last_view, |offset, dx, dy, c| {
            let inner_start = Instant::now();
            let (p, _s) = state.k.file_view_bounds((w, h));
            match c {
                None => {
                    // Starting a line
                    // Write line number
                    use std::fmt::Write;
                    linenum_buf.clear();
                    let _ = write!(linenum_buf, "{}", dy + 1);
                    let mut x = p.x - state.k.g.linenum_pad;
                    let y = p.y
                        + ((dy + 1) as f64 - state.scroll.pos.delta_y) as f32
                            * state.k.g.font_height;
                    for c in linenum_buf.bytes().rev() {
                        x -= filebuf.advance_for(c as char) as f32 * state.k.g.font_height;
                        state.draw.linenums.push(
                            &mut state.draw.glyphs,
                            Glyph {
                                id: state.draw.font.glyph_id(c as char),
                                scale: state.k.g.font_height.into(),
                                position: (x, y).into(),
                            },
                        );
                    }
                    // Draw previous selection box
                    let (y, x0, x1) = &mut sel_box;
                    if x1 > x0 {
                        state.draw.aux_vbo.push_quad(
                            vec2(*x0, *y),
                            vec2(*x1 - *x0, state.k.g.font_height),
                            state.k.g.selection_color,
                        );
                    }
                    *y =
                        p.y + (dy as f64 - state.scroll.pos.delta_y) as f32 * state.k.g.font_height;
                    *x0 = f32::INFINITY;
                    *x1 = f32::NEG_INFINITY;
                }
                Some((c, hadv)) => {
                    // Process a single character
                    // Figure out screen position of this character
                    let pos = p + dvec2(
                        dx - state.scroll.pos.delta_x,
                        (dy + 1) as f64 - state.scroll.pos.delta_y,
                    )
                    .as_vec2()
                        * state.k.g.font_height;
                    // Create and queue the glyph
                    let g = Glyph {
                        id: state.draw.font.glyph_id(c),
                        scale: state.k.g.font_height.into(),
                        position: pos.to_array().into(),
                    };
                    state.draw.text.push(&mut state.draw.glyphs, g);
                    // If the character is selected, make sure the selection box wraps it
                    if selected
                        .as_ref()
                        .map(|r| r.start <= offset && offset < r.end)
                        .unwrap_or(false)
                    {
                        let (_y, x0, x1) = &mut sel_box;
                        *x0 = x0.min(pos.x);
                        *x1 = x1.max(pos.x + hadv as f32 * state.k.g.font_height);
                    }
                }
            }
            textqueue += inner_start.elapsed();
        });
        {
            let (y, x0, x1) = &sel_box;
            if x1 > x0 {
                state.draw.aux_vbo.push_quad(
                    vec2(*x0, *y),
                    vec2(*x1 - *x0, state.k.g.font_height),
                    state.k.g.selection_color,
                );
            }
        }

        // If the backend is not idle, we should render periodically to show any updates
        all_loaded = file.is_backend_idle();
        // Inform the backend about what area of the file to load (and keep loaded)
        file.set_hot_area(state.scroll.last_view);
    } else {
        state.scroll.last_bounds = default();
        state.scroll.last_view = default();
    }

    let preuploadtex = Instant::now();

    // Process the queued glyphs, uploading their rasterized images to the GPU
    let res = state
        .draw
        .glyphs
        .cache_queued(&[&state.draw.font], |rect, data| {
            state.draw.texture.write(
                gl::glium::Rect {
                    left: rect.min[0],
                    bottom: rect.min[1],
                    width: rect.max[0] - rect.min[0],
                    height: rect.max[1] - rect.min[1],
                },
                gl::glium::texture::RawImage2d {
                    data: data.into(),
                    width: rect.max[0] - rect.min[0],
                    height: rect.max[1] - rect.min[1],
                    format: gl::glium::texture::ClientFormat::U8,
                },
            );
        });
    if let Err(err) = res {
        println!("failed to write font cache: {:#}", err);
    }

    let preuploadvert = Instant::now();

    // Generate and upload the vertex data
    state
        .draw
        .text
        .upload_verts(state.k.g.text_color, &mut state.draw.glyphs, &state.display)?;
    state.draw.linenums.upload_verts(
        state.k.g.linenum_color,
        &mut state.draw.glyphs,
        &state.display,
    )?;

    let predraw = Instant::now();

    //Draw the text and line numbers
    let mvp = Mat4::orthographic_rh_gl(0., w as f32, h as f32, 0., -1., 1.);
    {
        let uniforms = gl::glium::uniform! {
            glyph: state.draw.texture.sampled()
                .magnify_filter(MagnifySamplerFilter::Nearest)
                .minify_filter(MinifySamplerFilter::Nearest),
            mvp: mvp.to_cols_array_2d(),
        };
        let (p, s) = state.k.file_view_bounds((w, h));
        state.draw.text.draw(
            &mut frame,
            &state.draw.text_shader,
            &uniforms,
            &DrawParameters {
                blend: Blend::alpha_blending(),
                scissor: Some(gl::glium::Rect {
                    left: p.x.ceil() as u32,
                    bottom: (h as f32 - (p.y + s.y)).ceil() as u32,
                    width: s.x.floor() as u32,
                    height: s.y.floor() as u32,
                }),
                ..default()
            },
        )?;
        state.draw.linenums.draw(
            &mut frame,
            &state.draw.text_shader,
            &uniforms,
            &DrawParameters {
                blend: Blend::alpha_blending(),
                ..default()
            },
        )?;
    }

    // Draw scrollbars
    {
        let ydraw = state.scroll.ydraw(&state.k);
        let xdraw = state.scroll.xdraw(&state.k);

        if ydraw {
            // Draw the vertical scrollbar background
            let (p, s) = state.scroll.y_scrollbar_bounds(&state.k, w, h);
            state
                .draw
                .aux_vbo
                .push_quad(p, s, state.k.g.scrollbar_color);

            // Draw the vertical scrollbar handle
            let (p, s) = state.scroll.y_scrollhandle_bounds(&state.k, w, h);
            state
                .draw
                .aux_vbo
                .push_quad(p, s, state.k.g.scrollhandle_color);
        }

        if xdraw {
            // Draw the horizontal scrollbar background
            let (p, s) = state.scroll.x_scrollbar_bounds(&state.k, w, h);
            state
                .draw
                .aux_vbo
                .push_quad(p, s, state.k.g.scrollbar_color);

            // Draw the horizontal scrollbar handle
            let (p, s) = state.scroll.x_scrollhandle_bounds(&state.k, w, h);
            state
                .draw
                .aux_vbo
                .push_quad(p, s, state.k.g.scrollhandle_color);
        }

        if xdraw && ydraw {
            // Draw the scrollbar corner
            let (yp, ys) = state.scroll.y_scrollbar_bounds(&state.k, w, h);
            let (xp, xs) = state.scroll.x_scrollbar_bounds(&state.k, w, h);
            state.draw.aux_vbo.push_quad(
                vec2(yp.x, xp.y),
                vec2(ys.x, xs.y),
                state.k.g.scrollcorner_color,
            );
        }
    }

    // Draw the slide icon if sliding
    if let Drag::Slide { screen_base, .. } = &state.drag {
        state
            .draw
            .aux_vbo
            .push_prebuilt(&state.draw.slide_icon, screen_base.as_vec2().round());
    }

    // Draw the auxiliary decorations
    state.draw.aux_vbo.upload(&state.display)?;
    frame.draw(
        state.draw.aux_vbo.vbo(),
        IndicesSource::NoIndices {
            primitives: PrimitiveType::TrianglesList,
        },
        &state.draw.flat_shader,
        &gl::glium::uniform! {
            tint: [1f32; 4],
            mvp: mvp.to_cols_array_2d(),
        },
        &DrawParameters {
            blend: Blend::alpha_blending(),
            ..default()
        },
    )?;

    let preswap = Instant::now();

    // Swap frame (possibly waiting for vsync)
    frame.into_inner().finish()?;

    // Log timings if enabled
    let finish = Instant::now();
    if state.k.log.frame_timing {
        eprint!(
            "timings:
    frame init: {:3}ms
    total file access: {:3}ms
    total text queueing: {:3}ms
    texture upload: {:3}ms
    vertex upload: {:3}ms
    draw call: {:3}ms
    swap: {:3}ms
",
            (prefile - start).as_secs_f64() * 1000.,
            (preuploadtex - prefile - textqueue).as_secs_f64() * 1000.,
            textqueue.as_secs_f64() * 1000.,
            (preuploadvert - preuploadtex).as_secs_f64() * 1000.,
            (predraw - preuploadvert).as_secs_f64() * 1000.,
            (preswap - predraw).as_secs_f64() * 1000.,
            (finish - preswap).as_secs_f64() * 1000.,
        );
    }

    Ok(!all_loaded)
}
