use crate::{filebuf::ScrollRect, prelude::*, WindowState};
use ab_glyph::{Font, Glyph};
use gl::glium::{
    index::{IndicesSource, PrimitiveType},
    uniforms::{MagnifySamplerFilter, MinifySamplerFilter, Uniforms},
    Blend, DrawParameters, Frame, Program, Surface, Texture2d, VertexBuffer,
};
use glyph_brush_draw_cache::DrawCache;

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

struct TextScope {
    queue: Vec<Glyph>,
    vertex_buf: Vec<TextVertex>,
    vbo: VertexBuffer<TextVertex>,
    vbo_len: usize,
}
impl TextScope {
    fn new(display: &Display) -> Result<Self> {
        Ok(Self {
            queue: default(),
            vertex_buf: default(),
            vbo: VertexBuffer::empty_dynamic(display, 1024)?,
            vbo_len: 0,
        })
    }

    fn clear(&mut self) {
        self.queue.clear();
        self.vertex_buf.clear();
    }

    fn push(&mut self, cache: &mut DrawCache, g: Glyph) {
        self.queue.push(g.clone());
        cache.queue_glyph(0, g);
    }

    fn upload_verts(
        &mut self,
        color: [f32; 4],
        cache: &mut DrawCache,
        display: &Display,
    ) -> Result<()> {
        // Process the glyph queue and generate vertices/indices
        let color = color.map(|f| (f * 255.).clamp(0., 255.) as u8);
        for g in self.queue.iter() {
            if let Some((tex, pos)) = cache.rect_for(0, g) {
                macro_rules! vert {
                    ($x:ident, $y:ident) => {{
                        self.vertex_buf.push(TextVertex {
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
        let verts = &self.vertex_buf[..];
        if verts.len() > self.vbo.len() {
            self.vbo = VertexBuffer::empty_dynamic(display, verts.len().next_power_of_two())?;
        }
        if !verts.is_empty() {
            self.vbo.slice(0..verts.len()).unwrap().write(verts);
        }
        self.vbo_len = verts.len();

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
            self.vbo.slice(0..self.vbo_len).unwrap(),
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

pub struct DrawState {
    font: FontArc,
    glyphs: DrawCache,
    texture: Texture2d,
    text: TextScope,
    linenums: TextScope,
    shader: Program,
}
impl DrawState {
    pub fn new(display: &Display, font: &FontArc) -> Result<Self> {
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
            shader: {
                use gl::glium::program;
                let vertex = fs::read_to_string("shader/text.vert")
                    .context("failed to read text vertex shader")?;
                let fragment = fs::read_to_string("shader/text.frag")
                    .context("failed to read text vertex shader")?;
                program!(display,
                    330 => {
                        vertex: &*vertex,
                        fragment: &*fragment,
                    }
                )?
            },
        })
    }
}

/// Returns `true` if the backend is still loading and it would
/// be good to redraw after a certain timeout to include newly
/// loaded data.
pub fn draw(state: &mut WindowState) -> Result<bool> {
    let start = Instant::now();

    let mut frame = state.display.draw();
    {
        let [r, g, b, a] = state.k.bg_color;
        frame.clear_color(r, g, b, a);
    }
    let (w, h) = frame.get_dimensions();

    let text_view = dvec2(
        (w as f64 - state.k.left_bar as f64) / state.k.font_height as f64,
        h as f64 / state.k.font_height as f64,
    );

    // Go through file characters, queueing the text for rendering
    let prefile = Instant::now();
    let mut textqueue = Duration::ZERO;
    state.draw.text.clear();
    state.draw.linenums.clear();
    let mut all_loaded = true;
    if let Some(filebuf) = state.file.as_ref() {
        // Lock the shared file data
        // We want to do this only once, to minimize latency
        let mut file = filebuf.lock();
        // Clamp the scroll window to the loaded bounds
        file.clamp_scroll(&mut state.scroll);
        // Iterate over all characters on the screen and queue them up for rendering
        let mut linenum_buf = String::new();
        all_loaded = file.visit_rect(
            ScrollRect {
                corner: state.scroll,
                size: text_view,
            },
            |dx, dy, c| {
                let inner_start = Instant::now();
                match c {
                    None => {
                        // Starting a line
                        use std::fmt::Write;
                        linenum_buf.clear();
                        let _ = write!(linenum_buf, "{}", dy + 1);
                        let mut x = state.k.left_bar - state.k.linenum_pad;
                        let y =
                            ((dy + 1) as f64 - state.scroll.delta_y) as f32 * state.k.font_height;
                        for c in linenum_buf.bytes().rev() {
                            x -= filebuf.advance_for(c as char) as f32 * state.k.font_height;
                            state.draw.linenums.push(
                                &mut state.draw.glyphs,
                                Glyph {
                                    id: state.draw.font.glyph_id(c as char),
                                    scale: state.k.font_height.into(),
                                    position: (x, y).into(),
                                },
                            );
                        }
                    }
                    Some(c) => {
                        // Process a single character
                        let pos = dvec2(
                            dx - state.scroll.delta_x,
                            (dy + 1) as f64 - state.scroll.delta_y,
                        )
                        .as_vec2()
                            * state.k.font_height;
                        let g = Glyph {
                            id: state.draw.font.glyph_id(c),
                            scale: state.k.font_height.into(),
                            position: (state.k.left_bar + pos.x, pos.y).into(),
                        };
                        state.draw.text.push(&mut state.draw.glyphs, g);
                    }
                }
                textqueue += inner_start.elapsed();
            },
        );
    }

    // Process the queued glyphs, uploading their rasterized images to the GPU
    let preuploadtex = Instant::now();

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
        eprintln!("failed to write font cache: {:#}", err);
    }

    let preuploadvert = Instant::now();

    state
        .draw
        .text
        .upload_verts(state.k.text_color, &mut state.draw.glyphs, &state.display)?;
    state.draw.linenums.upload_verts(
        state.k.linenum_color,
        &mut state.draw.glyphs,
        &state.display,
    )?;

    let predraw = Instant::now();

    let mvp = Mat4::orthographic_rh_gl(0., w as f32, h as f32, 0., -1., 1.);
    let uniforms = gl::glium::uniform! {
        glyph: state.draw.texture.sampled()
            .magnify_filter(MagnifySamplerFilter::Nearest)
            .minify_filter(MinifySamplerFilter::Nearest),
        mvp: mvp.to_cols_array_2d(),
    };
    state.draw.text.draw(
        &mut frame,
        &state.draw.shader,
        &uniforms,
        &DrawParameters {
            blend: Blend::alpha_blending(),
            scissor: Some(gl::glium::Rect {
                left: state.k.left_bar.round() as u32,
                bottom: 0,
                width: w - state.k.left_bar.round() as u32,
                height: h,
            }),
            ..default()
        },
    )?;
    state.draw.linenums.draw(
        &mut frame,
        &state.draw.shader,
        &uniforms,
        &DrawParameters {
            blend: Blend::alpha_blending(),
            ..default()
        },
    )?;

    let preswap = Instant::now();

    frame.finish()?;

    let finish = Instant::now();
    if state.k.log_frame_timing {
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
