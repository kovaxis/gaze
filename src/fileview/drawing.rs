use ab_glyph::{Font, Glyph};
use gl::glium::{
    uniforms::{MagnifySamplerFilter, MinifySamplerFilter},
    Blend, DrawParameters, Surface,
};

use crate::{
    drawing::{FrameCtx, TRIANGLES_LIST},
    filebuf::FileRect,
    fileview::FileView,
    prelude::*,
    ScreenRect, WindowState,
};

use super::{Drag, FileTab};

pub fn draw_withtext(
    state: &mut WindowState,
    ftab: &mut FileTab,
    ctx: &mut FrameCtx,
) -> Result<()> {
    // Lock the shared file data
    // We want to do this only once, to minimize latency
    let mut file = ftab.file.lock();
    let fview = &mut ftab.view;

    state.draw.timing.mark("file-lock");

    let text_view = FileView::text_view(&state.k, fview.view);

    // Do any bookkeeping that requires the lock
    // This includes moving the selection, possibly moving the scroll position with it
    fview.bookkeep_file(state, &mut file);

    state.draw.timing.mark("book-keep");

    // Determine the bounds of the loaded area, and clamp the scroll position to it
    let scroll_bounds = file.bounding_rect(fview.scroll.pos.base_offset);
    fview.scroll.pos = scroll_bounds.clamp_pos(fview.scroll.pos);
    fview.scroll.last_view = FileRect {
        corner: fview.scroll.pos,
        size: (text_view.size() / state.k.g.font_height).as_dvec2(),
    };

    // Only update bounds if not dragging the scrollbar
    // This makes the scrollbar-drag experience much smoother
    // while the file is still being loaded
    if !fview.drag.is_scrollbar() {
        fview.scroll.last_bounds = scroll_bounds;
    }

    // Get the selection range
    let sel_range = if fview.selected.first <= fview.selected.second {
        fview.selected.first..fview.selected.second
    } else {
        fview.selected.second..fview.selected.first
    };

    state.draw.timing.mark("sync-misc");

    // Iterate over all characters on the screen and queue them up for rendering
    let mut sel_box = ScreenRect {
        min: default(),
        max: default(),
    };
    let absolute_start = file.lookup_offset(fview.scroll.pos.base_offset, 0);
    file.visit_rect(fview.scroll.last_view, |offset, dx, dy, c| {
        match c {
            None => {
                // Starting a line
                // Write line number
                {
                    let mut x = text_view.min.x - state.k.g.linenum_pad;
                    let y = text_view.min.y
                        + ((dy + 1) as f64 - fview.scroll.pos.delta_y) as f32
                            * state.k.g.font_height;
                    let mut draw_char = |c| {
                        x -=
                            ftab.file.layout().advance_for(c as u32) as f32 * state.k.g.font_height;
                        state.draw.linenums.push(
                            &mut state.draw.glyphs,
                            state.k.g.linenum_color,
                            Glyph {
                                id: state.draw.font.glyph_id(c),
                                scale: state.k.g.font_height.into(),
                                position: (x, y).into(),
                            },
                        );
                    };
                    let linenum = match absolute_start.as_ref() {
                        Some(file_start) => dy - file_start.dy + 1,
                        None => dy,
                    };
                    if linenum == 0 {
                        draw_char('0');
                    } else {
                        let mut n = linenum.abs();
                        while n != 0 {
                            draw_char((b'0' + (n % 10) as u8) as char);
                            n /= 10;
                        }
                    }
                    if absolute_start.is_none() {
                        draw_char(if linenum < 0 { '-' } else { '+' });
                    }
                }
                // Draw previous selection box
                if sel_box.min.x < sel_box.max.x {
                    state
                        .draw
                        .sel_vbo
                        .push_quad(sel_box, state.k.g.selection_bg_color);
                }
                let y = text_view.min.y
                    + (dy as f64 - fview.scroll.pos.delta_y) as f32 * state.k.g.font_height
                    + (state.k.g.selection_offset * state.k.g.font_height).round();
                sel_box = ScreenRect {
                    min: vec2(f32::INFINITY, y),
                    max: vec2(f32::NEG_INFINITY, y + state.k.g.font_height),
                };
            }
            Some((c, hadv)) => {
                // Process a single character
                // Figure out screen position of this character
                let pos = text_view.min
                    + dvec2(
                        dx - fview.scroll.pos.delta_x,
                        (dy + 1) as f64 - fview.scroll.pos.delta_y,
                    )
                    .as_vec2()
                        * state.k.g.font_height;
                // If the character is selected, make sure the selection box wraps it
                let is_sel = sel_range.start <= offset && offset < sel_range.end;
                if is_sel {
                    sel_box.min.x = sel_box.min.x.min(pos.x);
                    sel_box.max.x = sel_box
                        .max
                        .x
                        .max(pos.x + hadv as f32 * state.k.g.font_height);
                }
                // Create and queue the glyph
                let g = Glyph {
                    id: state.draw.font.glyph_id(char::from_u32(c).unwrap_or('\0')),
                    scale: state.k.g.font_height.into(),
                    position: pos.to_array().into(),
                };
                state.draw.text.push(
                    &mut state.draw.glyphs,
                    if is_sel {
                        state.k.g.selection_color
                    } else {
                        state.k.g.text_color
                    },
                    g,
                );
            }
        }
    });
    {
        if sel_box.min.x < sel_box.max.x {
            state
                .draw
                .sel_vbo
                .push_quad(sel_box, state.k.g.selection_bg_color);
        }
    }

    state.draw.timing.mark("draw-text");

    // Draw cursor
    if let Some(pos) = fview.selected.last_positions[1] {
        let (visible, next) = fview.selected.check_blink(&state.k);
        ctx.schedule_redraw(next);
        if visible && pos.base_offset == fview.scroll.pos.base_offset {
            let pos = text_view.min
                + dvec2(
                    pos.delta_x - fview.scroll.pos.delta_x,
                    pos.delta_y - fview.scroll.pos.delta_y + state.k.g.selection_offset as f64,
                )
                .as_vec2()
                    * state.k.g.font_height;
            state.draw.aux_vbo.push_quad(
                ScreenRect {
                    min: vec2(pos.x - state.k.g.cursor_width / 2., pos.y),
                    max: vec2(
                        pos.x + state.k.g.cursor_width / 2.,
                        pos.y + state.k.g.font_height,
                    ),
                },
                state.k.g.cursor_color,
            );
        }
    }

    state.draw.timing.mark("draw-cursor");

    // If the backend is not idle, we should render periodically to show any updates
    if !file.is_backend_idle() || fview.drag.requires_refresh() {
        state.redraw();
    }

    Ok(())
}

pub fn draw_notext(state: &mut WindowState, ftab: &mut FileTab, ctx: &mut FrameCtx) -> Result<()> {
    let fview = &mut ftab.view;
    let file_view_scissor = fview.view.as_gl_rect(ctx.size);
    let text_view_scissor = FileView::text_view(&state.k, fview.view).as_gl_rect(ctx.size);

    //Draw selection highlights, text and line numbers
    {
        let uniforms = gl::glium::uniform! {
            glyph: state.draw.texture.sampled()
                .magnify_filter(MagnifySamplerFilter::Nearest)
                .minify_filter(MinifySamplerFilter::Nearest),
            mvp: ctx.mvp.to_cols_array_2d(),
        };
        ctx.frame.draw(
            state.draw.sel_vbo.vbo(),
            TRIANGLES_LIST,
            &state.draw.flat_shader,
            &gl::glium::uniform! {
                tint: [1f32; 4],
                mvp: ctx.mvp.to_cols_array_2d(),
            },
            &DrawParameters {
                blend: Blend::alpha_blending(),
                scissor: Some(text_view_scissor),
                ..default()
            },
        )?;
        state.draw.text.draw(
            &mut ctx.frame,
            &state.draw.text_shader,
            &uniforms,
            &DrawParameters {
                blend: Blend::alpha_blending(),
                scissor: Some(text_view_scissor),
                ..default()
            },
        )?;
        state.draw.linenums.draw(
            &mut ctx.frame,
            &state.draw.text_shader,
            &uniforms,
            &DrawParameters {
                blend: Blend::alpha_blending(),
                scissor: Some(file_view_scissor),
                ..default()
            },
        )?;
    }

    // Draw scrollbars
    {
        let ydraw = fview.scroll.ydraw(&state.k);
        let xdraw = fview.scroll.xdraw(&state.k);

        if ydraw {
            // Draw the vertical scrollbar background
            let bar = fview.scroll.y_scrollbar_bounds(&state.k, fview.view);
            state.draw.aux_vbo.push_quad(bar, state.k.g.scrollbar_color);

            // Draw the vertical scrollbar handle
            let handle = fview.scroll.y_scrollhandle_bounds(&state.k, fview.view);
            state
                .draw
                .aux_vbo
                .push_quad(handle, state.k.g.scrollhandle_color);
        }

        if xdraw {
            // Draw the horizontal scrollbar background
            let bar = fview.scroll.x_scrollbar_bounds(&state.k, fview.view);
            state.draw.aux_vbo.push_quad(bar, state.k.g.scrollbar_color);

            // Draw the horizontal scrollbar handle
            let handle = fview.scroll.x_scrollhandle_bounds(&state.k, fview.view);
            state
                .draw
                .aux_vbo
                .push_quad(handle, state.k.g.scrollhandle_color);
        }

        if xdraw && ydraw {
            // Draw the scrollbar corner
            let hy = fview.scroll.y_scrollbar_bounds(&state.k, fview.view);
            let hx = fview.scroll.x_scrollbar_bounds(&state.k, fview.view);
            state.draw.aux_vbo.push_quad(
                ScreenRect {
                    min: vec2(hy.min.x, hx.min.y),
                    max: vec2(hy.max.x, hx.max.y),
                },
                state.k.g.scrollcorner_color,
            );
        }
    }

    // Draw the slide icon if sliding
    if let Drag::Slide { screen_base, .. } = &fview.drag {
        state
            .draw
            .aux_vbo
            .push_prebuilt(&state.draw.slide_icon, screen_base.round());
    }

    Ok(())
}
