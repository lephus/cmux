//! The GPUI window: one terminal grid, cursor, inline IME preedit.

use std::ops::Range;
use std::time::Duration;

use ghostty_vt::{CellWidth, CursorShape, Rgb};
use gpui::{
    App, Application, BorderStyle, Bounds, Context, Element, ElementId, ElementInputHandler,
    Entity, EntityInputHandler, FocusHandle, Focusable, Font, FontFallbacks, GlobalElementId, Hsla,
    InspectorElementId, IntoElement, KeyDownEvent, LayoutId, MouseButton, MouseDownEvent,
    PaintQuad, Pixels, Point, Render, ScrollWheelEvent, ShapedLine, SharedString,
    StrikethroughStyle, Style, TextRun, TitlebarOptions, UTF16Selection, UnderlineStyle, Window,
    WindowBounds, WindowOptions, div, fill, font, outline, point, prelude::*, px, relative, size,
};

use crate::session::{
    CURSOR, DEFAULT_BG, DEFAULT_FG, Session, Spawned, default_shell, spawn_reader,
};

const FONT_SIZE: f32 = 14.0;
const LINE_HEIGHT: f32 = 1.3;
const PADDING: f32 = 6.0;
const BASE_TITLE: &str = "ShellX terminal spike";

pub fn run() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(960.0), px(600.0)), cx);
        let window = cx
            .open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some(BASE_TITLE.into()),
                        ..Default::default()
                    }),
                    app_id: Some("pro.shellx.term-spike".into()),
                    ..Default::default()
                },
                |window, cx| cx.new(|cx| TermView::new(window, cx)),
            )
            .expect("failed to open the terminal window");
        window
            .update(cx, |view, window, cx| {
                window.focus(&view.focus_handle);
                cx.activate(true);
            })
            .ok();
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
    });
}

fn hsla(c: Rgb) -> Hsla {
    gpui::rgb((u32::from(c.r) << 16) | (u32::from(c.g) << 8) | u32::from(c.b)).into()
}

fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

fn utf16_to_utf8(s: &str, utf16: usize) -> usize {
    let mut units = 0;
    for (i, ch) in s.char_indices() {
        if units >= utf16 {
            return i;
        }
        units += ch.len_utf16();
    }
    s.len()
}

fn utf8_to_utf16(s: &str, utf8: usize) -> usize {
    utf16_len(&s[..utf8.min(s.len())])
}

/// Pick the first installed monospace family (Windows 11 ships Cascadia).
fn terminal_font(cx: &App) -> Font {
    let installed = cx.text_system().all_font_names();
    let family = ["Cascadia Mono", "Cascadia Code", "Consolas", "Courier New"]
        .into_iter()
        .find(|name| installed.iter().any(|f| f.eq_ignore_ascii_case(name)))
        .unwrap_or("Consolas");
    let mut f = font(family);
    f.fallbacks = Some(FontFallbacks::from_fonts(vec![
        "Consolas".into(),
        "Segoe UI".into(),
        "Segoe UI Emoji".into(),
        "Segoe UI Symbol".into(),
    ]));
    f
}

struct TermView {
    focus_handle: FocusHandle,
    font: Font,
    session: Option<Session>,
    error: Option<String>,
    /// IME composition text (preedit), shown inline at the cursor and never
    /// sent to the PTY until committed.
    marked: Option<String>,
    marked_selection: Option<Range<usize>>,
    /// Where the cursor was painted, for the IME candidate window.
    cursor_bounds: Option<Bounds<Pixels>>,
    cell_width: Pixels,
    line_height: Pixels,
    title: String,
}

impl TermView {
    fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut view = TermView {
            focus_handle: cx.focus_handle(),
            font: terminal_font(cx),
            session: None,
            error: None,
            marked: None,
            marked_selection: None,
            cursor_bounds: None,
            cell_width: px(8.0),
            line_height: px(18.0),
            title: BASE_TITLE.to_string(),
        };
        let (program, args) = default_shell();
        match Session::spawn(&program, &args, 80, 24) {
            Ok(Spawned { session, reader }) => {
                view.session = Some(session);
                let (tx, rx) = async_channel::unbounded::<Vec<u8>>();
                spawn_reader(reader, move |bytes| tx.send_blocking(bytes).is_ok());
                cx.spawn(async move |this, cx| {
                    while let Ok(mut batch) = rx.recv().await {
                        while let Ok(more) = rx.try_recv() {
                            batch.extend_from_slice(&more);
                        }
                        let fed = this.update(cx, |view, cx| {
                            if let Some(session) = view.session.as_mut() {
                                session.feed(&batch);
                            }
                            cx.notify();
                        });
                        if fed.is_err() {
                            return;
                        }
                    }
                })
                .detach();
                // ConPTY keeps its output pipe open after the shell exits, so
                // poll the child and close the app when PowerShell is gone.
                cx.spawn(async move |this, cx| {
                    loop {
                        cx.background_executor()
                            .timer(Duration::from_millis(500))
                            .await;
                        let exited = this
                            .update(cx, |view, _| {
                                view.session.as_mut().is_none_or(|s| s.child_exited())
                            })
                            .unwrap_or(true);
                        if exited {
                            let _ = cx.update(|cx| cx.quit());
                            return;
                        }
                    }
                })
                .detach();
            }
            Err(error) => view.error = Some(format!("{error:#}")),
        }
        view
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.marked.is_some() {
            return; // the IME owns keys while composing
        }
        let keystroke = &event.keystroke;
        let mods = keystroke.modifiers;
        if mods.platform {
            return;
        }
        let key = keystroke.key.as_str();
        let paste = (mods.control && mods.shift && key == "v") || (mods.shift && key == "insert");
        if paste {
            if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text())
                && let Some(session) = self.session.as_mut()
            {
                session.send_text(&text.replace("\r\n", "\r").replace('\n', "\r"));
            }
            cx.stop_propagation();
            return;
        }
        let named = matches!(
            key,
            "enter"
                | "backspace"
                | "tab"
                | "escape"
                | "up"
                | "down"
                | "left"
                | "right"
                | "home"
                | "end"
                | "pageup"
                | "pagedown"
                | "insert"
                | "delete"
        ) || (key.len() >= 2 && key.starts_with('f') && key[1..].parse::<u8>().is_ok());
        // Ctrl+Alt is AltGr on many layouts: leave it to WM_CHAR as text.
        let altgr = mods.control && mods.alt;
        if !(named || ((mods.control || mods.alt) && !altgr)) {
            // Plain text: Windows turns the key into WM_CHAR / IME messages,
            // which arrive through the EntityInputHandler below.
            return;
        }
        let mut chord = String::new();
        if mods.control {
            chord.push_str("ctrl+");
        }
        if mods.alt {
            chord.push_str("alt+");
        }
        if mods.shift {
            chord.push_str("shift+");
        }
        chord.push_str(key);
        if let Some(session) = self.session.as_mut()
            && session.send_chord(&chord)
        {
            cx.stop_propagation();
        }
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let delta = event.delta.pixel_delta(self.line_height);
        let lines = (f32::from(delta.y) / f32::from(self.line_height)).round() as isize;
        if lines != 0
            && let Some(session) = self.session.as_mut()
        {
            session.scroll_lines(-lines);
            cx.notify();
        }
    }
}

impl Focusable for TermView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for TermView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let root = div()
            .size_full()
            .bg(hsla(DEFAULT_BG))
            .text_color(hsla(DEFAULT_FG))
            .p(px(PADDING))
            .track_focus(&self.focus_handle)
            .key_context("Terminal")
            .on_key_down(cx.listener(Self::on_key_down))
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, _| {
                    window.focus(&this.focus_handle);
                }),
            );
        match &self.error {
            Some(error) => root.child(format!("Could not start the shell: {error}")),
            None => root.child(TermElement { view: cx.entity() }),
        }
    }
}

impl EntityInputHandler for TermView {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let marked = self.marked.as_deref().unwrap_or("");
        let start = utf16_to_utf8(marked, range_utf16.start);
        let end = utf16_to_utf8(marked, range_utf16.end).max(start);
        adjusted_range.replace(utf8_to_utf16(marked, start)..utf8_to_utf16(marked, end));
        Some(marked[start..end].to_string())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let len = self.marked.as_deref().map_or(0, utf16_len);
        Some(UTF16Selection {
            range: self.marked_selection.clone().unwrap_or(len..len),
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_deref().map(|m| 0..utf16_len(m))
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked = None;
        self.marked_selection = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Commit: typed characters, IME results, Unikey/EVKey injected text.
        self.marked = None;
        self.marked_selection = None;
        let text: String = text
            .chars()
            .filter(|c| *c == '\t' || !c.is_control())
            .collect();
        if let Some(session) = self.session.as_mut() {
            session.send_text(&text);
        }
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if new_text.is_empty() {
            self.marked = None;
            self.marked_selection = None;
        } else {
            self.marked = Some(new_text.to_string());
            self.marked_selection = new_selected_range;
        }
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _element_bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let cursor = self.cursor_bounds?;
        let x = cursor.origin.x + self.cell_width * range_utf16.start as f32;
        let width = self.cell_width * range_utf16.len().max(1) as f32;
        Some(Bounds::new(
            point(x, cursor.origin.y),
            size(width, self.line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

struct TermElement {
    view: Entity<TermView>,
}

impl IntoElement for TermElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

struct TextPaint {
    origin: Point<Pixels>,
    line: ShapedLine,
}

#[derive(Default)]
struct Prepaint {
    backgrounds: Vec<PaintQuad>,
    texts: Vec<TextPaint>,
    cursor: Option<PaintQuad>,
    preedit_bg: Option<PaintQuad>,
    preedit: Option<TextPaint>,
    cursor_bounds: Option<Bounds<Pixels>>,
    line_height: Pixels,
}

#[derive(Clone, PartialEq)]
struct RunStyle {
    fg: Hsla,
    bg: Option<Hsla>,
    bold: bool,
    italic: bool,
    underline: bool,
    strikethrough: bool,
}

struct RunAcc {
    col: usize,
    cols: usize,
    text: String,
    style: RunStyle,
    wide: bool,
}

fn cell_style(cell: &ghostty_vt::Cell, default_fg: Rgb, default_bg: Rgb) -> RunStyle {
    let mut fg = cell.resolved_fg.unwrap_or(default_fg);
    let mut bg = cell.resolved_bg;
    if cell.inverse {
        let new_bg = Some(fg);
        fg = bg.unwrap_or(default_bg);
        bg = new_bg;
    }
    let mut fg = hsla(fg);
    if cell.faint {
        fg.a = 0.6;
    }
    RunStyle {
        fg,
        bg: bg.filter(|b| *b != default_bg).map(hsla),
        bold: cell.bold,
        italic: cell.italic,
        underline: cell.underline || cell.underline_style.is_some(),
        strikethrough: cell.strikethrough,
    }
}

impl Element for TermElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;

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
        let base_font = self.view.read(cx).font.clone();
        let font_size = px(FONT_SIZE);
        let text_system = cx.text_system().clone();
        let font_id = text_system.resolve_font(&base_font);
        let cell_w = text_system
            .advance(font_id, font_size, 'M')
            .map(|s| s.width)
            .unwrap_or(px(FONT_SIZE * 0.6));
        // Whole device pixels per row so rows never blur at 125/150 %.
        let scale = window.scale_factor();
        let line_h = px((FONT_SIZE * LINE_HEIGHT * scale).round() / scale);
        let cols = (f32::from(bounds.size.width) / f32::from(cell_w))
            .floor()
            .max(2.0) as u16;
        let rows = (f32::from(bounds.size.height) / f32::from(line_h))
            .floor()
            .max(1.0) as u16;
        let cell_w_px = (f32::from(cell_w) * scale).round() as u32;
        let cell_h_px = (f32::from(line_h) * scale).round() as u32;

        let (frame, marked, focused, title) = self.view.update(cx, |view, _| {
            view.cell_width = cell_w;
            view.line_height = line_h;
            let focused = view.focus_handle.is_focused(window);
            let Some(session) = view.session.as_mut() else {
                return (None, None, focused, None);
            };
            let _ = session.resize(cols, rows, cell_w_px, cell_h_px);
            let title = session.title();
            (session.frame().ok(), view.marked.clone(), focused, title)
        });
        let Some(frame) = frame else {
            return Prepaint {
                line_height: line_h,
                ..Default::default()
            };
        };

        let mut out = Prepaint {
            line_height: line_h,
            ..Default::default()
        };
        let (default_bg, default_fg) = frame.default_colors;
        let cell_origin = |col: usize, row: usize| {
            point(
                bounds.origin.x + cell_w * col as f32,
                bounds.origin.y + line_h * row as f32,
            )
        };

        let flush = |acc: Option<RunAcc>, row: usize, out: &mut Prepaint, window: &mut Window| {
            let Some(acc) = acc else { return };
            let origin = cell_origin(acc.col, row);
            if let Some(bg) = acc.style.bg {
                out.backgrounds.push(fill(
                    Bounds::new(origin, size(cell_w * acc.cols as f32, line_h)),
                    bg,
                ));
            }
            let decorated = acc.style.underline || acc.style.strikethrough;
            if acc.text.trim().is_empty() && !decorated {
                return;
            }
            let mut run_font = base_font.clone();
            if acc.style.bold {
                run_font = run_font.bold();
            }
            if acc.style.italic {
                run_font = run_font.italic();
            }
            let fg = acc.style.fg;
            let run = TextRun {
                len: acc.text.len(),
                font: run_font,
                color: fg,
                background_color: None,
                underline: acc.style.underline.then_some(UnderlineStyle {
                    thickness: px(1.0),
                    color: Some(fg),
                    wavy: false,
                }),
                strikethrough: acc.style.strikethrough.then_some(StrikethroughStyle {
                    thickness: px(1.0),
                    color: Some(fg),
                }),
            };
            let line = window.text_system().shape_line(
                SharedString::from(acc.text),
                font_size,
                &[run],
                None,
            );
            out.texts.push(TextPaint { origin, line });
        };

        let mut cursor_line_text = String::new();
        for (row, cells) in frame.styled_rows().iter().enumerate() {
            let mut acc: Option<RunAcc> = None;
            let is_cursor_row = frame.cursor.is_some_and(|c| usize::from(c.y) == row);
            for (col, cell) in cells.iter().enumerate() {
                if cell.width == CellWidth::SpacerTail {
                    continue; // covered by the wide lead cell before it
                }
                let wide = cell.width == CellWidth::Wide;
                let text = if cell.text.is_empty()
                    || cell.invisible
                    || cell.width == CellWidth::SpacerHead
                {
                    " "
                } else {
                    cell.text.as_str()
                };
                if is_cursor_row {
                    cursor_line_text.push_str(text);
                }
                let style = cell_style(cell, default_fg, default_bg);
                let breaks = acc
                    .as_ref()
                    .is_none_or(|a| a.style != style || a.wide || wide);
                if breaks {
                    flush(acc.take(), row, &mut out, window);
                    acc = Some(RunAcc {
                        col,
                        cols: 0,
                        text: String::new(),
                        style,
                        wide,
                    });
                }
                let a = acc.as_mut().expect("run started above");
                a.text.push_str(text);
                a.cols += if wide { 2 } else { 1 };
            }
            flush(acc.take(), row, &mut out, window);
        }

        // Cursor, then the IME preedit drawn inline over the cursor cell.
        if let Some(cursor) = frame.cursor {
            let origin = cell_origin(usize::from(cursor.x), usize::from(cursor.y));
            let color = hsla(frame.cursor_color.unwrap_or(CURSOR));
            let mut cursor_origin = origin;
            if let Some(text) = marked.as_ref() {
                let run = TextRun {
                    len: text.len(),
                    font: base_font.clone(),
                    color: hsla(default_fg),
                    background_color: None,
                    underline: Some(UnderlineStyle {
                        thickness: px(1.5),
                        color: Some(color),
                        wavy: false,
                    }),
                    strikethrough: None,
                };
                let line = window.text_system().shape_line(
                    SharedString::from(text.clone()),
                    font_size,
                    &[run],
                    None,
                );
                let width = line.width.max(cell_w);
                out.preedit_bg = Some(fill(
                    Bounds::new(origin, size(width, line_h)),
                    hsla(default_bg),
                ));
                cursor_origin.x += line.width;
                out.preedit = Some(TextPaint { origin, line });
            }
            out.cursor_bounds = Some(Bounds::new(origin, size(cell_w, line_h)));
            let cell = Bounds::new(cursor_origin, size(cell_w, line_h));
            let shape = if marked.is_some() {
                CursorShape::Bar
            } else {
                cursor.shape
            };
            out.cursor = Some(match (focused, shape) {
                (false, _) | (true, CursorShape::BlockHollow) => {
                    outline(cell, color, BorderStyle::Solid)
                }
                (true, CursorShape::Bar) => {
                    fill(Bounds::new(cursor_origin, size(px(2.0), line_h)), color)
                }
                (true, CursorShape::Underline) => fill(
                    Bounds::new(
                        point(cursor_origin.x, cursor_origin.y + line_h - px(2.0)),
                        size(cell_w, px(2.0)),
                    ),
                    color,
                ),
                (true, CursorShape::Block) => {
                    let mut block = color;
                    block.a = 0.55;
                    fill(cell, block)
                }
            });
        }

        // Accessibility stopgap: GPUI 0.2.2 has no UIA tree, so expose the
        // cursor line through the window title (Narrator: Insert+T).
        let line = cursor_line_text.trim_end();
        let wanted = match (title, line.is_empty()) {
            (Some(t), true) => format!("{t} - {BASE_TITLE}"),
            (_, false) => format!("{line} - {BASE_TITLE}"),
            (None, true) => BASE_TITLE.to_string(),
        };
        let changed = self.view.update(cx, |view, _| {
            if view.title != wanted {
                view.title = wanted.clone();
                true
            } else {
                false
            }
        });
        if changed {
            window.set_window_title(&wanted);
        }
        out
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.view.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.view.clone()),
            cx,
        );

        for quad in prepaint.backgrounds.drain(..) {
            window.paint_quad(quad);
        }
        if let Some(cursor) = prepaint.cursor.take() {
            window.paint_quad(cursor);
        }
        let line_height = prepaint.line_height;
        for text in prepaint.texts.drain(..) {
            let _ = text.line.paint(text.origin, line_height, window, cx);
        }
        if let Some(bg) = prepaint.preedit_bg.take() {
            window.paint_quad(bg);
        }
        if let Some(text) = prepaint.preedit.take() {
            let _ = text.line.paint(text.origin, line_height, window, cx);
        }
        let cursor_bounds = prepaint.cursor_bounds;
        self.view
            .update(cx, |view, _| view.cursor_bounds = cursor_bounds);
    }
}
