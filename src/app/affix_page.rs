//! The affix page (public since 1.2.0). Laid out to be easy to find, recognise and use
//! (the user, 2026-11-08):
//! - find: on the map page, the affixes on it now come first ("On this map", in the panel's order, those not found too:
//!   their text as read and why, not in the list yet, unsure or cut off); below, every affix
//!   of the client's language (the user: only the current language's affixes), dangers first, then warnings, then the
//!   rest, each group in the order of its text with the leading number left out, so affixes that start alike sit
//!   together; the order follows the default grades, so a row does not jump away when its grade is changed; the list
//!   can be narrowed by grades and by states, several at once ([`Filter`]), and searched;
//! - recognise: text at the normal size, each grade by its colour and its name; on this map, an affix not graded is
//!   drawn in the colour the game shows it in (a dashed rim and a stripe, not the graded card's solid one);
//! - use: one click picks a grade (back to the game's colour too, the default), the changed ones can be listed alone;
//!   resetting every grade and marking the new affixes read come first under "Every affix" and ask in a window
//!   ([`Ask`]); once scrolled down, "Back to top" shows at the bottom right ([`top_button`]).
//!
//! The switch for the frames on the game and the frame style (A frame, the default, or B bar and tint; the user asked
//! for both) with a small preview come first. Every affix starts framed in the colour the game shows it in (since
//! 1.2.0, the user, 2026-11-11); only the grades the user picks are saved.
//!
//! A sentence's grade is buttons under it (danger, warning, not framed, and the game's colour), the one on show filled:
//! the user picked these over a coloured chip before the sentence and a menu under it, on seeing them (2026-11-08).

use super::affix_live::Seen;
use super::theme::{self, ACCENT, DIVIDER, MUTED, Switch};
use crate::affix::dict::Sentence;
use crate::affix::frames::{DANGER, FrameStyle, WARN};
use crate::affix::{Dict, Grade, Grades, Learned};
use crate::lang::{self, tr, trf};
use eframe::egui::{self, Align, Color32, Layout, RichText, Sense, Stroke, StrokeKind, Ui, Vec2};
use std::collections::BTreeSet;

/// Which sentences the list shows (the user, 2026-11-12): the grades picked, any of them (the game's colour, danger,
/// warning, not framed: [`GRADE_PICKS`]), and the states picked, any of them (changed, new); with both kinds picked, a
/// sentence has to match both ("changed" and "danger": the ones changed to danger). Nothing picked: all.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Filter {
    /// The grades picked, in the order of [`GRADE_PICKS`].
    grades: [bool; 4],
    /// The sentences the user changed.
    changed: bool,
    /// Affixes learned from the game log the user has not graded or marked read yet.
    new: bool,
}

/// The grades of [`Filter::grades`]: the game's colour (not graded), then the three grades.
const GRADE_PICKS: [Option<Grade>; 4] = [None, Some(Grade::Danger), Some(Grade::Warn), Some(Grade::None)];

impl Filter {
    /// Nothing picked: every sentence.
    fn is_all(&self) -> bool {
        *self == Filter::default()
    }

    /// Whether a sentence of grade `grade` (None: the game's colour), `changed` by the user or not and `new` or not is
    /// listed.
    fn keeps(&self, grade: Option<Grade>, changed: bool, new: bool) -> bool {
        let by_grade = !self.grades.contains(&true) || GRADE_PICKS.iter().zip(self.grades).any(|(g, on)| on && *g == grade);
        let by_state = !(self.changed || self.new) || (self.changed && changed) || (self.new && new);
        by_grade && by_state
    }

    /// What the filter shows and how many (`n`), in words above the list ("Changed and Danger: 1 affixes", "Danger or
    /// Warning: 3 affixes"): the states, then the grades; None for all.
    fn said(&self, n: usize) -> Option<String> {
        let states: Vec<&str> = [(self.changed, tr("Changed")), (self.new, tr("New"))].into_iter().filter(|(on, _)| *on).map(|(_, n)| n).collect();
        let grades: Vec<&str> = GRADE_PICKS.iter().zip(self.grades).filter(|(_, on)| *on).map(|(g, _)| g.map_or(tr("Game colour"), name)).collect();
        match (states.join(tr(" or ")), grades.join(tr(" or "))) {
            (s, g) if s.is_empty() && g.is_empty() => None,
            (s, g) if s.is_empty() || g.is_empty() => Some(trf!("{}: {} affixes", s + &g, n)),
            (s, g) => Some(trf!("{} and {}: {} affixes", s, g, n)),
        }
    }

    /// The filter of `ASCENDANT_PREVIEW_AFFIX_FILTER` (preview only): names split by `,` (game, danger, warn, none,
    /// changed, new).
    fn of_preview(v: &str) -> Filter {
        let mut f = Filter::default();
        for name in v.split(',').map(str::trim) {
            match name {
                "game" => f.grades[0] = true,
                "danger" => f.grades[1] = true,
                "warn" => f.grades[2] = true,
                "none" => f.grades[3] = true,
                "changed" => f.changed = true,
                "new" => f.new = true,
                _ => {}
            }
        }
        f
    }
}

/// A question the page asks in a window before it is done, with Cancel and Confirm (the user, 2026-11-12: a window,
/// not a second click on the same button).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ask {
    /// Every grade back to the game's colour.
    Reset,
    /// Every new affix of the client language marked read.
    ReadAll,
}

/// The page's own state.
pub struct AffixPage {
    search: String,
    filter: Filter,
    defaults: Dict,
    /// The question on show in a window, if any.
    ask: Option<Ask>,
    /// "Back to top" was clicked: the page's scroll area goes to the top on the next frame ([`AffixPage::take_to_top`]).
    to_top: bool,
    /// Preview only: the entries shown as on the map (`ASCENDANT_PREVIEW_AFFIX_SEEN`, see [`AffixPage::preview_seen`]).
    preview_seen: Vec<String>,
    /// Preview only: the hint of a panel at another interface scale shows (`ASCENDANT_PREVIEW_AFFIX_ODD_SCALE=1`).
    preview_odd_scale: bool,
    /// Sentences graded on the page, kept listed under the filter on show ([`Kept`]).
    kept: Kept,
    /// Preview only: the hint of a panel in the other client language shows (`ASCENDANT_PREVIEW_AFFIX_CLIENT=en` or
    /// `zh-TW`, the language the panel reads as).
    preview_client: Option<lang::Lang>,
}

impl AffixPage {
    /// The table the page lists: the defaults with the affixes learned from the game log (the live frames read with the
    /// same, so a sentence's index is the same in both).
    pub fn set_learned(&mut self, learned: &[Learned]) {
        let table = Dict::builtin().with_learned(learned);
        // the sentences kept listed are indices into the table: an affix learned may move them (a grade picked, which
        // comes here too, does not)
        let (old, new) = (self.defaults.sentences(), table.sentences());
        if old.len() != new.len() || old.iter().zip(new).any(|(a, b)| a.ids != b.ids || a.en != b.en || a.zh != b.zh) {
            self.kept = Kept::default();
        }
        self.defaults = table;
    }

    /// The page is left ([`super::ui`]'s `set_tab`): the question on show goes, as Cancel. Left open, a "Reset every
    /// grade?" window asked while the app switched to the board came back by itself the next time the page showed
    /// (review of 1.3.0), offering to do what cannot be undone; "Mark all as read?" the same.
    pub fn leave(&mut self) {
        self.ask = None;
    }

    /// "Back to top" was clicked ([`top_button`]): true once, for the page's scroll area to go to the top.
    pub fn take_to_top(&mut self) -> bool {
        std::mem::take(&mut self.to_top)
    }

    /// The answer to the question on show ([`Ask`]), which closes it: Confirm (`yes`) resets every grade, or marks read
    /// every new affix listed in the client language (`zh`: Traditional Chinese; the ones counted on the button, their
    /// grades untouched); Cancel changes nothing. True when a setting changed.
    fn answer(&mut self, yes: bool, grades: &mut Grades, read: &mut BTreeSet<String>, zh: bool) -> bool {
        let Some(ask) = self.ask.take() else { return false };
        if !yes {
            return false;
        }
        match ask {
            Ask::Reset => {
                let had = !grades.is_empty();
                grades.clear();
                had
            }
            Ask::ReadAll => {
                let new: Vec<String> = self.defaults.sentences().iter().filter(|s| text_in(s, zh).is_some() && is_new(s, grades, read)).map(|s| s.key().to_string()).collect();
                // the cards graded while new lose their mark too
                self.kept.new.clear();
                new.into_iter().fold(false, |added, k| read.insert(k) | added)
            }
        }
    }

    /// Preview only: the entries shown as on the map, from `ASCENDANT_PREVIEW_AFFIX_SEEN`: items split by `|` (by `,`
    /// when there is no `|`), each a sentence's id, or text read that was not found: `?text` not in the list, `~text`
    /// unsure, `text...` (after `?` or `~`) cut off; `#danger` or `#warn` at the end, the grade its game colour frames
    /// it with (an id's too).
    fn preview_seen(&self) -> Vec<Seen> {
        self.preview_seen
            .iter()
            .filter_map(|item| {
                let item = item.trim();
                let (item, game) = match (item.strip_suffix("#danger"), item.strip_suffix("#warn")) {
                    (Some(t), _) => (t, Grade::Danger),
                    (_, Some(t)) => (t, Grade::Warn),
                    _ => (item, Grade::None),
                };
                let (ambiguous, text) = match item.strip_prefix('?') {
                    Some(text) => (false, text),
                    None => match item.strip_prefix('~') {
                        Some(text) => (true, text),
                        None => return self.defaults.sentences().iter().position(|s| s.ids.iter().any(|x| x == item)).map(|i| Seen::Known(i, game)),
                    },
                };
                let (text, cut) = text.strip_suffix("...").map_or((text, false), |t| (t, true));
                Some(Seen::Unknown { text: text.to_string(), ambiguous, cut, game })
            })
            .collect()
    }
}

/// A learned affix the user has neither graded nor marked read (`read`, by any of its ids): marked "New" on the page.
fn is_new(s: &Sentence, grades: &Grades, read: &BTreeSet<String>) -> bool {
    s.learned && s.user_grade(grades).is_none() && !s.ids.iter().any(|id| read.contains(id))
}

impl Default for AffixPage {
    fn default() -> Self {
        let defaults = Dict::builtin();
        // preview only: the list's filter (see Filter::of_preview) and a question on show (reset, read)
        let filter = std::env::var("ASCENDANT_PREVIEW_AFFIX_FILTER").map(|v| Filter::of_preview(&v)).unwrap_or_default();
        let ask = match std::env::var("ASCENDANT_PREVIEW_AFFIX_ASK").as_deref() {
            Ok("reset") => Some(Ask::Reset),
            Ok("read") => Some(Ask::ReadAll),
            _ => None,
        };
        let preview_seen = std::env::var("ASCENDANT_PREVIEW_AFFIX_SEEN")
            .map(|v| v.split(if v.contains('|') { '|' } else { ',' }).filter(|s| !s.trim().is_empty()).map(str::to_string).collect())
            .unwrap_or_default();
        AffixPage {
            search: std::env::var("ASCENDANT_PREVIEW_AFFIX_SEARCH").unwrap_or_default(),
            filter,
            defaults,
            ask,
            to_top: false,
            preview_seen,
            preview_odd_scale: std::env::var("ASCENDANT_PREVIEW_AFFIX_ODD_SCALE").is_ok_and(|v| v == "1"),
            preview_client: std::env::var("ASCENDANT_PREVIEW_AFFIX_CLIENT").ok().map(|v| lang::Lang::from_key(&v)),
            kept: Kept::default(),
        }
    }
}

const ORDER: [Grade; 3] = [Grade::Danger, Grade::Warn, Grade::None];
/// How far down the page is scrolled before "Back to top" shows.
const TOP_AFTER: f32 = 240.0;

fn name(g: Grade) -> &'static str {
    match g {
        Grade::Danger => tr("Danger"),
        Grade::Warn => tr("Warning"),
        Grade::None => tr("Not framed"),
    }
}

fn colour(g: Grade) -> Color32 {
    match g {
        Grade::Danger => Color32::from_rgb(DANGER[0], DANGER[1], DANGER[2]),
        Grade::Warn => Color32::from_rgb(WARN[0], WARN[1], WARN[2]),
        Grade::None => Color32::from_rgb(0x3a, 0x40, 0x4c),
    }
}

/// `a` with `share` of `b` mixed in (opaque).
fn mix(a: Color32, b: Color32, share: f32) -> Color32 {
    let m = |x: u8, y: u8| (f32::from(x) * (1.0 - share) + f32::from(y) * share).round() as u8;
    Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}

/// Text on a grade's colour.
fn ink(g: Grade) -> Color32 {
    if g == Grade::None { Color32::from_gray(210) } else { Color32::BLACK }
}

/// A sentence as shown: the client language's text (`zh`: Traditional Chinese) when there is one, else the other
/// language's (flagged); `#`, a rolled number, shown as X.
fn shown_text(en: &str, zh_text: &str, zh: bool) -> (String, bool) {
    let (text, other) = match (zh, zh_text.is_empty(), en.is_empty()) {
        (true, false, _) => (zh_text, false),
        (true, true, _) => (en, true),
        (false, _, false) => (en, false),
        (false, _, true) => (zh_text, true),
    };
    (text.replace('#', "X"), other)
}

/// The grade of sentence `s`: the user's when changed (saved under any of its ids), else the default.
fn grade_of(grades: &Grades, s: &Sentence) -> Option<Grade> {
    s.user_grade(grades)
}

/// The user picked grade `g` for sentence `s` (None: the game's colour, the default); true when the saved grades
/// changed. A grade is saved under the sentence's key alone (a grade under another of its ids, from before it gained
/// ids or was folded, goes); the game's colour saves none.
fn set_grade(grades: &mut Grades, s: &Sentence, g: Option<Grade>) -> bool {
    let before: Vec<(String, Grade)> = s.ids.iter().filter_map(|id| grades.get(id).map(|&g| (id.clone(), g))).collect();
    for id in &s.ids {
        grades.remove(id);
    }
    if let Some(g) = g {
        grades.insert(s.key().to_string(), g);
    }
    let after: Vec<(String, Grade)> = s.ids.iter().filter_map(|id| grades.get(id).map(|&g| (id.clone(), g))).collect();
    before != after
}

/// Sentence `s`'s text in the client language (`zh`: Traditional Chinese), None when it was only seen in the other one.
fn text_in(s: &Sentence, zh: bool) -> Option<&str> {
    let t = if zh { &s.zh } else { &s.en };
    (!t.is_empty()).then_some(t.as_str())
}

/// The search box, with a clear button "×" at its right while it holds text: a click empties it and leaves the
/// keyboard in the box (the user's request, 2026-11-09).
/// Returns the text box's response and the clear button's rect when it shows.
fn search_box(ui: &mut egui::Ui, search: &mut String) -> (egui::Response, Option<egui::Rect>) {
    ui.horizontal(|ui| {
        let clear = !search.is_empty();
        let button_w = if clear { 22.0 + ui.spacing().item_spacing.x } else { 0.0 };
        let edit = ui.add(egui::TextEdit::singleline(search).hint_text(tr("Search")).desired_width((ui.available_width() - button_w).max(40.0)));
        let mut button = None;
        if clear {
            let r = ui.add(egui::Button::new("×").frame(false).min_size(egui::vec2(22.0, 0.0))).on_hover_text(tr("Clear search"));
            button = Some(r.rect);
            if r.clicked() {
                search.clear();
                edit.request_focus();
            }
        }
        (edit, button)
    })
    .inner
}

/// Whether a sentence's text matches the search text (ignoring case).
fn matches(text: &str, search: &str) -> bool {
    let needle = search.trim().to_lowercase();
    needle.is_empty() || text.to_lowercase().contains(&needle)
}

/// The text a sentence sorts by: lower case, without the leading number ("#%", "+#", "X%") and the spaces and marks
/// before the first word, so "#% Max Fire Resistance" sorts with the other "Max ..." and "一#% 玩家..." with "玩家...".
fn sort_text(text: &str) -> String {
    text.trim_start_matches(['#', '%', '+', '-', '\u{2212}', '一', 'X', ' ', '\u{3000}', ',', '，']).to_lowercase()
}

/// Draws the page; true when a setting changed (the caller saves and passes them on to the live frames). `seen`: the
/// entries on the map page now, from the live frames (found ones by their index in the page's table); `odd_scale`: the
/// map page's panel is of another interface scale than the reader was measured at (nothing framed, said here);
/// `client`: the game client language the panel reads as, when it is not the one set (said here, with where to set it);
/// `read`: the learned affixes marked read ("Mark all as read", saved with the settings).
#[allow(clippy::too_many_arguments)]
pub fn draw(ui: &mut Ui, page: &mut AffixPage, on: &mut bool, style: &mut FrameStyle, grades: &mut Grades, read: &mut BTreeSet<String>, seen: &[Seen], hints: Hints) -> bool {
    let Hints { odd_scale, client, map } = hints;
    let mut changed = false;
    theme::heading(ui, tr("Map affixes"));
    theme::group_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        let hover = tr("On the map page, frames the affixes graded below as danger (red) or warning (yellow).");
        changed |= ui.add(Switch::new(on, tr("Frame affixes on the game"))).on_hover_text(hover).changed();
        if odd_scale || page.preview_odd_scale {
            ui.add(egui::Label::new(RichText::new(tr("The affix panel is drawn at another interface scale than Ascendant knows yet: affixes are not framed. The game's default interface scale works.")).small().color(theme::WARN)).wrap());
        }
        if let Some(read_as) = client.or(page.preview_client) {
            ui.add(egui::Label::new(RichText::new(client_hint(read_as)).small().color(theme::WARN)).wrap());
        }
        ui.horizontal(|ui| {
            ui.label(tr("Frame style"));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let mut picked = *style;
                egui::ComboBox::from_id_salt("affix-frame-style").width(ui.available_width().min(120.0)).selected_text(style_name(*style)).show_ui(ui, |ui| {
                    for s in [FrameStyle::Outline, FrameStyle::Bar] {
                        ui.selectable_value(&mut picked, s, style_name(s));
                    }
                });
                if picked != *style {
                    *style = picked;
                    changed = true;
                }
            });
        });
        style_preview(ui, *style);
    });
    let seen: Vec<Seen> = if seen.is_empty() { page.preview_seen() } else { seen.to_vec() };
    let sentences = page.defaults.sentences();
    page.kept.update(page.filter, &page.search, ui.ctx().cumulative_pass_nr());
    if !seen.is_empty() {
        match map {
            None => theme::heading(ui, &trf!("On this map ({})", seen.len())),
            Some(MapTitle::This { from_log }) => {
                theme::heading(ui, &trf!("This map ({})", seen.len()));
                if from_log {
                    ui.label(RichText::new(tr("From the game log")).small().color(MUTED))
                        .on_hover_text(tr("The panel was not read in full before the map was entered: these are the affixes the game log names."));
                }
            }
            Some(MapTitle::Earlier { at }) => theme::heading(ui, &trf!("Earlier map · {} ({})", clock(at), seen.len())),
        }
        theme::group_frame().stroke(Stroke::new(1.0, ACCENT)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            for item in &seen {
                match item {
                    Seen::Known(i, game) => {
                        if let Some(s) = sentences.get(*i) {
                            let was_new = is_new(s, grades, read);
                            if sentence_row(ui, s, grades, read, page.kept.new.contains(i), *game) {
                                changed = true;
                                if was_new && !page.kept.new.contains(i) {
                                    page.kept.new.push(*i);
                                }
                            }
                        }
                    }
                    Seen::Unknown { text, ambiguous, cut, game } => unknown_row(ui, text, *ambiguous, *cut, *game),
                    Seen::Logged(id) => logged_row(ui, *id),
                }
            }
        });
    }
    // the client language's sentences, by default grade, then by text (see the module notes)
    let zh = lang::zh();
    let rank = |g: Grade| ORDER.iter().position(|&o| o == g).unwrap_or(ORDER.len());
    let mut listed: Vec<(usize, String)> = (0..sentences.len()).filter_map(|i| text_in(&sentences[i], zh).map(|t| (i, sort_text(t)))).collect();
    listed.sort_by(|a, b| rank(sentences[a.0].grade).cmp(&rank(sentences[b.0].grade)).then_with(|| a.1.cmp(&b.1)));
    let grade_n = GRADE_PICKS.map(|g| listed.iter().filter(|(i, _)| grade_of(grades, &sentences[*i]) == g).count());
    let changed_n = listed.iter().filter(|(i, _)| sentences[*i].user_grade(grades).is_some()).count();
    let new_n = listed.iter().filter(|(i, _)| is_new(&sentences[*i], grades, read)).count();
    // every sentence the user graded, either language: what a reset takes back
    let changed_all = sentences.iter().filter(|s| s.user_grade(grades).is_some()).count();
    // the heading, with "Mark all as read" and "Reset every grade" on its right (on a line of their own when narrow)
    let asks = |ui: &mut Ui| {
        let mut ask = None;
        let reset = egui::Button::new(RichText::new(tr("Reset every grade to the default")).small());
        if ui.add_enabled(!grades.is_empty(), reset).clicked() {
            ask = Some(Ask::Reset);
        }
        let read_all = egui::Button::new(RichText::new(trf!("Mark all as read ({})", new_n)).small().color(if new_n > 0 { theme::GOOD } else { MUTED }))
            .stroke(Stroke::new(1.0, if new_n > 0 { theme::GOOD } else { DIVIDER }))
            .fill(Color32::TRANSPARENT);
        if ui.add_enabled(new_n > 0, read_all).on_hover_text(tr("Clears the \"New\" marks; the grades stay.")).clicked() {
            ask = Some(Ask::ReadAll);
        }
        ask
    };
    ui.add_space(4.0);
    // on one line when the heading and both buttons fit, measured in the language on show
    let font = egui::TextStyle::Small.resolve(ui.style());
    let width = |text: String| ui.painter().layout_no_wrap(text, font.clone(), MUTED).size().x;
    let pad = ui.spacing().button_padding.x * 4.0 + ui.spacing().item_spacing.x * 3.0 + 8.0;
    let wide = width(tr("Every affix").to_string()) + width(trf!("Mark all as read ({})", new_n)) + width(tr("Reset every grade to the default").to_string()) + pad;
    let asked = if ui.available_width() >= wide {
        ui.horizontal(|ui| {
            ui.label(RichText::new(tr("Every affix")).small().strong().color(MUTED));
            ui.with_layout(Layout::right_to_left(Align::Center), asks).inner
        })
        .inner
    } else {
        ui.label(RichText::new(tr("Every affix")).small().strong().color(MUTED));
        ui.with_layout(Layout::right_to_left(Align::Min).with_main_wrap(true), asks).inner
    };
    if asked.is_some() {
        page.ask = asked;
    }
    // the filters, with how many each shows: the grades on one line, the states and "All" on the next
    let pick = |ui: &mut Ui, on: bool, text: String, c: Option<Color32>| {
        let rich = RichText::new(text).small().color(if on { Color32::BLACK } else { c.unwrap_or(theme::TEXT) });
        let button = egui::Button::new(rich).fill(if on { c.unwrap_or(ACCENT) } else { Color32::TRANSPARENT }).stroke(Stroke::new(1.0, c.unwrap_or(DIVIDER)));
        ui.add(button).clicked()
    };
    let row_name = |ui: &mut Ui, text: &str| {
        ui.add_sized(Vec2::new(32.0, 18.0), egui::Label::new(RichText::new(text).small().color(MUTED)));
    };
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        row_name(ui, tr("Grade"));
        for (n, (g, count)) in GRADE_PICKS.iter().zip(grade_n).enumerate() {
            let (text, c) = match g {
                None => (trf!("Game colour {}", count), Color32::from_gray(190)),
                Some(Grade::None) => (format!("{} {count}", name(Grade::None)), Color32::from_gray(150)),
                Some(g) => (format!("{} {count}", name(*g)), colour(*g)),
            };
            if pick(ui, page.filter.grades[n], text, Some(c)) {
                page.filter.grades[n] ^= true;
            }
        }
    });
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        row_name(ui, tr("State"));
        if pick(ui, page.filter.changed, trf!("Changed {}", changed_n), Some(ACCENT)) {
            page.filter.changed ^= true;
        }
        if pick(ui, page.filter.new, trf!("New {}", new_n), Some(theme::GOOD)) {
            page.filter.new ^= true;
        }
        if pick(ui, page.filter.is_all(), trf!("All {}", listed.len()), None) {
            page.filter = Filter::default();
        }
    });
    let _ = search_box(ui, &mut page.search);
    // the sentences listed: by the filter (or graded here, kept where they were) and the search
    let rows: Vec<usize> = listed
        .iter()
        .map(|(i, _)| *i)
        .filter(|i| {
            let s = &sentences[*i];
            (page.kept.list.contains(i) || page.filter.keeps(grade_of(grades, s), s.user_grade(grades).is_some(), is_new(s, grades, read)))
                && text_in(s, zh).is_some_and(|t| matches(t, &page.search))
        })
        .collect();
    if let Some(said) = page.filter.said(rows.len()) {
        ui.label(RichText::new(said).small().color(MUTED));
    }
    ui.add_space(4.0);
    for i in &rows {
        let s = &sentences[*i];
        let was_new = is_new(s, grades, read);
        if sentence_row(ui, s, grades, read, page.kept.new.contains(i), Grade::None) {
            changed = true;
            if !page.kept.list.contains(i) {
                page.kept.list.push(*i);
            }
            if was_new && !page.kept.new.contains(i) {
                page.kept.new.push(*i);
            }
        }
    }
    if rows.is_empty() {
        theme::line(ui, tr("No affix matches"), MUTED);
    }
    ui.add_space(8.0);
    // the question on show, in a window: Confirm does it, Cancel (or Escape, or a click beside the window) does not
    if let Some(ask) = page.ask {
        let (title, body) = match ask {
            Ask::Reset => (tr("Reset every grade?"), trf!("The {} affixes you changed all go back to the game's colour. This cannot be undone.", changed_all)),
            Ask::ReadAll => (tr("Mark all as read?"), trf!("The \"New\" mark of {} affixes goes; their grades stay, framed by the game's colour.", new_n)),
        };
        let width = (ui.ctx().content_rect().width() - 48.0).clamp(160.0, 320.0);
        let shown = egui::Modal::new(egui::Id::unique("affix-ask")).show(ui.ctx(), |ui| {
            ui.set_width(width);
            ui.label(RichText::new(title).strong());
            ui.add(egui::Label::new(RichText::new(body).small().color(MUTED)).wrap());
            ui.add_space(8.0);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let confirm = ui.add(egui::Button::new(RichText::new(tr("Confirm")).strong().color(Color32::WHITE)).fill(theme::ERROR));
                let cancel = ui.button(tr("Cancel"));
                if confirm.clicked() {
                    Some(true)
                } else if cancel.clicked() {
                    Some(false)
                } else {
                    None
                }
            })
            .inner
        });
        if let Some(yes) = shown.inner.or_else(|| shown.should_close().then_some(false)) {
            changed |= page.answer(yes, grades, read, zh);
        }
    }
    changed
}

/// "↑ Back to top" at the bottom right of the affix page's view (`view`, the tab's scroll area) once the page is
/// scrolled down (`offset`); a click takes the page to the top on the next frame ([`AffixPage::take_to_top`]; the user,
/// 2026-11-12). Not while a question is on show.
pub fn top_button(ctx: &egui::Context, page: &mut AffixPage, offset: f32, view: egui::Rect) {
    if offset < TOP_AFTER || page.ask.is_some() {
        return;
    }
    egui::Area::new(egui::Id::unique("affix-to-top"))
        .order(egui::Order::Foreground)
        .pivot(egui::Align2::RIGHT_BOTTOM)
        .fixed_pos(view.right_bottom() - Vec2::new(14.0, 10.0))
        .show(ctx, |ui| {
            let button = egui::Button::new(RichText::new(tr("\u{2191} Back to top")).small().strong().color(Color32::WHITE)).fill(ACCENT).corner_radius(12);
            if ui.add(button).clicked() {
                page.to_top = true;
                ctx.request_repaint();
            }
        });
}

/// Sentences graded on the page that the filter on show would now drop: kept listed where they were until the filter
/// or the search changes or the page is left, so a grade picked moves nothing below it (the user, 2026-11-10: the page
/// jumped as a grade was picked).
#[derive(Default)]
struct Kept {
    /// The sentences' indices in the page's table.
    list: Vec<usize>,
    /// Sentences graded on the page while new (learned, not graded): their "New" mark stays, so the card keeps its
    /// height (the user, 2026-11-10).
    new: Vec<usize>,
    /// The filter and search the list is of.
    of: Option<(Filter, String)>,
    /// The pass the page was last drawn on (a gap: it was left).
    drawn_pass: u64,
}

impl Kept {
    /// Forgets the sentences kept when the filter or the search changed or the page was not drawn on the pass before
    /// `pass` (left and come back to): the list is then drawn by the filter alone.
    fn update(&mut self, filter: Filter, search: &str, pass: u64) {
        let left = pass > self.drawn_pass + 1;
        if left || self.of.as_ref().is_none_or(|(f, s)| *f != filter || s != search) {
            self.list.clear();
            self.new.clear();
            self.of = Some((filter, search.to_string()));
        }
        self.drawn_pass = pass;
    }
}

/// One sentence with its three grade buttons, on a card of its own (a lighter ground, a rim and a gap below), so each
/// affix stands apart (a thin line between them did not, the user, 2026-11-08); true when the grade changed. `was_new`:
/// graded on the page while new, its "New" mark kept ([`Kept::new`]). `game`: the grade the game's colour frames it
/// with on this map (Grade::None: white, or not known), shown while the user has not graded it.
fn sentence_row(ui: &mut Ui, s: &Sentence, grades: &mut Grades, read: &BTreeSet<String>, was_new: bool, game: Grade) -> bool {
    let (text, other) = shown_text(&s.en, &s.zh, lang::zh());
    let now = grade_of(grades, s);
    // the card in its grade's colour (the user, 2026-11-08: "the colour"): a faint tint of it and a rim of it for danger
    // and warning, the plain card for affixes not framed. Not graded and coloured by the game on this map: a dashed rim
    // and a stripe of that colour on the plain card, so the game's colour and the user's grade tell apart (2026-11-12).
    let game = if now.is_none() { game } else { Grade::None };
    let (fill, rim) = match now {
        None | Some(Grade::None) => (theme::TRACK, if game == Grade::None { DIVIDER } else { Color32::TRANSPARENT }),
        Some(g) => (mix(theme::TRACK, colour(g), 0.16), colour(g)),
    };
    // no stripe down the left edge of a graded card: the user found it crowded the cards (2026-11-08)
    let card = egui::Frame::new().fill(fill).stroke(Stroke::new(1.0, rim)).corner_radius(6).inner_margin(egui::Margin::symmetric(8, 6));
    let shown = card.show(ui, |ui| {
        ui.set_width(ui.available_width());
        if was_new || is_new(s, grades, read) {
            // a short mark only (the user found extra lines under a sentence not needed); the hover says more. Until
            // graded it is framed in the game's colour on the map (the user's choice, 2026-11-09); wrapped on a narrow
            // window
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(tr("New")).small().strong().color(theme::GOOD)).on_hover_text(tr("Seen on a map, not graded yet"));
                ui.label(RichText::new(tr("By the game's colour")).small().strong().color(MUTED))
                    .on_hover_text(tr("Not graded yet: framed in the colour the game shows it in."));
            });
        }
        buttons_row(ui, &text, other, now, game)
    });
    if game != Grade::None {
        let (r, c) = (shown.response.rect.shrink(0.5), colour(game));
        let corners = [r.left_top(), r.right_top(), r.right_bottom(), r.left_bottom(), r.left_top()];
        ui.painter().extend(egui::Shape::dashed_line(&corners, Stroke::new(1.0, c), 5.0, 3.0));
        let stripe = egui::Rect::from_min_size(shown.response.rect.min, Vec2::new(3.0, shown.response.rect.height()));
        ui.painter().rect_filled(stripe, egui::CornerRadius { nw: 6, sw: 6, ne: 0, se: 0 }, c);
    }
    ui.add_space(4.0);
    shown.inner.is_some_and(|g| set_grade(grades, s, g))
}

/// The name of the game's colour that frames an affix as `g` (red a danger, orange a warning).
fn game_colour_name(g: Grade) -> &'static str {
    match g {
        Grade::Danger => tr("Game red"),
        Grade::Warn => tr("Game orange"),
        Grade::None => tr("Game colour"),
    }
}

/// An entry on the map not found in the list: its text as read on a plain card, with why it is not in the list and,
/// when the game's colour frames it (`game`), that grade marked as the game's (no grade buttons: it has no id to save
/// a grade under until the game log names it, see `affix::learned`).
fn unknown_row(ui: &mut Ui, text: &str, ambiguous: bool, cut: bool, game: Grade) {
    let (mark, why) = unknown_mark(ambiguous, cut);
    let card = egui::Frame::new().fill(theme::TRACK).stroke(Stroke::new(1.0, DIVIDER)).corner_radius(6).inner_margin(egui::Margin::symmetric(8, 6));
    card.show(ui, |ui| {
        ui.set_width(ui.available_width());
        // wrapped: the game's colour goes to a line of its own on a narrow window (it widened the card past the page)
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(mark).small().strong().color(MUTED)).on_hover_text(why);
            if game != Grade::None {
                ui.label(RichText::new(trf!("By the game's colour: {}", name(game))).small().strong().color(colour(game)))
                    .on_hover_text(tr("Framed in the colour the game shows it in until the affix list has it."));
            }
        });
        let shown = if cut { format!("{}\u{2026}", text.trim_end()) } else { text.to_string() };
        ui.add(egui::Label::new(RichText::new(shown).color(MUTED)).wrap()).on_hover_text(why);
    });
    ui.add_space(4.0);
}

/// An affix the game log names on entering the map that the list has no words for yet: its id on a plain card.
fn logged_row(ui: &mut Ui, id: u32) {
    let card = egui::Frame::new().fill(theme::TRACK).stroke(Stroke::new(1.0, DIVIDER)).corner_radius(6).inner_margin(egui::Margin::symmetric(8, 6));
    card.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new(trf!("Unknown affix (id {})", id)).color(MUTED))
            .on_hover_text(tr("The game log names it, but the affix list has no words for it yet."));
    });
    ui.add_space(4.0);
}

/// The mark of an entry not found, and its hover: cut off before unsure before not in the list (a cut entry is read
/// again when it shows whole; an unsure one is in the list, close to affixes of different grades).
fn unknown_mark(ambiguous: bool, cut: bool) -> (&'static str, &'static str) {
    if cut {
        (tr("Cut off"), tr("The panel's bottom cuts this affix off: it is read once it shows whole."))
    } else if ambiguous {
        (tr("Unsure"), tr("Reads like affixes of different grades: not framed."))
    } else {
        (tr("Not in the list"), tr("Not in the affix list yet: it is added from the game log once you enter this map."))
    }
}

/// What the live frames tell of the panel on show, said on the affix page ([`draw`]).
#[derive(Clone, Copy, Debug, Default)]
pub struct Hints {
    /// The panel is of another interface scale than the reader was measured at (nothing framed).
    pub odd_scale: bool,
    /// The game client language the panel reads as, when it is not the one set in Ascendant.
    pub client: Option<lang::Lang>,
    /// With no map page on show, whose list of affixes the page shows (`affix_live` `MapShown`); None: the page's.
    pub map: Option<MapTitle>,
}

/// Whose affixes the affix page lists with no map page on show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapTitle {
    /// The map entered (or the page just closed); `from_log`: as the game log names them, not as read.
    This { from_log: bool },
    /// An earlier map's, from this time (the hideout not heard since).
    Earlier { at: std::time::SystemTime },
}

/// A time of day in local time, "HH:MM".
fn clock(at: std::time::SystemTime) -> String {
    let secs = at.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let local = secs + super::report::local_offset(secs);
    format!("{:02}:{:02}", local.rem_euclid(86_400) / 3600, local.rem_euclid(3600) / 60)
}

/// The affix page's hint for a panel that reads as `read_as`, not the client language set in Ascendant (`draw`'s
/// `client`): where to set it, as the settings row is named in both languages.
fn client_hint(read_as: lang::Lang) -> &'static str {
    match read_as {
        lang::Lang::En => tr("The affix panel reads as English, but Ascendant's language is set to Traditional Chinese, so affixes are not read right. Set Settings > Language / 語言 to English."),
        lang::Lang::ZhTw => tr("The affix panel shows no English words: if the game client is in Traditional Chinese, set Settings > Language / 語言 to 繁體中文."),
    }
}

fn style_name(s: FrameStyle) -> &'static str {
    match s {
        FrameStyle::Outline => tr("A: frame"),
        FrameStyle::Bar => tr("B: bar and tint"),
    }
}

/// A small picture of the style: two lines of text, the first framed as danger and the second as a warning.
fn style_preview(ui: &mut Ui, style: FrameStyle) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 46.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 4.0, Color32::from_rgb(0x14, 0x17, 0x1d));
    for (n, grade) in [(0, Grade::Danger), (1, Grade::Warn)] {
        let c = colour(grade);
        let row = egui::Rect::from_min_size(rect.min + Vec2::new(12.0, 5.0 + n as f32 * 19.0), Vec2::new(rect.width() - 18.0, 17.0));
        // the "text": a few grey bars
        let mut x = row.left() + 8.0;
        for w in [30.0, 46.0, 22.0, 38.0] {
            if x + w > row.right() - 4.0 {
                break;
            }
            p.rect_filled(egui::Rect::from_min_size(egui::pos2(x, row.center().y - 2.5), Vec2::new(w, 5.0)), 2.0, Color32::from_gray(150));
            x += w + 5.0;
        }
        match style {
            FrameStyle::Outline => {
                p.rect_stroke(row, 0.0, Stroke::new(1.5, c), StrokeKind::Inside);
            }
            FrameStyle::Bar => {
                p.rect_filled(row, 0.0, Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 45));
                p.rect_filled(egui::Rect::from_min_size(row.min - Vec2::new(6.0, 0.0), Vec2::new(3.5, row.height())), 0.0, c);
            }
        }
    }
}

/// The sentence at the normal text size; grey with a note when only the other language's text is known.
fn sentence_label(ui: &mut Ui, text: &str, other: bool) {
    let rich = RichText::new(text).color(if other { MUTED } else { theme::TEXT });
    let label = ui.add(egui::Label::new(rich).wrap());
    if other {
        label.on_hover_text(tr("Only seen in the other language so far"));
    }
}

/// Buttons under the sentence, the grade on show filled in its colour: one click picks a grade, and the last, the
/// game's colour, is the default (its hover says so). No separate "default" line: the user found it not needed
/// (2026-11-08). `game`: the grade the game's colour frames it with on this map while not graded (Grade::None: none
/// shown), said after the sentence and dotted on the game's colour button. Returns the pick: a grade, or None for the
/// game's colour.
fn buttons_row(ui: &mut Ui, text: &str, other: bool, now: Option<Grade>, game: Grade) -> Option<Option<Grade>> {
    if game == Grade::None {
        sentence_label(ui, text, other);
    } else {
        ui.horizontal_wrapped(|ui| {
            sentence_label(ui, text, other);
            // whole, on the next line when it does not fit after the sentence
            let mark = RichText::new(format!("\u{25cf} {}", game_colour_name(game))).small().color(colour(game));
            ui.add(egui::Label::new(mark).wrap_mode(egui::TextWrapMode::Extend))
                .on_hover_text(tr("Not graded: framed in the colour the game shows it in on this map."));
        });
    }
    let mut picked = None;
    // wrapped: a narrow window (or a longer language) takes the last buttons to a line of their own
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 3.0;
        for g in ORDER {
            let on = now == Some(g);
            let rich = RichText::new(name(g)).small().color(if on { ink(g) } else { MUTED });
            // the buttons not picked keep a clear rim on the card's lighter ground, so they still read as buttons
            let button = egui::Button::new(rich).fill(if on { colour(g) } else { Color32::TRANSPARENT }).stroke(Stroke::new(1.0, if on { colour(g) } else { theme::BAR_IDLE }));
            if ui.add(button).clicked() {
                picked = Some(Some(g));
            }
        }
        let on = now.is_none();
        let fill = Color32::from_gray(190);
        let ink = if on { Color32::BLACK } else { MUTED };
        let font = egui::TextStyle::Small.resolve(ui.style());
        let mut label = egui::text::LayoutJob::default();
        if game != Grade::None {
            label.append("\u{25cf} ", 0.0, egui::TextFormat::simple(font.clone(), colour(game)));
        }
        label.append(tr("Game colour"), 0.0, egui::TextFormat::simple(font, ink));
        let button = egui::Button::new(label).fill(if on { fill } else { Color32::TRANSPARENT }).stroke(Stroke::new(1.0, if on { fill } else { theme::BAR_IDLE }));
        if ui.add(button).on_hover_text(tr("Default: framed in the colour the game shows it in.")).clicked() {
            picked = Some(None);
        }
    });
    picked
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sentence graded on the page stays listed under a filter that would now drop it, until the filter or the search
    /// changes or the page is left (a pass without it drawn).
    #[test]
    fn a_graded_sentence_stays_listed_until_the_filter_changes() {
        let danger = Filter { grades: [false, true, false, false], ..Filter::default() };
        let mut kept = Kept::default();
        kept.update(danger, "", 10);
        kept.list.push(3);
        kept.update(danger, "", 11);
        assert_eq!(kept.list, [3], "the next pass, the same filter: kept");
        kept.update(danger, "fire", 12);
        assert!(kept.list.is_empty(), "the search changed");
        kept.list.push(3);
        kept.update(Filter::default(), "fire", 13);
        assert!(kept.list.is_empty(), "the filter changed");
        kept.list.push(3);
        kept.update(Filter::default(), "fire", 14);
        assert_eq!(kept.list, [3]);
        kept.update(Filter::default(), "fire", 30);
        assert!(kept.list.is_empty(), "the page left and come back to");
        kept.new.push(5);
        kept.update(Filter::default(), "", 31);
        assert!(kept.new.is_empty(), "the New marks kept go with the rest");
    }

    /// An affix learned changes the page's table: the sentences kept listed go (their indices may have moved); a
    /// grade picked (set_learned with the same list) keeps them.
    #[test]
    fn an_affix_learned_forgets_what_is_kept() {
        let mut page = AffixPage::default();
        page.set_learned(&[]);
        page.kept.list.push(3);
        page.kept.new.push(3);
        page.set_learned(&[]);
        assert_eq!((page.kept.list.as_slice(), page.kept.new.as_slice()), (&[3][..], &[3][..]), "the same table: kept");
        page.set_learned(&[Learned { id: "5888888".into(), en: "#% a new affix".into(), zh: String::new() }]);
        assert!(page.kept.list.is_empty() && page.kept.new.is_empty(), "an affix learned: forgotten");
    }

    /// The client language's text when there is one, else the other one flagged; X for a rolled number.
    #[test]
    fn sentences_show_in_the_client_language() {
        assert_eq!(shown_text("#% additional Max Life", "", false), ("X% additional Max Life".to_string(), false));
        assert_eq!(shown_text("", "玩家 #% 祝福持續時間", false), ("玩家 X% 祝福持續時間".to_string(), true));
        assert_eq!(shown_text("#% additional Max Life", "怪物 #% 額外最大生命", true), ("怪物 X% 額外最大生命".to_string(), false));
        assert_eq!(shown_text("#% additional Max Life", "", true), ("X% additional Max Life".to_string(), true));
    }

    /// The search box shows its clear button only while it holds text; a click on it empties the box and leaves the
    /// keyboard in it.
    #[test]
    fn the_clear_button_empties_the_search_and_keeps_the_keyboard() {
        let ctx = egui::Context::default();
        let mut search = String::from("fire");
        let frame = |ctx: &egui::Context, search: &mut String, events: Vec<egui::Event>| {
            let input = egui::RawInput { events, screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 200.0))), ..Default::default() };
            let mut out = None;
            let mut output = ctx.run_ui(input, |ui| {
                out = Some(search_box(ui, search));
            });
            // nothing paints here: the font atlas's upload is dropped on purpose
            output.textures_delta.clear();
            out.expect("drawn")
        };
        let (_, button) = frame(&ctx, &mut search, Vec::new());
        let at = button.expect("a clear button while the box holds text").center();
        let press = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::default() };
        frame(&ctx, &mut search, vec![egui::Event::PointerMoved(at), press(true)]);
        let _ = frame(&ctx, &mut search, vec![press(false)]);
        assert!(search.is_empty(), "emptied");
        let (edit, button) = frame(&ctx, &mut search, Vec::new());
        assert!(button.is_none(), "no button on an empty box");
        assert!(edit.has_focus(), "the keyboard stays in the box");
    }

    /// The search finds a sentence by its shown text, ignoring case; an empty search finds all.
    #[test]
    fn search_the_shown_text() {
        assert!(matches("玩家受到致盲效果時", "致盲"));
        assert!(matches("玩家受到致盲效果時", ""));
        assert!(matches("chance to inflict Scorch on you", "scorch"));
        assert!(!matches("chance to inflict Scorch on you", "no such words"));
    }

    /// A pick is saved under the sentence's key only when it differs from the default; a learned sentence keeps any
    /// pick (its "New" mark goes, also for the grade already shown); a grade under another of its ids is replaced.
    #[test]
    fn picks_are_saved_under_the_key() {
        let d = Dict::builtin().with_learned(&[
            Learned { id: "5300010".into(), en: Dict::builtin().sentences()[0].en.clone(), zh: String::new() },
            Learned { id: "5300011".into(), en: "#% a brand new affix".into(), zh: String::new() },
        ]);
        let s = &d.sentences()[0];
        let other = Grade::Warn;
        let mut grades = Grades::from([("5300010".to_string(), other)]);
        assert_eq!(grade_of(&grades, s), Some(other));
        // the grade under the joined id moves to the key
        assert!(set_grade(&mut grades, s, Some(other)));
        assert_eq!(grades, Grades::from([(s.key().to_string(), other)]));
        // back to the game's colour: nothing saved
        assert!(set_grade(&mut grades, s, None));
        assert!(grades.is_empty() && grade_of(&grades, s).is_none());
        assert!(!set_grade(&mut grades, s, None));
        let new = d.sentences().last().unwrap();
        let read = BTreeSet::new();
        assert!(is_new(new, &grades, &read));
        assert!(set_grade(&mut grades, new, Some(Grade::None)));
        assert!(!is_new(new, &grades, &read));
        assert_eq!(grade_of(&grades, new), Some(Grade::None));
        assert!(!set_grade(&mut grades, new, Some(Grade::None)));
    }

    /// The preview's entries on the map: ids, texts not in the list, unsure ones and cut ones.
    #[test]
    fn preview_entries_on_the_map() {
        let mut page = AffixPage::default();
        let first = page.defaults.sentences()[0].key().to_string();
        page.preview_seen = vec![
            first.clone(),
            format!("{first}#danger"),
            "?Some new affix".into(),
            "~Like two".into(),
            "?Cut at the...".into(),
            "no-such-id".into(),
            "?Red bullet#danger".into(),
            "?Orange and cut...#warn".into(),
        ];
        assert_eq!(
            page.preview_seen(),
            [
                Seen::Known(0, Grade::None),
                Seen::Known(0, Grade::Danger),
                Seen::Unknown { text: "Some new affix".into(), ambiguous: false, cut: false, game: Grade::None },
                Seen::Unknown { text: "Like two".into(), ambiguous: true, cut: false, game: Grade::None },
                Seen::Unknown { text: "Cut at the".into(), ambiguous: false, cut: true, game: Grade::None },
                Seen::Unknown { text: "Red bullet".into(), ambiguous: false, cut: false, game: Grade::Danger },
                Seen::Unknown { text: "Orange and cut".into(), ambiguous: false, cut: true, game: Grade::Warn },
            ]
        );
        assert_eq!(unknown_mark(true, true).0, tr("Cut off"));
        assert_eq!(unknown_mark(true, false).0, tr("Unsure"));
        assert_eq!(unknown_mark(false, false).0, tr("Not in the list"));
    }

    /// Only the client language's sentences are listed; the leading number does not count in the order.
    #[test]
    fn listed_in_the_client_language_by_text() {
        let d = Dict::builtin();
        let en_only = d.sentences().iter().find(|s| s.zh.is_empty() && !s.en.is_empty()).expect("an English-only sentence");
        assert!(text_in(en_only, false).is_some());
        assert!(text_in(en_only, true).is_none());
        assert_eq!(sort_text("#% Max Fire Resistance for you"), "max fire resistance for you");
        assert_eq!(sort_text("一#% 玩家的腐蝕抗性上限"), "玩家的腐蝕抗性上限");
        assert_eq!(sort_text("X% additional Area Damage"), "additional area damage");
        assert!(sort_text("#% Max Cold Resistance") < sort_text("#% Max Fire Resistance"));
    }

    /// The filters (the user, 2026-11-12): grades picked match any of them, states picked any of them, a grade and a
    /// state both; nothing picked ("All") lists every sentence, and says nothing above the list.
    #[test]
    fn filters_pick_any_of_a_kind_and_both_kinds() {
        let pick = |names: &str| Filter::of_preview(names);
        let all = Filter::default();
        assert!(all.is_all() && all.said(9).is_none());
        for (grade, changed, new) in [(None, false, true), (Some(Grade::Danger), true, false), (Some(Grade::None), true, false)] {
            assert!(all.keeps(grade, changed, new), "all: {grade:?}");
        }
        // one kind: any of the grades picked
        let red_or_yellow = pick("danger,warn");
        assert!(red_or_yellow.keeps(Some(Grade::Danger), false, false) && red_or_yellow.keeps(Some(Grade::Warn), true, false));
        assert!(!red_or_yellow.keeps(Some(Grade::None), true, false) && !red_or_yellow.keeps(None, false, true));
        let game = pick("game");
        assert!(game.keeps(None, false, false) && !game.keeps(Some(Grade::None), true, false), "the game's colour is not graded");
        // any of the states picked
        let changed_or_new = pick("changed,new");
        assert!(changed_or_new.keeps(None, false, true) && changed_or_new.keeps(Some(Grade::Warn), true, false));
        assert!(!changed_or_new.keeps(None, false, false));
        // both kinds: both
        let changed_danger = pick("changed,danger");
        assert!(changed_danger.keeps(Some(Grade::Danger), true, false));
        assert!(!changed_danger.keeps(Some(Grade::Warn), true, false), "changed, not danger");
        assert!(!changed_danger.keeps(Some(Grade::Danger), false, false), "danger, not changed (a sentence graded is changed: never so, but the rule holds)");
        let new_or_changed_game = pick("game,new,changed");
        assert!(new_or_changed_game.keeps(None, false, true) && !new_or_changed_game.keeps(None, false, false));
        // said in words: the states, then the grades
        let or = tr(" or ");
        assert_eq!(changed_danger.said(1), Some(trf!("{} and {}: {} affixes", tr("Changed"), tr("Danger"), 1)));
        assert_eq!(red_or_yellow.said(3), Some(trf!("{}: {} affixes", format!("{}{or}{}", tr("Danger"), tr("Warning")), 3)));
        let both = trf!("{} and {}: {} affixes", format!("{}{or}{}", tr("Changed"), tr("New")), format!("{}{or}{}", tr("Game colour"), tr("Not framed")), 0);
        assert_eq!(pick("new,changed,game,none").said(0), Some(both));
    }

    /// A page with one learned affix (new) and one sentence of the table graded by the user.
    fn page_with_new() -> (AffixPage, usize) {
        let mut page = AffixPage::default();
        page.set_learned(&[Learned { id: "5888888".into(), en: "#% a brand new affix".into(), zh: "#% 全新的詞綴".into() }]);
        let i = page.defaults.sentences().iter().position(|s| s.ids == ["5888888"]).expect("learned");
        (page, i)
    }

    /// "Mark all as read": Cancel changes nothing; Confirm clears the "New" marks of the client language's affixes and
    /// keeps every grade (the affix is still framed by the game's colour); the marks stay read through a save of the
    /// settings and a new start (a new page, the same learned list).
    #[test]
    fn mark_all_as_read_asks_and_is_kept() {
        let (mut page, i) = page_with_new();
        let mut grades = Grades::from([("5111003".to_string(), Grade::Danger)]);
        let mut read = BTreeSet::new();
        let new = |page: &AffixPage, grades: &Grades, read: &BTreeSet<String>| is_new(&page.defaults.sentences()[i], grades, read);
        assert!(new(&page, &grades, &read));
        page.kept.new.push(i);
        page.ask = Some(Ask::ReadAll);
        assert!(!page.answer(false, &mut grades, &mut read, false), "Cancel");
        assert!(page.ask.is_none() && read.is_empty() && new(&page, &grades, &read) && page.kept.new == [i]);
        page.ask = Some(Ask::ReadAll);
        assert!(page.answer(true, &mut grades, &mut read, false), "Confirm");
        assert!(page.ask.is_none() && !new(&page, &grades, &read) && page.kept.new.is_empty());
        assert_eq!(read, BTreeSet::from(["5888888".to_string()]));
        assert_eq!(grades, Grades::from([("5111003".to_string(), Grade::Danger)]), "the grades stay");
        assert!(page.defaults.sentences()[i].user_grade(&grades).is_none(), "still the game's colour");
        // saved and started again
        let mut settings = super::super::settings::Settings { affix_grades: grades.clone(), affix_read: read.clone(), ..Default::default() };
        settings = serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        let (again, j) = page_with_new();
        assert!(!is_new(&again.defaults.sentences()[j], &settings.affix_grades, &settings.affix_read), "read after a new start");
        // nothing new left: Confirm changes nothing more
        page.ask = Some(Ask::ReadAll);
        assert!(!page.answer(true, &mut grades, &mut read, false));
    }

    /// "Reset every grade": Cancel keeps every grade; Confirm takes them all back to the game's colour (the affixes
    /// marked read stay read). No question on show: an answer does nothing.
    #[test]
    fn reset_every_grade_asks_first() {
        let (mut page, _) = page_with_new();
        let graded = Grades::from([("5111003".to_string(), Grade::Danger), ("5634003".to_string(), Grade::None)]);
        let mut grades = graded.clone();
        let mut read = BTreeSet::from(["5888888".to_string()]);
        assert!(!page.answer(true, &mut grades, &mut read, false), "no question on show");
        assert_eq!(grades, graded);
        page.ask = Some(Ask::Reset);
        assert!(!page.answer(false, &mut grades, &mut read, false), "Cancel");
        assert!(page.ask.is_none());
        assert_eq!(grades, graded);
        page.ask = Some(Ask::Reset);
        assert!(page.answer(true, &mut grades, &mut read, false), "Confirm");
        assert!(page.ask.is_none() && grades.is_empty());
        assert_eq!(read.len(), 1, "the read marks stay");
    }

    /// Leaving the page closes its question as Cancel (review of 1.3.0): drawn again after, no window shows and nothing
    /// changes, for either question.
    #[test]
    fn leaving_the_page_closes_its_question() {
        let ctx = egui::Context::default();
        for ask in [Ask::Reset, Ask::ReadAll] {
            let (mut page, i) = page_with_new();
            let graded = Grades::from([("5111003".to_string(), Grade::Danger)]);
            let mut grades = graded.clone();
            let mut read = BTreeSet::new();
            let (mut on, mut style) = (true, FrameStyle::default());
            page.ask = Some(ask);
            page.leave();
            let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 600.0))), ..Default::default() };
            let mut changed = false;
            let mut output = ctx.run_ui(input, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    changed = draw(ui, &mut page, &mut on, &mut style, &mut grades, &mut read, &[], Hints::default());
                });
            });
            output.textures_delta.clear();
            assert!(page.ask.is_none() && !changed, "{ask:?}");
            assert_eq!(grades, graded, "{ask:?}");
            assert!(read.is_empty() && is_new(&page.defaults.sentences()[i], &grades, &read), "{ask:?}");
        }
    }

    /// The question's window: Escape (or a click beside it) is Cancel; "Back to top" shows only once the page is
    /// scrolled down and not while a question is on show, and a click asks the scroll area to the top once.
    #[test]
    fn the_window_closes_as_cancel_and_back_to_top_goes_up_once() {
        let ctx = egui::Context::default();
        let (mut page, _) = page_with_new();
        let graded = Grades::from([("5111003".to_string(), Grade::Danger)]);
        let mut grades = graded.clone();
        let mut read = BTreeSet::new();
        let (mut on, mut style) = (true, FrameStyle::default());
        let mut frame = |page: &mut AffixPage, grades: &mut Grades, events: Vec<egui::Event>| {
            let input = egui::RawInput { events, screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 600.0))), ..Default::default() };
            let mut changed = false;
            let mut output = ctx.run_ui(input, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    changed = draw(ui, page, &mut on, &mut style, grades, &mut read, &[], Hints::default());
                });
            });
            output.textures_delta.clear();
            changed
        };
        page.ask = Some(Ask::Reset);
        frame(&mut page, &mut grades, Vec::new());
        assert_eq!(page.ask, Some(Ask::Reset), "on show");
        let escape = egui::Event::Key { key: egui::Key::Escape, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::default() };
        assert!(!frame(&mut page, &mut grades, vec![escape]));
        assert!(page.ask.is_none(), "closed");
        assert_eq!(grades, graded, "as Cancel");
        // back to top
        let view = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 600.0));
        let top = |page: &mut AffixPage, offset: f32, events: Vec<egui::Event>| {
            let input = egui::RawInput { events, screen_rect: Some(view), ..Default::default() };
            let mut output = ctx.run_ui(input, |ui| top_button(ui.ctx(), page, offset, view));
            output.textures_delta.clear();
            ctx.memory(|m| m.area_rect(egui::Id::unique("affix-to-top")))
        };
        assert!(top(&mut page, 0.0, Vec::new()).is_none(), "not scrolled: none");
        // the first frame of an area measures it, the next one places it
        top(&mut page, 500.0, Vec::new());
        let rect = top(&mut page, 500.0, Vec::new()).expect("scrolled down: shown");
        assert!(view.contains_rect(rect) && rect.right() > view.center().x && rect.bottom() > view.center().y, "at the bottom right: {rect:?}");
        let at = rect.center();
        let press = |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::default() };
        top(&mut page, 500.0, vec![egui::Event::PointerMoved(at), press(true)]);
        top(&mut page, 500.0, vec![press(false)]);
        assert!(page.take_to_top(), "clicked: to the top");
        assert!(!page.take_to_top(), "once");
    }
}
