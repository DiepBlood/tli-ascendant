//! The affix page (public since 1.2.0). Laid out to be easy to find, recognise and use
//! (the user, 2026-11-08):
//! - find: on the map page, the affixes on it now come first ("On this map", in the panel's order, those not found too:
//!   their text as read and why, not in the list yet, unsure or cut off); below, every affix
//!   of the client's language (the user: only the current language's affixes), dangers first, then warnings, then the
//!   rest, each group in the order of its text with the leading number left out, so affixes that start alike sit
//!   together; the order follows the default grades, so a row does not jump away when its grade is changed; the list
//!   can be narrowed by grade and searched;
//! - recognise: text at the normal size, each grade by its colour and its name;
//! - use: one click picks a grade (back to the game's colour too, the default), the changed ones can be listed alone,
//!   and resetting every grade asks for a second click.
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
use std::time::{Duration, Instant};

/// Which grades the list shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Filter {
    All,
    Only(Grade),
    /// The sentences the user changed.
    Changed,
    /// Affixes learned from the game log the user has not graded yet.
    New,
    /// The sentences framed by the game's colour (the user has not graded them).
    Game,
}

/// The page's own state.
pub struct AffixPage {
    search: String,
    filter: Filter,
    defaults: Dict,
    /// "Reset every grade" was clicked once: a second click before this does it.
    confirm_reset: Option<Instant>,
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

    /// Preview only: the entries shown as on the map, from `ASCENDANT_PREVIEW_AFFIX_SEEN`: items split by `|` (by `,`
    /// when there is no `|`), each a sentence's id, or text read that was not found: `?text` not in the list, `~text`
    /// unsure, `text...` (after `?` or `~`) cut off; `#danger` or `#warn` at the end, the grade its game colour frames
    /// it with.
    fn preview_seen(&self) -> Vec<Seen> {
        self.preview_seen
            .iter()
            .filter_map(|item| {
                let item = item.trim();
                let (ambiguous, text) = match item.strip_prefix('?') {
                    Some(text) => (false, text),
                    None => match item.strip_prefix('~') {
                        Some(text) => (true, text),
                        None => return self.defaults.sentences().iter().position(|s| s.ids.iter().any(|x| x == item)).map(Seen::Known),
                    },
                };
                let (text, game) = match (text.strip_suffix("#danger"), text.strip_suffix("#warn")) {
                    (Some(t), _) => (t, Grade::Danger),
                    (_, Some(t)) => (t, Grade::Warn),
                    _ => (text, Grade::None),
                };
                let (text, cut) = text.strip_suffix("...").map_or((text, false), |t| (t, true));
                Some(Seen::Unknown { text: text.to_string(), ambiguous, cut, game })
            })
            .collect()
    }
}

/// A learned affix the user has not graded yet: marked "New" on the page.
fn is_new(s: &Sentence, grades: &Grades) -> bool {
    s.learned && s.user_grade(grades).is_none()
}

impl Default for AffixPage {
    fn default() -> Self {
        let defaults = Dict::builtin();
        // preview only: the list's filter (danger, warn, none, changed)
        let filter = match std::env::var("ASCENDANT_PREVIEW_AFFIX_FILTER").as_deref() {
            Ok("danger") => Filter::Only(Grade::Danger),
            Ok("warn") => Filter::Only(Grade::Warn),
            Ok("none") => Filter::Only(Grade::None),
            Ok("changed") => Filter::Changed,
            Ok("new") => Filter::New,
            Ok("game") => Filter::Game,
            _ => Filter::All,
        };
        let preview_seen = std::env::var("ASCENDANT_PREVIEW_AFFIX_SEEN")
            .map(|v| v.split(if v.contains('|') { '|' } else { ',' }).filter(|s| !s.trim().is_empty()).map(str::to_string).collect())
            .unwrap_or_default();
        AffixPage {
            search: std::env::var("ASCENDANT_PREVIEW_AFFIX_SEARCH").unwrap_or_default(),
            filter,
            defaults,
            confirm_reset: None,
            preview_seen,
            preview_odd_scale: std::env::var("ASCENDANT_PREVIEW_AFFIX_ODD_SCALE").is_ok_and(|v| v == "1"),
            preview_client: std::env::var("ASCENDANT_PREVIEW_AFFIX_CLIENT").ok().map(|v| lang::Lang::from_key(&v)),
            kept: Kept::default(),
        }
    }
}

const ORDER: [Grade; 3] = [Grade::Danger, Grade::Warn, Grade::None];
/// The second click of "Reset every grade" has to come within this.
const CONFIRM_FOR: Duration = Duration::from_secs(4);

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
/// `client`: the game client language the panel reads as, when it is not the one set (said here, with where to set it).
pub fn draw(ui: &mut Ui, page: &mut AffixPage, on: &mut bool, style: &mut FrameStyle, grades: &mut Grades, seen: &[Seen], hints: Hints) -> bool {
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
                    Seen::Known(i) => {
                        if let Some(s) = sentences.get(*i) {
                            let was_new = is_new(s, grades);
                            if sentence_row(ui, s, grades, page.kept.new.contains(i)) {
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
    theme::heading(ui, tr("Every affix"));
    // the client language's sentences, by default grade, then by text (see the module notes)
    let zh = lang::zh();
    let rank = |g: Grade| ORDER.iter().position(|&o| o == g).unwrap_or(ORDER.len());
    let mut listed: Vec<(usize, String)> = (0..sentences.len()).filter_map(|i| text_in(&sentences[i], zh).map(|t| (i, sort_text(t)))).collect();
    listed.sort_by(|a, b| rank(sentences[a.0].grade).cmp(&rank(sentences[b.0].grade)).then_with(|| a.1.cmp(&b.1)));
    let counts = ORDER.map(|g| listed.iter().filter(|(i, _)| grade_of(grades, &sentences[*i]) == Some(g)).count());
    let game_n = listed.iter().filter(|(i, _)| grade_of(grades, &sentences[*i]).is_none()).count();
    let changed_n = listed.iter().filter(|(i, _)| sentences[*i].user_grade(grades).is_some()).count();
    let new_n = listed.iter().filter(|(i, _)| is_new(&sentences[*i], grades)).count();
    // the filters, with how many each shows
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let mut pick = |ui: &mut Ui, f: Filter, text: String, c: Option<Color32>| {
            let on = page.filter == f;
            let rich = RichText::new(text).small().color(if on { Color32::BLACK } else { c.unwrap_or(theme::TEXT) });
            let button = egui::Button::new(rich).fill(if on { c.unwrap_or(ACCENT) } else { Color32::TRANSPARENT }).stroke(Stroke::new(1.0, c.unwrap_or(DIVIDER)));
            if ui.add(button).clicked() {
                page.filter = f;
            }
        };
        pick(ui, Filter::All, trf!("All {}", listed.len()), None);
        pick(ui, Filter::Game, trf!("Game colour {}", game_n), Some(Color32::from_gray(190)));
        for (g, n) in ORDER.iter().zip(counts) {
            pick(ui, Filter::Only(*g), format!("{} {n}", name(*g)), Some(if *g == Grade::None { Color32::from_gray(150) } else { colour(*g) }));
        }
        if changed_n > 0 {
            pick(ui, Filter::Changed, trf!("Changed {}", changed_n), Some(ACCENT));
        }
        if new_n > 0 {
            pick(ui, Filter::New, trf!("New {}", new_n), Some(theme::GOOD));
        }
    });
    let _ = search_box(ui, &mut page.search);
    ui.add_space(4.0);
    let mut shown = 0;
    for (i, _) in &listed {
        let s = &sentences[*i];
        let keep = page.kept.list.contains(i)
            || match page.filter {
                Filter::All => true,
                Filter::Only(g) => grade_of(grades, s) == Some(g),
                Filter::Game => grade_of(grades, s).is_none(),
                Filter::Changed => s.user_grade(grades).is_some(),
                Filter::New => is_new(s, grades),
            };
        if !keep || !text_in(s, zh).is_some_and(|t| matches(t, &page.search)) {
            continue;
        }
        shown += 1;
        let was_new = is_new(s, grades);
        if sentence_row(ui, s, grades, page.kept.new.contains(i)) {
            changed = true;
            if !page.kept.list.contains(i) {
                page.kept.list.push(*i);
            }
            if was_new && !page.kept.new.contains(i) {
                page.kept.new.push(*i);
            }
        }
    }
    if shown == 0 {
        theme::line(ui, tr("No affix matches"), MUTED);
    }
    ui.add_space(8.0);
    // resetting every grade asks for a second click
    let armed = page.confirm_reset.is_some_and(|at| at.elapsed() < CONFIRM_FOR);
    let label = if armed { tr("Click again to reset every grade") } else { tr("Reset every grade to the default") };
    let reset = ui.add_enabled(!grades.is_empty(), egui::Button::new(RichText::new(label).small().color(if armed { theme::ERROR } else { theme::TEXT })));
    if reset.clicked() {
        if armed {
            grades.clear();
            page.confirm_reset = None;
            changed = true;
        } else {
            page.confirm_reset = Some(Instant::now());
        }
    }
    if armed {
        ui.ctx().request_repaint_after(CONFIRM_FOR);
    }
    changed
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
/// graded on the page while new, its "New" mark kept ([`Kept::new`]).
fn sentence_row(ui: &mut Ui, s: &Sentence, grades: &mut Grades, was_new: bool) -> bool {
    let (text, other) = shown_text(&s.en, &s.zh, lang::zh());
    let now = grade_of(grades, s);
    // the card in its grade's colour (the user, 2026-11-08: "the colour"): a faint tint of it and a rim of it for danger
    // and warning, the plain card for affixes not framed
    let (fill, rim) = match now {
        None | Some(Grade::None) => (theme::TRACK, DIVIDER),
        Some(g) => (mix(theme::TRACK, colour(g), 0.16), colour(g)),
    };
    // no stripe down the left edge: the user found it crowded the cards (2026-11-08)
    let card = egui::Frame::new().fill(fill).stroke(Stroke::new(1.0, rim)).corner_radius(6).inner_margin(egui::Margin::symmetric(8, 6));
    let picked = card
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            if was_new || is_new(s, grades) {
                // a short mark only (the user found extra lines under a sentence not needed); the hover says more. Until
                // graded it is framed in the game's colour on the map (the user's choice, 2026-11-09); wrapped on a narrow
                // window
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(tr("New")).small().strong().color(theme::GOOD)).on_hover_text(tr("Seen on a map, not graded yet"));
                    ui.label(RichText::new(tr("By the game's colour")).small().strong().color(MUTED))
                        .on_hover_text(tr("Not graded yet: framed in the colour the game shows it in."));
                });
            }
            buttons_row(ui, &text, other, now)
        })
        .inner;
    ui.add_space(4.0);
    picked.is_some_and(|g| set_grade(grades, s, g))
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
/// (2026-11-08). Returns the pick: a grade, or None for the game's colour.
fn buttons_row(ui: &mut Ui, text: &str, other: bool, now: Option<Grade>) -> Option<Option<Grade>> {
    sentence_label(ui, text, other);
    let mut picked = None;
    ui.horizontal(|ui| {
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
        let game = Color32::from_gray(190);
        let rich = RichText::new(tr("Game colour")).small().color(if on { Color32::BLACK } else { MUTED });
        let button = egui::Button::new(rich).fill(if on { game } else { Color32::TRANSPARENT }).stroke(Stroke::new(1.0, if on { game } else { theme::BAR_IDLE }));
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
        let danger = Filter::Only(Grade::Danger);
        let mut kept = Kept::default();
        kept.update(danger, "", 10);
        kept.list.push(3);
        kept.update(danger, "", 11);
        assert_eq!(kept.list, [3], "the next pass, the same filter: kept");
        kept.update(danger, "fire", 12);
        assert!(kept.list.is_empty(), "the search changed");
        kept.list.push(3);
        kept.update(Filter::All, "fire", 13);
        assert!(kept.list.is_empty(), "the filter changed");
        kept.list.push(3);
        kept.update(Filter::All, "fire", 14);
        assert_eq!(kept.list, [3]);
        kept.update(Filter::All, "fire", 30);
        assert!(kept.list.is_empty(), "the page left and come back to");
        kept.new.push(5);
        kept.update(Filter::All, "", 31);
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
        assert!(is_new(new, &grades));
        assert!(set_grade(&mut grades, new, Some(Grade::None)));
        assert!(!is_new(new, &grades));
        assert_eq!(grade_of(&grades, new), Some(Grade::None));
        assert!(!set_grade(&mut grades, new, Some(Grade::None)));
    }

    /// The preview's entries on the map: ids, texts not in the list, unsure ones and cut ones.
    #[test]
    fn preview_entries_on_the_map() {
        let mut page = AffixPage::default();
        let first = page.defaults.sentences()[0].key().to_string();
        page.preview_seen = vec![
            first,
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
                Seen::Known(0),
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
}
