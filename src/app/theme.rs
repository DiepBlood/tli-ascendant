//! Colours and shared small widgets of the Ascendant UI (layout B, modern dashboard): the colour
//! swatches, [`apply`] which applies them to the egui dark theme, the card frames, bars, verdict tags,
//! switch and key-cap tag. The game-related rarity colours are not here; the original ones in ui.rs and
//! vorax_view.rs are kept (to match the game).

use crate::lang::{tr, trf};
use eframe::egui::{self, Color32, CornerRadius, Frame, Margin, Pos2, Rect, Response, Role, RichText, Sense, Stroke, StrokeKind, TextStyle, TextWrapMode, Ui, Vec2, Widget, WidgetInfo, WidgetText};
use std::time::{Duration, Instant};

/// The smallest size of self-drawn text (FontId::proportional) (font spec point 2): cell numbers, badges, the auto
/// button, etc.
pub const MIN_TEXT: f32 = 11.0;
/// The large text of the advice card (shared by Tartarus and Vorax).
pub const ADVICE_TEXT: f32 = 17.0;

/// Window background.
pub const BG: Color32 = Color32::from_rgb(0x16, 0x18, 0x1d);
/// Left navigation bar.
pub const SIDEBAR: Color32 = Color32::from_rgb(0x1d, 0x20, 0x27);
/// Advice card.
pub const CARD: Color32 = Color32::from_rgb(0x1f, 0x2a, 0x3d);
/// Ordinary group card (settings page, run page), a little lighter than the background.
pub const PANEL: Color32 = Color32::from_rgb(0x1c, 0x1f, 0x26);
/// Primary colour: the selected tab, the advised option and cell.
pub const ACCENT: Color32 = Color32::from_rgb(0x4f, 0x8c, 0xff);
/// Secondary text (2026-11-05 font spec: brightened from #8a90a0, because the contrast of help text was too low to
/// read).
pub const MUTED: Color32 = Color32::from_rgb(0xaa, 0xb1, 0xc2);
/// Ordinary text (font spec: main text is about #f0f2f6).
pub const TEXT: Color32 = Color32::from_rgb(0xf0, 0xf2, 0xf6);
/// Bar that is not selected.
pub const BAR_IDLE: Color32 = Color32::from_rgb(0x3a, 0x45, 0x58);
/// "Also good" bar: a grey-blue a little lighter than the unselected one.
pub const BAR_NEAR: Color32 = Color32::from_rgb(0x5b, 0x72, 0x9a);
/// The track under a bar.
pub const TRACK: Color32 = Color32::from_rgb(0x26, 0x2a, 0x33);
/// Dividers and outlines.
pub const DIVIDER: Color32 = Color32::from_rgb(0x2c, 0x30, 0x3a);
/// Background and text of the "clearly better" tag.
pub const CLEAR_BG: Color32 = Color32::from_rgb(0x25, 0x41, 0x2e);
pub const CLEAR_FG: Color32 = Color32::from_rgb(0x9f, 0xe0, 0xad);
/// Background and text of the "either is fine" tag.
pub const EITHER_BG: Color32 = Color32::from_rgb(0x3d, 0x35, 0x20);
pub const EITHER_FG: Color32 = Color32::from_rgb(0xf0, 0xd2, 0x8a);
/// Warning text (doubts about a read, an unrecognised instrument).
pub const WARN: Color32 = Color32::from_rgb(0xf0, 0xd2, 0x8a);
/// Error text.
pub const ERROR: Color32 = Color32::from_rgb(0xff, 0x8a, 0x80);
/// Healthy (all health checks pass).
pub const GOOD: Color32 = Color32::from_rgb(0x9f, 0xe0, 0xad);
/// Gold for the icon (ring, four-point star) and the night-blue background.
pub const GOLD: Color32 = Color32::from_rgb(0xe8, 0xc4, 0x6a);
pub const STAR: Color32 = Color32::from_rgb(0xf3, 0xd9, 0x8e);
pub const NIGHT: Color32 = Color32::from_rgb(0x0f, 0x14, 0x24);

/// Apply Ascendant's colours to the dark theme (there is only the dark theme, see `AscendantApp::new`).
pub fn apply(ctx: &egui::Context) {
    ctx.style_mut_of(egui::Theme::Dark, |style| {
        let v = &mut style.visuals;
        v.panel_fill = BG;
        v.window_fill = SIDEBAR;
        v.window_stroke = Stroke::new(1.0, DIVIDER);
        v.window_corner_radius = CornerRadius::same(8);
        v.menu_corner_radius = CornerRadius::same(6);
        v.extreme_bg_color = Color32::from_rgb(0x12, 0x14, 0x18);
        v.faint_bg_color = Color32::from_rgb(0x1b, 0x1e, 0x24);
        v.code_bg_color = PANEL;
        v.hyperlink_color = ACCENT;
        v.warn_fg_color = WARN;
        v.error_fg_color = ERROR;
        v.selection.bg_fill = ACCENT.gamma_multiply(0.55);
        v.selection.stroke = Stroke::new(1.0, Color32::WHITE);
        let w = &mut v.widgets;
        w.noninteractive.bg_fill = BG;
        w.noninteractive.weak_bg_fill = BG;
        w.noninteractive.bg_stroke = Stroke::new(1.0, DIVIDER);
        w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        for (visuals, fill, edge) in [
            (&mut w.inactive, Color32::from_rgb(0x27, 0x2b, 0x34), DIVIDER),
            (&mut w.hovered, Color32::from_rgb(0x30, 0x36, 0x42), BAR_NEAR),
            (&mut w.active, Color32::from_rgb(0x34, 0x3c, 0x4c), ACCENT),
            (&mut w.open, Color32::from_rgb(0x27, 0x2b, 0x34), ACCENT),
        ] {
            visuals.bg_fill = fill;
            visuals.weak_bg_fill = fill;
            visuals.bg_stroke = Stroke::new(1.0, edge);
            visuals.corner_radius = CornerRadius::same(5);
        }
        w.inactive.fg_stroke = Stroke::new(1.0, TEXT);
        w.hovered.fg_stroke = Stroke::new(1.5, Color32::WHITE);
        w.active.fg_stroke = Stroke::new(2.0, Color32::WHITE);
    });
}

/// The frame of the advice card: a dark blue background in the primary colour family, rounded corners.
pub fn card_frame() -> Frame {
    Frame::new().fill(CARD).corner_radius(8).inner_margin(Margin::symmetric(8, 6))
}

/// The frame of an ordinary group card (settings page, run page).
pub fn group_frame() -> Frame {
    Frame::new().fill(PANEL).stroke(Stroke::new(1.0, DIVIDER)).corner_radius(8).inner_margin(Margin::symmetric(8, 6))
}

/// Small heading of a group: small text in the secondary text colour.
pub fn heading(ui: &mut Ui, text: &str) {
    ui.add_space(4.0);
    ui.label(RichText::new(text).small().strong().color(MUTED));
}

/// Small help text (licence notices etc.), wrapped by word: each English name (with the whitespace or
/// punctuation after it) is one unbreakable chunk, and if the line cannot hold it the whole chunk moves
/// to the next line (it used to use break_anywhere, and a narrow window would split `DinoRaos` into
/// `DinoR / aos`). Each Chinese character is its own chunk; a chunk wraps inside itself only when it is
/// wider than the whole line.
pub fn fine_print(ui: &mut Ui, text: &str, colour: Color32) -> Response {
    let font = TextStyle::Small.resolve(ui.style());
    let width = ui.available_width().max(40.0);
    let row_h = ui.text_style_height(&TextStyle::Small);
    // Lay out each chunk's position first, then allocate the whole extent at once, so the Response returned covers
    // the whole text.
    let mut placed: Vec<(Vec2, std::sync::Arc<egui::Galley>)> = Vec::new();
    let (mut x, mut y, mut right) = (0.0f32, 0.0f32, 0.0f32);
    for word in words(text) {
        let mut job = egui::text::LayoutJob::simple(word.to_owned(), font.clone(), colour, width);
        job.wrap.break_anywhere = true;
        let galley = ui.painter().layout_job(job);
        let size = galley.size();
        // Wrap if the line cannot hold it (trailing whitespace does not count toward the width); an over-wide chunk
        // that does not fit at the line start is placed anyway and wraps itself.
        let ink = ui.painter().layout_no_wrap(word.trim_end().to_owned(), font.clone(), colour).size().x;
        if x > 0.0 && x + ink > width + 0.5 {
            x = 0.0;
            y += row_h;
        }
        placed.push((Vec2::new(x, y), galley));
        x += size.x;
        right = right.max(x.min(width));
        if size.y > row_h + 0.5 {
            // An over-wide chunk wrapped: the next chunk starts after its last line.
            y += size.y - row_h;
        }
    }
    let (rect, response) = ui.allocate_exact_size(Vec2::new(right, y + row_h), Sense::hover());
    for (at, galley) in placed {
        ui.painter().galley(rect.min + at, galley, colour);
    }
    response
}

/// The wrap units of [`fine_print`]: a run of ASCII letters, digits and `-_./@` together with the
/// whitespace and half-width punctuation right after it is one chunk; other characters (Chinese,
/// full-width punctuation) are one chunk per character, but closing full-width punctuation such as
/// `、，）` sticks to the end of the previous chunk, and `（` sticks to the front of the next chunk, so
/// that none is left alone at a line start or end.
fn words(text: &str) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    let word_char = |c: char| c.is_ascii_alphanumeric() || "-_./@".contains(c);
    let closing = |c: char| "、，。）：；」』！？,;:)".contains(c) || c == ' ';
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let mut end = i + c.len_utf8();
        if word_char(c) {
            while let Some(&(j, d)) = chars.peek() {
                if !word_char(d) {
                    break;
                }
                end = j + d.len_utf8();
                chars.next();
            }
        }
        // Opening punctuation (`（`, `(`) goes together with the next character.
        if "（(「『".contains(c) {
            continue;
        }
        while let Some(&(j, d)) = chars.peek() {
            if !closing(d) {
                break;
            }
            end = j + d.len_utf8();
            chars.next();
        }
        out.push(&text[start..end]);
        start = end;
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// One bar: the track plus a fill of length `share` (0 to 1), width `width`, height 6.
pub fn bar(ui: &mut Ui, width: f32, share: f32, colour: Color32) -> Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 6.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 3.0, TRACK);
    let fill = Rect::from_min_size(rect.min, Vec2::new((width * share.clamp(0.0, 1.0)).max(3.0), 6.0));
    p.rect_filled(fill, 3.0, colour);
    response
}

/// The verdict of the advice against the runner-up, drawn as a tag at the right of the advice card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The gap exceeds the simulation error: following the advice is clearly better.
    Clear,
    /// The gap is within the error: either choice is fine.
    Either,
}

impl Verdict {
    pub fn text(self) -> &'static str {
        match self {
            Verdict::Clear => tr("Clearly better"),
            Verdict::Either => tr("Either is fine"),
        }
    }

    fn colours(self) -> (Color32, Color32) {
        match self {
            Verdict::Clear => (CLEAR_BG, CLEAR_FG),
            Verdict::Either => (EITHER_BG, EITHER_FG),
        }
    }
}

/// The width taken by the advice card's frame and inner margin (8 points of margin on each side, 1 point of frame):
/// usable width inside the card = usable width outside - this.
pub const CARD_INSET: f32 = 18.0;

/// The width available to the large text in a one-line advice card: `room` is the card's inner width
/// minus the tag, `small_w` is the width of the small text laid out on one line (0 when there is no
/// small text). The small text wrapped to two lines needs about half the width (at most 40%), the
/// large text takes the rest, at least 60%; with no small text the large text takes all of `room`.
/// Shared by [`advice_line`] and Vorax's shortening of option names, so both sides compute the same width.
pub fn big_room(room: f32, small_w: f32, spacing: f32) -> f32 {
    if small_w <= 0.0 {
        return room;
    }
    (room - (small_w / 2.0 + 12.0).min(room * 0.4) - spacing).max(room * 0.6)
}

/// The width taken by the tag (text width, left and right margin, and spacing); 0 when there is no tag.
pub fn tag_width(ui: &Ui, verdict: Option<Verdict>) -> f32 {
    verdict.map_or(0.0, |v| WidgetText::from(RichText::new(v.text()).small()).into_galley(ui, None, f32::INFINITY, TextStyle::Small).size().x + 10.0 + ui.spacing().item_spacing.x)
}

/// A one-line advice card (compact, shared by Tartarus and Vorax): large text on the left, then small text two lines
/// high (auto-wrapped), and the verdict tag on the right. The width is split by content: the small text wrapped to
/// two lines needs about half the width (at most 40%), the large text takes the rest (at least 60%); text is
/// truncated only when neither fits. The height is fixed at one line of large text. The background and frame are
/// supplied by the caller (Vorax greys out an old advice and keeps the frame thickness unchanged, so the height stays
/// the same). Returns the card's extent; the caller attaches the tooltip to it. Returns the card's extent and whether
/// any text was truncated (when truncated, the caller makes the card clickable to show the full text).
pub fn advice_line(ui: &mut Ui, big: &str, colour: Color32, small: &str, small_colour: Color32, verdict: Option<Verdict>, (fill, edge): (Color32, Color32)) -> (Rect, bool) {
    let mut cut = false;
    let rect = card_frame()
        .fill(fill)
        .stroke(Stroke::new(1.0, edge))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let font = egui::FontId::proportional(ADVICE_TEXT);
            let row_h = ui.painter().layout_no_wrap("\u{570b}".to_string(), font.clone(), colour).size().y;
            // The small-text area is fixed at two lines high (auto-wrapped), so this row is at least two small lines
            // high.
            let small_h = 2.0 * ui.text_style_height(&TextStyle::Small) + 2.0;
            let line_h = row_h.max(small_h);
            ui.horizontal(|ui| {
                ui.set_min_height(line_h);
                let spacing = ui.spacing().item_spacing.x;
                let tag_w = tag_width(ui, verdict);
                let room = (ui.available_width() - tag_w).max(20.0);
                let small_w = if small.is_empty() { 0.0 } else { ui.painter().layout_no_wrap(small.to_owned(), TextStyle::Small.resolve(ui.style()), colour).size().x };
                let mut job = egui::text::LayoutJob::simple(big.to_owned(), font, colour, big_room(room, small_w, spacing));
                job.wrap.max_rows = 1;
                job.wrap.break_anywhere = true;
                let galley = ui.painter().layout_job(job);
                cut |= galley.elided;
                let (rect, _) = ui.allocate_exact_size(Vec2::new(galley.size().x, line_h), Sense::hover());
                ui.painter().galley(Pos2::new(rect.left(), rect.center().y - galley.size().y / 2.0), galley, colour);
                let rest = (ui.available_width() - tag_w).max(0.0);
                ui.allocate_ui_with_layout(Vec2::new(rest, line_h), egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    ui.set_min_width(rest);
                    cut |= !small.is_empty() && elided(ui, small, rest, 2);
                    lines(ui, small, small_colour, 2);
                });
                if let Some(v) = verdict {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        tag(ui, v);
                    });
                }
            });
        })
        .response
        .rect;
    (rect, cut)
}

/// Verdict tag: a small rounded box with small text. Display only; a Label is selectable by default
/// (it takes clicks), so selection is turned off so that it does not block the game under click-through.
pub fn tag(ui: &mut Ui, verdict: Verdict) -> Response {
    let (bg, fg) = verdict.colours();
    Frame::new()
        .fill(bg)
        .corner_radius(4)
        .inner_margin(Margin::symmetric(5, 1))
        .show(ui, |ui| ui.add(egui::Label::new(RichText::new(verdict.text()).small().color(fg)).selectable(false)))
        .response
}

/// Small rounded tag (reroll count, held instruments). The text does not wrap; inside a
/// `horizontal_wrapped`, when the width left on this line cannot hold it, it moves to the next line
/// first (a Frame does not wrap by itself, and the text would be squeezed to one character per line).
pub fn chip(ui: &mut Ui, text: RichText) -> Response {
    let width = WidgetText::from(text.clone()).into_galley(ui, Some(TextWrapMode::Extend), f32::INFINITY, TextStyle::Small).size().x + 12.0;
    let at_start = ui.cursor().min.x <= ui.max_rect().min.x + 1.0;
    // In a wrapping layout available_width is the whole line width; available_size_before_wrap gives what is left on
    // this line.
    if width + ui.spacing().item_spacing.x > ui.available_size_before_wrap().x && !at_start {
        ui.end_row();
    }
    Frame::new()
        .fill(TRACK)
        .stroke(Stroke::new(1.0, DIVIDER))
        .corner_radius(4)
        .inner_margin(Margin::symmetric(5, 1))
        // Keep it selectable (it takes the mouse): when held instruments do not fit and collapse to `+N`, under
        // click-through this is what lets the mouse rest on it to see the full tooltip.
        .show(ui, |ui| ui.add(egui::Label::new(text).wrap_mode(TextWrapMode::Truncate)))
        .response
}

/// Key-cap tag: monospace text, a rounded outline, and a thickened bottom edge like a keycap. Display only.
pub fn keycap(ui: &mut Ui, text: &str) -> Response {
    let text = if text.trim().is_empty() { "—" } else { text.trim() };
    let galley = WidgetText::from(RichText::new(text).small().monospace().color(TEXT)).into_galley(ui, Some(TextWrapMode::Truncate), ui.available_width().max(24.0) - 10.0, TextStyle::Small);
    let size = galley.size() + Vec2::new(10.0, 5.0);
    let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 4.0, Color32::from_rgb(0x2a, 0x2f, 0x3a));
    p.rect_stroke(rect, 4.0, Stroke::new(1.0, BAR_IDLE), StrokeKind::Inside);
    p.line_segment([rect.left_bottom() + Vec2::new(3.0, -1.0), rect.right_bottom() + Vec2::new(-3.0, -1.0)], Stroke::new(1.5, Color32::from_rgb(0x12, 0x14, 0x18)));
    p.galley(rect.min + Vec2::new(5.0, 2.0), galley, TEXT);
    response
}

/// Switch: text on the left, a 28x16 capsule on the right. The whole row is clickable (Sense::click);
/// a click toggles it and marks changed, so it can be used with `changed()` like the original
/// checkbox; `add_enabled` and `on_disabled_hover_text` are also supported.
pub struct Switch<'a> {
    on: &'a mut bool,
    text: WidgetText,
}

impl<'a> Switch<'a> {
    pub fn new(on: &'a mut bool, text: impl Into<WidgetText>) -> Switch<'a> {
        Switch { on, text: text.into() }
    }
}

impl Widget for Switch<'_> {
    fn ui(self, ui: &mut Ui) -> Response {
        const CAPSULE: Vec2 = Vec2::new(28.0, 16.0);
        let width = ui.available_width().max(CAPSULE.x + 20.0);
        let galley = self.text.into_galley(ui, Some(TextWrapMode::Truncate), width - CAPSULE.x - 8.0, TextStyle::Body);
        let height = ui.spacing().interact_size.y.max(galley.size().y);
        let (rect, mut response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
        if response.clicked() {
            *self.on = !*self.on;
            response.mark_changed();
        }
        let on = *self.on;
        response.widget_info(|| WidgetInfo::selected(Role::CheckBox, ui.is_enabled(), on, galley.text()));
        if ui.is_rect_visible(rect) {
            let how_on = ui.ctx().animate_bool_responsive(response.id, on);
            let enabled = ui.is_enabled();
            let dim = |c: Color32| if enabled { c } else { c.gamma_multiply(0.45) };
            let p = ui.painter();
            let text_colour = dim(if response.hovered() { Color32::WHITE } else { TEXT });
            p.galley(Pos2::new(rect.left(), rect.center().y - galley.size().y / 2.0), galley, text_colour);
            let cap = Rect::from_min_size(Pos2::new(rect.right() - CAPSULE.x, rect.center().y - CAPSULE.y / 2.0), CAPSULE);
            p.rect_filled(cap, 8.0, dim(BAR_IDLE.lerp_to_gamma(ACCENT, how_on)));
            if response.hovered() && enabled {
                p.rect_stroke(cap, 8.0, Stroke::new(1.0, Color32::from_white_alpha(60)), StrokeKind::Outside);
            }
            let x = egui::lerp((cap.left() + 8.0)..=(cap.right() - 8.0), how_on);
            p.circle_filled(Pos2::new(x, cap.center().y), 6.0, dim(Color32::from_gray(235)));
        }
        response
    }
}

/// The same note (warning, status) must appear continuously this long before it is shown: a fleeting doubt about a
/// read is not drawn.
pub const SHOW_AFTER: Duration = Duration::from_millis(500);
/// After a note disappears it is kept this long before it is removed: a note that comes and goes does not flicker
/// with it.
pub const HOLD_FOR: Duration = Duration::from_millis(500);

/// Anti-flicker for notes (display only, the notes themselves are not changed): the same note is
/// identified by its text, shown only after it has been present continuously for [`SHOW_AFTER`], and
/// kept for [`HOLD_FOR`] after it disappears before it is removed. It does not depend on egui (`T` is
/// attached data such as a colour); the time is supplied by the caller, and unit tests use a fake clock.
#[derive(Clone, Debug)]
pub struct Steady<T> {
    entries: Vec<SteadyEntry<T>>,
}

#[derive(Clone, Debug)]
struct SteadyEntry<T> {
    text: String,
    extra: T,
    /// When this continuous appearance started.
    since: Instant,
    /// When it disappeared (None while still present).
    gone: Option<Instant>,
    /// Already shown (reached [`SHOW_AFTER`]).
    shown: bool,
}

impl<T> Default for Steady<T> {
    fn default() -> Steady<T> {
        Steady { entries: Vec::new() }
    }
}

impl<T: Clone> Steady<T> {
    /// The notes at this moment are `current` (notes with the same text count as one): returns the notes
    /// to show (those still present in the order of `current`, those being held before removal after
    /// them), and the time of the next state change (a repaint must be scheduled).
    pub fn update(&mut self, now: Instant, current: &[(String, T)]) -> (Vec<(String, T)>, Option<Duration>) {
        let mut old = std::mem::take(&mut self.entries);
        let mut next: Vec<SteadyEntry<T>> = Vec::with_capacity(current.len() + old.len());
        for (text, extra) in current {
            if next.iter().any(|e| &e.text == text) {
                continue;
            }
            let mut e = match old.iter().position(|e| &e.text == text) {
                Some(k) => old.remove(k),
                None => SteadyEntry { text: text.clone(), extra: extra.clone(), since: now, gone: None, shown: false },
            };
            e.extra = extra.clone();
            e.gone = None;
            if !e.shown && now.saturating_duration_since(e.since) >= SHOW_AFTER {
                e.shown = true;
            }
            next.push(e);
        }
        // Those absent this time: if never shown, drop them at once (the timer restarts when they reappear); those
        // already shown are removed after a delay.
        for mut e in old.into_iter().filter(|e| e.shown) {
            let gone = *e.gone.get_or_insert(now);
            if now.saturating_duration_since(gone) < HOLD_FOR {
                next.push(e);
            }
        }
        let wake = next
            .iter()
            .filter_map(|e| match e.gone {
                Some(gone) => Some((gone + HOLD_FOR).saturating_duration_since(now)),
                None if !e.shown => Some((e.since + SHOW_AFTER).saturating_duration_since(now)),
                None => None,
            })
            .min();
        self.entries = next;
        let shown = self.entries.iter().filter(|e| e.shown).map(|e| (e.text.clone(), e.extra.clone())).collect();
        (shown, wake)
    }
}

/// One line of small text, truncated with `…` when too long, with the full text shown on hover. The
/// height is fixed at one small line whether or not there is text. When not truncated it is display
/// only (Sense::hover); when truncated it becomes Sense::click, so that with click-through on the mouse
/// can still rest on it and see the full text (click-through leaves only the clickable widgets to this
/// window, see widget_rects in ui.rs).
pub fn line(ui: &mut Ui, text: &str, colour: Color32) -> Response {
    line_with(ui, text, colour, false, Sense::hover(), None, 1)
}

/// Like [`line`] but with a fixed height of `rows` lines and auto-wrapped text (the small text of an
/// advice card, a lone warning): the line count is fixed so the layout does not jump; text is
/// truncated only when `rows` lines cannot hold it, and there is a full-text tooltip. The spacing
/// between lines is 2 points, the same as the warning area.
pub fn lines(ui: &mut Ui, text: &str, colour: Color32, rows: usize) -> Response {
    line_with(ui, text, colour, false, Sense::hover(), None, rows.max(1))
}

/// Like [`line`], with the text right-aligned when `right`.
pub fn line_aligned(ui: &mut Ui, text: &str, colour: Color32, right: bool) -> Response {
    line_with(ui, text, colour, right, Sense::hover(), None, 1)
}

/// Like [`line`] but clickable (Sense::click): the text turns white while the mouse rests on it, showing the full
/// text (for example Tartarus's instrument and theme line).
pub fn click_line(ui: &mut Ui, text: &str, colour: Color32) -> Response {
    line_with(ui, text, colour, false, Sense::click(), None, 1)
}

/// Like [`line`], but draws the shortened `text` while the tooltip always shows the complete `full`
/// (Sense::click, so the mouse can rest on it under click-through too). When `full` equals `text`
/// it behaves as [`line`].
pub fn line_full(ui: &mut Ui, text: &str, full: &str, colour: Color32) -> Response {
    lines_full(ui, text, full, colour, 1)
}

/// Like [`lines`] (fixed `rows` lines, auto-wrapped), drawing the shortened `text` with the complete `full` in the
/// tooltip.
pub fn lines_full(ui: &mut Ui, text: &str, full: &str, colour: Color32, rows: usize) -> Response {
    if text == full {
        return line_with(ui, text, colour, false, Sense::hover(), None, rows.max(1));
    }
    line_with(ui, text, colour, false, Sense::click(), Some(full), rows.max(1))
}

/// The condensed form of warnings and status on screen: only a trailing parenthetical (`…開局（所有牢房未開啟…）`,
/// `… (every cell locked).`) is removed, and the whole sentence goes into the tooltip. If text follows
/// the parenthetical (`它和策略的選擇（某某）一樣好。`) it is left as it is, otherwise the meaning would
/// flip. The test: after stripping the trailing full stop and whitespace, the last character is `）` or `)`.
pub fn brief(text: &str) -> &str {
    let body = text.trim_end_matches(['\u{3002}', '.', ' ']);
    let open = if body.ends_with('\u{ff09}') {
        body.rfind('\u{ff08}')
    } else if body.ends_with(')') {
        body.rfind(" (").or_else(|| body.rfind('('))
    } else {
        None
    };
    match open {
        Some(i) if i > 0 => text[..i].trim_end(),
        _ => text,
    }
}

/// Whether small `text` is truncated at width `width` and at most `rows` lines (decides whether to make the widget
/// clickable to show the full text).
pub fn elided(ui: &Ui, text: &str, width: f32, rows: usize) -> bool {
    let mut job = egui::text::LayoutJob::simple(text.to_owned(), TextStyle::Small.resolve(ui.style()), TEXT, width.max(1.0));
    job.wrap.max_rows = rows.max(1);
    job.wrap.break_anywhere = rows <= 1;
    ui.painter().layout_job(job).elided
}

/// Like [`line_aligned`], but the tooltip always shows `tip` (Sense::click, so the mouse can rest on it
/// under click-through too). Only this one tooltip is attached, so it does not stack with the automatic
/// one shown on truncation.
pub fn line_tip(ui: &mut Ui, text: &str, tip: &str, colour: Color32, right: bool) -> Response {
    line_with(ui, text, colour, right, Sense::click(), Some(tip), 1)
}

fn line_with(ui: &mut Ui, text: &str, colour: Color32, right: bool, sense: Sense, tip: Option<&str>, rows: usize) -> Response {
    let row_h = ui.text_style_height(&TextStyle::Small);
    let h = row_h * rows as f32 + 2.0 * (rows - 1) as f32;
    let width = ui.available_width().max(10.0);
    let font = TextStyle::Small.resolve(ui.style());
    let mut job = egui::text::LayoutJob::simple(text.to_owned(), font, colour, width);
    job.wrap.max_rows = rows;
    // With one line it may break anywhere (for truncation); with several lines it wraps by word.
    job.wrap.break_anywhere = rows == 1;
    let galley = ui.painter().layout_job(job);
    // When truncated or when a full text is given, the mouse must be able to rest on it: make it clickable.
    let tip = tip.or(galley.elided.then_some(text));
    let sense = if tip.is_some() && !text.is_empty() { Sense::click() } else { sense };
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, h), sense);
    if text.is_empty() {
        return response;
    }
    let colour = if sense.senses_click() && response.hovered() { Color32::WHITE } else { colour };
    let x = if right { rect.right() - galley.size().x } else { rect.left() };
    // With one line, centre vertically; with several lines, lay out from the top.
    let y = if rows == 1 { rect.center().y - galley.size().y / 2.0 } else { rect.top() };
    ui.painter().galley_with_override_text_color(Pos2::new(x, y), galley, colour);
    match tip {
        Some(tip) => response.on_hover_text(tip),
        None if sense.senses_click() => response.on_hover_text(text),
        None => response,
    }
}

/// A warning area fixed at two lines high (shared by the Tartarus board page and the Vorax panel):
/// `now` is part of the advice (for example the chain order), shown at once without anti-flicker and
/// placed first; `items` are warnings and explanations, sorted by importance, placed after it once
/// through the [`Steady`] anti-flicker. Up to two notes are written out as they are; with more than
/// two, the second line is `N more` (the `"{} more"` text), and clicking it expands a scrollable list
/// (up to about 5 lines high), and clicking `Show less` returns to two lines. With no warnings both
/// lines stay blank.
pub fn notes(ui: &mut Ui, id: egui::Id, now: &[(String, Color32)], items: &[(String, Color32)]) {
    const ROWS: usize = 2;
    let (steady, wake) = ui.data_mut(|d| d.get_temp_mut_or_default::<Steady<Color32>>(id).update(Instant::now(), items));
    let shown: Vec<(String, Color32)> = now.iter().cloned().chain(steady).collect();
    if let Some(wake) = wake {
        // Wait 1 ms more, so that on waking the threshold has surely passed.
        ui.ctx().request_repaint_after(wake + Duration::from_millis(1));
    }
    let open_id = id.with("open");
    let mut open = ui.data(|d| d.get_temp::<bool>(open_id)).unwrap_or(false) && shown.len() > ROWS;
    let h = ui.text_style_height(&TextStyle::Small);
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        if open {
            egui::ScrollArea::vertical().id_salt(id.with("scroll")).max_height(h * 5.0 + 8.0).show(ui, |ui| {
                for (text, colour) in &shown {
                    ui.add(egui::Label::new(RichText::new(text).small().color(*colour)).wrap().selectable(false));
                }
            });
            if link_line(ui, tr("Show less")).clicked() {
                open = false;
            }
            return;
        }
        // Only one note: give it both lines, auto-wrapped (same height as two lines).
        if let [(text, colour)] = shown.as_slice() {
            lines_full(ui, brief(text), text, *colour, ROWS);
            return;
        }
        if shown.len() <= ROWS {
            for k in 0..ROWS {
                match shown.get(k) {
                    Some((text, colour)) => line_full(ui, brief(text), text, *colour),
                    None => line(ui, "", MUTED),
                };
            }
            return;
        }
        let (text, colour) = &shown[0];
        line_full(ui, brief(text), text, *colour);
        let all: Vec<&str> = shown.iter().map(|(t, _)| t.as_str()).collect();
        if link_line(ui, &trf!("{} more", shown.len() - 1)).on_hover_text(all.join("\n")).clicked() {
            open = true;
        }
    });
    ui.data_mut(|d| d.insert_temp(open_id, open));
}

/// One line of clickable small text (primary colour): full line width, one small line high, Sense::click.
fn link_line(ui: &mut Ui, text: &str) -> Response {
    let h = ui.text_style_height(&TextStyle::Small);
    let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width().max(10.0), h), Sense::click());
    let colour = if response.hovered() { Color32::WHITE } else { ACCENT };
    let font = TextStyle::Small.resolve(ui.style());
    ui.painter().text(Pos2::new(rect.left(), rect.center().y), egui::Align2::LEFT_CENTER, text, font, colour);
    response
}

/// A scaled-down app icon (at the top of the left navigation bar): a night-blue rounded square, a gold
/// ring, a horizon sitting low, and a four-point star rising over the horizon, the same design as
/// assets\icon.png.
pub fn paint_logo(p: &egui::Painter, rect: Rect) {
    let s = rect.width().min(rect.height());
    let r = Rect::from_center_size(rect.center(), Vec2::splat(s));
    p.rect_filled(r, s * 0.22, NIGHT);
    let c = r.center();
    let ring = s * 0.34;
    p.circle_stroke(c, ring, Stroke::new((s * 0.06).max(1.2), GOLD));
    let horizon = c.y + s * 0.12;
    let half = ring * 0.98;
    p.line_segment([Pos2::new(c.x - half, horizon), Pos2::new(c.x + half, horizon)], Stroke::new((s * 0.05).max(1.0), GOLD));
    star(p, Pos2::new(c.x, horizon - s * 0.17), s * 0.17, STAR);
}

/// A four-point star with centre `c` and radius `r`.
pub fn star(p: &egui::Painter, c: Pos2, r: f32, colour: Color32) {
    let inner = r * 0.3;
    let points: Vec<Pos2> = (0..8)
        .map(|k| {
            let a = std::f32::consts::FRAC_PI_4 * k as f32 - std::f32::consts::FRAC_PI_2;
            let len = if k % 2 == 0 { r } else { inner };
            c + Vec2::angled(a) * len
        })
        .collect();
    // A star is not a convex polygon: draw it as triangles from the centre to each tip.
    for k in 0..8 {
        let (a, b) = (points[k], points[(k + 1) % 8]);
        p.add(egui::Shape::convex_polygon(vec![c, a, b], colour, Stroke::NONE));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The condensed form of a warning: only a trailing parenthetical is removed; when text follows the parenthetical
    /// it is left as it is (so the meaning does not flip).
    #[test]
    fn brief_drops_only_a_trailing_bracket() {
        assert_eq!(brief("選單蓋住盤面，也沒讀過盤面：先當作開局（所有牢房未開啟；對局中途的選擇請先讀取盤面）"), "選單蓋住盤面，也沒讀過盤面：先當作開局");
        assert_eq!(brief("選單蓋住盤面：牢房用上次讀到的（第 6 天）"), "選單蓋住盤面：牢房用上次讀到的");
        // The zh_tw.rs text `模擬評估它和策略的選擇（{}）一樣好。`: text follows the parenthetical, so it is left
        // alone.
        assert_eq!(brief("模擬評估它和策略的選擇（去 04 號）一樣好。"), "模擬評估它和策略的選擇（去 04 號）一樣好。");
        assert_eq!(brief("模擬找到比策略的選擇（去 04 號）更好的走法。"), "模擬找到比策略的選擇（去 04 號）更好的走法。");
        // The matching English: the parenthetical is at the sentence end (only the full stop follows), so it is
        // removed, and the name the strategy picked is in the tooltip.
        assert_eq!(brief("The playouts rate it as good as the strategy's pick (Go to 04)."), "The playouts rate it as good as the strategy's pick");
        assert_eq!(brief("The playouts found a better move than the strategy's pick (Go to 04)."), "The playouts found a better move than the strategy's pick");
        // Another sentence follows the parenthetical: left alone.
        assert_eq!(brief("the run start is assumed (every cell locked). For a pick later, read first."), "the run start is assumed (every cell locked). For a pick later, read first.");
        assert_eq!(brief("沒有括號"), "沒有括號");
        assert_eq!(brief("（只有括號）"), "（只有括號）");
    }

    /// The width of the large text in a one-line advice card: all of it with no small text; with small text, the
    /// small text needs about half the width (at most 40%) and the large text at least 60%.
    #[test]
    fn big_room_is_the_whole_room_without_small_text() {
        assert_eq!(big_room(300.0, 0.0, 8.0), 300.0);
        assert_eq!(big_room(300.0, 100.0, 8.0), 300.0 - 62.0 - 8.0);
        assert_eq!(big_room(300.0, 1000.0, 8.0), 180.0);
    }

    /// Fake clock: the same starting point plus a number of milliseconds.
    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    fn items(texts: &[&str]) -> Vec<(String, ())> {
        texts.iter().map(|t| (t.to_string(), ())).collect()
    }

    fn texts(shown: &[(String, ())]) -> Vec<&str> {
        shown.iter().map(|(t, _)| t.as_str()).collect()
    }

    #[test]
    fn a_note_under_half_a_second_is_not_shown() {
        let (base, mut s) = (Instant::now(), Steady::default());
        let (shown, wake) = s.update(at(base, 0), &items(&["讀不到 3 號"]));
        assert!(shown.is_empty());
        assert_eq!(wake, Some(Duration::from_millis(500)));
        let (shown, _) = s.update(at(base, 499), &items(&["讀不到 3 號"]));
        assert!(shown.is_empty());
        // Disappears before 0.5 s: not shown and not kept; the timer restarts when it reappears.
        let (shown, wake) = s.update(at(base, 520), &items(&[]));
        assert!(shown.is_empty());
        assert_eq!(wake, None);
        let (shown, _) = s.update(at(base, 600), &items(&["讀不到 3 號"]));
        assert!(shown.is_empty());
        let (shown, _) = s.update(at(base, 1050), &items(&["讀不到 3 號"]));
        assert!(shown.is_empty());
    }

    #[test]
    fn a_note_shows_after_half_a_second() {
        let (base, mut s) = (Instant::now(), Steady::default());
        s.update(at(base, 0), &items(&["甲"]));
        let (shown, _) = s.update(at(base, 200), &items(&["甲", "乙"]));
        assert!(shown.is_empty());
        let (shown, wake) = s.update(at(base, 500), &items(&["甲", "乙"]));
        assert_eq!(texts(&shown), ["甲"]);
        // `乙` starts at 200 and reaches the threshold at 700.
        assert_eq!(wake, Some(Duration::from_millis(200)));
        let (shown, wake) = s.update(at(base, 700), &items(&["甲", "乙"]));
        assert_eq!(texts(&shown), ["甲", "乙"]);
        assert_eq!(wake, None);
    }

    #[test]
    fn a_gone_note_stays_for_half_a_second() {
        let (base, mut s) = (Instant::now(), Steady::default());
        s.update(at(base, 0), &items(&["甲"]));
        s.update(at(base, 600), &items(&["甲"]));
        // Disappears at 1000: still there until 1499, removed at 1500.
        let (shown, wake) = s.update(at(base, 1000), &items(&[]));
        assert_eq!(texts(&shown), ["甲"]);
        assert_eq!(wake, Some(Duration::from_millis(500)));
        let (shown, _) = s.update(at(base, 1499), &items(&[]));
        assert_eq!(texts(&shown), ["甲"]);
        let (shown, wake) = s.update(at(base, 1500), &items(&[]));
        assert!(shown.is_empty());
        assert_eq!(wake, None);
    }

    #[test]
    fn a_note_back_within_the_hold_keeps_showing() {
        let (base, mut s) = (Instant::now(), Steady::default());
        s.update(at(base, 0), &items(&["甲"]));
        s.update(at(base, 500), &items(&["甲"]));
        s.update(at(base, 800), &items(&[]));
        let (shown, wake) = s.update(at(base, 1000), &items(&["甲"]));
        assert_eq!(texts(&shown), ["甲"]);
        assert_eq!(wake, None);
        // After coming back it disappears again: 0.5 s counted from the new disappearance time.
        let (shown, _) = s.update(at(base, 1400), &items(&[]));
        assert_eq!(texts(&shown), ["甲"]);
        let (shown, _) = s.update(at(base, 1850), &items(&[]));
        assert_eq!(texts(&shown), ["甲"]);
        let (shown, _) = s.update(at(base, 1900), &items(&[]));
        assert!(shown.is_empty());
    }
}
