use alacritty_terminal::grid::Scroll;
use alacritty_terminal::index::Side;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::CursorShape;
use gpui::{
    App, BorderStyle, Bounds, ContentMask, CursorStyle, DispatchPhase, Element, ElementId,
    ElementInputHandler, Entity, FocusHandle, Font, GlobalElementId, Hitbox, HitboxBehavior, Hsla,
    InspectorElementId, IntoElement, LayoutId, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    Pixels, Point, ScrollEvent, ShapedLine, SharedString, StrikethroughStyle, Style, TextAlign,
    TextRun, UnderlineStyle, Window, fill, font, outline, point, px, relative, size,
};
use gpui_kit::component::ActiveTheme as _;

use super::colors::Palette;
use super::panel::TerminalPanel;
use super::{SnapCell, Snapshot, TermSize, Terminal, mouse};
use crate::settings;

const PAD_X: f32 = 6.;
const PAD_Y: f32 = 4.;

pub struct TerminalElement {
    pub terminal: Entity<Terminal>,
    pub panel: Entity<TerminalPanel>,
    pub focus: FocusHandle,
}

pub struct CursorLayout {
    bounds: Bounds<Pixels>,
    shape: CursorShape,
    /// The glyph under a block cursor, re-shaped in the background color.
    text: Option<ShapedLine>,
}

/// Where the grid sits inside the element, for mapping pointer positions to cells.
#[derive(Clone, Copy)]
pub struct Geometry {
    pub origin: Point<Pixels>,
    pub cell_w: Pixels,
    pub line_h: Pixels,
    pub cols: usize,
    pub rows: usize,
}

impl Geometry {
    /// `(col, row, side)` of the cell under `position`, clamped to the grid.
    pub fn cell(&self, position: Point<Pixels>) -> (usize, usize, Side) {
        mouse::grid_cell(
            position,
            self.origin,
            self.cell_w,
            self.line_h,
            self.cols,
            self.rows,
        )
    }
}

pub struct Layout {
    hitbox: Hitbox,
    geometry: Geometry,
    palette: Palette,
    bg_rects: Vec<(Bounds<Pixels>, Hsla)>,
    selection_rects: Vec<Bounds<Pixels>>,
    /// `(bounds, color)` of search matches inside the viewport.
    search_rects: Vec<(Bounds<Pixels>, Hsla)>,
    runs: Vec<(Point<Pixels>, ShapedLine)>,
    /// IME composition: backdrop bounds and the underlined text.
    ime: Option<(Bounds<Pixels>, ShapedLine)>,
    cursor: Option<CursorLayout>,
    focused: bool,
}

#[derive(Clone, Copy, PartialEq)]
struct RunStyle {
    fg: Hsla,
    bold: bool,
    italic: bool,
    /// 0 = none, 1 = straight, 2 = wavy.
    underline: u8,
    strikeout: bool,
}

impl RunStyle {
    fn of(cell: &SnapCell) -> Self {
        let flags = cell.flags;
        let underline = if flags.contains(Flags::UNDERCURL) {
            2
        } else if flags.intersects(
            Flags::UNDERLINE
                | Flags::DOUBLE_UNDERLINE
                | Flags::DOTTED_UNDERLINE
                | Flags::DASHED_UNDERLINE,
        ) {
            1
        } else {
            0
        };
        Self {
            fg: cell.fg,
            bold: flags.contains(Flags::BOLD),
            italic: flags.contains(Flags::ITALIC),
            underline,
            strikeout: flags.contains(Flags::STRIKEOUT),
        }
    }

    fn text_run(&self, base: &Font, len: usize) -> TextRun {
        let mut font = base.clone();
        if self.bold {
            font = font.bold();
        }
        if self.italic {
            font = font.italic();
        }
        TextRun {
            len,
            font,
            color: self.fg,
            background_color: None,
            underline: (self.underline > 0).then(|| UnderlineStyle {
                thickness: px(1.),
                color: Some(self.fg),
                wavy: self.underline == 2,
            }),
            strikethrough: self.strikeout.then(|| StrikethroughStyle {
                thickness: px(1.),
                color: Some(self.fg),
            }),
        }
    }
}

struct PendingRun {
    row: usize,
    start_col: usize,
    next_col: usize,
    style: RunStyle,
    text: String,
}

struct Metrics {
    origin: Point<Pixels>,
    cell_w: Pixels,
    line_h: Pixels,
}

impl Metrics {
    fn cell_origin(&self, row: usize, col: usize) -> Point<Pixels> {
        point(
            self.origin.x + self.cell_w * col,
            self.origin.y + self.line_h * row,
        )
    }
}

/// One rectangle per viewport row covered by the grid range `start..=end`.
fn range_rects(
    start: alacritty_terminal::index::Point,
    end: alacritty_terminal::index::Point,
    is_block: bool,
    snapshot: &Snapshot,
    metrics: &Metrics,
) -> Vec<Bounds<Pixels>> {
    let offset = snapshot.display_offset as i32;
    let mut rects = Vec::new();
    for row in 0..snapshot.rows {
        let line = row as i32 - offset;
        if line < start.line.0 || line > end.line.0 {
            continue;
        }
        let first = if is_block || line == start.line.0 {
            start.column.0
        } else {
            0
        };
        let last = if is_block || line == end.line.0 {
            end.column.0
        } else {
            snapshot.cols.saturating_sub(1)
        };
        if first > last {
            continue;
        }
        rects.push(Bounds::new(
            metrics.cell_origin(row, first),
            size(metrics.cell_w * (last + 1 - first), metrics.line_h),
        ));
    }
    rects
}

fn background_rects(snapshot: &Snapshot, metrics: &Metrics) -> Vec<(Bounds<Pixels>, Hsla)> {
    let mut rects = Vec::new();
    // (row, start_col, end_col_exclusive, color)
    let mut current: Option<(usize, usize, usize, Hsla)> = None;
    let mut flush = |run: (usize, usize, usize, Hsla)| {
        let (row, start, end, color) = run;
        rects.push((
            Bounds::new(
                metrics.cell_origin(row, start),
                size(metrics.cell_w * (end - start), metrics.line_h),
            ),
            color,
        ));
    };
    for cell in &snapshot.cells {
        let Some(bg) = cell.bg else { continue };
        match &mut current {
            Some((row, _, end, color)) if *row == cell.row && *end == cell.col && *color == bg => {
                *end += 1;
            }
            _ => {
                if let Some(run) = current.take() {
                    flush(run);
                }
                current = Some((cell.row, cell.col, cell.col + 1, bg));
            }
        }
    }
    if let Some(run) = current.take() {
        flush(run);
    }
    rects
}

impl IntoElement for TerminalElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = Layout;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let appearance = &settings::get(cx).appearance;
        let base_font = font(SharedString::from(appearance.font_family.clone()));
        let font_size = px(appearance.font_size);
        let line_h = (font_size * appearance.line_height).round();

        let text_system = window.text_system().clone();
        let font_id = text_system.resolve_font(&base_font);
        let cell_w = text_system
            .advance(font_id, font_size, 'm')
            .map(|advance| advance.width)
            .unwrap_or(font_size * 0.6);

        let cols = ((f32::from(bounds.size.width) - PAD_X * 2.) / f32::from(cell_w))
            .floor()
            .max(2.) as usize;
        let rows = ((f32::from(bounds.size.height) - PAD_Y * 2.) / f32::from(line_h))
            .floor()
            .max(1.) as usize;
        let term_size = TermSize {
            cols,
            rows,
            cell_w,
            line_h,
        };
        self.terminal
            .update(cx, |terminal, _| terminal.set_size(term_size));

        // A selection drag outside the viewport scrolls one line per frame.
        if let Some(step) = self.panel.read(cx).autoscroll() {
            let terminal = self.terminal.read(cx);
            terminal.scroll(Scroll::Delta(step.delta));
            terminal.update_selection(step.row, step.col, step.side);
            window.request_animation_frame();
        }

        let palette = Palette::from_theme(cx);
        let snapshot = self.terminal.read(cx).snapshot(&palette);
        let metrics = Metrics {
            origin: bounds.origin + point(px(PAD_X), px(PAD_Y)),
            cell_w,
            line_h,
        };

        let bg_rects = background_rects(&snapshot, &metrics);
        let selection_rects = snapshot
            .selection
            .map(|range| range_rects(range.start, range.end, range.is_block, &snapshot, &metrics))
            .unwrap_or_default();
        // The theme's `accent` is a near-background tint, invisible as a highlight.
        let accent = cx.theme().yellow.opacity(0.6);
        let muted = cx.theme().muted_foreground.opacity(0.3);
        let search_rects = self
            .panel
            .update(cx, |panel, cx| panel.search_highlights(cx))
            .into_iter()
            .flat_map(|(found, is_current)| {
                let color = if is_current { accent } else { muted };
                range_rects(*found.start(), *found.end(), false, &snapshot, &metrics)
                    .into_iter()
                    .map(move |rect| (rect, color))
            })
            .collect();

        let mut runs = Vec::new();
        let mut flush = |run: PendingRun| {
            let text_run = run.style.text_run(&base_font, run.text.len());
            let line =
                text_system.shape_line(run.text.into(), font_size, &[text_run], Some(cell_w));
            runs.push((metrics.cell_origin(run.row, run.start_col), line));
        };
        let mut current: Option<PendingRun> = None;
        for cell in &snapshot.cells {
            let flags = cell.flags;
            if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            let style = RunStyle::of(cell);
            if cell.c == ' '
                && cell.zerowidth.is_empty()
                && style.underline == 0
                && !style.strikeout
            {
                continue;
            }
            let extends = current.as_ref().is_some_and(|run| {
                run.row == cell.row && run.next_col == cell.col && run.style == style
            });
            if !extends {
                if let Some(run) = current.take() {
                    flush(run);
                }
                current = Some(PendingRun {
                    row: cell.row,
                    start_col: cell.col,
                    next_col: cell.col,
                    style,
                    text: String::new(),
                });
            }
            if let Some(run) = current.as_mut() {
                run.text.push(cell.c);
                run.text.extend(cell.zerowidth.iter());
                run.next_col = cell.col + 1;
            }
            // A wide glyph spans two cells: the next cell starts a new run at its own x.
            if flags.contains(Flags::WIDE_CHAR)
                && let Some(run) = current.take()
            {
                flush(run);
            }
        }
        if let Some(run) = current.take() {
            flush(run);
        }

        let focused = self.focus.is_focused(window);
        let cursor = snapshot.cursor.map(|(row, col, shape)| {
            let cell = snapshot
                .cells
                .iter()
                .find(|cell| cell.row == row && cell.col == col);
            let wide = cell.is_some_and(|cell| cell.flags.contains(Flags::WIDE_CHAR));
            let text = cell
                .filter(|cell| focused && shape == CursorShape::Block && cell.c != ' ')
                .map(|cell| {
                    let mut text = String::from(cell.c);
                    text.extend(cell.zerowidth.iter());
                    let style = RunStyle {
                        fg: palette.background,
                        underline: 0,
                        strikeout: false,
                        ..RunStyle::of(cell)
                    };
                    let run = style.text_run(&base_font, text.len());
                    text_system.shape_line(text.into(), font_size, &[run], Some(cell_w))
                });
            CursorLayout {
                bounds: Bounds::new(
                    metrics.cell_origin(row, col),
                    size(cell_w * if wide { 2. } else { 1. }, line_h),
                ),
                shape,
                text,
            }
        });

        // The IME composition is drawn over the cursor cell.
        let ime = self.panel.read(cx).marked_text.clone().map(|text| {
            let (row, col) = snapshot
                .cursor
                .map(|(row, col, _)| (row, col))
                .unwrap_or((0, 0));
            let style = RunStyle {
                fg: palette.foreground,
                bold: false,
                italic: false,
                underline: 1,
                strikeout: false,
            };
            let run = style.text_run(&base_font, text.len());
            let line = text_system.shape_line(text.into(), font_size, &[run], None);
            let cells = (f32::from(line.width) / f32::from(cell_w)).ceil().max(1.);
            (
                Bounds::new(metrics.cell_origin(row, col), size(cell_w * cells, line_h)),
                line,
            )
        });

        Layout {
            hitbox: window.insert_hitbox(bounds, HitboxBehavior::Normal),
            geometry: Geometry {
                origin: metrics.origin,
                cell_w,
                line_h,
                cols,
                rows,
            },
            palette,
            bg_rects,
            selection_rects,
            search_rects,
            runs,
            ime,
            cursor,
            focused,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        layout: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        window.handle_input(
            &self.focus,
            ElementInputHandler::new(bounds, self.panel.clone()),
            cx,
        );

        let line_h = layout.geometry.line_h;
        let blink_on = self.panel.read(cx).blink_on;
        window.set_cursor_style(CursorStyle::IBeam, &layout.hitbox);
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            window.paint_quad(fill(bounds, layout.palette.background));
            for (rect, color) in &layout.bg_rects {
                window.paint_quad(fill(*rect, *color));
            }
            for rect in &layout.selection_rects {
                window.paint_quad(fill(*rect, layout.palette.selection));
            }
            for (rect, color) in &layout.search_rects {
                window.paint_quad(fill(*rect, *color));
            }
            for (origin, line) in &layout.runs {
                let _ = line.paint(*origin, line_h, TextAlign::Left, None, window, cx);
            }
            if let Some((rect, line)) = &layout.ime {
                window.paint_quad(fill(*rect, layout.palette.background));
                let _ = line.paint(rect.origin, line_h, TextAlign::Left, None, window, cx);
            } else if let Some(cursor) = &layout.cursor {
                let color = layout.palette.cursor;
                let rect = cursor.bounds;
                if !layout.focused {
                    window.paint_quad(outline(rect, color, BorderStyle::default()));
                } else if blink_on {
                    match cursor.shape {
                        CursorShape::Block => {
                            window.paint_quad(fill(rect, color));
                            if let Some(text) = &cursor.text {
                                let _ = text.paint(
                                    rect.origin,
                                    line_h,
                                    TextAlign::Left,
                                    None,
                                    window,
                                    cx,
                                );
                            }
                        }
                        CursorShape::Beam => window.paint_quad(fill(
                            Bounds::new(rect.origin, size(px(2.), rect.size.height)),
                            color,
                        )),
                        CursorShape::Underline => window.paint_quad(fill(
                            Bounds::new(
                                point(rect.origin.x, rect.bottom() - px(2.)),
                                size(rect.size.width, px(2.)),
                            ),
                            color,
                        )),
                        CursorShape::HollowBlock => {
                            window.paint_quad(outline(rect, color, BorderStyle::default()));
                        }
                        CursorShape::Hidden => {}
                    }
                }
            }
        });

        let cursor_bounds = layout.cursor.as_ref().map(|cursor| cursor.bounds);
        self.panel
            .update(cx, |panel, _| panel.cursor_bounds = cursor_bounds);

        let geometry = layout.geometry;
        window.on_mouse_event({
            let panel = self.panel.clone();
            let hitbox = layout.hitbox.clone();
            move |event: &MouseDownEvent, phase, window, cx| {
                if phase == DispatchPhase::Bubble && hitbox.is_hovered(window) {
                    panel.update(cx, |panel, cx| {
                        panel.mouse_down(event, geometry, window, cx)
                    });
                }
            }
        });
        window.on_mouse_event({
            let panel = self.panel.clone();
            let hitbox = layout.hitbox.clone();
            move |event: &MouseMoveEvent, phase, window, cx| {
                if phase == DispatchPhase::Bubble {
                    let hovered = hitbox.is_hovered(window);
                    panel.update(cx, |panel, cx| {
                        panel.mouse_move(event, geometry, hovered, cx)
                    });
                }
            }
        });
        window.on_mouse_event({
            let panel = self.panel.clone();
            move |event: &MouseUpEvent, phase, _window, cx| {
                if phase == DispatchPhase::Bubble {
                    panel.update(cx, |panel, cx| panel.mouse_up(event, geometry, cx));
                }
            }
        });
        window.on_mouse_event({
            let panel = self.panel.clone();
            let hitbox = layout.hitbox.clone();
            move |event: &ScrollEvent, phase, window, cx| {
                if phase == DispatchPhase::Bubble && hitbox.should_handle_scroll(window) {
                    panel.update(cx, |panel, cx| panel.scroll_wheel(event, geometry, cx));
                }
            }
        });
    }
}
