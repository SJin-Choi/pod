//! Custom GPUI element that paints a [`TerminalView`]'s grid and wires mouse
//! and IME input back to it.
//!
//! Strategy (validated by pod-spike):
//! - snapshot the VT grid once per render, then paint from the copy
//! - background: merge horizontal runs of equal bg into one quad
//! - text: one shaped line per (row, segment); segments break only at wide
//!   chars, so the monospace grid stays aligned via `force_width`
//! - box-drawing / block glyphs (U+2500–U+259F) are painted as cell-sized
//!   quads (`boxdraw`) and appear as spaces in the shaped text
//! - GPUI's line layout cache makes unchanged rows cheap frame-to-frame
//! - the element only runs when its view re-renders: hosts embed the view
//!   with `Entity::cached` so idle terminals reuse last frame's paint

use std::time::Instant;

use gpui_kit::*;
use unicode_width::UnicodeWidthChar;

use crate::boxdraw;
use crate::palette;
use crate::selection::{SelPoint, row_span};
use crate::view::{LayoutInfo, TerminalView};
use crate::vt::{CellSnap, CursorShape, GridSize, Snapshot};

pub struct TerminalElement {
    view: Entity<TerminalView>,
    focus: FocusHandle,
    focused: bool,
}

impl TerminalElement {
    pub fn new(view: Entity<TerminalView>, focus: FocusHandle, focused: bool) -> Self {
        Self { view, focus, focused }
    }
}

#[derive(Clone)]
pub struct Metrics {
    pub cell_w: Pixels,
    pub line_h: Pixels,
    pub font: Font,
    pub font_size: Pixels,
}

pub fn metrics(family: &SharedString, font_size: f32, line_height: f32, window: &Window) -> Metrics {
    let font = font(family.clone());
    let ts = window.text_system();
    let font_id = ts.resolve_font(&font);
    let size = px(font_size);
    let cell_w = ts.advance(font_id, size, 'm').map(|s| s.width).unwrap_or(px(font_size * 0.6));
    let line_h = px((font_size * line_height).round());
    Metrics { cell_w, line_h, font, font_size: size }
}

pub struct Prepared {
    snapshot: Snapshot,
    metrics: Metrics,
    /// Selection as an absolute-line range plus the absolute line of screen row 0.
    selection: Option<(SelPoint, SelPoint)>,
    top_abs: i64,
    marked: Option<String>,
    /// Ctrl-hovered link: underlined, hand cursor.
    link: Option<(SelPoint, SelPoint)>,
    hitbox: Hitbox,
}

impl IntoElement for TerminalElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepared;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(&mut self, _: Option<&GlobalElementId>, _: Option<&InspectorElementId>, window: &mut Window, cx: &mut App) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Prepared {
        let config = self.view.read(cx).config().clone();
        let metrics = metrics(&config.font_family, config.font_size, config.line_height, window);
        let cols = ((bounds.size.width / metrics.cell_w).floor() as u16).max(2);
        let rows = ((bounds.size.height / metrics.line_h).floor() as u16).max(1);
        let cell_px = (metrics.cell_w.as_f32().round() as u16, metrics.line_h.as_f32().round() as u16);
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let (snapshot, selection, top_abs, marked, link) = self.view.update(cx, |view, _| {
            view.resize_grid(GridSize { cols, rows }, cell_px);
            let snapshot = view.vt().snapshot();
            view.layout =
                Some(LayoutInfo { origin: bounds.origin, cell_w: metrics.cell_w, line_h: metrics.line_h, size: snapshot.size, cursor: snapshot.cursor_point });
            let top_abs = snapshot.history_size as i64 - snapshot.display_offset as i64;
            let link = view.hovered_link().map(|l| (l.start, l.end));
            (snapshot, view.selection_range(), top_abs, view.marked_text().map(str::to_string), link)
        });
        Prepared { snapshot, metrics, selection, top_abs, marked, link, hitbox }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        prepared: &mut Prepared,
        window: &mut Window,
        cx: &mut App,
    ) {
        let started = Instant::now();
        let Prepared { snapshot, metrics, selection, top_abs, marked, link, hitbox } = prepared;
        let Metrics { cell_w, line_h, font, font_size } = metrics.clone();
        let cols = snapshot.size.cols as usize;
        let origin = bounds.origin;
        let scale = window.scale_factor();

        window.set_cursor_style(if link.is_some() { CursorStyle::PointingHand } else { CursorStyle::IBeam }, hitbox);
        window.paint_quad(fill(bounds, hsla_of(snapshot.default_bg)));

        let snap_rect = |c0: usize, c1: usize, row: usize| {
            let a = boxdraw::cell_rect(origin, cell_w, line_h, c0, row, scale);
            let z = boxdraw::cell_rect(origin, cell_w, line_h, c1, row, scale);
            Bounds::from_corners(point(px(a.x0 as f32 / scale), px(a.y0 as f32 / scale)), point(px(z.x1 as f32 / scale), px(z.y1 as f32 / scale)))
        };

        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for (row, cells) in snapshot.cells.chunks(cols).enumerate() {
                let y = origin.y + line_h * row as f32;

                // Background runs (device-pixel snapped so no sub-pixel slivers).
                let mut col = 0;
                while col < cells.len() {
                    let bg = cells[col].bg;
                    let start = col;
                    while col < cells.len() && cells[col].bg == bg {
                        col += 1;
                    }
                    if bg != snapshot.default_bg {
                        window.paint_quad(fill(snap_rect(start, col - 1, row), hsla_of(bg)));
                    }
                }

                // Selection highlight, under the text.
                if let Some(range) = selection
                    && let Some((c0, c1)) = row_span(*range, *top_abs + row as i64, cols as u16)
                {
                    window.paint_quad(fill(snap_rect(c0 as usize, c1 as usize, row), hsla_of(self.view.read(cx).palette.selection_bg)));
                }

                // Box-drawing / block cells: full-cell quads, not font glyphs.
                for (col, cell) in cells.iter().enumerate() {
                    if boxdraw::is_custom(cell.c) {
                        let r = boxdraw::cell_rect(origin, cell_w, line_h, col, row, scale);
                        boxdraw::paint(cell.c, r, hsla_of(cell.fg), scale, font_size.as_f32(), window);
                    }
                }

                // Ctrl-hovered link: underline in the text color of its first cell.
                if let Some(range) = link
                    && let Some((c0, c1)) = row_span(*range, *top_abs + row as i64, cols as u16)
                {
                    let r = snap_rect(c0 as usize, c1 as usize, row);
                    let color = cells.get(c0 as usize).map_or(snapshot.default_fg, |c| c.fg);
                    window.paint_quad(fill(Bounds::new(point(r.origin.x, r.origin.y + r.size.height - px(1.5)), size(r.size.width, px(1.))), hsla_of(color)));
                }

                // Text segments.
                for seg in segments(cells) {
                    if seg.text.trim().is_empty() {
                        continue;
                    }
                    let runs: Vec<TextRun> = seg.runs.iter().map(|(len, c)| text_run(*len, &font, c, c.fg)).collect();
                    let (line, dx) = shape_cells(SharedString::from(seg.text), &runs, font_size, cell_w, seg.wide, window);
                    let x = origin.x + cell_w * seg.start_col as f32 + dx;
                    let _ = line.paint(point(x, y), line_h, TextAlign::Left, None, window, cx);
                }
            }

            if let Some(text) = marked.as_deref() {
                let at = snapshot.cursor_point.unwrap_or((0, 0));
                paint_preedit(text, at, origin, cell_w, line_h, font_size, &font, snapshot, window, cx);
            } else if let Some((ccol, crow, shape)) = snapshot.cursor {
                paint_cursor(self.focused, ccol, crow, shape, origin, cell_w, line_h, font_size, &font, snapshot, window, cx);
            }

            paint_scrollbar(bounds, snapshot, window);
        });

        // IME / text input: only registered while focused (GPUI checks).
        window.handle_input(&self.focus, ElementInputHandler::new(bounds, self.view.clone()), cx);
        self.register_mouse(hitbox.clone(), window);

        crate::perf::record_paint(started.elapsed());
    }
}

impl TerminalElement {
    fn register_mouse(&self, hitbox: Hitbox, window: &mut Window) {
        let view = self.view.clone();
        let hb = hitbox.clone();
        window.on_mouse_event(move |e: &MouseDownEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && hb.is_hovered(window) {
                view.update(cx, |v, cx| v.mouse_down(e, window, cx));
            }
        });
        let view = self.view.clone();
        let hb = hitbox.clone();
        window.on_mouse_event(move |e: &MouseMoveEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble {
                let hovered = hb.is_hovered(window);
                view.update(cx, |v, cx| v.mouse_move(e, hovered, window, cx));
            }
        });
        let view = self.view.clone();
        window.on_mouse_event(move |e: &MouseUpEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble {
                view.update(cx, |v, cx| v.mouse_up(e, window, cx));
            }
        });
        let view = self.view.clone();
        let hb = hitbox;
        window.on_mouse_event(move |e: &ScrollEvent, phase, window, cx| {
            // Not `is_hovered`: that is false in keyboard modality (right after typing,
            // before the mouse moves), and the wheel must still scroll then.
            if phase == DispatchPhase::Bubble && hb.should_handle_scroll(window) {
                view.update(cx, |v, cx| v.scroll_wheel(e, window, cx));
                cx.stop_propagation();
            }
        });
    }
}

fn text_run(len: usize, font: &Font, c: &CellSnap, color: u32) -> TextRun {
    TextRun {
        len,
        font: styled_font(font, c),
        color: hsla_of(color),
        background_color: None,
        underline: c.underline.then(|| UnderlineStyle { thickness: px(1.), color: Some(hsla_of(color)), wavy: false }),
        strikethrough: c.strikeout.then(|| StrikethroughStyle { thickness: px(1.), color: Some(hsla_of(color)) }),
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_cursor(
    focused: bool,
    col: u16,
    row: u16,
    shape: CursorShape,
    origin: Point<Pixels>,
    cell_w: Pixels,
    line_h: Pixels,
    font_size: Pixels,
    font: &Font,
    snapshot: &Snapshot,
    window: &mut Window,
    cx: &mut App,
) {
    let cols = snapshot.size.cols as usize;
    let cell = snapshot.cells.get(row as usize * cols + col as usize).copied();
    let width = if cell.is_some_and(|c| c.wide) { cell_w * 2. } else { cell_w };
    let at = point(origin.x + cell_w * col as f32, origin.y + line_h * row as f32);
    let color = hsla_of(snapshot.cursor_color);
    let b = Bounds::new(at, size(width, line_h));
    match (focused, shape) {
        (true, CursorShape::Block) => {
            window.paint_quad(fill(b, color));
            // Re-draw the glyph under the cursor in the background color.
            if let Some(c) = cell
                && !c.c.is_whitespace()
                && c.c != '\0'
                && !boxdraw::is_custom(c.c)
            {
                let text = SharedString::from(c.c.to_string());
                let run = text_run(text.len(), font, &c, snapshot.default_bg);
                let (line, dx) = shape_cells(text, &[run], font_size, cell_w, c.wide, window);
                let _ = line.paint(point(at.x + dx, at.y), line_h, TextAlign::Left, None, window, cx);
            }
        }
        (true, CursorShape::Beam) => window.paint_quad(fill(Bounds::new(at, size(px(2.), line_h)), color)),
        (true, CursorShape::Underline) => window.paint_quad(fill(Bounds::new(point(at.x, at.y + line_h - px(2.)), size(width, px(2.))), color)),
        _ => window.paint_quad(outline(b, color, BorderStyle::Solid)),
    }
}

/// IME composition text drawn over the grid at the cursor, underlined.
#[allow(clippy::too_many_arguments)]
fn paint_preedit(
    text: &str,
    at: (u16, u16),
    origin: Point<Pixels>,
    cell_w: Pixels,
    line_h: Pixels,
    font_size: Pixels,
    font: &Font,
    snapshot: &Snapshot,
    window: &mut Window,
    cx: &mut App,
) {
    let x0 = origin.x + cell_w * at.0 as f32;
    let y = origin.y + line_h * at.1 as f32;
    let widths: Vec<usize> = text.chars().map(|c| c.width().unwrap_or(0)).collect();
    let total: usize = widths.iter().sum::<usize>().max(1);
    let fg = hsla_of(snapshot.default_fg);
    window.paint_quad(fill(Bounds::new(point(x0, y), size(cell_w * total as f32, line_h)), hsla_of(snapshot.default_bg)));
    let style =
        CellSnap { c: ' ', fg: snapshot.default_fg, bg: snapshot.default_bg, bold: false, italic: false, underline: false, strikeout: false, wide: false };
    let mut col = 0usize;
    for (c, w) in text.chars().zip(widths) {
        if w == 0 {
            continue;
        }
        let s = SharedString::from(c.to_string());
        let run = text_run(s.len(), font, &style, snapshot.default_fg);
        let (line, dx) = shape_cells(s, &[run], font_size, cell_w, w > 1, window);
        let _ = line.paint(point(x0 + cell_w * col as f32 + dx, y), line_h, TextAlign::Left, None, window, cx);
        col += w;
    }
    // Composition underline + caret after the preedit.
    window.paint_quad(fill(Bounds::new(point(x0, y + line_h - px(2.)), size(cell_w * total as f32, px(2.))), fg));
    window.paint_quad(fill(Bounds::new(point(x0 + cell_w * col as f32, y), size(px(2.), line_h)), hsla_of(snapshot.cursor_color)));
}

/// Thin overlay scrollbar on the right edge while there is scrollback.
fn paint_scrollbar(bounds: Bounds<Pixels>, snapshot: &Snapshot, window: &mut Window) {
    let history = snapshot.history_size;
    if history == 0 {
        return;
    }
    let rows = snapshot.size.rows as f32;
    let total = history as f32 + rows;
    let h = bounds.size.height;
    let thumb_h = (h * (rows / total)).max(px(16.));
    let top_line = (history - snapshot.display_offset.min(history)) as f32;
    let travel = h - thumb_h;
    let y = bounds.origin.y + travel * (top_line / history as f32);
    let w = px(4.);
    let x = bounds.origin.x + bounds.size.width - w - px(1.);
    let alpha = if snapshot.display_offset > 0 { 0.75 } else { 0.3 };
    window.paint_quad(fill(Bounds::new(point(x, bounds.origin.y), size(w, h)), hsla_of(palette::SCROLLBAR).opacity(0.08)));
    window.paint_quad(fill(Bounds::new(point(x, y), size(w, thumb_h)), hsla_of(palette::SCROLLBAR).opacity(alpha)).corner_radii(px(2.)));
}

/// Shape grid text. Narrow cells get a fixed `cell_w` advance per glyph. A
/// wide (2-cell) char is shaped at its natural advance and placed by
/// [`wide_fit`]; returns the line and the x offset inside its first cell.
fn shape_cells(text: SharedString, runs: &[TextRun], font_size: Pixels, cell_w: Pixels, wide: bool, window: &Window) -> (ShapedLine, Pixels) {
    let ts = window.text_system();
    if !wide {
        return (ts.shape_line(text, font_size, runs, Some(cell_w)), px(0.));
    }
    let natural = ts.shape_line(text.clone(), font_size, runs, None);
    let (dx, k) = wide_fit(natural.width.as_f32(), cell_w.as_f32() * 2., window.scale_factor());
    if (k - 1.).abs() > 0.001 { (ts.shape_line(text, font_size * k, runs, None), px(dx)) } else { (natural, px(dx)) }
}

/// Largest enlargement of a wide glyph (keeps it inside the line height:
/// line height is 1.3 em, a 1.35× Hangul syllable is about 1.25 em tall).
const WIDE_MAX_GROW: f32 = 1.35;

/// Placement of a wide glyph with natural `advance` in a 2-cell `slot`
/// (logical px): `(x offset, font scale)`, the offset snapped to device pixels.
///
/// Fallback fonts give Hangul/CJK glyphs about 0.92 em of advance (Malgun
/// Gothic) while two cells of a mono font are ≈ 1.17 em (Cascadia Mono), so at
/// their natural size every syllable left a visible gap ("문 서"). Like Windows
/// Terminal, the glyph is scaled so its advance fills the two cells (at most
/// [`WIDE_MAX_GROW`]; wider glyphs such as some emoji shrink), then centered.
pub(crate) fn wide_fit(advance: f32, slot: f32, scale: f32) -> (f32, f32) {
    if !(advance > 0. && slot > 0.) {
        return (0., 1.);
    }
    let k = (slot / advance).min(WIDE_MAX_GROW);
    let dx = ((slot - advance * k) / 2.).max(0.);
    ((dx * scale).round() / scale.max(0.01), k)
}

struct Segment {
    start_col: usize,
    /// A single wide (2-cell) char.
    wide: bool,
    text: String,
    /// (byte length, style source cell)
    runs: Vec<(usize, CellSnap)>,
}

fn same_style(a: &CellSnap, b: &CellSnap) -> bool {
    a.fg == b.fg && a.bold == b.bold && a.italic == b.italic && a.underline == b.underline && a.strikeout == b.strikeout
}

/// Split a row into segments; a wide char ends a segment so that every
/// segment can be shaped with a fixed per-glyph advance.
fn segments(cells: &[CellSnap]) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut cur = Segment { start_col: 0, wide: false, text: String::new(), runs: Vec::new() };
    for (col, cell) in cells.iter().enumerate() {
        if cell.c == '\0' {
            if cur.text.is_empty() {
                cur.start_col = col + 1;
            }
            continue;
        }
        if cell.wide {
            if !cur.text.is_empty() {
                out.push(std::mem::replace(&mut cur, Segment { start_col: col, wide: false, text: String::new(), runs: Vec::new() }));
            }
            cur.start_col = col;
            cur.wide = true;
            push_char(&mut cur, cell);
            out.push(std::mem::replace(&mut cur, Segment { start_col: col + 2, wide: false, text: String::new(), runs: Vec::new() }));
            continue;
        }
        push_char(&mut cur, cell);
    }
    if !cur.text.is_empty() {
        out.push(cur);
    }
    out
}

fn push_char(seg: &mut Segment, cell: &CellSnap) {
    // Custom-drawn cells become spaces: same fixed advance, so the grid stays aligned.
    let c = if cell.c.is_control() || boxdraw::is_custom(cell.c) { ' ' } else { cell.c };
    seg.text.push(c);
    let len = c.len_utf8();
    match seg.runs.last_mut() {
        Some((l, s)) if same_style(s, cell) => *l += len,
        _ => seg.runs.push((len, *cell)),
    }
}

fn styled_font(base: &Font, c: &CellSnap) -> Font {
    let mut f = base.clone();
    if c.bold {
        f.weight = FontWeight::BOLD;
    }
    if c.italic {
        f.style = FontStyle::Italic;
    }
    f
}

pub fn hsla_of(c: u32) -> Hsla {
    rgb(c).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    // GPUI's glob exports its own `test` attribute; these are plain Rust tests.
    use core::prelude::v1::test;

    fn cell(c: char, wide: bool) -> CellSnap {
        CellSnap { c, fg: 0xffffff, bg: 0, bold: false, italic: false, underline: false, strikeout: false, wide }
    }

    #[test]
    fn wide_glyphs_fill_their_two_cells() {
        // Cascadia Mono at 19.5 px: cell 11.43 px, slot 22.85; Malgun Gothic
        // Hangul ≈ 0.92 em = 17.9 px → grown to fill the slot (no gap after).
        let (dx, k) = wide_fit(17.9, 22.85, 1.5);
        assert!((17.9 * k - 22.85).abs() < 0.01, "{k}");
        assert_eq!(dx, 0.);
        // Very narrow glyphs grow at most 1.35× and are centered, snapped to device pixels.
        let (dx, k) = wide_fit(10., 22.85, 1.5);
        assert_eq!(k, WIDE_MAX_GROW);
        assert!((dx - (22.85 - 13.5) / 2.).abs() < 0.34, "{dx}");
        assert!(((dx * 1.5).round() - dx * 1.5).abs() < 1e-4, "snapped to device pixels");
        // Exactly two cells: unchanged.
        assert_eq!(wide_fit(22.85, 22.85, 1.0), (0., 1.));
    }

    #[test]
    fn wider_glyphs_shrink_to_the_slot() {
        let (dx, k) = wide_fit(30., 24., 1.0);
        assert_eq!(dx, 0.);
        assert!((k - 0.8).abs() < 1e-6);
        // Degenerate inputs leave the glyph alone.
        assert_eq!(wide_fit(0., 24., 1.0), (0., 1.));
    }

    #[test]
    fn segments_isolate_wide_chars() {
        // "문서: a" — 문(2) 서(2) ':' ' ' 'a'
        let cells = [cell('문', true), cell('\0', false), cell('서', true), cell('\0', false), cell(':', false), cell(' ', false), cell('a', false)];
        let segs = segments(&cells);
        let shape: Vec<(usize, bool, &str)> = segs.iter().map(|s| (s.start_col, s.wide, s.text.as_str())).collect();
        assert_eq!(shape, vec![(0, true, "문"), (2, true, "서"), (4, false, ": a")]);
    }
}
