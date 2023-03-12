use crate::{prelude::*, WindowState};
use gl::glium::{
    index::{IndicesSource, PrimitiveType},
    uniforms::{MagnifySamplerFilter, MinifySamplerFilter},
    Blend, DrawParameters, Program, Surface, Texture2d, VertexBuffer,
};
use glyph_brush::{
    ab_glyph::FontArc, BrushAction, GlyphBrush, GlyphBrushBuilder, HorizontalAlign, Layout,
    Section, Text,
};

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

pub struct DrawState {
    glyph: GlyphBrush<[TextVertex; 6]>,
    texture: Texture2d,
    vbo: VertexBuffer<TextVertex>,
    vbo_len: usize,
    shader: Program,
}
impl DrawState {
    pub fn new(display: &Display, font: &FontArc) -> Result<Self> {
        let cache_size = (512, 512);
        Ok(Self {
            glyph: GlyphBrushBuilder::using_font(font.clone())
                .initial_cache_size(cache_size)
                .build(),
            texture: Texture2d::empty(display, cache_size.0, cache_size.1)?,
            vbo: VertexBuffer::empty_dynamic(display, 1024)?,
            vbo_len: 0,
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

pub fn draw(state: &mut WindowState) -> Result<()> {
    let start = Instant::now();

    let mut frame = state.display.draw();
    {
        let [r, g, b, a] = state.k.bg_color;
        frame.clear_color(r, g, b, a);
    }
    let (w, h) = frame.get_dimensions();

    let min_draw = dvec2(state.scroll.delta_x, state.scroll.delta_y);
    let max_draw = min_draw
        + dvec2(
            (w as f64 - state.k.left_bar as f64) / state.k.font_height as f64,
            h as f64 / state.k.font_height as f64,
        );

    let prelock = Instant::now();
    let mut prefile = Instant::now();

    if let Some(file) = state.file.as_ref() {
        // TODO: Line numbers
        let mut linenum_buf = String::new();
        file.iter_lines(
            state.scroll.base_offset,
            min_draw,
            max_draw,
            |dx, dy, rawtext| {
                prefile = Instant::now();

                // Draw the visible window of this line
                let text = String::from_utf8_lossy(rawtext);
                let pos =
                    dvec2(dx - min_draw.x, dy as f64 - min_draw.y).as_vec2() * state.k.font_height;
                state.draw.glyph.queue(
                    Section::new()
                        .add_text(
                            Text::new(&text)
                                .with_scale(state.k.font_height)
                                .with_color(state.k.text_color),
                        )
                        .with_screen_position((state.k.left_bar + pos.x, pos.y))
                        .with_layout(Layout::default()),
                );

                // Draw the line number
                linenum_buf.clear();
                {
                    use std::fmt::Write;
                    let _ = write!(linenum_buf, "{}", dy + 1);
                }
                let linenum_x = state.k.left_bar - state.k.linenum_pad;
                state.draw.glyph.queue(
                    Section::new()
                        .add_text(
                            Text::new(&linenum_buf)
                                .with_scale(state.k.font_height)
                                .with_color(state.k.linenum_color),
                        )
                        .with_screen_position((linenum_x, pos.y))
                        .with_bounds((linenum_x, h as f32 - pos.y))
                        .with_layout(Layout::default_single_line().h_align(HorizontalAlign::Right)),
                );
            },
        );
        file.set_hot_pos(&mut state.scroll);
        /*
        file.access_data(|linemap, data| {
            prefile = Instant::now();
            let mut linenum_buf = String::new();
            for lines in linemap.iter(linerange.0, linerange.1) {
                eprintln!(
                    "drawing lines {} to {}, offsets {} to {}",
                    lines.start.line, lines.end.line, lines.start.offset, lines.end.offset
                );
                if let Some((_offset, rawseg)) =
                    data.iter(lines.start.offset, lines.end.offset).next()
                {
                    let text = String::from_utf8_lossy(rawseg);
                    let y = (lines.start.line as f64 - state.scroll) as f32 * state.k.font_height;
                    state.draw.glyph.queue(
                        Section::new()
                            .add_text(
                                Text::new(&text)
                                    .with_scale(state.k.font_height)
                                    .with_color(state.k.text_color),
                            )
                            .with_screen_position((state.k.left_bar, y))
                            .with_layout(Layout::default_single_line()),
                    );
                    linenum_buf.clear();
                    for l in lines.start.line..lines.end.line {
                        use std::fmt::Write;
                        let _ = write!(linenum_buf, "{}\n", l);
                    }
                    let linenum_x = state.k.left_bar - state.k.linenum_pad;
                    state.draw.glyph.queue(
                        Section::new()
                            .add_text(
                                Text::new(&linenum_buf)
                                    .with_scale(state.k.font_height)
                                    .with_color(state.k.linenum_color),
                            )
                            .with_screen_position((linenum_x, y))
                            .with_bounds((linenum_x, h as f32 - y))
                            .with_layout(
                                Layout::default_single_line().h_align(HorizontalAlign::Right),
                            ),
                    );
                }
            }
        });*/
        //file.set_hot_line((linerange.0 + linerange.1) / 2);
    }

    let preuploadtex = Instant::now();

    let draw_text = state.draw.glyph.process_queued(
        |rect, data| {
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
        },
        |vert| {
            let color = [
                (vert.extra.color[0] * 255.).clamp(0., 255.) as u8,
                (vert.extra.color[1] * 255.).clamp(0., 255.) as u8,
                (vert.extra.color[2] * 255.).clamp(0., 255.) as u8,
                (vert.extra.color[3] * 255.).clamp(0., 255.) as u8,
            ];
            macro_rules! vert {
                ($x:ident, $y:ident) => {{
                    TextVertex {
                        pos: [vert.pixel_coords.$x.x, vert.pixel_coords.$y.y],
                        uv: [vert.tex_coords.$x.x, vert.tex_coords.$y.y],
                        color,
                    }
                }};
            }
            [
                vert!(min, min),
                vert!(max, min),
                vert!(max, max),
                vert!(min, min),
                vert!(max, max),
                vert!(min, max),
            ]
        },
    )?;

    let preuploadvert = Instant::now();

    match draw_text {
        BrushAction::Draw(verts_grouped) => {
            let verts = unsafe {
                std::slice::from_raw_parts(
                    verts_grouped.as_ptr() as *const TextVertex,
                    verts_grouped.len() * 6,
                )
            };
            if verts.len() > state.draw.vbo.len() {
                state.draw.vbo =
                    VertexBuffer::empty_dynamic(&state.display, verts.len().next_power_of_two())?;
            }
            if !verts.is_empty() {
                state.draw.vbo.slice(0..verts.len()).unwrap().write(verts);
            }
            state.draw.vbo_len = verts.len();
        }
        BrushAction::ReDraw => {}
    }

    let predraw = Instant::now();

    let mvp = Mat4::orthographic_rh_gl(0., w as f32, h as f32, 0., -1., 1.);
    let uniforms = gl::glium::uniform! {
        glyph: state.draw.texture.sampled()
            .magnify_filter(MagnifySamplerFilter::Nearest)
            .minify_filter(MinifySamplerFilter::Nearest),
        mvp: mvp.to_cols_array_2d(),
    };
    let draw_parameters = DrawParameters {
        blend: Blend::alpha_blending(),
        ..default()
    };
    frame.draw(
        state.draw.vbo.slice(0..state.draw.vbo_len).unwrap(),
        IndicesSource::NoIndices {
            primitives: PrimitiveType::TrianglesList,
        },
        &state.draw.shader,
        &uniforms,
        &draw_parameters,
    )?;

    let preswap = Instant::now();

    frame.finish()?;

    let finish = Instant::now();
    eprint!(
        "timings:
    file lock: {:3}ms
    text queueing: {:3}ms
    texture upload: {:3}ms
    vertex upload: {:3}ms
    draw call: {:3}ms
    swap: {:3}ms
",
        (prefile - prelock).as_secs_f64() * 1000.,
        (preuploadtex - prefile).as_secs_f64() * 1000.,
        (preuploadvert - preuploadtex).as_secs_f64() * 1000.,
        (predraw - preuploadvert).as_secs_f64() * 1000.,
        (preswap - predraw).as_secs_f64() * 1000.,
        (finish - preswap).as_secs_f64() * 1000.,
    );

    Ok(())
}
