//! The terminal terminal widget: renders a `Session` and feeds it input.
//!
//! Rendering follows the cell-grid discipline (ghostty's renderer notes):
//! integer cell advance from the monospace font, background rects merged
//! into per-row runs, glyphs batched into one instance buffer, underline
//! styles drawn by a dedicated shader, block/bar/underline cursor with a
//! hollow variant when unfocused.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use makepad_widgets::text::geom::Point;
use makepad_widgets::makepad_draw::shader::draw_text::{FontFamily, PreparedGlyph};
use makepad_widgets::*;

use crate::session::Session;
use crate::term::color::{default_palette, Rgb};
use crate::term::key_encode::{
    encode_key, Key, KeyAction, KeyEncodeOptions, KeyEvent as TermKeyEvent, KeyMods, KittyFlags,
};
use crate::term::modes::Mode;
use crate::term::mouse_encode::{
    encode_mouse, MouseButton as TermMouseButton, MouseEventKind, MouseFormat, MouseReport,
    MouseTracking,
};
use crate::term::page::CellContent;
use crate::term::screen::CursorStyle;
use crate::term::style::{StyleColor, StyleFlags};
use crate::term::terminal::{TermEvent, Terminal};

script_mod! {
    use mod.prelude.widgets_internal.*
    use mod.widgets.*

    /** The terminal cell background: one flat premultiplied quad per run
     * of same-colored cells, batched into its own draw-call group. */
    set_type_default() do #(DrawTermBg::script_shader(vm)) {
        ..mod.draw.DrawQuad
        draw_call_group: @term_cell_bg
        /** default cell background */
        color: #x1a1b26
        pixel: fn() {
            return vec4(self.color.rgb * self.color.a, self.color.a)
        }
    }

    // Each cell shades a continuing stroke at its outer edges. Independent
    // antialiased vector paths fade at every cell boundary and make TUI rules
    // look dotted, especially when the terminal sits on a zoomed canvas.
    set_type_default() do #(DrawTermBox::script_shader(vm)) {
        ..mod.draw.DrawQuad
        draw_call_group: @term_box
        color: #fff
        arms: vec4(0.0, 0.0, 0.0, 0.0)
        center: vec2(0.0, 0.0)
        corner: vec2(0.0, 0.0)
        thickness: 1.0
        radius: 0.0
        pixel_size: 1.0
        pixel: fn() {
            let d = self.pos * self.rect_size - self.center
            let half = self.thickness * 0.5
            var distance = 10000.0
            if self.radius > 0.0 {
                let q = d * self.corner
                if q.x >= self.radius {
                    distance = abs(q.y) - half
                } else if q.y >= self.radius {
                    distance = abs(q.x) - half
                } else {
                    distance = abs(length(q - vec2(self.radius)) - self.radius) - half
                }
            } else {
                if self.arms.x + self.arms.y > 0.0 {
                    let left = if self.arms.x > 0.0 {-10000.0} else {-half}
                    let right = if self.arms.y > 0.0 {10000.0} else {half}
                    distance = max(abs(d.y) - half, max(left - d.x, d.x - right))
                }
                if self.arms.z + self.arms.w > 0.0 {
                    let top = if self.arms.z > 0.0 {-10000.0} else {-half}
                    let bottom = if self.arms.w > 0.0 {10000.0} else {half}
                    distance = min(distance, max(abs(d.x) - half, max(top - d.y, d.y - bottom)))
                }
            }
            let coverage = clamp(0.5 - distance / self.pixel_size, 0.0, 1.0)
            let alpha = self.color.a * coverage
            return vec4(self.color.rgb * alpha, alpha)
        }
    }

    set_type_default() do #(DrawTermUnderline::script_shader(vm)) {
        ..mod.draw.DrawQuad
        draw_call_group: @term_underline
        color: #xc0caf5
        kind: 1.0
        on: fn(a: float) {
            return vec4(self.color.rgb * self.color.a * a, self.color.a * a)
        }
        pixel: fn() {
            if self.kind < 1.5 {
                return self.on(1.0)
            }
            if self.kind < 2.5 {
                // Double: two bands with a gap.
                if self.pos.y < 0.33 || self.pos.y > 0.66 {
                    return self.on(1.0)
                }
                return self.on(0.0)
            }
            if self.kind < 3.5 {
                // Curly: sine band.
                let center = 0.5 + sin(self.pos.x * self.rect_size.x * 1.2) * 0.3
                if abs(self.pos.y - center) < 0.22 {
                    return self.on(1.0)
                }
                return self.on(0.0)
            }
            if self.kind < 4.5 {
                // Dotted.
                if modf(self.pos.x * self.rect_size.x, 3.0) < 1.5 {
                    return self.on(1.0)
                }
                return self.on(0.0)
            }
            // Dashed.
            if modf(self.pos.x * self.rect_size.x, 8.0) < 5.0 {
                return self.on(1.0)
            }
            return self.on(0.0)
        }
    }

    set_type_default() do #(DrawTermCursor::script_shader(vm)) {
        ..mod.draw.DrawQuad
        draw_call_group: @term_cursor
        color: #xc0caf5
        hollow: 0.0
        pixel: fn() {
            if self.hollow < 0.5 {
                return vec4(self.color.rgb * self.color.a, self.color.a)
            }
            let border = 1.0
            let bx = border / self.rect_size.x
            let by = border / self.rect_size.y
            if self.pos.x < bx || self.pos.x > 1.0 - bx || self.pos.y < by || self.pos.y > 1.0 - by {
                return vec4(self.color.rgb, 1.0)
            }
            return vec4(0.0, 0.0, 0.0, 0.0)
        }
    }

    mod.widgets.MpTermBase = #(MpTerm::register_widget(vm))

    /** The terminal widget: cell-background, glyph, underline and cursor
     * draw layers over a monospace grid. */
    mod.widgets.MpTerm = set_type_default() do mod.widgets.MpTermBase {
        width: Fill
        height: Fill
        /** terminal font size in points 6..24 step 0.5 */
        font_size: 10.0
        /** horizontal inner padding in pixels 0..32 step 1 */
        pad_x: 6.0
        /** vertical inner padding in pixels 0..32 step 1 */
        pad_y: 4.0
        draw_bg +: {
            color: uniform(#x1a1b26)
            inset: uniform(0.0)
            corner_radius: uniform(0.0)
            frame_width: uniform(0.0)
            frame_color: uniform(#0000)
            pixel: fn() {
                let p = self.pos * self.rect_size
                if self.corner_radius > 0.0 {
                    let sdf = Sdf2d.viewport(p)
                    let edge = self.frame_width
                    sdf.box(edge, edge, self.rect_size.x - 2.0 * edge,
                        self.rect_size.y - 2.0 * edge, self.corner_radius)
                    sdf.fill_keep(self.color)
                    sdf.stroke(self.frame_color, edge)
                    return sdf.result
                }
                let tl = min(p.x, p.y)
                let br = min(self.rect_size.x - p.x, self.rect_size.y - p.y)
                if min(tl, br) < self.inset {
                    // Classic sunken client edge: shadow then black at the
                    // top/left, highlight then button face at bottom/right.
                    let edge = if tl < br {
                        if tl < 1.0 {#808080} else {#000000}
                    } else {
                        if br < 1.0 {#ffffff} else {theme.color_bg_app}
                    }
                    return edge
                }
                return vec4(self.color.rgb * self.color.a, self.color.a)
            }
        }
        draw_text +: {
            draw_call_group: @term_text
            text_style: theme.font_code{line_spacing: 1.0}
        }
        bold_text_style: theme.font_code{line_spacing: 1.0}
        draw_cell_bg +: {}
        draw_underline +: {}
        draw_cursor +: {}
    }
}

#[derive(Script, ScriptHook)]
#[repr(C)]
struct DrawTermBg {
    #[deref]
    draw_super: DrawQuad,
    #[live]
    color: Vec4f,
}

#[derive(Script, ScriptHook)]
#[repr(C)]
struct DrawTermUnderline {
    #[deref]
    draw_super: DrawQuad,
    #[live]
    color: Vec4f,
    #[live]
    kind: f32,
}

#[derive(Script, ScriptHook)]
#[repr(C)]
struct DrawTermBox {
    #[deref]
    draw_super: DrawQuad,
    #[live]
    color: Vec4f,
    #[live]
    arms: Vec4f,
    #[live]
    center: Vec2f,
    #[live]
    corner: Vec2f,
    #[live]
    thickness: f32,
    #[live]
    radius: f32,
    #[live]
    pixel_size: f32,
}

#[derive(Script, ScriptHook)]
#[repr(C)]
struct DrawTermCursor {
    #[deref]
    draw_super: DrawQuad,
    #[live]
    color: Vec4f,
    #[live]
    hollow: f32,
}

#[derive(Clone, Debug, Default)]
pub enum MpTermAction {
    TitleChanged(String),
    PwdChanged(String),
    Bell,
    Notification {
        title: String,
        body: String,
    },
    Exited,
    FileDropped {
        path: PathBuf,
    },
    PromptSubmitted,
    #[default]
    None,
}

#[derive(Clone, Copy)]
struct CachedGlyph {
    glyph: PreparedGlyph,
    font_size_in_lpxs: f32,
    x_offset_in_lpxs: f32,
    y_offset_in_lpxs: f32,
}

/// The tokyo-night terminal palette (terminal's default look; makepad-app-wm
/// re-themes at spawn time via --theme args later).
pub fn default_theme() -> ([Rgb; 16], Rgb, Rgb) {
    let base16 = [
        Rgb::new(0x1a, 0x1b, 0x26), // black = background
        Rgb::new(0xf7, 0x76, 0x8e),
        Rgb::new(0x9e, 0xce, 0x6a),
        Rgb::new(0xe0, 0xaf, 0x68),
        Rgb::new(0x7a, 0xa2, 0xf7),
        Rgb::new(0xad, 0x8e, 0xe6),
        Rgb::new(0x44, 0x9d, 0xab),
        Rgb::new(0xa9, 0xb1, 0xd6), // white = foreground
        Rgb::new(0x41, 0x48, 0x68), // bright black = muted
        Rgb::new(0xff, 0x7a, 0x93),
        Rgb::new(0xb9, 0xf2, 0x7c),
        Rgb::new(0xff, 0x9e, 0x64),
        Rgb::new(0x7d, 0xa6, 0xff),
        Rgb::new(0xbb, 0x9a, 0xf7),
        Rgb::new(0x0d, 0xb9, 0xd7),
        Rgb::new(0xc0, 0xca, 0xf5), // bright white = bright fg
    ];
    (
        base16,
        Rgb::new(0xa9, 0xb1, 0xd6),
        Rgb::new(0x1a, 0x1b, 0x26),
    )
}

const SELECTION_COLOR: Vec4f = Vec4f {
    x: 0x29 as f32 / 255.0,
    y: 0x2e as f32 / 255.0,
    z: 0x42 as f32 / 255.0,
    w: 1.0,
};

/// Optional host frame. Padding belongs to the terminal grid, so drawing,
/// pointer input and IME use the same full-sized rounded surface.
#[derive(Clone, Copy, PartialEq)]
pub struct TerminalPresentationFrame {
    pub padding: f64,
    pub radius: f32,
    pub border_width: f32,
    pub border_color: Vec4f,
}

#[derive(Default)]
pub struct TerminalColors {
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
    pub cursor: Option<Rgb>,
    pub palette: Vec<(u8, Rgb)>,
    pub background_opacity: Option<f32>,
}

#[derive(Script, Widget)]
pub struct MpTerm {
    #[uid]
    uid: WidgetUid,
    #[source]
    source: ScriptObjectRef,
    #[walk]
    walk: Walk,
    #[layout]
    layout: Layout,
    #[redraw]
    #[live]
    draw_bg: DrawQuad,
    #[live]
    draw_text: DrawText,
    #[live]
    bold_text_style: TextStyle,
    #[live]
    draw_boxes: DrawTermBox,
    #[live]
    draw_dots: DrawVector,
    #[live]
    draw_cell_bg: DrawTermBg,
    #[live]
    draw_underline: DrawTermUnderline,
    #[live]
    draw_cursor: DrawTermCursor,
    #[live(10.0)]
    font_size: f64,
    #[rust((1.0, 0.0))]
    cell_height_adjustment: (f64, f64),
    #[rust]
    system_fonts_configured: bool,
    #[rust]
    color_overrides: TerminalColors,
    #[live(6.0)]
    pad_x: f64,
    #[live(4.0)]
    pad_y: f64,
    /// Take the keyboard on the first draw (a terminal window wants it; a
    /// host with other fields, like a docked panel, gives it on click or
    /// through `take_key_focus`).
    #[live(true)]
    focus_on_first_draw: bool,

    #[rust]
    session: Option<Session>,
    #[rust]
    pub cwd: Option<PathBuf>,
    #[rust]
    pub shell: Option<String>,
    /// Variables the session's shell starts with on top of this process's
    /// environment (a host naming a context file for the job it runs).
    #[rust]
    pub env: Vec<(String, String)>,
    /// A transformed host supplies its screen area and local-to-screen mapping.
    #[rust]
    pub canvas_ime_anchor: Option<(Area, PopupAnchorTransform)>,
    /// A one-shot job instead of the interactive shell (`--preview` runs
    /// the pager on a file); the session ends when it exits.
    #[rust]
    pub command: Option<String>,
    #[rust]
    pub copy_on_select: bool,
    #[rust(true)]
    pub ligatures: bool,
    /// An unloaded Quick Look panel: no session, and none respawned until
    /// the next retarget (`restart_with`).
    #[rust]
    dormant: bool,
    #[rust]
    ime_preedit: String,
    #[rust]
    area: Area,
    /// Key focus grabbed on the first draw (a terminal owns the keyboard
    /// without needing a click).
    #[rust]
    took_focus: bool,
    #[rust]
    rect: Rect,
    #[rust]
    cell_w: f64,
    #[rust]
    cell_h: f64,
    #[rust]
    cell_baseline: f64,
    #[rust]
    glyph_cache: HashMap<(char, bool, u8), Option<CachedGlyph>>,
    #[rust]
    run_cache: HashMap<(String, bool), Option<Vec<CachedGlyph>>>,
    #[rust]
    glyph_cache_key: (u64, u64, u64, u64, u64),
    /// Lines scrolled back from the bottom (0 = live).
    #[rust]
    view_offset: usize,
    // Selection in absolute (eviction-stable) rows.
    #[rust]
    sel_anchor: Option<(u64, usize)>,
    #[rust]
    sel_cursor: Option<(u64, usize)>,
    #[rust]
    selecting: bool,
    #[rust]
    last_finger: Option<Vec2d>,
    #[rust]
    select_scroll_frame: NextFrame,
    #[rust]
    bell_frames: u8,
    #[rust]
    last_mouse_cell: Option<(u32, u32, u8)>,
    /// Background alpha (focused, unfocused): Omarchy's window opacity rule
    /// "0.78 0.70", handed down by makepad-app-wm via MAKEPAD_TERMINAL_OPACITY. Standalone
    /// runs are opaque. The shared swapchain is BGRA and the compositor
    /// blends premultiplied, so the wallpaper shows through for free.
    #[rust((1.0, 1.0))]
    bg_opacity: (f32, f32),
    #[rust]
    opaque_style: bool,
    #[rust]
    background_dimming: f32,
    #[rust(1.0)]
    presentation_font_scale: f64,
    #[rust]
    presentation_frame: Option<TerminalPresentationFrame>,
    #[rust]
    style_colors: Option<(Rgb, Rgb)>,
    /// The host theme's `color_terminal_cursor`, when it sets one.
    #[rust]
    style_cursor: Option<Rgb>,
    #[rust]
    original_colors: Option<([Rgb; 16], Rgb, Rgb)>,
    /// A viewer, not a shell: no key focus on first draw, no typing, paste,
    /// drop or IME. Selection, copy and scrollback still work.
    #[rust]
    pub read_only: bool,
    /// Rows leaving the primary grid for scrollback also go here (kept
    /// across session restarts); see `set_scrollback_sink`.
    #[rust]
    scrollback_sink: Option<crate::term::screen::ScrollbackSink>,
}

impl ScriptHook for MpTerm {
    fn on_after_apply(
        &mut self,
        vm: &mut ScriptVm,
        _apply: &Apply,
        _scope: &mut Scope,
        _value: ScriptValue,
    ) {
        self.glyph_cache.clear();
        let style = desktop_style::current_style(vm);
        let retro = matches!(
            style,
            desktop_style::DesktopStyle::Windows2000 | desktop_style::DesktopStyle::NextStep
        );
        self.opaque_style = retro || style == desktop_style::DesktopStyle::Android;
        self.draw_bg.draw_vars.set_uniform(
            vm.cx_mut(),
            id!(inset),
            &[if retro { 2.0 } else { 0.0 }],
        );
        let foreground = theme_tokens::color(vm, "color_terminal_text")
            .or_else(|| theme_tokens::color(vm, "color_text"));
        let background = theme_tokens::color(vm, "color_terminal_bg")
            .or_else(|| theme_tokens::color(vm, "color_bg_app"));
        let rgb = |color: Vec4f| {
            let rgba = color.to_u32();
            Rgb::new((rgba >> 24) as u8, (rgba >> 16) as u8, (rgba >> 8) as u8)
        };
        self.style_colors = foreground.zip(background).map(|(fg, bg)| (rgb(fg), rgb(bg)));
        self.style_cursor = theme_tokens::color(vm, "color_terminal_cursor").map(rgb);
        if let (Some(session), Some(cursor)) = (&mut self.session, self.style_cursor) {
            session.terminal.cursor_color = Some(cursor);
        }
        if let (Some(session), Some(colors)) = (&mut self.session, self.original_colors) {
            self.color_overrides.apply(&mut session.terminal, colors, self.style_colors);
        }
    }
}

impl MpTerm {
    /// Prepend installed faces to the template's bundled glyph fallbacks.
    pub fn apply_system_font_families(
        &mut self,
        cx: &mut Cx,
        regular: &[String],
        bold: &[String],
        bold_weight: f32,
    ) {
        if regular.is_empty() && bold.is_empty() {
            return;
        }
        cx.with_vm(|vm| {
            let source: ScriptValue = self.source.clone().into();
            let Some(source) = source.as_object() else {
                return;
            };
            if !regular.is_empty() {
                if let Some(style) = vm
                    .value(source, id!(draw_text))
                    .as_object()
                    .and_then(|draw| vm.value(draw, id!(text_style)).as_object())
                {
                    self.draw_text.text_style.font_family =
                        system_font_family(vm, style, regular.iter(), 400.0);
                }
            }
            if let Some(style) = vm.value(source, id!(bold_text_style)).as_object() {
                self.bold_text_style.font_family =
                    system_font_family(vm, style, bold.iter().chain(regular), bold_weight);
                self.system_fonts_configured = true;
            }
        });
        self.glyph_cache.clear();
    }

    /// Override the widget template's font settings when creating a hosted terminal.
    pub fn apply_font_overrides(
        &mut self,
        size: Option<f64>,
        bold_weight: Option<f32>,
        height_scale: f64,
        height_pixels: f64,
    ) {
        if let Some(size) = size {
            self.font_size = size;
        }
        if let Some(weight) = bold_weight {
            self.bold_text_style.font_family = self
                .bold_text_style
                .font_family
                .with_axes(&[(u32::from_be_bytes(*b"wght"), weight)]);
        }
        self.cell_height_adjustment = (height_scale, height_pixels);
        self.glyph_cache.clear();
    }

    /// Set the initial colors for a hosted terminal before its session is started.
    pub fn set_color_overrides(&mut self, colors: TerminalColors) {
        self.color_overrides = colors;
    }

    pub fn set_presentation_frame(&mut self, cx: &mut Cx, frame: Option<TerminalPresentationFrame>) {
        if self.presentation_frame != frame {
            self.presentation_frame = frame;
            self.draw_bg.redraw(cx);
        }
    }

    fn inner_padding(&self) -> DVec2 {
        self.presentation_frame.map_or(dvec2(self.pad_x, self.pad_y), |frame| {
            dvec2(frame.padding, frame.padding)
        })
    }

    /// Scale a hosted terminal's typography without replacing its session or
    /// changing the user's configured base font size.
    pub fn set_presentation_font_scale(&mut self, cx: &mut Cx, scale: f64) {
        let scale = scale.clamp(0.5, 2.0);
        if self.presentation_font_scale != scale {
            self.presentation_font_scale = scale;
            self.draw_bg.redraw(cx);
        }
    }
    /// A hosted presentation can deepen the surface without changing the PTY
    /// palette or restarting its session. Zero restores the provider theme.
    pub fn set_background_dimming(&mut self, cx: &mut Cx, amount: f32) {
        let amount = amount.clamp(0.0, 0.8);
        if self.background_dimming != amount {
            self.background_dimming = amount;
            self.draw_bg.redraw(cx);
        }
    }
    /// Also hand every row that leaves the primary grid for scrollback to
    /// `sink` (a host keeping its own copy of the history). The rows already
    /// in scrollback are sent first, oldest first; later sessions of this
    /// widget (a restart) keep the sink. `None` stops it.
    pub fn set_scrollback_sink(&mut self, sink: Option<crate::term::screen::ScrollbackSink>) {
        self.scrollback_sink = sink.clone();
        if let Some(session) = self.session.as_mut() {
            let primary = &mut session.terminal.primary;
            if let Some(sink) = &sink {
                for row in &primary.scrollback {
                    sink(row);
                }
            }
            primary.scrollback_sink = sink;
        }
    }

    /// One cell in logical pixels at the current font and presentation
    /// scale (zero before the first draw), and that scale.
    pub fn cell_size(&self) -> (DVec2, f64) {
        (dvec2(self.cell_w, self.cell_h), self.presentation_font_scale)
    }

    /// The grid's columns and rows (None without a session).
    pub fn grid(&self) -> Option<(usize, usize)> {
        let term = &self.session.as_ref()?.terminal;
        Some((term.cols(), term.rows()))
    }

    /// Start the session now rather than on the first draw: a restored
    /// resident that a hidden presentation has not drawn yet still runs.
    pub fn ensure_started(&mut self, cx: &mut Cx) {
        self.ensure_session(cx);
    }
    /// A hosting presentation is hiding this terminal: end any selection
    /// drag and its edge auto-scroll so nothing keeps requesting frames.
    pub fn cancel_gestures(&mut self, cx: &mut Cx) {
        if self.selecting || self.last_finger.is_some() {
            self.selecting = false;
            self.last_finger = None;
            self.area.redraw(cx);
        }
    }
    pub fn child_pid(&self) -> Option<i32> {
        self.session.as_ref().map(Session::child_pid)
    }

    /// Give this terminal the keyboard (after its first draw).
    pub fn take_key_focus(&mut self, cx: &mut Cx) {
        cx.set_key_focus(self.area);
    }

    /// Whether keyboard input currently goes to this terminal's PTY area.
    pub fn has_input_focus(&self, cx: &Cx) -> bool {
        cx.has_key_focus(self.area)
    }

    /// The area that holds the keyboard when this terminal has it.
    pub fn focus_area(&self) -> Area {
        self.area
    }

    /// Rows currently painted in the widget, or the last `lines` rows of
    /// scrollback plus the active grid. Used by the terminal's AI service;
    /// row text comes from the same cell content the renderer uses.
    pub fn ai_screen_rows(
        &self,
        recent_lines: Option<usize>,
    ) -> Option<(Vec<String>, usize, usize)> {
        let session = self.session.as_ref()?;
        let screen = session.terminal.screen();
        let total = screen.total_rows();
        let (start, end) = match recent_lines {
            Some(lines) => (total.saturating_sub(lines), total),
            None => {
                let start = screen.scrollback.len().saturating_sub(self.view_offset);
                (start, (start + screen.rows).min(total))
            }
        };
        let rows = (start..end)
            .filter_map(|index| screen.row_virtual(index))
            .map(|row| row.text())
            .collect();
        Some((rows, screen.cursor.y, screen.cursor.x))
    }

    /// Type bytes through the live PTY's ordinary input path.
    pub fn ai_type_bytes(&mut self, bytes: &[u8]) -> bool {
        let Some(session) = self.session.as_mut() else {
            return false;
        };
        session.write(bytes);
        true
    }

    pub fn run_command(&mut self, cx: &mut Cx, command: &str) -> Result<(), String> {
        if command.trim().is_empty() {
            return Err("Terminal command must not be empty".into());
        }
        let enter = self.encoded_key_chord(Key::Enter, KeyMods::default(), "", 0);
        if !self.paste_bytes_with_suffix(command, &enter) {
            return Err("Terminal did not accept command input".into());
        }
        self.redraw(cx);
        Ok(())
    }

    pub fn press_key(&mut self, cx: &mut Cx, chord: &str) -> Result<(), String> {
        let (key, mods, text, unshifted) = parse_key_chord(chord)?;
        let input = self.encoded_key_chord(key, mods, &text, unshifted);
        if !self.session.as_mut().is_some_and(|session| session.try_write(&input)) {
            return Err("Terminal did not accept the key chord".into());
        }
        self.scroll_to_bottom();
        self.redraw(cx);
        Ok(())
    }

    pub fn signal_foreground(&mut self, name: &str) -> Result<(), String> {
        self.session.as_mut().ok_or("Terminal session is not running")?
            .signal_foreground(name).map_err(|err| format!("Cannot signal the foreground job: {err}"))
    }

    fn encoded_key_chord(&self, key: Key, mods: KeyMods, text: &str, unshifted: u32) -> Vec<u8> {
        let opts = self.key_opts();
        let mut event = TermKeyEvent {
            action: KeyAction::Press,
            key,
            mods,
            consumed_mods: KeyMods::default(),
            utf8: text.to_string(),
            unshifted_codepoint: unshifted,
        };
        let mut bytes = encode_key(&event, &opts);
        event.action = KeyAction::Release;
        bytes.extend(encode_key(&event, &opts));
        bytes
    }

    /// Insert one file exactly as a native terminal drop: shell-quoted path,
    /// trailing space, normal bracketed-paste protocol, and never Enter.
    /// The caller owns any file inspection/copying; this performs no file I/O.
    pub fn ai_drop_file(&mut self, path: &Path) -> bool {
        let Some(path) = path.to_str().filter(|text| Self::valid_drop_path(text)) else {
            return false;
        };
        let quoted = format!("'{}' ", path.replace('\'', "'\\''"));
        self.paste_bytes(&quoted)
    }

    fn valid_drop_path(path: &str) -> bool {
        Path::new(path).is_absolute() && path.len() <= 4096 && !path.chars().any(char::is_control)
    }

    /// Quick Look retarget (`WmEvent::PreviewFile`): tear down the current
    /// job — dropping the session kills the PTY child — and run a new one
    /// in place, same process, same window.
    pub fn restart_with(&mut self, cx: &mut Cx, cwd: Option<PathBuf>, command: Option<String>) {
        self.session = None;
        self.cwd = cwd;
        self.command = command;
        self.dormant = false;
        self.view_offset = 0;
        self.sel_anchor = None;
        self.sel_cursor = None;
        self.selecting = false;
        self.area.redraw(cx);
    }

    /// Nonblocking child status for hosts that close when their command ends.
    pub fn process_exited(&mut self) -> bool {
        self.session.as_mut().is_some_and(Session::process_exited)
    }

    /// Quick Look unload (`WmEvent::PreviewUnload`): drop the job and idle
    /// blank until the next retarget. Not a close — the process stays warm.
    pub fn unload(&mut self, cx: &mut Cx) {
        self.session = None;
        self.dormant = true;
        self.view_offset = 0;
        self.sel_anchor = None;
        self.sel_cursor = None;
        self.selecting = false;
        self.area.redraw(cx);
    }

    fn ensure_session(&mut self, _cx: &mut Cx) {
        if self.session.is_some() || self.dormant {
            return;
        }
        // draw_walk already measured this widget and its font. Starting an
        // 80x24 ConPTY and immediately resizing it reflows the first TUI frame
        // before the child's differential renderer knows its screen changed.
        let (cols, rows) = if self.rect.size.x > 0.0 && self.rect.size.y > 0.0
            && self.cell_w > 0.0 && self.cell_h > 0.0 {
            self.grid_size()
        } else { (80, 24) };
        if let Ok(spec) = std::env::var("MAKEPAD_TERMINAL_OPACITY") {
            let mut it = spec
                .split_whitespace()
                .filter_map(|s| s.parse::<f32>().ok());
            if let Some(active) = it.next() {
                let inactive = it.next().unwrap_or(active);
                self.bg_opacity = (active.clamp(0.0, 1.0), inactive.clamp(0.0, 1.0));
            }
        }
        let env: Vec<(&str, &str)> = self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        match Session::spawn(
            cols,
            rows,
            self.cwd.as_deref(),
            self.shell.as_deref(),
            self.command.as_deref(),
            &env,
        ) {
            Ok(mut session) => {
                // makepad-app-wm hands the splash theme's terminal palette down
                // via MAKEPAD_TERMINAL_COLORS; standalone runs use the bundled default.
                let (mut base16, mut fg, mut bg) = default_theme();
                if let Ok(env) = std::env::var("MAKEPAD_TERMINAL_COLORS") {
                    for pair in env.split(';') {
                        let Some((key, value)) = pair.split_once('=') else {
                            continue;
                        };
                        let Some(rgb) = parse_hex_rgb(value) else {
                            continue;
                        };
                        if let Some(idx) = key.strip_prefix("color") {
                            if let Ok(i) = idx.parse::<usize>() {
                                if i < 16 {
                                    base16[i] = rgb;
                                }
                            }
                        } else if key == "foreground" {
                            fg = rgb;
                        } else if key == "background" {
                            bg = rgb;
                        } else if key == "cursor" {
                            session.terminal.cursor_color = Some(rgb);
                        }
                    }
                }
                self.original_colors = Some((base16, fg, bg));
                if session.terminal.cursor_color.is_none() {
                    session.terminal.cursor_color = self.style_cursor;
                }
                self.color_overrides.apply(&mut session.terminal, (base16, fg, bg), self.style_colors);
                session.terminal.primary.scrollback_sink = self.scrollback_sink.clone();
                self.session = Some(session);
            }
            Err(err) => {
                error!("terminal: failed to spawn shell: {}", err);
            }
        }
    }

    fn refresh_metrics(&mut self, cx: &mut Cx2d) {
        let font_size = self.font_size * self.presentation_font_scale;
        self.draw_text.text_style.font_size = font_size as f32;
        let metrics = self.draw_text.prepare_single_line_run(cx, "M");
        if self.system_fonts_configured {
            // Resolve finished bold lookups before checking for cached glyphs.
            self.bold_text_style.ensure_fonts_loaded(cx);
        }
        // The atlas epoch moves when glyph slots are evicted or the atlas
        // starts over: cached slots are void then.
        // So does a font family taking a face it lacked (a lazy CJK or emoji
        // face arriving): the missing-glyph boxes cached before are stale.
        let epoch = cx.fonts.borrow().rasterizer().borrow().epoch();
        let families = cx.fonts.borrow().font_family_generation();
        let key = (
            font_size.to_bits(),
            cx.current_dpi_factor().to_bits(),
            self.raster_scale().to_bits(),
            epoch,
            families,
        );
        if key != self.glyph_cache_key {
            self.glyph_cache.clear();
            self.run_cache.clear();
            self.glyph_cache_key = key;
        }
        if let Some(run) = metrics {
            let g = &run.glyphs[0];
            self.cell_w = g.advance_in_lpxs as f64;
            let glyph_h = (run.ascender_in_lpxs - run.descender_in_lpxs) as f64;
            let (scale, pixels) = self.cell_height_adjustment;
            let height = glyph_h * self.draw_text.text_style.line_spacing as f64;
            self.cell_h = (height * scale + pixels / cx.current_dpi_factor().max(1.0)).max(1.0);
            self.cell_baseline = (self.cell_h - glyph_h) * 0.5 + run.ascender_in_lpxs as f64;
        }
        if self.cell_w <= 0.0 {
            self.cell_w = font_size * 0.6;
        }
        if self.cell_h <= 0.0 {
            self.cell_h = font_size * 1.35;
        }
    }

    fn grid_size(&self) -> (usize, usize) {
        let padding = self.inner_padding();
        let cols = ((self.rect.size.x - padding.x * 2.0) / self.cell_w)
            .floor()
            .max(2.0) as usize;
        let rows = ((self.rect.size.y - padding.y * 2.0) / self.cell_h)
            .floor()
            .max(2.0) as usize;
        (cols, rows)
    }

    fn raster_scale(&self) -> f64 {
        self.canvas_ime_anchor
            .map(|(_, transform)| transform.scale)
            .filter(|scale| scale.is_finite())
            .unwrap_or(1.0)
            .clamp(0.1, 8.0)
    }

    fn cached_glyph(
        &mut self,
        cx: &mut Cx2d,
        ch: char,
        bold: bool,
        columns: u8,
    ) -> Option<CachedGlyph> {
        if let Some(hit) = self.glyph_cache.get(&(ch, bold, columns)) {
            return *hit;
        }
        let mut buf = [0u8; 4];
        let text: &str = ch.encode_utf8(&mut buf);
        // Rasterize at the actual screen size, then retain local-grid metrics.
        // Canvas zoom otherwise scales a low-resolution glyph cached at 1x.
        let scale = self.raster_scale() as f32;
        let style = self.draw_text.text_style.clone();
        if bold {
            self.draw_text.text_style.font_family = self.bold_text_style.font_family.clone();
        }
        self.draw_text.text_style.font_size *= scale;
        let prepared = self.draw_text.prepare_single_line_run(cx, text);
        self.draw_text.text_style = style;
        let cached = prepared.and_then(|run| {
            run.glyphs.first().map(|g| {
                // Proportional fallback symbols must stay inside the cells
                // allocated by the terminal, without shrinking normal mono
                // glyphs for small rounding differences in the grid advance.
                let available = self.cell_w as f32 * columns.max(1) as f32;
                let advance = run.width_in_lpxs / scale;
                let fit = if advance > available * 1.05 {
                    available / advance
                } else {
                    1.0
                };
                let font_size = g.font_size_in_lpxs / scale;
                let center_y = g.glyph.center_y_in_ems() * font_size;
                CachedGlyph {
                    glyph: g.glyph,
                    font_size_in_lpxs: font_size * fit,
                    x_offset_in_lpxs: (g.pen_x_in_lpxs + g.offset_x_in_lpxs) / scale * fit,
                    y_offset_in_lpxs: center_y * (1.0 - fit),
                }
            })
        });
        // A miss or a glyph still on its way from an asynchronous rasterizer
        // (an unresolved stand-in) is not kept: the rasterizer redraws
        // everything when the glyph lands, and the next draw caches it.
        let resolved = cached.is_some_and(|g| !g.glyph.is_pending());
        if resolved {
            self.glyph_cache.insert((ch, bold, columns), cached);
        }
        cached
    }

    fn cached_run(&mut self, cx: &mut Cx2d, text: &str, bold: bool) -> Option<Vec<CachedGlyph>> {
        let key = (text.to_string(), bold);
        if let Some(hit) = self.run_cache.get(&key) {
            return hit.clone();
        }
        let scale = self.raster_scale() as f32;
        let style = self.draw_text.text_style.clone();
        if bold {
            self.draw_text.text_style.font_family = self.bold_text_style.font_family.clone();
        }
        self.draw_text.text_style.font_size *= scale;
        let prepared = self.draw_text.prepare_single_line_run(cx, text);
        self.draw_text.text_style = style;
        let cached = prepared.map(|run| {
            run.glyphs
                .into_iter()
                .map(|g| CachedGlyph {
                    glyph: g.glyph,
                    font_size_in_lpxs: g.font_size_in_lpxs / scale,
                    x_offset_in_lpxs: (g.pen_x_in_lpxs + g.offset_x_in_lpxs) / scale,
                    y_offset_in_lpxs: g.offset_y_in_lpxs / scale,
                })
                .collect::<Vec<_>>()
        });
        let resolved = cached
            .as_ref()
            .is_some_and(|gs| !gs.is_empty() && gs.iter().all(|g| !g.glyph.is_pending()));
        if resolved {
            if self.run_cache.len() >= 4096 {
                self.run_cache.clear();
            }
            self.run_cache.insert(key, cached.clone());
        }
        cached
    }

    /// Cell geometry, rather than a font's side bearings/line gap, joins
    /// common TUI borders. Thin strokes occupy whole device pixels; rounded
    /// corners remain curves, and ANSI faint is applied once to the result.
    fn draw_box_glyph(&mut self, cx: &mut Cx2d, ch: char, x: f64, y: f64, color: Vec4f) -> bool {
        let (arms, heavy, rounded) = match ch {
            '─' => (3, false, false),
            '━' => (3, true, false),
            '│' => (12, false, false),
            '┃' => (12, true, false),
            '┌' => (10, false, false),
            '┏' => (10, true, false),
            '╭' => (10, false, true),
            '┐' => (9, false, false),
            '┓' => (9, true, false),
            '╮' => (9, false, true),
            '└' => (6, false, false),
            '┗' => (6, true, false),
            '╰' => (6, false, true),
            '┘' => (5, false, false),
            '┛' => (5, true, false),
            '╯' => (5, false, true),
            '├' => (14, false, false),
            '┣' => (14, true, false),
            '┤' => (13, false, false),
            '┫' => (13, true, false),
            '┬' => (11, false, false),
            '┳' => (11, true, false),
            '┴' => (7, false, false),
            '┻' => (7, true, false),
            '┼' => (15, false, false),
            '╋' => (15, true, false),
            _ => return false,
        };
        let scale = self.raster_scale();
        let dpi = cx.current_dpi_factor().max(0.1);
        let physical = dpi * scale;
        let pixels = ((self.font_size * self.presentation_font_scale * 0.07 * physical)
            .round()
            .max(1.0))
            * if heavy { 2.0 } else { 1.0 };
        let thickness = pixels / physical;
        let (mx, my) = (x + self.cell_w * 0.5, y + self.cell_h * 0.5);
        let translation = self
            .canvas_ime_anchor
            .map(|(_, transform)| transform.translation)
            .unwrap_or_default();
        let snap = |value: f64, translation: f64| {
            (((value * scale + translation) * dpi - pixels * 0.5).round() + pixels * 0.5) / physical
                - translation / scale
        };
        let (mx, my) = (snap(mx, translation.x), snap(my, translation.y));
        self.draw_boxes.color = color;
        self.draw_boxes.arms = vec4(
            (arms & 1 != 0) as u8 as f32,
            (arms & 2 != 0) as u8 as f32,
            (arms & 4 != 0) as u8 as f32,
            (arms & 8 != 0) as u8 as f32,
        );
        self.draw_boxes.center = vec2((mx - x) as f32, (my - y) as f32);
        self.draw_boxes.corner = vec2(
            if arms & 2 != 0 { 1.0 } else { -1.0 },
            if arms & 8 != 0 { 1.0 } else { -1.0 },
        );
        self.draw_boxes.thickness = thickness as f32;
        self.draw_boxes.radius = if rounded {
            (self.cell_w.min(self.cell_h) * 0.35).max(thickness) as f32
        } else {
            0.0
        };
        self.draw_boxes.pixel_size = (1.0 / physical) as f32;
        self.draw_boxes.draw_abs(
            cx,
            Rect {
                pos: dvec2(x, y),
                size: dvec2(self.cell_w, self.cell_h),
            },
        );
        true
    }

    /// Braille is a fixed two-by-four dot grid, also used by terminal plots
    /// and animated logos. Render all 256 patterns without font fallback.
    fn draw_braille_glyph(&mut self, ch: char, x: f64, y: f64, color: Vec4f) -> bool {
        let Some(pattern) = (ch as u32).checked_sub(0x2800).filter(|v| *v <= 0xff) else {
            return false;
        };
        if pattern == 0 {
            return true;
        }
        self.draw_dots.clear();
        self.draw_dots.set_color(color.x, color.y, color.z, color.w);
        let radius = (self.cell_w * 0.13).min(self.cell_h * 0.065) as f32;
        // Unicode dot numbering: 1,2,3,7 down the left; 4,5,6,8 right.
        for (bit, col, row) in [
            (0, 0, 0),
            (1, 0, 1),
            (2, 0, 2),
            (6, 0, 3),
            (3, 1, 0),
            (4, 1, 1),
            (5, 1, 2),
            (7, 1, 3),
        ] {
            if pattern & (1 << bit) != 0 {
                self.draw_dots.circle(
                    (x + self.cell_w * (0.25 + col as f64 * 0.5)) as f32,
                    (y + self.cell_h * (0.125 + row as f64 * 0.25)) as f32,
                    radius,
                );
            }
        }
        self.draw_dots.fill();
        true
    }

    fn rgb_to_vec4(rgb: Rgb, alpha: f32) -> Vec4f {
        vec4(
            rgb.r as f32 / 255.0,
            rgb.g as f32 / 255.0,
            rgb.b as f32 / 255.0,
            alpha,
        )
    }

    /// Resolve a cell's fg/bg honoring inverse (cell + global DECSCNM),
    /// bold-brightening for the 8 base palette colors, faint and invisible.
    fn resolve_colors(
        session: &Session,
        style: &crate::term::style::Style,
        global_inverse: bool,
    ) -> (Option<Vec4f>, Option<Vec4f>) {
        let term = &session.terminal;
        let mut fg_color = style.fg_color;
        // Classic bold-brightens: palette 0-7 + bold renders 8-15.
        if style.flags.has(StyleFlags::BOLD) {
            if let StyleColor::Palette(i) = fg_color {
                if i < 8 {
                    fg_color = StyleColor::Palette(i + 8);
                }
            }
        }
        let mut fg = fg_color.resolve(&term.palette, term.default_fg);
        let mut bg = style.bg_color.resolve_opt(&term.palette);

        let inverse = style.flags.has(StyleFlags::INVERSE) != global_inverse;
        if inverse {
            let old_fg = fg;
            fg = bg.unwrap_or(term.default_bg);
            bg = Some(old_fg);
        }

        if style.flags.has(StyleFlags::INVISIBLE) {
            return (None, bg.map(|b| Self::rgb_to_vec4(b, 1.0)));
        }
        let alpha = if style.flags.has(StyleFlags::FAINT) {
            0.55
        } else {
            1.0
        };
        (
            Some(Self::rgb_to_vec4(fg, alpha)),
            bg.map(|b| Self::rgb_to_vec4(b, 1.0)),
        )
    }

    // --------------------------------------------------------------
    // Selection
    // --------------------------------------------------------------

    fn sel_ordered(&self) -> Option<((u64, usize), (u64, usize))> {
        let a = self.sel_anchor?;
        let c = self.sel_cursor?;
        if a == c {
            return None;
        }
        Some(if a <= c { (a, c) } else { (c, a) })
    }

    fn cell_selected(&self, abs_row: u64, col: usize) -> bool {
        let Some(((sr, sc), (er, ec))) = self.sel_ordered() else {
            return false;
        };
        if abs_row < sr || abs_row > er {
            return false;
        }
        if sr == er {
            return col >= sc && col < ec;
        }
        if abs_row == sr {
            return col >= sc;
        }
        if abs_row == er {
            return col < ec;
        }
        true
    }

    fn selected_text(&self) -> Option<String> {
        let session = self.session.as_ref()?;
        let screen = session.terminal.screen();
        let ((sr, sc), (er, ec)) = self.sel_ordered()?;
        let mut out = String::new();
        for abs in sr..=er {
            let Some(virt) = screen.virtual_of_absolute(abs) else {
                continue;
            };
            let Some(row) = screen.row_virtual(virt) else {
                continue;
            };
            let from = if abs == sr { sc } else { 0 };
            let to = if abs == er { ec } else { screen.cols };
            let mut line = String::new();
            for col in from..to.min(screen.cols) {
                if let Some(cell) = row.cell(col) {
                    cell.content.push_text(&mut line);
                } else {
                    line.push(' ');
                }
            }
            let line = line.trim_end();
            out.push_str(line);
            if abs < er {
                // A soft-wrapped row continues logically: no newline.
                if !row.wrapped {
                    out.push('\n');
                }
            }
        }
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }

    /// Abs row + col at a window position.
    fn pick(&self, abs_pos: Vec2d) -> Option<(u64, usize)> {
        let session = self.session.as_ref()?;
        let screen = session.terminal.screen();
        let padding = self.inner_padding();
        let local_x = abs_pos.x - self.rect.pos.x - padding.x;
        let local_y = abs_pos.y - self.rect.pos.y - padding.y;
        let col = (local_x / self.cell_w).floor().max(0.0) as usize;
        let col = col.min(screen.cols.saturating_sub(1));
        let visual_row = (local_y / self.cell_h).floor().max(0.0) as usize;
        let top_virtual = self.top_virtual_row();
        let virt = (top_virtual + visual_row).min(screen.total_rows().saturating_sub(1));
        Some((screen.absolute_of_virtual(virt), col))
    }

    /// First visible virtual row for the current view offset.
    fn top_virtual_row(&self) -> usize {
        let Some(session) = self.session.as_ref() else {
            return 0;
        };
        let screen = session.terminal.screen();
        screen.scrollback.len().saturating_sub(self.view_offset)
    }

    // --------------------------------------------------------------
    // Input
    // --------------------------------------------------------------

    fn key_opts(&self) -> KeyEncodeOptions {
        let Some(session) = self.session.as_ref() else {
            return KeyEncodeOptions::default();
        };
        let term = &session.terminal;
        KeyEncodeOptions {
            cursor_key_application: term.modes.get(Mode::CursorKeys),
            keypad_key_application: term.modes.get(Mode::KeypadKeys),
            ignore_keypad_with_numlock: term.modes.get(Mode::IgnoreKeypadWithNumlock),
            alt_esc_prefix: term.modes.get(Mode::AltEscPrefix),
            modify_other_keys_state_2: term.modify_other_keys == 2,
            kitty_flags: KittyFlags(term.kitty_flags()),
            backarrow_key_mode: term.modes.get(Mode::BackarrowKeyMode),
        }
    }

    fn mods_of(m: &KeyModifiers) -> KeyMods {
        KeyMods {
            shift: m.shift,
            ctrl: m.control,
            alt: m.alt,
            super_: m.logo,
            caps_lock: false,
            num_lock: false,
        }
    }

    fn send_key(
        &mut self,
        cx: &mut Cx,
        key: Key,
        mods: &KeyModifiers,
        action: KeyAction,
        utf8: &str,
        unshifted: u32,
    ) {
        let opts = self.key_opts();
        let event = TermKeyEvent {
            action,
            key,
            mods: Self::mods_of(mods),
            consumed_mods: KeyMods::default(),
            utf8: utf8.to_string(),
            unshifted_codepoint: unshifted,
        };
        let bytes = encode_key(&event, &opts);
        if !bytes.is_empty() {
            self.scroll_to_bottom();
            if let Some(session) = self.session.as_mut() {
                session.write(&bytes);
            }
            self.redraw(cx);
        }
    }

    fn paste(&mut self, cx: &mut Cx, text: &str) {
        if self.paste_bytes(text) {
            self.redraw(cx);
        }
    }

    fn paste_bytes(&mut self, text: &str) -> bool {
        self.paste_bytes_with_suffix(text, &[])
    }

    fn paste_bytes_with_suffix(&mut self, text: &str, suffix: &[u8]) -> bool {
        let Some(bracketed) = self
            .session
            .as_ref()
            .filter(|session| !session.exited)
            .map(|s| s.terminal.modes.get(Mode::BracketedPaste))
        else {
            return false;
        };
        let mut bytes = Vec::with_capacity(text.len() + 16);
        if bracketed {
            bytes.extend_from_slice(b"\x1b[200~");
            // Sanitize: no escape bytes inside a bracketed paste body.
            bytes.extend(text.bytes().filter(|b| *b != 0x1b));
            bytes.extend_from_slice(b"\x1b[201~");
        } else {
            // Legacy paste: newlines become CR like every terminal.
            for b in text.bytes() {
                bytes.push(if b == b'\n' { b'\r' } else { b });
            }
        }
        bytes.extend_from_slice(suffix);
        self.scroll_to_bottom();
        self.session
            .as_mut()
            .is_some_and(|session| session.try_write(&bytes))
    }

    fn scroll_to_bottom(&mut self) {
        self.view_offset = 0;
    }

    fn mouse_tracking(&self) -> (MouseTracking, MouseFormat) {
        let Some(session) = self.session.as_ref() else {
            return (MouseTracking::None, MouseFormat::X10);
        };
        let m = &session.terminal.modes;
        let tracking = if m.get(Mode::MouseEventAny) {
            MouseTracking::Any
        } else if m.get(Mode::MouseEventButton) {
            MouseTracking::Button
        } else if m.get(Mode::MouseEventNormal) {
            MouseTracking::Normal
        } else if m.get(Mode::MouseEventX10) {
            MouseTracking::X10
        } else {
            return (MouseTracking::None, MouseFormat::X10);
        };
        let format = if m.get(Mode::MouseFormatSgrPixels) {
            MouseFormat::SgrPixels
        } else if m.get(Mode::MouseFormatSgr) {
            MouseFormat::Sgr
        } else if m.get(Mode::MouseFormatUrxvt) {
            MouseFormat::Urxvt
        } else if m.get(Mode::MouseFormatUtf8) {
            MouseFormat::Utf8
        } else {
            MouseFormat::X10
        };
        (tracking, format)
    }

    fn mouse_cell(&self, abs: Vec2d) -> (u32, u32, u32, u32) {
        let padding = self.inner_padding();
        let (cols, rows) = self.session.as_ref().map(|session| {
            let screen = session.terminal.screen();
            (screen.cols, screen.rows)
        }).unwrap_or_else(|| self.grid_size());
        let x = (abs.x - self.rect.pos.x - padding.x)
            .clamp(0.0, (cols as f64 * self.cell_w - 1.0).max(0.0));
        let y = (abs.y - self.rect.pos.y - padding.y)
            .clamp(0.0, (rows as f64 * self.cell_h - 1.0).max(0.0));
        let col = ((x / self.cell_w).floor() as u32).min(cols.saturating_sub(1) as u32);
        let row = ((y / self.cell_h).floor() as u32).min(rows.saturating_sub(1) as u32);
        (col, row, x as u32, y as u32)
    }

    /// Send a mouse report if the app asked for it; true when consumed.
    fn report_mouse(
        &mut self,
        cx: &mut Cx,
        abs: Vec2d,
        kind: MouseEventKind,
        button: TermMouseButton,
        mods: &KeyModifiers,
    ) -> bool {
        let (tracking, format) = self.mouse_tracking();
        if tracking == MouseTracking::None || mods.shift {
            return false;
        }
        let (col, row, x_px, y_px) = self.mouse_cell(abs);
        // Motion dedup: only report when the cell (or button) changed.
        if kind == MouseEventKind::Motion {
            let sig = (col, row, button as u8);
            if self.last_mouse_cell == Some(sig) {
                return true;
            }
            self.last_mouse_cell = Some(sig);
        } else {
            self.last_mouse_cell = None;
        }
        let report = MouseReport {
            kind,
            button,
            mods: Self::mods_of(mods),
            col,
            row,
            x_px,
            y_px,
        };
        if let Some(bytes) = encode_mouse(&report, tracking, format) {
            if let Some(session) = self.session.as_mut() {
                session.write(&bytes);
                self.redraw(cx);
            }
        }
        true
    }

    fn handle_scroll(&mut self, cx: &mut Cx, e: &FingerScrollEvent) {
        let Some((alt_scroll, max)) = self.session.as_ref().map(|session| {
            let term = &session.terminal;
            (
                matches!(term.active, crate::term::terminal::ActiveScreen::Alternate)
                    && term.modes.get(Mode::MouseAlternateScroll),
                term.screen().scrollback.len(),
            )
        }) else {
            return;
        };
        let lines = if e.device.is_mouse() {
            (e.scroll.y / 40.0).abs().ceil().max(1.0) as usize
        } else {
            ((e.scroll.y.abs() / self.cell_h).ceil()).max(1.0) as usize
        };
        let down = e.scroll.y > 0.0;

        let (tracking, _) = self.mouse_tracking();
        if tracking != MouseTracking::None && !e.modifiers.shift {
            let button = if down {
                TermMouseButton::WheelDown
            } else {
                TermMouseButton::WheelUp
            };
            for _ in 0..lines.min(8) {
                self.report_mouse(cx, e.abs, MouseEventKind::Press, button, &e.modifiers);
            }
            return;
        }

        if alt_scroll {
            // Alternate scroll: wheel becomes arrow keys.
            let key = if down { Key::ArrowDown } else { Key::ArrowUp };
            for _ in 0..lines.min(8) {
                self.send_key(cx, key, &KeyModifiers::default(), KeyAction::Press, "", 0);
            }
            return;
        }

        // Scrollback.
        if down {
            self.view_offset = self.view_offset.saturating_sub(lines);
        } else {
            self.view_offset = (self.view_offset + lines).min(max);
        }
        self.redraw(cx);
    }

    fn map_keycode(kc: KeyCode) -> Option<Key> {
        Some(match kc {
            KeyCode::ReturnKey => Key::Enter,
            KeyCode::NumpadEnter => Key::NumpadEnter,
            KeyCode::Tab => Key::Tab,
            KeyCode::Backspace => Key::Backspace,
            KeyCode::Escape => Key::Escape,
            KeyCode::Delete => Key::Delete,
            KeyCode::Insert => Key::Insert,
            KeyCode::Home => Key::Home,
            KeyCode::End => Key::End,
            KeyCode::PageUp => Key::PageUp,
            KeyCode::PageDown => Key::PageDown,
            KeyCode::ArrowUp => Key::ArrowUp,
            KeyCode::ArrowDown => Key::ArrowDown,
            KeyCode::ArrowLeft => Key::ArrowLeft,
            KeyCode::ArrowRight => Key::ArrowRight,
            KeyCode::F1 => Key::F1,
            KeyCode::F2 => Key::F2,
            KeyCode::F3 => Key::F3,
            KeyCode::F4 => Key::F4,
            KeyCode::F5 => Key::F5,
            KeyCode::F6 => Key::F6,
            KeyCode::F7 => Key::F7,
            KeyCode::F8 => Key::F8,
            KeyCode::F9 => Key::F9,
            KeyCode::F10 => Key::F10,
            KeyCode::F11 => Key::F11,
            KeyCode::F12 => Key::F12,
            _ => return None,
        })
    }

    fn is_special(kc: KeyCode) -> bool {
        Self::map_keycode(kc).is_some()
    }

    // --------------------------------------------------------------
    // Drawing
    // --------------------------------------------------------------

    fn draw_terminal_background(&mut self, cx: &mut Cx2d, bg: Rgb, inverse: bool) {
        let alpha = self.color_overrides.background_opacity.unwrap_or_else(|| {
            if self.opaque_style { 1.0 }
            else if cx.has_key_focus(self.area) { self.bg_opacity.0 }
            else { self.bg_opacity.1 }
        });
        let mut v = Self::rgb_to_vec4(bg, alpha);
        if !inverse {
            let scale = 1.0 - self.background_dimming;
            v.x *= scale;
            v.y *= scale;
            v.z *= scale;
        }
        self.draw_bg.draw_vars.set_uniform(cx, id!(color), &[v.x, v.y, v.z, v.w]);
        let (radius, width, color) = self.presentation_frame.map(|frame| {
            // The stroke extends both ways from its inset. Keep the inside
            // edge clear of the grid even at the smallest canvas zoom.
            (frame.radius, frame.border_width.min(frame.padding as f32 * 0.5), frame.border_color)
        }).unwrap_or((0.0, 0.0, vec4(0.0, 0.0, 0.0, 0.0)));
        self.draw_bg.draw_vars.set_uniform(cx, id!(corner_radius), &[radius]);
        self.draw_bg.draw_vars.set_uniform(cx, id!(frame_width), &[width]);
        self.draw_bg.draw_vars.set_uniform(cx, id!(frame_color), &[color.x, color.y, color.z, color.w]);
        self.draw_bg.draw_abs(cx, self.rect);
    }

    fn draw_terminal(&mut self, cx: &mut Cx2d) {
        // The session moves out of self for the draw so cached-glyph and
        // selection helpers can borrow self freely.
        let Some(mut session) = self.session.take() else {
            let bg = self.color_overrides.background
                .or_else(|| self.style_colors.map(|(_, bg)| bg))
                .or_else(|| self.original_colors.map(|(_, _, bg)| bg))
                .unwrap_or_else(|| default_theme().2);
            self.draw_terminal_background(cx, bg, false);
            return;
        };
        self.draw_terminal_inner(cx, &mut session);
        self.session = Some(session);
    }

    fn draw_terminal_inner(&mut self, cx: &mut Cx2d, session: &mut Session) {
        session.terminal.dirty = false;

        let has_focus = cx.has_key_focus(self.area);
        let padding = self.inner_padding();
        let origin_x = self.rect.pos.x + padding.x;
        let origin_y = self.rect.pos.y + padding.y;
        let (cell_w, cell_h) = (self.cell_w, self.cell_h);
        let global_inverse = session.terminal.modes.get(Mode::ReverseColors);
        let cursor_visible = session.terminal.modes.get(Mode::CursorVisible);
        let (default_bg, default_fg, cursor_color, cursor_style) = {
            let t = &session.terminal;
            (
                t.default_bg,
                t.default_fg,
                t.cursor_color.or(t.base_cursor).unwrap_or(t.default_fg),
                t.cursor_style,
            )
        };

        // Background fill honoring DECSCNM.
        let bg_fill = if global_inverse {
            default_fg
        } else {
            default_bg
        };
        self.draw_terminal_background(cx, bg_fill, global_inverse);

        let screen = session.terminal.screen();
        let rows = screen.rows;
        let cols = screen.cols;
        let top_virtual = screen.scrollback.len().saturating_sub(self.view_offset);
        let total = screen.total_rows();

        // Collect draw data first (bg runs, glyphs, decorations), then issue
        // batched draws per layer.
        struct BgRun {
            x: f64,
            y: f64,
            w: f64,
            color: Vec4f,
        }
        struct GlyphDraw {
            x: f64,
            y: f64,
            ch: char,
            color: Vec4f,
            bold: bool,
            columns: u8,
        }
        struct DecoDraw {
            x: f64,
            y: f64,
            w: f64,
            color: Vec4f,
            kind: f32,
            strike: bool,
        }
        let mut bg_runs: Vec<BgRun> = Vec::new();
        let mut glyphs: Vec<GlyphDraw> = Vec::with_capacity(rows * cols / 2);
        let mut decos: Vec<DecoDraw> = Vec::new();

        for vis_row in 0..rows {
            let virt = top_virtual + vis_row;
            if virt >= total {
                break;
            }
            let abs = screen.absolute_of_virtual(virt);
            let row = match screen.row_virtual(virt) {
                Some(r) => r,
                None => continue,
            };
            let y = origin_y + vis_row as f64 * cell_h;
            let mut run: Option<(usize, usize, Vec4f)> = None;
            for col in 0..cols {
                let cell = row.cell(col);
                let (fg, bg) = match cell {
                    Some(c) => Self::resolve_colors(session, &c.style, global_inverse),
                    None => (None, None),
                };
                let selected = self.cell_selected(abs, col);
                let bg = if selected { Some(SELECTION_COLOR) } else { bg };

                // Merge bg runs.
                match (&mut run, bg) {
                    (Some((_, end, color)), Some(c)) if *color == c && *end == col => {
                        *end = col + 1;
                    }
                    (prev, next) => {
                        if let Some((start, end, color)) = prev.take() {
                            bg_runs.push(BgRun {
                                x: origin_x + start as f64 * cell_w,
                                y,
                                w: (end - start) as f64 * cell_w,
                                color,
                            });
                        }
                        if let Some(c) = next {
                            *prev = Some((col, col + 1, c));
                        }
                    }
                }

                let Some(cell) = cell else { continue };
                let Some(fg) = fg else { continue };

                // Decorations.
                let underline = cell.style.flags.underline();
                let strike = cell.style.flags.has(StyleFlags::STRIKETHROUGH);
                if underline != crate::term::style::Underline::None || strike {
                    let ul_color = cell
                        .style
                        .underline_color
                        .resolve_opt(&session.terminal.palette)
                        .map(|c| Self::rgb_to_vec4(c, 1.0))
                        .unwrap_or(fg);
                    let width = cell.content.width().max(1) as f64 * cell_w;
                    if underline != crate::term::style::Underline::None {
                        decos.push(DecoDraw {
                            x: origin_x + col as f64 * cell_w,
                            y,
                            w: width,
                            color: ul_color,
                            kind: underline as u8 as f32,
                            strike: false,
                        });
                    }
                    if strike {
                        decos.push(DecoDraw {
                            x: origin_x + col as f64 * cell_w,
                            y,
                            w: width,
                            color: fg,
                            kind: 1.0,
                            strike: true,
                        });
                    }
                }

                // Text.
                match &cell.content {
                    CellContent::Char(c) | CellContent::WideChar(c) => {
                        if *c != ' ' {
                            glyphs.push(GlyphDraw {
                                x: origin_x + col as f64 * cell_w,
                                y,
                                ch: *c,
                                color: fg,
                                bold: cell.style.flags.has(StyleFlags::BOLD),
                                columns: cell.content.width(),
                            });
                        }
                    }
                    CellContent::Cluster(cluster) => {
                        // First codepoint via the cache; combining marks are
                        // drawn over it.
                        for c in &cluster.cps {
                            glyphs.push(GlyphDraw {
                                x: origin_x + col as f64 * cell_w,
                                y,
                                ch: *c,
                                color: fg,
                                bold: cell.style.flags.has(StyleFlags::BOLD),
                                columns: cell.content.width(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            if let Some((start, end, color)) = run.take() {
                bg_runs.push(BgRun {
                    x: origin_x + start as f64 * cell_w,
                    y,
                    w: (end - start) as f64 * cell_w,
                    color,
                });
            }
        }

        // Cursor (only when the live bottom is in view).
        let cursor = if self.view_offset == 0 && cursor_visible && !session.exited {
            let s = session.terminal.screen();
            Some((s.cursor.x.min(cols - 1), s.cursor.y))
        } else {
            None
        };
        let cursor_span = cursor.map(|(col, row)| {
            let (head, width) = screen.row(row).cell_span(col);
            (col, row, head, width)
        });

        // Layer 1: backgrounds.
        self.draw_cell_bg.new_draw_call(cx);
        for r in &bg_runs {
            self.draw_cell_bg.color = r.color;
            self.draw_cell_bg.draw_abs(
                cx,
                Rect {
                    pos: dvec2(r.x, r.y),
                    size: dvec2(r.w, cell_h),
                },
            );
        }

        // Layer 2: cursor under text (block) — text stays readable on top.
        if let Some((cx_col, cx_row, head, width)) = cursor_span {
            let x = origin_x + cx_col as f64 * cell_w;
            let y = origin_y + cx_row as f64 * cell_h;
            let span_x = origin_x + head as f64 * cell_w;
            let span_width = f64::from(width) * cell_w;
            let color = Self::rgb_to_vec4(cursor_color, 1.0);
            self.draw_cursor.new_draw_call(cx);
            self.draw_cursor.color = color;
            self.draw_cursor.hollow = if has_focus { 0.0 } else { 1.0 };
            let (rect, hollow_override) = match cursor_style {
                CursorStyle::BlinkingBar | CursorStyle::SteadyBar => (
                    Rect {
                        pos: dvec2(x, y),
                        size: dvec2((cell_w * 0.15).max(1.5), cell_h),
                    },
                    Some(0.0),
                ),
                CursorStyle::BlinkingUnderline | CursorStyle::SteadyUnderline => (
                    Rect {
                        pos: dvec2(span_x, y + cell_h - (cell_h * 0.12).max(2.0)),
                        size: dvec2(span_width, (cell_h * 0.12).max(2.0)),
                    },
                    Some(0.0),
                ),
                _ => (
                    Rect {
                        pos: dvec2(span_x, y),
                        size: dvec2(span_width, cell_h),
                    },
                    None,
                ),
            };
            if let Some(h) = hollow_override {
                if !has_focus {
                    // Non-block cursors just dim when unfocused.
                    self.draw_cursor.color = vec4(color.x, color.y, color.z, 0.5);
                }
                self.draw_cursor.hollow = h;
            }
            self.draw_cursor.draw_abs(cx, rect);
        }

        // Layer 3: glyphs, one batch.
        self.draw_boxes.new_draw_call(cx);
        self.draw_dots.begin();
        self.draw_text.new_draw_call(cx);
        self.draw_text.begin_many_instances(cx);
        self.draw_text.glyph_depth = self.draw_text.draw_depth;
        let baseline = self.cell_baseline;
        let effective_color = |g: &GlyphDraw| -> Vec4f {
            let mut color = g.color;
            if let Some((_, crow, head, _)) = cursor_span {
                let gx = ((g.x - origin_x) / cell_w).round() as usize;
                let gy = ((g.y - origin_y) / cell_h).round() as usize;
                if gx == head
                    && gy == crow
                    && has_focus
                    && matches!(
                        cursor_style,
                        CursorStyle::Default
                            | CursorStyle::BlinkingBlock
                            | CursorStyle::SteadyBlock
                    )
                {
                    color = Self::rgb_to_vec4(default_bg, 1.0);
                }
            }
            color
        };

        let mut i = 0;
        while i < glyphs.len() {
            let g = &glyphs[i];
            let color = effective_color(g);
            if self.ligatures && g.columns == 1 && g.ch.is_ascii_graphic() {
                let mut end = i + 1;
                let mut run_text = String::new();
                run_text.push(g.ch);
                while end < glyphs.len() {
                    let prev = &glyphs[end - 1];
                    let next = &glyphs[end];
                    if next.columns != 1
                        || !next.ch.is_ascii_graphic()
                        || next.bold != g.bold
                        || (next.y - g.y).abs() > 0.5
                        || ((next.x - prev.x) - cell_w).abs() > 0.5
                        || effective_color(next) != color
                    {
                        break;
                    }
                    run_text.push(next.ch);
                    end += 1;
                }
                if end - i >= 2 {
                    if let Some(shaped) = self.cached_run(cx, &run_text, g.bold) {
                        for sg in shaped {
                            let point = Point::new(
                                (g.x + sg.x_offset_in_lpxs as f64) as f32,
                                (g.y + baseline) as f32 + sg.y_offset_in_lpxs,
                            );
                            self.draw_text.draw_prepared_glyph_abs(
                                cx,
                                point,
                                sg.font_size_in_lpxs,
                                sg.glyph,
                                color,
                            );
                        }
                        i = end;
                        continue;
                    }
                }
            }

            if self.draw_box_glyph(cx, g.ch, g.x, g.y, color) {
                i += 1;
                continue;
            }
            if self.draw_braille_glyph(g.ch, g.x, g.y, color) {
                i += 1;
                continue;
            }
            if let Some(glyph) = self.cached_glyph(cx, g.ch, g.bold, g.columns) {
                let point = Point::new(
                    (g.x + glyph.x_offset_in_lpxs as f64) as f32,
                    (g.y + baseline) as f32 + glyph.y_offset_in_lpxs,
                );
                self.draw_text.draw_prepared_glyph_abs(
                    cx,
                    point,
                    glyph.font_size_in_lpxs,
                    glyph.glyph,
                    color,
                );
            }
            i += 1;
        }
        self.draw_text.end_many_instances(cx);
        self.draw_dots.end(cx);

        // Layer 4: decorations.
        if !decos.is_empty() {
            self.draw_underline.new_draw_call(cx);
            for d in &decos {
                self.draw_underline.color = d.color;
                self.draw_underline.kind = d.kind;
                let (dy, dh) = if d.strike {
                    (d.y + cell_h * 0.5 - 1.0, 1.5)
                } else if d.kind >= 2.5 && d.kind < 3.5 {
                    // Curly gets a taller band.
                    (d.y + cell_h - 4.0, 4.0)
                } else if d.kind >= 1.5 && d.kind < 2.5 {
                    (d.y + cell_h - 4.0, 4.0)
                } else {
                    (d.y + cell_h - 2.0, 1.5)
                };
                self.draw_underline.draw_abs(
                    cx,
                    Rect {
                        pos: dvec2(d.x, dy),
                        size: dvec2(d.w, dh),
                    },
                );
            }
        }

        // Inline IME preedit preview at the terminal cursor.
        if !self.ime_preedit.is_empty() && has_focus {
            if let Some((cx_col, cx_row)) = cursor {
                let x = origin_x + cx_col as f64 * cell_w;
                let y = origin_y + cx_row as f64 * cell_h;
                let cols: usize = self
                    .ime_preedit
                    .chars()
                    .map(|c| makepad_cell_width::char_width(c as u32).max(1) as usize)
                    .sum();
                let w = (cols as f64 * cell_w).max(cell_w);
                self.draw_cell_bg.new_draw_call(cx);
                self.draw_cell_bg.color = vec4(0.18, 0.24, 0.38, 1.0);
                self.draw_cell_bg.draw_abs(
                    cx,
                    Rect {
                        pos: dvec2(x, y),
                        size: dvec2(w, cell_h),
                    },
                );
                self.draw_underline.new_draw_call(cx);
                self.draw_underline.color = vec4(0.48, 0.64, 0.97, 1.0);
                self.draw_underline.kind = 1.0;
                self.draw_underline.draw_abs(
                    cx,
                    Rect {
                        pos: dvec2(x, y + cell_h - 2.0),
                        size: dvec2(w, 2.0),
                    },
                );
                self.draw_text.new_draw_call(cx);
                self.draw_text.color = vec4(0.95, 0.97, 1.0, 1.0);
                self.draw_text.draw_abs(cx, dvec2(x, y), &self.ime_preedit);
            }
        }

        // Bell flash.
        if self.bell_frames > 0 {
            self.bell_frames -= 1;
            self.draw_cell_bg.new_draw_call(cx);
            self.draw_cell_bg.color = vec4(1.0, 1.0, 1.0, 0.06 * self.bell_frames as f32);
            self.draw_cell_bg.draw_abs(cx, self.rect);
            self.draw_bg.redraw(cx);
        }

        // Scrollback position indicator.
        if self.view_offset > 0 {
            let sb = session.terminal.screen().scrollback.len().max(1);
            let frac = self.view_offset as f64 / sb as f64;
            let h = (self.rect.size.y * 0.2).max(24.0);
            let track = self.rect.size.y - h;
            let y = self.rect.pos.y + track * (1.0 - frac);
            self.draw_cell_bg.new_draw_call(cx);
            self.draw_cell_bg.color = vec4(1.0, 1.0, 1.0, 0.25);
            self.draw_cell_bg.draw_abs(
                cx,
                Rect {
                    pos: dvec2(self.rect.pos.x + self.rect.size.x - 4.0, y),
                    size: dvec2(3.0, h),
                },
            );
        }

        // Exited banner.
        if session.exited {
            self.draw_cell_bg.new_draw_call(cx);
            self.draw_cell_bg.color = vec4(0.0, 0.0, 0.0, 0.5);
            self.draw_cell_bg.draw_abs(cx, self.rect);
        }
    }

    fn pump_session(&mut self, cx: &mut Cx) {
        let mut actions: Vec<MpTermAction> = Vec::new();
        let mut needs_redraw = false;
        if let Some(session) = self.session.as_mut() {
            if session.drain() {
                needs_redraw = true;
            }
            // Inside wm, shell facts go to the compositor through the WM
            // API so it can title the tile and open new terminals in our
            // cwd (the Omarchy behavior).
            for event in session.take_events() {
                match event {
                    TermEvent::TitleChanged(title) => {
                        // Hosted in wm: the bar shows it (no-op standalone).
                        makepad_wm_api::set_title(cx, &title);
                        actions.push(MpTermAction::TitleChanged(title))
                    }
                    TermEvent::Bell => {
                        self.bell_frames = 6;
                        actions.push(MpTermAction::Bell);
                        needs_redraw = true;
                    }
                    TermEvent::ClipboardSet { text, .. } => {
                        cx.copy_to_clipboard(&text);
                    }
                    TermEvent::PwdChanged(url) => {
                        let Some(path) = osc7_path(&url) else {
                            continue;
                        };
                        self.cwd = Some(PathBuf::from(&path));
                        // Hosted: new terminals open here (Omarchy's
                        // terminal-in-cwd); no-op standalone.
                        makepad_wm_api::set_cwd(cx, Path::new(&path));
                        actions.push(MpTermAction::PwdChanged(path));
                    }
                    TermEvent::Notification { title, body } => {
                        actions.push(MpTermAction::Notification { title, body });
                    }
                }
            }
            if session.exited {
                actions.push(MpTermAction::Exited);
            }
        }
        for action in actions {
            cx.widget_action(self.uid, action);
        }
        if needs_redraw {
            self.draw_bg.redraw(cx);
        }
    }
}

fn osc7_path(value: &str) -> Option<String> {
    let Some(uri) = value.strip_prefix("file://") else {
        return Some(value.to_owned());
    };
    let path = &uri[uri.find('/')?..];
    #[cfg(windows)]
    let path = if matches!(path.as_bytes(), [b'/', b'a'..=b'z' | b'A'..=b'Z', b':', b'/', ..]) {
        &path[1..]
    } else {
        path
    };
    let mut bytes = path.bytes();
    let mut decoded = Vec::with_capacity(path.len());
    while let Some(byte) = bytes.next() {
        decoded.push(if byte == b'%' {
            let high = (bytes.next()? as char).to_digit(16)?;
            let low = (bytes.next()? as char).to_digit(16)?;
            (high * 16 + low) as u8
        } else {
            byte
        });
    }
    String::from_utf8(decoded).ok()
}

impl Widget for MpTerm {
    fn draw_walk(&mut self, cx: &mut Cx2d, _scope: &mut Scope, walk: Walk) -> DrawStep {
        cx.begin_turtle(walk, self.layout);
        self.rect = cx.turtle().rect();
        self.refresh_metrics(cx);
        self.ensure_session(cx);

        let (cols, rows) = self.grid_size();
        if let Some(session) = self.session.as_mut() {
            session.resize(cols, rows);
            session.drain();
        }

        self.draw_terminal(cx);

        cx.end_turtle_with_area(&mut self.area);
        // A terminal OWNS the keyboard from its first frame — standalone
        // and hosted alike. Without this a hosted tile that was never
        // clicked silently dropped every forwarded key (the WM decides
        // which tile receives keys; inside the child this widget must
        // hold its own key focus for them to land).
        if !self.took_focus {
            self.took_focus = true;
            // Canvas hosts decide focus from actual input. Redrawing their
            // areas must still let the framework migrate an existing focus.
            if self.focus_on_first_draw && self.canvas_ime_anchor.is_none() && !self.read_only {
                cx.set_key_focus(self.area);
            }
        }
        if self.session.is_some() && cx.has_key_focus(self.area) && !self.read_only {
            let s = self
                .session
                .as_ref()
                .map(|s| {
                    let sc = s.terminal.screen();
                    (sc.cursor.x, sc.cursor.y)
                })
                .unwrap_or((0, 0));
            let ime = self.inner_padding() + dvec2(
                s.0 as f64 * self.cell_w,
                (s.1 + 1) as f64 * self.cell_h,
            );
            if let Some((anchor, transform)) = self.canvas_ime_anchor {
                let screen = (self.rect.pos + ime) * transform.scale
                    + transform.translation;
                let cursor = screen - anchor.rect(cx).pos;
                cx.show_text_ime(anchor, cursor);
            } else {
                cx.show_text_ime(self.area, ime);
            }
        }
        DrawStep::done()
    }

    fn handle_event(&mut self, cx: &mut Cx, event: &Event, _scope: &mut Scope) {
        if matches!(event, Event::Drag(_) | Event::Drop(_)) && !self.read_only {
            match event.drag_hits(cx, self.area) {
                DragHit::Drag(drag) => {
                    let accepts = self.session.as_ref().is_some_and(|session| !session.exited)
                        && drag.items.iter().any(|item| matches!(item, DragItem::FilePath {path,..} if Self::valid_drop_path(path)));
                    if let Ok(mut response) = drag.response.try_lock() {
                        *response = if accepts {
                            DragResponse::Copy
                        } else {
                            DragResponse::None
                        };
                    }
                }
                DragHit::Drop(drop) => {
                    if let Some(path) = drop.items.iter().find_map(|item| match item {
                        DragItem::FilePath { path, .. } if Self::valid_drop_path(path) => {
                            Some(path)
                        }
                        _ => None,
                    }) {
                        if self.ai_drop_file(Path::new(path)) {
                            cx.widget_action(
                                self.uid,
                                MpTermAction::FileDropped {
                                    path: PathBuf::from(path),
                                },
                            );
                            self.redraw(cx);
                        }
                    }
                }
                _ => {}
            }
        }
        if let Event::Signal = event {
            // No check_and_clear here: the event loop already consumed the
            // global flag to dispatch this event, so a second check would
            // only steal a signal raised since — including the one our own
            // budget-capped drain re-arms to continue a flood next tick.
            self.pump_session(cx);
        }

        // Edge auto-scroll while selecting.
        if self.selecting && self.select_scroll_frame.is_event(event).is_some() {
            self.select_scroll_frame = cx.new_next_frame();
            if let Some(abs) = self.last_finger {
                let band = self.cell_h.max(6.0);
                let top = self.rect.pos.y + band;
                let bottom = self.rect.pos.y + self.rect.size.y - band;
                let max = self
                    .session
                    .as_ref()
                    .map(|s| s.terminal.screen().scrollback.len())
                    .unwrap_or(0);
                if abs.y < top {
                    self.view_offset = (self.view_offset + 1).min(max);
                } else if abs.y > bottom {
                    self.view_offset = self.view_offset.saturating_sub(1);
                }
                if let Some(pos) = self.pick(abs) {
                    self.sel_cursor = Some(pos);
                }
                self.draw_bg.redraw(cx);
            }
        }

        match event.hits(cx, self.area) {
            Hit::FingerDown(e) => {
                cx.set_key_focus(self.area);
                if self.report_mouse(
                    cx,
                    e.abs,
                    MouseEventKind::Press,
                    TermMouseButton::Left,
                    &e.modifiers,
                ) {
                    return;
                }
                if let Some(pos) = self.pick(e.abs) {
                    if e.tap_count >= 3 {
                        // Line selection.
                        self.sel_anchor = Some((pos.0, 0));
                        self.sel_cursor = Some((
                            pos.0,
                            self.session
                                .as_ref()
                                .map(|s| s.terminal.cols())
                                .unwrap_or(80),
                        ));
                        self.selecting = false;
                    } else if e.tap_count == 2 {
                        let (start, end) = self.word_range(pos);
                        self.sel_anchor = Some((pos.0, start));
                        self.sel_cursor = Some((pos.0, end));
                        self.selecting = false;
                    } else {
                        self.sel_anchor = Some(pos);
                        self.sel_cursor = Some(pos);
                        self.selecting = true;
                        self.last_finger = Some(e.abs);
                        self.select_scroll_frame = cx.new_next_frame();
                    }
                    self.draw_bg.redraw(cx);
                }
            }
            Hit::FingerMove(e) => {
                if self.report_mouse(
                    cx,
                    e.abs,
                    MouseEventKind::Motion,
                    TermMouseButton::Left,
                    &e.modifiers,
                ) {
                    return;
                }
                if self.selecting {
                    if let Some(pos) = self.pick(e.abs) {
                        self.sel_cursor = Some(pos);
                    }
                    self.last_finger = Some(e.abs);
                    self.draw_bg.redraw(cx);
                }
            }
            Hit::FingerUp(e) => {
                let consumed = self.report_mouse(
                    cx,
                    e.abs,
                    MouseEventKind::Release,
                    TermMouseButton::Left,
                    &e.modifiers,
                );
                if !consumed
                    && self.copy_on_select
                    && self.sel_anchor.is_some()
                    && self.sel_anchor != self.sel_cursor
                {
                    if let Some(text) = self.selected_text() {
                        cx.copy_to_clipboard(&text);
                    }
                }
                self.selecting = false;
                self.last_finger = None;
            }
            Hit::FingerHoverIn(_) | Hit::FingerHoverOver(_) => {
                cx.set_cursor(MouseCursor::Text);
            }
            Hit::FingerScroll(e) => {
                self.handle_scroll(cx, &e);
            }
            Hit::KeyFocus(_) => {
                if let Some(session) = self.session.as_mut() {
                    if session.terminal.modes.get(Mode::FocusEvent) {
                        session.write(b"\x1b[I");
                    }
                }
                self.draw_bg.redraw(cx);
            }
            Hit::KeyFocusLost(_) => {
                self.ime_preedit.clear();
                if let Some(session) = self.session.as_mut() {
                    if session.terminal.modes.get(Mode::FocusEvent) {
                        session.write(b"\x1b[O");
                    }
                }
                cx.hide_text_ime();
                self.draw_bg.redraw(cx);
            }
            Hit::KeyDown(_) | Hit::TextInput(_) if self.read_only => {}
            Hit::KeyDown(e) => {
                if self.session.is_some()
                    && e.key_code == KeyCode::ReturnKey
                    && !e.is_repeat
                    && !e.modifiers.shift
                    && !e.modifiers.control
                    && !e.modifiers.logo
                    && !e.modifiers.alt
                {
                    cx.widget_action(self.uid, MpTermAction::PromptSubmitted);
                }
                // Clear selection on typing.
                if Self::is_special(e.key_code) {
                    self.sel_anchor = None;
                    self.sel_cursor = None;
                    let key = Self::map_keycode(e.key_code).unwrap();
                    let action = if e.is_repeat {
                        KeyAction::Repeat
                    } else {
                        KeyAction::Press
                    };
                    self.send_key(cx, key, &e.modifiers, action, "", 0);
                } else if e.modifiers.control && !e.modifiers.logo {
                    if let Some(ch) = e.key_code.to_char(e.modifiers.shift) {
                        let key = letter_key(ch).unwrap_or(Key::Unidentified);
                        let action = if e.is_repeat {
                            KeyAction::Repeat
                        } else {
                            KeyAction::Press
                        };
                        self.send_key(
                            cx,
                            key,
                            &e.modifiers,
                            action,
                            "",
                            ch.to_ascii_lowercase() as u32,
                        );
                    }
                }
            }
            Hit::TextInput(e) => {
                if e.replace_last {
                    self.ime_preedit = e.input.clone();
                    self.redraw(cx);
                    return;
                }
                if !self.ime_preedit.is_empty() {
                    self.ime_preedit.clear();
                    self.redraw(cx);
                }
                if e.was_paste {
                    self.paste(cx, &e.input);
                } else {
                    let filtered: String = e
                        .input
                        .chars()
                        .filter(|c| *c != '\n' && *c != '\r')
                        .collect();
                    if !filtered.is_empty() {
                        self.sel_anchor = None;
                        self.sel_cursor = None;
                        self.scroll_to_bottom();
                        if let Some(session) = self.session.as_mut() {
                            session.write(filtered.as_bytes());
                        }
                        self.redraw(cx);
                    }
                }
            }
            Hit::TextCopy(e) => {
                if let Some(text) = self.selected_text() {
                    *e.response.borrow_mut() = Some(text);
                }
            }
            _ => {}
        }
    }
}

impl MpTerm {
    fn word_range(&self, pos: (u64, usize)) -> (usize, usize) {
        let Some(session) = self.session.as_ref() else {
            return (pos.1, pos.1 + 1);
        };
        let screen = session.terminal.screen();
        let Some(virt) = screen.virtual_of_absolute(pos.0) else {
            return (pos.1, pos.1 + 1);
        };
        let Some(row) = screen.row_virtual(virt) else {
            return (pos.1, pos.1 + 1);
        };
        let kind_of = |col: usize| -> Option<bool> {
            let c = row.cell(col).and_then(|c| c.content.primary())?;
            if c.is_whitespace() {
                None
            } else {
                Some(c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/')
            }
        };
        let Some(kind) = kind_of(pos.1) else {
            return (pos.1, pos.1 + 1);
        };
        let mut start = pos.1;
        while start > 0 && kind_of(start - 1) == Some(kind) {
            start -= 1;
        }
        let mut end = pos.1 + 1;
        while end < screen.cols && kind_of(end) == Some(kind) {
            end += 1;
        }
        (start, end)
    }

    fn redraw(&mut self, cx: &mut Cx) {
        self.draw_bg.redraw(cx);
    }
}

fn system_font_family<'a>(
    vm: &mut ScriptVm,
    style: ScriptObject,
    names: impl Iterator<Item = &'a String>,
    weight: f32,
) -> FontFamily {
    let family = vm.new_object();
    for name in names {
        let member = vm.new_object();
        let system = vm.new_string(name);
        vm.set_value_def(member, id!(system), system);
        vm.set_value_def(member, id!(weight), weight);
        vm.vec_push(family, NIL, member);
    }
    if let Some(fallback) = vm.value(style, id!(font_family)).as_object() {
        vm.heap_mut().vec_push_vec(family, fallback, NoTrap);
    }
    FontFamily::script_from_value(vm, family.into())
}

impl TerminalColors {
    fn apply(
        &self,
        terminal: &mut Terminal,
        (mut base16, fg, bg): ([Rgb; 16], Rgb, Rgb),
        style: Option<(Rgb, Rgb)>,
    ) {
        let (fg, bg) = style.unwrap_or((fg, bg));
        if style.is_some() {
            base16[0] = bg;
            base16[7] = fg;
        }
        let mut palette = default_palette();
        palette[..16].copy_from_slice(&base16);
        for &(index, color) in &self.palette {
            palette[usize::from(index)] = color;
        }
        if let Some(cursor) = self.cursor {
            terminal.base_cursor = Some(cursor);
            terminal.cursor_color = Some(cursor);
        }
        terminal.set_palette_theme(palette, self.foreground.unwrap_or(fg), self.background.unwrap_or(bg));
    }
}

fn parse_hex_rgb(s: &str) -> Option<Rgb> {
    let s = s.trim().trim_start_matches('#');
    if s.len() != 6 {
        return None;
    }
    Some(Rgb::new(
        u8::from_str_radix(&s[0..2], 16).ok()?,
        u8::from_str_radix(&s[2..4], 16).ok()?,
        u8::from_str_radix(&s[4..6], 16).ok()?,
    ))
}

fn parse_key_chord(chord: &str) -> Result<(Key, KeyMods, String, u32), String> {
    let invalid = || format!("Invalid terminal key chord: {chord}");
    let mut parts = chord.rsplit('+');
    let name = parts.next().unwrap_or_default().to_ascii_lowercase();
    let mut mods = KeyMods::default();
    for part in parts {
        let flag = match part.to_ascii_lowercase().as_str() {
            "ctrl" => &mut mods.ctrl,
            "alt" => &mut mods.alt,
            "shift" => &mut mods.shift,
            "meta" | "cmd" => &mut mods.super_,
            _ => return Err(invalid()),
        };
        if *flag {
            return Err(invalid());
        }
        *flag = true;
    }
    let key = match name.as_str() {
        "enter" => Key::Enter,
        "escape" => Key::Escape,
        "tab" => Key::Tab,
        "backspace" => Key::Backspace,
        "delete" => Key::Delete,
        "insert" => Key::Insert,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "arrowup" => Key::ArrowUp,
        "arrowdown" => Key::ArrowDown,
        "arrowleft" => Key::ArrowLeft,
        "arrowright" => Key::ArrowRight,
        _ => {
            let ch = match name.as_str() {
                "space" => ' ',
                "plus" => '+',
                _ if name.len() == 1 => name.as_bytes()[0] as char,
                _ => return Err(invalid()),
            };
            if let Some(key) = letter_key(ch) {
                let text = if mods.shift { ch.to_ascii_uppercase() } else { ch };
                return Ok((key, mods, text.to_string(), ch as u32));
            }
            const SYMBOLS: [(Key, char, char); 22] = [
                (Key::Digit0, '0', ')'), (Key::Digit1, '1', '!'), (Key::Digit2, '2', '@'),
                (Key::Digit3, '3', '#'), (Key::Digit4, '4', '$'), (Key::Digit5, '5', '%'),
                (Key::Digit6, '6', '^'), (Key::Digit7, '7', '&'), (Key::Digit8, '8', '*'),
                (Key::Digit9, '9', '('), (Key::Minus, '-', '_'), (Key::Equal, '=', '+'),
                (Key::BracketLeft, '[', '{'), (Key::BracketRight, ']', '}'),
                (Key::Backslash, '\\', '|'), (Key::Semicolon, ';', ':'), (Key::Quote, '\'', '"'),
                (Key::Backquote, '`', '~'), (Key::Comma, ',', '<'), (Key::Period, '.', '>'),
                (Key::Slash, '/', '?'), (Key::Space, ' ', ' '),
            ];
            let (key, base, shifted) = SYMBOLS.into_iter()
                .find(|&(_, base, shifted)| ch == base || ch == shifted)
                .ok_or_else(invalid)?;
            mods.shift |= base != shifted && ch == shifted;
            let text = if mods.shift { shifted } else { base };
            return Ok((key, mods, text.to_string(), base as u32));
        }
    };
    Ok((key, mods, String::new(), 0))
}

fn letter_key(ch: char) -> Option<Key> {
    Some(match ch.to_ascii_lowercase() {
        'a' => Key::KeyA,
        'b' => Key::KeyB,
        'c' => Key::KeyC,
        'd' => Key::KeyD,
        'e' => Key::KeyE,
        'f' => Key::KeyF,
        'g' => Key::KeyG,
        'h' => Key::KeyH,
        'i' => Key::KeyI,
        'j' => Key::KeyJ,
        'k' => Key::KeyK,
        'l' => Key::KeyL,
        'm' => Key::KeyM,
        'n' => Key::KeyN,
        'o' => Key::KeyO,
        'p' => Key::KeyP,
        'q' => Key::KeyQ,
        'r' => Key::KeyR,
        's' => Key::KeyS,
        't' => Key::KeyT,
        'u' => Key::KeyU,
        'v' => Key::KeyV,
        'w' => Key::KeyW,
        'x' => Key::KeyX,
        'y' => Key::KeyY,
        'z' => Key::KeyZ,
        ' ' => Key::Space,
        '[' => Key::BracketLeft,
        ']' => Key::BracketRight,
        '\\' => Key::Backslash,
        '/' => Key::Slash,
        '-' => Key::Minus,
        '=' => Key::Equal,
        ';' => Key::Semicolon,
        '\'' => Key::Quote,
        ',' => Key::Comma,
        '.' => Key::Period,
        '`' => Key::Backquote,
        '0'..='9' => match ch {
            '0' => Key::Digit0,
            '1' => Key::Digit1,
            '2' => Key::Digit2,
            '3' => Key::Digit3,
            '4' => Key::Digit4,
            '5' => Key::Digit5,
            '6' => Key::Digit6,
            '7' => Key::Digit7,
            '8' => Key::Digit8,
            _ => Key::Digit9,
        },
        _ => return None,
    })
}
