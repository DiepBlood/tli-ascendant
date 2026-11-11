//! Window half of Vorax: hands the captured frames and the game log's cues to
//! [`VoraxSession`], pauses and resumes Tartarus's [`Session`] when Vorax mode is entered or left, draws
//! the advice boxes over the game, and draws the Vorax panel of the Board tab (plan sections 2.2, 3, 4.1, 4.2).
//!
//! Outside Vorax mode, apart from the Vorax log cue (`Signal::Vorax`), events go back to
//! ui.rs untouched and Tartarus carries on as before.
//!
//! The panel text follows the window language: every label is an English key looked up with `tr()` / `trf!`
//! (src/lang/zh_tw.rs has the Traditional Chinese). Game names (cards, instruments, monsters) come from the name
//! table, and the option names and the status line come from VoraxSession, already in the client's language.

use super::capture::{Event, Watcher, capture, game_in_front, game_rect};
use super::gamelog::Signal;
use super::mark::{self, Mark, Notice};
use super::session::Session;
use super::settings::Settings;
use super::theme::{self, ACCENT, DIVIDER, MUTED, Verdict};
use super::vorax_settle::{self, Arrival};
use super::vorax_session::{self, BoardFrom, BoxStyle, Config, Cue, GameSummary, ModeChange, Page, PageState, Status, View, VoraxSession};
use crate::lang::{tr, trf};
use crate::reader::Rgba;
use crate::vorax::AdviceRow;
use crate::vorax::names::rarity_name;
use crate::vorax::read::{self, Kind};
use ::vorax::Rarity;
use eframe::egui::{self, Align, Align2, Color32, FontId, Layout, RichText, Sense, Stroke, StrokeKind, Vec2};
use serde_json::json;
use std::time::Duration;

/// The last usable advice (`View::stale`) is drawn in grey.
const STALE: Color32 = Color32::GRAY;
/// The fixed credit line on the last row of the panel; the "About" section of the settings page uses it too (Chinese
/// UI).
pub const CREDIT: &str = "渴血求解器：vorax-solver-rs（MIT，DinoRaos、tidewarden、Moebing）";
/// The same credit line in English, used by "About" in the English UI.
/// The endgame battle's give-up hint ([`VoraxView::show_give_up`]).
const GIVE_UP_LINE: &str = "Score at its cap: give up to settle at once";

pub const CREDIT_EN: &str = "Vorax solver: vorax-solver-rs (MIT; DinoRaos, tidewarden, Moebing)";

/// What Vorax's live advice looks like in the window and over the game. There is only one in the whole program.
pub struct VoraxView {
    session: VoraxSession,
    /// The boxes over the game, created only when first needed. Does not share ui.rs's `mark`: Tartarus's
    /// `clicked` and `clicks` (see `mark::Clicks`) are tied to that one.
    marks: Vec<Mark>,
    /// The advised option's expected final activity on the game, above the first box (see [`advised_k`]), made
    /// when first needed.
    value: Option<Notice>,
    /// The capture window changed during Vorax mode: handed to [`Session::log_window`] on return to Tartarus.
    window: Option<String>,
    /// The content last written to the `vorax_view` row, see [`VoraxView::log_marks`].
    last_view: Option<Logged>,
    /// The reason boxes were to be drawn but are all hidden (see [`VoraxView::show_marks`]), logged to the
    /// `vorax_view` row.
    hidden: Option<&'static str>,
    /// Test only (ASCENDANT_PREVIEW_VORAX in ui.rs): preview with the frame fed in; re-reads do not capture the game.
    preview: bool,
    /// Test only (ASCENDANT_PREVIEW_BATTLE): the panel draws this fake endgame battle reading.
    preview_battle: Option<read::Battle>,
    /// Test only (ASCENDANT_PREVIEW_VORAX_RESULT): the result card draws this made-up finished game.
    preview_result: Option<GameSummary>,
    /// The `ended_at` of the result card a Tartarus reading took the place of (see [`VoraxView::dismiss_result`]):
    /// [`VoraxView::showing`] leaves that card out, so it cannot cover the Tartarus board.
    dismissed: Option<i64>,
    /// The capture window last described in any mode: written into the Vorax log on entering (or resuming) Vorax,
    /// since a size change while Vorax was paused went to Tartarus's log only.
    latest_window: Option<String>,
    /// Test only (ASCENDANT_PREVIEW_VORAX_PANEL): the panel draws this made-up view instead of the session's.
    preview_view: Option<View>,
    /// The endgame battle's give-up hint on the game ([`View::give_up`]): the line, a gold notice as Tartarus's at the
    /// top of the game, and the box on the Give Up button; made when first needed.
    give_up_notice: Option<Notice>,
    give_up_mark: Option<Mark>,
}

/// The content written to the `vorax_view` row, matching the fields written: the boxes drawn (what and `Mark::at()`),
/// plus best, solving, kind and round filled in by VoraxSession.
struct Logged {
    marks: Vec<(String, [i32; 4])>,
    best: Option<String>,
    solving: bool,
    kind: Option<Kind>,
    round: Option<i32>,
    hidden: Option<&'static str>,
}

impl VoraxView {
    /// Builds the VoraxSession from the settings; asks for a repaint when a background read or solve finishes.
    pub fn new(settings: &Settings, ctx: egui::Context) -> VoraxView {
        let mut session = VoraxSession::new(Config::from_settings(settings));
        session.on_progress(move || ctx.request_repaint());
        let mut view = VoraxView { session, marks: Vec::new(), value: None, window: None, last_view: None, hidden: None, preview: false, preview_battle: None, preview_result: None, dismissed: None, latest_window: None, preview_view: None, give_up_notice: None, give_up_mark: None };
        // ASCENDANT_PREVIEW_VORAX_PANEL=advice, none or blank: the panel draws a made-up board (see preview_view).
        if let Ok(which) = std::env::var("ASCENDANT_PREVIEW_VORAX_PANEL") {
            view.preview_panel(&which);
        }
        view
    }

    /// Test only (ASCENDANT_PREVIEW_VORAX_PANEL): the window shows the Vorax panel drawn from a made-up board
    /// ([`preview_view`]) instead of a read one; the session is not entered, and nothing is captured or read.
    fn preview_panel(&mut self, which: &str) {
        self.preview = true;
        self.preview_view = Some(preview_view(which));
    }

    /// The view the panel draws: the made-up one of a preview, else the session's.
    fn view(&self) -> &View {
        self.preview_view.as_ref().unwrap_or_else(|| self.session.view())
    }

    /// Test only (called only when ui.rs reads the environment variable ASCENDANT_PREVIEW_VORAX): enters Vorax mode
    /// directly and reads this frame twice (board steady) to measure the real program's Vorax panel. Later re-reads
    /// do not capture the game (see [`VoraxView::tick`]).
    pub fn preview_read(&mut self, frame: Rgba) {
        self.preview = true;
        let now = vorax_session::now_ms();
        self.session.note_cue(Cue::Page(Page::Main, PageState::Run), Some(now));
        self.session.read(frame.clone(), now, "preview");
        self.session.read(frame, now + 100, "preview");
    }

    /// Test only (ASCENDANT_PREVIEW_BATTLE=2 or full): the panel draws a fake endgame battle (wave 2/3, or all
    /// rewards taken).
    pub fn preview_battle(&mut self, which: &str) {
        let full = which == "full";
        self.preview_battle = Some(read::Battle {
            wave: Some(if full { (3, 3) } else { (2, 3) }),
            activity: Some(if full { 836_400.0 } else { 568_500.0 }),
            bonus: Some(if full { 381_900.0 } else { 221_200.0 }),
            next: Some(if full { 999_900_000.0 } else { 854_000.0 }),
            percent: Some(if full { 46 } else { 39 }),
            raw: String::new(),
        });
    }

    /// In Vorax mode (intro page, board, result screen), or showing the made-up panel of a preview.
    pub fn active(&self) -> bool {
        self.session.active() || self.preview_view.is_some()
    }

    /// The last finished Vorax game, kept after the mode ends until a new Vorax game starts or Tartarus is played (see
    /// [`VoraxSession::last_game`]).
    pub fn last_game(&self) -> Option<&GameSummary> {
        self.preview_result.as_ref().or_else(|| self.session.last_game())
    }

    /// Whether the Board tab shows Vorax: in Vorax mode (the live panel), or after a game with its result card, until
    /// a Tartarus reading dismisses that card (see [`VoraxView::dismiss_result`]). Only the Board tab uses it; frames
    /// and reads still go by [`VoraxView::active`].
    pub fn showing(&self) -> bool {
        self.active() || card_shows(self.last_game().map(|g| g.ended_at), self.dismissed)
    }

    /// Tartarus has a reading on show (a manual or an automatic read of the board succeeded): the result card on
    /// show, if any, gives way to the board for good. The card was cleared only by Tartarus's log signals or a
    /// Treasure or Day event, so with the log's light red, or auto read off and reads by hand, it covered the board.
    /// The game stays in [`VoraxView::last_game`] for the Run tab.
    pub fn dismiss_result(&mut self) {
        if let Some(at) = self.last_game().map(|g| g.ended_at) {
            self.dismissed = Some(at);
        }
    }

    /// Test only (ASCENDANT_PREVIEW_VORAX_RESULT=battle or none): a made-up finished game for the result card, with
    /// or without an endgame battle; no log folder, so nothing is settled.
    pub fn preview_result(&mut self, which: &str) {
        let battle = (which == "battle").then(|| read::Battle {
            wave: Some((3, 3)),
            activity: Some(812_300.0),
            bonus: Some(304_600.0),
            next: Some(1_120_000.0),
            percent: Some(37),
            raw: String::new(),
        });
        let board = if battle.is_some() { 812_345 } else { 655_120 };
        let from = if battle.is_some() { BoardFrom::ResultScreen } else { BoardFrom::LastBoard };
        // Made-up Chinese names in the Chinese window; in the English one the first three instruments of the name table.
        let instruments = if crate::lang::zh() {
            ["\u{7cbe}\u{5bc6}\u{93b7}\u{5b50}", "\u{6d88}\u{6bd2}\u{9257}", "\u{9aa8}\u{92f8}"].iter().map(|s| s.to_string()).collect()
        } else {
            crate::vorax::names::names().cards.iter().filter(|c| c.category == crate::vorax::names::Category::Instrument).take(3).map(|c| c.en.clone()).collect()
        };
        self.preview_result = Some(GameSummary { log_dir: None, ended_at: vorax_session::now_ms(), board: Some(board), board_from: Some(from), rounds: 13, instruments, battle });
    }

    /// A receiver that gets a message once the Vorax log has written everything sent so far (None when logging is
    /// off), for the settlement to wait on.
    pub fn log_flushed(&self) -> Option<std::sync::mpsc::Receiver<()>> {
        self.session.log().map(|l| l.flushed())
    }

    /// The window language was switched: the Vorax log's window row says the new OCR language from here on.
    pub fn log_language(&mut self) {
        self.session.log_language();
    }

    /// Reading or solving (the window shows a spinner).
    pub fn busy(&self) -> bool {
        self.session.busy()
    }

    /// The settings changed (ui.rs forwards here every time it saves settings).
    pub fn configure(&mut self, config: Config) {
        self.session.configure(config);
    }

    /// One event from the capture thread: Vorax log cues are always intercepted and given to VoraxSession, pausing or
    /// resuming Tartarus's `session` on a switch; in Vorax mode frames go to VoraxSession and Tartarus events are
    /// intercepted too. Everything else is returned unchanged for Tartarus to handle as before.
    pub fn route(&mut self, event: Event, session: &mut Session, watcher: Option<&Watcher>) -> Option<Event> {
        // On a Tartarus signal, drop the result card of the last finished Vorax game (last_game). A paused game is
        // kept as it is: going back to Tartarus no longer ends it (only Fight, GiveUp or a new game does).
        let tartarus = matches!(&event, Event::Game(s, Some(_), _) if !matches!(s, Signal::Vorax(_))) || matches!(event, Event::Treasure | Event::Day { .. });
        self.session.drop_paused(vorax_session::now_ms(), tartarus);
        if let Event::Window(window) = &event {
            self.latest_window = Some(window.clone());
        }
        // Vorax log cues: whether or not in the mode, they only go to note_cue and not to Tartarus (so Tartarus's
        // run log gets no extra game rows). stamp is the log line's time (UTC ms); None means it was found by
        // looking back at startup, the same convention as note_cue.
        if let Event::Game(Signal::Vorax(cue), stamp, _) = &event {
            match self.session.note_cue(*cue, *stamp) {
                ModeChange::Entered => self.enter(session),
                ModeChange::Left => self.leave(session, watcher),
                ModeChange::None => {}
            }
            return None;
        }
        if !self.session.active() {
            return Some(event);
        }
        match event {
            Event::Frame(frame, _) => {
                self.session.read(frame, vorax_session::now_ms(), "auto");
                None
            }
            Event::Ahead(frame) => {
                self.session.read(frame, vorax_session::now_ms(), "ahead");
                None
            }
            // Only remember it (plan section 2.2): Vorax's log writes window rows, handed to
            // Session::log_window on return to Tartarus.
            Event::Window(window) => {
                self.session.log_window(&window);
                self.window = Some(window);
                None
            }
            // The capture state has nothing to do with Tartarus (the window shows "auto read waiting"); pass it back
            // as usual.
            Event::Status(_) => Some(event),
            // A Tartarus log signal arrived in Vorax mode: deadlock guard (e.g. the page cue for leaving Vorax was
            // missed, yet the game is already back in the prison). When VoraxSession decides it has left, return to
            // Tartarus and hand the signal to Tartarus as usual; if it has not left, drop it. stamp is already an
            // Option (the log line's UTC ms), passed on as is.
            Event::Game(signal, stamp, _) => {
                if self.session.note_other_signal(&signal, stamp) == ModeChange::Left {
                    self.leave(session, watcher);
                    return Some(event);
                }
                None
            }
            // The prison's treasure and day count exist only in Tartarus: like the log signals above, treat them as
            // leaving, and after leaving pass them to Tartarus as usual (so signals such as [S16LastDay] are not lost
            // when Vorax was entered by mistake). The day counts looked back at startup are sent before the look-back
            // for Vorax, so everything received here is a live line.
            Event::Treasure | Event::Day { .. } => {
                let what = if matches!(event, Event::Treasure) { "treasure" } else { "day" };
                if self.session.note_tartarus(what, Some(vorax_session::now_ms())) == ModeChange::Left {
                    self.leave(session, watcher);
                    return Some(event);
                }
                None
            }
            // Observe-mode frames: not given to Tartarus in Vorax mode.
            Event::Observe(..) => None,
            // The riot battle's boss goes to the Tartarus run log whatever the mode: it does not leave Vorax.
            Event::RiotBoss(_) | Event::RiotEnd(_) => Some(event),
            // The map pages go to the affix samples whatever the mode (a map page is never open in Vorax mode).
            Event::MapLog(..) | Event::MapShot { .. } => Some(event),
        }
    }

    /// Enter Vorax mode: pause Tartarus. Tartarus's boxes, tooltips and chain numbers are hidden by the logic in ui.rs
    /// on every tick in Vorax mode.
    fn enter(&mut self, session: &mut Session) {
        session.suspend();
        self.window = None;
        self.last_view = None;
        // The window as it is now, in the Vorax folder too (a resumed game's size may have changed while paused).
        if let Some(window) = &self.latest_window {
            self.session.log_window(window);
        }
    }

    /// Leave Vorax mode: take down the Vorax boxes (logic no longer calls tick), resume Tartarus, write
    /// the window rows for the Vorax period, and clear the "same" fingerprint capture recorded during Vorax.
    fn leave(&mut self, session: &mut Session, watcher: Option<&Watcher>) {
        self.hide_all();
        session.resume();
        if let Some(window) = self.window.take() {
            session.log_window(&window);
        }
        if let Some(w) = watcher {
            w.report(true, false);
        }
    }

    /// Force a leave when capture is stopped or rebuilt (auto read turned off, window title changed): afterwards this
    /// Watcher's log cues no longer arrive, and Vorax mode might never see the page cue for leaving. Resume Tartarus,
    /// take down the boxes, write the window rows for the Vorax period. VoraxSession returns to Off (writes one
    /// `vorax_mode` row, with cue `why`), and `active()` is false afterwards.
    pub fn force_leave(&mut self, session: &mut Session, why: &str) {
        self.session.force_off(why);
        self.leave(session, None);
    }

    /// Called by ui.rs on every tick outside Vorax mode: if boxes are still drawn, take them all down (e.g. leftovers
    /// after leaving). Does nothing when the boxes are already hidden.
    pub fn hide_marks(&mut self) {
        if self.marks.iter().any(Mark::shown) || self.value.as_ref().is_some_and(Notice::shown) || self.give_up_shown() {
            self.hide_all();
        }
    }

    /// Take down all boxes and write one empty marks row (not written when identical to the previous row).
    fn hide_all(&mut self) {
        self.marks.iter_mut().for_each(Mark::hide);
        if let Some(v) = &mut self.value {
            v.hide();
        }
        self.hide_give_up();
        // This is taking the boxes down on leaving Vorax mode, not hidden by show_marks.
        self.hidden = None;
        self.log_marks(vorax_session::now_ms());
    }

    /// Read hotkey or read button: hand this frame to VoraxSession.
    pub fn read_hotkey(&mut self, frame: Rgba) {
        self.session.read(frame, vorax_session::now_ms(), "hotkey");
    }

    /// Called once per repaint in Vorax mode: collect background results, confirm re-reads, report capture, draw the
    /// boxes over the game and log changes to the boxes.
    pub fn tick(&mut self, ctx: &egui::Context, watcher: Option<&Watcher>, settings: &Settings) {
        let now = vorax_session::now_ms();
        if self.session.poll(now) {
            ctx.request_repaint();
        }
        // The test preview does not re-read (no game capture).
        if self.session.take_read_next() && !self.preview {
            match watcher {
                Some(w) => w.read_next(),
                // Auto read is off: capture one directly, like ui.rs's read_now.
                None => {
                    if let Ok(frame) = capture(&settings.window_title) {
                        self.session.read(frame, now, "auto");
                    }
                }
            }
        }
        // The endgame battle reads the panel once a second: the agreed time is handed to auto read; with auto read
        // off, repaint at that time and poll will ask for a frame.
        if let Some(after) = self.session.wants_read_in()
            && !self.preview
        {
            if let Some(w) = watcher {
                w.read_in(after);
            }
            ctx.request_repaint_after(after + Duration::from_millis(vorax_session::READ_IN_LATE_MS));
        }
        if let Some((news, off_board)) = self.session.take_news()
            && let Some(w) = watcher
        {
            w.report(news, off_board);
        }
        self.show_marks(ctx, settings);
        self.show_give_up(ctx, settings);
        self.log_marks(now);
    }

    /// The give-up hint is on the game.
    fn give_up_shown(&self) -> bool {
        self.give_up_notice.as_ref().is_some_and(Notice::shown) || self.give_up_mark.as_ref().is_some_and(Mark::shown)
    }

    fn hide_give_up(&mut self) {
        if let Some(n) = &mut self.give_up_notice {
            n.hide();
        }
        if let Some(m) = &mut self.give_up_mark {
            m.hide();
        }
    }

    /// The endgame battle's give-up hint ([`View::give_up`]) with the setting on and the game in front: the line as
    /// Tartarus's notice, centred at the top of the game (as ui.rs `show_notice`), and the Give Up button boxed as an
    /// advised card is ([`VoraxView::show_marks`]: frame pixels plus the client area's top-left, `unit = height /
    /// 1080`). Everything else hides it.
    fn show_give_up(&mut self, ctx: &egui::Context, settings: &Settings) {
        let title = &settings.window_title;
        let b = self.session.view().give_up.clone().filter(|_| settings.vorax_give_up_hint);
        if b.is_some() {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
        let rect = b.as_ref().filter(|_| game_in_front(title)).and_then(|_| game_rect(title).ok());
        let (Some(b), Some(rect @ [left, top, width, height])) = (b, rect) else {
            self.hide_give_up();
            return;
        };
        let unit = height as f32 / 1080.0;
        if self.give_up_notice.is_none() {
            self.give_up_notice = Notice::new();
        }
        if let Some(n) = &mut self.give_up_notice {
            n.show(tr(GIVE_UP_LINE), left as f32 + width as f32 / 2.0, top as f32 + height as f32 * 8.0 / 1080.0, unit, width as f32 * 0.6);
        }
        if self.give_up_mark.is_none() {
            self.give_up_mark = Mark::new();
        }
        if let Some(m) = &mut self.give_up_mark {
            let (x, y, half_w, half_h) = b.at;
            if m.show([left as f32 + x, top as f32 + y, half_w, half_h, unit], mark::Style::Plain, rect) {
                ctx.request_repaint();
            }
        }
    }

    /// Draw the boxes over the game according to `View::boxes`, with the same coordinate conversion as ui.rs's
    /// `mark_spot`: frame pixels plus the top-left of the game client area, `unit = height / 1080`, cropped to the
    /// game's extent. Everything is hidden when there are no boxes, the setting turns marks off, or the game is not
    /// in the foreground. With "Show the expected value on the game" on, the advised option's expected final activity
    /// ([`advised_k`], the number the panel shows) goes just above the first box (the card or the reroll), whether
    /// the boxes are on or not; it goes with the boxes, and when the advice is stale.
    fn show_marks(&mut self, ctx: &egui::Context, settings: &Settings) {
        let title = &settings.window_title;
        let view = self.session.view();
        let boxes = &view.boxes;
        let value = if settings.show_value { advised_k(view, self.always_take()) } else { None };
        let wanted = !boxes.is_empty() && (settings.highlight || value.is_some());
        // While boxes are to be drawn, look again every 50 ms whether or not the game is in the foreground, so window
        // moves and the game returning to the foreground are followed (same as highlight in ui.rs).
        if wanted {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
        let front = wanted && game_in_front(title);
        let rect = if front { game_rect(title).ok() } else { None };
        // Record the reason when there are boxes but nothing is drawn: in one 2026-11-03 case the boxes vanished
        // while reads were all normal, and it was impossible to tell whether the setting turned marks off, the game
        // was briefly not in the foreground, or the game's position could not be obtained.
        self.hidden = match (boxes.is_empty(), settings.highlight, front, rect.is_some()) {
            (true, ..) | (false, true, true, true) => None,
            (false, false, ..) => Some("off"),
            (false, true, false, _) => Some("behind"),
            (false, true, true, false) => Some("rect"),
        };
        let Some(rect @ [left, top, _, height]) = rect else {
            self.marks.iter_mut().for_each(Mark::hide);
            if let Some(v) = &mut self.value {
                v.hide();
            }
            return;
        };
        let unit = height as f32 / 1080.0;
        let style = |b: &vorax_session::MarkBox| match b.style {
            BoxStyle::Plain => mark::Style::Plain,
            BoxStyle::Loud => mark::Style::Loud,
        };
        if settings.highlight {
            while self.marks.len() < boxes.len() {
                let Some(m) = Mark::new() else { break };
                self.marks.push(m);
            }
        }
        for (k, m) in self.marks.iter_mut().enumerate() {
            match boxes.get(k).filter(|_| settings.highlight) {
                Some(b) => {
                    let (x, y, half_w, half_h) = b.at;
                    // While a box is zooming in, redraw on every tick.
                    if m.show([left as f32 + x, top as f32 + y, half_w, half_h, unit], style(b), rect) {
                        ctx.request_repaint();
                    }
                }
                None => m.hide(),
            }
        }
        match (value, boxes.first()) {
            (Some(text), Some(b)) => {
                if self.value.is_none() {
                    self.value = Notice::value();
                }
                if let Some(v) = &mut self.value {
                    let (x, y, _, half_h) = b.at;
                    let (x, y) = (left as f32 + x, top as f32 + y);
                    let (above, below) = mark::reach(half_h, unit, style(b));
                    v.show_value(&text, x, (y - above, y + below), top as f32, unit);
                }
            }
            _ => {
                if let Some(v) = &mut self.value {
                    v.hide();
                }
            }
        }
    }

    /// Write a `vorax_view` row only when the content to write changed: `marks` is each drawn box
    /// `{what, at: Mark::at()}`, and best, solving, kind and round are filled in by VoraxSession.
    /// Called on every tick, so compare in place first and allocate nothing when unchanged.
    fn log_marks(&mut self, now: i64) {
        let view = self.session.view();
        let shown = || view.boxes.iter().zip(&self.marks).filter_map(|(b, m)| m.at().map(|at| (b.what.as_str(), at)));
        let best = view.advice.as_ref().map(|a| a.headline.as_str());
        let solving = view.status == Status::Solving;
        let same = self.last_view.as_ref().is_some_and(|l| {
            l.best.as_deref() == best
                && l.solving == solving
                && l.kind == view.kind
                && l.round == view.round
                && l.hidden == self.hidden
                && l.marks.iter().map(|(w, at)| (w.as_str(), *at)).eq(shown())
        });
        if same {
            return;
        }
        let marks: Vec<(String, [i32; 4])> = shown().map(|(w, at)| (w.to_string(), at)).collect();
        let logged = Logged { best: best.map(str::to_string), solving, kind: view.kind, round: view.round, hidden: self.hidden, marks };
        let fields = json!({
            "marks": logged.marks.iter().map(|(w, at)| json!({ "what": w, "at": at })).collect::<Vec<_>>(),
            "hidden": logged.hidden,
            // Which window was in front instead of the game (diagnostics, see `capture::front_window`).
            "front": if logged.hidden == Some("behind") { super::capture::front_window() } else { None },
        });
        self.session.log_view(fields, now);
        self.last_view = Some(logged);
    }

    /// The panel for the Board tab in Vorax mode (plan section 4.1). The layout matches Tartarus (the option A the
    /// user picked on 2026-11-04): progress bars for round and final activity, remaining rerolls, the large advice
    /// text and the numbers of the top three, small squares for the six tanks and the held instruments; the full
    /// ranking and the board text that was read are in an expandable section. When `compact`, draws the compact
    /// version (when the window is small, see `next_compact` in ui.rs). Line 1 of the compact version is drawn by
    /// ui.rs's status line (merged into it, see [`VoraxView::compact_head`]).
    /// At the bottom, the switch of the endgame battle's give-up hint (`give_up`, settings `vorax_give_up_hint`; the
    /// player asked for it on this panel, 2026-11-09): true when it was switched, for ui.rs to save.
    pub fn panel(&self, ui: &mut egui::Ui, compact: bool, give_up: &mut bool) -> bool {
        self.panel_body(ui, compact);
        ui.add_space(6.0);
        let hover = tr("Vorax endgame battle: once the score reaches its cap (1.12M, every reward tier taken), a line on the game says giving up settles at once, and the Give Up button is boxed. More battle adds nothing then");
        ui.add(theme::Switch::new(give_up, tr("Vorax: hint to give up at the score cap"))).on_hover_text(hover).changed()
    }

    fn panel_body(&self, ui: &mut egui::Ui, compact: bool) {
        if !self.active()
            && let Some(g) = self.last_game()
        {
            result_panel(g, ui, compact);
            return;
        }
        let injected;
        let v = match &self.preview_battle {
            Some(b) => {
                injected = with_battle(self.view(), b);
                &injected
            }
            None => self.view(),
        };
        let always = self.always_take();
        if compact {
            compact_body(v, ui, always);
        } else {
            draw_panel(v, ui, always);
        }
    }

    /// The advice on show is a card always taken (see [`always_taken`]), from the advice for the board on show.
    fn always_take(&self) -> bool {
        always_taken(self.session.state().map(|x| x.hand.as_slice()), self.session.advice().map(|a| &a.best))
    }

    /// The header of line 1 in the compact version, `渴血 · 第 R 回合 / 13` (Vorax, round R of 13): ui.rs merges it
    /// with the status line into one line (header on the left, status on the right), with no blank line above.
    /// Remaining rerolls are not on this line (when click-through is on the status line always has text and would
    /// squeeze it out), and go to the right of the held-instruments row instead (see [`held_row`]).
    pub fn compact_head(&self) -> String {
        if !self.active() && self.last_game().is_some() {
            return tr("Vorax \u{b7} game over").to_string();
        }
        head_text(self.view())
    }
}

/// The body of [`VoraxView::panel`]: looks only at [`View`] and does not touch capture (also used when previewing the
/// layout offline).
///
/// Fixed skeleton (2026-11-05, same as the Tartarus board page): from top to bottom always the header
/// `渴血 · 第 R 回合 / 13` (1 line), the progress bar of total activity against 840k, remaining rerolls (fixed 1
/// line, blank when none), the advice card (fixed height, see [`advice_card`]), the six tanks in a row as in the game
/// (always 6 cells of fixed height, the advised card's tanks marked, see [`tank_strip`]; 2026-11-08 the user: the
/// panel must show which tank the advised card acts on), the `選項｜水箱｜達 840k｜平均` columns (option | tanks |
/// reaches 840k | average) and the top three (fixed 3 rows), the held instruments (fixed 1 line, truncated when too many), warnings and status (fixed 2 lines, flicker guard as
/// in Tartarus, see [`theme::notes`]), then the expandable full ranking and the board that was read. Below the top
/// three is another small line for ranks 4-6 (with 5 cards or 6 options all of them are visible). The credit line
/// ([`CREDIT`]) has been only in the settings "About" since 2026-11-05, and is no longer written at the bottom of the
/// panel (the user asked). While waiting for a new hand, while resolving, during a protocol, or when the board is
/// unreadable, i.e. nothing with advice or ranking, each block still keeps its usual place and is just left blank.
/// The column widths of the top three are computed once at the start from the panel width ([`Cols`]), shared by the
/// header and every row, so they align at window width 200 too.
pub fn draw_panel(v: &View, ui: &mut egui::Ui, always: bool) {
    // Column spacing 4 (narrower than the default), so the name column gets a bit more room at window width 200.
    let cols = Cols::new(ui.available_width(), 4.0);
    let head = match v.round {
        Some(round) => trf!("Vorax \u{b7} Round {} / {}", round, ::vorax::game::LAST_ROUND),
        None => tr("Vorax").to_string(),
    };
    theme::line(ui, &head, MUTED);
    let target = ::vorax::value::TARGET;
    match v.ultimate {
        Some(ultimate) => {
            let colour = if ultimate >= target { theme::GOOD } else { ACCENT };
            progress(ui, tr("Activity"), ultimate as f32 / target as f32, format!("{} / {}", short_k(ultimate), short_k(target)), colour)
                .on_hover_text(trf!("Final activity {} (target {})", grouped(ultimate), grouped(target)));
        }
        // No activity read yet: an empty bar (fill same as the track), the position is kept.
        None => {
            progress(ui, tr("Activity"), 0.0, format!("\u{2014} / {}", short_k(target)), theme::TRACK);
        }
    }
    rerolls(ui, v);
    ui.add_space(4.0);
    advice_card(ui, v);
    let rows = advice_rows(v);
    // The six tanks in a row as in the game, the ones the advised card acts on marked (2026-11-08 layout), above the
    // ranking whose tanks column draws them small.
    ui.add_space(2.0);
    tank_strip(ui, &v.tanks, advised_targets(rows), v.stale, false);
    ui.add_space(2.0);
    ranking_rows(ui, &cols, rows, v.stale, always, Some(&v.tanks));
    rest_line(ui, rows);
    held(ui, &v.held);
    theme::notes(ui, egui::Id::unique("vorax-notes"), &now_items(v), &note_items(v));
    // The two blocks the user expands themselves: the header is always there (`—` when there is no content), so the
    // credit line does not jump up and down.
    egui::CollapsingHeader::new(tr("Full ranking")).id_salt("vorax-ranking").show(ui, |ui| {
        if rows.is_empty() {
            ui.label(RichText::new("\u{2014}").small().color(MUTED));
        } else {
            ranking_table(ui, rows, v.stale);
        }
    });
    egui::CollapsingHeader::new(tr("Board as read")).id_salt("vorax-board").show(ui, |ui| match &v.board {
        Some(board) => ui.label(RichText::new(board).small().monospace()),
        None => ui.label(RichText::new("\u{2014}").small().color(MUTED)),
    });
}

/// The compact Vorax panel (when the window is small; the layout diagram the user confirmed, target 421x351,
/// everything fits at the standard font size): line 1 has the header on the left and the short remaining-rerolls on
/// the right; the activity progress bar; a one-line advice card with a label and one small line under it; then, as in
/// the full version since 2026-11-08 (the user: the compact panel was still the old one), the six tanks in a row with
/// the advised card's tanks framed in gold (short cells, see [`tank_strip`]), the `選項｜水箱｜達 840k｜平均` columns
/// (option | tanks | reaches 840k | average) and the top three across the whole width, one line for ranks 4-6 and one
/// line of held instruments with the remaining rerolls on the right; at the bottom the warning area (fixed 2 lines, flicker guard) and two links `全部排名 ▸　讀到的盤面 ▸` (full
/// ranking, board read), which expand below when clicked (scrollable when expanded; the `＋／－` after a link means
/// collapsed / expanded, since Microsoft JhengHei UI has no glyph for symbols like ▸). The credit line is only in
/// "About". Every block has a fixed height, same as the full version.
pub fn draw_panel_compact(v: &View, ui: &mut egui::Ui, always: bool) {
    let (head, rerolls) = (head_text(v), rerolls_text(v));
    let h = ui.text_style_height(&egui::TextStyle::Small);
    ui.horizontal(|ui| {
        let w = ui.available_width();
        ui.allocate_ui_with_layout(Vec2::new(w * 0.45, h), Layout::left_to_right(Align::Center), |ui| theme::line(ui, &head, MUTED));
        theme::line_aligned(ui, &rerolls, MUTED, true);
    });
    compact_body(v, ui, always);
}

/// The Vorax header `渴血 · 第 R 回合 / 13` (Vorax, round R of 13).
fn head_text(v: &View) -> String {
    match v.round {
        Some(round) => trf!("Vorax \u{b7} Round {} / {}", round, ::vorax::game::LAST_ROUND),
        None => tr("Vorax").to_string(),
    }
}

/// The short remaining-rerolls form `藥劑重擲 2 · 用具重擲 ?` (potion rerolls 2, instrument rerolls ?); an empty
/// string when not seen yet.
fn rerolls_text(v: &View) -> String {
    let count = |n: Option<i32>| n.map_or_else(|| "?".to_string(), |n| n.to_string());
    if v.potion_rerolls.is_some() || v.instrument_rerolls.is_some() {
        format!("{} \u{b7} {}", trf!("Potion rerolls {}", count(v.potion_rerolls)), trf!("Instrument rerolls {}", count(v.instrument_rerolls)))
    } else {
        String::new()
    }
}

/// Below line 1 of the compact version: the activity progress bar, the advice card (only the large text and the
/// label) and one small line right under the card (reason first, then the solve note), the six tanks in a row, the
/// top three with the tanks column, ranks 4-6, the held instruments, the warning area, and the two expand links.
fn compact_body(v: &View, ui: &mut egui::Ui, always: bool) {
    let target = ::vorax::value::TARGET;
    match v.ultimate {
        Some(ultimate) => {
            let colour = if ultimate >= target { theme::GOOD } else { ACCENT };
            progress(ui, tr("Activity"), ultimate as f32 / target as f32, format!("{} / {}", short_k(ultimate), short_k(target)), colour)
                .on_hover_text(trf!("Final activity {} (target {})", grouped(ultimate), grouped(target)));
        }
        None => {
            progress(ui, tr("Activity"), 0.0, format!("\u{2014} / {}", short_k(target)), theme::TRACK);
        }
    }
    let (big, small) = card_text(v);
    let brief = card_brief(v);
    let colour = match &v.advice {
        // The battle card's large text is white (there is no advice during battle).
        _ if in_battle(v) => Color32::WHITE,
        Some(_) if v.stale => STALE,
        Some(_) => Color32::WHITE,
        None => MUTED,
    };
    // During battle view.advice may still hold the last move's advice from round 13: the battle card does not carry
    // its verdict label and is not greyed by it.
    let battle = in_battle(v);
    let (fill, edge) = if v.stale && !battle { (theme::PANEL, DIVIDER) } else { (theme::CARD, theme::CARD) };
    let verdict = if battle { None } else { v.advice.as_ref().and_then(|a| verdict(&a.rows, v.stale)) };
    // The card has only the large text and the label (small text goes below); the large text takes the card's inner
    // width minus the label: when it does not fit, truncate the potion name first and keep `→ 水箱 N` (to tank N),
    // since the player most needs to know which tank to add to. The full text is in the card's tooltip. The width
    // uses the same algorithm as theme::advice_line (card inner width minus label; with no small text the large text
    // uses it all), so the shortened result fits exactly.
    let room = (ui.available_width() - theme::CARD_INSET - theme::tag_width(ui, verdict)).max(20.0);
    let shown = fit_option(ui, &big, theme::big_room(room, 0.0, ui.spacing().item_spacing.x), &FontId::proportional(theme::ADVICE_TEXT));
    let (card, cut) = theme::advice_line(ui, &shown, colour, "", MUTED, verdict, (fill, edge));
    // Clickable to see the full text only when truncated (shortened or still cut); otherwise hover only, so
    // click-through does not block the game.
    let sense = if cut || shown != big { Sense::click() } else { Sense::hover() };
    ui.interact(card, egui::Id::unique("vorax-card"), sense).on_hover_text(format!("{big}\n{small}"));
    // Right under the card: fixed one line (reason first, solve note only up to the first `；`), the full text in the
    // tooltip. One line rather than the two of before 2026-11-08: the row of six tanks needs the room at 421x351.
    card_small(ui, v, &brief, &small, 1);
    let rows = advice_rows(v);
    // The same order as the full panel (2026-11-08 layout): the six tanks in a row as in the game, the advised card's
    // tanks marked (short cells, see [`tank_strip`]), then the top three across the whole width with the tanks column.
    tank_strip(ui, &v.tanks, advised_targets(rows), v.stale, true);
    ui.scope(|ui| {
        // The rows touch (each is already a widget high, taller than its text), so the tanks fit at 421x351.
        ui.spacing_mut().item_spacing.y = 0.0;
        let width = ui.available_width();
        // Below 300 wide a tanks column would leave the option names a letter or two: there the names keep
        // `→ 水箱 N` (to tank N) as before, and the row of tanks above still marks the advised card's.
        let column = (width >= 300.0).then_some(v.tanks.as_slice());
        ranking_rows(ui, &Cols::new(width, 4.0), rows, v.stale, always, column);
    });
    rest_line(ui, rows);
    // Held instruments on the left, remaining rerolls on the right.
    held_row(ui, &v.held, &rerolls_text(v));
    theme::notes(ui, egui::Id::unique("vorax-notes"), &now_items(v), &note_items(v));
    // Two links; clicking expands below (the state is kept in egui's temp storage, so switching layout does not
    // affect the full version's collapsed state).
    let (all_id, board_id) = (egui::Id::unique("vorax-compact-ranking"), egui::Id::unique("vorax-compact-board"));
    let (mut all, mut board) = ui.data(|d| (d.get_temp::<bool>(all_id).unwrap_or(false), d.get_temp::<bool>(board_id).unwrap_or(false)));
    ui.horizontal(|ui| {
        let arrow = |open: bool| if open { "\u{ff0d}" } else { "\u{ff0b}" };
        if ui.add(egui::Label::new(RichText::new(format!("{} {}", tr("Full ranking"), arrow(all))).small().color(ACCENT)).sense(Sense::click()).selectable(false)).clicked() {
            all = !all;
        }
        ui.add_space(12.0);
        if ui.add(egui::Label::new(RichText::new(format!("{} {}", tr("Board as read"), arrow(board))).small().color(ACCENT)).sense(Sense::click()).selectable(false)).clicked() {
            board = !board;
        }
    });
    ui.data_mut(|d| {
        d.insert_temp(all_id, all);
        d.insert_temp(board_id, board);
    });
    if all {
        if rows.is_empty() {
            ui.label(RichText::new("\u{2014}").small().color(MUTED));
        } else {
            ranking_table(ui, rows, v.stale);
        }
    }
    if board {
        match &v.board {
            Some(text) => ui.label(RichText::new(text).small().monospace()),
            None => ui.label(RichText::new("\u{2014}").small().color(MUTED)),
        };
    }
}

/// The result card after a Vorax game (vorax-result-spec 2a), shown on the Board tab once the mode has ended until a
/// new Vorax game starts, Tartarus is played or a Tartarus reading dismisses it. Fixed skeleton, the same in both
/// layouts (each line is always drawn, blank when it has nothing, but for the reward line, left out with no battle
/// read; the detail and battle lines take a second line only when one would cut them short): the header
/// `渴血 · 本局結束` (Vorax, game over; in the compact layout ui.rs merges it into the status line), the final activity bar against 840k, then a card with the large `812k · 到達 840k`
/// (reached 840k or not) and the rounds and the board's final activity (with `（最後盤面讀數）`, the last board read,
/// when the result screen was not read), the endgame battle (activity + bonus = total, the tiers' drop bonus, and the reward tier
/// reached), the held instruments, and a hint that the review is on the Run tab. Fits 421x351 and 296x560 with no
/// scrollbar.
pub fn result_panel(g: &GameSummary, ui: &mut egui::Ui, compact: bool) {
    let target = ::vorax::value::TARGET;
    if !compact {
        theme::line(ui, tr("Vorax \u{b7} game over"), MUTED);
    }
    match g.board {
        Some(board) => {
            // Same verdict as the title: a battle panel value within the tolerance is unsure, not green.
            let colour = match card_arrival(g, target) {
                Some(Arrival::Reached) => theme::GOOD,
                Some(Arrival::Unsure) => theme::WARN,
                _ => ACCENT,
            };
            progress(ui, tr("Activity"), board as f32 / target as f32, format!("{} / {}", short_k(board), short_k(target)), colour)
                .on_hover_text(trf!("Final activity {} (target {})", grouped(board), grouped(target)));
        }
        None => {
            progress(ui, tr("Activity"), 0.0, format!("\u{2014} / {}", short_k(target)), theme::TRACK);
        }
    }
    ui.add_space(4.0);
    let from_battle = g.board_from == Some(BoardFrom::Battle);
    let (big, big_colour) = match (g.board, card_arrival(g, target)) {
        (Some(b), Some(Arrival::Reached)) => (format!("{} \u{b7} {}", short_k(b), trf!("Reached {}", short_k(target))), theme::GOOD),
        (Some(b), Some(Arrival::Unsure)) => (trf!("~{} \u{b7} unsure if {} was reached", short_k(b), short_k(target)), theme::WARN),
        (Some(b), _) => (format!("{} \u{b7} {}", short_k(b), trf!("Missed {}", short_k(target))), theme::WARN),
        (None, _) => (tr("Final activity not read").to_string(), MUTED),
    };
    // A battle panel value is rounded and runs a little over the board's: say so with an approximate sign.
    let board = g.board.map_or_else(|| "\u{2014}".to_string(), |b| if from_battle { trf!("~{}", grouped(b)) } else { grouped(b) });
    let mut detail = trf!("{} rounds \u{b7} final board activity {}", g.rounds, board);
    if g.board_from == Some(BoardFrom::LastBoard) {
        detail.push_str(tr(" (last board read)"));
    } else if from_battle {
        detail.push_str(tr(" (battle panel)"));
    }
    let (battle, reward) = result_battle(g);
    theme::card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 2.0;
        let r = ui.add(egui::Label::new(RichText::new(&big).size(theme::ADVICE_TEXT).strong().color(big_colour)).truncate().show_tooltip_when_elided(false));
        r.on_hover_text(format!("{big}\n{detail}"));
        fitted_line(ui, &detail, theme::TEXT);
    });
    ui.add_space(4.0);
    theme::line(ui, tr("Endgame battle"), MUTED);
    fitted_line(ui, &battle, if g.battle.is_some() { theme::TEXT } else { MUTED });
    // No battle read (or no next tier read): no reward line, not a blank one.
    if !reward.is_empty() {
        theme::line(ui, &reward, MUTED);
    }
    let held_items: Vec<(String, bool)> = g.instruments.iter().map(|n| (n.clone(), false)).collect();
    held(ui, &held_items);
    theme::line(ui, tr("The review of this game is on the Run tab"), MUTED.gamma_multiply(0.8));
}

/// Whether the card's game reached `target`: a final activity taken from the battle panel is approximate, so close to
/// the target it is [`Arrival::Unsure`]; None when no final activity was read.
fn card_arrival(g: &GameSummary, target: i64) -> Option<Arrival> {
    g.board.map(|b| vorax_settle::arrival(b, g.board_from == Some(BoardFrom::Battle), target))
}

/// `numbers` and, when the game shows one, the drop bonus of the reward tiers reached: the `(+15%)` beside the antidote
/// vials on the battle panel and the result screen is the tiers' "Antidote Vial Drop Quantity +15%" (854k), not a
/// share of the bonus (2026-11-08, the user: the bonus was 44% of the activity while the panel showed +15%). The
/// share was shown as `加成 …（+15%）` until then, and computed from activity and bonus when unread, which was wrong
/// both ways. A negative reading is treated as unreadable.
fn with_drops(numbers: String, percent: Option<i32>) -> String {
    match percent {
        Some(p) if p >= 0 => format!("{numbers} \u{b7} {}", trf!("drops +{}%", p)),
        _ => numbers,
    }
}

/// The two battle lines of the result card: `活性 812.3k ＋ 加成 304.6k ＝ 1.11M · 掉落 +15%` (activity + bonus = total,
/// numbers rounded down, see [`battle_k`], then the drop bonus, see [`with_drops`]) and the reward reached (`獎勵：下一階 1.12M` next tier, or all taken);
/// with no battle read, a line saying so and an empty reward, which is not drawn.
fn result_battle(g: &GameSummary) -> (String, String) {
    let Some(b) = &g.battle else { return (tr("No endgame battle read").to_string(), String::new()) };
    let k = battle_k;
    let total = k(g.total());
    let numbers = trf!("Activity {} + bonus {} = {}", k(b.activity), k(b.bonus), total);
    let numbers = with_drops(numbers, b.percent);
    let reward = match b.next {
        Some(n) if n < 1e8 => trf!("Reward: next tier {}", k(Some(n))),
        Some(_) => tr("Reward: all taken").to_string(),
        None => String::new(),
    };
    (numbers, reward)
}

/// The rows the top three actually draw: the top three, with the advised one in third place when its rank is lower.
fn shown_rows(rows: &[AdviceRow]) -> Vec<usize> {
    let mut shown: Vec<usize> = (0..rows.len().min(TOP_ROWS)).collect();
    if let Some(b) = rows.iter().position(|r| r.best).filter(|&b| b >= TOP_ROWS)
        && let Some(last) = shown.last_mut()
    {
        *last = b;
    }
    shown
}

/// The small line under the top three: the next three options not written in the top three,
/// `4 name 94% 812k · 5 … · 6 …` (the user's choice, with 5 cards or 6 options all are visible). Fixed one line:
/// blank with fewer than 4 options, truncated when too long, with the full text in the hover tooltip.
fn rest_line(ui: &mut egui::Ui, rows: &[AdviceRow]) {
    let full = rest_text(rows);
    let small = egui::TextStyle::Small.resolve(ui.style());
    let width = ui.available_width();
    let w = |t: &str| ui.painter().layout_no_wrap(t.to_string(), small.clone(), Color32::WHITE).size().x;
    let shown = if full.is_empty() || w(&full) <= width {
        full.clone()
    } else {
        // Does not fit: each item gets an equal share of the width and shortens its own name (keeping `→ 水箱 N` and
        // the numbers); if after the split a name cannot even keep two characters, drop one item (rank 4 first), with
        // the full text in the hover tooltip.
        let entries = rest_entries(rows);
        let sep = " \u{b7} ";
        // List n items: each gets an equal share of the width and shortens its own name; the name must keep at least
        // its beginning (not only `…→N`), and the whole line, as actually measured, must fit to count. Try from most
        // to fewest; use the first that fits.
        let build = |n: usize| -> Option<String> {
            let per = (width - w(sep) * (n.saturating_sub(1)) as f32) / n as f32;
            let mut parts = Vec::with_capacity(n);
            for (rank, label, nums) in &entries[..n] {
                let (fixed, tail) = (format!("{rank} "), format!(" {nums}"));
                let name = fit_option(ui, label, per - w(&fixed) - w(&tail), &small);
                if name.starts_with('\u{2026}') {
                    return None;
                }
                parts.push(format!("{fixed}{name}{tail}"));
            }
            let joined = parts.join(sep);
            (w(&joined) <= width + 0.5).then_some(joined)
        };
        (1..=entries.len()).rev().find_map(build).unwrap_or_else(|| full.clone())
    };
    theme::line_full(ui, &shown, &full, MUTED);
}

/// For each of ranks 4-6: (rank, name, `94% 812k`).
fn rest_entries(rows: &[AdviceRow]) -> Vec<(usize, String, String)> {
    let shown = shown_rows(rows);
    (0..rows.len()).filter(|k| !shown.contains(k)).take(3).map(|k| (k + 1, rows[k].label.clone(), format!("{} {}", chance(&rows[k]), mean_k(&rows[k])))).collect()
}

fn rest_text(rows: &[AdviceRow]) -> String {
    rest_entries(rows).into_iter().map(|(rank, label, nums)| format!("{rank} {label} {nums}")).collect::<Vec<_>>().join(" \u{b7} ")
}

/// A range of fixed height (normal widget height) laid out left to right inside; the overflow is cropped (no
/// wrapping), returns the whole row's Response (Sense::hover).
fn fixed_row(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) -> egui::Response {
    let h = ui.spacing().interact_size.y;
    let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), Sense::hover());
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect).layout(Layout::left_to_right(Align::Center)));
    child.set_clip_rect(rect.intersect(ui.clip_rect()));
    child.spacing_mut().item_spacing.x = 4.0;
    add(&mut child);
    response
}

/// The two small remaining-reroll labels, fixed one line; this line stays blank when not seen yet.
fn rerolls(ui: &mut egui::Ui, v: &View) {
    fixed_row(ui, |ui| {
        if v.potion_rerolls.is_some() || v.instrument_rerolls.is_some() {
            let count = |n: Option<i32>| n.map_or_else(|| "?".to_string(), |n| n.to_string());
            theme::chip(ui, RichText::new(trf!("Potion rerolls {}", count(v.potion_rerolls))).small());
            theme::chip(ui, RichText::new(trf!("Instrument rerolls {}", count(v.instrument_rerolls))).small());
        }
    });
}

/// The held instruments, fixed one line: those that do not fit collapse to `+N`, and hovering over this line shows
/// them all. Blank when there are none.
fn held(ui: &mut egui::Ui, held: &[(String, bool)]) {
    let width = ui.available_width();
    let chip_w = |ui: &egui::Ui, text: &str| egui::WidgetText::from(RichText::new(text).small()).into_galley(ui, Some(egui::TextWrapMode::Extend), f32::INFINITY, egui::TextStyle::Small).size().x + 12.0;
    let widths: Vec<f32> = held.iter().map(|(name, _)| chip_w(ui, name)).collect();
    let gap = 4.0;
    let total = widths.iter().sum::<f32>() + gap * widths.len().saturating_sub(1) as f32;
    // If all fit, show all; otherwise leave room for a `+N`, and show as many as fit in front of it.
    let fit = if total <= width {
        held.len()
    } else {
        let more = chip_w(ui, &format!("+{}", held.len())) + gap;
        let mut used = 0.0;
        widths.iter().take_while(|w| {
            used += *w + gap;
            used + more <= width
        }).count()
    };
    let response = fixed_row(ui, |ui| {
        for (name, opening) in &held[..fit] {
            let text = if *opening { RichText::new(name).small().color(MUTED) } else { RichText::new(name).small() };
            theme::chip(ui, text);
        }
        if fit < held.len() {
            theme::chip(ui, RichText::new(format!("+{}", held.len() - fit)).small().color(MUTED));
        }
    });
    if !held.is_empty() {
        response.on_hover_text(held.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>().join(tr(", ")));
    }
}

/// The number of rows shown for the top three (the same three rows as Tartarus).
const TOP_ROWS: usize = 3;
/// Options that differ from the advice by only this much (as a ratio of expected score) are drawn in a brighter
/// grey-blue (same as Tartarus's "also good").
const NEAR: f64 = 0.01;

/// The column widths of each top-three row: rank, option name, chance to reach 840k (`94%`), average (`812k`); the
/// name uses the remaining width (wider than before now that the bars are gone).
#[derive(Clone, Copy)]
struct Cols {
    rank: f32,
    label: f32,
    reach: f32,
    mean: f32,
    spacing: f32,
}

impl Cols {
    fn new(width: f32, spacing: f32) -> Cols {
        let (rank, reach, mean) = (14.0, 40.0, 40.0);
        let label = (width - rank - reach - mean - 3.0 * spacing).max(20.0);
        Cols { rank, label, reach, mean, spacing }
    }
}

/// The verdict of the advice against the runner-up (plan section 3): no label when there are fewer than two ranking
/// rows or the advice is stale (grey). Compare the advised row with the first row that is not it: if the 840k chance
/// differs by less than 1 percentage point, or the expected score differs by less than 1% of the advice's, it counts
/// as "either is fine" (`都可以`); otherwise "clearly better" (`明顯較好`).
fn verdict(rows: &[AdviceRow], stale: bool) -> Option<Verdict> {
    if rows.len() < 2 || stale {
        return None;
    }
    let b = rows.iter().position(|r| r.best).unwrap_or(0);
    let rival = rows.iter().enumerate().find(|&(k, _)| k != b).map(|(_, r)| r)?;
    let best = &rows[b];
    let close_reach = matches!((best.reached, rival.reached), (Some(x), Some(y)) if (x - y).abs() <= 0.01);
    let close_mean = best.mean - rival.mean <= NEAR * best.mean.abs();
    Some(if close_reach || close_mean { Verdict::Either } else { Verdict::Clear })
}

/// The advice card, fixed height (the same height while computing, with no advice, or with advice): the large advice
/// text takes at most two lines (truncated if longer, full text in the hover tooltip) with the verdict label on the
/// right, and one small line below. With advice the small line is how it was computed (method, number of runs, time
/// taken); with no advice the large text is `計算中…` (computing) or `—`, and the small line is the first line of the
/// status line (the protocol's description, unreadable, etc.). When the advice is stale the whole card turns grey
/// with no label; the border width is unchanged so the height does not differ even slightly.
fn advice_card(ui: &mut egui::Ui, v: &View) {
    let stale = v.stale;
    let (big, small) = card_text(v);
    let brief = card_brief(v);
    let colour = match &v.advice {
        _ if in_battle(v) => Color32::WHITE,
        Some(_) if stale => STALE,
        Some(_) => Color32::WHITE,
        None => MUTED,
    };
    // During battle view.advice may still hold the last move's advice from round 13: the battle card does not carry
    // its verdict label and is not greyed by it.
    let battle = in_battle(v);
    let verdict = if battle { None } else { v.advice.as_ref().and_then(|a| verdict(&a.rows, stale)) };
    let (fill, edge) = if stale && !battle { (theme::PANEL, DIVIDER) } else { (theme::CARD, theme::CARD) };
    theme::card_frame().fill(fill).stroke(Stroke::new(1.0, edge)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 2.0;
        let font = FontId::proportional(theme::ADVICE_TEXT);
        let row_h = ui.painter().layout_no_wrap("\u{570b}".to_string(), font.clone(), colour).size().y;
        ui.horizontal_top(|ui| {
            let spacing = ui.spacing().item_spacing.x;
            let tag_w = verdict.map_or(0.0, |v| {
                egui::WidgetText::from(RichText::new(v.text()).small()).into_galley(ui, None, f32::INFINITY, egui::TextStyle::Small).size().x + 10.0 + spacing
            });
            let width = (ui.available_width() - tag_w).max(20.0);
            let layout = |text: &str| {
                let mut job = egui::text::LayoutJob::simple(text.to_owned(), font.clone(), colour, width);
                job.wrap.max_rows = 2;
                ui.painter().layout_job(job)
            };
            // When one line does not fit, break before `→` (`建議：迷魂酊劑` / `→ 水箱 1`, i.e. "Advice: Bewitching
            // Tincture" / "-> tank 1"), so that `水箱` (tank) is not split across two lines; when two lines still do
            // not fit, shorten the potion name and keep `→ 水箱 N` (two lines are about twice the width, minus a
            // little for the wrap loss).
            let one_line = ui.painter().layout_no_wrap(big.clone(), font.clone(), colour).size().x <= width;
            let split = big.replacen(" \u{2192} ", "\n\u{2192} ", 1);
            let mut galley = layout(if one_line { &big } else { &split });
            let elided = galley.elided;
            if elided {
                galley = layout(&fit_option(ui, &big, width * 2.0 - theme::ADVICE_TEXT * 1.5, &font));
            }
            // Clickable to see the full text only when truncated (also hoverable under click-through).
            let (rect, response) = ui.allocate_exact_size(Vec2::new(width, row_h * 2.0), if elided { Sense::click() } else { Sense::hover() });
            ui.painter().galley(rect.min, galley, colour);
            if elided {
                response.on_hover_text(&big);
            }
            if let Some(v) = verdict {
                ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                    theme::tag(ui, v);
                });
            }
        });
        // The small text is fixed at two lines high and auto-wrapped; the solve note is written only up to the first
        // `；`, with the full text in the tooltip. During battle it is the battle numbers.
        card_small(ui, v, &brief, &small, 2);
    });
}

/// The large and small text of the advice card, computed directly from [`View`] on every frame without the flicker
/// guard (advice must switch in live).
/// - With advice: the large text is the advice; the small text is the first line of the status line (`求解中`
///   solving, `讀不到：` unreadable, etc.; while recomputing or unreadable, the old advice turns grey and this says
///   why), and when the advice is fresh it says how it was computed (method, number of runs, time taken).
/// - No advice: the large text is `計算中…` (computing) or `—`, and the small text is the first line of the status
///   line (the protocol's description, etc.).
fn card_text(v: &View) -> (String, String) {
    card_parts(v, false)
}

/// The compact on-screen form of the advice card's small text: same as the small text of [`card_text`], but the solve
/// note (the long sentence from the vorax module) is written only up to the first `；` (or the English `;`), followed
/// by the number of runs and the time; the whole sentence is in the tooltip.
fn card_brief(v: &View) -> String {
    card_parts(v, true).1
}

fn card_parts(v: &View, brief: bool) -> (String, String) {
    let status = v.lines.first().cloned().unwrap_or_default();
    // Endgame battle: the advice card becomes the battle card (until the panel has been read the small text is the
    // first line of the status line).
    if in_battle(v) {
        return match &v.battle {
            Some(b) => battle_text(b),
            None => (tr("Endgame battle").to_string(), status),
        };
    }
    match &v.advice {
        Some(a) if v.status == Status::Advised => {
            let method = match a.method.find('\u{ff1b}').or_else(|| a.method.find(';')) {
                Some(i) if brief => format!("{} \u{b7} ", &a.method[..i]),
                // The English sentence ends in `.` and needs a space before the count; the Chinese one ends in `。`.
                _ if a.method.ends_with('.') => format!("{} ", a.method),
                _ => a.method.clone(),
            };
            let mut how = trf!("{}{} runs", method, v.rollouts);
            if let Some(ms) = v.solve_ms {
                how += &trf!(", {} s", format!("{:.2}", ms as f64 / 1000.0));
            }
            // When the advice is not rank 1, or the ranking is not by 840k chance, the small text first gives the
            // reason (see advice_reason).
            match advice_reason(&a.rows) {
                Some(reason) => (a.headline.clone(), format!("{reason} \u{b7} {how}")),
                None => (a.headline.clone(), how),
            }
        }
        Some(a) => (a.headline.clone(), status),
        None if v.status == Status::Solving => (tr("Solving...").to_string(), String::new()),
        None => ("\u{2014}".to_string(), status),
    }
}

/// The reason on the advice card's small text (checked by the session responsible for Vorax: the box and the advice
/// always take the best=true row; best not being row 0 happens in only two cases, a and c):
/// - a. The best row is a potion kit and row 0 is a reroll (refresh): `有藥劑箱時先開藥劑箱，不重擲` (open the potion
///   kit first when there is one, no reroll).
/// - b. The best is row 0, but another row has a higher 840k chance (the most common, about 6%):
///   `排名看平均分數，不是達 840k 機率` (ranked by average score, not 840k chance).
/// - c. Row 0 is a reroll and the best row is not a potion kit (the reroll was eliminated midway through solving):
///   `重擲在模擬中途已被淘汰， 分數是較少局數算的` (the reroll was dropped partway through the simulation, its score
///   was computed over fewer runs).
///
/// Other cases where best is not row 0 should not happen in principle, and no reason is written.
fn advice_reason(rows: &[AdviceRow]) -> Option<&'static str> {
    let b = rows.iter().position(|r| r.best)?;
    if b != 0 {
        if !rows[0].refresh {
            return None;
        }
        return Some(if is_kit(&rows[b].label) { tr("Potion kit first, no reroll") } else { tr("Reroll dropped early \u{b7} fewer runs") });
    }
    let top = rows[0].reached?;
    rows.iter().skip(1).any(|r| r.reached.is_some_and(|p| p > top)).then(|| tr("Ranked by average, not 840k chance"))
}

/// The option is a potion kit: AdviceRow has no identifying field, so match by name; the name comes from the same
/// source as the label (data/vorax-names-zh-TW.json: `藥劑箱（大）` and `稀有藥劑箱（小）`, in English
/// `Potion Kit (Large)` and `Rare Potion Kit (Small)`), and both Chinese and English are recognised.
fn is_kit(label: &str) -> bool {
    label.contains("\u{85e5}\u{5291}\u{7bb1}") || label.contains("Potion Kit")
}

/// The Vorax ranking rows: there is no ranking in the endgame battle (empty; the top three are still padded with
/// three blank rows).
fn advice_rows(v: &View) -> &[AdviceRow] {
    if in_battle(v) {
        return &[];
    }
    v.advice.as_ref().map_or(&[], |a| &a.rows)
}

/// In the endgame battle (the status is battle, or the battle panel was read).
fn in_battle(v: &View) -> bool {
    v.status == Status::Battle || v.battle.is_some()
}

/// Test only: draw `b` as the battle panel that was read (changes only this drawn copy).
fn with_battle(v: &View, b: &read::Battle) -> View {
    View { status: Status::Battle, battle: Some(b.clone()), advice: None, lines: Vec::new(), ..v.clone() }
}

/// Test only (ASCENDANT_PREVIEW_VORAX_PANEL): a made-up board for the panel. Six tanks with all four archetypes and
/// all four rarities; `advice` has a potion acting on tanks 2 and 5 advised, a reroll row and a potion with no target;
/// `stale` is that advice greyed while solving again; `none` is the same tanks unreadable, with no advice; `blank` is
/// nothing read yet.
fn preview_view(which: &str) -> View {
    use ::vorax::Archetype::{Aemberon, Agito, BoneSentry, Pest};
    let zh = crate::lang::zh();
    let zh_en = |zh_text: &'static str, en: &'static str| if zh { zh_text } else { en };
    if which == "blank" {
        return View { mode: vorax_session::Mode::Board, status: Status::Idle, lines: vec![zh_en("等待讀取", "Waiting for a read").to_string()], ..View::default() };
    }
    // Monster names: in Chinese the first of each archetype in the name table, in English made up.
    let monster = |a: ::vorax::Archetype, en: &str| {
        if zh { crate::vorax::names::names().monsters.iter().find(|(_, x)| *x == a).map_or_else(|| en.to_string(), |(n, _)| n.clone()) } else { en.to_string() }
    };
    let tank = |a: ::vorax::Archetype, en: &str, rarity: Rarity, activity: i64, quantity: i64| read::Tank {
        name: Some(monster(a, en)),
        archetype: Some(a),
        rarity: Some(rarity),
        activity: Some(activity),
        quantity: Some(quantity),
        total: Some(activity * quantity),
        ..read::Tank::default()
    };
    let tanks = vec![
        tank(Pest, "Gnawing Pest", Rarity::Normal, 3_200, 4),
        tank(BoneSentry, "Bone Sentry", Rarity::Magic, 8_400, 3),
        tank(Agito, "Agito Seer", Rarity::Rare, 21_500, 2),
        tank(Aemberon, "Aemberon Wisp", Rarity::Boss, 64_000, 1),
        tank(Pest, "Plague Pest", Rarity::Magic, 5_100, 5),
        tank(BoneSentry, "Bone Warden", Rarity::Normal, 2_800, 6),
    ];
    if which == "none" {
        return View {
            mode: vorax_session::Mode::Board,
            status: Status::Unreadable,
            lines: vec![zh_en("讀不到：", "Unreadable:").to_string(), zh_en("卡 2 的名稱", "the name of card 2").to_string()],
            round: Some(6),
            ultimate: Some(612_000),
            potion_rerolls: Some(2),
            instrument_rerolls: Some(1),
            tanks,
            held: vec![(zh_en("骨鋸", "Bone Saw").to_string(), false)],
            ..View::default()
        };
    }
    let potions: Vec<String> = crate::vorax::names::names().cards.iter().filter(|c| c.category == crate::vorax::names::Category::Potion && c.zh.is_some()).take(4).map(|c| c.shown().to_string()).collect();
    let potion = |k: usize| potions.get(k).cloned().unwrap_or_else(|| format!("Potion {k}"));
    let label = |name: &str, targets: &[u8]| {
        if targets.is_empty() {
            return name.to_string();
        }
        let ts: Vec<String> = targets.iter().map(|k| (k + 1).to_string()).collect();
        format!("{name} \u{2192} {} {}", zh_en("水箱", "tank"), ts.join(zh_en("\u{3001}", ",")))
    };
    let row = |name: &str, targets: &[u8], mean: f64, reached: f64, refresh: bool, best: bool| AdviceRow { label: label(name, targets), name: name.to_string(), targets: targets.to_vec(), mean, reached: Some(reached), refresh, best };
    let rows = vec![
        row(&potion(0), &[1, 4], 812_400.0, 0.62, false, true),
        row(zh_en("重擲", "Reroll"), &[], 790_100.0, 0.55, true, false),
        row(&potion(1), &[2], 771_800.0, 0.50, false, false),
        row(&potion(2), &[0], 702_300.0, 0.41, false, false),
        row(&potion(3), &[], 650_900.0, 0.33, false, false),
    ];
    let headline = format!("{}{}{}", zh_en("建議", "Pick"), zh_en("\u{ff1a}", ": "), rows[0].label);
    let method = zh_en("藥劑：每個選項模擬到終局；期望分數是終局最終活性的平均（上限 840k）。", "Potions: each option played to the end; mean is the final Ultimate Activity, capped at 840k.");
    let stale = which == "stale";
    View {
        mode: vorax_session::Mode::Board,
        status: if stale { Status::Solving } else { Status::Advised },
        lines: vec![if stale { zh_en("求解中", "Solving") } else { zh_en("建議", "Advised") }.to_string()],
        stale,
        round: Some(6),
        ultimate: Some(612_000),
        potion_rerolls: Some(2),
        instrument_rerolls: Some(1),
        advice: Some(crate::vorax::AdviceRows { headline, note: None, rows, method: method.to_string(), unknown: vec![] }),
        rollouts: 4096,
        solve_ms: Some(1234),
        board: Some("preview".to_string()),
        tanks,
        held: vec![(zh_en("骨鋸", "Bone Saw").to_string(), false)],
        ..View::default()
    }
}

/// The battle card: large text `終局戰鬥 第 2/3 波` (endgame battle, wave 2/3), small text
/// `活性 568.5k ＋ 加成 221.2k · 下一格 854.0k · 掉落 +15%` (activity 568.5k + bonus 221.2k, next tier 854.0k, drops +15% (see
/// [`with_drops`])), and
/// after the last tier is taken it writes `獎勵全拿` (all rewards taken) (threshold 999.9M). The numbers are those of
/// the result card (see [`battle_k`]): rounded down, k with one decimal, M with two.
fn battle_text(b: &read::Battle) -> (String, String) {
    let k = battle_k;
    let big = match b.wave {
        Some((w, n)) => trf!("Endgame battle \u{b7} wave {}/{}", w, n),
        None => tr("Endgame battle").to_string(),
    };
    let numbers = trf!("Activity {} + bonus {}", k(b.activity), k(b.bonus));
    // The next tier before the drop bonus: the compact panel's one line is cut at its end, and the tier is what the
    // player plays for (the drop bonus is in the tooltip then).
    let numbers = match b.next {
        Some(n) if n < 1e8 => format!("{numbers} \u{b7} {}", trf!("next {}", k(Some(n)))),
        Some(_) => format!("{numbers} \u{b7} {}", tr("all rewards taken")),
        None => numbers,
    };
    (big, with_drops(numbers, b.percent))
}

/// The small-text area of the advice card (fixed `rows` lines high: two in the full panel, one in the compact one):
/// normally the compact form with the full text in the tooltip; in the endgame battle the first line is the battle
/// numbers and the second line is the very faint `戰鬥中讀數有雜訊，結束時最準` (readings are noisy during battle,
/// most accurate at the end). With one line, the battle numbers alone (cut short when too long) and the faint note
/// only in the tooltip.
fn card_small(ui: &mut egui::Ui, v: &View, brief: &str, small: &str, rows: usize) {
    if in_battle(v) && rows < 2 {
        let noise = tr("Readings are noisy during the battle; most accurate at its end");
        theme::lines_full(ui, small, &format!("{small}\n{noise}"), theme::TEXT, 1);
    } else if in_battle(v) {
        let noise = tr("Readings are noisy during the battle; most accurate at its end");
        let faint = MUTED.gamma_multiply(0.6);
        ui.scope(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            // When one line fits: battle numbers on the first line, faint text on the second. When it does not fit
            // (narrow window): the first line is `活性 ＋ 加成` (activity + bonus), the second line is `下一格` (next
            // tier) followed by the faint text, so the next tier is not cut off.
            match small.split_once(" \u{b7} ") {
                Some((numbers, next)) if theme::elided(ui, small, ui.available_width(), 1) => {
                    theme::line(ui, numbers, theme::TEXT);
                    let font = egui::TextStyle::Small.resolve(ui.style());
                    let mut job = egui::text::LayoutJob::default();
                    job.append(next, 0.0, egui::TextFormat::simple(font.clone(), theme::TEXT));
                    job.append(&format!(" \u{b7} {noise}"), 0.0, egui::TextFormat::simple(font, faint));
                    job.wrap.max_width = ui.available_width();
                    job.wrap.max_rows = 1;
                    job.wrap.break_anywhere = true;
                    let galley = ui.painter().layout_job(job);
                    let h = ui.text_style_height(&egui::TextStyle::Small);
                    let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), h), if galley.elided { Sense::click() } else { Sense::hover() });
                    let elided = galley.elided;
                    ui.painter().galley(rect.left_center() - Vec2::new(0.0, galley.size().y / 2.0), galley, theme::TEXT);
                    if elided {
                        response.on_hover_text(format!("{small}\n{noise}"));
                    }
                }
                _ => {
                    theme::line(ui, small, theme::TEXT);
                    theme::line(ui, noise, faint);
                }
            }
        });
    } else {
        theme::lines_full(ui, brief, small, MUTED, rows);
    }
}

/// The held-instruments row (compact version): the held instruments on the left, remaining rerolls
/// `藥劑重擲 2 · 用具重擲 ?` (potion rerolls 2, instrument rerolls ?) on the right (always visible).
fn held_row(ui: &mut egui::Ui, items: &[(String, bool)], rerolls: &str) {
    let small = egui::TextStyle::Small.resolve(ui.style());
    let rr_w = if rerolls.is_empty() { 0.0 } else { ui.painter().layout_no_wrap(rerolls.to_string(), small, MUTED).size().x + 8.0 };
    ui.horizontal(|ui| {
        let w = (ui.available_width() - rr_w).max(40.0);
        let h = ui.spacing().interact_size.y;
        ui.allocate_ui_with_layout(Vec2::new(w, h), Layout::left_to_right(Align::Center), |ui| {
            ui.set_width(w);
            held(ui, items);
        });
        theme::line_aligned(ui, rerolls, MUTED, true);
    });
}

/// Shortening an option name that is too long: for `potion name → tank N`, truncate the potion name first and keep
/// the target tank, e.g. `靶向異種… → 水箱 5` (Targeted Xeno-Hormone... -> tank 5); if it still does not fit, shorten
/// the target instead, `靶向異種激…→5`. A name without `→` is returned as is (left to truncation). `width` is the
/// available width and `font` is the font to draw with.
fn fit_option(ui: &egui::Ui, label: &str, width: f32, font: &FontId) -> String {
    let w = |t: &str| ui.painter().layout_no_wrap(t.to_string(), font.clone(), Color32::WHITE).size().x;
    if w(label) <= width {
        return label.to_string();
    }
    let Some((name, target)) = label.split_once(" \u{2192} ") else { return label.to_string() };
    let digits: String = target.chars().filter(|c| c.is_ascii_digit() || *c == '\u{3001}' || *c == ',').collect();
    let short = if digits.is_empty() { target.to_string() } else { digits };
    for tail in [format!("\u{2026} \u{2192} {target}"), format!("\u{2026}\u{2192}{short}")] {
        let room = width - w(&tail);
        let chars: Vec<char> = name.chars().collect();
        // Keep at least two characters of the potion name, otherwise switch to a shorter form.
        if let Some(n) = (2..chars.len()).rev().find(|&n| w(&chars[..n].iter().collect::<String>()) <= room) {
            return format!("{}{tail}", chars[..n].iter().collect::<String>());
        }
    }
    format!("\u{2026}\u{2192}{short}")
}

/// Content of the warning area that needs the flicker guard: the blocking reason and the unreadable reason (the
/// second line of the status line onward). The first line of the status line (solving, advice, ...) and the advice
/// itself are not here, see [`card_text`]; the solver's note belongs to the advice, see [`now_items`].
fn note_items(v: &View) -> Vec<(String, Color32)> {
    let mut notes: Vec<(String, Color32)> = v.blocked.iter().map(|b| (b.clone(), theme::WARN)).collect();
    notes.extend(v.lines.iter().skip(1).map(|l| (l.clone(), MUTED)));
    notes
}

/// Content of the warning area that switches live together with the advice: the solver's note on this advice. It
/// skips the flicker guard, so that when the advice changes the note does not linger on the previous one for 0.5
/// seconds.
fn now_items(v: &View) -> Vec<(String, Color32)> {
    if in_battle(v) {
        return Vec::new();
    }
    v.advice.as_ref().and_then(|a| a.note.as_ref()).map(|note| (note.clone(), theme::TEXT)).into_iter().collect()
}

/// One progress-bar row: title on the left, bar in the middle, number on the right.
fn progress(ui: &mut egui::Ui, title: &str, share: f32, text: String, colour: Color32) -> egui::Response {
    ui.horizontal(|ui| {
        let h = ui.spacing().interact_size.y;
        let spacing = ui.spacing().item_spacing.x;
        // At least 30 wide (the Chinese title fits in it); a longer title (the English one) gets its own width.
        let small = egui::TextStyle::Small.resolve(ui.style());
        let title_w = ui.painter().layout_no_wrap(title.to_string(), small, MUTED).size().x.ceil().max(30.0);
        ui.add_sized([title_w, h], egui::Label::new(RichText::new(title).small().color(MUTED)));
        let width = (ui.available_width() - 70.0 - spacing).max(30.0);
        theme::bar(ui, width, share, colour);
        ui.allocate_ui_with_layout(Vec2::new(70.0, h), Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(text).small());
        });
    })
    .response
}

/// The top three (the advised one goes in third place when its rank is lower; 2026-11-05 the user: the bars carried
/// no meaning, so only numbers are written): rank, name, chance to reach 840k, average score (expected score, written
/// as `812k` by short_k). Ranked by average score. The header and the three rows are always drawn (blank rows pad it
/// when there are fewer than three).
///
/// With `tanks` (both panels, 2026-11-08 layout) the option column writes the card name alone ([`AdviceRow::name`])
/// and a `水箱` (tanks) column after it draws the six tanks small, the ones the card acts on enlarged in a gold frame
/// (see [`tank_minis`]); without (the compact panel below 300 wide), the option column writes the whole label,
/// `卡名 → 水箱 2、5`, with no tanks column.
fn ranking_rows(ui: &mut egui::Ui, cols: &Cols, rows: &[AdviceRow], stale: bool, always: bool, tanks: Option<&[read::Tank]>) {
    let shown = shown_rows(rows);
    // The number columns use the width actually needed (the wider of the value and the column header), and the rest
    // goes to the name column.
    let width_of = |text: &str, style: egui::TextStyle| ui.painter().layout_no_wrap(text.to_string(), style.resolve(ui.style()), Color32::WHITE).size().x;
    let reach_w = shown.iter().map(|&k| width_of(&chance(&rows[k]), egui::TextStyle::Body)).fold(width_of(tr("Reach 840k"), egui::TextStyle::Small), f32::max) + 2.0;
    let mean_w = shown.iter().map(|&k| width_of(&row_mean(&rows[k], always), egui::TextStyle::Body)).fold(width_of(tr("Average"), egui::TextStyle::Small).max(width_of("888k", egui::TextStyle::Body)), f32::max) + 2.0;
    let h = ui.spacing().interact_size.y;
    let total = cols.rank + cols.label + cols.reach + cols.mean;
    // The tanks column: six cells about a row high, at most 30% of the row (narrow windows shrink the cells).
    let tanks_w = match tanks {
        Some(_) => (6.0 * (h - 4.0) + 5.0 * MINI_GAP).min(total * 0.3).max(width_of(tr("Tanks"), egui::TextStyle::Small)),
        None => 0.0,
    };
    let tanks_room = if tanks.is_some() { tanks_w + cols.spacing } else { 0.0 };
    let cols = &Cols { label: (total - cols.rank - reach_w - mean_w - tanks_room).max(20.0), reach: reach_w, mean: mean_w, ..*cols };
    let blanks = shown.len()..TOP_ROWS;
    ui.spacing_mut().item_spacing.x = cols.spacing;
    let reach_tip = tr("The chance that the final Ultimate Activity reaches 840k");
    let mean_tip = tr("The average final Ultimate Activity, counting nothing above 840k; ranked by this");
    // The column header `選項｜達 840k｜平均` (option | reaches 840k | average), with the same widths as each row
    // below.
    ui.horizontal(|ui| {
        let head = |text: &str| RichText::new(text).small().color(MUTED);
        let left = cols.rank + cols.spacing + cols.label;
        ui.allocate_ui_with_layout(Vec2::new(left, h * 0.8), Layout::left_to_right(Align::Center), |ui| {
            ui.set_min_width(left);
            ui.label(head(tr("Option")));
        });
        if tanks.is_some() {
            ui.allocate_ui_with_layout(Vec2::new(tanks_w, h * 0.8), Layout::centered_and_justified(egui::Direction::LeftToRight), |ui| {
                ui.label(head(tr("Tanks"))).on_hover_text(tr("The tanks the card acts on are framed in gold"));
            });
        }
        ui.allocate_ui_with_layout(Vec2::new(cols.reach, h * 0.8), Layout::right_to_left(Align::Center), |ui| {
            ui.label(head(tr("Reach 840k"))).on_hover_text(reach_tip);
        });
        ui.allocate_ui_with_layout(Vec2::new(cols.mean, h * 0.8), Layout::right_to_left(Align::Center), |ui| {
            ui.label(head(tr("Average"))).on_hover_text(mean_tip);
        });
    });
    let body = egui::TextStyle::Body.resolve(ui.style());
    for k in shown {
        let r = &rows[k];
        // The full panel writes the card name alone (the tanks column shows where it goes).
        let name = if tanks.is_some() { r.name.clone() } else { fit_option(ui, &r.label, cols.label, &body) };
        let (label, reach, mean) = (RichText::new(name), RichText::new(chance(r)), RichText::new(row_mean(r, always)));
        let (label, reach, mean) = if stale {
            (label.color(STALE), reach.color(STALE), mean.color(STALE))
        } else if r.best {
            (label.strong().color(ACCENT), reach.color(ACCENT), mean.color(ACCENT))
        } else {
            (label.color(theme::TEXT), reach.color(theme::TEXT), mean.color(MUTED))
        };
        ui.horizontal(|ui| {
            ui.add_sized([cols.rank, h], egui::Label::new(RichText::new((k + 1).to_string()).color(MUTED)));
            ui.allocate_ui_with_layout(Vec2::new(cols.label, h), Layout::left_to_right(Align::Center), |ui| {
                ui.set_min_width(cols.label);
                // Keep only one tooltip (the full option name): when truncated, egui's own tooltip is turned off so
                // two do not stack.
                ui.add(egui::Label::new(label).truncate().show_tooltip_when_elided(false)).on_hover_text(&r.label);
            });
            if let Some(tanks) = tanks {
                let (rect, response) = ui.allocate_exact_size(Vec2::new(tanks_w, h), Sense::hover());
                // The reroll acts on no tank: its cells are left out.
                if !r.refresh {
                    tank_minis(ui.painter(), rect, tanks, &r.targets, stale);
                    response.on_hover_text(&r.label);
                }
            }
            ui.allocate_ui_with_layout(Vec2::new(cols.reach, h), Layout::right_to_left(Align::Center), |ui| {
                ui.label(reach).on_hover_text(reach_tip);
            });
            ui.allocate_ui_with_layout(Vec2::new(cols.mean, h), Layout::right_to_left(Align::Center), |ui| {
                ui.label(mean).on_hover_text(format!("{mean_tip}\n{}", grouped(r.mean.round() as i64)));
            });
        });
    }
    // Pad with blank rows when there are fewer than three (no advice, a protocol with no ranking), so the tanks below
    // do not shrink upward.
    for _ in blanks {
        ui.horizontal(|ui| {
            ui.add_sized([cols.rank, h], egui::Label::new(" "));
        });
    }
}

/// The chance to reach 840k, `94%`; `—` when the solver did not give it (the average has its own column).
fn chance(r: &AdviceRow) -> String {
    r.reached.map_or_else(|| "\u{2014}".to_string(), |p| format!("{:.0}%", p * 100.0))
}

/// The full ranking table (as it originally looked).
fn ranking_table(ui: &mut egui::Ui, rows: &[AdviceRow], stale: bool) {
    egui::Grid::new("vorax-ranking-all").striped(true).show(ui, |ui| {
        for title in [tr("Rank"), tr("Option"), tr("Expected score"), tr("840k chance")] {
            ui.label(RichText::new(title).small().weak());
        }
        ui.end_row();
        for (k, row) in rows.iter().enumerate() {
            let cell = |text: String| {
                let text = RichText::new(text).small();
                let text = if row.best { text.strong() } else { text };
                if stale { text.color(STALE) } else { text }
            };
            ui.label(cell((k + 1).to_string()));
            ui.label(cell(row.label.clone()));
            ui.label(cell(grouped(row.mean.round() as i64)));
            ui.label(cell(row.reached.map_or_else(|| "—".to_string(), |p| format!("{:.1}%", p * 100.0))));
            ui.end_row();
        }
    });
}

/// Height of a cell in the full panel's row of six tanks ([`tank_strip`]).
const CELL_H: f32 = 66.0;
/// Room above the row for the upper half of the card-name tag on a marked cell's top edge.
const TAG_ROOM: f32 = 8.0;
/// The gap between the small tank cells of the ranking's tanks column ([`tank_minis`]).
const MINI_GAP: f32 = 2.0;
/// How much of its own colour a dimmed tank keeps (the rest is the window background).
const DIM: f32 = 0.38;

/// The advised card's name and the tanks it acts on: the advised row when it is a card with target tanks; None for a
/// reroll, a card with no target tank, a protocol and no advice (nothing is marked then, nothing dimmed).
fn advised_targets(rows: &[AdviceRow]) -> Option<(&str, &[u8])> {
    rows.iter().find(|r| r.best && !r.refresh && !r.targets.is_empty()).map(|r| (r.name.as_str(), r.targets.as_slice()))
}

/// The six tanks in one row, as in the game (tank 1 to 6 left to right; 2026-11-08 layout the user confirmed): each
/// cell has from top to bottom the archetype icon (drawn, see [`archetype_icon`]), the monster's name in its rarity
/// colour and its total activity. The tanks the advised card acts on (`focus`: its name and tanks) get a gold cell, a
/// thick gold frame, a larger icon, a bold name and a gold tag with the card's name on the top edge; the other tanks
/// are dimmed. When the advice is stale (grey) the frame and tag are grey and nothing is dimmed. Fixed height: before
/// the tanks are read the six cells are outlines only, and the tag's room above is kept with or without a tag.
/// Hovering a cell shows the monster name, rarity and `活性 × 數量` (activity x count).
///
/// `compact` (the compact panel, 2026-11-08) draws short cells two small lines high: the icon with the total activity
/// beside it, then the name; the marked tanks keep the gold cell and frame (2 wide), the bold name and the larger
/// icon, but get no tag (the advice card right above names the card), and no room is kept for one. Below 300 wide
/// the gaps narrow to 2.
fn tank_strip(ui: &mut egui::Ui, tanks: &[read::Tank], focus: Option<(&str, &[u8])>, stale: bool, compact: bool) {
    const TANKS: usize = 6;
    let width = ui.available_width();
    let gap = if compact && width < 300.0 { 2.0 } else { 4.0 };
    let small = FontId::proportional(theme::MIN_TEXT);
    let line_h = ui.painter().layout_no_wrap("\u{570b}".to_string(), small.clone(), Color32::WHITE).size().y;
    let (room, cell_h) = if compact { (0.0, 2.0 * line_h + 2.0) } else { (TAG_ROOM, CELL_H) };
    let (strip, _) = ui.allocate_exact_size(Vec2::new(width, room + cell_h), Sense::hover());
    let w = ((width - gap * (TANKS as f32 - 1.0)) / TANKS as f32).max(16.0);
    let gold = if stale { STALE } else { theme::GOLD };
    let frame = if compact { 2.0 } else { 3.0 };
    for k in 0..TANKS {
        let rect = egui::Rect::from_min_size(egui::pos2(strip.left() + k as f32 * (w + gap), strip.top() + room), Vec2::new(w, cell_h));
        let marked = focus.is_some_and(|(_, ts)| ts.contains(&(k as u8)));
        let keep = if focus.is_some() && !marked && !stale { DIM } else { 1.0 };
        let p = ui.painter();
        let radius = (w * 0.12).clamp(3.0, 6.0);
        let Some(t) = tanks.get(k) else {
            p.rect_stroke(rect, radius, Stroke::new(1.0, DIVIDER), StrokeKind::Inside);
            continue;
        };
        let rarity = t.rarity.map_or(Color32::from_gray(150), rarity_colour);
        let (fill, edge) = match (marked, stale) {
            (true, false) => (mix(theme::PANEL, theme::GOLD, 0.26), Stroke::new(frame, theme::GOLD)),
            (true, true) => (theme::PANEL, Stroke::new(frame, STALE)),
            _ => (fade(Color32::from_rgb(0x1f, 0x22, 0x2a), keep), Stroke::new(1.0, fade(if t.empty { DIVIDER } else { rarity.gamma_multiply(0.55) }, keep))),
        };
        p.rect_filled(rect, radius, fill);
        p.rect_stroke(rect, radius, edge, StrokeKind::Inside);
        if t.empty {
            p.text(rect.center(), Align2::CENTER_CENTER, tr("Empty"), small.clone(), fade(MUTED, keep));
        } else if compact {
            // Line 1: the icon and the total activity beside it, centred together; line 2: the name.
            let (top, mid) = (rect.top() + 1.0 + line_h / 2.0, rect.top() + 1.0 + line_h * 1.5);
            let r = if marked { line_h * 0.5 } else { line_h * 0.38 };
            let total = t.total.map_or_else(|| "?".to_string(), short_k);
            let galley = one_line(p, &total, small.clone(), fade(Color32::from_gray(220), keep), w - 8.0 - 2.0 * r);
            let left = rect.center().x - (2.0 * r + 2.0 + galley.size().x) / 2.0;
            archetype_icon(p, egui::pos2(left + r, top), r, t.archetype, keep);
            p.galley(egui::pos2(left + 2.0 * r + 2.0, top - galley.size().y / 2.0), galley, Color32::from_gray(220));
            let name = t.name.clone().unwrap_or_else(|| "?".to_string());
            text_in(p, egui::pos2(rect.center().x, mid), &name, small.clone(), fade(rarity, keep), w - 6.0, marked);
        } else {
            let r = if marked { (w * 0.24).min(14.0) } else { (w * 0.17).min(10.0) };
            archetype_icon(p, egui::pos2(rect.center().x, rect.top() + 21.0), r, t.archetype, keep);
            let name = t.name.clone().unwrap_or_else(|| "?".to_string());
            let name_font = FontId::proportional(if marked { theme::MIN_TEXT + 1.0 } else { theme::MIN_TEXT });
            text_in(p, egui::pos2(rect.center().x, rect.top() + 43.0), &name, name_font, fade(rarity, keep), w - 6.0, marked);
            let total = t.total.map_or_else(|| "?".to_string(), short_k);
            text_in(p, egui::pos2(rect.center().x, rect.top() + 57.0), &total, small.clone(), fade(Color32::from_gray(220), keep), w - 6.0, false);
        }
        if marked && !compact && let Some((card, _)) = focus {
            // The tag on the top edge: gold with dark text (grey when stale), at most the cell's width plus the gaps.
            let font = FontId::proportional(theme::MIN_TEXT);
            let room = w + gap - 8.0;
            let galley = one_line(p, card, font, theme::NIGHT, room);
            let tag = egui::Rect::from_center_size(egui::pos2(rect.center().x, rect.top()), Vec2::new(galley.size().x + 8.0, galley.size().y + 2.0));
            p.rect_filled(tag, 3.0, gold);
            p.galley(tag.center() - galley.size() / 2.0, galley, theme::NIGHT);
        }
        let rarity_text = t.rarity.map_or("?", rarity_name);
        let values = match (t.activity, t.quantity) {
            (Some(a), Some(q)) => format!("{} × {}", grouped(a), grouped(q)),
            _ => "?".to_string(),
        };
        let name = t.name.clone().unwrap_or_else(|| "?".to_string());
        if !t.empty {
            ui.interact(rect, egui::Id::unique(("vorax-tank", k)), Sense::hover())
                .on_hover_text(trf!("{} ({})\nActivity \u{d7} count: {}\nTotal activity: {}", name, rarity_text, values, t.total.map_or_else(|| "?".to_string(), grouped)));
        }
    }
}

/// The small tanks of one ranking row (the tanks column): six cells with the archetype icon; the tanks the card
/// acts on (`targets`) are enlarged, framed in gold on a light gold cell, the others dimmed (grey frame and nothing
/// dimmed when the advice is `stale`). A card with no target tank has all six dimmed. Before the tanks are read the
/// cells are outlines only.
fn tank_minis(p: &egui::Painter, rect: egui::Rect, tanks: &[read::Tank], targets: &[u8], stale: bool) {
    const TANKS: usize = 6;
    let s = ((rect.width() - MINI_GAP * (TANKS as f32 - 1.0)) / TANKS as f32).min(rect.height() - 4.0).max(4.0);
    let used = s * TANKS as f32 + MINI_GAP * (TANKS as f32 - 1.0);
    let left = rect.center().x - used / 2.0;
    for k in 0..TANKS {
        let cell = egui::Rect::from_min_size(egui::pos2(left + k as f32 * (s + MINI_GAP), rect.center().y - s / 2.0), Vec2::splat(s));
        let marked = targets.contains(&(k as u8));
        let keep = if marked || stale { 1.0 } else { DIM };
        let Some(t) = tanks.get(k) else {
            p.rect_stroke(cell, 2.0, Stroke::new(1.0, DIVIDER), StrokeKind::Inside);
            continue;
        };
        if marked {
            let big = cell.expand(1.5);
            let (fill, edge) = if stale { (theme::PANEL, STALE) } else { (mix(theme::PANEL, theme::GOLD, 0.3), theme::GOLD) };
            p.rect_filled(big, 3.0, fill);
            p.rect_stroke(big, 3.0, Stroke::new(1.5, edge), StrokeKind::Inside);
        } else {
            p.rect_filled(cell, 2.0, fade(Color32::from_rgb(0x22, 0x25, 0x2e), keep));
        }
        if t.empty {
            p.line_segment([cell.center() - Vec2::new(s * 0.2, 0.0), cell.center() + Vec2::new(s * 0.2, 0.0)], Stroke::new(1.0, fade(MUTED, keep)));
        } else {
            archetype_icon(p, cell.center(), if marked { s * 0.46 } else { s * 0.36 }, t.archetype, keep);
        }
    }
}

/// `c` dimmed: mixed toward the window background, keeping `keep` of it (1 is unchanged).
fn fade(c: Color32, keep: f32) -> Color32 {
    if keep >= 1.0 { c } else { mix(theme::BG, c, keep) }
}

/// `a` mixed toward `b` by `t` (0 is `a`, 1 is `b`), opaque.
fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round().clamp(0.0, 255.0) as u8;
    Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}

/// `text` laid out on one line no wider than `max_w`, cut with `…` when longer.
fn one_line(p: &egui::Painter, text: &str, font: FontId, colour: Color32, max_w: f32) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_string(), font, colour);
    job.wrap.max_width = max_w.max(8.0);
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    p.layout_job(job)
}

/// One line of `text` centred on `at` (cut with `…` beyond `max_w`); `bold` draws it twice, a little apart (the font
/// has no bold face).
fn text_in(p: &egui::Painter, at: egui::Pos2, text: &str, font: FontId, colour: Color32, max_w: f32, bold: bool) {
    let galley = one_line(p, text, font, colour, max_w);
    let min = at - galley.size() / 2.0;
    if bold {
        p.galley(min + Vec2::new(0.6, 0.0), galley.clone(), colour);
    }
    p.galley(min, galley, colour);
}

/// The archetype's icon, drawn as shapes in the style of the game's tank icons (the user chose drawn shapes over the
/// game's images, 2026-11-08), centred on `c` with radius `r`, keeping `keep` of its colours (see [`fade`]):
/// - Pest: a purple bead, a dark inner circle and a small highlight at the upper left.
/// - Bone Sentry: a yellow helmet with a horn on each side and a dark T-shaped face slit.
/// - Agito: a red eye with rays: eight short rays, a red almond eye and a light pupil.
/// - Aemberon: a blue pinwheel: four curved blades around a dark centre.
///
/// An archetype not read is a grey circle.
fn archetype_icon(p: &egui::Painter, c: egui::Pos2, r: f32, archetype: Option<::vorax::Archetype>, keep: f32) {
    use ::vorax::Archetype;
    let col = |rgb: [u8; 3]| fade(Color32::from_rgb(rgb[0], rgb[1], rgb[2]), keep);
    let at = |x: f32, y: f32| egui::pos2(c.x + x * r, c.y + y * r);
    match archetype {
        Some(Archetype::Pest) => {
            p.circle_filled(c, r, col([0xa3, 0x5c, 0xdc]));
            p.circle_filled(c, r * 0.55, col([0x2b, 0x16, 0x42]));
            p.circle_filled(at(-0.5, -0.5), (r * 0.17).max(0.8), col([0xec, 0xd8, 0xff]));
        }
        Some(Archetype::BoneSentry) => {
            let yellow = col([0xe6, 0xb8, 0x3e]);
            let dark = col([0x3a, 0x2a, 0x10]);
            // Horns, then the helmet over their roots.
            p.add(egui::Shape::convex_polygon(vec![at(-1.0, -0.98), at(-0.28, -0.42), at(-0.62, 0.0)], yellow, Stroke::NONE));
            p.add(egui::Shape::convex_polygon(vec![at(1.0, -0.98), at(0.62, 0.0), at(0.28, -0.42)], yellow, Stroke::NONE));
            let mut helmet: Vec<egui::Pos2> = (0..=10).map(|k| {
                let a = std::f32::consts::PI * (1.0 + k as f32 / 10.0);
                at(0.68 * a.cos(), -0.08 + 0.68 * a.sin())
            }).collect();
            helmet.extend([at(0.68, 0.45), at(0.4, 0.82), at(-0.4, 0.82), at(-0.68, 0.45)]);
            p.add(egui::Shape::convex_polygon(helmet, yellow, Stroke::NONE));
            p.rect_filled(egui::Rect::from_min_max(at(-0.46, 0.04), at(0.46, 0.22)), 0.0, dark);
            p.rect_filled(egui::Rect::from_min_max(at(-0.1, 0.04), at(0.1, 0.72)), 0.0, dark);
        }
        Some(Archetype::Agito) => {
            let red = col([0xe0, 0x48, 0x50]);
            let ray = Stroke::new((r * 0.13).max(0.8), red);
            for k in 0..8 {
                let a = std::f32::consts::FRAC_PI_4 * k as f32;
                p.line_segment([at(0.74 * a.cos(), 0.74 * a.sin()), at(a.cos(), a.sin())], ray);
            }
            let n = 10;
            let mut eye: Vec<egui::Pos2> = (0..=n).map(|k| {
                let s = k as f32 / n as f32;
                at(-0.62 + 1.24 * s, -0.4 * (std::f32::consts::PI * s).sin())
            }).collect();
            eye.extend((1..n).rev().map(|k| {
                let s = k as f32 / n as f32;
                at(-0.62 + 1.24 * s, 0.4 * (std::f32::consts::PI * s).sin())
            }));
            p.add(egui::Shape::convex_polygon(eye, red, Stroke::NONE));
            p.circle_filled(c, (r * 0.2).max(0.8), col([0xff, 0xe2, 0xd6]));
        }
        Some(Archetype::Aemberon) => {
            let blue = col([0x4f, 0x9e, 0xec]);
            // Each blade is a lens from near the centre to a tip turned 35 degrees on, bulging more on one side, so
            // the four read as a pinwheel turning.
            for k in 0..4 {
                let a = std::f32::consts::FRAC_PI_2 * k as f32;
                let base = at(0.08 * a.cos(), 0.08 * a.sin());
                let turn = a + 0.7;
                let tip = at(0.98 * turn.cos(), 0.98 * turn.sin());
                let along = tip - base;
                let side = Vec2::new(-along.y, along.x).normalized();
                let n = 8;
                let mut blade: Vec<egui::Pos2> = (0..=n).map(|j| {
                    let s = j as f32 / n as f32;
                    base + along * s - side * (0.5 * r * (std::f32::consts::PI * s).sin())
                }).collect();
                blade.extend((1..n).rev().map(|j| {
                    let s = j as f32 / n as f32;
                    base + along * s + side * (0.1 * r * (std::f32::consts::PI * s).sin())
                }));
                p.add(egui::Shape::convex_polygon(blade, blue, Stroke::NONE));
            }
            p.circle_filled(c, (r * 0.26).max(0.8), col([0x10, 0x22, 0x44]));
        }
        None => {
            p.circle_stroke(c, r * 0.8, Stroke::new(1.0, col([0x90, 0x90, 0x90])));
        }
    }
}

/// Rarity colours: white = normal, blue = magic, gold = rare, red = boss (the same colours as monster names in the
/// game).
fn rarity_colour(r: Rarity) -> Color32 {
    match r {
        Rarity::Normal => Color32::from_gray(200),
        Rarity::Magic => Color32::from_rgb(90, 150, 255),
        Rarity::Rare => Color32::from_rgb(230, 180, 60),
        Rarity::Boss => Color32::from_rgb(225, 70, 85),
    }
}

/// An endgame battle number, rounded down and never up, with a lower-case k as [`short_k`]: 1,116,900 → "1.11M"
/// (rounding to the nearest gave "1.12M", read as the 1.12M reward tier reached), 812,345 → "812.3k"; "?" when not
/// read.
fn battle_k(v: Option<f64>) -> String {
    v.map_or_else(|| "?".to_string(), |v| if v >= 1e6 { format!("{:.2}M", (v / 1e4).floor() / 100.0) } else { format!("{:.1}k", (v / 100.0).floor() / 10.0) })
}

/// Whether the result card of the game that ended at `ended_at` (None: no game kept) shows, the card dismissed by a
/// Tartarus reading being `dismissed` (see [`VoraxView::dismiss_result`]).
fn card_shows(ended_at: Option<i64>, dismissed: Option<i64>) -> bool {
    ended_at.is_some_and(|at| dismissed != Some(at))
}

/// [`theme::line`], on two lines only when one line would cut `text` short (the result card at window width 296).
fn fitted_line(ui: &mut egui::Ui, text: &str, colour: Color32) {
    let rows = if theme::elided(ui, text, ui.available_width(), 1) { 2 } else { 1 };
    theme::lines(ui, text, colour, rows);
}

/// An option's expected final activity as the panel writes it (`812k`, see [`short_k`]); a dash for one the solver
/// did not play out (see [`played_out`]).
fn mean_k(r: &AdviceRow) -> String {
    if played_out(r) { short_k(r.mean.round() as i64) } else { "\u{2014}".to_string() }
}

/// Whether the solver gave the option a value: a card always taken (a Xenopanacea, a Holy Water rite) is advised
/// without a playout, with no chance and a mean of 0, which is no value.
fn played_out(r: &AdviceRow) -> bool {
    r.reached.is_some() || r.mean != 0.0
}

/// Whether the advice `best`, on the hand `hand` (the cards' English names), is a card the solver always takes by its
/// rule (`is_always_take`: a Xenopanacea, a Holy Water rite, a Dissection), advised without a playout (no end
/// reached). Its value is that rule, not a number: the panel and the value on the game write "Always take".
fn always_taken(hand: Option<&[String]>, best: Option<&::vorax::advisor::Best>) -> bool {
    let (Some(hand), Some(::vorax::advisor::Best::Card(r))) = (hand, best) else { return false };
    r.end.is_none() && hand.get(r.index).is_some_and(|name| ::vorax::advisor::is_always_take(name))
}

/// A top-three row's value cell: "Always take" for the advised card when it is always taken (`always`), else its
/// expected final activity ([`mean_k`]).
fn row_mean(r: &AdviceRow, always: bool) -> String {
    if always && r.best { tr("Always take").to_string() } else { mean_k(r) }
}

/// The advised option's value as the panel writes it ([`row_mean`]), for the value on the game: "Always take" for a
/// card always taken (`always`), else its expected final activity; None when the advice is stale (the last usable
/// one, greyed out), there are no boxes, no row is the advised one, or it has no value.
fn advised_k(view: &View, always: bool) -> Option<String> {
    if view.stale || view.boxes.is_empty() {
        return None;
    }
    view.advice.as_ref()?.rows.iter().find(|r| r.best).filter(|r| always || played_out(r)).map(|r| row_mean(r, always))
}

/// Number abbreviation: 1234 → "1.2k", 88128 → "88k", 840000 → "840k", 1250000 → "1.25M".
fn short_k(n: i64) -> String {
    let a = n.unsigned_abs();
    if a >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    } else if a >= 10_000 {
        format!("{:.0}k", n as f64 / 1000.0)
    } else if a >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// Group digits with thousands commas: 130459 → "130,459".
fn grouped(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if n < 0 {
        out.push('-');
    }
    for (k, c) in digits.chars().enumerate() {
        if k > 0 && (digits.len() - k).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The value on the game is the advised row's value as the panel's top three writes it (the same `row_mean`), not
    /// the first row's; none when the advice is stale, there is no box, or no advice. A card always taken by the
    /// solver's rule writes "Always take" in both; a card with no value that is not that rule writes nothing on the
    /// game and a dash in the panel.
    #[test]
    fn the_value_on_the_game_is_the_panels_number() {
        let row = |mean: f64, best: bool| AdviceRow { name: String::new(), targets: vec![], label: String::new(), mean, reached: Some(0.4), refresh: false, best };
        let rows = vec![row(812_400.0, false), row(694_600.0, true), row(600_000.0, false)];
        let mut view = View {
            advice: Some(crate::vorax::AdviceRows { headline: String::new(), note: None, rows: rows.clone(), method: String::new(), unknown: vec![] }),
            boxes: vec![vorax_session::MarkBox { what: "card 2".into(), at: (1416.0, 1140.0, 110.0, 185.0), style: BoxStyle::Plain }],
            ..View::default()
        };
        assert_eq!(advised_k(&view, false).as_deref(), Some("695k"));
        // The panel's own cell for that row.
        assert_eq!(advised_k(&view, false), Some(row_mean(&rows[1], false)));
        view.stale = true;
        assert_eq!(advised_k(&view, false), None, "stale advice");
        assert_eq!(advised_k(&view, true), None, "stale, even always taken");
        view.stale = false;
        view.boxes.clear();
        assert_eq!(advised_k(&view, false), None, "no box (the gate after a confirm)");
        assert_eq!(advised_k(&View::default(), false), None, "no advice");
        // A card always taken is advised without a playout (no chance, mean 0): "Always take" on the game and in the
        // panel's cell for it.
        let taken = AdviceRow { name: String::new(), targets: vec![], label: String::new(), mean: 0.0, reached: None, refresh: false, best: true };
        view.boxes = vec![vorax_session::MarkBox { what: "card 1".into(), at: (1205.0, 1140.0, 110.0, 185.0), style: BoxStyle::Plain }];
        view.advice = Some(crate::vorax::AdviceRows { headline: String::new(), note: None, rows: vec![taken.clone()], method: String::new(), unknown: vec![] });
        assert_eq!(advised_k(&view, true).as_deref(), Some(tr("Always take")));
        assert_eq!(row_mean(&taken, true), tr("Always take"));
        // The same row without that rule: no value, a dash in the panel.
        assert_eq!(advised_k(&view, false), None);
        assert_eq!(row_mean(&taken, false), "\u{2014}");
        // A value worked out as 0 is a value, not the rule.
        let zero = AdviceRow { name: String::new(), targets: vec![], label: String::new(), mean: 0.0, reached: Some(0.0), refresh: false, best: true };
        assert_eq!(row_mean(&zero, false), "0");
    }

    /// "Always take" goes by the solver's rule on the advised card's name, advised without a playout; not by a mean
    /// of 0.
    #[test]
    fn always_taken_goes_by_the_rule() {
        use ::vorax::advisor::{Best, End, Ranked};
        let hand: Vec<String> = ["Xenopanacea", "Bone Salve"].map(String::from).to_vec();
        let card = |index: usize, end: Option<End>| Best::Card(Ranked { index, targets: vec![], value: 0.0, end, no_effect: None });
        assert!(always_taken(Some(hand.as_slice()), Some(&card(0, None))));
        assert!(!always_taken(Some(hand.as_slice()), Some(&card(1, None))), "not an always-take card");
        assert!(!always_taken(Some(hand.as_slice()), Some(&card(0, Some(End { activity: 0.0, reached: 0.0 })))), "played out");
        assert!(!always_taken(Some(hand.as_slice()), Some(&Best::Refresh)));
        assert!(!always_taken(None, Some(&card(0, None))), "no board for the advice");
        assert!(!always_taken(Some(hand.as_slice()), None));
    }

    #[test]
    fn numbers_are_grouped_by_thousands() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(840), "840");
        assert_eq!(grouped(130_459), "130,459");
        assert_eq!(grouped(1_000_000), "1,000,000");
        assert_eq!(grouped(-812_340), "-812,340");
    }

    #[test]
    fn numbers_are_shortened_for_the_bars() {
        assert_eq!(short_k(840), "840");
        assert_eq!(short_k(1234), "1.2k");
        assert_eq!(short_k(88_128), "88k");
        assert_eq!(short_k(840_000), "840k");
        assert_eq!(short_k(1_250_000), "1.25M");
    }

    /// Advice and status (solving, advice) are written straight onto the advice card on every frame, without the
    /// warning area's flicker guard: as soon as the advice appears the card switches to it, with no 0.5 second delay.
    #[test]
    fn advice_and_status_skip_the_debounce() {
        let advice = crate::vorax::AdviceRows { headline: "建議：重擲".into(), note: Some("聖水一定拿".into()), rows: vec![], method: "推演".into(), unknown: vec![] };
        // Just computed: the card is the new advice at once, and the status `建議` (advice) does not enter the
        // warning area.
        let v = View { status: Status::Advised, lines: vec!["建議".into()], advice: Some(advice.clone()), rollouts: 4096, ..View::default() };
        assert_eq!(card_text(&v).0, "建議：重擲");
        // The solver's note belongs to the advice: shown live, and not in the warnings that need the flicker guard.
        assert!(note_items(&v).is_empty());
        assert_eq!(now_items(&v).into_iter().map(|(t, _)| t).collect::<Vec<_>>(), ["聖水一定拿"]);
        // Recomputing (old advice greyed): `求解中` (solving) is written in the card's small text and does not enter
        // the warning area.
        let v = View { status: Status::Solving, lines: vec!["求解中".into()], advice: Some(advice), stale: true, ..View::default() };
        assert_eq!(card_text(&v), ("建議：重擲".to_string(), "求解中".to_string()));
        assert!(note_items(&v).iter().all(|(t, _)| t != "求解中"));
        // No advice, solving: the card says `計算中…` (computing).
        let v = View { status: Status::Solving, lines: vec!["求解中".into()], ..View::default() };
        assert_eq!(card_text(&v).0, tr("Solving..."));
        assert!(note_items(&v).is_empty());
        // Unreadable: the first line is on the card, the reason goes into the warning area (only these need the
        // flicker guard).
        let v = View { status: Status::Unreadable, lines: vec!["讀不到：".into(), "卡 2 的名稱".into()], ..View::default() };
        assert_eq!(card_text(&v).1, "讀不到：");
        assert_eq!(note_items(&v).into_iter().map(|(t, _)| t).collect::<Vec<_>>(), ["卡 2 的名稱"]);
    }

    /// The line for ranks 4-6: those already written in the top three are not repeated; when the advice is ranked 5th
    /// it is in the third row, and this line writes 3, 4, 6.
    #[test]
    fn rest_line_lists_the_options_below_the_bars() {
        let row = |label: &str, best: bool| AdviceRow { name: String::new(), targets: vec![], label: label.into(), mean: 812_000.0, reached: Some(0.5), refresh: false, best };
        let five: Vec<AdviceRow> = ["甲", "乙", "丙", "丁", "戊"].iter().map(|l| row(l, *l == "甲")).collect();
        assert_eq!(rest_text(&five), "4 丁 50% 812k \u{b7} 5 戊 50% 812k");
        let three: Vec<AdviceRow> = five[..3].to_vec();
        assert_eq!(rest_text(&three), "");
        let mut moved = five.clone();
        moved[0].best = false;
        moved[4].best = true;
        assert_eq!(rest_text(&moved), "3 丙 50% 812k \u{b7} 4 丁 50% 812k");
    }

    /// Battle numbers are rounded down with a lower-case k: 812.3k + 304.6k = 1,116,900 is "1.11M", not "1.12M",
    /// which reads as the 1.12M reward tier reached.
    #[test]
    fn battle_numbers_round_down_with_lower_case_k() {
        assert_eq!(battle_k(Some(1_116_900.0)), "1.11M");
        assert_eq!(battle_k(Some(1_120_000.0)), "1.12M");
        assert_eq!(battle_k(Some(812_345.0)), "812.3k");
        assert_eq!(battle_k(Some(812_399.0)), "812.3k");
        assert_eq!(battle_k(Some(999_999.0)), "999.9k");
        assert_eq!(battle_k(None), "?");
        // The result card's total and next tier (the ASCENDANT_PREVIEW_VORAX_RESULT=battle game).
        let b = read::Battle { wave: Some((3, 3)), activity: Some(812_300.0), bonus: Some(304_600.0), next: Some(1_120_000.0), percent: Some(37), raw: String::new() };
        let g = GameSummary { log_dir: None, ended_at: 0, board: Some(812_345), board_from: Some(BoardFrom::ResultScreen), rounds: 13, instruments: Vec::new(), battle: Some(b) };
        let (numbers, reward) = result_battle(&g);
        assert!(numbers.contains("812.3k") && numbers.contains("304.6k") && numbers.contains("1.11M") && !numbers.contains('K'), "{numbers}");
        assert!(reward.contains("1.12M"), "{reward}");
        // No battle: no reward line to draw.
        let none = GameSummary { battle: None, ..g };
        assert!(result_battle(&none).1.is_empty());
    }

    /// A final activity from the battle panel just over 840k is unsure on the card; an exact one is not given a
    /// tolerance. Tolerance at 843.5k: 100 + ceil(843,500 * 0.5%) = 4,318, so 840k..=844,317 is unsure.
    #[test]
    fn card_calls_a_near_battle_panel_value_unsure() {
        let card = |board: i64, from: BoardFrom| GameSummary { log_dir: None, ended_at: 0, board: Some(board), board_from: Some(from), rounds: 13, instruments: Vec::new(), battle: None };
        let target = ::vorax::value::TARGET;
        assert_eq!(card_arrival(&card(843_500, BoardFrom::Battle), target), Some(Arrival::Unsure));
        assert_eq!(card_arrival(&card(845_000, BoardFrom::Battle), target), Some(Arrival::Reached));
        assert_eq!(card_arrival(&card(843_500, BoardFrom::LastBoard), target), Some(Arrival::Reached));
        assert_eq!(card_arrival(&card(839_900, BoardFrom::Battle), target), Some(Arrival::Missed));
        let none = GameSummary { board: None, ..card(0, BoardFrom::Battle) };
        assert_eq!(card_arrival(&none, target), None);
    }

    /// The result card gives way to the Tartarus board once a Tartarus reading dismisses it; a later game's card
    /// shows again.
    #[test]
    fn a_dismissed_result_card_does_not_show() {
        assert!(!card_shows(None, None));
        assert!(card_shows(Some(1_000), None));
        assert!(!card_shows(Some(1_000), Some(1_000)));
        assert!(card_shows(Some(2_000), Some(1_000)));
    }

    /// The endgame battle card's text: which wave, the activity bonus and percentage, the next tier or all rewards
    /// taken.
    #[test]
    fn battle_card_text() {
        let b = read::Battle { wave: Some((2, 3)), activity: Some(568_500.0), bonus: Some(221_200.0), next: Some(854_000.0), percent: Some(39), raw: String::new() };
        let (big, small) = battle_text(&b);
        assert_eq!(big, trf!("Endgame battle \u{b7} wave {}/{}", 2, 3));
        assert_eq!(small, format!("{} \u{b7} {} \u{b7} {}", trf!("Activity {} + bonus {}", "568.5k", "221.2k"), trf!("next {}", "854.0k"), trf!("drops +{}%", 39)));
        let full = read::Battle { wave: Some((3, 3)), next: Some(999_900_000.0), activity: Some(1_260_000.0), ..b.clone() };
        assert!(battle_text(&full).1.contains(tr("all rewards taken")));
        assert!(battle_text(&full).1.contains("1.26M"));
        assert!(!battle_text(&full).1.contains('K'));
        // No ranking during battle.
        let v = with_battle(&View::default(), &b);
        assert!(in_battle(&v) && advice_rows(&v).is_empty() && now_items(&v).is_empty());
    }

    /// Potion kit: both Chinese and English names are recognised (the English UI is not misjudged as "reroll dropped
    /// midway").
    #[test]
    fn kit_is_recognised_in_both_languages() {
        let row = |label: &str, refresh: bool, best: bool| AdviceRow { name: String::new(), targets: vec![], label: label.into(), mean: 800_000.0, reached: Some(0.5), refresh, best };
        assert!(is_kit("藥劑箱（大）") && is_kit("稀有藥劑箱（小）") && is_kit("Potion Kit (Large)") && is_kit("Rare Potion Kit (Small)"));
        assert!(!is_kit("迷魂酊劑 \u{2192} 水箱 1") && !is_kit("Reroll"));
        assert_eq!(advice_reason(&[row("Reroll", true, false), row("Potion Kit (Small)", false, true)]), Some(tr("Potion kit first, no reroll")));
        assert_eq!(advice_reason(&[row("Reroll", true, false), row("Ectoplasm \u{2192} Tank 1", false, true)]), Some(tr("Reroll dropped early \u{b7} fewer runs")));
    }

    /// The Vorax advice card's reasons: a reroll ranked first but potion kit opened, b ranking by average score, c
    /// reroll dropped midway.
    #[test]
    fn reason_follows_the_best_row() {
        let row = |label: &str, reached: f64, refresh: bool, best: bool| AdviceRow { name: String::new(), targets: vec![], label: label.into(), mean: 800_000.0, reached: Some(reached), refresh, best };
        // a: row 0 is a reroll, the advice is a potion kit.
        assert_eq!(advice_reason(&[row("重擲", 0.5, true, false), row("藥劑箱（小）", 0.4, false, true)]), Some(tr("Potion kit first, no reroll")));
        // b: the advice is row 0, but row 1 has a higher 840k chance.
        assert_eq!(advice_reason(&[row("甲", 0.5, false, true), row("乙", 0.6, false, false)]), Some(tr("Ranked by average, not 840k chance")));
        // c: row 0 is a reroll, the advice is not a potion kit.
        assert_eq!(advice_reason(&[row("重擲", 0.5, true, false), row("迷魂酊劑 → 水箱 1", 0.4, false, true)]), Some(tr("Reroll dropped early \u{b7} fewer runs")));
        // Other cases where best is not row 0 (should not happen in principle): no reason is written.
        assert_eq!(advice_reason(&[row("甲", 0.5, false, false), row("乙", 0.4, false, true)]), None);
        // The advice is row 0 and its chance is also the highest: no reason.
        assert_eq!(advice_reason(&[row("甲", 0.6, false, true), row("乙", 0.5, false, false)]), None);
    }

    #[test]
    fn verdict_compares_the_advice_with_the_runner_up() {
        let row = |mean: f64, reached: Option<f64>, best: bool| AdviceRow { name: String::new(), targets: vec![], label: String::new(), mean, reached, refresh: false, best };
        // 5 percentage points of chance difference, 5% expected score difference: clearly better.
        assert_eq!(verdict(&[row(800_000.0, Some(0.60), true), row(760_000.0, Some(0.55), false)], false), Some(Verdict::Clear));
        // Chance differs by less than 1 percentage point: either is fine.
        assert_eq!(verdict(&[row(800_000.0, Some(0.60), true), row(760_000.0, Some(0.595), false)], false), Some(Verdict::Either));
        // No chance, expected score differs by less than 1%: either is fine.
        assert_eq!(verdict(&[row(800_000.0, None, true), row(795_000.0, None, false)], false), Some(Verdict::Either));
        // When the advice is in the second row, compare against the first row.
        assert_eq!(verdict(&[row(800_000.0, Some(0.30), false), row(799_000.0, Some(0.50), true)], false), Some(Verdict::Either));
        // Only one row, or the advice is stale: no label.
        assert_eq!(verdict(&[row(800_000.0, Some(0.6), true)], false), None);
        assert_eq!(verdict(&[row(800_000.0, Some(0.60), true), row(500_000.0, Some(0.1), false)], true), None);
    }

    /// The English keys this file looks up (`tr("...")` and `trf!("...")` outside the tests), unescaped.
    fn panel_keys() -> Vec<String> {
        let source = include_str!("vorax_view.rs");
        let code = &source[..source.find("\nmod tests {").expect("the tests module")];
        let calls = code.match_indices("tr(\"").chain(code.match_indices("trf!(\""));
        let mut keys = Vec::new();
        for (start, _) in calls {
            // Only the calls themselves, not a name ending in `tr`.
            if code[..start].chars().next_back().is_some_and(|c| c.is_alphanumeric() || c == '_') {
                continue;
            }
            let rest = &code[start..];
            let mut chars = rest[rest.find('"').unwrap() + 1..].chars();
            let mut key = String::new();
            while let Some(c) = chars.next() {
                match c {
                    '"' => break,
                    '\\' => match chars.next() {
                        Some('n') => key.push('\n'),
                        Some('u') => {
                            let hex: String = chars.by_ref().skip(1).take_while(|&c| c != '}').collect();
                            key.push(char::from_u32(u32::from_str_radix(&hex, 16).unwrap()).unwrap());
                        }
                        Some(c) => key.push(c),
                        None => break,
                    },
                    c => key.push(c),
                }
            }
            keys.push(key);
        }
        keys
    }

    /// Every English key the panel looks up has its Traditional Chinese in src/lang/zh_tw.rs.
    #[test]
    fn every_panel_key_has_chinese() {
        let keys = panel_keys();
        assert!(keys.len() >= 40, "only {} keys found", keys.len());
        let missing: Vec<&String> = keys.iter().filter(|k| crate::lang::tr_in(crate::lang::Lang::ZhTw, Box::leak(k.to_string().into_boxed_str())) == k.as_str()).collect();
        assert!(missing.is_empty(), "no Chinese for {missing:?}");
    }

    /// The labels that were written in Chinese before they went through `tr()` read exactly as before in Chinese.
    #[test]
    fn the_chinese_panel_reads_as_before() {
        use crate::lang::{Lang, fill, tr_in};
        let zh = |en: &'static str| tr_in(Lang::ZhTw, en);
        assert_eq!(zh("Activity"), "活性");
        assert_eq!(fill(zh("Final activity {} (target {})"), &[&"612,000", &"840,000"]), "最終活性 612,000（目標 840,000）");
        assert_eq!([zh("Full ranking"), zh("Board as read")], ["全部排名", "讀到的盤面"]);
        let rerolls = format!("{} \u{b7} {}", fill(zh("Potion rerolls {}"), &[&2]), fill(zh("Instrument rerolls {}"), &[&"?"]));
        assert_eq!(rerolls, "藥劑重擲 2 \u{b7} 用具重擲 ?");
        assert_eq!(fill(zh("{}{} runs"), &[&"藥劑：每個選項模擬到終局 \u{b7} ", &4096]) + &fill(zh(", {} s"), &[&"1.23"]), "藥劑：每個選項模擬到終局 \u{b7} 4096 局，1.23 秒");
        assert_eq!([zh("Option"), zh("Rank"), zh("Expected score"), zh("840k chance")], ["選項", "排名", "期望分數", "達840k"]);
        assert_eq!([zh("Empty"), zh(", ")], ["空", "、"]);
        let tip = fill(zh("{} ({})\nActivity \u{d7} count: {}\nTotal activity: {}"), &[&"吸血蝠", &"稀有", &"1,200 × 3", &"3,600"]);
        assert_eq!(tip, "吸血蝠（稀有）\n活性 × 數量：1,200 × 3\n總活性：3,600");
    }

    /// The text the panel draws, laid out off screen at 421 wide.
    fn drawn_text(draw: impl Fn(&mut egui::Ui)) -> String {
        drawn_at(421.0, draw).0
    }

    /// The text the panel draws, laid out off screen at `width` wide, one line per text shape, and the rectangles it
    /// strokes as (stroke width, colour).
    fn drawn_at(width: f32, draw: impl Fn(&mut egui::Ui)) -> (String, Vec<(f32, Color32)>) {
        let ctx = egui::Context::default();
        let input = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(width, 900.0))), ..Default::default() };
        let mut output = ctx.run_ui(input, |ui| draw(ui));
        // egui asserts in debug builds that texture updates are taken before the output is dropped.
        output.textures_delta.clear();
        fn walk(shape: &egui::Shape, out: &mut String, strokes: &mut Vec<(f32, Color32)>) {
            match shape {
                egui::Shape::Text(t) => {
                    out.push_str(t.galley.text());
                    out.push('\n');
                }
                egui::Shape::Rect(r) if r.stroke.width > 0.0 => strokes.push((r.stroke.width, r.stroke.color)),
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| walk(s, out, strokes)),
                _ => {}
            }
        }
        let (mut out, mut strokes) = (String::new(), Vec::new());
        output.shapes.iter().for_each(|c| walk(&c.shape, &mut out, &mut strokes));
        (out, strokes)
    }

    /// The compact panel draws the 2026-11-08 layout too (the user saw the old one in a 397x392 window): the six
    /// tanks in a row, each named (an empty one `Empty`), the advised card's two tanks framed in gold (2 wide) and
    /// the rest not, and the ranking's tanks column (from 300 wide); stale advice frames them grey, and with no advice
    /// nothing is framed. At 421 wide and in the narrow fallback.
    #[test]
    fn the_compact_panel_marks_the_advised_tanks() {
        let named = |which: &str| {
            let mut view = preview_view(which);
            for (k, t) in view.tanks.iter_mut().enumerate() {
                t.name = Some(format!("T{}", k + 1));
            }
            view.tanks[5].empty = true;
            view
        };
        let frames = |strokes: &[(f32, Color32)], colour: Color32| strokes.iter().filter(|&&(w, c)| w == 2.0 && c == colour).count();
        let view = named("advice");
        assert_eq!(advised_targets(advice_rows(&view)).map(|(_, ts)| ts), Some(&[1u8, 4][..]));
        for width in [421.0, 240.0] {
            let (text, strokes) = drawn_at(width, |ui| draw_panel_compact(&view, ui, false));
            for k in 1..=5 {
                assert!(text.lines().any(|l| l == format!("T{k}")), "tank {k} at {width}:\n{text}");
            }
            assert!(!text.lines().any(|l| l == "T6") && text.lines().any(|l| l == "Empty"), "{text}");
            assert!(text.lines().any(|l| l == "Option"), "{text}");
            // The ranking's tanks column from 300 wide; below, the option names keep `→ tank N`.
            assert_eq!(text.lines().any(|l| l == "Tanks"), width >= 300.0, "{text}");
            assert_eq!(text.lines().any(|l| l.contains('\u{2192}') && l.ends_with('3')), width < 300.0, "{text}");
            assert_eq!(frames(&strokes, theme::GOLD), 2, "gold frames at {width}");
        }
        let (_, strokes) = drawn_at(421.0, |ui| draw_panel_compact(&named("stale"), ui, false));
        assert_eq!((frames(&strokes, theme::GOLD), frames(&strokes, STALE)), (0, 2));
        let (text, strokes) = drawn_at(421.0, |ui| draw_panel_compact(&named("none"), ui, false));
        assert_eq!(frames(&strokes, theme::GOLD) + frames(&strokes, STALE), 0);
        assert!(text.lines().any(|l| l == "T1") && text.lines().any(|l| l == "Tanks"), "{text}");
    }

    /// Chinese characters and punctuation; the full-width plus and minus of the compact links are symbols.
    fn chinese(c: char) -> bool {
        matches!(c, '\u{3000}'..='\u{303f}' | '\u{3400}'..='\u{4dbf}' | '\u{4e00}'..='\u{9fff}') || (matches!(c, '\u{ff00}'..='\u{ffef}') && !matches!(c, '\u{ff0b}' | '\u{ff0d}'))
    }

    /// In the English window, the panel (both layouts, the battle card and the result card) draws no Chinese when the
    /// game's text it is given is English.
    #[test]
    fn the_english_panel_draws_no_chinese() {
        assert_eq!(crate::lang::lang(), crate::lang::Lang::En, "the tests run in English");
        let tank = |empty: bool, archetype: ::vorax::Archetype| read::Tank { empty, name: Some("Leech".into()), archetype: Some(archetype), rarity: Some(Rarity::Rare), activity: Some(1200), quantity: Some(3), total: Some(3600), ..read::Tank::default() };
        let tanks = vec![tank(false, ::vorax::Archetype::Pest), tank(false, ::vorax::Archetype::Aemberon), tank(true, ::vorax::Archetype::Pest), tank(false, ::vorax::Archetype::BoneSentry), tank(false, ::vorax::Archetype::Agito), tank(true, ::vorax::Archetype::Pest)];
        let row = |label: &str, best: bool| AdviceRow { name: String::new(), targets: vec![], label: label.into(), mean: 812_000.0, reached: Some(0.5), refresh: false, best };
        let rows = vec![row("Bewitching Tincture \u{2192} Tank 1", true), row("Reroll", false), row("Potion Kit (Large)", false), row("Bone Saw", false)];
        let method = "Potions: each option played to the end; mean is the final Ultimate Activity, capped at 840k.".to_string();
        let advice = crate::vorax::AdviceRows { headline: "Bewitching Tincture \u{2192} Tank 1".into(), note: Some("Something else.".into()), rows, method, unknown: vec![] };
        let view = View {
            status: Status::Advised,
            round: Some(5),
            ultimate: Some(612_000),
            potion_rerolls: Some(2),
            instrument_rerolls: None,
            advice: Some(advice),
            rollouts: 4096,
            solve_ms: Some(1234),
            board: Some("tank 1".into()),
            tanks,
            held: vec![("Scalpel".into(), true), ("Bone Saw".into(), false)],
            lines: vec!["Advised".into()],
            ..View::default()
        };
        let battle = read::Battle { wave: Some((2, 3)), activity: Some(568_500.0), bonus: Some(221_200.0), next: Some(854_000.0), percent: Some(39), raw: String::new() };
        let game = GameSummary { log_dir: None, ended_at: 0, board: Some(812_345), board_from: Some(BoardFrom::ResultScreen), rounds: 13, instruments: vec!["Scalpel".into()], battle: Some(battle.clone()) };
        let texts = [
            drawn_text(|ui| draw_panel(&view, ui, false)),
            drawn_text(|ui| draw_panel_compact(&view, ui, false)),
            drawn_text(|ui| draw_panel(&with_battle(&view, &battle), ui, false)),
            drawn_text(|ui| result_panel(&game, ui, false)),
            drawn_text(|ui| result_panel(&game, ui, true)),
        ];
        for text in &texts {
            assert!(text.contains("Activity"), "{text}");
            assert!(!text.chars().any(chinese), "Chinese in the English panel:\n{text}");
        }
        assert!(texts[0].contains("Potion rerolls 2") && texts[0].contains("Instrument rerolls ?") && texts[0].contains("Full ranking"), "{}", texts[0]);
        assert_eq!(card_text(&view).1, "Potions: each option played to the end; mean is the final Ultimate Activity, capped at 840k. 4096 runs, 1.23 s");
        assert_eq!(card_brief(&view), "Potions: each option played to the end \u{b7} 4096 runs, 1.23 s");
        assert!(texts[1].contains("Board as read"), "{}", texts[1]);
    }
}
