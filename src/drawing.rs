use std::mem::ManuallyDrop;

use crate::{cfg::Cfg, prelude::*, ScreenRect, WindowState};
use ab_glyph::Glyph;
use gl::glium::{
    index::{IndicesSource, PrimitiveType},
    uniforms::{MagnifySamplerFilter, MinifySamplerFilter, Uniforms},
    vertex::VertexBufferSlice,
    Blend, DrawParameters, Frame, Program, Surface, Texture2d, VertexBuffer,
};
use glyph_brush_draw_cache::DrawCache;

pub const TRIANGLES_LIST: IndicesSource = IndicesSource::NoIndices {
    primitives: PrimitiveType::TrianglesList,
};

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

pub struct VertexBuf<T: Copy> {
    buf: Vec<T>,
    vbo: VertexBuffer<T>,
    vbo_len: usize,
}
impl<T: Copy + gl::glium::Vertex> VertexBuf<T> {
    pub fn new(display: &Display) -> Result<Self> {
        Ok(Self {
            buf: default(),
            vbo: VertexBuffer::empty_dynamic(display, 1024)?,
            vbo_len: 0,
        })
    }

    pub fn clear(&mut self) {
        self.buf.clear();
        self.vbo_len = 0;
    }

    pub fn push(&mut self, v: T) {
        self.buf.push(v);
    }

    pub fn upload(&mut self, display: &Display) -> Result<()> {
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

    pub fn vbo(&self) -> VertexBufferSlice<T> {
        self.vbo.slice(..self.vbo_len).unwrap()
    }
}
impl VertexBuf<FlatVertex> {
    pub fn push_quad(&mut self, rect: ScreenRect, color: [u8; 4]) {
        let (o, x, y) = (rect.min, vec2(rect.size().x, 0.), vec2(0., rect.size().y));
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

    pub fn push_prebuilt(&mut self, prebuilt: &[FlatVertex], offset: Vec2) {
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

pub struct TextScope {
    queue: Vec<(Glyph, [u8; 4])>,
    buf: VertexBuf<TextVertex>,
}
impl TextScope {
    pub fn new(display: &Display) -> Result<Self> {
        Ok(Self {
            queue: default(),
            buf: VertexBuf::new(display)?,
        })
    }

    pub fn clear(&mut self) {
        self.queue.clear();
        self.buf.clear();
    }

    pub fn push(&mut self, cache: &mut DrawCache, color: [u8; 4], g: Glyph) {
        self.queue.push((g.clone(), color));
        cache.queue_glyph(0, g);
    }

    pub fn upload_verts(&mut self, cache: &mut DrawCache, display: &Display) -> Result<()> {
        // Process the glyph queue and generate vertices/indices
        for (g, color) in self.queue.iter() {
            if let Some((tex, pos)) = cache.rect_for(0, g) {
                macro_rules! vert {
                    ($x:ident, $y:ident) => {{
                        self.buf.push(TextVertex {
                            pos: [pos.$x.x, pos.$y.y],
                            uv: [tex.$x.x, tex.$y.y],
                            color: *color,
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

    pub fn draw(
        &self,
        frame: &mut Frame,
        shader: &Program,
        uniforms: &impl Uniforms,
        draw_params: &DrawParameters,
    ) -> Result<()> {
        frame.draw(
            self.buf.vbo(),
            TRIANGLES_LIST,
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
    pub font: FontArc,
    pub glyphs: DrawCache,
    pub texture: Texture2d,
    pub text: TextScope,
    pub linenums: TextScope,
    pub sel_vbo: VertexBuf<FlatVertex>,
    pub text_shader: Program,
    pub flat_shader: Program,
    pub slide_icon: Vec<FlatVertex>,
    pub aux_vbo: VertexBuf<FlatVertex>,
    pub aux_text: TextScope,
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
            sel_vbo: VertexBuf::new(display)?,
            text_shader: load_shader(display, "text")?,
            flat_shader: load_shader(display, "flat")?,
            slide_icon: VertexBuf::build_slide_icon(k),
            aux_vbo: VertexBuf::new(display)?,
            aux_text: TextScope::new(display)?,
        })
    }
}

pub struct FrameCtx {
    pub frame: ManuallyDrop<Frame>,
    pub size: (u32, u32),
    pub mvp: Mat4,
}
impl FrameCtx {
    fn into_frame(mut self) -> Frame {
        // SAFETY: Safe to take out because `self` is forgotten
        unsafe {
            let frame = ManuallyDrop::take(&mut self.frame);
            mem::forget(self);
            frame
        }
    }
}
impl Drop for FrameCtx {
    fn drop(&mut self) {
        // SAFETY: After dropping the frame will never be accessed
        unsafe {
            if let Err(err) = ManuallyDrop::take(&mut self.frame).finish() {
                println!("frame was emergency-dropped and raised an error: {:#}", err);
            }
        }
    }
}

/// Returns `true` if the backend is still loading and it would
/// be good to redraw after a certain timeout to include newly
/// loaded data.
pub fn draw(state: &mut WindowState) -> Result<()> {
    // Initialize frame
    let frame = state.display.draw();
    let (w, h) = frame.get_dimensions();
    state.screen = ScreenRect {
        min: vec2(0., 0.),
        max: vec2(w as f32, h as f32),
    };
    let mut ctx = FrameCtx {
        frame: mem::ManuallyDrop::new(frame),
        size: (w, h),
        mvp: Mat4::orthographic_rh_gl(0., w as f32, h as f32, 0., -1., 1.),
    };

    // Reset frame
    {
        let [r, g, b, a] = state.k.g.bg_color;
        let s = 255f32.recip();
        ctx.frame
            .clear_color(r as f32 * s, g as f32 * s, b as f32 * s, a as f32 * s);
    }
    state.draw.text.clear();
    state.draw.linenums.clear();
    state.draw.sel_vbo.clear();
    state.draw.aux_vbo.clear();
    state.draw.aux_text.clear();

    // Draw file text, and anything else that requires locking the shared file block
    if let Some(mut fview) = state.take_fview(state.cur_tab) {
        crate::fileview::drawing::draw_withtext(state, &mut fview)?;
        state.put_fview(state.cur_tab, fview);
    }

    // Draw the tab list
    {
        let tabs_view = WindowState::tab_bar_bounds(&state.k, state.screen);
        state
            .draw
            .aux_vbo
            .push_quad(tabs_view, state.k.g.tab_bg_color);

        for (i, _tab) in state.tabs.iter().enumerate() {
            let active = i == state.cur_tab;
            let active_idx = (!active) as usize;
            let tab_view = WindowState::tab_bounds(&state.k, i, state.tabs.len(), state.screen);
            // let [top, rt, bot, lt] = state.k.g.tab_padding;
            // let fonth = state.k.g.tab_height - top - bot;
            state
                .draw
                .aux_vbo
                .push_quad(tab_view, state.k.g.tab_fg_color[active_idx]);
        }
    }

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

    // Generate and upload the text vertex data
    state.draw.sel_vbo.upload(&state.display)?;
    state
        .draw
        .text
        .upload_verts(&mut state.draw.glyphs, &state.display)?;
    state
        .draw
        .linenums
        .upload_verts(&mut state.draw.glyphs, &state.display)?;
    state
        .draw
        .aux_text
        .upload_verts(&mut state.draw.glyphs, &state.display)?;

    // Draw non-text file view components
    if let Some(mut fview) = state.take_fview(state.cur_tab) {
        crate::fileview::drawing::draw_notext(state, &mut fview, &mut ctx)?;
        state.put_fview(state.cur_tab, fview);
    }

    // Draw the auxiliary decorations
    state.draw.aux_vbo.upload(&state.display)?;
    ctx.frame.draw(
        state.draw.aux_vbo.vbo(),
        TRIANGLES_LIST,
        &state.draw.flat_shader,
        &gl::glium::uniform! {
            tint: [1f32; 4],
            mvp: ctx.mvp.to_cols_array_2d(),
        },
        &DrawParameters {
            blend: Blend::alpha_blending(),
            ..default()
        },
    )?;

    // Draw the text overlay above decorations
    state.draw.aux_text.draw(
        &mut ctx.frame,
        &state.draw.text_shader,
        &gl::glium::uniform! {
            glyph: state.draw.texture.sampled()
                .magnify_filter(MagnifySamplerFilter::Nearest)
                .minify_filter(MinifySamplerFilter::Nearest),
            mvp: ctx.mvp.to_cols_array_2d(),
        },
        &DrawParameters {
            blend: Blend::alpha_blending(),
            ..default()
        },
    )?;

    // Swap frame (possibly waiting for vsync)
    ctx.into_frame().finish()?;

    Ok(())
}
