//! The window: reads on the read hotkey, the Read button, a saved screenshot
//! or by itself when the game's screen changes (auto read, turned on and off
//! by the Auto button or the auto read hotkey). It has three tabs. Board shows
//! the screen's ring of cells (click a cell to correct it) with the pick and
//! the other good picks marked on their cells, the pick written in the
//! courtyard inside the ring, and under it the three moves the playouts rated best.
//! Run holds the held instruments and the run theme, and Settings the keys
//! and options. The window moves itself to the empty area
//! under the game's instrument panel once, when the game is first found.
//! Under the tab row it offers to install itself when it runs from elsewhere,
//! asks once whether to share run logs, and offers to update once installed,
//! when GitHub has a newer release.
//!
//! Ascendant layout (2026-11-05): the tabs, Read and Auto buttons sit in a vertical navigation rail on the left
//! ([`AscendantApp::rail`]);
//! the board page runs top to bottom as the small header line, the advice card with its "Clearly better" /
//! "Either is fine" label, the top three (numbers only), and the cell ring;
//! colours and small widgets live in [`super::theme`].
//!
//! Fixed skeleton (2026-11-05): the status bar on the top line ([`AscendantApp::status_bar`]) and every block
//! of the board page and the Vorax panel keep a fixed position and height, so a screen change only swaps
//! content; the warning area is fixed at two lines with a flicker guard ([`theme::notes`]).

use super::capture::{Event, Watcher, capture, game_in_front, game_pid, game_rect, on_screen};
use super::elevation;
use super::mark::{self, Mark, Notice};
use super::gamelog::{self, Fill};
use super::hotkey::{Hotkey, Recorded, parse};
use super::session::{CellEdit, Session};
use super::theme::{self, ACCENT, ADVICE_TEXT, BAR_IDLE, DIVIDER, MIN_TEXT, MUTED, SIDEBAR, Switch, Verdict};
use super::through::ClickThrough;
use super::vorax_session::Config;
use super::vorax_view::{self, VoraxView};
use crate::solver::advise::also_good;
use crate::solver::solve::SolveOption;
use super::runlog::{RunLog, folder_name};
use super::settings::{self, Settings};
use super::settle::{self, Cause, Outcome, RunKind, Settled};
use super::share::{self, Uploader};
use super::update::{self, Release};
use super::install::{self, OcrInstall, OcrLang};
use super::report::{Game, RunEntry};
use super::run_review::RunReview;
use crate::board::{CellModel, EventMark, Model};
use crate::engine::data::{Beast, RAINBOW, Tool, data};
use crate::engine::text::{beast_name, cell_label, hunt_name, incident_term, inmate_name, instrument, instrument_term, objective_name, theme_name, tier_name};
use crate::engine::{Action, ChoiceId};
use crate::lang::{self, Lang, tr, trf};
use crate::reader::load_png;
use crate::reader::model::WarningKind;
use crate::reader::panels::Screen;
use crate::reader::ocr;
use eframe::egui::{self, Align, Align2, Color32, FontId, Layout, Pos2, Rect, RichText, Sense, Shape, Stroke, StrokeKind, Vec2};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

/// The box given by [`AscendantApp::mark_spot`]: its centre on screen, half width, half height, screen pixels
/// per reference pixel, plus the game's bounds; when there is no box, the reason.
type MarkSpot = Result<(f32, f32, f32, f32, f32, [i32; 4]), &'static str>;

/// A running update: its thread's outcome and the [`update::Step`] it is at.
type Updating = (Receiver<Result<(), update::Failed>>, Arc<AtomicU8>);

/// The update check's thread: a verified newer release, none, or why not, with the ETag to send next time and the
/// wait GitHub asked for.
type Checking = Receiver<update::Checked>;

/// Space between two board tiles.
const GAP: f32 = 4.0;
const WARDEN: Color32 = Color32::from_rgb(240, 200, 60);
const MARKER: Color32 = Color32::from_rgb(255, 140, 40);
/// Width of the left navigation rail.
const RAIL: f32 = 32.0;
/// Minimum width kept for the key-cap label on the settings page (on a narrow window it truncates to `Ctrl+…`
/// and never to just `…`).
const KEYCAP_MIN: f32 = 50.0;
/// The top left of the empty area under the game's Objective and instrument
/// panels, in a 1920x1080 client area, low enough to leave room for the
/// instruments' tooltips.
const PLACE: (f32, f32) = (15.0, 560.0);
/// How long the window size must hold still after the player's drag before
/// the Board tab fits the window to the board again.
const RESIZE_SETTLE: Duration = Duration::from_millis(400);
/// How long after a font size change (including the one applied at startup) a change of the window size or
/// position in points is not treated as a player action.
const RESCALE_SETTLE: Duration = Duration::from_millis(400);
/// Window height (points) the full layout needs at the standard font size: measured in the sixth round on
/// 2026-11-05 (after the small text became two lines). At a window of 296×560 the scroll content of the full
/// Vorax panel (the tallest page) is 429.2 high and Tartarus 385.8; adding the status bar and margins gives
/// about 467, so 470 is used.
/// The default window of 296×520 is the full layout and the user's 421×351 is the compact one. In auto mode it
/// switches to compact below `FULL_HEIGHT - LAYOUT_HYSTERESIS` and back to full above
/// `FULL_HEIGHT + LAYOUT_HYSTERESIS` (see [`next_compact`]).
const FULL_HEIGHT: f32 = 470.0;
const LAYOUT_HYSTERESIS: f32 = 10.0;
/// Options of the "Font size" setting on the settings page: standard, large, extra large, with their overall
/// scale (font spec point 4).
const TEXT_SCALES: [(f32, &str); 3] = [(1.0, "Standard"), (1.15, "Large"), (1.3, "Extra large")];

/// The window's tabs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Board,
    Run,
    /// Accumulated progress (Vorax, Tartarus); see [`AscendantApp::progress_section`].
    Progress,
    /// The map affixes' switch, frame style and grades (`affix_page.rs`).
    Affix,
    Settings,
}

/// Which global hotkey: read now, or turn auto read on or off. Click-through has no hotkey since 1.0.2: the
/// navigation rail's "Thru" light turns it on and off (its clicks stay in the window, see [`widget_rects`]).
#[derive(Clone, Copy, PartialEq)]
enum Which {
    Read,
    Auto,
}

impl Which {
    const ALL: [Which; 2] = [Which::Read, Which::Auto];

    fn name(self) -> &'static str {
        match self {
            Which::Read => "read",
            Which::Auto => "auto read",
        }
    }

    /// The saved key.
    fn setting(self, settings: &mut Settings) -> &mut String {
        match self {
            Which::Read => &mut settings.read_hotkey,
            Which::Auto => &mut settings.auto_hotkey,
        }
    }
}

/// A hotkey being recorded on the settings page (see [`AscendantApp::record_key`]): which one, and why the last key
/// pressed was refused, shown in its row in place of "Press a key..." until the next key: short enough for a narrow
/// window's key cap, and in full on hover.
struct Recording {
    which: Which,
    refused: Option<Refusal>,
}

/// Why a key was refused while recording: a short line for the key cap and the whole reason for its hover.
#[derive(Debug, PartialEq)]
struct Refusal {
    short: String,
    full: String,
}

/// What recording a hotkey does with the key pressed (see [`judge`]).
#[derive(Debug, PartialEq)]
enum Judged {
    /// Esc: stop, the hotkey as it was.
    Cancel,
    /// Try to register this key.
    Try(String),
    /// Refused, with the reason shown in the row; recording goes on.
    Refuse(Refusal),
}

/// What recording a hotkey does with `got`, the other hotkey being `other` (its saved text and which it is). A key
/// that is the other's, or one that would take a key from typing, is refused before Windows is asked.
fn judge(got: Recorded, other: (&str, Which)) -> Judged {
    match got {
        Recorded::Cancel => Judged::Cancel,
        Recorded::Clipboard => Judged::Refuse(Refusal { short: tr("Used by copy and paste").into(), full: tr("It would take copy, cut or paste from every program").into() }),
        Recorded::Unusable => Judged::Refuse(Refusal { short: tr("Not a key for a hotkey").into(), full: tr("Use F1 to F24, or a letter or digit with Ctrl or Alt").into() }),
        Recorded::Typing(text) => Judged::Refuse(Refusal { short: trf!("{} needs Ctrl/Alt", text), full: trf!("{} alone would get in the way of typing: add Ctrl or Alt", text) }),
        Recorded::Key(text) if parse(&text).is_ok() && parse(&text) == parse(other.0) => {
            Judged::Refuse(Refusal { short: trf!("Already the {} key", tr(other.1.name())), full: trf!("{} is already the {} key.", text, tr(other.1.name())) })
        }
        Recorded::Key(text) => Judged::Try(text),
    }
}

/// A registered hotkey, the text its row shows (the saved key) and
/// the flag its presses set.
#[derive(Default)]
struct Key {
    hotkey: Option<Hotkey>,
    text: String,
    pressed: Arc<AtomicBool>,
}

pub struct AscendantApp {
    session: Session,
    settings: Settings,
    ctx: egui::Context,
    /// The read and auto read hotkeys, indexed by [`Which`].
    keys: [Key; 2],
    /// Clicks on the window's empty parts pass to the one under it. Saved (`Settings::click_through`) and restored at
    /// start: the rail's "Thru" light, which always takes clicks, turns it off.
    click_through: bool,
    /// The page's scroll area the mouse stays in under click-through, so the wheel scrolls it ([`wheel_area`]).
    wheel_area: Option<Rect>,
    /// The thread that lets those clicks through, while `click_through` is on.
    through: Option<ClickThrough>,
    /// The window's opacity as last set, see [`see_through`].
    see_through: Option<u8>,
    /// The last capture, read, hotkey or settings error.
    error: Option<String>,
    /// The screenshot on show, when one was opened.
    path: String,
    /// The screen cell whose correction window is open.
    editing: Option<u8>,
    /// Auto read's capture thread, while auto read is on.
    watcher: Option<Watcher>,
    /// The window title the watcher looks for.
    watched: String,
    /// The game log the watcher was started with (`Settings::game_log`): a change rebuilds it.
    watched_log: Option<PathBuf>,
    /// "Choose..." of the game log row was pressed: the file dialog opens after the frame is drawn, with the window
    /// as its parent (see [`AscendantApp::pick_game_log`]).
    pick_log: bool,
    /// "Open screenshot..." was pressed: the file dialog opens after the frame is drawn, owned by this window (see
    /// [`AscendantApp::open`]).
    pick_png: bool,
    /// "Verify downloaded files..." was pressed: the folder dialog opens after the frame is drawn, owned by this
    /// window (see [`AscendantApp::start_verify`]).
    pick_verify: bool,
    /// For previews (ASCENDANT_PREVIEW_LOG): the game log status shown instead of the watcher's; Some(None) is as if
    /// auto read were off.
    preview_log: Option<Option<gamelog::LogStatus>>,
    /// Why auto read can't see the game, shown quietly.
    auto_status: Option<String>,
    /// The window auto read's capture session is on, as the watcher describes it.
    watching: Option<String>,
    /// Why Windows OCR can't read the game (no English OCR language).
    ocr_problem: Option<String>,
    /// The window was moved to its saved place or to the game once, or the
    /// player moved it first; it is never moved again, so a window the
    /// player drags stays put.
    placed: bool,
    /// Where `place` put the window, in physical pixels.
    placed_at: Option<[i32; 2]>,
    /// The window's outer top left last frame, in physical pixels, to tell
    /// the player's move.
    last_pos: Option<[i32; 2]>,
    /// The window's inner size last frame, to tell the player's drag.
    last_size: Option<Vec2>,
    /// The size the app last set the window to (a fit or a nudge) and when,
    /// so its arrival is not taken for the player's drag.
    asked: Option<(Vec2, Instant)>,
    /// Until when the window is taken to be in the player's drag, with no fit.
    resizing_until: Option<Instant>,
    /// The player resized the window since the last nudge; see [`AscendantApp::track_size`].
    dragged: bool,
    /// The height to set back once a nudge's extra height has arrived.
    nudge_back: Option<f32>,
    /// The tab on show.
    tab: Tab,
    /// The warnings of the last reading, notes left out, so the Board tab
    /// opens when a reading brings the first.
    warned: usize,
    /// This exe; None when Windows can't tell, which hides install and update.
    current: Option<PathBuf>,
    /// This exe runs from the install folder.
    installed: bool,
    /// The update check's thread, until it answers.
    checking: Option<Checking>,
    /// A newer release, once the check finds one.
    release: Option<Release>,
    /// Why the update check failed, shown quietly in the Settings tab.
    check_error: Option<update::Problem>,
    /// A check answered (with or without a newer release), for the "Up to date" line.
    checked: bool,
    /// The running check is the hourly one (see [`AscendantApp::start_check`]).
    check_quiet: bool,
    /// `release` was found by the hourly check: only a red dot on the Settings button tells of it, the status bar
    /// stays as it is, so nothing moves under a player's fast clicks (see [`AscendantApp::update_dot`]).
    release_quiet: bool,
    /// The API's ETag and the outcome of the last verified check, sent with the next check ([`update::check_tagged`]).
    seen: Option<update::Seen>,
    /// When the hourly check runs next ([`update::next_check`]); None until a check has answered.
    next_check: Option<Instant>,
    /// The verified notes an update kept for this version ([`update::load_notes_in`]): "Updated to X.Y.Z" shows while
    /// there are some (the text shown is the built-in one when there is, see [`release_notes`]).
    this_notes: Option<update::SavedNotes>,
    /// The release notes of every version, newest first: the ones built into the exe, and `this_notes` only for a
    /// version that has none built in ([`release_notes`]).
    notes_history: Vec<update::SavedNotes>,
    /// "What's new" and the instruments and theme window, never both open (see [`Floats`]).
    floats: Floats,
    /// The version a download refused as unverified: later checks do not offer it again ([`update::drop_refused`]).
    /// Kept in memory only, so a restart checks it afresh.
    refused: Option<[u64; 3]>,
    /// An hourly check's answer that came while the Settings tab was on show: applied once the player leaves it,
    /// so the update group does not change under the pointer (the red dot shows a release in it at once).
    held: Option<update::Checked>,
    /// For previews (ASCENDANT_PREVIEW_UPDATED=1): whether "Updated to" shows, whatever `updated_seen` says
    /// (dismissing it then touches no setting); None outside that preview.
    preview_updated: Option<bool>,
    /// The update's thread (download, verify, swap, restart) and the step it is at ([`update::Step`]), while it
    /// runs.
    updating: Option<Updating>,
    /// Why the last update failed ([`update::Failed::text`] says what state it left), shown in the Settings tab.
    update_error: Option<update::Failed>,
    /// An update failed after the exe was moved ([`update::Failed::Lost`] or [`update::Failed::NotRestarted`]): the
    /// running exe is no longer the one on disk, so "Update" stays off until Ascendant is started again.
    update_locked: bool,
    /// The "Verify downloaded files..." thread, while it runs, and its last answer.
    verifying: Option<Receiver<Result<update::Verified, update::Problem>>>,
    verified: Option<Result<update::Verified, update::Problem>>,
    /// For previews (ASCENDANT_PREVIEW_UPDATE): keep the settings page scrolled to the update group.
    preview_update_scroll: bool,
    /// Sends the finished runs' logs while sharing is on; None when APPDATA is not set.
    uploader: Option<Uploader>,
    /// Why the last run log upload stopped, shown quietly in the Settings tab.
    upload_error: Option<String>,
    /// The run log's folder last seen: a change means the run before it is
    /// finished, so it is sent.
    log_dir: Option<PathBuf>,
    /// The mark on what to click on the game, made when first shown.
    mark: Option<Mark>,
    /// Why a box was not drawn although there is advice ([`Session::mark_or_why`] and
    /// [`AscendantApp::mark_spot`]), recorded into the view log.
    mark_hidden: Option<&'static str>,
    /// What the window and the mark showed when last logged, see
    /// [`AscendantApp::log_view`].
    last_view: Option<serde_json::Value>,
    /// The line shown on the game itself, see [`Session::notice`].
    notice: Option<Notice>,
    /// The numbers on the targets of a planned chain, see [`AscendantApp::show_chain`].
    badges: Vec<Notice>,
    /// The advised move's expected value on the game, above the mark's spot (see [`AscendantApp::highlight`]).
    value: Option<Notice>,
    /// The screen, board and advised action the player clicked in the game
    /// after: no mark until a reading changes one of them, so the re-reads of
    /// the same screen as the game moves on after the click leave it hidden.
    clicked: Option<(Screen, Model, Action)>,
    /// When `clicked` was set, and when the reading on show then was read: with no move in the game's log since, the
    /// mark comes back (see [`click_unconfirmed`]).
    clicked_at: Option<(Instant, Instant)>,
    /// When the game's log last said a move was played (see [`confirms_click`]).
    move_seen: Option<Instant>,
    /// The left button's clicks on the game, from press to release (see [`mark::Clicks`]), for `mark_spot`.
    clicks: mark::Clicks,
    /// Live advice for Vorax surgery: while the mode is on, frames and game log hints go to it and the Tartarus
    /// Session is paused.
    vorax: VoraxView,
    /// Private builds only: map page frames saved as samples for the affix reader, with observe mode on (see
    /// `affix_samples.rs`); made on the first map page event, dropped when observe mode or the run logs are off.
    #[cfg(feature = "dev-samples")]
    affix_samples: Option<super::affix_samples::AffixSamples>,
    /// The affix frames on the game while a map page shows (see `affix_live.rs`).
    affix_live: super::affix_live::AffixLive,
    /// The affix page's own state (search text, default table).
    affix_page: super::affix_page::AffixPage,
    /// The affixes learned from the game log (`affix-learned.tsv`, see `affix::learned`).
    affix_learned: crate::affix::learned::Store,
    /// The hotkey being recorded on the settings page, after its "Change" was pressed: the next key pressed becomes it
    /// (see [`AscendantApp::record_key`]). Both hotkeys are released meanwhile, so the key pressed reaches this window
    /// and does nothing else.
    recording: Option<Recording>,
    /// At this frame's start a text box had the keyboard or a hotkey was being recorded: Esc is theirs, so it does
    /// not close "What's new" (see [`esc_closes`]).
    esc_busy: bool,
    /// The Run tab's review: the automatic settlements of the last Tartarus run and Vorax game (see
    /// [`settle::spawn`], [`super::vorax_settle::spawn`]), the run picked to review instead, and the export card.
    review: RunReview,
    /// The `ended_at` of the last Vorax game a settlement was started for (or skipped, with no log folder).
    vorax_ended: Option<i64>,
    /// When (UTC ms) the Tartarus run on show was first seen over, for the review's time; taken by the settlement.
    run_over_at: Option<i64>,
    /// Whether the run was over last frame (to see it become over).
    was_over: bool,
    /// Whether the board page mentions the settlement under "The run is over": only if a settlement started during
    /// this end screen (a run that does not end on the last day is not settled, so the previous run's result must
    /// not be quoted).
    over_reviewed: bool,
    /// The "Restore defaults" confirmation window is open.
    restoring: bool,
    /// Whether the layout is currently compact (see [`next_compact`]); None before the first frame.
    compact: Option<bool>,
    /// When the font size was last changed, see [`AscendantApp::rescaling`].
    rescaled: Option<Instant>,
    /// For testing (only when the environment variable ASCENDANT_PREVIEW_SCROLL=bottom is set): scroll the tab
    /// to the very bottom, used to capture the lower half of the settings page.
    preview_bottom: bool,
    /// For testing (ASCENDANT_PREVIEW_SCROLL=hotkeys): keep the settings page scrolled to the "Advice and controls"
    /// group (the hotkeys); with ASCENDANT_PREVIEW_MEASURE the measure line also gives where that group is, from the
    /// content's top (`marks`), to compare the page with and without the administrator notice.
    preview_hotkeys: bool,
    /// For testing (ASCENDANT_PREVIEW_SCROLL=<points>): the page scrolled that far down, to capture a long page
    /// (Settings) from top to bottom in parts.
    preview_scroll_y: Option<f32>,
    /// The settings page's positions for the measure line (see `preview_hotkeys`).
    preview_marks: String,
    /// For testing (only when the environment variable ASCENDANT_PREVIEW_MEASURE is set): write the content
    /// height of the scroll area into this file every frame.
    preview_measure: Option<PathBuf>,
    /// For testing (only when the environment variable ASCENDANT_PREVIEW_TOOLTIP=1 is set): always show the
    /// "Expected loot" tooltip.
    preview_tooltip: bool,
    /// For testing (only when the environment variable ASCENDANT_PREVIEW_TOOLTIP=progress is set): always show
    /// the progress button tooltip.
    preview_tooltip_progress: bool,
    /// For testing (ASCENDANT_PREVIEW_TOOLTIP=icon, cell or row): always show that tooltip of the first top-three
    /// row (the action icon's meaning, the small cell's contents, or the move's full label).
    preview_tooltip_move: Option<MoveTip>,
    /// For testing (only when the environment variable ASCENDANT_PREVIEW_NOT_FIRST=1 is set): replace the
    /// advice with the 2nd ranked move on screen.
    preview_not_first: bool,
    /// For testing (only when the environment variable ASCENDANT_PREVIEW_TOOLTIP=card is set): always show the
    /// advice card tooltip.
    preview_tooltip_card: bool,
    /// For testing (only when the environment variable ASCENDANT_PREVIEW_RUN_OVER=1 is set): the board page
    /// behaves as if the run is over.
    preview_run_over: bool,
    /// For testing (only when the environment variable ASCENDANT_PREVIEW_CHARGES is set, for example
    /// vorax:48,tartarus:46): use these fake values instead of the accumulated progress read from the game log.
    preview_charges: Option<gamelog::Charges>,
    /// For previews (ASCENDANT_PREVIEW_PICK with ASCENDANT_PREVIEW_PICK_AT=<seconds>): the run picked that many
    /// seconds after start, as a pick in the run picker would (to capture before, while and after it settles).
    preview_pick: Option<(Instant, PathBuf)>,
    /// For previews (ASCENDANT_PREVIEW_SWITCH=<page>@<seconds>): the page switched to that many seconds after start,
    /// as a press on its rail button would (to capture the windows closing with the page).
    preview_switch: Option<(Instant, Tab)>,
    /// The OCR language being installed and its thread (see [`install::install_ocr`]).
    ocr_installing: Option<(OcrLang, Receiver<OcrInstall>)>,
    /// How the last OCR install ended.
    ocr_result: Option<(OcrLang, OcrInstall)>,
    /// For previews (ASCENDANT_PREVIEW_NO_OCR): as if no OCR language were installed, the system's OCR untouched.
    preview_no_ocr: bool,
    /// Whether the game runs with more rights than Ascendant (then the hotkeys do nothing while it is in front).
    rights: elevation::GameRights,
    /// "Restart as administrator" waiting for Windows' prompt (see [`elevation::relaunch`]).
    relaunching: Option<Receiver<elevation::Relaunch>>,
    /// The player answered No to Windows' prompt (from the notice, or "Start as administrator" at start): the
    /// status bar's administrator notice says so.
    relaunch_declined: bool,
    /// The administrator notice's "x": not shown again while Ascendant stays open (not saved).
    elevated_dismissed: bool,
    /// For previews (ASCENDANT_PREVIEW_ELEVATED=1, or =declined: as if the game ran as administrator; =0: as if it
    /// did not): in place of what [`elevation::GameRights`] found.
    preview_elevated: Option<bool>,
    /// For previews (ASCENDANT_PREVIEW_AUTO_WAITING=<text>): the status bar's "Auto read waiting" item with that
    /// text, with auto read off (the game is not captured), to show the "+N" colour of a plain item.
    preview_auto_waiting: Option<String>,
    /// For previews (ASCENDANT_PREVIEW_STATUS_OPEN=1): the floating list of the administrator notice's text kept open.
    preview_status_open: bool,
    /// For previews (ASCENDANT_PREVIEW_RECORD): a hotkey row recording; the window not being in front does not cancel it.
    preview_recording: bool,
}

impl AscendantApp {
    /// `current` is this exe, and `installed` whether it runs from the install folder.
    pub fn new(cc: &eframe::CreationContext<'_>, settings: Settings, preload: Option<&Path>, current: Option<PathBuf>, installed: bool) -> AscendantApp {
        cc.egui_ctx.set_theme(egui::Theme::Dark);
        theme::apply(&cc.egui_ctx);
        add_ui_font(&cc.egui_ctx);
        text_sizes(&cc.egui_ctx);
        // Turn off egui's built-in Ctrl+plus/minus zoom: it steps by 10% at a time, does not match the three
        // "Font size" levels and would leave the setting and the screen inconsistent; an accidental press
        // during a game would also suddenly change the layout. The font size is changed only from the settings
        // page.
        cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);
        let mut session = Session::new(settings.rollouts, settings.held_tools(), settings.theme, settings.tools_days);
        session.log = settings.run_log.then(RunLog::in_appdata).flatten();
        let ctx = cc.egui_ctx.clone();
        session.on_progress(move || ctx.request_repaint());
        let vorax = VoraxView::new(&settings, cc.egui_ctx.clone());
        let this_notes = settings::app_dir().and_then(|d| update::load_notes_in(&d, env!("CARGO_PKG_VERSION")));
        let mut app = AscendantApp {
            notes_history: release_notes(this_notes.as_ref()),
            this_notes,
            session,
            keys: [
                Key { text: settings.read_hotkey.clone(), ..Key::default() },
                Key { text: settings.auto_hotkey.clone(), ..Key::default() },
            ],
            click_through: false,
            wheel_area: None,
            through: None,
            see_through: None,
            settings,
            ctx: cc.egui_ctx.clone(),
            error: None,
            path: String::new(),
            editing: None,
            watcher: None,
            watched: String::new(),
            watched_log: None,
            pick_log: false,
            pick_png: false,
            pick_verify: false,
            preview_log: None,
            auto_status: None,
            watching: None,
            ocr_problem: ocr::problem(),
            placed: false,
            placed_at: None,
            last_pos: None,
            last_size: None,
            asked: None,
            resizing_until: None,
            dragged: false,
            nudge_back: None,
            tab: Tab::Board,
            warned: 0,
            current,
            installed,
            checking: None,
            release: None,
            check_error: None,
            checked: false,
            check_quiet: false,
            release_quiet: false,
            seen: None,
            next_check: None,
            floats: Floats::default(),
            refused: None,
            held: None,
            preview_updated: None,
            updating: None,
            update_error: None,
            update_locked: false,
            verifying: None,
            verified: None,
            preview_update_scroll: false,
            // With uploads off, the upload thread is not started, so the share consent notice does not appear
            // either (see `AscendantApp::notice`).
            uploader: settings::app_dir().filter(|_| share::ENABLED).map(|d| {
                let ctx = cc.egui_ctx.clone();
                Uploader::start(d.join("runs"), move || ctx.request_repaint())
            }),
            upload_error: None,
            log_dir: None,
            mark: None,
            mark_hidden: None,
            last_view: None,
            notice: None,
            badges: Vec::new(),
            value: None,
            clicked: None,
            clicked_at: None,
            move_seen: None,
            clicks: mark::Clicks::default(),
            vorax,
            #[cfg(feature = "dev-samples")]
            affix_samples: None,
            affix_live: {
                let ctx = cc.egui_ctx.clone();
                super::affix_live::AffixLive::new(move || ctx.request_repaint())
            },
            affix_page: Default::default(),
            affix_learned: crate::affix::learned::Store::open(learned_path()),
            recording: None,
            esc_busy: false,
            review: RunReview::default(),
            vorax_ended: None,
            run_over_at: None,
            was_over: false,
            over_reviewed: false,
            restoring: false,
            compact: None,
            rescaled: None,
            preview_bottom: false,
            preview_hotkeys: false,
            preview_scroll_y: None,
            preview_marks: String::new(),
            preview_measure: None,
            preview_tooltip: false,
            preview_tooltip_progress: false,
            preview_tooltip_move: None,
            preview_not_first: false,
            preview_tooltip_card: false,
            preview_run_over: false,
            preview_charges: None,
            preview_pick: None,
            preview_switch: None,
            ocr_installing: None,
            ocr_result: None,
            preview_no_ocr: false,
            rights: elevation::GameRights::current(),
            relaunching: None,
            relaunch_declined: false,
            elevated_dismissed: false,
            preview_elevated: None,
            preview_auto_waiting: None,
            preview_status_open: false,
            preview_recording: false,
        };
        app.set_text_scale(&cc.egui_ctx, app.settings.font_scale);
        // Previews (ASCENDANT_PREVIEW_UPDATE, ASCENDANT_PREVIEW_UPDATED) inject the update state instead and never ask
        // GitHub, not even hourly (no check answers, so none is scheduled).
        if app.settings.check_updates && !preview_update() {
            app.start_check(false);
        }
        app.watch();
        for which in Which::ALL {
            let text = which.setting(&mut app.settings).clone();
            match app.register(which, &text) {
                Ok(h) => app.keys[which as usize].hotkey = Some(h),
                Err(e) => app.error = Some(e),
            }
        }
        app.click_through = app.settings.click_through;
        if let Some(p) = preload {
            app.read_png(p);
        }
        // "Start as administrator" tried at start and this copy carried on: declined at Windows' prompt (the notice says
        // so) or failed.
        match elevation::at_start() {
            Some(elevation::Relaunch::Cancelled) => app.relaunch_declined = true,
            Some(elevation::Relaunch::Failed(e)) => app.error = Some(trf!("Could not restart as administrator: {}", e)),
            Some(elevation::Relaunch::Started) | None => {}
        }
        // Started again as administrator under another Windows account (one answered at Windows' prompt): that
        // account's settings and run logs are in use, which the player would otherwise not see.
        if elevation::other_account() {
            app.error = Some(tr("Ascendant was started again as another Windows account, so that account's settings and run logs are in use. To use your own, close it and start Ascendant yourself").to_string());
        }
        // The test entry point goes last: without auto read, watch() forces Vorax mode off, so the Vorax
        // preview has to come after it.
        app.preview_entry();
        app.configure_affixes();
        app.look_back_affixes();
        app
    }

    /// Registers `text` as the `which` hotkey, unless it is another one's.
    fn register(&mut self, which: Which, text: &str) -> Result<Hotkey, String> {
        for other in Which::ALL.into_iter().filter(|&w| w != which) {
            if parse(text).is_ok() && parse(text) == parse(other.setting(&mut self.settings)) {
                return Err(trf!("{} is already the {} key.", text, tr(other.name())));
            }
        }
        let (flag, ctx) = (self.keys[which as usize].pressed.clone(), self.ctx.clone());
        Hotkey::register(text, move || {
            flag.store(true, Ordering::SeqCst);
            ctx.request_repaint();
        })
    }

    /// Logs a "view" line whenever what the player sees changes: the advice
    /// the window shows (none while the playouts run), whether the mark is
    /// on the game, the screen of the reading and its kept line. Advice lines
    /// log only each solve's end, so a change under the player's eyes went
    /// unseen (2026-10-30).
    fn log_view(&mut self) {
        let Some(log) = &self.session.log else { return };
        let reading = self.session.reading.as_ref();
        let view = serde_json::json!({
            "advice": self.session.shown_advice().map(|a| a.label),
            "solving": self.session.solving,
            "mark": self.mark.as_ref().is_some_and(Mark::shown),
            "mark_at": self.mark.as_ref().and_then(Mark::at),
            "hidden": self.mark_hidden,
            // Which window was in front instead of the game (diagnostics, see `capture::front_window`).
            "front": if self.mark_hidden == Some("behind") { super::capture::front_window() } else { None },
            "screen": reading.map(|r| format!("{:?}", r.screen)),
            "line": reading.and_then(|r| r.kept_lines().first().map(|l| l.to_string())),
            // A Cage Fight's chain badges on the game, cell numbers in order.
            "chain": self.badges.iter().any(Notice::shown).then(|| self.session.chain_plan().iter().map(|&n| n + 1).collect::<Vec<_>>()),
        });
        if self.last_view.as_ref() != Some(&view) {
            log.event("view", SystemTime::now(), view.clone());
            self.last_view = Some(view);
        }
    }

    /// Shows the mark on what to click on the game while the setting is on
    /// and [`AscendantApp::mark_spot`] gives one; hides it otherwise. Looks
    /// again every 50 ms while the session has a mark, shown or not, so a
    /// window move, a click or the game coming back in front is seen.
    /// With "Show the expected value on the game" on, the advised move's expected loot ([`loot_text`], the number the
    /// window shows) goes just above the same spot, whether the mark is on or not, and goes when the mark's spot does.
    fn highlight(&mut self, ctx: &egui::Context) {
        let value = if self.settings.show_value { self.session.shown_advice().and_then(|a| loot_text(&self.session, a.option)) } else { None };
        let spot = if self.settings.highlight || value.is_some() { self.session.mark_or_why() } else { Err("off") };
        if spot.is_ok() {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
        // A Cage Fight's target screen: the chain's numbered badges stand in for the mark (see `Session::mark_or_why`).
        let badges = spot == Err("badges");
        let placed = spot.and_then(|spot| self.mark_spot(spot));
        // With no advice there is no box to draw, so nothing counts as hidden; with the mark off, that is why.
        self.mark_hidden = if self.settings.highlight {
            match placed {
                Err(why) if why != "no_advice" => Some(why),
                _ => None,
            }
        } else {
            self.session.shown_advice().is_some().then_some("off")
        };
        let mut at = None;
        match placed.ok() {
            Some((x, y, half_w, half_h, unit, game)) => {
                // A target pick paints its targets red and gold: its mark
                // is the loud one.
                let pick = self.session.reading.as_ref().is_some_and(|r| matches!(r.screen, Screen::ChainTarget | Screen::CellPick));
                let style = if pick { mark::Style::Loud } else { mark::Style::Plain };
                if self.settings.highlight {
                    if self.mark.is_none() {
                        self.mark = Mark::new();
                    }
                    // A mark growing in is drawn again each frame until in place.
                    if let Some(mark) = &mut self.mark
                        && mark.show([x, y, half_w, half_h, unit], style, game)
                    {
                        ctx.request_repaint();
                    }
                } else if let Some(mark) = &mut self.mark {
                    mark.hide();
                }
                // The value goes with the spot the mark holds (it keeps where it was first marked), else the one read.
                let spot = self.mark.as_ref().and_then(Mark::spot).unwrap_or([x, y, half_w, half_h, unit]);
                at = Some((spot, style, game[1]));
            }
            None => {
                if let Some(mark) = &mut self.mark {
                    mark.hide();
                }
            }
        }
        // On the badges' screen the value goes above the first badge's cell, the bite the advice starts, as it would
        // above a mark there; with the mark setting off the badges are not drawn, but the value is.
        if at.is_none() && badges && value.is_some() {
            let title = &self.settings.window_title;
            let game = if game_in_front(title) { game_rect(title).ok() } else { None };
            at = first_badge_spot(&self.session.chain_spots(), game).map(|(spot, top)| (spot, mark::Style::Loud, top));
            ctx.request_repaint_after(Duration::from_millis(50));
        }
        match (value, at) {
            (Some(text), Some(([x, y, _, half_h, unit], style, game_top))) => {
                if self.value.is_none() {
                    self.value = Notice::value();
                }
                if let Some(v) = &mut self.value {
                    let (above, below) = mark::reach(half_h, unit, style);
                    v.show_value(&text, x, (y - above, y + below), game_top as f32, unit);
                }
            }
            _ => {
                if let Some(v) = &mut self.value {
                    v.hide();
                }
            }
        }
    }

    /// Shows the session's notice (see [`Session::notice`]) on the game,
    /// centred above the board where the days counter sits, while the mark
    /// setting is on and the game or this window is in front; hides it
    /// otherwise. Looks again every 200 ms while there is one, so a window
    /// move is followed.
    /// Numbers the targets of the chain the advice starts (a Cage Fight run,
    /// see [`Session::chain_plan`]) on the game in the order to click them,
    /// at each cell's top left corner, clear of the portrait, while the mark
    /// setting is on and the game or this window is in front.
    fn show_chain(&mut self, ctx: &egui::Context) {
        let title = &self.settings.window_title;
        let spots = if self.settings.highlight && game_in_front(title) { self.session.chain_spots() } else { Vec::new() };
        let rect = if spots.is_empty() { None } else { game_rect(title).ok() };
        let Some([left, top, _, height]) = rect else {
            self.badges.iter_mut().for_each(Notice::hide);
            return;
        };
        ctx.request_repaint_after(Duration::from_millis(200));
        let unit = height as f32 / 1080.0;
        while self.badges.len() < spots.len() {
            let Some(badge) = Notice::badge() else { break };
            self.badges.push(badge);
        }
        for (k, badge) in self.badges.iter_mut().enumerate() {
            match spots.get(k) {
                Some(&(number, (x, y, half_w, half_h))) => {
                    let centre = left as f32 + x - half_w - BADGE_LEFT * unit;
                    badge.show(&number.to_string(), centre, top as f32 + y - half_h, unit * BADGE_SCALE, 200.0 * unit);
                }
                None => badge.hide(),
            }
        }
    }

    fn show_notice(&mut self, ctx: &egui::Context) {
        let title = &self.settings.window_title;
        let text = self.settings.highlight.then(|| self.session.notice()).flatten();
        // A notice may come (a problem that lasts) or go (the run's end) with nothing read.
        if text.is_some() || !self.session.problems.is_empty() || self.session.run_over.is_some() {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
        let at = text.filter(|_| game_in_front(title)).and_then(|text| {
            let [left, top, width, height] = game_rect(title).ok()?;
            Some((text, left as f32 + width as f32 / 2.0, top as f32 + height as f32 * 8.0 / 1080.0, height as f32 / 1080.0, width as f32 * 0.6))
        });
        match at {
            Some((text, x, y, unit, max_width)) => {
                if self.notice.is_none() {
                    self.notice = Notice::new();
                }
                if let Some(notice) = &mut self.notice {
                    notice.show(&text, x, y, unit, max_width);
                }
            }
            None => {
                if let Some(notice) = &mut self.notice {
                    notice.hide();
                }
            }
        }
    }

    /// The mark in physical screen px (centre, half width, half height, and
    /// screen px per reference px) and the game's rect: the session's `(x, y, half_w, half_h)`
    /// (see [`Session::mark`]) while the game or this window is in front and
    /// the player has not clicked it on this screen, board and advice.
    /// Gives the reason when there is no box (see [`AscendantApp::mark_hidden`]).
    fn mark_spot(&mut self, (x, y, half_w, half_h): (f32, f32, f32, f32)) -> MarkSpot {
        let r = self.session.reading.as_ref().ok_or("unread")?;
        let shown = (r.screen, r.model.clone(), self.session.advice().ok_or("no_advice")?.action);
        // When the reading on show was read, on the clock of `move_seen`.
        let read_at = Instant::now().checked_sub(SystemTime::now().duration_since(r.at).unwrap_or_default()).unwrap_or_else(Instant::now);
        let title = &self.settings.window_title;
        if self.clicked.as_ref() == Some(&shown) {
            // A click the game did not play (a way to cancel it that `mark::Clicks` does not know) logs no move: the
            // mark comes back.
            // Only while the game's log is followed (its light green): with auto read off there is no log follower, and
            // with the light red no line comes, so every click would bring its mark back as 1.0.3 never did.
            let log_live = matches!(self.log_status(), Some(gamelog::LogStatus::Following { .. }));
            if !self.clicked_at.is_some_and(|at| click_unconfirmed(shown.0, at, self.move_seen, Instant::now(), log_live)) {
                return Err("clicked");
            }
            (self.clicked, self.clicked_at) = (None, None);
        }
        if !game_in_front(title) {
            return Err("behind");
        }
        let rect @ [left, top, _, height] = game_rect(title).map_err(|_| "rect")?;
        let read = [left as f32 + x, top as f32 + y, half_w, half_h, height as f32 / 1080.0];
        // Clicks while no mark (nor value) showed are not clicks on it.
        if !self.mark.as_ref().is_some_and(Mark::shown) && !self.value.as_ref().is_some_and(Notice::shown) {
            self.clicks.forget();
        }
        // Only a click on the target or its box hides the mark (see `on_mark`). A click anywhere in the game did: on
        // 2026-11-07 the box went 5 times in a run on a click that played nothing, the game logging no move for 1.4 to
        // 74 s after (a Hunt's target pick twice, the manage screen twice, a patrol once). A click on the card but off
        // the box still hides it once the game logs the move (`played`, 0.02 to 0.12 s after the click in 156 hides of
        // three runs). As a button takes it, the press and the release both count (2026-11-08: pressed on the box and
        // let go elsewhere, the game played nothing and the box went).
        // A click on an instrument or treasure card only selects it; the mark stays until "Confirm" takes the cards away
        // (the screen changes or the target is gone). The click is still read, so it is not counted later.
        let drawn = self.mark.as_ref().and_then(Mark::spot);
        if self.clicks.next(mark::look()).is_some_and(|click| on_mark(click, rect, read, drawn)) && click_hides(shown.0, &shown.2) {
            (self.clicked, self.clicked_at) = (Some(shown), Some((Instant::now(), read_at)));
            return Err("clicked");
        }
        Ok((read[0], read[1], half_w, half_h, read[4], rect))
    }

    /// Reads the watcher's newest frame, else a fresh capture.
    fn read_now(&mut self) {
        match self.watcher.as_ref().and_then(Watcher::read_latest).map_or_else(|| capture(&self.settings.window_title), Ok) {
            Ok(frame) => {
                // In Vorax mode the hotkey read is handed to Vorax.
                if self.vorax.active() {
                    self.vorax.read_hotkey(frame);
                } else {
                    self.session.read_live(frame);
                }
                self.path.clear();
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
    }

    fn read_png(&mut self, path: &Path) {
        // In Vorax mode the Tartarus Session is paused: screenshots are not handed to it and the capture does
        // not take the next one.
        if self.vorax.active() {
            self.error = Some("渴血手術中不能開啟截圖".to_string());
            return;
        }
        self.path = path.display().to_string();
        match load_png(path) {
            Ok(frame) => {
                self.session.read_in_background(frame);
                if let Some(w) = &self.watcher {
                    w.adopt_next();
                }
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// Turns click-through on or off, kept for the next start.
    fn set_click_through(&mut self, on: bool) {
        self.click_through = on;
        self.settings.click_through = on;
        self.save();
    }

    fn set_auto_read(&mut self, on: bool) {
        self.settings.auto_read = on;
        self.watch();
        self.save();
    }

    /// Starts auto read's watcher when auto read is on and none watches the
    /// set window title and game log; stops it when auto read is off.
    fn watch(&mut self) {
        if !self.settings.auto_read || self.watched != self.settings.window_title || self.watched_log != self.settings.game_log {
            // After the capture is stopped or rebuilt, the Vorax log hints no longer arrive: leave Vorax mode
            // first.
            if self.vorax.active() {
                self.vorax.force_leave(&mut self.session, "watcher stopped");
            }
            // The old watcher stops before a new one opens a session.
            (self.watcher, self.auto_status, self.watching) = (None, None, None);
        }
        if self.settings.auto_read && self.watcher.is_none() {
            self.watched = self.settings.window_title.clone();
            self.watched_log = self.settings.game_log.clone();
            let ctx = self.ctx.clone();
            self.watcher = Some(Watcher::start(&self.watched, self.watched_log.clone(), move || ctx.request_repaint()));
        }
        if let Some(w) = &self.watcher {
            w.observe(self.settings.observe && self.settings.run_log);
        }
    }

    /// "Open screenshot...": the Windows file dialog, owned by this window; a PNG picked is read.
    fn open(&mut self, frame: &eframe::Frame) {
        if let Some(path) = rfd::FileDialog::new().add_filter(tr("Screenshot"), &["png"]).set_parent(frame).pick_file() {
            self.read_png(&path);
        }
    }

    /// The game log's status: the watcher's, None when auto read is off (no watcher); a preview's when set.
    fn log_status(&self) -> Option<gamelog::LogStatus> {
        match &self.preview_log {
            Some(status) => status.clone(),
            None => self.watcher.as_ref().map(Watcher::log_status),
        }
    }

    /// The Windows file dialog for the game log (UE_game.log, *.log), owned by this window; a file picked is
    /// saved and the watcher rebuilt on it.
    fn pick_game_log(&mut self, frame: &eframe::Frame) {
        let mut dialog = rfd::FileDialog::new().set_title(tr("Choose the game log (UE_game.log)")).add_filter("UE_game.log", &["log"]).set_parent(frame);
        if let Some(dir) = self.settings.game_log.as_deref().and_then(Path::parent).filter(|d| d.is_dir()) {
            dialog = dialog.set_directory(dir);
        }
        if let Some(path) = dialog.pick_file() {
            self.set_game_log(Some(path));
        }
    }

    /// Sets the game log (None: find it by itself), rebuilding the watcher on it.
    fn set_game_log(&mut self, path: Option<PathBuf>) {
        if self.settings.game_log != path {
            self.settings.game_log = path;
            self.watch();
            self.save();
        }
    }

    fn save(&mut self) {
        // Every settings change is saved: the Vorax settings (log, rollouts and so on) are updated along with
        // it.
        self.vorax.configure(Config::from_settings(&self.settings));
        super::runlog::set_settings(self.settings.log_summary());
        if let Err(e) = self.settings.save() {
            self.error = Some(trf!("Settings not saved: {}", e));
        }
    }

    /// "Change" of the `which` row: that row waits for a key (see [`AscendantApp::record_key`]). Both hotkeys are
    /// released meanwhile: one still registered takes its key before this window sees it (F8 would turn auto read on
    /// instead of being recorded).
    fn start_recording(&mut self, which: Which) {
        for key in &mut self.keys {
            key.hotkey = None;
        }
        self.recording = Some(Recording { which, refused: None });
    }

    /// Recording ends (a key saved, Esc, a click elsewhere, the window left): the hotkeys not registered are
    /// registered again from the saved keys.
    fn stop_recording(&mut self) {
        if self.recording.take().is_none() {
            return;
        }
        for which in Which::ALL {
            if self.keys[which as usize].hotkey.is_none() {
                let text = which.setting(&mut self.settings).clone();
                match self.register(which, &text) {
                    Ok(h) => self.keys[which as usize].hotkey = Some(h),
                    Err(e) => self.error = Some(e),
                }
            }
        }
    }

    /// While a hotkey is recorded, at the frame's start before any widget is drawn: every key pressed is taken out of
    /// the frame (it reaches no button or window), and the first becomes the hotkey ([`hotkey::record`], [`judge`]).
    /// Esc cancels; a refused key leaves its reason in the row and recording goes on; a key Windows registers is
    /// saved and recording ends, and one Windows refuses (another program has it) is refused the same way. The window
    /// losing the keyboard cancels too (not in the recording preview, whose window need not be in front).
    fn record_key(&mut self, ctx: &egui::Context) {
        let Some(which) = self.recording.as_ref().map(|r| r.which) else { return };
        if !self.preview_recording && !ctx.input(|i| i.focused) {
            self.stop_recording();
            return;
        }
        let got = ctx.input_mut(|i| {
            let got = super::hotkey::record(&i.events);
            i.events.retain(|e| !matches!(e, egui::Event::Key { .. } | egui::Event::Text(_) | egui::Event::Copy | egui::Event::Cut | egui::Event::Paste(_)));
            got
        });
        let Some(got) = got else { return };
        let other = if which == Which::Read { Which::Auto } else { Which::Read };
        let other_text = other.setting(&mut self.settings).clone();
        match judge(got, (&other_text, other)) {
            Judged::Cancel => self.stop_recording(),
            Judged::Refuse(why) => self.refuse_key(why),
            Judged::Try(text) => match self.register(which, &text) {
                Ok(h) => {
                    let i = which as usize;
                    self.keys[i].hotkey = Some(h);
                    self.keys[i].text.clone_from(&text);
                    *which.setting(&mut self.settings) = text;
                    self.error = None;
                    self.save();
                    self.stop_recording();
                }
                Err(e) => self.refuse_key(Refusal { short: trf!("{} is taken by another program", text), full: e }),
            },
        }
    }

    /// The key just pressed is refused: `why` shows in the recording row until the next key.
    fn refuse_key(&mut self, why: Refusal) {
        if let Some(r) = &mut self.recording {
            r.refused = Some(why);
        }
    }

    /// Registers the `which` hotkey its row shows (`Key::text`, set by "Restore defaults") in place of the current
    /// one; on failure the previous key stays.
    fn apply_hotkey(&mut self, which: Which) {
        let i = which as usize;
        let text = self.keys[i].text.trim().to_string();
        let saved = which.setting(&mut self.settings).clone();
        if text == saved && self.keys[i].hotkey.is_some() {
            return;
        }
        self.keys[i].hotkey = None;
        match self.register(which, &text) {
            Ok(h) => {
                self.keys[i].hotkey = Some(h);
                *which.setting(&mut self.settings) = text;
                self.error = None;
                self.save();
            }
            Err(e) => {
                self.error = Some(e);
                self.keys[i].hotkey = self.register(which, &saved).ok();
                self.keys[i].text = saved;
            }
        }
    }

    /// Follows the window size: a change the app did not ask for is the
    /// player's drag, and its height becomes the chosen one. Returns whether
    /// the window is in a drag or waits on a size the app asked for, when no
    /// fit should resize it.
    ///
    /// While the player drags an edge, wider tiles make the board taller from
    /// frame to frame; a fit then fights the drag and the border flickers, so
    /// the size must hold still for [`RESIZE_SETTLE`] first. And a drag can
    /// leave wgpu's DX12 surface drawn from the window's outer corner, its top
    /// under the title bar and a black band below, until the app itself next
    /// resizes the window; so after a drag the window is nudged a little
    /// taller and back.
    fn track_size(&mut self, ctx: &egui::Context) -> bool {
        let (size, minimized, maximized) = ctx.input(|i| (i.viewport().inner_rect.map(|r| r.size()), i.viewport().minimized, i.viewport().maximized));
        let Some(size) = size.filter(|_| minimized != Some(true)) else { return true };
        // The font size was just changed (including the one applied at startup): the window size in points
        // changes with it, but the player did not drag it.
        if self.rescaling(ctx) {
            self.last_size = None;
            return true;
        }
        let now = Instant::now();
        // A size asked for that never came (the system refused it) is not waited on.
        self.asked = self.asked.filter(|&(_, at)| now.duration_since(at) < Duration::from_secs(1));
        let changed = |a: Vec2, b: Vec2| (a.x - b.x).abs() > 0.5 || (a.y - b.y).abs() > 0.5;
        match self.last_size {
            Some(last) if changed(last, size) => {
                if self.asked.is_some_and(|(want, _)| !changed(want, size)) {
                    self.asked = None;
                } else {
                    self.resizing_until = Some(now + RESIZE_SETTLE);
                    self.dragged = true;
                }
            }
            _ => {}
        }
        self.last_size = Some(size);
        if let Some(until) = self.resizing_until.filter(|&until| now < until) {
            ctx.request_repaint_after(until - now);
            return true;
        }
        if let Some((_, at)) = self.asked {
            ctx.request_repaint_after((at + Duration::from_secs(1)).saturating_duration_since(now));
            return true;
        }
        if let Some(height) = self.nudge_back.take() {
            self.resize(ctx, Vec2::new(size.x, height));
            return true;
        }
        // A maximized window is left as it is.
        if std::mem::take(&mut self.dragged) && maximized != Some(true) {
            // The size the player left it at opens the window next time.
            // Save the size without the font size scale: when the window is opened (mod.rs) the scale is not
            // applied yet, so the next launch gets the same physical size.
            let zoom = ctx.zoom_factor();
            self.settings.window_size = Some([size.x * zoom, size.y * zoom]);
            self.save();
            self.nudge_back = Some(size.y);
            self.resize(ctx, Vec2::new(size.x, size.y + 2.0));
            return true;
        }
        false
    }

    /// Sets the window's inner size as the app's own change; see [`AscendantApp::track_size`].
    fn resize(&mut self, ctx: &egui::Context, size: Vec2) {
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
        self.asked = Some((size, Instant::now()));
    }

    /// Apply the font size (overall scale, see [`text_scale`]): text, cells, progress bars and spacing grow
    /// together while the physical window size stays the same (content that does not fit scrolls). The scale
    /// takes effect on the next frame; for [`RESCALE_SETTLE`] after that, changes in points are not treated as
    /// the player dragging or moving the window.
    fn set_text_scale(&mut self, ctx: &egui::Context, setting: f32) {
        let zoom = text_scale(setting);
        if (ctx.zoom_factor() - zoom).abs() > f32::EPSILON {
            ctx.set_zoom_factor(zoom);
            self.rescaled = Some(Instant::now());
        }
    }

    /// The font size was just changed and the window info may still be converted with the old scale: wait for
    /// it to settle (a repaint is scheduled meanwhile).
    fn rescaling(&mut self, ctx: &egui::Context) -> bool {
        let Some(at) = self.rescaled else { return false };
        let left = RESCALE_SETTLE.saturating_sub(at.elapsed());
        if left.is_zero() {
            self.rescaled = None;
            return false;
        }
        ctx.request_repaint_after(left);
        true
    }

    /// Moves the window where the player last left it while that is on a
    /// monitor, or else to the top left of the empty area under the game's
    /// instrument panel the first time the game window is found.
    fn place(&mut self, ctx: &egui::Context) {
        if self.placed {
            return;
        }
        // A point in the title bar: the outer top left of a window against a
        // screen edge sits off screen, in the invisible resize border.
        let at = match self.settings.window_pos.filter(|&[x, y]| on_screen(x + 20, y + 20)) {
            Some(at) => at,
            None if !self.settings.auto_place => return,
            None => {
                let Ok([left, top, width, height]) = game_rect(&self.settings.window_title) else {
                    ctx.request_repaint_after(Duration::from_secs(1));
                    return;
                };
                let (x, y) = PLACE;
                [left + (x * width as f32 / 1920.0) as i32, top + (y * height as f32 / 1080.0) as i32]
            }
        };
        // Window coordinates (points) = physical pixels ÷ (system scale × font size scale), see
        // [`physical_per_point`].
        if self.rescaling(ctx) {
            return;
        }
        let ppp = physical_per_point(ctx);
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(Pos2::new(at[0] as f32, at[1] as f32) / ppp));
        (self.placed, self.placed_at) = (true, Some(at));
    }

    /// Saves where the player moves the window: any move but the window
    /// arriving where `place` put it. A window moved before the game is
    /// found is not placed after.
    fn remember_position(&mut self, ctx: &egui::Context) {
        // The font size was just changed: for a frame or two the position is still converted with the old
        // scale, so it does not count as the player moving the window.
        if self.rescaling(ctx) {
            self.last_pos = None;
            return;
        }
        let (rect, minimized) = ctx.input(|i| (i.viewport().outer_rect, i.viewport().minimized));
        let ppp = physical_per_point(ctx);
        let Some(rect) = rect.filter(|_| minimized != Some(true)) else { return };
        let at = [(rect.min.x * ppp).round() as i32, (rect.min.y * ppp).round() as i32];
        let near = |a: [i32; 2], b: [i32; 2]| (a[0] - b[0]).abs() <= 2 && (a[1] - b[1]).abs() <= 2;
        let moved = self.last_pos.is_some_and(|l| !near(l, at)) && self.placed_at.is_none_or(|p| !near(p, at));
        self.last_pos = Some(at);
        if moved {
            self.placed = true;
            self.settings.window_pos = Some(at);
            self.save();
        }
    }

    /// Reads the game and writes the window in `language` from now on, and
    /// solves again so the moves are named in it.
    fn set_language(&mut self, language: Lang) {
        lang::set(language);
        self.settings.language = language.key().to_string();
        self.ocr_problem = if self.preview_no_ocr { ocr::problem_with(None) } else { ocr::problem() };
        // OCR is made again for the new language: the run log's window line says so from here on.
        self.session.log_language();
        self.vorax.log_language();
        // The runs the Run tab kept were settled in the language before: settled again when picked.
        self.review.language_changed();
        self.session.solve();
        self.save();
    }

    fn set_tools(&mut self, tools: Vec<Tool>) {
        self.settings.tools = tools.iter().map(|t| t.key().to_string()).collect();
        self.settings.tools_days = self.session.days_left();
        self.session.set_tools(tools);
        self.save();
    }

    fn set_theme(&mut self, theme: Option<u32>) {
        self.settings.theme = theme;
        self.settings.tools_days = self.session.days_left();
        self.session.set_theme(theme);
        self.save();
    }

    /// Takes the update check's answer and the update's outcome when they
    /// come; a finished update closes the window for the new exe.
    fn poll_update(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.checking {
            let answer = match rx.try_recv() {
                Ok(answer) => Some(answer),
                Err(TryRecvError::Disconnected) => Some(update::Checked { result: Err(update::Problem::Network(tr("The update check stopped unexpectedly.").to_string())), seen: self.seen.clone(), wait: None }),
                Err(TryRecvError::Empty) => None,
            };
            if let Some(answer) = answer {
                self.checking = None;
                // A version a download refused is not offered again, nor is its ETag kept.
                let answer = update::drop_refused(answer, self.refused);
                self.seen = answer.seen.clone();
                // Whatever the answer, the next check is an hour away (or later, when GitHub asked): never sooner.
                self.next_check = Some(update::next_check(Instant::now(), answer.wait));
                if !self.check_quiet {
                    // "Check now" or the check at start: newer than any hourly answer still held.
                    self.held = None;
                    self.take_answer(answer.result, false);
                } else if self.tab == Tab::Settings {
                    self.held = Some(answer);
                } else {
                    self.take_answer(answer.result, true);
                }
            }
        }
        // An hourly answer held while the Settings tab was on show goes in once the player has left it.
        if self.tab != Tab::Settings
            && let Some(held) = self.held.take()
        {
            self.take_answer(held.result, true);
        }
        // The hourly check: not while a check or an update runs, nor after an update that moved the exe. While busy
        // no wake-up is asked for: the busy thread repaints when it ends (see `in_background`).
        let now = Instant::now();
        let on = update::ENABLED && self.settings.check_updates;
        let busy = self.checking.is_some() || self.updating.is_some() || self.update_locked;
        if update::check_due(on, now, self.next_check, busy) {
            self.start_check(true);
        } else if let Some(wait) = update::wake_in(on, now, self.next_check, busy) {
            ctx.request_repaint_after(wait);
        }
        if let Some(rx) = &self.verifying {
            match rx.try_recv() {
                Ok(result) => (self.verified, self.verifying) = (Some(result), None),
                Err(TryRecvError::Disconnected) => self.verifying = None,
                Err(TryRecvError::Empty) => {}
            }
        }
        if let Some((rx, _)) = &self.updating {
            let done = match rx.try_recv() {
                Ok(done) => done,
                Err(TryRecvError::Disconnected) => Err(update::Failed::Kept(tr("The update stopped unexpectedly.").to_string())),
                Err(TryRecvError::Empty) => return,
            };
            self.updating = None;
            match done {
                Ok(()) => self.close(ctx),
                Err(e) => {
                    // A release that fails verification on download is no longer offered, nor brought back by a
                    // 304 to the next check.
                    // Its version is remembered until Ascendant restarts (see `drop_refused`). No check runs during an
                    // update and "Update" is off during a check, so `release` is the one that was downloaded.
                    if matches!(e, update::Failed::Problem(update::Problem::Unverified(_))) {
                        self.refused = self.release.as_ref().map(|r| r.version);
                        (self.release, self.seen, self.held) = (None, None, None);
                    }
                    // The exe on disk is no longer this one: no further update until Ascendant starts again.
                    if locks_update(&e) {
                        self.update_locked = true;
                    }
                    self.error = Some(e.text());
                    self.update_error = Some(e);
                }
            }
        }
    }

    /// "Verify downloaded files...": picks a folder (Windows' own folder dialog, owned by this window) and checks
    /// the Ascendant.exe in it against the SHA256SUMS.txt and signature beside it with this build's release key,
    /// on a thread ([`update::verify_download_report`], with its warnings about this version).
    fn start_verify(&mut self, frame: &eframe::Frame) {
        if self.verifying.is_some() {
            return;
        }
        let dialog = rfd::FileDialog::new().set_title(tr("Folder with Ascendant.exe, SHA256SUMS.txt and SHA256SUMS.txt.sig")).set_parent(frame);
        let Some(dir) = dialog.pick_folder() else { return };
        self.verified = None;
        self.verifying = Some(in_background(&self.ctx, move || update::verify_download_report(&dir, &update::RELEASE_PUBLIC_KEY, env!("CARGO_PKG_VERSION"))));
    }

    /// Asks GitHub for the latest release on a thread (at start and every hour when "Check for updates at start" is
    /// on, and on "Check now"), with the last answer's ETag. Only reads the release list; nothing is downloaded.
    /// `quiet` is the hourly check: a release it finds shows only as the red dot (see [`AscendantApp::update_dot`]).
    fn start_check(&mut self, quiet: bool) {
        // Previews inject the update state: "Check now" must not reach GitHub there either.
        if update::ENABLED && self.checking.is_none() && self.updating.is_none() && !preview_update() {
            // The hourly check leaves the line of the last answer as it is until its own answer is taken.
            if !quiet {
                self.check_error = None;
            }
            self.check_quiet = quiet;
            let seen = self.seen.clone();
            self.checking = Some(in_background(&self.ctx, move || update::check(seen)));
        }
    }

    /// Takes a check's outcome. The hourly check's (`quiet`) finds a release quietly (the red dot), keeps a release
    /// the status bar already offers when it answers "none" (see [`quiet_release`]), and fills the error line only
    /// when it has no answer from before to keep showing.
    fn take_answer(&mut self, result: Result<Option<Release>, update::Problem>, quiet: bool) {
        match result {
            Ok(found) => {
                (self.release, self.release_quiet) = if quiet { quiet_release(self.release.take(), self.release_quiet, found) } else { (found, false) };
                (self.check_error, self.checked) = (None, true);
            }
            // An hourly failure never replaces the line already there (an earlier answer or error still stands); it is
            // shown only when there is no line yet.
            Err(e) if !quiet || (!self.checked && self.check_error.is_none()) => self.check_error = Some(e),
            Err(_) => {}
        }
    }

    /// The release the red dot on the Settings button stands for: one the hourly check found (the status bar does
    /// not show it), taken already or held while the Settings tab is on show, while it can be installed. The dot stays
    /// until the release is installed or no longer offered; it is off while the update runs (the Settings tab shows
    /// its step) and after a failure that moved the exe (`update_locked`, whose error is in the status bar and says
    /// what to do).
    fn dot_release(&self) -> Option<&Release> {
        if self.updating.is_some() || self.update_locked {
            return None;
        }
        let loud = self.release.is_some() && !self.release_quiet;
        let held = self.held.as_ref().and_then(|h| h.result.as_ref().ok()).and_then(Option::as_ref).filter(|_| !loud);
        held.or(self.release.as_ref().filter(|_| self.release_quiet))
    }

    fn update_dot(&self) -> bool {
        self.dot_release().is_some()
    }

    /// The "Updated to" notice was dismissed by its "x", or a game ended since it appeared (a Tartarus run's
    /// `run_end`, or a Vorax game's end): it does not come back, after a restart either.
    fn seen_updated(&mut self) {
        // A preview's notice goes away without touching the settings.
        if self.preview_updated.is_some() {
            self.preview_updated = Some(false);
            return;
        }
        if dismiss_updated(&mut self.settings, self.this_notes.as_ref()) {
            self.save();
        }
    }

    /// On the player's "Update" only: downloads the release and verifies its signature and hashes
    /// ([`update::download`]), and only then swaps it in beside this exe and starts it, all on a thread. A download
    /// or verification failure keeps the current version; a failed swap or restart can leave the exe moved or
    /// replaced, and its [`update::Failed`] text says what to do.
    fn start_update(&mut self) {
        let (Some(release), Some(current)) = (self.release.clone(), self.current.clone()) else { return };
        // Not during a check either: its answer could bring back a release this update is about to refuse.
        if self.updating.is_some() || self.checking.is_some() || self.update_locked {
            return;
        }
        (self.error, self.update_error) = (None, None);
        let step = Arc::new(AtomicU8::new(update::Step::Signature as u8));
        let at = step.clone();
        let rx = in_background(&self.ctx, move || {
            let dir = current.parent().ok_or_else(|| update::Failed::Kept(format!("{}: no folder", current.display())))?;
            let new = update::download(&release, dir, &at)?;
            // The verified notes, for the new version to show; a failure to keep them does not stop the update.
            update::save_notes(&release);
            update::Step::Restart.set(&at);
            update::swap_checked(&current, &new)?;
            // The new exe is in place from here on: a failed start must not claim the old one was kept.
            update::restart(&current).map_err(update::Failed::NotRestarted)
        });
        self.updating = Some((rx, step));
    }

    /// The step of the update running now, if any.
    fn update_step(&self) -> Option<update::Step> {
        self.updating.as_ref().map(|(_, step)| update::Step::load(step))
    }

    /// Installs this exe and starts the installed copy in its place.
    fn install(&mut self, ctx: &egui::Context) {
        let Some(current) = &self.current else { return };
        match install::install(current).and_then(|exe| update::restart(&exe)) {
            Ok(()) => self.close(ctx),
            Err(e) => self.error = Some(trf!("Install failed: {}", e)),
        }
    }

    /// Saves the settings and closes the window, for the exe just started.
    fn close(&mut self, ctx: &egui::Context) {
        self.save();
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// Whether the game runs with more rights than Ascendant (see [`elevation::GameRights`]), or a preview says so.
    fn game_higher(&self) -> bool {
        self.preview_elevated.unwrap_or_else(|| self.rights.higher())
    }

    /// "Restart as administrator": Windows asks the player on a background thread (the window keeps drawing).
    fn start_relaunch(&mut self, ctx: &egui::Context) {
        if self.relaunching.is_some() {
            return;
        }
        self.relaunch_declined = false;
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(elevation::relaunch());
            ctx.request_repaint();
        });
        self.relaunching = Some(rx);
    }

    /// Takes how "Restart as administrator" went: the copy as administrator started, so this one saves and closes
    /// (the copy waits for that); declined at Windows' prompt, so this one keeps running and says so; or an error.
    fn poll_relaunch(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.relaunching else { return };
        let out = match rx.try_recv() {
            Ok(out) => out,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => elevation::Relaunch::Failed(tr("the restart stopped unexpectedly").to_string()),
        };
        self.relaunching = None;
        match out {
            elevation::Relaunch::Started => self.close(ctx),
            elevation::Relaunch::Cancelled => self.relaunch_declined = true,
            elevation::Relaunch::Failed(e) => self.error = Some(trf!("Could not restart as administrator: {}", e)),
        }
    }

    /// Turns run log sharing on or off. Turning it on sends the runs from now
    /// on, and makes the install id the first time.
    fn set_sharing(&mut self, on: bool) {
        if on && self.settings.install_id.is_empty() {
            match share::new_install_id() {
                Ok(id) => self.settings.install_id = id,
                Err(e) => {
                    self.upload_error = Some(e);
                    return;
                }
            }
        }
        self.settings.share_logs = Some(on);
        if on {
            self.settings.shared_from = folder_name(std::time::SystemTime::now());
        }
        self.save();
        self.sync_logs();
    }

    /// Asks the uploader to send the finished runs, while sharing and the run log are on.
    fn sync_logs(&self) {
        let (Some(uploader), Some(log), Some(true)) = (&self.uploader, &self.session.log, self.settings.share_logs) else { return };
        uploader.sync(log.dir().to_path_buf(), self.settings.shared_from.clone(), self.settings.install_id.clone(), log.flushed());
    }

    /// Syncs the run logs when the run log's folder changes, and takes the
    /// uploads' outcome.
    ///
    /// A changed folder (the same log moving from the old folder to a new one) means the old run has ended:
    /// settle it in the background (read only after the log is fully written). It does not count when the run
    /// log was just turned on or off (one side is None).
    fn poll_upload(&mut self) {
        if self.session.run_over.is_none() {
            self.over_reviewed = false;
        }
        let over = self.session.run_over.is_some();
        if over && !self.was_over {
            self.run_over_at = Some(super::vorax_session::now_ms());
            // The run ended (the session wrote its `run_end`): a game was played since "Updated to" appeared, and
            // the Run tab follows the latest run again.
            self.seen_updated();
            self.review.game_ended();
        }
        self.was_over = over;
        if self.session.log.as_ref().map(|l| l.dir()) != self.log_dir.as_deref() {
            let ended = self.log_dir.take();
            self.log_dir = self.session.log.as_ref().map(|l| l.dir().to_path_buf());
            if let (Some(ended), Some(log)) = (ended, &self.session.log) {
                let written = log.flushed();
                self.start_settle(ended, Some(written));
                self.over_reviewed = self.session.run_over.is_some();
            }
            self.sync_logs();
        }
        if let Some(result) = self.uploader.as_ref().and_then(Uploader::poll) {
            self.upload_error = result.err();
        }
    }

    /// Settle the run in folder `dir` in the background; `written` is the notification that the log is fully
    /// written (see [`RunLog::flushed`]).
    /// When the previous run is still being computed, switch to computing this one (the previous result is
    /// discarded).
    fn start_settle(&mut self, dir: PathBuf, written: Option<Receiver<()>>) {
        let rx = spawn_settle(&self.ctx, dir.clone(), Game::Tartarus, written);
        let at = self.run_over_at.take().unwrap_or_else(super::vorax_session::now_ms);
        self.review.start_tartarus(dir, rx, at);
    }

    /// Settle the Vorax game in folder `dir` in the background, once `written` says its log is written (see
    /// [`VoraxView::log_flushed`]); `at` is when it ended (None: the same game again, the time kept).
    fn start_vorax_settle(&mut self, dir: PathBuf, written: Option<Receiver<()>>, at: Option<i64>) {
        let rx = spawn_settle(&self.ctx, dir.clone(), Game::Vorax, written);
        self.review.start_vorax(dir, rx, at);
    }

    /// A map page event ([`Event::MapLog`], [`Event::MapShot`]): the affix frames take it, a level entered teaches its
    /// affixes, and private builds hand it on to the affix samples ([`AscendantApp::sample_map_event`]).
    fn map_event(&mut self, event: Event) {
        // the live frames first, whatever observe mode says
        match &event {
            Event::MapLog(signal, ..) => self.affix_live.note(*signal),
            Event::MapShot { frame, after, at, .. } => self.affix_live.frame(frame, after, *at),
            _ => {}
        }
        // a level entered: the affixes its block named are learned (new ones, or another language's text)
        if matches!(event, Event::MapLog(gamelog::Signal::MapAffixes(_), ..)) {
            self.learn_affixes();
        }
        self.sample_map_event(event);
    }

    /// Private builds only: a map page event into the affix samples while observe mode and the run logs are on.
    #[cfg(feature = "dev-samples")]
    fn sample_map_event(&mut self, event: Event) {
        if !(self.settings.observe && self.settings.run_log) {
            self.affix_samples = None;
            return;
        }
        if self.affix_samples.is_none() {
            self.affix_samples = super::affix_samples::AffixSamples::new();
        }
        let Some(samples) = &mut self.affix_samples else { return };
        match event {
            Event::MapLog(signal, stamp, read) => {
                // the display scaling the visit is taken at, for its first row
                if matches!(signal, gamelog::Signal::MapPage { state: gamelog::PageState::Shown, .. }) {
                    samples.set_dpi(super::capture::game_dpi(&self.settings.window_title));
                }
                samples.note(signal, stamp, read)
            }
            // the once-a-second frames (twice a second with frames up), and the frames the live frames ask for to read a
            // panel again, are for the live frames only: as samples they would fill a visit's 40 with tooltips coming
            // and going and crowd out the frames after the clicks
            Event::MapShot { frame, at, after, delay_ms, taken_ms } if after != "refresh" && after != "watch" && after != "retry" => {
                samples.shot(frame, at, after, delay_ms, taken_ms);
            }
            // the frames the live frames asked for: their text area only, for replays at the real times
            Event::MapShot { frame, at, after: "retry", .. } => samples.quick(&frame, at, "retry"),
            _ => {}
        }
    }

    #[cfg(not(feature = "dev-samples"))]
    fn sample_map_event(&mut self, _event: Event) {}

    /// Whether the affix samples are being taken: private builds with observe mode and the run logs on.
    fn sampling_affixes(&self) -> bool {
        cfg!(feature = "dev-samples") && self.settings.observe && self.settings.run_log
    }

    /// The affix frames follow the reads done and the game's place, and go when it is not in front (as the mark,
    /// ui.rs `mark_spot`). Looks at the game's window only while a map page shows or frames are up.
    fn poll_affix_live(&mut self) {
        // affix texts the look back found (a lock on an empty queue when there are none)
        self.learn_affixes();
        // the map page is captured only for those who want it: the affix frames when their switch is on, private
        // builds' samples when they are taken (observe mode and run logs on); neither, no map page frame (the user,
        // 2026-11-09)
        if let Some(w) = &self.watcher {
            w.map_frames(self.settings.affix_frames, self.sampling_affixes());
            // frames up: the page is looked at twice a second, so a scroll or a popup takes them down sooner
            w.map_watch(self.affix_live.frames_up());
        }
        // what the live frames did, into the visit's samples log while the sampler runs (private builds): a live test
        // then shows where the time went. Taken before the poll (the events' clicks, and nothing more when no page
        // shows) and after it (the reads it took), so no note waits a tick, when a closed page would drop it or a page
        // shown next would get it.
        self.forward_affix_notes();
        if !self.affix_live.active() {
            return;
        }
        let title = &self.settings.window_title;
        let game = if game_in_front(title) { game_rect(title).ok() } else { None };
        self.affix_live.poll(game);
        self.forward_affix_notes();
        // a frame asked of the capture: a quick frame while the panel comes in or settles after a click, or a panel to
        // read again though it did not change
        if let Some(after) = self.affix_live.wants_shot_in()
            && let Some(w) = &self.watcher
        {
            w.map_shot_in(after);
        }
        // come back soon: the game may move or lose the front with nothing else waking the window
        self.ctx.request_repaint_after(std::time::Duration::from_millis(200));
    }

    /// The live frames' notes, taken on every tick so none pile up: into the visit's samples log in private builds,
    /// dropped otherwise.
    fn forward_affix_notes(&mut self) {
        let notes = self.affix_live.take_notes();
        #[cfg(feature = "dev-samples")]
        if let Some(samples) = &mut self.affix_samples {
            for note in &notes {
                samples.live(note);
            }
        }
        drop(notes);
    }

    /// The affix page; a change is saved and passed on to the live frames.
    fn affix_tab(&mut self, ui: &mut egui::Ui) {
        let s = &mut self.settings;
        let seen = self.affix_live.seen().to_vec();
        let hints = self.affix_live.hints();
        if super::affix_page::draw(ui, &mut self.affix_page, &mut s.affix_frames, &mut s.affix_frame_style, &mut s.affix_grades, &seen, hints) {
            self.save();
            self.configure_affixes();
        }
    }

    /// The affix settings and the learned affixes passed on to the live frames and the page.
    fn configure_affixes(&mut self) {
        // Once, on the first start since 1.2.0: the grades the user saw keep, though every affix not graded is framed
        // by the game's colour from now on (the table's own grades were the defaults before). Only a user who had
        // affix grades (a private build's) had those defaults to keep.
        if self.settings.affix_defaults < super::settings::AFFIX_DEFAULTS_GAME_COLOUR {
            if !self.settings.affix_grades.is_empty() {
                let table = crate::affix::Dict::builtin().with_learned(&self.affix_learned.list);
                crate::affix::dict::keep_grades_before(&table, &mut self.settings.affix_grades);
            }
            self.settings.affix_defaults = super::settings::AFFIX_DEFAULTS_GAME_COLOUR;
            self.save();
        }
        self.affix_live.configure(self.settings.affix_frames, self.settings.affix_frame_style, &self.settings.affix_grades, &self.affix_learned.list);
        self.affix_page.set_learned(&self.affix_learned.list);
    }

    /// The affixes the game's logs named before this start are learned too ([`gamelog::look_back_affixes`]: the log
    /// and the game's backups of earlier ones), read on a thread of its own below normal priority; their texts wait in
    /// the same queue as a level's, taken on every tick ([`AscendantApp::poll_affix_live`]). Not in previews: they show
    /// set affixes and must not read the game's logs.
    fn look_back_affixes(&self) {
        if std::env::vars_os().any(|(k, _)| k.to_string_lossy().starts_with("ASCENDANT_PREVIEW")) {
            return;
        }
        let manual = self.settings.game_log.clone();
        let ctx = self.ctx.clone();
        std::thread::spawn(move || {
            // Windows' THREAD_PRIORITY_BELOW_NORMAL, as the settlement's threads
            crate::solver::solve::set_priority(-1);
            if let Ok((log, _)) = gamelog::locate(manual.as_deref(), None) {
                gamelog::look_back_affixes(&log, &std::sync::atomic::AtomicBool::new(false));
                ctx.request_repaint();
            }
        });
    }

    /// The affixes the game log named on entering a level, or the look back found ([`gamelog::take_affix_texts`]),
    /// join the learned list; a change is saved (`affix-learned.tsv` in the app's folder) and passed on.
    fn learn_affixes(&mut self) {
        let mut changed = false;
        for (id, text) in gamelog::take_affix_texts() {
            changed |= crate::affix::learned::add(&mut self.affix_learned.list, id, &text);
        }
        if changed {
            // a file that did not read at start is left alone (see `affix::learned::Store`)
            let _ = self.affix_learned.save();
            self.configure_affixes();
        }
    }

    /// A Vorax game ended with a log folder (a new `ended_at` in [`VoraxView::last_game`]): settle it in the
    /// background, once its log is written. Kept apart from Tartarus's settlement.
    fn poll_vorax_settle(&mut self) {
        if let Some(g) = self.vorax.last_game()
            && self.vorax_ended != Some(g.ended_at)
        {
            let (ended, dir) = (g.ended_at, g.log_dir.clone());
            self.vorax_ended = Some(ended);
            // A Vorax game ended (its `vorax_end`, which makes `last_game`): a game was played since "Updated to"
            // appeared.
            self.seen_updated();
            if let Some(dir) = dir {
                let written = self.vorax.log_flushed();
                self.start_vorax_settle(dir, written, Some(ended));
            }
        }
    }

    /// Takes the settlements' results and starts the picked run's (see [`RunReview::poll`]).
    fn poll_review(&mut self) {
        let ctx = self.ctx.clone();
        self.review.poll(&mut |dir, game| spawn_settle(&ctx, dir, game, None));
    }

    /// Switches the page; every switch goes through here (the preview entry only sets the first page). The floating
    /// windows that sit over a page close as it is left: "What's new" (opened from the notice or Settings), the
    /// instruments and theme window and the cell correction window (both opened from the board), and the "Restore
    /// defaults" confirmation (Settings). They share the spot under the title bar, so one left open would cover the
    /// next page.
    fn set_tab(&mut self, tab: Tab) {
        if self.tab != tab {
            self.floats = Floats::default();
            self.editing = None;
            self.restoring = false;
            // A hotkey being recorded is on the settings page: leaving it cancels, and the hotkeys come back.
            self.stop_recording();
        }
        self.tab = tab;
    }

    /// Opens "What's new", closing the instruments window at the same spot (see [`Floats`]); with `pending`, the newer
    /// release on offer comes first with its verified notes, copied only now (see [`AscendantApp::pending_notes`]).
    fn open_whats_new(&mut self, pending: bool) {
        let notes = if pending { self.pending_notes() } else { None };
        self.floats.open_whats_new(WhatsNew::new(notes));
    }

    /// The verified notes of the newer release on offer, when it has some.
    fn pending_notes(&self) -> Option<update::SavedNotes> {
        self.release.as_ref().filter(|r| !r.notes.is_empty()).map(|r| update::SavedNotes { version: r.version_text(), notes: r.notes.clone() })
    }

    /// Open the Run tab and review the finished run `entry` instead of the latest (the run picker, and the
    /// ASCENDANT_PREVIEW_PICK preview); the export card picks the same run.
    fn review_run(&mut self, entry: RunEntry) {
        self.set_tab(Tab::Run);
        self.review.pick(entry);
        self.poll_review();
    }

    /// Notice (one at a time): asks whether to install when the program is not in the install location, asks
    /// once whether to share run logs, says a new version is available when already installed (only one the check
    /// at start or "Check now" found: the hourly check's shows as the red dot instead), or says which version an
    /// update just installed until it is dismissed.
    fn notice(&self) -> Option<NoticeKind<'_>> {
        if self.current.is_some() && !self.installed && self.settings.offer_install {
            Some(NoticeKind::Install)
        } else if self.settings.share_logs.is_none() && self.settings.run_log && self.uploader.is_some() {
            Some(NoticeKind::Share)
        } else if let Some(r) = self.release.as_ref().filter(|_| !self.update_locked && !self.release_quiet) {
            // After a failure that moved the exe (see `locks_update`) there is no update to offer until Ascendant starts
            // again: the notice would show an "Update" that does nothing, and the error says what to do instead.
            Some(NoticeKind::Update { version: r.version_text(), notes: &r.notes, history: !self.notes_history.is_empty(), updating: self.update_step(), checking: self.checking.is_some() })
        } else {
            self.this_notes
                .as_ref()
                .filter(|n| self.preview_updated.unwrap_or_else(|| update::show_updated(Some(n), env!("CARGO_PKG_VERSION"), &self.settings.updated_seen)))
                // The built-in notes of that version when there are some (see `release_notes`), for the tooltip too.
                .map(|n| {
                    let notes = self.notes_history.iter().find(|h| h.version == n.version).map_or(&n.notes, |h| &h.notes);
                    NoticeKind::Updated { version: n.version.clone(), notes }
                })
        }
    }

    /// All status bar states, in priority order: error, game log light red, the game run as administrator, notice,
    /// progress full, auto read waiting, run check. Click-through is not one: the rail's "Thru" light shows it.
    fn status_items(&self) -> Vec<StatusItem<'_>> {
        let mut items = Vec::new();
        if let Some(e) = &self.error {
            items.push(StatusItem::Error(e.clone()));
        }
        if let Some(alert) = log_alert(self.log_status().as_ref()) {
            items.push(StatusItem::Log(alert.to_string()));
        }
        if elevation::notify(self.game_higher(), self.rights.own_elevated(), self.elevated_dismissed, self.relaunch_declined) {
            let (auto, read) = (&self.settings.auto_hotkey, &self.settings.read_hotkey);
            let text = if self.relaunch_declined {
                trf!("Cancelled: Ascendant still runs without administrator rights, so {} and {} still do nothing while the game is in front", auto, read)
            } else {
                trf!("The game runs as administrator: {} and {} do nothing while the game is in front", auto, read)
            };
            items.push(StatusItem::Notice(NoticeKind::Elevated { text, waiting: self.relaunching.is_some() }));
        }
        if let Some(n) = self.notice() {
            items.push(StatusItem::Notice(n));
        }
        let charges = self.charges();
        for item in CHARGE_ITEMS.iter().filter(|i| (i.value)(&charges).is_some_and(|v| gamelog::fill(v) == Fill::Full)) {
            let v = (item.value)(&charges).unwrap_or_default();
            items.push(StatusItem::Charge(trf!("{} full {}/{}", tr(item.name), v, item.max)));
        }
        if let Some(s) = self.auto_status.as_ref().filter(|_| self.settings.auto_read).or(self.preview_auto_waiting.as_ref()) {
            items.push(StatusItem::Auto(trf!("Auto read waiting: {}", s)));
        }
        if let Some(h) = &self.session.health {
            items.push(StatusItem::Health { summary: h.status_line(), issues: h.issues.clone() });
        }
        items
    }

    /// The status bar, one line high and fixed at the very top (shared by Tartarus, Vorax and every tab): it
    /// shows only the highest-priority item, truncated when too long with the full text in the hover tooltip;
    /// when there are more items the line ends with `＋N`, and clicking it floats out the whole list below (the
    /// floating list covers the content and does not push the layout). Notice buttons (install, share,
    /// update...) are small buttons on this line; the run check details are in the tooltip and the floating
    /// list. With no state at all the line is left blank.
    ///
    /// Compact Tartarus (`head` is Some): on the same line the header `Tartarus · Day N · M days left` is on
    /// the left and the state, rewritten in its short form ([`StatusItem::short`]), is right-aligned; this line
    /// replaces both the status bar and the header.
    fn status_bar(&mut self, ui: &mut egui::Ui, merged: Option<(&str, &str)>) {
        let (head, idle) = (merged.map(|(h, _)| h), merged.map_or("", |(_, i)| i));
        let items = self.status_items();
        let h = ui.spacing().interact_size.y;
        let width = ui.available_width();
        let mut act = None;
        ui.allocate_ui_with_layout(Vec2::new(width, h), Layout::right_to_left(Align::Center), |ui| {
            ui.set_min_size(Vec2::new(width, h));
            ui.spacing_mut().item_spacing.x = 4.0;
            if items.len() > 1 {
                let all: Vec<String> = items.iter().map(StatusItem::text).collect();
                // Coloured by the most serious of the items it folds away (see `more_colour`).
                let colour = more_colour(&items[1..]);
                let more = ui.add(egui::Button::new(RichText::new(format!("\u{ff0b}{}", items.len() - 1)).small().color(colour)).small()).on_hover_text(all.join("\n"));
                if let Some(a) = status_popup(&more, &items, width, false) {
                    act = Some(a);
                }
            }
            if let Some(StatusItem::Notice(kind)) = items.first()
                && let Some(a) = notice_buttons(ui, kind, true, width < NARROW_STATUS)
            {
                act = Some(a);
            }
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                if let Some(head) = head {
                    // The header takes at most 60% of what remains of the line; the state is written
                    // right-aligned in the rest.
                    let font = egui::TextStyle::Small.resolve(ui.style());
                    let w = ui.painter().layout_no_wrap(head.to_owned(), font, MUTED).size().x + 8.0;
                    let w = w.min(ui.available_width() * 0.6);
                    ui.allocate_ui_with_layout(Vec2::new(w, h), Layout::left_to_right(Align::Center), |ui| theme::line(ui, head, MUTED));
                }
                let Some(first) = items.first() else {
                    // No state: when merged into line 1, write `idle` on the right (Vorax's remaining rerolls),
                    // otherwise leave it blank.
                    theme::line_aligned(ui, idle, MUTED, true);
                    return;
                };
                let text = if head.is_some() { first.short() } else { first.text() };
                // When the run check has details, the tooltip carries the full text plus the details (only this
                // one is attached, so it does not stack with the automatic tooltip shown on truncation).
                match first {
                    // The run check: its details in the tooltip as before, and a click opens them in a window, all clear
                    // too (2026-11-08, the user).
                    StatusItem::Health { issues, .. } => {
                        let details = if issues.is_empty() { String::new() } else { format!("\n{}", issues.join("\n")) };
                        let tip = format!("{}{details}\n{}", first.text(), tr("Click to see the run check"));
                        if theme::line_tip(ui, &text, &tip, first.colour(), head.is_some()).clicked() {
                            act = Some(NoticeAct::RunCheck);
                        }
                    }
                    // A new version, or the version just installed: the release notes in full in the tooltip.
                    StatusItem::Notice(NoticeKind::Update { notes, .. } | NoticeKind::Updated { notes, .. }) if !notes.is_empty() => {
                        theme::line_tip(ui, &text, &format!("{}\n\n{}", first.text(), notes_in(notes, lang::lang().key())), first.colour(), head.is_some());
                    }
                    // The game run as administrator: a click on the text floats out the whole list, as "+N" does,
                    // with all of the notice's buttons ("Always" too, which a narrow line leaves out) and its whole
                    // text. The list floats over the page, so nothing moves.
                    StatusItem::Notice(NoticeKind::Elevated { .. }) => {
                        let tip = format!("{}\n{}", first.text(), tr("Click for all of its buttons"));
                        let r = theme::line_tip(ui, &text, &tip, first.colour(), head.is_some());
                        if let Some(a) = status_popup(&r, &items, width, self.preview_status_open) {
                            act = Some(a);
                        }
                    }
                    // The game log light is red: the reminder, its note and a click that opens Settings.
                    StatusItem::Log(alert) => {
                        let tip = format!("{alert}\n{}\n{}", tr(LOG_NOTE), tr("Click to open Settings"));
                        if theme::line_tip(ui, &text, &tip, first.colour(), head.is_some()).clicked() {
                            act = Some(NoticeAct::Settings);
                        }
                    }
                    _ => {
                        theme::line_aligned(ui, &text, first.colour(), head.is_some());
                    }
                }
            });
        });
        match act {
            Some(NoticeAct::Install) => self.install(ui.ctx()),
            Some(NoticeAct::NotNow) => {
                self.settings.offer_install = false;
                self.save();
            }
            Some(NoticeAct::Share(on)) => self.set_sharing(on),
            Some(NoticeAct::Update) => self.start_update(),
            Some(NoticeAct::WhatsNew(pending)) => self.open_whats_new(pending),
            Some(NoticeAct::Settings) => self.set_tab(Tab::Settings),
            Some(NoticeAct::Dismiss) => self.seen_updated(),
            Some(NoticeAct::Relaunch) => self.start_relaunch(ui.ctx()),
            Some(NoticeAct::RelaunchAlways) => {
                self.settings.start_as_admin = true;
                self.save();
                self.start_relaunch(ui.ctx());
            }
            Some(NoticeAct::DismissElevated) => self.elevated_dismissed = true,
            Some(NoticeAct::RunCheck) => self.floats.open_health(),
            None => {}
        }
    }
}

/// Notice kinds of the status bar (see [`AscendantApp::notice`]); the release notes are borrowed, not copied for
/// every frame.
enum NoticeKind<'a> {
    Install,
    Share,
    /// A newer release: its version and verified notes, whether there is a notes history (see
    /// [`AscendantApp::notes_history`]), the update's step while it runs, and whether a check runs (which turns
    /// "Update" off).
    Update { version: String, notes: &'a str, history: bool, updating: Option<update::Step>, checking: bool },
    /// An update installed this version: its version and the notes kept from its verified release.
    Updated { version: String, notes: &'a str },
    /// The game runs with more rights than Ascendant, so the hotkeys do nothing while it is in front (or starting again
    /// as administrator was just declined): the text, and whether Windows' prompt is waiting for an answer.
    Elevated { text: String, waiting: bool },
}

/// The action of a notice button, run after the status bar is drawn (the state is borrowed while drawing).
enum NoticeAct {
    Install,
    NotNow,
    Share(bool),
    Update,
    /// Open the "What's new" window (see [`AscendantApp::whats_new_window`]), with the newer release on offer and not
    /// installed yet above the history when there is one (true: its notes are copied only when the button is
    /// pressed, see [`AscendantApp::pending_notes`]); no browser is opened.
    WhatsNew(bool),
    /// Open the Settings tab (the game log reminder).
    Settings,
    /// The "x" of "Updated to X.Y.Z": not shown again.
    Dismiss,
    /// Start again as administrator, this once.
    Relaunch,
    /// Turn on "Start as administrator" and start again as administrator now.
    RelaunchAlways,
    /// The "x" of the administrator notice: not shown again while Ascendant stays open.
    DismissElevated,
    /// Open the run check window (see [`AscendantApp::health_window`]).
    RunCheck,
}

/// One item of the status bar (see [`AscendantApp::status_items`]).
enum StatusItem<'a> {
    Error(String),
    Notice(NoticeKind<'a>),
    /// The game log light is red: the reminder (see [`log_alert`]).
    Log(String),
    Auto(String),
    /// Accumulated progress is full (`Vorax full 48/48`).
    Charge(String),
    Health { summary: String, issues: Vec<String> },
}

impl StatusItem<'_> {
    /// Short form for line 1 of the compact layout: auto read shows only "waiting"; the rest is the same as
    /// [`StatusItem::text`].
    fn short(&self) -> String {
        match self {
            StatusItem::Auto(_) => tr("Auto read waiting").to_string(),
            StatusItem::Log(_) => tr("Game log: see Settings").to_string(),
            _ => self.text(),
        }
    }

    fn text(&self) -> String {
        match self {
            StatusItem::Error(t) | StatusItem::Auto(t) | StatusItem::Charge(t) | StatusItem::Log(t) => t.clone(),
            StatusItem::Notice(NoticeKind::Install) => {
                // When updates are off (`update::ENABLED`) an install no longer brings updates, so the hint
                // does not mention them.
                tr(if update::ENABLED { "Install Ascendant: Start menu and desktop shortcuts, and updates" } else { "Install Ascendant: Start menu and desktop shortcuts" }).to_string()
            }
            StatusItem::Notice(NoticeKind::Share) => tr("Share anonymous run logs (no screenshots) to improve Ascendant?").to_string(),
            StatusItem::Notice(NoticeKind::Update { updating: Some(step), .. }) => tr(step.text()).to_string(),
            StatusItem::Notice(NoticeKind::Update { version, .. }) => trf!("Ascendant {} is available", version),
            StatusItem::Notice(NoticeKind::Updated { version, .. }) => trf!("Updated to {}", version),
            StatusItem::Notice(NoticeKind::Elevated { text, .. }) => text.clone(),
            StatusItem::Health { summary, .. } => summary.clone(),
        }
    }

    fn colour(&self) -> Color32 {
        match self {
            StatusItem::Error(_) | StatusItem::Charge(_) | StatusItem::Log(_) => theme::ERROR,
            StatusItem::Notice(NoticeKind::Elevated { .. }) => theme::WARN,
            StatusItem::Notice(_) => theme::TEXT,
            StatusItem::Auto(_) => MUTED,
            StatusItem::Health { issues, .. } if issues.is_empty() => theme::GOOD,
            StatusItem::Health { .. } => theme::WARN,
        }
    }

    /// How serious the item is, for the colour of the status bar's "+N": 2 an error, 1 something for the player to
    /// see to (the game log light red, the game run as administrator, progress full, a run check with issues), 0 the
    /// rest.
    fn severity(&self) -> u8 {
        match self {
            StatusItem::Error(_) => 2,
            StatusItem::Log(_) | StatusItem::Charge(_) | StatusItem::Notice(NoticeKind::Elevated { .. }) => 1,
            StatusItem::Health { issues, .. } if !issues.is_empty() => 1,
            StatusItem::Notice(_) | StatusItem::Auto(_) | StatusItem::Health { .. } => 0,
        }
    }
}

/// The colour of the status bar's "+N": the most serious of the items it folds away (`folded`, all but the one on
/// show), red for an error, yellow for something to see to, blue for the rest.
fn more_colour(folded: &[StatusItem]) -> Color32 {
    match folded.iter().map(StatusItem::severity).max() {
        Some(2) => theme::ERROR,
        Some(1) => theme::WARN,
        _ => ACCENT,
    }
}

/// The status bar's floating list of every item (`items`), each with its full text and buttons, opened by a click on
/// `anchor` ("+N", or the administrator notice's text) and closed by a click outside; at most `width` wide. `open`
/// keeps it open (previews). It floats over the page, so nothing below moves. The action of a button pressed in it.
fn status_popup(anchor: &egui::Response, items: &[StatusItem], width: f32, open: bool) -> Option<NoticeAct> {
    let (popup_w, popup_h) = popup_limits(&anchor.ctx);
    let mut act = None;
    let mut popup = egui::Popup::menu(anchor).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).width(popup_w.min(width));
    if open {
        popup = popup.open_memory(egui::SetOpenCommand::Bool(true));
    }
    popup.show(|ui| {
        egui::ScrollArea::vertical().max_height(popup_h).show(ui, |ui| {
            for (k, item) in items.iter().enumerate() {
                if k > 0 {
                    ui.separator();
                }
                if let Some(a) = status_detail(ui, item) {
                    act = Some(a);
                }
            }
        });
    });
    act
}

/// Below this width (points) the status bar's line leaves out a notice's buttons that are also elsewhere (see
/// [`notice_spare`]), so the notice's text is not cut to nothing.
const NARROW_STATUS: f32 = 340.0;

/// Small button of a notice. With `rtl` it is drawn in a right-to-left layout (the rightmost first) and the
/// left-to-right order of the buttons stays the same; `narrow` (the status bar's line, narrow) leaves out the spare
/// ones (they stay in the floating list and Settings).
fn notice_buttons(ui: &mut egui::Ui, kind: &NoticeKind, rtl: bool, narrow: bool) -> Option<NoticeAct> {
    let mut buttons = notice_choices(kind);
    if narrow {
        buttons.retain(|(_, _, a)| !notice_spare(a));
    }
    if rtl {
        buttons.reverse();
    }
    let mut act = None;
    for (text, enabled, a) in buttons {
        // On the status bar's one line a long label gives way to a short one; the hover has the whole of it.
        let shown = if rtl { notice_short(&a).map_or(text.clone(), str::to_string) } else { text.clone() };
        let mut r = ui.add_enabled(enabled, egui::Button::new(RichText::new(shown).small()).small());
        if let Some(hover) = notice_hover(&a) {
            r = r.on_hover_text(format!("{text}\n{hover}"));
        }
        if r.clicked() {
            act = Some(a);
        }
    }
    if matches!(kind, NoticeKind::Update { updating: Some(_), .. }) {
        ui.add(egui::Spinner::new().size(12.0));
    }
    act
}

/// The buttons of a notice, left to right: text, whether enabled, and action. "What's new" shows the verified notes
/// inside Ascendant (never the GitHub release page, whose text is not signed): a newer release's own notes, marked
/// not installed yet, above the history; it is off when there are none to show.
fn notice_choices(kind: &NoticeKind) -> Vec<(String, bool, NoticeAct)> {
    match kind {
        NoticeKind::Install => vec![(tr("Install").into(), true, NoticeAct::Install), (tr("Not now").into(), true, NoticeAct::NotNow)],
        NoticeKind::Share => vec![(tr("Share").into(), true, NoticeAct::Share(true)), (tr("Don't share").into(), true, NoticeAct::Share(false))],
        NoticeKind::Update { notes, history, updating, checking, .. } => {
            // Only whether there are notes: they are copied when "What's new" is pressed, not for every frame.
            let pending = !notes.is_empty();
            let enabled = updating.is_none() && (pending || *history);
            vec![(tr("Update").into(), updating.is_none() && !checking, NoticeAct::Update), (tr("What's new").into(), enabled, NoticeAct::WhatsNew(pending))]
        }
        // The version just installed: its notes are the newest of the history.
        NoticeKind::Updated { notes, .. } => vec![(tr("What's new").into(), !notes.is_empty(), NoticeAct::WhatsNew(false)), ("\u{d7}".into(), true, NoticeAct::Dismiss)],
        // Off while Windows' prompt waits for an answer, so one click starts one copy.
        NoticeKind::Elevated { waiting, .. } => vec![
            (tr("Restart as administrator").into(), !waiting, NoticeAct::Relaunch),
            (tr("Always").into(), !waiting, NoticeAct::RelaunchAlways),
            ("\u{d7}".into(), true, NoticeAct::DismissElevated),
        ],
    }
}

/// Whether a notice button can be left out of a narrow status bar line: "Always" of the administrator notice. It stays
/// in the floating list that a click on the notice's text opens (or "+N", see [`status_popup`]), and it is the switch
/// "Start as administrator" in Settings too.
fn notice_spare(act: &NoticeAct) -> bool {
    matches!(act, NoticeAct::RelaunchAlways)
}

/// A notice button's short label for the status bar's one line, when its label is long.
fn notice_short(act: &NoticeAct) -> Option<&'static str> {
    match act {
        NoticeAct::Relaunch => Some(tr("Restart")),
        _ => None,
    }
}

/// What a notice button does, for its hover, when its label does not say it all.
fn notice_hover(act: &NoticeAct) -> Option<&'static str> {
    match act {
        NoticeAct::Relaunch => Some(tr("Windows asks for administrator permission, then Ascendant starts again as administrator and this window closes; the settings are kept")),
        NoticeAct::RelaunchAlways => Some(tr("Turns on Start as administrator in Settings and restarts as administrator now; Windows asks at every start")),
        NoticeAct::DismissElevated => Some(tr("Not shown again while Ascendant stays open")),
        _ => None,
    }
}

/// The background settlement of the `game` run in folder `dir` ([`settle::spawn`] or [`super::vorax_settle::spawn`]),
/// once `written` says its log is written; it repaints the window when done.
fn spawn_settle(ctx: &egui::Context, dir: PathBuf, game: Game, written: Option<Receiver<()>>) -> Receiver<Outcome> {
    let ctx = ctx.clone();
    let notify = move || ctx.request_repaint();
    match game {
        Game::Tartarus => settle::spawn(dir, written, notify),
        Game::Vorax => super::vorax_settle::spawn(dir, written, notify),
    }
}

/// The "What's new" window while it is open: the newer release on offer (not installed yet) with its verified notes,
/// listed above the built-in ones, when the window was opened from it; and a number of its own, so the folding of
/// each version starts afresh every time the window opens.
struct WhatsNew {
    pending: Option<update::SavedNotes>,
    opened: u64,
}

impl WhatsNew {
    fn new(pending: Option<update::SavedNotes>) -> WhatsNew {
        static OPENED: AtomicU64 = AtomicU64::new(0);
        WhatsNew { pending, opened: OPENED.fetch_add(1, Ordering::Relaxed) }
    }
}

/// The floating windows anchored at the same spot under the title bar: "What's new" (see
/// [`AscendantApp::whats_new_window`]), the instruments and theme window (see [`AscendantApp::tools_window`]) and the
/// run check (see [`AscendantApp::health_window`]). Only one is open at a time, so none covers another: opening one
/// closes the others.
#[derive(Default)]
struct Floats {
    whats_new: Option<WhatsNew>,
    tools: bool,
    health: bool,
    /// The one open as a window of its own: where it was opened and where it was last seen (see [`Placing`]).
    placing: Option<Placing>,
}

/// One of the small windows that open as a window of the system's own (1.1.0, the user 2026-11-08: they floated inside
/// the main window and could not be dragged out of it). Each is its own always-on-top window of fixed size, not
/// resizable and without minimize or maximize, out of the taskbar; it can be dragged anywhere, and where the player
/// leaves it is kept in the settings by its key (see [`Settings::float_pos`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Float {
    WhatsNew,
    Health,
    Tools,
}

impl Float {
    /// The key its place is kept under in the settings.
    fn key(self) -> &'static str {
        match self {
            Float::WhatsNew => "whats_new",
            Float::Health => "run_check",
            Float::Tools => "tools",
        }
    }

    fn id(self) -> egui::ViewportId {
        egui::ViewportId::from_hash_of(("ascendant-float", self.key()))
    }

    /// Its inner size in points, fixed so the layout never moves (the text wraps to it and scrolls when longer); the
    /// width is that of the floating windows before (at most 360, see [`popup_width`]).
    fn size(self) -> Vec2 {
        match self {
            Float::WhatsNew => Vec2::new(360.0, 440.0),
            Float::Health => Vec2::new(360.0, 300.0),
            Float::Tools => Vec2::new(250.0, 300.0),
        }
    }
}

/// The small window open as a window of its own: which, the position (points) it was created at, which stays the same
/// for its whole life (a builder that changed position would move it back while dragged), and its outer top left
/// (physical pixels) as last seen, with whether a move is not saved yet.
struct Placing {
    which: Float,
    at: Option<Pos2>,
    last: Option<[i32; 2]>,
    unsaved: bool,
    /// When it opened, or was last moved by the program (see [`FLOAT_SETTLES`]).
    opened: Instant,
}

/// How long after a small window opens, or the program moves it, its position changing is not taken as a move of the
/// player's.
const FLOAT_SETTLES: Duration = Duration::from_millis(300);

/// Ascendant's icon for the small windows' title bars, decoded once.
fn float_icon() -> Option<std::sync::Arc<egui::IconData>> {
    static ICON: std::sync::OnceLock<Option<std::sync::Arc<egui::IconData>>> = std::sync::OnceLock::new();
    ICON.get_or_init(|| eframe::icon_data::from_png_bytes(include_bytes!("../../assets/icon.png")).ok().map(std::sync::Arc::new)).clone()
}

/// Where a small window of `size` (points) opens when the player has not placed it: over the main window's page,
/// centred across it under the status bar, where it floated before (`main` the main window's outer rect, points).
fn float_default_at(main: Rect, size: Vec2) -> Pos2 {
    Pos2::new(main.min.x + RAIL / 2.0 + (main.width() - size.x) / 2.0, main.min.y + 30.0)
}

/// Whether the outer top left `now` (physical pixels) of a small window, seen `last` the frame before, is a move the
/// player made: a change of more than 2 pixels; the first sight of a window is none.
fn float_moved(last: Option<[i32; 2]>, now: [i32; 2]) -> bool {
    last.is_some_and(|l| (l[0] - now[0]).abs() > 2 || (l[1] - now[1]).abs() > 2)
}

impl Floats {
    fn open_whats_new(&mut self, w: WhatsNew) {
        self.whats_new = Some(w);
        (self.tools, self.health) = (false, false);
    }

    /// A click on the instruments line: opens the window (closing the others) or closes it.
    fn toggle_tools(&mut self) {
        self.tools = !self.tools;
        if self.tools {
            (self.whats_new, self.health) = (None, false);
        }
    }

    /// A click on the run check's line: opens its window, closing the others.
    fn open_health(&mut self) {
        self.health = true;
        (self.whats_new, self.tools) = (None, false);
    }
}

/// A made-up run check for previews (ASCENDANT_PREVIEW_HEALTH): with `issues`, three of the run check's own sentences,
/// else all clear.
fn preview_health(issues: bool) -> super::health::Report {
    let issues = if issues {
        vec![
            trf!("Day {}: clicked with no advice on show.", 15),
            trf!("Day {}: clicked with no advice on show.", 18),
            trf!("The result, {} Chromatics' worth, fell far below the {} predicted on day 1.", "4.0", "8.2"),
        ]
    } else {
        Vec::new()
    };
    super::health::Report { decisions: 312, cards_median: Some(0.42), issues }
}

/// Whether Esc (`pressed` this frame) closes "What's new": not while a text box has the keyboard or a hotkey is
/// being recorded (`busy`, as at the frame's start), whose Esc it is.
fn esc_closes(pressed: bool, busy: bool) -> bool {
    pressed && !busy
}

/// The release notes of every version, one file per version in data/release-notes (`<X.Y.Z>.txt`, UTF-8 without a
/// byte order mark, LF, at most 8000 characters, no control or hidden format characters; the tests check all of it
/// and that the running version has one), one section per language, each opened by its marker line (`[[zh-TW]]`,
/// `[[en]]`, see [`notes_in`]). They are built into the exe, which the release's signature and SHA-256
/// cover, so they are as trusted as the program and a fresh install sees the whole history. A new version adds its
/// file here.
const RELEASE_NOTES: &[(&str, &str)] =
    &[("1.2.0", include_str!("../../data/release-notes/1.2.0.txt")), ("1.1.0", include_str!("../../data/release-notes/1.1.0.txt")),("1.0.4", include_str!("../../data/release-notes/1.0.4.txt")),("1.0.3", include_str!("../../data/release-notes/1.0.3.txt")), ("1.0.2", include_str!("../../data/release-notes/1.0.2.txt")), ("1.0.1", include_str!("../../data/release-notes/1.0.1.txt")), ("1.0.0", include_str!("../../data/release-notes/1.0.0.txt"))];

/// Which of the versions "What's new" lists (in its order, each with whether it is a release not installed yet) starts
/// unfolded: the release not installed yet when there is one, else the `running` version, else the newest.
fn unfolded_notes(shown: &[(&str, bool)], running: &str) -> Option<usize> {
    shown.iter().position(|&(_, pending)| pending).or_else(|| shown.iter().position(|&(v, _)| v == running)).or((!shown.is_empty()).then_some(0))
}

/// The notes "What's new" lists, newest first by version (1.0.10 above 1.0.9): the built-in ones ([`RELEASE_NOTES`]),
/// and `saved` (the notes an update kept for the running version, see [`update::load_notes_in`]) only for a version
/// that has none built in. When both have a version, the built-in text is shown: it is part of the exe that was
/// verified, and the one shipped with that build.
fn release_notes(saved: Option<&update::SavedNotes>) -> Vec<update::SavedNotes> {
    let mut notes: Vec<update::SavedNotes> = RELEASE_NOTES.iter().map(|&(version, text)| update::SavedNotes { version: version.to_string(), notes: update::plain_notes(text) }).collect();
    if let Some(s) = saved.filter(|s| notes.iter().all(|n| n.version != s.version)) {
        notes.push(s.clone());
    }
    newest_first(&mut notes);
    notes
}

/// Sorts `notes` newest first by semantic version (1.0.10 above 1.0.9), not by text.
fn newest_first(notes: &mut [update::SavedNotes]) {
    notes.sort_by_key(|n| std::cmp::Reverse(update::semver(&n.version).unwrap_or_default()));
}

/// How many lines of the release notes are shown; the whole text is in the tooltip.
const NOTES_LINES: usize = 4;

/// The release notes as plain text: the first [`NOTES_LINES`] lines, with all of them on hover.
fn notes_label(ui: &mut egui::Ui, notes: &str) {
    let notes = notes_in(notes, lang::lang().key());
    let (preview, more) = update::notes_preview(notes, NOTES_LINES);
    let shown = if more { format!("{preview}\n\u{2026}") } else { preview };
    ui.add(egui::Label::new(RichText::new(shown).small().color(MUTED)).wrap()).on_hover_text(notes);
}

/// The language key of a section marker line: `[[<key>]]` alone on its line (spaces around allowed), the key being
/// [`lang::Lang::key`]'s value (`zh-TW`, `en`; other keys such as `ja` are sections too, for languages to come).
fn notes_marker(line: &str) -> Option<&str> {
    let key = line.trim().strip_prefix("[[")?.strip_suffix("]]")?;
    (!key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')).then_some(key)
}

/// The section of release notes in language `key` ([`lang::Lang::key`], matched exactly), for display only: the
/// signature and checks always use the whole text. Each section starts with its marker line ([`notes_marker`]), which
/// is not shown, and runs to the next marker; blank lines at both ends are trimmed. With no section in `key`, the `en`
/// one is shown, else the first; text before the first marker is not shown. Notes without any marker, or whose
/// sections are all blank, are shown whole, so nothing is ever lost.
fn notes_in<'a>(notes: &'a str, key: &str) -> &'a str {
    let mut sections: Vec<(&str, usize, usize)> = Vec::new();
    let mut at = 0;
    for line in notes.split_inclusive('\n') {
        if let Some(k) = notes_marker(line) {
            if let Some(last) = sections.last_mut() {
                last.2 = at;
            }
            sections.push((k, at + line.len(), notes.len()));
        }
        at += line.len();
    }
    let text = |&(_, start, end): &(&str, usize, usize)| notes[start..end].trim();
    let pick = |wanted: &str| sections.iter().find(|s| s.0 == wanted).map(text).filter(|t| !t.is_empty());
    pick(key).or_else(|| pick("en")).or_else(|| sections.iter().map(text).find(|t| !t.is_empty())).unwrap_or(notes)
}

/// Whether an update failure leaves "Update" off until Ascendant starts again: the swap could not be undone
/// ([`update::Failed::Lost`]) or the new exe is in place but did not start ([`update::Failed::NotRestarted`]). Either
/// way the exe on disk is no longer the running one, and its text says what the player has to do.
fn locks_update(e: &update::Failed) -> bool {
    matches!(e, update::Failed::Lost { .. } | update::Failed::NotRestarted(_))
}

/// Whether a preview injects the update state (ASCENDANT_PREVIEW_UPDATE or ASCENDANT_PREVIEW_UPDATED is set): then
/// GitHub is never asked, at start or hourly.
fn preview_update() -> bool {
    std::env::var_os("ASCENDANT_PREVIEW_UPDATE").is_some() || std::env::var_os("ASCENDANT_PREVIEW_UPDATED").is_some()
}

/// The release on offer, and whether it is quiet (the red dot only), after an hourly check found `found`: a release
/// the status bar already offers (`current` and not `quiet`) stays there, even when the check answers none (it goes
/// when the player installs it or Ascendant restarts); otherwise what the check found is offered quietly.
fn quiet_release(current: Option<Release>, quiet: bool, found: Option<Release>) -> (Option<Release>, bool) {
    match (current, found) {
        (Some(shown), None) if !quiet => (Some(shown), false),
        (Some(_), Some(found)) if !quiet => (Some(found), false),
        (_, found) => (found, true),
    }
}

/// Records that the "Updated to" notice of `saved` was seen (its "x", or a game played since), so it does not show
/// again; true when that changed `settings` (they need saving then).
fn dismiss_updated(settings: &mut Settings, saved: Option<&update::SavedNotes>) -> bool {
    match saved {
        Some(s) if settings.updated_seen != s.version => {
            settings.updated_seen = s.version.clone();
            true
        }
        _ => false,
    }
}

/// A wrapped line of small text in `colour`, with `detail` (the technical reason) on hover when there is one.
fn problem_label(ui: &mut egui::Ui, text: &str, detail: &str, colour: Color32) {
    let r = ui.add(egui::Label::new(RichText::new(text).small().color(colour)).wrap());
    if !detail.is_empty() {
        r.on_hover_text(detail);
    }
}

/// One item in the status bar's floating list: the full text wraps, a notice adds its buttons, and the run
/// check adds its details.
fn status_detail(ui: &mut egui::Ui, item: &StatusItem) -> Option<NoticeAct> {
    ui.add(egui::Label::new(RichText::new(item.text()).small().color(item.colour())).wrap());
    match item {
        StatusItem::Notice(kind) => {
            if let NoticeKind::Update { notes, .. } | NoticeKind::Updated { notes, .. } = kind
                && !notes.is_empty()
            {
                notes_label(ui, notes);
            }
            ui.horizontal(|ui| notice_buttons(ui, kind, false, false)).inner
        }
        StatusItem::Health { issues, .. } => {
            for issue in issues {
                ui.add(egui::Label::new(RichText::new(issue).small()).wrap());
            }
            ui.small_button(tr("See")).on_hover_text(tr("Click to see the run check")).clicked().then_some(NoticeAct::RunCheck)
        }
        StatusItem::Log(_) => {
            ui.add(egui::Label::new(RichText::new(tr(LOG_NOTE)).small().color(MUTED)).wrap());
            ui.small_button(tr("Open Settings")).clicked().then_some(NoticeAct::Settings)
        }
        _ => None,
    }
}

/// A chain badge's number and its cell (centre and half size, frame px), as [`Session::chain_spots`] gives them.
type ChainSpot = (usize, (f32, f32, f32, f32));

/// Where the expected value goes on a Cage Fight's target screen, whose numbered badges stand in for the mark: the
/// first badge's cell (`spots` as [`Session::chain_spots`] gives them, frame px), as a mark's spot in screen px
/// (`[x, y, half_w, half_h, unit]`), with the game's top; None without a badge or without the game's rect.
fn first_badge_spot(spots: &[ChainSpot], game: Option<[i32; 4]>) -> Option<([f32; 5], i32)> {
    let [left, top, _, height] = game?;
    let &(_, (x, y, half_w, half_h)) = spots.first()?;
    Some(([left as f32 + x, top as f32 + y, half_w, half_h, height as f32 / 1080.0], top))
}

/// A chain badge's size against the notice's (see [`AscendantApp::show_chain`]).
const BADGE_SCALE: f32 = 0.9;
/// How far left of a target's edge a chain badge's centre sits, in reference
/// px: clear of the mark's box (11 px out, its faint glow beyond; see
/// [`mark`]), the badge about 38 px wide, and short of the next cell's
/// portrait.
const BADGE_LEFT: f32 = 34.0;

/// Runs `job` on a thread; its result comes over the returned channel, with a repaint.
/// A "game" row of the run log: a signal from the game's log, how long
/// after its line's time it was read off the log (`late_ms`), and how long
/// it then waited for the window (`queued_ms`).
fn game_row(signal: super::gamelog::Signal, stamp: Option<i64>, read: SystemTime) -> serde_json::Value {
    let ms = |t: SystemTime| t.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
    serde_json::json!({
        "signal": format!("{signal:?}"),
        "late_ms": stamp.map(|at| ms(read) - at),
        "queued_ms": SystemTime::now().duration_since(read).map_or(0, |d| d.as_millis() as i64),
    })
}

fn in_background<T: Send + 'static>(ctx: &egui::Context, job: impl FnOnce() -> T + Send + 'static) -> Receiver<T> {
    let (tx, rx) = mpsc::channel();
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let _ = tx.send(job());
        ctx.request_repaint();
    });
    rx
}

/// UI fonts (the font spec the user confirmed on 2026-11-05): Chinese, English and digits all use "Microsoft
/// JhengHei UI", placed first for both Proportional and Monospace, replacing egui's built-in thin English font
/// (mixing the two was hard to read before).
/// msjh.ttc is a font collection: face 0 is Microsoft JhengHei and face 1 is Microsoft JhengHei UI (checked
/// against the files on this Windows 10 machine on 2026-11-05); this is not hard-coded, [`face_index`] finds it
/// by name and falls back to face 0 only if the name is not found.
/// Without JhengHei it falls back, in order, to msjh.ttf, PMingLiU and Microsoft YaHei; if none exists only
/// egui's default font is used (Chinese turns into boxes).
pub fn add_ui_font(ctx: &egui::Context) {
    let fonts_dir = std::env::var_os("WINDIR").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from).join("Fonts");
    let candidates = [("msjh.ttc", "Microsoft JhengHei UI"), ("msjh.ttf", "Microsoft JhengHei UI"), ("mingliu.ttc", "MingLiU"), ("msyh.ttc", "Microsoft YaHei UI")];
    let Some((bytes, index)) = candidates.iter().find_map(|(file, family)| {
        let bytes = std::fs::read(fonts_dir.join(file)).ok()?;
        let index = face_index(&bytes, family).unwrap_or(0);
        Some((bytes, index))
    }) else {
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    let mut data = egui::FontData::from_owned(bytes);
    data.index = index;
    fonts.font_data.insert("ui".to_owned(), Arc::new(data));
    // Proportional puts JhengHei UI first; Monospace keeps egui's original monospace font first with JhengHei
    // UI as the fallback (for Chinese characters), so monospace content such as the board as read lines up.
    fonts.families.entry(egui::FontFamily::Proportional).or_default().insert(0, "ui".to_owned());
    fonts.families.entry(egui::FontFamily::Monospace).or_default().push("ui".to_owned());
    ctx.set_fonts(fonts);
}

/// Index of the face in a TrueType collection (.ttc) whose family name (name table ID 1, Windows platform,
/// English) equals `family`; for a non-collection there is only face 0, and 0 is returned if the name matches.
/// Returns None for a file it cannot parse.
fn face_index(bytes: &[u8], family: &str) -> Option<u32> {
    let u16_at = |at: usize| bytes.get(at..at + 2).map(|b| u16::from_be_bytes([b[0], b[1]]));
    let u32_at = |at: usize| bytes.get(at..at + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
    let faces: Vec<usize> = if bytes.get(..4) == Some(b"ttcf".as_slice()) {
        let count = u32_at(8)? as usize;
        (0..count).map(|k| u32_at(12 + 4 * k).map(|o| o as usize)).collect::<Option<_>>()?
    } else {
        vec![0]
    };
    let wanted: Vec<u16> = family.encode_utf16().collect();
    for (k, &face) in faces.iter().enumerate() {
        let tables = u16_at(face + 4)? as usize;
        let Some(name) = (0..tables).map(|t| face + 12 + 16 * t).find(|&rec| bytes.get(rec..rec + 4) == Some(b"name".as_slice())) else { continue };
        let table = u32_at(name + 8)? as usize;
        let (count, strings) = (u16_at(table + 2)? as usize, table + u16_at(table + 4)? as usize);
        for r in 0..count {
            let rec = table + 6 + 12 * r;
            let (platform, language, id) = (u16_at(rec)?, u16_at(rec + 4)?, u16_at(rec + 6)?);
            if platform != 3 || language != 0x409 || id != 1 {
                continue;
            }
            let (len, at) = (u16_at(rec + 8)? as usize, strings + u16_at(rec + 10)? as usize);
            let text: Option<Vec<u16>> = (0..len / 2).map(|i| u16_at(at + 2 * i)).collect();
            if text? == wanted {
                return Some(k as u32);
            }
        }
    }
    None
}

/// Standard font sizes (font spec point 2): body 15, small 13 (the same for Chinese and English), buttons the
/// same as body, and headings and monospace scaled from egui's default proportions (default body 13). The
/// "Large" / "Extra large" font sizes use the overall scale ([`text_scale`]) and do not change this.
pub fn text_sizes(ctx: &egui::Context) {
    const BODY: f32 = 15.0;
    const SMALL: f32 = 13.0;
    let defaults = egui::Style::default().text_styles;
    let ratio = BODY / defaults[&egui::TextStyle::Body].size;
    ctx.all_styles_mut(|style| {
        for (text_style, font) in style.text_styles.iter_mut() {
            font.size = match text_style {
                egui::TextStyle::Small => SMALL,
                egui::TextStyle::Body | egui::TextStyle::Button => BODY,
                other => defaults.get(other).map_or(font.size, |d| (d.size * ratio).round()),
            };
        }
    });
}

/// egui tooltips and floating windows cannot be drawn outside the native window (on 2026-11-05 the user
/// reported a tooltip cut off at the right edge): every frame the tooltip width is set to min(window width −
/// 16, 360) points so long tooltips wrap automatically; egui constrains the position to the window (the tooltip
/// Area's default constrain). The style is changed only when the width changed. Other floating things are
/// limited on their own: the status bar `＋N` list (width ≤ window width, scrolls when too tall), the
/// instruments and theme window and the cell correction window (`popup_limits`), and the restore-defaults
/// confirmation window (width follows the window width).
fn fit_popups(ctx: &egui::Context) {
    let width = popup_width(ctx.content_rect().width());
    if (ctx.global_style().spacing.tooltip_width - width).abs() > 0.5 {
        ctx.all_styles_mut(|style| style.spacing.tooltip_width = width);
    }
}

/// Width of tooltips and floating lists: window width minus 8 points on each side, at most 360 and at least
/// 120.
fn popup_width(window: f32) -> f32 {
    (window - 16.0).clamp(120.0, 360.0)
}

/// Maximum width and height of a floating window (egui::Window): width no more than the window width and height
/// no more than the window height (minus margins and the title bar); the window scrolls when it exceeds that.
fn popup_limits(ctx: &egui::Context) -> (f32, f32) {
    let r = ctx.content_rect();
    (popup_width(r.width()), (r.height() - 56.0).max(80.0))
}

/// Convert the font size in settings ([`Settings::font_scale`]) into the actual overall scale: take the closest
/// one in [`TEXT_SCALES`]; a non-finite value (a corrupted settings file) gives the standard size.
fn text_scale(setting: f32) -> f32 {
    if !setting.is_finite() {
        return 1.0;
    }
    TEXT_SCALES.iter().map(|&(scale, _)| scale).min_by(|a, b| (a - setting).abs().total_cmp(&(b - setting).abs())).unwrap_or(1.0)
}

/// Physical pixels ÷ points: the system scale times the font size scale. egui-winit uses this to convert the
/// window's position and size (`pixels_per_point = zoom_factor × native_pixels_per_point`); with only the
/// system scale, the position remembered and restored at "Large" / "Extra large" would be off.
fn physical_per_point(ctx: &egui::Context) -> f32 {
    let zoom = ctx.zoom_factor();
    ctx.input(|i| i.viewport().native_pixels_per_point.map_or(i.pixels_per_point, |n| n * zoom))
}

/// This app's window.
fn own_window(frame: &eframe::Frame) -> Option<windows::Win32::Foundation::HWND> {
    use eframe::wgpu::rwh::{HasWindowHandle, RawWindowHandle};
    let RawWindowHandle::Win32(handle) = frame.window_handle().ok()?.as_raw() else { return None };
    Some(windows::Win32::Foundation::HWND(handle.hwnd.get() as *mut _))
}

/// Sets `hwnd`'s opacity (see [`settings::MIN_OPACITY`]); with
/// `click_through` it stays layered, which `WS_EX_TRANSPARENT` needs (see
/// [`ClickThrough`], which owns that bit). `last` is the opacity as last
/// set, returned anew: winit writes the whole extended style on some of its
/// own changes, which drops these, so it is checked on every frame and set
/// again only when it differs.
fn see_through(hwnd: windows::Win32::Foundation::HWND, opacity: f32, click_through: bool, last: Option<u8>) -> Option<u8> {
    use windows::Win32::Foundation::COLORREF;
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetWindowLongPtrW, LWA_ALPHA, SetLayeredWindowAttributes, SetWindowLongPtrW, WS_EX_LAYERED, WS_EX_TRANSPARENT,
    };
    let alpha = (opacity.clamp(settings::MIN_OPACITY, 1.0) * 255.0).round() as u8;
    let layered = alpha < 255 || click_through;
    let (layer, through) = (WS_EX_LAYERED.0 as isize, WS_EX_TRANSPARENT.0 as isize);
    // SAFETY: plain Win32 calls on this app's own window.
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let mut want = style & !(layer | through);
        if layered {
            want |= layer;
        }
        if click_through {
            want |= style & through;
        }
        if want == style && last == Some(alpha) {
            return last;
        }
        if want != style {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, want);
        }
        // A window made layered shows nothing until its opacity is set.
        if layered {
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA);
        }
        Some(alpha)
    }
}

/// The rects, in client pixels, of the last frame's widgets that take a
/// click or a drag: where click-through leaves clicks to this window.
fn widget_rects(ctx: &egui::Context) -> Vec<[i32; 4]> {
    let scale = ctx.pixels_per_point();
    let rects: Vec<(egui::LayerId, Rect)> = ctx.viewport(|vp| {
        let widgets = &vp.prev_pass.widgets;
        widgets
            .layer_ids()
            .flat_map(|layer| widgets.get_layer(layer).filter(|w| w.enabled && w.sense.interactive()).map(move |w| (layer, w.interact_rect)))
            .collect()
    });
    rects
        .into_iter()
        .map(|(layer, rect)| client_pixels(scale, ctx.layer_transform_to_global(layer).map_or(rect, |t| t * rect)))
        .collect()
}

/// A rect in points as client pixels (left, top, right, bottom), rounded outward.
fn client_pixels(scale: f32, rect: Rect) -> [i32; 4] {
    [(rect.min.x * scale).floor() as i32, (rect.min.y * scale).floor() as i32, (rect.max.x * scale).ceil() as i32, (rect.max.y * scale).ceil() as i32]
}

/// The page's scroll area that keeps the mouse under click-through, so the wheel scrolls it: the visible part of a
/// page taller than the window, on every tab but the board (where clicks on the empty parts are meant for the game
/// under it). egui's scroll areas take drags only on touch screens, so with click-through on the wheel reached the
/// page only over a widget and went to the game everywhere else (the user's report of 2026-11-09).
fn wheel_area(tab: Tab, content_h: f32, view: Rect) -> Option<Rect> {
    (tab != Tab::Board && content_h > view.height() + 0.5).then_some(view)
}

impl eframe::App for AscendantApp {
    /// The hotkeys, auto read and the worker results. eframe skips `ui` while
    /// the game covers the window but still calls this on a repaint request.
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // Recording a hotkey stops once the window loses the keyboard (a click on the game, say), here as well as in
        // `ui`, which eframe may skip while the window is covered: the hotkeys, released while recording, come back.
        if self.recording.is_some() && !self.preview_recording && !ctx.input(|i| i.focused) {
            self.stop_recording();
        }
        self.place(ctx);
        self.remember_position(ctx);
        self.poll_update(ctx);
        self.poll_ocr_install();
        self.poll_relaunch(ctx);
        // The game's rights: looked up every few seconds, each game process judged once (see GameRights).
        self.rights.poll(Instant::now(), game_pid, elevation::probe);
        if self.keys[Which::Read as usize].pressed.swap(false, Ordering::SeqCst) {
            self.read_now();
        }
        if self.keys[Which::Auto as usize].pressed.swap(false, Ordering::SeqCst) {
            self.set_auto_read(!self.settings.auto_read);
        }
        if let Some(hwnd) = own_window(frame) {
            self.see_through = see_through(hwnd, self.settings.opacity, self.click_through, self.see_through);
            match (&self.through, self.click_through) {
                (None, true) => self.through = Some(ClickThrough::start(hwnd)),
                (Some(_), false) => self.through = None,
                _ => {}
            }
        }
        if let Some(through) = &self.through {
            let mut rects = widget_rects(ctx);
            rects.extend(self.wheel_area.map(|r| client_pixels(ctx.pixels_per_point(), r)));
            through.set_rects(rects);
        }
        while let Some(event) = self.watcher.as_ref().and_then(Watcher::event) {
            // The captured window is updated whether or not Vorax mode is on (in Vorax mode route intercepts
            // the events).
            if let Event::Window(window) = &event {
                self.watching = Some(window.clone());
            }
            // Vorax mode: frames and log hints go to VoraxView; switching pauses or resumes the Tartarus
            // Session.
            let Some(event) = self.vorax.route(event, &mut self.session, self.watcher.as_ref()) else { continue };
            match event {
                Event::Frame(frame, trigger) => {
                    self.session.read_triggered(frame, trigger);
                    self.path.clear();
                }
                Event::Ahead(frame) => self.session.read_ahead(frame),
                Event::Status(status) => {
                    if status.is_some() {
                        self.watching = None;
                    }
                    self.auto_status = status;
                }
                Event::Window(window) => {
                    self.session.log_window(&window);
                    self.watching = Some(window);
                }
                Event::Treasure => self.session.note_treasure(),
                Event::Day { cur, max } => self.session.note_day(cur, max),
                Event::Game(signal, stamp, read) => {
                    if let Some(log) = &self.session.log {
                        log.event("game", read, game_row(signal, stamp, read));
                    }
                    if confirms_click(signal) {
                        self.move_seen = Some(Instant::now());
                    }
                    self.session.note_game(signal);
                }
                Event::Observe(frame, at) => {
                    if let Some(log) = &self.session.log {
                        log.observed(frame, at);
                    }
                }
                Event::RiotBoss(boss) => self.session.log_riot_boss(boss),
                // How the riot went: into the run log of the run just ended, and its settlement on the Run tab.
                Event::RiotEnd(end) => {
                    if let Some(dir) = self.session.log_riot_end(end) {
                        self.review.riot_done(&dir, end);
                    }
                }
                Event::MapLog(..) | Event::MapShot { .. } => self.map_event(event),
            }
        }
        self.poll_vorax_settle();
        self.poll_affix_live();
        // For previews (ASCENDANT_PREVIEW_SWITCH): the page switched once its time comes.
        if let Some((at, tab)) = self.preview_switch {
            let now = Instant::now();
            if now >= at {
                self.preview_switch = None;
                self.set_tab(tab);
            } else {
                ctx.request_repaint_after(at - now);
            }
        }
        // For previews (ASCENDANT_PREVIEW_PICK_AT): the run picked once its time comes.
        if let Some((at, _)) = &self.preview_pick {
            let now = Instant::now();
            if now >= *at {
                if let Some((_, dir)) = self.preview_pick.take() {
                    self.review_run(RunEntry::of(dir));
                }
            } else {
                ctx.request_repaint_after(*at - now);
            }
        }
        self.poll_review();
        if self.vorax.active() {
            // Vorax mode: Tartarus polling, marking, tooltips and the view log are all skipped; only the Vorax
            // box is drawn.
            self.vorax.tick(ctx, self.watcher.as_ref(), &self.settings);
            if let Some(m) = &mut self.mark {
                m.hide();
            }
            if let Some(n) = &mut self.notice {
                n.hide();
            }
            if let Some(v) = &mut self.value {
                v.hide();
            }
            self.badges.iter_mut().for_each(Notice::hide);
            // While in Vorax mode the Board tab does not show Tartarus's warning count.
            self.warned = 0;
            self.poll_upload();
            return;
        }
        // Not in Vorax mode: if the Vorax box is still drawn, take it down (nothing to do when both are
        // hidden).
        self.vorax.hide_marks();
        self.session.poll();
        // A Tartarus reading (resuming from Vorax drops the last one, so any is new) dismisses the Vorax result card:
        // with the log's light red, or auto read off and reads by hand, nothing else cleared it and it covered the
        // board.
        if self.session.reading.is_some() {
            self.vorax.dismiss_result();
        }
        if let Some((news, off_board)) = self.session.take_news()
            && let Some(w) = &self.watcher
        {
            w.report(news, off_board);
        }
        if self.session.wants_reread() {
            match &self.watcher {
                Some(w) => w.read_next(),
                None => self.read_now(),
            }
        }
        if self.session.wants_recheck()
            && let Some(w) = &self.watcher
        {
            w.read_soon();
        }
        if let Some(after) = self.session.wants_read_in()
            && let Some(w) = &self.watcher
        {
            w.read_in(after);
        }
        self.poll_upload();
        let warned = self.session.reading.as_ref().map_or(0, |r| r.warnings.iter().filter(|w| w.kind != WarningKind::Note).count());
        if self.warned == 0 && warned > 0 {
            self.set_tab(Tab::Board);
        }
        self.warned = warned;
        // The count goes with the held instruments and the theme, so a
        // restart in a new run (a higher count) clears them.
        let held = !self.session.tools.is_empty() || self.session.theme.is_some();
        let recount = held && self.settings.tools_days != self.session.days_left();
        if self.session.take_tools_changed() || recount {
            self.settings.tools = self.session.tools.iter().map(|t| t.key().to_string()).collect();
            self.settings.theme = self.session.theme;
            self.settings.tools_days = self.session.days_left();
            self.save();
        }
        self.highlight(ctx);
        self.show_notice(ctx);
        self.show_chain(ctx);
        self.log_view();
    }


    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        // Before anything is drawn: whose Esc this frame's is (see `esc_closes`), and the key a hotkey being recorded
        // takes out of the frame.
        self.esc_busy = ui.ctx().text_edit_focused() || self.recording.is_some();
        self.record_key(&ui.ctx().clone());
        // Measure the size after the player drags and remember it (reused on the next launch); the window no
        // longer resizes itself.
        self.track_size(ui.ctx());
        fit_popups(ui.ctx());
        // Layout: look only at the height of the window's content area (points), not at the content, so it does
        // not flip back and forth while reading; no switching while dragging.
        let height = ui.ctx().content_rect().height();
        let resizing = self.resizing_until.is_some_and(|until| Instant::now() < until);
        let was = self.compact.unwrap_or(height < FULL_HEIGHT);
        let compact = next_compact(layout_pref(&self.settings.layout), was, height, resizing);
        self.compact = Some(compact);
        // Left navigation rail: it cannot be dragged wider (click-through only lets clicks pass where there are
        // no interactive widgets, and a drag handle would block them).
        egui::Panel::left("ascendant-rail")
            .exact_size(RAIL)
            .resizable(false)
            .show_separator_line(false)
            .frame(egui::Frame::new().fill(SIDEBAR).inner_margin(egui::Margin::symmetric(4, 6)))
            .show(ui, |ui| self.rail(ui));
        egui::CentralPanel::default().show(ui, |ui| {
            // Fixed skeleton (2026-11-05): the status bar on the top line is always there and only its content
            // changes, so the blocks below are not pushed around.
            // In the compact Tartarus board page it is merged into line 1 (see board_tab).
            // In the compact board page the status bar is merged into line 1: Tartarus draws it inside
            // board_tab; Vorax draws it here (header on the left, state on the right, remaining rerolls when
            // there is no state), and the Vorax panel no longer draws its own line 1.
            let merged = compact && self.tab == Tab::Board;
            if !merged {
                self.status_bar(ui, None);
            } else if self.vorax.showing() {
                let head = self.vorax.compact_head();
                self.status_bar(ui, Some((&head, "")));
            }
            let page_h = ui.available_height();
            // The window size is entirely up to the player and content that does not fit scrolls: previously
            // the board page grew and shrank the window automatically to fit the whole board, so the window
            // jumped whenever the screen changed (patrol, manage, Vorax) (the user chose no auto resizing on
            // 2026-11-05).
            let mut area = egui::ScrollArea::vertical();
            if let Some(y) = self.preview_scroll_y {
                area = area.vertical_scroll_offset(y);
            }
            let tab = self.tab;
            let scrolled = area.show(ui, |ui| {
                match self.tab {
                    Tab::Board if self.vorax.showing() => {
                        // the give-up hint's switch lives on the Vorax panel (the player's choice, 2026-11-09)
                        let mut give_up = self.settings.vorax_give_up_hint;
                        if self.vorax.panel(ui, compact, &mut give_up) {
                            self.settings.vorax_give_up_hint = give_up;
                            self.save();
                        }
                    }
                    Tab::Board => self.board_tab(ui, compact, page_h),
                    Tab::Run => self.review_section(ui),
                    Tab::Progress => self.progress_section(ui),
                    Tab::Affix => self.affix_tab(ui),
                    Tab::Settings => self.settings_section(ui),
                }
                // For testing (ASCENDANT_PREVIEW_SCROLL=bottom): scroll to the bottom.
                if self.preview_bottom {
                    ui.scroll_to_cursor(Some(Align::BOTTOM));
                }
            });
            self.wheel_area = wheel_area(tab, scrolled.content_size.y, scrolled.inner_rect);
            // For testing (ASCENDANT_PREVIEW_MEASURE=<file>): write the content height and visible height, to
            // prove the content fits and there is no scrollbar.
            if let Some(path) = &self.preview_measure {
                let prepare = self.review.report.prepare_top.map_or_else(|| "none".to_string(), |y| format!("{y:.1}"));
                let text = format!("compact={compact} content_h={:.1} view_h={:.1} page_h={page_h:.1} prepare_y={prepare}{}\n", scrolled.content_size.y, scrolled.inner_rect.height(), self.preview_marks);
                let _ = std::fs::write(path, text);
            }
        });
        self.edit_window(&ui.ctx().clone());
        self.restore_dialog(&ui.ctx().clone());
        self.tools_window(&ui.ctx().clone());
        self.whats_new_window(&ui.ctx().clone());
        self.health_window(&ui.ctx().clone());
        self.forget_closed_float();
        if std::mem::take(&mut self.pick_log) {
            self.pick_game_log(frame);
        }
        if std::mem::take(&mut self.pick_png) {
            self.open(frame);
        }
        if std::mem::take(&mut self.pick_verify) {
            self.start_verify(frame);
        }
    }
}

impl AscendantApp {
    /// Left navigation rail: the App icon at the very top, then the three tab buttons Board, Run and Settings
    /// (the selected one in the primary colour, the Board button with a small yellow dot for the warning
    /// count); at the very bottom, from the top, the click-through light, the game log light (see [`log_lamp`]), the
    /// Auto and the Read buttons, with a spinner above them while reading.
    fn rail(&mut self, ui: &mut egui::Ui) {
        let (mut read, mut auto, mut through) = (false, false, false);
        let size = Vec2::new(RAIL - 8.0, 24.0);
        ui.vertical_centered(|ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            let (rect, response) = ui.allocate_exact_size(Vec2::splat(RAIL - 8.0), Sense::hover());
            theme::paint_logo(ui.painter(), rect);
            response.on_hover_text("Ascendant");
            ui.add_space(4.0);
            for (tab, name, icon) in [(Tab::Board, tr("Board"), Icon::Board), (Tab::Run, tr("Run"), Icon::Run)] {
                let badge = if tab == Tab::Board { self.warned } else { 0 };
                if rail_button(ui, size, icon, self.tab == tab, badge).on_hover_text(name).clicked() {
                    self.set_tab(tab);
                }
            }
            // Progress: yellow background when nearly full, red background plus a red dot when full; the hover
            // tooltip has one line per item.
            let charges = self.charges();
            let progress = progress_button(ui, size, self.tab == Tab::Progress, worst_fill(&charges)).on_hover_text(progress_hover(&charges));
            if self.preview_tooltip_progress {
                egui::Tooltip::for_widget(&progress).show(|ui| ui.label(progress_hover(&charges)));
            }
            if progress.clicked() {
                self.set_tab(Tab::Progress);
            }
            if rail_button(ui, size, Icon::Affix, self.tab == Tab::Affix, 0).on_hover_text(tr("Affixes")).clicked() {
                self.set_tab(Tab::Affix);
            }
            // A newer release the hourly check found: only a red dot drawn over the button, which takes no room.
            let dot = self.update_dot();
            let hover = match self.dot_release() {
                Some(r) => format!("{}\n{}", tr("Settings"), trf!("Ascendant {} is available", r.version_text())),
                None => tr("Settings").to_string(),
            };
            let settings = rail_button(ui, size, Icon::Settings, self.tab == Tab::Settings, 0);
            if dot {
                ui.painter().circle_filled(settings.rect.right_top() + Vec2::new(-3.0, 4.0), 4.0, theme::ERROR);
            }
            if settings.on_hover_text(hover).clicked() {
                self.set_tab(Tab::Settings);
            }
        });
        ui.with_layout(Layout::bottom_up(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.y = 6.0;
            read = read_button(ui, size).on_hover_text(trf!("Read now ({})", self.settings.read_hotkey)).clicked();
            let on = self.settings.auto_read;
            let hover = if on {
                let mut hover = trf!("Auto read is on: the game is read when its screen changes. Click or {} to turn it off", self.settings.auto_hotkey);
                if let Some(w) = &self.watching {
                    hover.push_str(&trf!("\nWatching {}", w));
                }
                hover
            } else {
                trf!("Auto read is off. Click or {} to turn it on", self.settings.auto_hotkey)
            };
            auto = auto_button(ui, Vec2::new(size.x, 30.0), on).on_hover_text(hover).clicked();
            // The game log light: green, yellow, red, grey without a watcher; a click opens Settings.
            let status = self.log_status();
            if log_button(ui, Vec2::new(size.x, 30.0), log_lamp(status.as_ref())).on_hover_text(log_hover(status.as_ref())).clicked() {
                self.set_tab(Tab::Settings);
            }
            // The click-through light, right above the game log light: green while on, a click turns it on or off.
            // Its clicks stay in the window while click-through is on, as every widget's do (see `widget_rects`).
            let hover = if self.click_through {
                tr("Click-through is on: clicks on this window's empty parts go to the game behind it. Click here to turn it off")
            } else {
                tr("Click-through is off. Click here to turn it on: clicks on this window's empty parts then go to the game behind it, while its buttons and cells still take them")
            };
            through = through_button(ui, Vec2::new(size.x, 30.0), self.click_through).on_hover_text(hover).clicked();
            // The spinner's slot is always reserved, so the buttons do not jump when reading starts or ends.
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
            if self.session.scanning() || self.vorax.busy() {
                egui::Spinner::new().size(14.0).color(ACCENT).paint_at(ui, rect);
            }
        });
        if read {
            self.read_now();
        }
        if auto {
            self.set_auto_read(!self.settings.auto_read);
        }
        if through {
            self.set_click_through(!self.click_through);
        }
    }

    /// The Tartarus board page, with a fixed skeleton (2026-11-05: the user reported that every read made
    /// blocks appear or disappear and pushed what is below up and down). Patrol, picking instruments, picking
    /// treasure, interrogation, picking a cell, no board read yet and run over all share the same skeleton and
    /// only swap content: places with no content stay blank, and with no board the cell ring draws empty cells.
    ///
    /// Full layout from top to bottom: small header line (1 line), instruments and theme (1 line, a click opens
    /// the edit window), advice card (fixed height), top three (fixed 3 rows), cell ring, warning area (fixed 2
    /// lines, see [`theme::notes`]).
    ///
    /// Compact layout (`compact`, when the window is small, see [`next_compact`]): line 1 has the header on the
    /// left and the state on the right (replacing the status bar at the same time, see
    /// [`AscendantApp::status_bar`]), then instruments and theme, a one-line advice card (the objective
    /// progress is written here too), and the cell ring (cell width fills the space, height is computed from
    /// the space `page_h` leaves), with the top three placed in the courtyard in the middle of the ring, and
    /// the warning area.
    /// `page_h` is the height available to this page (the visible height of the scroll area); the compact
    /// layout uses it to make the whole page fit exactly.
    ///
    /// Advice card: large advice text, objective and expected loot, and the "Clearly better" / "Either is fine"
    /// label. Cell ring: the advised cell gets a primary-colour outline and other good choices get a small dot
    /// at the top-left corner. The advice itself is drawn directly every frame, without the flicker guard.
    fn board_tab(&mut self, ui: &mut egui::Ui, compact: bool, page_h: f32) {
        let top = ui.cursor().top();
        let notes_id = egui::Id::unique("ascendant-board-notes");
        // First only read the session to compute the header, then draw line 1 and the instruments line, which
        // need to modify self.
        // For testing (ASCENDANT_PREVIEW_RUN_OVER=1): on screen treat the run as over and the settlement as
        // luck, to see the card tooltip.
        let preview_over = self.preview_run_over.then(|| trf!("The run is over: {} Chromatic Prisoners vaulted.", 10));
        let head = {
            let s = &self.session;
            let reading = s.reading.as_ref().filter(|_| s.run_over.is_none() && preview_over.is_none());
            let mut head = reading.map_or_else(String::new, |r| header(r.model.day, r.days_left));
            // The compact layout has no courtyard text: "Last reading" is merged into the header.
            if compact && reading.is_some_and(|r| !r.kept_lines().is_empty()) {
                head = format!("{head} \u{b7} {}", tr("Last reading"));
            }
            head
        };
        if compact {
            self.status_bar(ui, Some((&head, "")));
        } else {
            theme::line(ui, &head, MUTED);
        }
        self.tools_row(ui);
        let s = &self.session;
        // Height of the compact cell ring: the height left on this page minus the warning area (2 lines of
        // small text plus spacing).
        // With no OCR language and no board read, the install button row goes under the notes: the ring leaves room for it.
        let ocr_row = self.ocr_problem.is_some() && s.reading.is_none() && s.run_over.is_none();
        let ring_h = |ui: &egui::Ui| {
            let notes = 2.0 * ui.text_style_height(&egui::TextStyle::Small) + 2.0 + 3.0 * ui.spacing().item_spacing.y + 6.0;
            let button = if ocr_row { ui.spacing().interact_size.y + ui.spacing().item_spacing.y } else { 0.0 };
            page_h - (ui.cursor().top() - top) - notes - button
        };
        let run_over = s.run_over.clone().or(preview_over);
        let Some(reading) = s.reading.as_ref().filter(|_| run_over.is_none()) else {
            // Run over or no board read yet: the skeleton is still drawn and the content is left blank.
            // The full run-over note (for the tooltip; the compact card only shows a short one).
            let mut over_note: Option<&str> = None;
            let (big, small, notes) = match &run_over {
                Some(text) => {
                    // The settlement of this run is on the Run tab; mention it first on the end screen.
                    // The full layout writes the whole sentence "Within the expected range: see the Run tab"; the
                    // compact card has only a short space on the right, so it writes "As expected ›" (the whole
                    // card is clickable and the tooltip says "Click to open the Run tab"), so it does not wrap
                    // and leave a single word.
                    let within = (tr("Within the expected range: see the Run tab"), tr("As expected \u{203a}"));
                    let note = match (&self.review.settling, &self.review.settled) {
                        _ if self.preview_run_over => Some(within),
                        _ if !self.over_reviewed => None,
                        (Some(_), _) => Some((tr("Reviewing the run: see the Run tab"), tr("Reviewing \u{203a}"))),
                        (None, Some((_, Outcome::Settled(r)))) => match r.cause {
                            Some(Cause::Within) => Some(within),
                            Some(Cause::Luck { high: true }) => Some((tr("Luck ran high this run: see the Run tab"), tr("Luck high \u{203a}"))),
                            Some(Cause::Luck { high: false }) => Some((tr("Luck ran low this run: see the Run tab"), tr("Luck low \u{203a}"))),
                            Some(Cause::Decisions) => Some((tr("Decisions lost points this run: see the Run tab"), tr("Decisions lost \u{203a}"))),
                            None => None,
                        },
                        _ => None,
                    };
                    over_note = note.map(|(full, _)| full);
                    let note = note.map(|(full, short)| if compact { short } else { full });
                    (Some(text.as_str()), note.unwrap_or_default(), Vec::new())
                }
                None => {
                    let mut notes = Vec::new();
                    if !s.scanning() {
                        notes.push((tr("No board read yet.").to_string(), MUTED));
                        if let Some(problem) = &self.ocr_problem {
                            notes.push((problem.clone(), theme::WARN));
                        }
                        if let Some(missed) = &s.missed {
                            notes.push((missed.clone(), MUTED));
                        }
                    }
                    (None, "", notes)
                }
            };
            // The "see the Run tab" note when the run is over: small text in the primary colour; the whole card
            // is clickable and a click switches to the Run tab.
            let link = run_over.is_some() && !small.is_empty();
            let small_colour = if link { ACCENT } else { MUTED };
            let (card, cut) = if compact { advice_line(ui, big, small, small_colour, None, false) } else { advice_card(ui, big, small, small_colour, None, false) };
            let mut go_run = false;
            if let Some(big) = big {
                let small_full = over_note.unwrap_or(small);
                let mut tip = if small_full.is_empty() { big.to_string() } else { format!("{big}\n{small_full}") };
                if link {
                    tip = format!("{tip}\n{}", tr("Click to open the Run tab"));
                }
                // Clickable only when some text is truncated or the Run tab can be switched to (so the mouse
                // can still hover and read the full text under click-through); otherwise only hover is used, so
                // the game's clicks are not blocked while click-through is on.
                let sense = if cut || link { Sense::click() } else { Sense::hover() };
                let r = ui.interact(card, egui::Id::unique("ascendant-courtyard"), sense);
                if self.preview_tooltip_card {
                    egui::Tooltip::for_widget(&r).show(|ui| ui.label(&tip));
                }
                go_run = r.on_hover_text(tip).clicked() && link;
            }
            if compact {
                ui.add_space(4.0);
                let fit = ring_h(ui);
                let court = empty_board(ui, Some(fit));
                self.moves(&mut court_ui(ui, court), None, false);
            } else {
                self.moves(ui, None, false);
                ui.add_space(6.0);
                empty_board(ui, None);
            }
            theme::notes(ui, notes_id, &[], &notes);
            // No OCR language: the install button (the game language's) under the line in the notes.
            if self.ocr_problem.is_some() && run_over.is_none() {
                self.ocr_install_ui(ui, &[OcrLang::of(lang::lang())]);
            }
            if go_run {
                self.set_tab(Tab::Run);
            }
            return;
        };
        let advice = s.shown_advice();
        let warden = reading.model.warden;
        let pick = advice.as_ref().and_then(|a| action_cell(&a.action, warden));
        // The good moves on a cell are marked on the ring.
        let near = advice.as_ref().map(|a| also_good(&s.ranking, a.option)).unwrap_or_default();
        let marks = Marks {
            pick,
            near: near.into_iter().filter_map(|k| action_cell(&s.ranking[k].action, warden)).filter(|&c| Some(c) != pick).collect(),
        };
        // Objective progress (`Undercurrents 1/2`), written on both the advice card and the courtyard.
        let goal = reading.model.objective.as_ref().and_then(|goal| {
            let o = data().objective(goal.id)?;
            let name = objective_name(goal.id)?;
            Some(if goal.progress >= o.target { trf!("{} \u{b7} done", name) } else { format!("{name} {}/{}", goal.progress, o.target) })
        });
        let mut option = advice.as_ref().and_then(|a| a.option);
        let mut pick = advice.as_ref().map(|a| move_name(&a.action, short(&a.label)));
        // For testing (ASCENDANT_PREVIEW_NOT_FIRST=1): only on screen replace the advice with the 2nd ranked
        // move, to see how the reason is worded.
        if self.preview_not_first && s.ranking.len() >= 2 {
            option = Some(1);
            pick = Some(move_name(&s.ranking[1].action, short(&s.ranking[1].label)));
        }
        // Whether there is advice (the test replacement counts too): with no advice no reason is written.
        let shown = if self.preview_not_first && s.ranking.len() >= 2 { Some(option) } else { advice.as_ref().map(|_| option) };
        let reason = advice_reason(&s.ranking, shown, s.solving, s.rounds, s.races());
        // A move with no value (not played out) writes a dash, so the line keeps its parts.
        let expected = option.filter(|&k| k < s.ranking.len()).map(|k| trf!("Expected loot {}", loot_text(s, Some(k)).unwrap_or_else(|| "\u{2014}".to_string())));
        // Small text: when the advice is not the 1st ranked, write the reason first, then the objective
        // progress and expected loot.
        let line: Vec<&str> = reason.into_iter().chain(goal.iter().map(String::as_str)).chain(expected.iter().map(String::as_str)).collect();
        let line = line.join(" \u{b7} ");
        let verdict = verdict(&s.ranking, option, s.solving);
        let (card, cut) = if compact { advice_line(ui, pick.as_deref(), &line, MUTED, verdict, s.solving) } else { advice_card(ui, pick.as_deref(), &line, MUTED, verdict, s.solving) };
        // The tooltip that used to hang on the courtyard (the advice's full name and reason, the note on the
        // reading) now hangs on the advice card; the small text may be truncated, so the full text is put in as
        // well. It is clickable only when some text is truncated (so it can be hovered under click-through
        // too); otherwise only hover is used and the game is not blocked.
        let mut hover: Vec<String> = Vec::new();
        if !line.is_empty() {
            hover.push(line.clone());
        }
        hover.extend(advice.iter().map(|a| format!("{}\n{}", a.label, a.why)));
        hover.extend(reading.warnings.iter().filter(|w| w.kind == WarningKind::Note).map(|w| w.text.clone()));
        if !hover.is_empty() {
            let r = ui.interact(card, egui::Id::unique("ascendant-courtyard"), if cut { Sense::click() } else { Sense::hover() });
            if self.preview_tooltip_card {
                egui::Tooltip::for_widget(&r).show(|ui| ui.label(hover.join("\n\n")));
            }
            r.on_hover_text(hover.join("\n\n"));
        }
        let warned: Vec<(usize, &str)> = reading.warnings.iter().filter_map(|w| Some((w.cell?, w.text.as_str()))).collect();
        let clicked = if compact {
            ui.add_space(4.0);
            let fit = ring_h(ui);
            let (clicked, court) = board(ui, &reading.model, &warned, &marks, Some(fit));
            // The courtyard holds the top three; while computing, a spinner at the top right (also when the
            // advice card already shows "Solving...").
            self.moves(&mut court_ui(ui, court), option, true);
            if s.solving {
                let r = Rect::from_min_size(court.right_top() + Vec2::new(-14.0, 2.0), Vec2::splat(12.0));
                egui::Spinner::new().size(12.0).color(ACCENT).paint_at(ui, r);
            }
            clicked
        } else {
            self.moves(ui, option, true);
            ui.add_space(6.0);
            let (clicked, court) = board(ui, &reading.model, &warned, &marks, None);
            let mut lines: Vec<String> = goal.into_iter().collect();
            if !reading.kept_lines().is_empty() {
                lines.push(tr("Last reading").to_string());
            }
            courtyard(ui, court, &lines, s.solving);
            clicked
        };
        // Warning area (formerly a few lines under the header and under the ring that appeared or disappeared
        // on every read), ordered by importance.
        let mut notes: Vec<(String, Color32)> = s.problems.iter().map(|p| (p.clone(), theme::ERROR)).collect();
        for w in reading.warnings.iter().filter(|w| w.cell.is_none() && w.kind != WarningKind::Note) {
            let text = match w.kind {
                WarningKind::HeldToolsUnknown => {
                    trf!("The {} panel shows icons the reader can't name: add them with the {}s line on the Board tab.", instrument_term(), instrument_term())
                }
                _ => w.text.clone(),
            };
            notes.push((text, theme::WARN));
        }
        // The advice itself (header, advice card, top three, marks on the cell ring) is drawn directly, without
        // Steady; in the warning area the chain order and the reason for the advice (why_shown, the line under
        // the advice) also belong to the advice and are shown immediately in `now`, so that when the advice
        // changes the reason does not stay on the previous one for 0.5 seconds. The rest are warnings and go
        // through the flicker guard.
        let mut now: Vec<(String, Color32)> = s.chain_line().map(|line| (line, theme::WARN)).into_iter().collect();
        if let Some(a) = &advice {
            now.push((s.why_shown(a), MUTED));
        }
        theme::notes(ui, notes_id, &now, &notes);
        if clicked.is_some() {
            self.editing = clicked;
        }
    }

    /// Current values of the accumulated progress: with auto read on, the capture thread reads them from the
    /// game log (`Watcher::charges`, made by another session); with auto read off there is no Watcher and both
    /// are None. Test fake values take precedence.
    fn charges(&self) -> gamelog::Charges {
        self.preview_charges.unwrap_or_else(|| self.watcher.as_ref().map(|w| w.charges()).unwrap_or_default())
    }

    /// Progress page (spec settled by the user): a header line (when an item is full it says "Vorax full:
    /// playing more maps adds nothing", otherwise blank so nothing below jumps), then one card per item (name,
    /// value/limit, a thin progress bar, runs left N; yellow when nearly full, red outline and bar when full;
    /// "—" when there is no value), and at the bottom a line of small text explaining where the numbers come
    /// from. All of it is display only (hover).
    fn progress_section(&mut self, ui: &mut egui::Ui) {
        let charges = self.charges();
        let full: Vec<&str> = CHARGE_ITEMS.iter().filter(|i| (i.value)(&charges).is_some_and(|v| gamelog::fill(v) == Fill::Full)).map(|i| tr(i.name)).collect();
        theme::heading(ui, tr("Progress"));
        let head = if full.is_empty() { String::new() } else { trf!("{} full: playing more maps adds nothing", join_names(&full)) };
        theme::line(ui, &head, theme::ERROR);
        ui.add_space(2.0);
        for item in &CHARGE_ITEMS {
            let value = (item.value)(&charges);
            let fill = value.map(gamelog::fill);
            let (edge, bar) = match fill {
                Some(Fill::Full) => (theme::ERROR, theme::ERROR),
                Some(Fill::Near) => (theme::WARN, theme::WARN),
                _ => (DIVIDER, ACCENT),
            };
            theme::group_frame().stroke(Stroke::new(if fill == Some(Fill::Full) { 1.5 } else { 1.0 }, edge)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = 4.0;
                ui.horizontal(|ui| {
                    let name = RichText::new(tr(item.name)).strong().color(if fill == Some(Fill::Near) { theme::WARN } else { egui::Color32::WHITE });
                    ui.label(name);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let text = value.map_or_else(|| "\u{2014}".to_string(), |v| format!("{v} / {}", item.max));
                        ui.label(RichText::new(text).color(if fill == Some(Fill::Full) { theme::ERROR } else { theme::TEXT }));
                    });
                });
                let share = value.map_or(0.0, |v| v as f32 / item.max as f32);
                theme::bar(ui, ui.available_width(), share, if value.is_some() { bar } else { theme::TRACK });
                let runs = value.map_or_else(|| "\u{2014}".to_string(), |v| trf!("{} runs left ({} per run)", v / item.per_run, item.per_run));
                theme::line(ui, &runs, MUTED);
            });
            ui.add_space(4.0);
        }
        theme::line(ui, tr("From the game's log; shown once auto read is on and the game is running"), MUTED);
    }

    /// Instruments and theme (Tartarus board page, in both the full and the compact layout): a fixed line
    /// `Theme: None   Instruments: A · B`, truncated when too long with the full text in the hover tooltip. The
    /// line is clickable (Sense::click); a click toggles the edit window (see [`AscendantApp::tools_window`]),
    /// which floats on top and does not push the layout.
    fn tools_row(&mut self, ui: &mut egui::Ui) {
        let names: Vec<String> = self.session.tools.iter().map(|t| instrument(*t).to_string()).collect();
        let list = if names.is_empty() { tr("None").to_string() } else { names.join(" \u{b7} ") };
        let theme_text = theme_name(self.session.theme).unwrap_or(tr("None"));
        // The theme goes first: it is one short name, while a long instrument list (English names run to 30
        // characters) pushed it past the edge, where only the tooltip showed it.
        let text = trf!("Theme: {}   {}s: {}", theme_text, instrument_term(), list);
        if theme::click_line(ui, &text, MUTED).clicked() {
            self.floats.toggle_tools();
        }
    }

    /// Shows `which` as a window of its own (see [`Float`]), titled `title`, its page drawn by `body` and scrolled when
    /// longer than the window; gives whether it stays open. Its "x" closes it, and with `esc` so does Esc pressed while
    /// it has the keyboard (not while a text box or a hotkey recording has the keys, see [`esc_closes`]); the caller
    /// still checks Esc pressed in the main window, as before. It opens where the player last left it while that is on
    /// a monitor, else over the main window where it floated before; a move is saved once the window stops moving, or
    /// as it closes. Fonts, style and the text scale are the main window's (one egui context); the system's scale is
    /// the monitor's it is on. When the main window closes, the program ends and this window with it.
    fn float_viewport(&mut self, ctx: &egui::Context, which: Float, title: &str, esc: bool, mut body: impl FnMut(&mut Self, &mut egui::Ui)) -> bool {
        if self.floats.placing.as_ref().is_none_or(|p| p.which != which) {
            let ppp = physical_per_point(ctx);
            let saved = self.settings.float_pos.get(which.key()).copied().filter(|&[x, y]| on_screen(x + 20, y + 20));
            let at = match saved {
                Some([x, y]) => Some(Pos2::new(x as f32, y as f32) / ppp),
                None => ctx.input(|i| i.viewport().outer_rect).map(|main| float_default_at(main, which.size())),
            };
            self.floats.placing = Some(Placing { which, at, last: None, unsaved: false, opened: Instant::now() });
        } else if !self.settings.float_pos.contains_key(which.key())
            && let Some(main) = ctx.input(|i| i.viewport().outer_rect)
            && let Some(p) = self.floats.placing.as_mut()
        {
            // One the player has never placed stays over the main window as that moves (it is placed at start, by
            // `place`, after the first frames): the builder's new position moves it, which is no move of the player's.
            let at = float_default_at(main, which.size());
            if p.at.is_none_or(|was| (was - at).length() > 1.0) {
                (p.at, p.last, p.opened) = (Some(at), None, Instant::now());
            }
        }
        let mut builder = egui::ViewportBuilder::default()
            .with_title(title)
            .with_inner_size(which.size())
            .with_resizable(false)
            .with_minimize_button(false)
            .with_maximize_button(false)
            .with_always_on_top()
            .with_taskbar(false);
        if let Some(at) = self.floats.placing.as_ref().and_then(|p| p.at) {
            builder = builder.with_position(at);
        }
        if let Some(icon) = float_icon() {
            builder = builder.with_icon(icon);
        }
        let recording = self.recording.is_some();
        let (close, seen) = ctx.show_viewport_immediate(which.id(), builder, |ui, _| {
            let busy = ui.ctx().text_edit_focused() || recording;
            let close = ui.input(|i| i.viewport().close_requested()) || (esc && esc_closes(ui.input(|i| i.key_pressed(egui::Key::Escape)), busy));
            egui::CentralPanel::default().show(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| body(self, ui));
            });
            let ppp = physical_per_point(ui.ctx());
            let seen = ui.input(|i| i.viewport().outer_rect.filter(|_| i.viewport().minimized != Some(true)));
            (close, seen.map(|r| [(r.min.x * ppp).round() as i32, (r.min.y * ppp).round() as i32]))
        });
        let mut save = false;
        if let Some(p) = self.floats.placing.as_mut().filter(|p| p.which == which) {
            if let Some(now) = seen {
                // Windows can still settle the new window in its first frames: that is no move of the player's.
                if float_moved(p.last, now) && p.opened.elapsed() >= FLOAT_SETTLES {
                    self.settings.float_pos.insert(which.key().to_string(), now);
                    p.unsaved = true;
                } else if p.unsaved {
                    (p.unsaved, save) = (false, true);
                }
                p.last = Some(now);
            }
            if close && p.unsaved {
                (p.unsaved, save) = (false, true);
            }
            // A window at rest draws no frame: one more comes to see the move has stopped, and save it.
            if p.unsaved {
                ctx.request_repaint_after(Duration::from_millis(250));
            }
        }
        if save {
            self.save();
        }
        !close
    }

    /// With no small window open, its placing is dropped (a move not saved yet, which closing it saves, is saved here
    /// too: the window can close by a page switch or by another opening), so the next one opens afresh.
    fn forget_closed_float(&mut self) {
        let open = match self.floats.placing.as_ref().map(|p| p.which) {
            Some(Float::WhatsNew) => self.floats.whats_new.is_some(),
            Some(Float::Health) => self.floats.health,
            Some(Float::Tools) => self.floats.tools,
            None => return,
        };
        if !open && let Some(p) = self.floats.placing.take()
            && p.unsaved
        {
            self.save();
        }
    }

    /// The edit window for instruments and theme (formerly a feature of the Run tab, behaviour unchanged:
    /// set_tools, set_theme), a window of its own (see [`Float`]). Esc does not close it, as before.
    fn tools_window(&mut self, ctx: &egui::Context) {
        if !self.floats.tools {
            return;
        }
        let title = trf!("{}s and run theme", instrument_term());
        if !self.float_viewport(ctx, Float::Tools, &title, false, |app, ui| app.tools_editor(ui)) {
            self.floats.tools = false;
        }
    }

    /// Top three (2026-11-05, user: bars show no meaning, so write only numbers): rank, option name, expected
    /// loot (2 decimals), and for ranks 2 and 3 one more column with the gap to the first (for example
    /// `−0.12`). A gap within the simulation error (|gap| ≤ 2×gap_se, the same test as the advice card label,
    /// see [`within_error`]) is grey, meaning about the same; beyond it the normal text colour is used.
    /// The advised row uses the primary colour; when the advice ranks below 3rd it goes in the 3rd row. With
    /// fewer than 3 rows, blank rows fill the space so the layout does not jump.
    /// Since 1.0.4 (user, both languages) a row names its move by an action icon plus the cell number and a small
    /// copy of the board's tile, or the icon plus a short text (see [`move_row`]), each part with its own tooltip.
    fn moves(&self, ui: &mut egui::Ui, advised: Option<usize>, rated: bool) {
        const ROWS: usize = 3;
        let s = &self.session;
        ui.add_space(4.0);
        let rows = if rated { s.ranking.len().min(ROWS) } else { 0 };
        let mut shown: Vec<usize> = (0..rows).collect();
        if let Some(a) = advised.filter(|&a| rated && a >= ROWS && a < s.ranking.len())
            && let Some(last) = shown.get_mut(ROWS - 1)
        {
            *last = a;
        }
        let top = s.ranking.first().map_or(0.0, |o| o.mean);
        // Use a narrower spacing between columns, so that at a window width of 200 the name column can still
        // fit `去 04 號` ("Go to 04").
        let spacing = 4.0;
        ui.spacing_mut().item_spacing.x = spacing;
        let h = ui.spacing().interact_size.y;
        // The number columns use the width they actually need (the wider of the value and the column heading)
        // and the rest goes to the name column.
        let width_of = |text: &str, style: egui::TextStyle| ui.painter().layout_no_wrap(text.to_string(), style.resolve(ui.style()), Color32::WHITE).size().x;
        let num_w = shown.iter().map(|&k| width_of(&loot_text(s, Some(k)).unwrap_or_default(), egui::TextStyle::Body)).fold(width_of("0.00", egui::TextStyle::Body), f32::max);
        let gap_w = shown.iter().map(|&k| width_of(&format!("\u{2212}{:.2}", (s.ranking[k].mean - top).abs()), egui::TextStyle::Small)).fold(width_of(tr("Gap"), egui::TextStyle::Small), f32::max);
        let loot_head_w = width_of(tr("Expected loot"), egui::TextStyle::Small);
        let head_w = loot_head_w + spacing + width_of(tr("Gap"), egui::TextStyle::Small);
        // The cell number column of a row (see below): as wide as two digits, so the small tiles line up.
        let cell_no_w = width_of("00", egui::TextStyle::Body) + 2.0;
        // Fixed widths, so the number and the small tile line up in every row and nothing moves between frames.
        let icon_w = (h * 0.85).round();
        let tile = Vec2::new(((h - 2.0) * 1.4).round(), h - 2.0);
        // Within the window: in the compact layout the courtyard runs past its right edge when the window is too
        // narrow for the ring (font size "Extra large" at a width of 200).
        let width = ui.available_width().min(ui.clip_rect().right() - ui.cursor().min.x).max(0.0);
        let cols = Cols::fit(width, spacing, Need { num: num_w + 2.0, gap: gap_w + 2.0, head: head_w, loot: loot_head_w, cell: icon_w + 3.0 + cell_no_w, icon: icon_w });
        // What of the move fits the name column, the same in every row so the rows look alike: the small tile goes
        // first, then the cell number; the icon always stays. A dropped part's tooltip is the row's.
        let show_no = cols.label >= icon_w + 3.0 + cell_no_w;
        let show_tile = cols.label >= icon_w + 3.0 + cell_no_w + 3.0 + tile.x;
        // A text move (no cell) writes its text only when at least a letter and the ellipsis fit.
        let text_min = width_of("0\u{2026}", egui::TextStyle::Body);
        let loot_tip = tr("Playing this move, the Chromatics vaulted by the end on average over thousands of simulated runs (an Ultimate counts 0.2, a Legendary 0.05)");
        let gap_tip = tr("How far behind the first; grey means within the simulation error, so either is about the same");
        // Column headings: `前三名` ("Top moves") on the left, and "Expected loot" and "Gap" on the right (each
        // aligned with its column below).
        ui.horizontal(|ui| {
            let head = |text: &str| RichText::new(text).small().color(MUTED);
            ui.allocate_ui_with_layout(Vec2::new(cols.rank + spacing + cols.label, h * 0.8), Layout::left_to_right(Align::Center), |ui| {
                ui.set_min_width(cols.rank + spacing + cols.label);
                ui.add(egui::Label::new(head(tr("Top moves"))).truncate());
            });
            let right = if cols.gap > 0.0 { cols.num + spacing + cols.gap } else { cols.num };
            ui.allocate_ui_with_layout(Vec2::new(right, h * 0.8), Layout::right_to_left(Align::Center), |ui| {
                if cols.gap > 0.0 {
                    ui.label(head(tr("Gap"))).on_hover_text(gap_tip);
                }
                // Elided only when the window is too narrow for it (see `Cols::fit`).
                let loot = ui.add(egui::Label::new(head(tr("Expected loot"))).truncate()).on_hover_text(loot_tip);
                // For testing (ASCENDANT_PREVIEW_TOOLTIP=1): always show this long tooltip, to capture how it
                // wraps and where it sits.
                if self.preview_tooltip {
                    egui::Tooltip::for_widget(&loot).show(|ui| ui.label(loot_tip));
                }
            });
        });
        let blanks = shown.len()..ROWS;
        // Each row names its move as an action icon and the cell it acts on (number and a small copy of the board's
        // tile), or the icon and a short text for a move without a cell (see [`move_row`]): "Go to cell 11 (Magic
        // Mammoth)" was cut to "Go to cell..." in English at a width of 421, hiding the number.
        let model = s.reading.as_ref().map(|r| &r.model);
        let warden = model.map_or(-1, |m| m.warden);
        for (row, k) in shown.into_iter().enumerate() {
            let o = &s.ranking[k];
            let chosen = advised == Some(k);
            let gap = o.mean - top;
            let (icon, tail) = move_row(&o.action, &o.label, warden);
            let label = RichText::new(match &tail {
                MoveTail::Cell(n) => cell_label(*n),
                MoveTail::Text(text) => text.clone(),
            });
            let number = RichText::new(loot_text(s, Some(k)).unwrap_or_else(|| "\u{2014}".to_string()));
            // The first row (the first rank itself) writes no gap. The gap is written with its sign: the order
            // is "candidates still running first, then by mean" (solve.rs), so an eliminated row or one ranked
            // later may have a higher mean than the first; a gap that rounds to 0 is written `±0.00`.
            let diff = (row > 0 && k > 0).then(|| {
                let text = if gap.abs() < 0.005 {
                    "\u{b1}0.00".to_string()
                } else if gap > 0.0 {
                    format!("+{gap:.2}")
                } else {
                    format!("\u{2212}{:.2}", -gap)
                };
                RichText::new(text).small()
            });
            let (label, number, diff) = if chosen {
                (label.strong().color(ACCENT), number.color(ACCENT), diff.map(|d| d.color(ACCENT)))
            } else {
                let tone = if within_error(gap, o.gap_se) { MUTED } else { theme::TEXT };
                (label.color(theme::TEXT), number.color(theme::TEXT), diff.map(|d| d.color(tone)))
            };
            ui.horizontal(|ui| {
                ui.add_sized([cols.rank, h], egui::Label::new(RichText::new((k + 1).to_string()).color(MUTED)));
                ui.allocate_ui_with_layout(Vec2::new(cols.label, h), Layout::left_to_right(Align::Center), |ui| {
                    ui.set_min_width(cols.label);
                    ui.spacing_mut().item_spacing.x = 3.0;
                    let colour = if chosen { ACCENT } else { theme::TEXT };
                    // Three hover areas side by side, none overlapping: the icon says what the action is, the
                    // small tile what the cell holds, and the rest of the column the full move as before.
                    let (rect, icon_r) = ui.allocate_exact_size(Vec2::new(icon_w, h), Sense::hover());
                    paint_move_icon(ui.painter(), rect, icon, colour);
                    let icon_tip = icon.meaning();
                    if row == 0 && self.preview_tooltip_move == Some(MoveTip::Icon) {
                        egui::Tooltip::for_widget(&icon_r).show(|ui| ui.label(&icon_tip));
                    }
                    icon_r.on_hover_text(icon_tip);
                    let text_r = match tail {
                        MoveTail::Cell(_) if !show_no => None,
                        MoveTail::Cell(n) => {
                            let r = ui.add_sized([cell_no_w, h], egui::Label::new(label).truncate().show_tooltip_when_elided(false));
                            if let Some(cell) = model.and_then(|m| m.cells.get(n)).filter(|_| show_tile) {
                                let (rect, cell_r) = ui.allocate_exact_size(Vec2::new(tile.x, h), Sense::hover());
                                let at = Rect::from_center_size(rect.center(), tile);
                                tile_shape(ui.painter(), at, 3.0, cell, n, cell_r.hovered());
                                let cell_tip = cell_tip(n, cell);
                                if row == 0 && self.preview_tooltip_move == Some(MoveTip::Cell) {
                                    egui::Tooltip::for_widget(&cell_r).show(|ui| ui.label(&cell_tip));
                                }
                                cell_r.on_hover_text(cell_tip);
                            }
                            Some(r)
                        }
                        MoveTail::Text(_) if ui.available_width() < text_min => None,
                        MoveTail::Text(_) => Some(ui.add(egui::Label::new(label).truncate().show_tooltip_when_elided(false))),
                    };
                    if let Some(text_r) = text_r {
                        if row == 0 && self.preview_tooltip_move == Some(MoveTip::Row) {
                            egui::Tooltip::for_widget(&text_r).show(|ui| ui.label(&o.label));
                        }
                        text_r.on_hover_text(&o.label);
                    }
                    // The rest of the column, up to the expected loot.
                    let rest = ui.available_width();
                    if rest > 0.0 {
                        ui.allocate_exact_size(Vec2::new(rest, h), Sense::hover()).1.on_hover_text(&o.label);
                    }
                });
                ui.allocate_ui_with_layout(Vec2::new(cols.num, h), Layout::right_to_left(Align::Center), |ui| {
                    ui.label(number).on_hover_text(loot_tip);
                });
                if cols.gap > 0.0 {
                    ui.allocate_ui_with_layout(Vec2::new(cols.gap, h), Layout::right_to_left(Align::Center), |ui| {
                        if let Some(diff) = diff {
                            ui.label(diff).on_hover_text(gap_tip);
                        }
                    });
                }
            });
        }
        for _ in blanks {
            ui.horizontal(|ui| {
                ui.add_sized([cols.rank, h], egui::Label::new(" "));
            });
        }
    }

    /// Upper half of the Run tab: the settlement of the previous run (see [`super::settle`]). Two number cards,
    /// "Vaulted" and "Expected", the settlement card (mostly luck or decisions, decision loss, how many standard
    /// deviations off; the status when there is no result yet), and the "Largest step". All are display only
    /// (hover) and click-through is not affected; the only button is "Review again" when the language does not
    /// match. Above them the run picker: "Latest run", or a finished run to review instead (see [`RunReview`]).
    /// While a run settles, the block of cards keeps the height of what was on show before ([`RunReview::block_min`]):
    /// the largest step is gone meanwhile, and the export card below must not jump up and back.
    fn review_section(&mut self, ui: &mut egui::Ui) {
        theme::heading(ui, tr("Run review"));
        self.run_picker(ui);
        let width = ui.available_width();
        let min_h = self.review.block_min(width);
        let block = ui.vertical(|ui| {
            ui.set_min_height(min_h);
            self.review_block(ui)
        });
        self.review.block_drawn(width, block.response.rect.height());
        if let Some(dir) = block.inner {
            // Settled again the way it was: the picked run as a pick, else the game's own automatic settlement.
            if !self.review.again(&dir) {
                match self.review.kind() {
                    RunKind::Tartarus => self.start_settle(dir, None),
                    RunKind::Vorax => self.start_vorax_settle(dir, None, None),
                }
            }
        }
        // Borrowed, not cloned: the review and the install id are copied only when an export starts.
        self.review.show_export(ui, &self.settings.install_id, self.current.as_deref());
    }

    /// The number cards, the settlement card and the largest step of the run on show; Some(folder) when "Review
    /// again" was pressed.
    fn review_block(&self, ui: &mut egui::Ui) -> Option<PathBuf> {
        let kind = self.review.kind();
        let (settling, settled_out) = self.review.shown();
        let settled = match (settling, settled_out) {
            (false, Some((_, Outcome::Settled(s)))) => Some(s.as_ref()),
            _ => None,
        };
        stat_cards(ui, settled, kind);
        ui.add_space(4.0);
        let mut again = None;
        theme::card_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 2.0;
            let small = |ui: &mut egui::Ui, text: &str| {
                ui.add(egui::Label::new(RichText::new(text).small().color(MUTED)).wrap());
            };
            if settling {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new().size(14.0).color(ACCENT));
                    ui.label(RichText::new(tr("Reviewing the run...")).strong());
                });
                small(ui, tr("Re-solving in the background at low priority; the advice is not held up."));
                return;
            }
            match settled_out {
                None if !self.settings.run_log => {
                    ui.label(RichText::new(tr("Shown after the run ends")).color(MUTED));
                    small(ui, tr("Turn on Keep a run log in Settings: the review reads the run's log."));
                }
                None => {
                    ui.label(RichText::new(tr("Shown after the run ends")).color(MUTED));
                    small(ui, tr("Each move of the run is re-solved to split luck from decisions."));
                }
                Some((_, Outcome::Settled(s))) => {
                    verdict_lines(ui, s);
                    riot_row(ui, s);
                }
                Some((_, Outcome::Unfinished)) => {
                    ui.label(RichText::new(tr("The last run was not finished")).color(theme::WARN));
                    small(ui, tr("Its log has no run end, so it is not reviewed."));
                }
                Some((dir, Outcome::Language { log, ui: now })) => {
                    ui.label(RichText::new(tr("Not reviewed: another language")).color(theme::WARN));
                    small(ui, &trf!("The run was advised in {}, the window is in {}: switch the language in Settings, then review again.", log.native_name(), now.native_name()));
                    if ui.small_button(tr("Review again")).clicked() {
                        again = Some(dir.clone());
                    }
                }
                Some((_, Outcome::Empty)) => {
                    ui.label(RichText::new(tr("Nothing to review")).color(MUTED));
                    small(ui, tr("The last run has no advice to re-solve."));
                }
                Some((_, Outcome::Failed(e))) => {
                    ui.label(RichText::new(trf!("Review failed: {}", e)).small().color(theme::ERROR));
                }
            }
        });
        if let Some(s) = settled {
            largest_step(ui, s);
        }
        again
    }

    /// The Run tab's run picker, for the review and the export card below: "Latest run" (the game that ended last) and
    /// the finished runs (read in the background; until the list is in, only "Latest run").
    fn run_picker(&mut self, ui: &mut egui::Ui) {
        let latest = tr("Latest run");
        let was = self.review.picked().map(|e| e.dir.clone());
        // The closed picker shows the picked run without its size, so it stays one line at the narrowest window like
        // "Latest run" (no jump); the open list shows each run with its size.
        // Should it still not fit (a larger text size), it is cut short with "..." rather than wrapped: never a jump.
        let shown = self.review.picked().map_or_else(|| latest.to_string(), RunEntry::title);
        let hover = match self.review.picked() {
            Some(e) => format!("{}\n{}", e.label(), tr("Which run to review and export: Latest run follows each game as it ends, or pick a finished run")),
            None => tr("Which run to review and export: Latest run follows each game as it ends, or pick a finished run").to_string(),
        };
        let mut chosen = was.clone();
        let runs = self.review.report.runs(ui.ctx());
        egui::ComboBox::from_id_salt("review-run")
            .width(ui.available_width())
            .selected_text(shown)
            .truncate()
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut chosen, None, latest);
                for r in runs {
                    ui.selectable_value(&mut chosen, Some(r.dir.clone()), r.label());
                }
            })
            .response
            .on_hover_text(hover);
        if chosen == was {
            return;
        }
        let entry = chosen.and_then(|d| runs.iter().find(|r| r.dir == d).cloned());
        match entry {
            Some(entry) => self.review_run(entry),
            None => self.review.latest(),
        }
    }

    /// Takes the OCR install thread's outcome; once installed the OCR is checked again (it mostly needs a restart).
    fn poll_ocr_install(&mut self) {
        let Some((lang, rx)) = &self.ocr_installing else { return };
        match rx.try_recv() {
            Ok(out) => {
                self.ocr_result = Some((*lang, out));
                self.ocr_installing = None;
                if !self.preview_no_ocr {
                    self.ocr_problem = ocr::problem();
                }
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.ocr_result = Some((*lang, OcrInstall::Failed(tr("the install thread stopped").to_string())));
                self.ocr_installing = None;
            }
        }
    }

    /// The OCR install buttons (one per `langs`) and the installing, done or failed lines; on a failure or a No, the command to run by hand.
    fn ocr_install_ui(&mut self, ui: &mut egui::Ui, langs: &[OcrLang]) {
        let name = |l: OcrLang| match l {
            OcrLang::ZhTw => tr("Install Traditional Chinese OCR"),
            OcrLang::EnUs => tr("Install English OCR"),
        };
        let small = |ui: &mut egui::Ui, text: &str, colour: Color32| {
            ui.add(egui::Label::new(RichText::new(text).small().color(colour)).wrap());
        };
        if let Some((l, _)) = &self.ocr_installing {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(12.0).color(ACCENT));
                ui.label(RichText::new(trf!("Installing: {}...", name(*l))).small());
            });
            small(ui, tr("Windows asks for administrator permission first; the download can take a few minutes."), MUTED);
            return;
        }
        let mut start = None;
        ui.horizontal_wrapped(|ui| {
            for &l in langs {
                let hover = trf!("Runs {} in PowerShell as administrator; Windows asks you first", install::ocr_manual_command(l));
                if ui.button(RichText::new(name(l)).small()).on_hover_text(hover).clicked() {
                    start = Some(l);
                }
            }
        });
        match &self.ocr_result {
            Some((_, OcrInstall::Done { restart })) => {
                let text = if *restart { tr("OCR installed: restart Windows, then reopen Ascendant.") } else { tr("OCR installed: reopen Ascendant to use it.") };
                small(ui, text, theme::GOOD);
            }
            Some((l, OcrInstall::Declined)) => {
                small(ui, tr("Not installed: the administrator permission was declined."), theme::WARN);
                small(ui, &trf!("Or run it yourself in PowerShell as administrator: {}", install::ocr_manual_command(*l)), MUTED);
            }
            Some((l, OcrInstall::Failed(e))) => {
                small(ui, &trf!("Install failed: {}", e), theme::ERROR);
                small(ui, &trf!("Or run it yourself in PowerShell as administrator: {}", install::ocr_manual_command(*l)), MUTED);
            }
            None => {}
        }
        if let Some(l) = start {
            let ctx = ui.ctx().clone();
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(install::install_ocr(l));
                ctx.request_repaint();
            });
            self.ocr_installing = Some((l, rx));
            self.ocr_result = None;
        }
    }

    /// Held instruments, a list to add one, and the run theme.
    /// The instruments and theme editor (since 2026-11-05 in the popup window of the "Instruments and theme"
    /// line on the board page; the Run tab keeps only the settlement), behaviour unchanged.
    fn tools_editor(&mut self, ui: &mut egui::Ui) {
        ui.set_width(ui.available_width().min(220.0));
        let d = data();
        let held = self.session.tools.clone();
        let (mut remove, mut add) = (None, None);
        theme::heading(ui, &trf!("{}s", instrument_term()));
        theme::group_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            if !held.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = Vec2::new(4.0, 4.0);
                    for (k, t) in held.iter().enumerate() {
                        if ui.button(RichText::new(format!("{} \u{d7}", instrument(*t))).small()).on_hover_text(tr("Remove")).clicked() {
                            remove = Some(k);
                        }
                    }
                });
            }
            egui::ComboBox::from_id_salt("add-instrument").width(ui.available_width().min(160.0)).selected_text(tr("Add...")).show_ui(ui, |ui| {
                let mut offer: Vec<Tool> = Tool::ALL.iter().copied().filter(|t| !held.contains(t)).collect();
                offer.sort_by_key(|t| instrument(*t));
                for t in offer {
                    if ui.selectable_label(false, instrument(t)).clicked() {
                        add = Some(t);
                    }
                }
            });
        });
        if let Some(k) = remove {
            let mut tools = held;
            tools.remove(k);
            self.set_tools(tools);
        } else if let Some(t) = add {
            let mut tools = held;
            tools.push(t);
            self.set_tools(tools);
        }
        let theme = self.session.theme;
        let name = |id: Option<u32>| theme_name(id).unwrap_or(tr("None"));
        let mut picked = theme;
        theme::heading(ui, tr("Run theme"));
        theme::group_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ComboBox::from_id_salt("run-theme").width(ui.available_width().min(160.0)).selected_text(name(theme)).show_ui(ui, |ui| {
                for id in std::iter::once(None).chain(d.themes.iter().map(|t| Some(t.id))) {
                    ui.selectable_value(&mut picked, id, name(id));
                }
            });
        });
        ui.add_space(2.0);
        ui.add(egui::Label::new(RichText::new(tr("Both are read from the game; set them here if a read missed them.")).small().color(MUTED)).wrap());
        if picked != theme {
            self.set_theme(picked);
        }
    }

    /// Settings page, by what each setting is about: "Game" (what Ascendant reads: language, game window, game log, OCR),
    /// "Advice and controls" (the mark on the game, the hotkeys, starting as administrator), "Window", "Logs and
    /// reports", "Updates", "Advanced" (playouts, install, restore defaults) and "About" at the very bottom. Checkboxes
    /// become switches, hotkeys are shown as a key-cap label plus a "Change" button; each setting's own saving,
    /// recomputation and hover text are unchanged by where it sits. A run is exported from the Run tab only.
    fn settings_section(&mut self, ui: &mut egui::Ui) {
        let mut open = false;
        let top = ui.cursor().top();
        theme::heading(ui, tr("Game"));
        theme::group_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            row(ui, "Language / 語言", |ui| {
                // Named in both languages, so a player who can't read the one on show finds it.
                let mut picked = lang::lang();
                // The language cannot be switched while a settlement is running: when audit compares the advice
                // it temporarily changes the global language and then changes it back, and a switch in the
                // middle would be overwritten.
                // An export's run review re-solve counts too (see report::settle_running).
                let settling = self.review.busy();
                ui.add_enabled_ui(!settling, |ui| {
                    let _ = egui::ComboBox::from_id_salt("language")
                        .width(ui.available_width().min(110.0))
                        .selected_text(picked.native_name())
                        .show_ui(ui, |ui| {
                            for l in Lang::ALL {
                                ui.selectable_value(&mut picked, l, l.native_name());
                            }
                        })
                        .response
                        .on_hover_text(tr("The game client's language: Ascendant reads the game and writes this window in it"))
                        .on_disabled_hover_text(tr("Reviewing the last run: the language can be changed when it is done"));
                });
                if picked != lang::lang() && !settling {
                    self.set_language(picked);
                }
            });
            row(ui, tr("Game window"), |ui| {
                let r = ui
                    .add(egui::TextEdit::singleline(&mut self.settings.window_title).desired_width(ui.available_width().min(110.0)))
                    .on_hover_text(tr("Used when no torchlight_infinite.exe window is found: a part of the game window's title"));
                if r.lost_focus() {
                    self.watch();
                    self.save();
                }
            });
            self.game_log_row(ui);
            // OCR languages: both can be installed, Traditional Chinese (the main one) first.
            self.ocr_install_ui(ui, &[OcrLang::ZhTw, OcrLang::EnUs]);
        });
        theme::heading(ui, tr("Advice and controls"));
        let controls = theme::group_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            let mut highlight = self.settings.highlight;
            let hover = tr("Marks on the game the cell, card or button to click for the advised move. Clicks go through it to the game");
            if ui.add(Switch::new(&mut highlight, tr("Highlight where to click"))).on_hover_text(hover).changed() {
                self.settings.highlight = highlight;
                self.save();
            }
            let mut value = self.settings.show_value;
            let hover = tr("Shows the advised move's expected value just above where to click on the game (Tartarus: expected loot, Vorax: expected final activity), in small text. Clicks go through it to the game");
            if ui.add(Switch::new(&mut value, tr("Show the expected value on the game"))).on_hover_text(hover).changed() {
                self.settings.show_value = value;
                self.save();
            }
            // Width of the wider of "Change" and "Cancel": the row's button is one or the other, and the label column
            // keeps its width either way.
            let button = [tr("Change"), tr("Cancel")]
                .map(|t| egui::WidgetText::from(RichText::new(t).small()).into_galley(ui, None, f32::INFINITY, egui::TextStyle::Small).size().x)
                .into_iter()
                .fold(0.0, f32::max);
            let (mut start, mut cancel, mut outside) = (None, false, false);
            for (which, label) in [(Which::Read, tr("Read key")), (Which::Auto, tr("Auto read key"))] {
                // The right side is left for the button and a key-cap label at least KEYCAP_MIN wide, so on a narrow
                // window the start of the key name (such as `Ctrl+…`) is still visible.
                let keep = button + ui.spacing().button_padding.x * 2.0 + ui.spacing().item_spacing.x + KEYCAP_MIN;
                // While recording, the label column narrows to the label itself, so the key cap has room for why a
                // key was refused; only widths change, the row's height does not.
                let keep = if self.recording.as_ref().is_some_and(|r| r.which == which) {
                    let label_w = egui::WidgetText::from(label).into_galley(ui, None, f32::INFINITY, egui::TextStyle::Body).size().x + 4.0;
                    keep.max(ui.available_width() - label_w - ui.spacing().item_spacing.x)
                } else {
                    keep
                };
                row_with(ui, label, keep, |ui| {
                    let hint = tr("Click, then press the new key");
                    // Recording: the same button and key cap, so the row keeps its height; the cap asks for the key, or
                    // says why the last one was refused.
                    if let Some(r) = self.recording.as_ref().filter(|r| r.which == which) {
                        let b = ui.small_button(tr("Cancel")).on_hover_text(tr("Press the new key, or Esc to cancel"));
                        let (text, full, colour) = match &r.refused {
                            Some(why) => (why.short.as_str(), why.full.as_str(), theme::WARN),
                            None => (tr("Press a key..."), tr("Press a key..."), ACCENT),
                        };
                        let cap = theme::keycap_prompt(ui, text, colour).on_hover_text(format!("{full}\n{}", tr("Press the new key, or Esc to cancel")));
                        cancel |= b.clicked();
                        // A press anywhere else in the window cancels (the other row's "Change" then starts that row).
                        let area = b.rect.union(cap.rect);
                        outside |= ui.input(|i| i.pointer.any_pressed() && i.pointer.interact_pos().is_some_and(|p| !area.contains(p)));
                    } else {
                        if ui.small_button(tr("Change")).on_hover_text(hint).clicked() {
                            start = Some(which);
                        }
                        // On a narrow window the key name is truncated: the hover tooltip shows the full key.
                        let key = &self.keys[which as usize].text;
                        theme::keycap(ui, key).on_hover_text(format!("{key}\n{hint}"));
                    }
                });
            }
            if cancel || outside {
                self.stop_recording();
            }
            if let Some(which) = start {
                self.start_recording(which);
            }
            // Always shown, so nothing moves; the status bar's notice tells when it is needed.
            let mut admin = self.settings.start_as_admin;
            let hover = trf!("Windows asks for confirmation at every start. Only needed when the game runs as administrator and you need {} and {}", self.settings.auto_hotkey, self.settings.read_hotkey);
            if ui.add(Switch::new(&mut admin, tr("Start as administrator"))).on_hover_text(hover).changed() {
                self.settings.start_as_admin = admin;
                self.save();
            }
        });
        if self.preview_hotkeys {
            controls.response.scroll_to_me(Some(Align::TOP));
        }
        if self.preview_measure.is_some() {
            let (r, at) = (controls.response.rect, ui.cursor().top());
            self.preview_marks = format!(" marks: controls_group_y={:.1}..{:.1} after_controls_y={:.1}", r.top() - top, r.bottom() - top, at - top);
        }
        theme::heading(ui, tr("Window"));
        theme::group_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            row(ui, tr("Layout"), |ui| {
                let current = layout_pref(&self.settings.layout);
                let mut picked = current;
                let _ = egui::ComboBox::from_id_salt("layout")
                    .width(ui.available_width().min(110.0))
                    .selected_text(tr(current.name()))
                    .show_ui(ui, |ui| {
                        for pref in [LayoutPref::Auto, LayoutPref::Full, LayoutPref::Compact] {
                            ui.selectable_value(&mut picked, pref, tr(pref.name()));
                        }
                    })
                    .response
                    .on_hover_text(tr("Auto: the compact layout when the window is short. Full or Compact: always that layout"));
                if picked != current {
                    self.settings.layout = picked.key().to_string();
                    self.save();
                }
            });
            row(ui, tr("Text size"), |ui| {
                let current = text_scale(self.settings.font_scale);
                let mut picked = current;
                let name = |scale: f32| TEXT_SCALES.iter().find(|&&(s, _)| s == scale).map_or("", |&(_, n)| tr(n));
                let _ = egui::ComboBox::from_id_salt("text-size")
                    .width(ui.available_width().min(110.0))
                    .selected_text(name(current))
                    .show_ui(ui, |ui| {
                        for (scale, n) in TEXT_SCALES {
                            ui.selectable_value(&mut picked, scale, tr(n));
                        }
                    })
                    .response
                    .on_hover_text(tr("Makes the text, the cells and the bars of this window larger"));
                if picked != current {
                    self.settings.font_scale = picked;
                    self.set_text_scale(&ui.ctx().clone(), picked);
                    self.save();
                }
            });
            row(ui, tr("Opacity"), |ui| {
                // Leave width for the "100%" on the right; on a narrow window the slider shrinks accordingly.
                ui.spacing_mut().slider_width = (ui.available_width() - 44.0).clamp(24.0, 120.0);
                let slider = egui::Slider::new(&mut self.settings.opacity, settings::MIN_OPACITY..=1.0).custom_formatter(|v, _| format!("{:.0}%", v * 100.0));
                let r = ui.add(slider);
                if r.drag_stopped() || (r.changed() && !r.dragged()) {
                    self.save();
                }
            });
            let mut place = self.settings.auto_place;
            let hover = tr("Move this window under the game's instrument panel when the game is first found, until you move it yourself; Ascendant then opens where you left it. Turning this on forgets that place");
            if ui.add(Switch::new(&mut place, tr("Place by the game panels"))).on_hover_text(hover).changed() {
                self.settings.auto_place = place;
                if place {
                    self.settings.window_pos = None;
                }
                self.save();
            }
        });
        theme::heading(ui, tr("Logs and reports"));
        theme::group_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            let mut log = self.settings.run_log;
            let runs = settings::app_dir().map_or_else(String::new, |d| d.join("runs").display().to_string());
            let hover = trf!("Keep each run's reads, advice and corrections, with the frames read, in {}", runs);
            if ui.add(Switch::new(&mut log, tr("Keep a run log"))).on_hover_text(hover).changed() {
                self.settings.run_log = log;
                self.session.log = log.then(RunLog::in_appdata).flatten();
                if let Some(w) = &self.watcher {
                    w.observe(self.settings.observe && log);
                }
                self.save();
            }
            let mut keep_all = self.settings.keep_all_logs;
            let hover = tr("Off: Tartarus and Vorax each keep only their newest 10 finished games (with screenshots); older logs are deleted automatically. On: every log and screenshot is kept and never deleted automatically (except empty folders from opening and closing Ascendant without any read), so watch your free disk space");
            if ui.add(Switch::new(&mut keep_all, tr("Keep all run logs"))).on_hover_text(hover).changed() {
                self.settings.keep_all_logs = keep_all;
                super::runlog::set_keep_all(keep_all);
                self.save();
            }
            let mut observe = self.settings.observe;
            let hover = tr("Also keeps about 25 game frames a second in each run's folder while the prison shows (about 250 MB a run), to review how a run looked as it was played");
            if ui.add_enabled(self.settings.run_log, Switch::new(&mut observe, tr("Observe mode"))).on_hover_text(hover).changed() {
                self.settings.observe = observe;
                if let Some(w) = &self.watcher {
                    w.observe(observe && self.settings.run_log);
                }
                self.save();
            }
            open = ui.button(tr("Open screenshot...")).clicked();
            if !self.path.is_empty() {
                ui.add(egui::Label::new(RichText::new(&self.path).small().color(MUTED)).wrap());
            }
            // When upload is off (`share::ENABLED`) its option is not shown, so that whoever gets this exe does not
            // think ticking it does something.
            if share::ENABLED {
                let mut share = self.settings.share_logs == Some(true);
                let hover = tr("Sends each finished run's log to warden.arcasti.dev to improve Ascendant: no screenshots, no window titles");
                let r = ui.add_enabled(self.settings.run_log, Switch::new(&mut share, tr("Share run logs"))).on_hover_text(hover).on_disabled_hover_text(hover);
                if r.changed() {
                    self.set_sharing(share);
                }
                if let Some(e) = &self.upload_error {
                    ui.small(RichText::new(trf!("Run log upload failed: {}", e)).color(MUTED));
                }
            }
        });
        let install = !self.installed && self.current.is_some();
        if update::ENABLED {
            self.update_section(ui);
        }
        // The "Advanced" group always has "Restore defaults", so it is always shown.
        theme::heading(ui, tr("Advanced"));
        theme::group_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            row(ui, tr("Playouts"), |ui| {
                let r = ui.add(egui::DragValue::new(&mut self.settings.rollouts).range(50..=5000).speed(10)).on_hover_text(tr("Per option, for each solve"));
                if r.drag_stopped() || (r.changed() && !r.dragged()) {
                    self.session.rollouts = self.settings.rollouts;
                    self.session.solve();
                    self.save();
                }
            });
            if install {
                let hover = if update::ENABLED { "Start menu and desktop shortcuts and updates, for this Windows user" } else { "Start menu and desktop shortcuts, for this Windows user" };
                if ui.button(tr("Install")).on_hover_text(tr(hover)).clicked() {
                    self.install(ui.ctx());
                }
            }
            if ui.button(tr("Restore defaults")).on_hover_text(tr("Asks first. The language and the install are kept; saved run logs are not deleted")).clicked() {
                self.restoring = true;
            }
        });
        theme::heading(ui, tr("About"));
        theme::group_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::hover());
                theme::paint_logo(ui.painter(), rect);
                ui.vertical(|ui| {
                    ui.label(RichText::new("Ascendant").strong().size(15.0));
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(trf!("Version {}", env!("CARGO_PKG_VERSION"))).small().color(MUTED));
                        // The verified notes kept for this version and the earlier ones, only when there are some:
                        // the button shows them in the "What's new" window, no network needed (no tooltip: the
                        // window has them).
                        if !self.notes_history.is_empty() && ui.add(egui::Button::new(RichText::new(tr("What's new")).small()).small()).clicked() {
                            self.open_whats_new(false);
                        }
                    });
                });
            });
            // The release key's fingerprint (the first 32 hex digits, 128 bits, of its SHA-256), to compare with the one
            // published in SECURITY.md and the README. Showing it proves nothing by itself: updates and "Verify downloaded
            // files..." rely on the signature check with the built-in key, not on this text.
            let fingerprint = update::key_fingerprint(&update::RELEASE_PUBLIC_KEY);
            let key = if update::has_release_key() { trf!("Release key fingerprint: {}", fingerprint) } else { trf!("Release key fingerprint: {} (placeholder: this build cannot update)", fingerprint) };
            theme::fine_print(ui, &key, MUTED);
            ui.add(egui::Label::new(RichText::new(tr("Unofficial decision tool for Torchlight: Infinite")).small()).wrap());
            // The two licence notices must be kept.
            theme::fine_print(ui, tr("Based on arcasti/tli-warden (GPL-3.0)"), MUTED);
            theme::fine_print(ui, if lang::zh() { vorax_view::CREDIT } else { vorax_view::CREDIT_EN }, MUTED);
        });
        if open {
            self.pick_png = true;
        }
    }

    /// The settings page's "Game log" row: "Choose..." (the Windows file dialog, see
    /// [`AscendantApp::pick_game_log`]) and "Restore automatic", and under them two fixed lines: the status in the
    /// light's colour (red adds its reminder) and the path, truncated when too long with the full text on hover; a
    /// red light then adds its reminder and the note [`LOG_NOTE`] as wrapped lines.
    fn game_log_row(&mut self, ui: &mut egui::Ui) {
        let mut automatic = false;
        row(ui, tr("Game log"), |ui| {
            let manual = self.settings.game_log.is_some();
            automatic = ui
                .add_enabled(manual, egui::Button::new(RichText::new(tr("Restore automatic")).small()).small())
                .on_hover_text(tr("Find UE_game.log by itself: the game's folder, then Local app data, then Steam"))
                .clicked();
            if ui.add(egui::Button::new(RichText::new(tr("Choose...")).small()).small()).on_hover_text(tr("Choose UE_game.log yourself (in the game folder: UE_game\\TorchLight\\Saved\\Logs)")).clicked() {
                self.pick_log = true;
            }
        });
        let status = self.log_status();
        let lamp = log_lamp(status.as_ref());
        let line = log_line(status.as_ref());
        match log_alert(status.as_ref()) {
            Some(alert) => theme::line_full(ui, &line, &format!("{line}\n{alert}\n{}", tr(LOG_NOTE)), lamp.colour()),
            None => theme::line(ui, &line, lamp.text_colour()),
        };
        let path = log_path(status.as_ref()).or_else(|| self.settings.game_log.clone()).map_or_else(String::new, |p| p.display().to_string());
        theme::line(ui, &path, MUTED);
        // A red light's reminder and note show on the page itself, wrapped, not only on hover.
        if let Some(alert) = log_alert(status.as_ref()) {
            ui.add(egui::Label::new(RichText::new(alert).small().color(lamp.colour())).wrap());
            ui.add(egui::Label::new(RichText::new(tr(LOG_NOTE)).small().color(MUTED)).wrap());
        }
        if automatic {
            self.set_game_log(None);
        }
    }

    /// The settings page's "Updates" group: the check-at-start switch, "Check now" and the check's answer, a newer
    /// release (shown only once its signature verified) with its notes and the "Update" button, the update's step
    /// while it runs, why it failed, and "Verify downloaded files..." with its answer. Nothing is downloaded without
    /// the player's "Update".
    fn update_section(&mut self, ui: &mut egui::Ui) {
        let (mut check_now, mut update_now, mut verify_now, mut show_notes) = (false, false, false, false);
        theme::heading(ui, tr("Updates"));
        let group = theme::group_frame().show(ui, |ui| {
            ui.set_width(ui.available_width());
            let mut check = self.settings.check_updates;
            let hover = tr("Asks api.github.com for the latest release when Ascendant starts and every hour; nothing else is sent");
            if ui.add(Switch::new(&mut check, tr("Check for updates at start"))).on_hover_text(hover).changed() {
                self.settings.check_updates = check;
                // Turned on after a start without a check: the hourly check starts an hour from now.
                if check && self.next_check.is_none() && self.checking.is_none() {
                    self.next_check = Some(update::next_check(Instant::now(), None));
                }
                self.save();
            }
            let busy = self.checking.is_some() || self.updating.is_some();
            // The hourly check runs unseen: the last answer's line stays as it is meanwhile.
            let shown_checking = self.checking.is_some() && !self.check_quiet;
            ui.horizontal(|ui| {
                if ui.add_enabled(!busy, egui::Button::new(RichText::new(tr("Check now")).small()).small()).clicked() {
                    check_now = true;
                }
                if shown_checking {
                    ui.add(egui::Spinner::new().size(12.0));
                    ui.small(RichText::new(tr("Checking...")).color(MUTED));
                } else if self.checked && self.release.is_none() && self.check_error.is_none() && self.update_error.is_none() {
                    ui.small(RichText::new(trf!("Up to date ({})", env!("CARGO_PKG_VERSION"))).color(theme::GOOD));
                }
            });
            if let Some(p) = self.check_error.as_ref().filter(|_| !shown_checking) {
                let colour = if matches!(p, update::Problem::Unverified(_)) { theme::ERROR } else { MUTED };
                problem_label(ui, &p.headline(true), p.detail(), colour);
            }
            if let Some(r) = &self.release {
                ui.label(RichText::new(trf!("New version {}", r.version_text())).strong().color(ACCENT));
                if !r.notes.is_empty() {
                    notes_label(ui, &r.notes);
                }
                ui.horizontal(|ui| {
                    // Off for good after a failure that moved the exe (see `locks_update`): its text says what to do.
                    // Off during a check too (see `start_update`).
                    let enabled = self.updating.is_none() && self.checking.is_none() && self.current.is_some() && !self.update_locked;
                    if ui.add_enabled(enabled, egui::Button::new(RichText::new(tr("Update")).small()).small()).on_hover_text(tr("Downloads the new version, checks its signature and SHA-256, then restarts Ascendant")).clicked() {
                        update_now = true;
                    }
                    // The "What's new" window: this release's verified notes, marked not installed yet, above the
                    // history; off when there are none to show.
                    let enabled = !r.notes.is_empty() || !self.notes_history.is_empty();
                    if ui.add_enabled(enabled, egui::Button::new(RichText::new(tr("What's new")).small()).small()).clicked() {
                        show_notes = true;
                    }
                });
                if let Some(step) = self.update_step() {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(12.0));
                        ui.small(RichText::new(tr(step.text())).color(MUTED));
                    });
                }
            }
            if let Some(e) = &self.update_error {
                let colour = if matches!(e, update::Failed::Problem(update::Problem::Network(_))) { MUTED } else { theme::ERROR };
                problem_label(ui, &e.text(), e.detail(), colour);
            }
            ui.horizontal(|ui| {
                let hover = tr("Checks Ascendant.exe in a folder against the SHA256SUMS.txt and SHA256SUMS.txt.sig beside it, with the release key built into this Ascendant");
                if ui.add_enabled(self.verifying.is_none(), egui::Button::new(RichText::new(tr("Verify downloaded files...")).small()).small()).on_hover_text(hover).clicked() {
                    verify_now = true;
                }
                if self.verifying.is_some() {
                    ui.add(egui::Spinner::new().size(12.0));
                }
            });
            match &self.verified {
                Some(Ok(v)) => {
                    // With warnings nothing is shown in the success colour: the files verified, but the warnings
                    // must be dealt with before running the exe.
                    let colour = if v.warnings.is_empty() { theme::GOOD } else { theme::WARN };
                    ui.add(egui::Label::new(RichText::new(&v.line).small().color(colour)).wrap());
                    if !v.others.is_empty() {
                        let others = trf!("Also matches the signed list: {}.", v.others.join(", "));
                        ui.add(egui::Label::new(RichText::new(others).small().color(colour)).wrap());
                    }
                    for warning in &v.warnings {
                        ui.add(egui::Label::new(RichText::new(warning).small().color(theme::WARN)).wrap());
                    }
                }
                Some(Err(p)) => {
                    let colour = if matches!(p, update::Problem::Files(_)) { MUTED } else { theme::ERROR };
                    problem_label(ui, &p.headline(false), p.detail(), colour);
                }
                None => {}
            }
            theme::fine_print(ui, tr("Checking for updates only connects to GitHub and sends no logs. An update is installed only when its signature checks out."), MUTED);
        });
        if self.preview_update_scroll {
            group.response.scroll_to_me(Some(Align::TOP));
        }
        if check_now {
            self.start_check(false);
        }
        if update_now {
            self.start_update();
        }
        if verify_now {
            self.pick_verify = true;
        }
        if show_notes {
            self.open_whats_new(true);
        }
    }

    /// The "What's new" window: the release notes of every version in [`AscendantApp::notes_history`] (built into the
    /// exe), newest first, each in a section under its version that folds. Opened from a newer release on offer, that
    /// release's own verified notes (the signed RELEASE-NOTES.txt the check verified) come first, marked not installed
    /// yet, and only they start unfolded: the running version's notes, already read, start folded with the older ones
    /// so the new ones are not pushed down. Otherwise the running version (the latest update) starts unfolded and the
    /// older ones folded. Nothing comes from the GitHub page. A window of its own of fixed size (see [`Float`]), so the
    /// main window's layout does not move; the text wraps, can be selected and copied, and scrolls when it is taller
    /// than the window. Its "x" or Esc (in it or in the main window) closes it.
    fn whats_new_window(&mut self, ctx: &egui::Context) {
        let Some(w) = &self.floats.whats_new else { return };
        let opened = w.opened;
        let pending = w.pending.clone().map(|p| (p, true));
        let kept = self.notes_history.iter().filter(|n| w.pending.as_ref().is_none_or(|p| p.version != n.version)).map(|n| (n.clone(), false));
        let shown: Vec<(update::SavedNotes, bool)> = pending.into_iter().chain(kept).collect();
        let unfolded = unfolded_notes(&shown.iter().map(|(n, pending)| (n.version.as_str(), *pending)).collect::<Vec<_>>(), env!("CARGO_PKG_VERSION"));
        let open = self.float_viewport(ctx, Float::WhatsNew, tr("What's new"), true, |app, ui| {
                ui.set_width(ui.available_width());
                // Only a build with the real release key can say so: a build from source without it verified nothing.
                if update::has_release_key() {
                    theme::fine_print(ui, tr("Verified with Ascendant's release key"), MUTED);
                    ui.add_space(2.0);
                }
                for (k, (n, pending)) in shown.iter().enumerate() {
                    let head = if *pending { trf!("{} (not installed yet)", n.version) } else { n.version.clone() };
                    egui::CollapsingHeader::new(RichText::new(head).strong().color(if *pending { ACCENT } else { theme::TEXT }))
                        .id_salt(("whats-new", opened, &n.version, *pending))
                        .default_open(Some(k) == unfolded)
                        .show(ui, |ui| {
                            ui.add(egui::Label::new(RichText::new(notes_in(&n.notes, lang::lang().key())).small()).wrap().selectable(true));
                        });
                }
                // For testing (ASCENDANT_PREVIEW_SCROLL=bottom): the window's list scrolled to its end too.
                if app.preview_bottom {
                    ui.scroll_to_cursor(Some(Align::BOTTOM));
                }
            });
        // Esc pressed in the main window closes it too, as before it was a window of its own.
        if !open || esc_closes(ctx.input(|i| i.key_pressed(egui::Key::Escape)), self.esc_busy) {
            self.floats.whats_new = None;
        }
    }

    /// The run check window (2026-11-08, the user: the status bar's line showed the issues only on hover): the run's
    /// folder name (its start, UTC), the summary and every issue, as health.txt has them, in text that wraps and can be
    /// selected and copied. A window of its own (see [`Float`]), one at a time with the others (see [`Floats`]); it
    /// scrolls when taller than the window, and its "x" or Esc (in it or in the main window) closes it (not while a text
    /// box or a hotkey recording has the keys, see [`esc_closes`]). Closed when there is no run check to show.
    fn health_window(&mut self, ctx: &egui::Context) {
        if !self.floats.health {
            return;
        }
        let Some(report) = self.session.health.clone() else {
            self.floats.health = false;
            return;
        };
        let folder = self.session.health_dir.as_ref().and_then(|d| d.file_name()).map(|n| n.to_string_lossy().into_owned());
        let open = self.float_viewport(ctx, Float::Health, tr("Run check"), true, |_, ui| {
                ui.set_width(ui.available_width());
                if let Some(folder) = &folder {
                    theme::fine_print(ui, &trf!("Run {}", folder), MUTED);
                    ui.add_space(2.0);
                }
                let colour = if report.issues.is_empty() { theme::GOOD } else { theme::WARN };
                ui.add(egui::Label::new(RichText::new(report.window_summary()).color(colour)).wrap().selectable(true));
                for issue in &report.issues {
                    ui.add(egui::Label::new(RichText::new(format!("- {issue}")).small()).wrap().selectable(true));
                }
            });
        // Esc pressed in the main window closes it too, as before it was a window of its own.
        if !open || esc_closes(ctx.input(|i| i.key_pressed(egui::Key::Escape)), self.esc_busy) {
            self.floats.health = false;
        }
    }

    /// The correction window of the cell clicked on the board.
    fn edit_window(&mut self, ctx: &egui::Context) {
        let Some(cell) = self.editing else { return };
        let (mut open, mut edit, mut marker) = (true, None, None);
        let marked = self.session.reading.as_ref().and_then(|r| r.model.cells.get(usize::from(cell) - 1)).is_some_and(|c| c.event().is_some());
        let warnings: Vec<String> = self.session.reading.iter().flat_map(|r| &r.warnings).filter(|w| w.cell == Some(usize::from(cell) - 1)).map(|w| w.text.clone()).collect();
        let (max_w, max_h) = popup_limits(ctx);
        egui::Window::new(trf!("Cell {}", format!("{cell:02}")))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .max_width(max_w)
            .max_height(max_h)
            .scroll([true, true])
            .constrain(true)
            .anchor(Align2::CENTER_TOP, [RAIL / 2.0, 8.0])
            .show(ctx, |ui| {
                for text in &warnings {
                    ui.label(RichText::new(text).small().color(theme::WARN));
                }
                ui.horizontal(|ui| {
                    if ui.small_button(tr("Locked")).clicked() {
                        edit = Some(CellEdit::Locked);
                    }
                    if ui.small_button(tr("Empty")).clicked() {
                        edit = Some(CellEdit::Empty);
                    }
                    let mut on = marked;
                    if ui.checkbox(&mut on, trf!("{} marker", incident_term())).changed() {
                        marker = Some(on);
                    }
                });
                egui::Grid::new("edit-cell").spacing([2.0, 2.0]).show(ui, |ui| {
                    ui.label("");
                    for tier in 0..RAINBOW {
                        ui.label(RichText::new(tier_short(tier)).size(11.0).color(tier_colour(tier)));
                    }
                    ui.end_row();
                    for beast in Beast::ALL {
                        ui.label(RichText::new(beast_name(beast)).small());
                        for tier in 0..RAINBOW {
                            let text = RichText::new(initial(beast)).strong().size(11.0).color(Color32::WHITE);
                            let button = egui::Button::new(text).fill(tier_colour(tier).gamma_multiply(0.45)).min_size(Vec2::new(24.0, 18.0));
                            let name = inmate_name(tier, beast);
                            if ui.add(button).on_hover_text(name).clicked() {
                                edit = Some(CellEdit::Inmate { beast, tier: i64::from(tier) });
                            }
                        }
                        ui.end_row();
                    }
                });
            });
        let done = match (edit, marker) {
            (Some(edit), _) => Some(self.session.edit_cell(cell, edit)),
            (None, Some(on)) => Some(self.session.set_marker(cell, on)),
            (None, None) => None,
        };
        if let Some(result) = done {
            if let Err(e) = result {
                self.error = Some(e);
            }
            self.editing = None;
        } else if !open {
            self.editing = None;
        }
    }
}

impl AscendantApp {
    /// Test entry points (offline screenshots need not simulate clicks); they take effect only when the
    /// environment variable is set, and behaviour is unchanged when it is not:
    /// ASCENDANT_PREVIEW_TAB=board/run/settings picks the starting tab; ASCENDANT_PREVIEW_SCROLL=bottom keeps
    /// the tab scrolled to the very bottom; ASCENDANT_PREVIEW_RESTORE=1 opens the "Restore defaults"
    /// confirmation window right away; ASCENDANT_PREVIEW_TOOLS=1 opens the instruments and theme edit window
    /// right away; ASCENDANT_PREVIEW_CHARGES injects fake accumulated progress (see [`preview_charges`]);
    /// ASCENDANT_PREVIEW_TAB also accepts progress.
    fn preview_entry(&mut self) {
        match std::env::var("ASCENDANT_PREVIEW_TAB").as_deref() {
            Ok("board") => self.tab = Tab::Board,
            Ok("run") => self.tab = Tab::Run,
            Ok("progress") => self.tab = Tab::Progress,
            Ok("affixes") => self.tab = Tab::Affix,
            Ok("settings") => self.tab = Tab::Settings,
            _ => {}
        }
        self.preview_bottom = std::env::var("ASCENDANT_PREVIEW_SCROLL").is_ok_and(|v| v == "bottom");
        self.preview_hotkeys = std::env::var("ASCENDANT_PREVIEW_SCROLL").is_ok_and(|v| v == "hotkeys");
        self.preview_scroll_y = std::env::var("ASCENDANT_PREVIEW_SCROLL").ok().and_then(|v| v.parse::<f32>().ok());
        self.preview_measure = std::env::var_os("ASCENDANT_PREVIEW_MEASURE").map(PathBuf::from);
        self.preview_tooltip = std::env::var("ASCENDANT_PREVIEW_TOOLTIP").is_ok_and(|v| v == "1");
        self.preview_tooltip_progress = std::env::var("ASCENDANT_PREVIEW_TOOLTIP").is_ok_and(|v| v == "progress");
        self.preview_tooltip_move = match std::env::var("ASCENDANT_PREVIEW_TOOLTIP").as_deref() {
            Ok("icon") => Some(MoveTip::Icon),
            Ok("cell") => Some(MoveTip::Cell),
            Ok("row") => Some(MoveTip::Row),
            _ => None,
        };
        // ASCENDANT_PREVIEW_MOVES=patrol, manage or pick: a made-up Tartarus board on that screen, solved as a read one
        // is, to capture the top three without the game (auto read off). Any other value, empty or "0" included, does
        // nothing.
        if let Ok(which) = std::env::var("ASCENDANT_PREVIEW_MOVES").map(|v| v.trim().to_string())
            && matches!(which.as_str(), "patrol" | "manage" | "pick")
        {
            self.session.reading = Some(preview_reading(&which));
            self.session.solve();
        }
        self.preview_not_first = std::env::var("ASCENDANT_PREVIEW_NOT_FIRST").is_ok_and(|v| v == "1");
        self.preview_run_over = std::env::var("ASCENDANT_PREVIEW_RUN_OVER").is_ok_and(|v| v == "1");
        // ASCENDANT_PREVIEW_RECORD=read or auto: that hotkey row recording ("Press a key..."); with :same or :typing
        // added, as after the other hotkey's key or a plain letter was pressed (the refusal in the row).
        if let Ok(v) = std::env::var("ASCENDANT_PREVIEW_RECORD") {
            let (row, then) = v.split_once(':').unwrap_or((v.as_str(), ""));
            let which = if row == "auto" { Which::Auto } else { Which::Read };
            let other = if which == Which::Read { Which::Auto } else { Which::Read };
            self.preview_recording = true;
            self.start_recording(which);
            let other_text = other.setting(&mut self.settings).clone();
            let got = match then {
                "same" => Some(Recorded::Key(other_text.clone())),
                "typing" => Some(Recorded::Typing("A".into())),
                _ => None,
            };
            if let Some(Judged::Refuse(why)) = got.map(|g| judge(g, (&other_text, other))) {
                self.refuse_key(why);
            }
        }
        // ASCENDANT_PREVIEW_LOG=following, searching, closed, stale, missing, manual-missing or off: that game log
        // status instead of the watcher's (for the settings page, the light and the status bar).
        if let Ok(v) = std::env::var("ASCENDANT_PREVIEW_LOG") {
            self.preview_log = Some(preview_log_status(&v));
        }
        // ASCENDANT_PREVIEW_VORAX=<frame png>: go straight into Vorax mode and read this frame (the game is not
        // captured), to measure the real program's Vorax panel; ASCENDANT_PREVIEW_BATTLE=2 or full: the panel
        // is drawn with a fake endgame battle (wave 2/3, or all rewards taken).
        if let Some(frame) = std::env::var_os("ASCENDANT_PREVIEW_VORAX").and_then(|p| load_png(Path::new(&p)).ok()) {
            self.vorax.preview_read(frame);
        }
        if let Ok(which) = std::env::var("ASCENDANT_PREVIEW_BATTLE") {
            self.vorax.preview_battle(&which);
        }
        // ASCENDANT_PREVIEW_VORAX_RESULT=battle or none: the Board tab shows the result card of a made-up finished
        // Vorax game (with or without an endgame battle).
        if let Ok(which) = std::env::var("ASCENDANT_PREVIEW_VORAX_RESULT") {
            self.vorax.preview_result(&which);
            self.vorax_ended = self.vorax.last_game().map(|g| g.ended_at);
        }
        // ASCENDANT_PREVIEW_VORAX_REVIEW=<vorax game folder>: the Run tab settles that Vorax game (as one just ended).
        if let Some(dir) = std::env::var_os("ASCENDANT_PREVIEW_VORAX_REVIEW").map(PathBuf::from) {
            self.start_vorax_settle(dir, None, Some(super::vorax_session::now_ms()));
            self.tab = Tab::Run;
        }
        // ASCENDANT_PREVIEW_PICK=<run folder>: the Run tab's run picker has that run picked (Tartarus, or Vorax for a
        // folder under vorax\), settling it as a pick from the list does; with ASCENDANT_PREVIEW_PICK_AT=<seconds>,
        // picked that long after start.
        if let Some(dir) = std::env::var_os("ASCENDANT_PREVIEW_PICK").map(PathBuf::from).filter(|d| d.join("log.jsonl").is_file()) {
            match std::env::var("ASCENDANT_PREVIEW_PICK_AT").ok().and_then(|v| v.parse::<f32>().ok()) {
                Some(secs) => self.preview_pick = Some((Instant::now() + Duration::from_secs_f32(secs.max(0.0)), dir)),
                None => self.review_run(RunEntry::of(dir)),
            }
        }
        // ASCENDANT_PREVIEW_SWITCH=<board|run|progress|settings>@<seconds>: that page that long after start.
        if let Some((page, secs)) = std::env::var("ASCENDANT_PREVIEW_SWITCH").ok().and_then(|v| v.split_once('@').map(|(p, s)| (p.to_string(), s.to_string()))) {
            let tab = match page.as_str() {
                "board" => Some(Tab::Board),
                "run" => Some(Tab::Run),
                "progress" => Some(Tab::Progress),
                "settings" => Some(Tab::Settings),
                _ => None,
            };
            if let (Some(tab), Ok(secs)) = (tab, secs.parse::<f32>()) {
                self.preview_switch = Some((Instant::now() + Duration::from_secs_f32(secs.max(0.0)), tab));
            }
        }
        // For testing (ASCENDANT_PREVIEW_TOOLTIP=card): always show the advice card tooltip, see board_tab.
        self.preview_tooltip_card = std::env::var("ASCENDANT_PREVIEW_TOOLTIP").is_ok_and(|v| v == "card");
        self.preview_charges = std::env::var("ASCENDANT_PREVIEW_CHARGES").ok().map(|v| preview_charges(&v));
        // ASCENDANT_PREVIEW_ELEVATED=1: as if the game ran as administrator (the status bar's notice); =declined: as after
        // the player declined Windows' prompt of "Restart as administrator"; =0: as if the game did not, whatever runs.
        if let Ok(v) = std::env::var("ASCENDANT_PREVIEW_ELEVATED") {
            self.preview_elevated = Some(v != "0");
            self.relaunch_declined = v == "declined";
        }
        self.preview_auto_waiting = std::env::var("ASCENDANT_PREVIEW_AUTO_WAITING").ok();
        self.preview_status_open = std::env::var("ASCENDANT_PREVIEW_STATUS_OPEN").is_ok_and(|v| v == "1");
        self.restoring = std::env::var("ASCENDANT_PREVIEW_RESTORE").is_ok_and(|v| v == "1");
        self.floats.tools = std::env::var("ASCENDANT_PREVIEW_TOOLS").is_ok_and(|v| v == "1");
        // ASCENDANT_PREVIEW_WHATS_NEW=1: "What's new" open from the start, as its button opens it.
        if std::env::var("ASCENDANT_PREVIEW_WHATS_NEW").is_ok_and(|v| v == "1") {
            self.open_whats_new(false);
        }
        // ASCENDANT_PREVIEW_HEALTH=issues or clear: a made-up run check (see `preview_health`) with its window open.
        if let Ok(v) = std::env::var("ASCENDANT_PREVIEW_HEALTH")
            && matches!(v.as_str(), "issues" | "clear")
        {
            self.session.health = Some(preview_health(v == "issues"));
            self.session.health_dir = Some(PathBuf::from("2026-11-07_18-18-08"));
            self.floats.open_health();
        }
        // ASCENDANT_PREVIEW_NO_OCR=1: as if this computer had no OCR language (the system's OCR is not removed),
        // for the line and the install button; =installing, declined, failed or done shows that state too (PowerShell never starts).
        if let Ok(v) = std::env::var("ASCENDANT_PREVIEW_NO_OCR") {
            self.preview_no_ocr = true;
            self.ocr_problem = ocr::problem_with(None);
            let lang = OcrLang::of(lang::lang());
            match v.as_str() {
                "installing" => {
                    let (tx, rx) = mpsc::channel();
                    // Preview: the sender is kept, so it stays "Installing...".
                    std::mem::forget(tx);
                    self.ocr_installing = Some((lang, rx));
                }
                "declined" => self.ocr_result = Some((lang, OcrInstall::Declined)),
                "failed" => self.ocr_result = Some((lang, install::ocr_outcome(0x800F_0954))),
                "done" => self.ocr_result = Some((lang, OcrInstall::Done { restart: false })),
                _ => {}
            }
        }
        // ASCENDANT_PREVIEW_UPDATE=current, checking, available, dot (as the hourly check found it: only the red dot
        // on Settings), updating, failed (a download that did not verify), unverified (a check that found an
        // unverified release), offline (a check that could not reach GitHub), lost (a swap that could not be undone)
        // or notrestarted (the new exe is in place but did not start), the last two with "Update" off: that update
        // state with a made-up release (nothing is fetched or downloaded; the start check is skipped too, and no
        // hourly check is ever scheduled), the settings page scrolled to the update group.
        // ASCENDANT_PREVIEW_UPDATED=1: as if an update had just installed this version: "Updated to X.Y.Z" in the
        // status bar and "What's new" in About, with made-up saved notes in place of any (nothing is written for them;
        // the start check is skipped; the built-in notes of this version are shown before them, see `release_notes`).
        // ASCENDANT_PREVIEW_UPDATED=notes: the same, with two made-up older versions (0.9.0 and 0.9.10, in this preview
        // only) after the built-in ones and the "What's new" window open (with ASCENDANT_PREVIEW_UPDATE=available, as
        // opened from that release).
        let updated = std::env::var("ASCENDANT_PREVIEW_UPDATED").ok();
        if matches!(updated.as_deref(), Some("1" | "notes")) {
            self.this_notes = Some(update::SavedNotes {
                version: env!("CARGO_PKG_VERSION").to_string(),
                notes: "[[zh-TW]]\n- 開著時每小時檢查一次新版，有新版只在設定圖示加紅點\n- 更新後顯示這版的更新內容\n- 其他小修正\n[[en]]\n- Checks for a new version every hour while open; a new one only puts a red dot on Settings\n- Shows what is new after an update\n- Other small fixes".to_string(),
            });
            self.notes_history = release_notes(self.this_notes.as_ref());
            if updated.as_deref() == Some("notes") {
                let made_up = [
                    ("0.9.0", "[[zh-TW]]\n- 預覽用假資料：第一個測試版\n- 塔羅斯盤面讀取\n[[en]]\n- Made-up preview notes: the first test build\n- Tartarus board reading"),
                    ("0.9.10", "[[zh-TW]]\n- 預覽用假資料：0.9.10 排在 0.9.0 之上\n- 渴血建議\n[[en]]\n- Made-up preview notes: 0.9.10 sorts above 0.9.0\n- Vorax advice"),
                ];
                for (version, notes) in made_up {
                    self.notes_history.push(update::SavedNotes { version: version.into(), notes: notes.into() });
                }
                newest_first(&mut self.notes_history);
            }
            // Shown whatever `updated_seen` says, which is left as it is: dismissing it in a preview saves nothing.
            self.preview_updated = Some(true);
        }
        // ASCENDANT_PREVIEW_VERIFY=ok, warn (verified, with a stray DLL and an older version), bad or missing: the
        // answer of "Verify downloaded files..." (no dialog opens, no folder is read).
        if let Ok(v) = std::env::var("ASCENDANT_PREVIEW_UPDATE") {
            self.preview_update_scroll = true;
            let release = update::Release {
                version: [1, 1, 0],
                notes: "[[zh-TW]]\n- 自動更新：下載後先驗證 Ed25519 簽章\n- 渴血：排名第 4～6 名改成一行\n- 塔羅斯：盤面讀取更穩定\n- 設定頁新增「立即檢查」\n- 其他小修正\n[[en]]\n- Updates: the Ed25519 signature is checked after the download\n- Vorax: ranks 4 to 6 on one line\n- Tartarus: steadier board reading\n- Settings: new \"Check now\"\n- Other small fixes".to_string(),
                exe: update::Asset { url: String::new(), size: 0, digest: None },
                sums: update::Asset { url: String::new(), size: 0, digest: None },
                sig: update::Asset { url: String::new(), size: 0, digest: None },
                signed: Some([0; 32]),
            };
            match v.as_str() {
                "current" => self.checked = true,
                "checking" => {
                    let (tx, rx) = mpsc::channel();
                    // Preview: the sender is kept, so it stays "Checking...".
                    std::mem::forget(tx);
                    self.checking = Some(rx);
                }
                "available" => (self.release, self.checked) = (Some(release), true),
                // The hourly check found it: the red dot on Settings only, the status bar unchanged.
                "dot" => (self.release, self.checked, self.release_quiet) = (Some(release), true, true),
                "updating" => {
                    let (tx, rx) = mpsc::channel();
                    std::mem::forget(tx);
                    self.updating = Some((rx, Arc::new(AtomicU8::new(update::Step::Download as u8))));
                    (self.release, self.checked) = (Some(release), true);
                }
                "failed" => {
                    let e = update::Failed::Problem(update::Problem::Unverified(update::signature_mismatch()));
                    (self.checked, self.error, self.update_error) = (true, Some(e.text()), Some(e));
                }
                "unverified" => self.check_error = Some(update::Problem::Unverified(update::signature_mismatch())),
                "offline" => self.check_error = Some(update::Problem::Network("https://api.github.com/repos/...: timed out".into())),
                "lost" => {
                    let e = update::Failed::Lost { old: PathBuf::from(r"C:\Games\Ascendant\Ascendant.old.exe"), error: "Access is denied. (os error 5)".into() };
                    (self.release, self.checked, self.error, self.update_error, self.update_locked) = (Some(release), true, Some(e.text()), Some(e), true);
                }
                "notrestarted" => {
                    let e = update::Failed::NotRestarted(r"C:\Games\Ascendant\Ascendant.exe: Access is denied. (os error 5)".into());
                    (self.release, self.checked, self.error, self.update_error, self.update_locked) = (Some(release), true, Some(e.text()), Some(e), true);
                }
                _ => {}
            }
        }
        if updated.as_deref() == Some("notes") {
            self.open_whats_new(true);
        }
        if let Ok(v) = std::env::var("ASCENDANT_PREVIEW_VERIFY") {
            self.preview_update_scroll = true;
            // A made-up answer with the texts verify_download_report would give.
            let verified = |version: [u64; 3], others: Vec<String>, warnings: Vec<String>| {
                let [a, b, c] = version;
                let line = trf!("Verified: Ascendant.exe is version {} as signed with Ascendant's release key.", format!("{a}.{b}.{c}"));
                update::Verified { version, line, others, warnings }
            };
            self.verified = match v.as_str() {
                "ok" => Some(Ok(verified([1, 1, 0], Vec::new(), Vec::new()))),
                "warn" => {
                    let stray = trf!("Warning: the folder also holds {}. Windows may load a DLL from the folder an exe starts in: move Ascendant.exe to a folder of its own before starting it.", "d3dcompiler_47.dll");
                    let older = trf!("Warning: this is an older version ({}) than the one running ({}).", "0.9.0", env!("CARGO_PKG_VERSION"));
                    Some(Ok(verified([0, 9, 0], vec![update::NOTES.to_string()], vec![stray, older])))
                }
                "bad" => Some(Err(update::Problem::Unverified(trf!("the downloaded {} does not match the signed SHA-256", "Ascendant.exe")))),
                "missing" => Some(Err(update::Problem::Files(trf!("The folder has no {}: put Ascendant.exe, {} and {} in one folder.", "SHA256SUMS.txt.sig", update::SUMS, update::SIG)))),
                _ => None,
            };
        }
        // ASCENDANT_PREVIEW_REPORT=run folder: exports that run on the Run tab at start; ASCENDANT_PREVIEW_REPORT_SAVE=1
        // saves it once made (for the saved screen).
        if let Some(dir) = super::report::preview_dir() {
            // Saving by itself only in debug or preview builds (see report::preview_save_allowed).
            self.review.report.preview_save = super::report::preview_save_allowed() && std::env::var("ASCENDANT_PREVIEW_REPORT_SAVE").is_ok_and(|v| v == "1");
            let ctx = self.ctx.clone();
            self.review.report.start_dir(dir, None, &self.settings.install_id, move || ctx.request_repaint());
            self.tab = Tab::Run;
        }
    }

    /// The "Restore defaults" confirmation window: it runs only when "Restore" is pressed; "Cancel", a click
    /// outside the window or Esc just close it.
    fn restore_dialog(&mut self, ctx: &egui::Context) {
        if !self.restoring {
            return;
        }
        let (mut restore, mut cancel) = (false, false);
        let width = (ctx.content_rect().width() - 48.0).clamp(140.0, 260.0);
        let modal = egui::Modal::new(egui::Id::unique("restore-defaults")).show(ctx, |ui| {
            ui.set_width(width);
            ui.label(RichText::new(tr("Restore defaults")).strong().color(egui::Color32::WHITE));
            ui.add_space(4.0);
            ui.add(egui::Label::new(tr("Restore the settings to their defaults? The language and the install are kept; saved run logs are not deleted.")).wrap());
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                restore = ui.button(tr("Restore")).clicked();
                cancel = ui.button(tr("Cancel")).clicked();
            });
        });
        if restore {
            self.restore_defaults(ctx);
        }
        if restore || cancel || modal.should_close() {
            self.restoring = false;
        }
    }

    /// Restore defaults (which fields are kept: see [`restored`]), effective immediately: the two hotkeys
    /// are registered again, click-through is turned off, and the auto read and capture window title,
    /// run log, rollout count and font size are applied and then saved; opacity is applied by `logic` every
    /// frame. The window size and position return to the defaults only on the next launch (296×520, placed by
    /// the game panel); the window is not touched now.
    fn restore_defaults(&mut self, ctx: &egui::Context) {
        let old = std::mem::take(&mut self.settings);
        self.settings = restored(&old);
        // Release the two old hotkeys before registering the new ones: old and new may swap (for example the
        // old read key is the new auto read key).
        self.recording = None;
        for which in Which::ALL {
            let key = &mut self.keys[which as usize];
            key.hotkey = None;
            key.text = which.setting(&mut self.settings).clone();
        }
        for which in Which::ALL {
            self.apply_hotkey(which);
        }
        self.click_through = false;
        if old.run_log != self.settings.run_log {
            self.session.log = self.settings.run_log.then(RunLog::in_appdata).flatten();
        }
        if old.rollouts != self.settings.rollouts {
            self.session.rollouts = self.settings.rollouts;
            self.session.solve();
        }
        // Auto read, game window title and observe mode: turn the capture on, off or rebuild it according to
        // the new settings.
        self.watch();
        self.set_text_scale(ctx, self.settings.font_scale);
        self.save();
        // the affix frames' switch and style are back to their defaults: the live frames and the affix page follow now
        self.configure_affixes();
    }
}

/// The settings after "Restore defaults" (font spec point 5). How each Settings field is handled:
/// - Kept: `language`; `offer_install`, `desktop_shortcut` (install state); `check_updates`, `share_logs`,
///   `shared_from`, `install_id` (update check and share consent, related to install and privacy); `updated_seen`
///   (the "Updated to" notice already dismissed); `tools`,
///   `tools_days`, `theme` (the instruments and theme held in this run are game progress, not preferences, and
///   restoring them would make this run's advice wrong); `run_log` (whether to save run logs is a privacy and
///   disk choice, and restoring does not silently turn it on); `keep_all_logs` (restoring must never start
///   deleting the player's run logs); `affix_grades` (the grade the user gave each map affix, one
///   by one: a list of their own rather than a preference, with its own two-click reset on the affix page).
/// - Back to default: `affix_frames` (on) and `affix_frame_style` (A frame).
/// - Back to default: `read_hotkey`, `auto_hotkey`, `click_through` (off), `opacity`,
///   `window_title`, `rollouts`, `auto_read`, `auto_place`, `window_pos` and `window_size` (back to 296×520 on
///   the next launch and placed by the game panel again), `float_pos` (the small windows open over the main window
///   again), `observe`, `highlight`, `show_value`, `font_scale` (standard),
///   `vorax_enabled`, `vorax_rollouts`, `vorax_learn`, `vorax_strips`, `game_log` (automatic again), `start_as_admin`
///   ("Start as administrator", off).
///
/// A field added later that is not listed as kept returns to the default. The run log files themselves are not
/// deleted.
fn restored(old: &Settings) -> Settings {
    Settings {
        language: old.language.clone(),
        offer_install: old.offer_install,
        desktop_shortcut: old.desktop_shortcut,
        check_updates: old.check_updates,
        // A dismissed "Updated to" notice is not a preference: restoring must not bring it back.
        updated_seen: old.updated_seen.clone(),
        share_logs: old.share_logs,
        shared_from: old.shared_from.clone(),
        install_id: old.install_id.clone(),
        tools: old.tools.clone(),
        tools_days: old.tools_days,
        theme: old.theme,
        // Whether to save the run log: when the user turned it off, that is a privacy and disk choice, and
        // restoring does not silently turn it on.
        run_log: old.run_log,
        // Keeping every run log: restoring must never start deleting the player's logs.
        keep_all_logs: old.keep_all_logs,
        // The user's own grade of each map affix: kept (the affix page has its own reset).
        affix_grades: old.affix_grades.clone(),
        ..Settings::default()
    }
}

/// The "Layout" setting on the settings page: auto (by window size), full, compact. Stored in
/// [`Settings::layout`] ("auto", "full", "compact"); an unrecognised value (including the default when an old
/// settings file lacks the field) counts as auto.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LayoutPref {
    Auto,
    Full,
    Compact,
}

impl LayoutPref {
    fn key(self) -> &'static str {
        match self {
            LayoutPref::Auto => "auto",
            LayoutPref::Full => "full",
            LayoutPref::Compact => "compact",
        }
    }

    fn name(self) -> &'static str {
        match self {
            LayoutPref::Auto => "Auto",
            LayoutPref::Full => "Full",
            LayoutPref::Compact => "Compact",
        }
    }
}

fn layout_pref(key: &str) -> LayoutPref {
    match key {
        "full" => LayoutPref::Full,
        "compact" => LayoutPref::Compact,
        _ => LayoutPref::Auto,
    }
}

/// Whether this frame uses the compact layout. Full and compact are fixed; auto looks only at the height
/// `height` of the window's content area (points, including the font size scale, so "Large" / "Extra large"
/// switch to compact more easily), with hysteresis: when currently full it switches to compact only below
/// `FULL_HEIGHT - 10`, and when currently compact it switches back only above `FULL_HEIGHT + 10`; in between it
/// stays as it was, so it does not flicker near the threshold. While the player is dragging the window size
/// (`resizing`, see [`AscendantApp::track_size`]) it stays as it was, and is decided after release.
fn next_compact(pref: LayoutPref, was: bool, height: f32, resizing: bool) -> bool {
    match pref {
        LayoutPref::Full => false,
        LayoutPref::Compact => true,
        LayoutPref::Auto if resizing => was,
        LayoutPref::Auto if was => height <= FULL_HEIGHT + LAYOUT_HYSTERESIS,
        LayoutPref::Auto => height < FULL_HEIGHT - LAYOUT_HYSTERESIS,
    }
}

/// The note added under every red game log reminder (the user, 2026-11-05: the game may have turned logging on by
/// itself, but the hint stays).
const LOG_NOTE: &str = "Also make sure logging is on in the game: Settings \u{2192} Other \u{2192} Enable log (or the menu at the top right of the login screen \u{2192} Enable log)";

/// The game log light in the navigation rail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lamp {
    Green,
    Yellow,
    Red,
    /// Auto read is off: there is no watcher, so the log is not read.
    Grey,
}

impl Lamp {
    fn colour(self) -> Color32 {
        match self {
            Lamp::Green => theme::GOOD,
            Lamp::Yellow => theme::WARN,
            Lamp::Red => theme::ERROR,
            Lamp::Grey => Color32::from_gray(85),
        }
    }

    /// The status line's colour on the settings page (grey text is too dark, so it is muted).
    fn text_colour(self) -> Color32 {
        if self == Lamp::Grey { MUTED } else { self.colour() }
    }
}

/// The light for a game log status (None: no watcher): green while followed and written; yellow while searching
/// or while the game is closed; red when it is not written while the game runs, not found while the game runs, or
/// the chosen file does not exist.
fn log_lamp(status: Option<&gamelog::LogStatus>) -> Lamp {
    use gamelog::LogStatus as S;
    match status {
        None => Lamp::Grey,
        Some(S::Following { .. }) => Lamp::Green,
        Some(S::Searching | S::GameClosed { .. }) => Lamp::Yellow,
        Some(S::Stale { .. } | S::Missing | S::ManualMissing(_)) => Lamp::Red,
    }
}

fn source_name(source: &gamelog::LogSource) -> &'static str {
    match source {
        gamelog::LogSource::Manual => tr("Manual"),
        gamelog::LogSource::GameFolder => tr("Game folder"),
        gamelog::LogSource::LocalAppData => tr("Local app data"),
        gamelog::LogSource::Steam => tr("Steam"),
    }
}

/// The status line of the game log (the settings page and the light's tooltip).
fn log_line(status: Option<&gamelog::LogStatus>) -> String {
    use gamelog::{LogSource, LogStatus as S};
    match status {
        None => tr("Auto read is off: the game log is not read").to_string(),
        Some(S::Searching) => tr("Game log: automatic, searching... (try again once the game is open)").to_string(),
        Some(S::Missing) => tr("Game log: automatic, not found while the game is open").to_string(),
        Some(S::ManualMissing(_)) => tr("Game log: the chosen file does not exist").to_string(),
        Some(S::Following { source: LogSource::Manual, .. }) => tr("Game log: manual").to_string(),
        Some(S::Following { source, .. }) => trf!("Game log: automatic (found: {})", source_name(source)),
        Some(S::GameClosed { source: LogSource::Manual, .. }) => tr("Game log: manual (the game is not open)").to_string(),
        Some(S::GameClosed { source, .. }) => trf!("Game log: automatic (found: {}; the game is not open)", source_name(source)),
        Some(S::Stale { source: LogSource::Manual, .. }) => tr("Game log: manual, not being written").to_string(),
        Some(S::Stale { source, .. }) => trf!("Game log: automatic (found: {}), not being written", source_name(source)),
    }
}

/// The reminder of a red light (also on the status bar); None for the other lights. The note [`LOG_NOTE`] goes
/// under it.
fn log_alert(status: Option<&gamelog::LogStatus>) -> Option<&'static str> {
    use gamelog::LogStatus as S;
    match status? {
        S::Stale { .. } => Some(tr("The game is open but its log is not being written: check the log's place in Settings (game folder\\UE_game\\TorchLight\\Saved\\Logs\\UE_game.log)")),
        S::Missing => Some(tr("Game log not found: choose UE_game.log in Settings")),
        S::ManualMissing(_) => Some(tr("The chosen log file does not exist: choose it again in Settings, or restore automatic")),
        _ => None,
    }
}

/// The file the status names, if any.
fn log_path(status: Option<&gamelog::LogStatus>) -> Option<PathBuf> {
    use gamelog::LogStatus as S;
    match status? {
        S::Following { path, .. } | S::Stale { path, .. } | S::GameClosed { path, .. } | S::ManualMissing(path) => Some(path.clone()),
        S::Searching | S::Missing => None,
    }
}

/// The light's tooltip: the status, the path, and for a red light its reminder and note; a click opens Settings.
fn log_hover(status: Option<&gamelog::LogStatus>) -> String {
    let mut hover = log_line(status);
    if let Some(path) = log_path(status) {
        hover.push_str(&format!("\n{}", path.display()));
    }
    if let Some(alert) = log_alert(status) {
        hover.push_str(&format!("\n{alert}\n{}", tr(LOG_NOTE)));
    }
    hover.push_str(&format!("\n{}", tr("Click to open Settings")));
    hover
}

/// A game log status for previews (ASCENDANT_PREVIEW_LOG); None ("off") is as if auto read were off.
fn preview_log_status(which: &str) -> Option<gamelog::LogStatus> {
    use gamelog::{LogSource, LogStatus as S};
    let path = PathBuf::from(r"F:\SteamLibrary\steamapps\common\Torchlight Infinite\UE_game\TorchLight\Saved\Logs\UE_game.log");
    let source = LogSource::GameFolder;
    match which {
        "following" => Some(S::Following { path, source }),
        "searching" => Some(S::Searching),
        "closed" => Some(S::GameClosed { path, source }),
        "stale" => Some(S::Stale { path, source }),
        "missing" => Some(S::Missing),
        "manual-missing" => Some(S::ManualMissing(PathBuf::from(r"D:\Games\Logs\UE_game.log"))),
        _ => None,
    }
}

/// The game log light in the navigation rail: a dot in the light's colour on top and the small text "Log" below,
/// laid out as the Auto button above it.
fn log_button(ui: &mut egui::Ui, size: Vec2, lamp: Lamp) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let visuals = ui.style().interact(&response);
    let p = ui.painter();
    p.rect_filled(rect, 5.0, visuals.weak_bg_fill);
    p.rect_stroke(rect, 5.0, visuals.bg_stroke, StrokeKind::Inside);
    p.circle_filled(rect.center_top() + Vec2::new(0.0, 8.5), 3.5, lamp.colour());
    p.text(rect.center_bottom() - Vec2::new(0.0, 4.0), Align2::CENTER_BOTTOM, tr("Log"), FontId::proportional(MIN_TEXT), visuals.fg_stroke.color);
    response
}

/// One accumulated progress item (a list shared by the progress page, the navigation rail and the status bar):
/// the name (English key; the Chinese is in zh_tw.rs), how to get the value from [`gamelog::Charges`], the
/// limit, and the cost per run. To add an item later (for example the "Tarot" the user mentioned, whose code
/// and limit are still being looked up) just add one entry; when Charges becomes a list, replace `value` with a
/// table lookup.
struct ChargeItem {
    name: &'static str,
    value: fn(&gamelog::Charges) -> Option<u32>,
    max: u32,
    per_run: u32,
}

fn vorax_charge(c: &gamelog::Charges) -> Option<u32> {
    c.vorax
}

fn tartarus_charge(c: &gamelog::Charges) -> Option<u32> {
    c.tartarus
}

/// The names are written only as "Vorax" and "Tartarus" (Chinese `渴血` and `塔羅斯` in the zh table), with no other
/// words.
const CHARGE_ITEMS: [ChargeItem; 2] = [
    ChargeItem { name: "Vorax", value: vorax_charge, max: gamelog::CHARGE_MAX, per_run: gamelog::CHARGE_PER_RUN },
    ChargeItem { name: "Tartarus", value: tartarus_charge, max: gamelog::CHARGE_MAX, per_run: gamelog::CHARGE_PER_RUN },
];

/// The fullest state among all items (full > nearly full > normal); None when none has a value (the navigation
/// button is grey).
fn worst_fill(c: &gamelog::Charges) -> Option<Fill> {
    let rank = |f: Fill| match f {
        Fill::Normal => 0,
        Fill::Near => 1,
        Fill::Full => 2,
    };
    CHARGE_ITEMS.iter().filter_map(|i| (i.value)(c)).map(gamelog::fill).max_by_key(|&f| rank(f))
}

/// Hover tooltip of the progress button: one line per item such as "Vorax 48/48 (4 runs left)", "Tartarus —"
/// when there is no value, and a last line "Click to see all progress".
fn progress_hover(c: &gamelog::Charges) -> String {
    let mut lines: Vec<String> = CHARGE_ITEMS
        .iter()
        .map(|i| match (i.value)(c) {
            Some(v) => trf!("{} {}/{} ({} runs left)", tr(i.name), v, i.max, v / i.per_run),
            None => format!("{} \u{2014}", tr(i.name)),
        })
        .collect();
    lines.push(tr("Click to see all progress").to_string());
    lines.join("\n")
}

/// Full items are joined in one phrase: in Chinese `渴血、塔羅斯`, in English "Vorax and Tartarus".
fn join_names(names: &[&str]) -> String {
    names.join(if lang::zh() { "\u{3001}" } else { " and " })
}

/// Test fake values `vorax:48,tartarus:46`; parts that are not recognised are skipped.
fn preview_charges(text: &str) -> gamelog::Charges {
    let mut c = gamelog::Charges::default();
    for part in text.split(',') {
        let Some((key, value)) = part.split_once(':') else { continue };
        let value = value.trim().parse().ok();
        match key.trim() {
            "vorax" => c.vorax = value,
            "tartarus" => c.tartarus = value,
            _ => {}
        }
    }
    c
}

/// The progress button of the navigation rail (an hourglass, the same size as the other tab buttons): grey
/// normally; yellow background when any item is nearly full, red background plus a red dot at the top right
/// when full; the primary colour when selected (with a warning, a primary-colour outline instead, keeping the
/// warning's background).
fn progress_button(ui: &mut egui::Ui, size: Vec2, selected: bool, fill: Option<Fill>) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let p = ui.painter();
    let alert = match fill {
        Some(Fill::Full) => Some(Color32::from_rgb(0x8a, 0x2c, 0x2c)),
        Some(Fill::Near) => Some(Color32::from_rgb(0x8a, 0x70, 0x22)),
        _ => None,
    };
    match alert {
        Some(bg) => {
            p.rect_filled(rect, 6.0, bg);
            if selected {
                p.rect_stroke(rect, 6.0, Stroke::new(1.5, ACCENT), StrokeKind::Inside);
            }
        }
        None if selected => {
            p.rect_filled(rect, 6.0, ACCENT);
        }
        None if response.hovered() => {
            p.rect_filled(rect, 6.0, Color32::from_rgb(0x2a, 0x2f, 0x3a));
        }
        None => {}
    }
    let colour = if selected || alert.is_some() || response.hovered() { Color32::WHITE } else { MUTED };
    let c = rect.center();
    let (w, h) = (5.0, 7.0);
    let stroke = Stroke::new(1.3, colour);
    // Hourglass: two horizontal lines at the top and bottom, and two triangles with their tips facing each
    // other in the middle.
    p.line_segment([Pos2::new(c.x - w - 1.0, c.y - h), Pos2::new(c.x + w + 1.0, c.y - h)], stroke);
    p.line_segment([Pos2::new(c.x - w - 1.0, c.y + h), Pos2::new(c.x + w + 1.0, c.y + h)], stroke);
    let glass = vec![Pos2::new(c.x - w, c.y - h), Pos2::new(c.x + w, c.y - h), Pos2::new(c.x, c.y), Pos2::new(c.x + w, c.y + h), Pos2::new(c.x - w, c.y + h), Pos2::new(c.x, c.y)];
    p.add(Shape::closed_line(glass, stroke));
    // The sand in the lower half.
    p.add(Shape::convex_polygon(vec![Pos2::new(c.x, c.y + 2.5), Pos2::new(c.x + 3.0, c.y + h - 0.5), Pos2::new(c.x - 3.0, c.y + h - 0.5)], colour, Stroke::NONE));
    if fill == Some(Fill::Full) {
        p.circle_filled(rect.right_top() + Vec2::new(-3.0, 4.0), 4.0, theme::ERROR);
    }
    response
}

/// The small header line of the board page: `Tartarus · Day 7 · 13 days left`.
fn header(day: i64, days_left: Option<u32>) -> String {
    match days_left {
        Some(n) => trf!("Tartarus \u{b7} Day {} \u{b7} {} days left", day, n),
        None => trf!("Tartarus \u{b7} Day {} \u{b7} Riot not read", day),
    }
}

/// The conclusion of comparing the advice with the runner-up (plan §3): no label when there are fewer than two
/// ranks, no matching rank row, or still computing. When the advice is the first rank it is compared with the
/// second, otherwise with the first; `gap_se` is the paired standard error of the gap to the first (the first's
/// own is 0, so for first against second the second's is used). A gap beyond 2 standard errors is "Clearly
/// better", otherwise "Either is fine"; when the standard error is not finite (fewer than two playout rounds)
/// no conclusion is drawn.
fn verdict(ranking: &[SolveOption], option: Option<usize>, solving: bool) -> Option<Verdict> {
    let k = option.filter(|&k| k < ranking.len())?;
    if ranking.len() < 2 || solving {
        return None;
    }
    let (rival, se) = if k == 0 { (&ranking[1], ranking[1].gap_se) } else { (&ranking[0], ranking[k].gap_se) };
    if !se.is_finite() {
        return None;
    }
    let margin = ranking[k].mean - rival.mean;
    Some(if margin > 2.0 * se { Verdict::Clear } else { Verdict::Either })
}

/// Column widths of each top-three row: rank, option name, expected loot (`5.69`), gap (small text `−0.12`);
/// the column headings and every row share them, so they stay aligned even at a width of 200. The name uses the
/// remaining width (wider than before now that the bars are gone).
/// `gap` is 0 when the window is too narrow for the gap column: the rows and the heading then leave it out.
struct Cols {
    rank: f32,
    label: f32,
    num: f32,
    gap: f32,
}

/// What the top-three columns need, see [`Cols::fit`]: the expected loot column (`num`), the gap column (`gap`), the
/// two headings over them (`head`, spanning both, right-aligned) or the expected loot's alone (`loot`, without the gap
/// column), and the name column with the action icon and the
/// cell number (`cell`) or the icon alone (`icon`).
struct Need {
    num: f32,
    gap: f32,
    head: f32,
    loot: f32,
    cell: f32,
    icon: f32,
}

impl Cols {
    #[cfg(test)]
    fn new(width: f32, spacing: f32) -> Cols {
        Cols::fit(width, spacing, Need { num: 36.0, gap: 38.0, head: 0.0, loot: 0.0, cell: 40.0, icon: 15.0 })
    }

    /// The number columns get `num` and `gap` (the width actually needed) and the name column what is left, so the
    /// columns add up to `width` and every row's numbers stay under their headings. When the name column would be
    /// too narrow, things give way in this order (the row itself drops the small tile first, see `moves`): the
    /// expected loot column no longer widens for its heading (the heading is elided), the gap column goes, the name
    /// column keeps only the icon (the gap back if it then fits). The cell number goes after the gap, as it says
    /// which cell the move acts on. Only when even the icon and the expected loot do not fit does the row run past
    /// `width`.
    fn fit(width: f32, spacing: f32, need: Need) -> Cols {
        let rank = 12.0;
        let wide = need.num.max(need.head - spacing - need.gap);
        let wide_alone = need.num.max(need.loot);
        let label = |num: f32, gap: f32| {
            let columns = if gap > 0.0 { 3.0 } else { 2.0 };
            width - rank - num - gap - columns * spacing
        };
        let tries = [
            (wide, need.gap, need.cell),
            (need.num, need.gap, need.cell),
            (wide_alone, 0.0, need.cell),
            (need.num, 0.0, need.cell),
            (need.num, need.gap, need.icon),
            (need.num, 0.0, need.icon),
        ];
        let (num, gap) = tries.iter().find(|&&(num, gap, least)| label(num, gap) >= least).map_or((need.num, 0.0), |&(num, gap, _)| (num, gap));
        Cols { rank, label: label(num, gap).max(need.icon), num, gap }
    }
}

/// `label` without the leading words of the move template `en` in the window's language (`Take {}` gives "Take " or
/// `選擇 `), which the row's icon already says; the label itself when it does not start with them or nothing would
/// be left. The advice card and tooltips keep the full wording.
fn strip_template(label: &str, en: &'static str) -> String {
    let head = tr(en).split("{}").next().unwrap_or_default().to_string();
    match label.strip_prefix(head.as_str()) {
        Some(rest) if !head.is_empty() && !rest.is_empty() => rest.to_string(),
        _ => label.to_string(),
    }
}

/// Which tooltip of the first top-three row a preview keeps on show (ASCENDANT_PREVIEW_TOOLTIP=icon, cell or row).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MoveTip {
    Icon,
    Cell,
    Row,
}

/// The action icon of a top-three row, drawn with the painter (lines and triangles, see [`paint_move_icon`]) rather
/// than with font characters, which differ between computers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MoveIcon {
    /// Go to a cell (a patrol, the Interrogation Room included, or the Sewer Ratway's move): an arrow to the right.
    GoTo,
    /// Consume, the day's first bite: a triangle pointing down.
    Consume,
    /// Chain on into the next cell: two triangles pointing down, one above the other.
    Chain,
    /// End the chain: a square (stop).
    EndChain,
    /// Replace the Inmate in the Warden's cell: two arrows, one each way.
    Replace,
    /// Re-roll the instrument offer: a circle of arrow.
    Reroll,
    /// Take or pick an option (an instrument, a treasure, an Interrogation card): a tick.
    Take,
    /// Hunt, mark a prey: a crosshair.
    Hunt,
    /// Confirm a Prison Incident: an exclamation mark in a ring.
    Confirm,
    /// End the day: a crescent moon.
    EndDay,
    /// Finish the run (End on the last day): the same crescent moon.
    Finish,
}

impl MoveIcon {
    /// What the icon means, for its tooltip.
    fn meaning(self) -> String {
        match self {
            MoveIcon::GoTo => tr("Go to (move to this cell)").to_string(),
            MoveIcon::Consume => tr("Consume (eat the Inmate in this cell)").to_string(),
            MoveIcon::Chain => tr("Chain on (eat this cell next)").to_string(),
            MoveIcon::EndChain => tr("End the chain (stop eating)").to_string(),
            MoveIcon::Replace => tr("Replace (a new Inmate in this cell)").to_string(),
            MoveIcon::Reroll => tr("Re-roll (draw a new offer)").to_string(),
            MoveIcon::Take => tr("Take (pick this option)").to_string(),
            MoveIcon::Hunt => trf!("{} (mark the Inmate in this cell as prey)", hunt_name()),
            MoveIcon::Confirm => trf!("Confirm (go on with this {})", incident_term()),
            MoveIcon::EndDay => tr("End the day (nothing more today)").to_string(),
            MoveIcon::Finish => tr("Finish the run (the last day ends)").to_string(),
        }
    }
}

/// What follows the icon in a top-three row: the cell the move acts on (its number and a small tile), or a short
/// text for a move without one.
#[derive(Clone, Debug, PartialEq, Eq)]
enum MoveTail {
    Cell(usize),
    Text(String),
}

/// A move's icon and what follows it in the top three; `warden` is the Warden's model cell (a Replace acts there).
fn move_row(action: &Action, label: &str, warden: i64) -> (MoveIcon, MoveTail) {
    let cell = |n: usize| MoveTail::Cell(n);
    match action {
        Action::Patrol { cell: n } => (MoveIcon::GoTo, cell(*n)),
        Action::Devour { to } => (MoveIcon::Consume, cell(*to)),
        Action::Chain { to } => (MoveIcon::Chain, cell(*to)),
        Action::Hunt { cell: n } => (MoveIcon::Hunt, cell(*n)),
        // The Hunt's own target pick and the Sewer Ratway's move both choose a cell; only the Hunt's label starts
        // with its name (see solve.rs `hunt_label`).
        Action::Choose { id: ChoiceId::Cell(n) } if label.starts_with(hunt_name()) => (MoveIcon::Hunt, cell(*n)),
        Action::Choose { id: ChoiceId::Cell(n) } => (MoveIcon::GoTo, cell(*n)),
        Action::Choose { id: ChoiceId::Name(_) } => (MoveIcon::Take, MoveTail::Text(strip_template(label, "Take {}"))),
        Action::Swap => match usize::try_from(warden) {
            Ok(n) => (MoveIcon::Replace, cell(n)),
            Err(_) => (MoveIcon::Replace, MoveTail::Text(tr("Replace").to_string())),
        },
        Action::Reroll => (MoveIcon::Reroll, MoveTail::Text(label.to_string())),
        Action::EndChain => (MoveIcon::EndChain, MoveTail::Text(label.to_string())),
        Action::Event => (MoveIcon::Confirm, MoveTail::Text(strip_template(label, "Confirm {}"))),
        Action::End if label == tr("Finish the run") => (MoveIcon::Finish, MoveTail::Text(label.to_string())),
        Action::End => (MoveIcon::EndDay, MoveTail::Text(label.to_string())),
    }
}

/// The tooltip of a row's small tile: the cell's number and what it holds, "Cell 11: Magic Mammoth".
fn cell_tip(n: usize, cell: &CellModel) -> String {
    trf!("Cell {}: {}", cell_label(n), describe(cell))
}

/// Draws `icon` in `rect` (a row's height) in `colour`.
fn paint_move_icon(p: &egui::Painter, rect: Rect, icon: MoveIcon, colour: Color32) {
    let s = (rect.height() * 0.62).min(rect.width());
    let c = rect.center();
    let half = s / 2.0;
    let stroke = Stroke::new(1.6, colour);
    let at = |x: f32, y: f32| Pos2::new(c.x + x * half, c.y + y * half);
    let triangle = |a: Pos2, b: Pos2, d: Pos2| {
        p.add(Shape::convex_polygon(vec![a, b, d], colour, Stroke::NONE));
    };
    match icon {
        MoveIcon::GoTo => {
            p.line_segment([at(-1.0, 0.0), at(0.2, 0.0)], stroke);
            triangle(at(0.0, -0.7), at(1.0, 0.0), at(0.0, 0.7));
        }
        MoveIcon::Consume => triangle(at(-0.9, -0.6), at(0.9, -0.6), at(0.0, 0.8)),
        MoveIcon::Chain => {
            triangle(at(-0.8, -1.0), at(0.8, -1.0), at(0.0, -0.05));
            triangle(at(-0.8, 0.1), at(0.8, 0.1), at(0.0, 1.05));
        }
        MoveIcon::EndChain => {
            p.rect_filled(Rect::from_center_size(c, Vec2::splat(s * 0.75)), 1.0, colour);
        }
        MoveIcon::Replace => {
            p.line_segment([at(-1.0, -0.45), at(0.4, -0.45)], stroke);
            triangle(at(0.3, -0.85), at(1.0, -0.45), at(0.3, -0.05));
            p.line_segment([at(-0.4, 0.45), at(1.0, 0.45)], stroke);
            triangle(at(-0.3, 0.05), at(-1.0, 0.45), at(-0.3, 0.85));
        }
        MoveIcon::Reroll => {
            // An arc of about 290 degrees from the top, clockwise, with the arrowhead at its end.
            let r = half * 0.8;
            let (start, sweep) = (-std::f32::consts::FRAC_PI_2 + 0.45, 5.0_f32);
            let arc: Vec<Pos2> = (0..=16).map(|k| start + sweep * k as f32 / 16.0).map(|a| Pos2::new(c.x + r * a.cos(), c.y + r * a.sin())).collect();
            p.add(Shape::line(arc, stroke));
            let end = start + sweep;
            let tip = Pos2::new(c.x + r * end.cos(), c.y + r * end.sin());
            // The arrowhead points along the arc's direction at its end (clockwise on screen).
            let dir = Vec2::new(-end.sin(), end.cos());
            let side = Vec2::new(dir.y, -dir.x);
            let len = half * 0.6;
            triangle(tip + dir * len * 0.7, tip - dir * len * 0.3 + side * len * 0.6, tip - dir * len * 0.3 - side * len * 0.6);
        }
        MoveIcon::Take => {
            p.add(Shape::line(vec![at(-0.9, 0.05), at(-0.3, 0.65), at(0.95, -0.7)], Stroke::new(2.0, colour)));
        }
        MoveIcon::Hunt => {
            p.circle_stroke(c, half * 0.62, stroke);
            for (a, b) in [((-1.0, 0.0), (-0.35, 0.0)), ((0.35, 0.0), (1.0, 0.0)), ((0.0, -1.0), (0.0, -0.35)), ((0.0, 0.35), (0.0, 1.0))] {
                p.line_segment([at(a.0, a.1), at(b.0, b.1)], stroke);
            }
            p.circle_filled(c, 1.3, colour);
        }
        MoveIcon::Confirm => {
            p.circle_stroke(c, half, Stroke::new(1.3, colour));
            p.line_segment([at(0.0, -0.6), at(0.0, 0.2)], Stroke::new(1.8, colour));
            p.circle_filled(at(0.0, 0.52), 1.1, colour);
        }
        MoveIcon::EndDay | MoveIcon::Finish => {
            // A crescent opening to the right: between the outer circle's arc and the arc of a circle set to the right
            // through the same two tips (at ±60 degrees). Filled as a mesh of strips (it is not convex), with a thin
            // outline for smooth edges.
            const N: usize = 16;
            let r = half * 0.95;
            let d = 0.45 * r;
            let tip = std::f32::consts::FRAC_PI_3;
            let inner_r = ((r * tip.cos() - d).powi(2) + (r * tip.sin()).powi(2)).sqrt();
            let inner_tip = (r * tip.sin()).atan2(r * tip.cos() - d);
            let arc = |centre: Pos2, radius: f32, from: f32| -> Vec<Pos2> {
                let sweep = std::f32::consts::TAU - 2.0 * from;
                (0..=N).map(|k| -from - sweep * k as f32 / N as f32).map(|a| Pos2::new(centre.x + radius * a.cos(), centre.y + radius * a.sin())).collect()
            };
            let outer = arc(c, r, tip);
            let inner = arc(c + Vec2::new(d, 0.0), inner_r, inner_tip);
            let mut mesh = egui::Mesh::default();
            for (o, i) in outer.iter().zip(&inner) {
                mesh.colored_vertex(*o, colour);
                mesh.colored_vertex(*i, colour);
            }
            for k in 0..N as u32 {
                let (o, i, o2, i2) = (2 * k, 2 * k + 1, 2 * k + 2, 2 * k + 3);
                mesh.add_triangle(o, o2, i);
                mesh.add_triangle(i, o2, i2);
            }
            p.add(Shape::mesh(mesh));
            let mut outline = outer;
            outline.extend(inner.into_iter().rev().skip(1).take(N - 1));
            p.add(Shape::closed_line(outline, Stroke::new(0.8, colour)));
        }
    }
}

/// A made-up Tartarus board for previews (ASCENDANT_PREVIEW_MOVES): `patrol` the destination pick from the
/// Interrogation Room, `manage` the decision on a Magic Mammoth's cell, `pick` an instrument offer with a Re-roll.
fn preview_reading(which: &str) -> crate::app::session::Reading {
    use crate::board::{ChoiceKind, Step};
    let inmate = |beast, tier| CellModel::Inmate { beast, tier, ivory: false, prey: None, event: None };
    let mut m = Model::blank();
    m.day = 3;
    m.cells[1] = CellModel::Empty { event: None };
    m.cells[2] = inmate(Beast::Rat, 0);
    m.cells[3] = CellModel::Sealed { event: None };
    m.cells[4] = inmate(Beast::Wolf, 2);
    m.cells[6] = CellModel::Inmate { beast: Beast::Lion, tier: 1, ivory: false, prey: None, event: Some(EventMark::Unknown) };
    m.cells[9] = inmate(Beast::Rat, 1);
    m.cells[10] = inmate(Beast::Mammoth, 1);
    m.cells[11] = inmate(Beast::Wolf, 0);
    m.cells[12] = CellModel::Empty { event: None };
    let screen = match which {
        "manage" => {
            m.step = Step::Manage;
            m.warden = 10;
            Screen::Manage
        }
        "pick" => {
            m.step = Step::Choice;
            m.warden = 4;
            m.choice.kind = ChoiceKind::Tool;
            m.choice.options = Tool::ALL.iter().copied().take(3).collect();
            m.choice.rerolls = 1;
            Screen::InstrumentPick
        }
        _ => {
            m.step = Step::Patrol;
            m.warden = 0;
            m.dest = vec![10, 3, 6];
            Screen::Patrol
        }
    };
    crate::app::session::Reading { model: m, warnings: Vec::new(), screen, days_left: Some(12), at: SystemTime::now(), eater_inferred: false }
}

/// The minimum number of playouts at which the advice can be "shown steadily" during simulation (the same as
/// `SHOW_MIN` in session.rs, which is a private constant there; change both together).
const SHOW_MIN_SHOWN: usize = 500;

/// The reason written in the small text of the advice card when the advice is not ranked 1st. By `shown_advice`
/// in session.rs, the advice on screen has only two sources (read on 2026-11-05):
/// - Settled (simulation finished, or in a Cage Fight run past SHOW_MIN playouts with the same first rank twice in
///   a row and clear of the others beyond the error): `advice()` is the simulation's first rank, then adjusted
///   by keep_shown (keep the earlier one on show), prefer_bite, keep_to_chain and prefer_change; these adjustments only swap in rows
///   within the error of it (also_good), except for the Tartarus cage fight's chain plan (which has its own
///   tolerance).
/// - Not yet settled (simulating): the advice settled earlier that still holds on this screen (held), or the
///   first bite settled on the manage screen (manage_bite); if neither, no advice is shown. The strategy's own
///   choice is used only when fewer than LIVE_MIN playouts have run, when it is not yet settled and not shown,
///   so no "provisional strategy" is written.
///
/// So: nothing is written when there is no advice (`shown` is None), when the advice is the 1st rank, or when
/// there are fewer than two ranks; during simulation with fewer than SHOW_MIN playouts it must be the advice
/// settled earlier (`Simulating · the settled advice is kept`); when the gap is within the error it writes
/// "Within error of #1" (a numeric fact, whichever rule chose it); while still simulating with a gap beyond the
/// error it can only be the advice settled earlier; in every other case (simulation finished yet the gap is
/// beyond the error, for example the cage fight's chain plan) the reason cannot be told and nothing is written.
fn advice_reason(ranking: &[SolveOption], shown: Option<Option<usize>>, solving: bool, rounds: usize, early: bool) -> Option<&'static str> {
    let option = shown?;
    if ranking.len() < 2 || option == Some(0) {
        return None;
    }
    let kept = tr("Simulating \u{b7} the settled advice is kept");
    // Without early showing (`Session::races`), any advice on show while simulating is the one settled earlier.
    if solving && (rounds < SHOW_MIN_SHOWN || !early) {
        return Some(kept);
    }
    match option {
        Some(k) if k < ranking.len() && within_error(ranking[k].mean - ranking[0].mean, ranking[k].gap_se) => Some(tr("Within error of #1")),
        _ if solving => Some(kept),
        _ => None,
    }
}

/// The gap `gap` to the first rank is within the simulation error: |gap| ≤ 2× the paired standard error
/// `gap_se` (the same as "Either is fine" of [`verdict`]).
/// When the standard error is not finite (simulation has not reached two playout rounds) it cannot be told, and
/// it is also treated as about the same (grey).
fn within_error(gap: f64, gap_se: f64) -> bool {
    !gap_se.is_finite() || gap.abs() <= 2.0 * gap_se
}

/// Advice card: large advice text (truncated when too long), the conclusion label on the right, and one line of
/// small text below (objective and expected loot).
/// The height is fixed: with no advice the large text stays `—` or "Solving...", and with no label or small
/// text the space is left blank, so the top three and the board below do not jump up and down with solving.
/// Returns the card's rect, on which the tooltip hangs.
/// Returns the card's rect and whether any text was truncated (when truncated the caller makes the card
/// clickable so the full text can be read).
fn advice_card(ui: &mut egui::Ui, pick: Option<&str>, line: &str, line_colour: Color32, verdict: Option<Verdict>, solving: bool) -> (Rect, bool) {
    let mut cut = false;
    let big = match pick {
        Some(pick) => RichText::new(pick).size(ADVICE_TEXT).strong().color(egui::Color32::WHITE),
        None if solving => RichText::new(tr("Solving...")).size(ADVICE_TEXT).color(MUTED),
        None => RichText::new("\u{2014}").size(ADVICE_TEXT).color(MUTED),
    };
    let rect = theme::card_frame()
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.horizontal(|ui| {
                let spacing = ui.spacing().item_spacing.x;
                let tag_w = verdict.map_or(0.0, |v| {
                    egui::WidgetText::from(RichText::new(v.text()).small()).into_galley(ui, None, f32::INFINITY, egui::TextStyle::Small).size().x + 10.0 + spacing
                });
                let width = (ui.available_width() - tag_w).max(20.0);
                cut |= ui.painter().layout_no_wrap(big.text().to_string(), FontId::proportional(ADVICE_TEXT), Color32::WHITE).size().x > width;
                ui.allocate_ui_with_layout(Vec2::new(width, 22.0), Layout::left_to_right(Align::Center), |ui| {
                    ui.set_min_width(width);
                    // The full text is in the card's tooltip (attached by the caller), so no tooltip of its own
                    // is made here, to avoid stacking two.
                    // Not selectable: a Label is selectable by default and would take clicks, which blocks the
                    // game under click-through.
                    ui.add(egui::Label::new(big).truncate().show_tooltip_when_elided(false).selectable(false));
                });
                if let Some(v) = verdict {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        theme::tag(ui, v);
                    });
                }
            });
            let line = if line.is_empty() { " " } else { line };
            // The small text is fixed at two lines high and wraps automatically; it is truncated only if it
            // does not fit in two lines (the full text is in the card's tooltip).
            cut |= theme::elided(ui, line, ui.available_width(), 2);
            theme::lines(ui, line, line_colour, 2);
        })
        .response
        .rect;
    (rect, cut)
}

/// A settlement's amount in its game's units: Tartarus Chromatics with two decimals, Vorax activity as `812k`.
fn amount(kind: RunKind, v: f64) -> String {
    match kind {
        RunKind::Tartarus => format!("{v:.2}"),
        RunKind::Vorax => k_amount(v),
    }
}

/// [`amount`] with its sign (luck).
fn signed(kind: RunKind, v: f64) -> String {
    match kind {
        RunKind::Tartarus => format!("{v:+.2}"),
        RunKind::Vorax if v < 0.0 => format!("-{}", k_amount(-v)),
        RunKind::Vorax => format!("+{}", k_amount(v)),
    }
}

/// An amount with its unit: `0.52 Chromatics` (`0.52 綺彩`) or `12k activity` (`12k 活性`).
fn with_unit(kind: RunKind, text: String) -> String {
    match kind {
        RunKind::Tartarus => trf!("{} Chromatics", text),
        RunKind::Vorax => trf!("{} activity", text),
    }
}

/// Activity in thousands: 812345 -> `812k`, 1234 -> `1.2k`, 850 -> `850`, 1250000 -> `1.25M`.
fn k_amount(v: f64) -> String {
    let a = v.abs();
    if a >= 1e6 {
        format!("{:.2}M", v / 1e6)
    } else if a >= 1e4 {
        format!("{:.0}k", v / 1e3)
    } else if a >= 1e3 {
        format!("{:.1}k", v / 1e3)
    } else {
        format!("{v:.0}")
    }
}

/// The two number cards of the Run tab: "Vaulted" (large text the vaulted count, small text the actual
/// result including the board) and "Expected" (large text the expected loot at the start, small text the standard
/// deviation of a single run). With no settlement they show `—` and the height does not change.
/// Vorax (`kind`): the first card is "Final activity" (counted up to 840k), in activity as `812k`.
fn stat_cards(ui: &mut egui::Ui, s: Option<&Settled>, kind: RunKind) {
    let dash = || "\u{2014}".to_string();
    let target = k_amount(::vorax::value::TARGET as f64);
    let (title, vaulted, actual, hover) = match kind {
        RunKind::Tartarus => (
            tr("Vaulted"),
            s.and_then(|s| s.vaulted).map_or_else(dash, |v| v.to_string()),
            s.and_then(|s| s.actual).map_or_else(|| " ".to_string(), |a| trf!("Actual {}", format!("{a:.2}"))),
            tr("Chromatic Prisoners vaulted this run; the actual result adds the last board's loot and objective cache").to_string(),
        ),
        RunKind::Vorax => (
            tr("Final activity"),
            s.and_then(|s| s.actual).map_or_else(dash, k_amount),
            trf!("Counted up to {}", target),
            trf!("The board's final activity this game, counted up to {} (the target)", target),
        ),
    };
    let expected = s.and_then(|s| s.expected);
    let mean = expected.map_or_else(dash, |(e, _)| amount(kind, e));
    let sd = expected.map_or_else(|| " ".to_string(), |(_, sd)| format!("\u{b1} {}", amount(kind, sd)));
    let expected_hover = match kind {
        RunKind::Tartarus => tr("Expected loot at the run's start (the first advice), and the spread of one run"),
        RunKind::Vorax => tr("Expected final activity at the game's start (the best opening's average), and the spread of one game"),
    };
    // Align the top edges of the two cards (horizontal centres vertically by default, which would misalign
    // cards of different heights).
    ui.with_layout(Layout::left_to_right(Align::Min), |ui| {
        let spacing = 6.0;
        ui.spacing_mut().item_spacing.x = spacing;
        let width = ((ui.available_width() - spacing) / 2.0).max(40.0);
        stat_card(ui, width, title, &vaulted, &actual).on_hover_text(hover);
        stat_card(ui, width, tr("Expected"), &mean, &sd).on_hover_text(expected_hover);
    });
}

/// One number card: a small heading, large text, and a line of small text (truncated when too long).
fn stat_card(ui: &mut egui::Ui, width: f32, title: &str, big: &str, small: &str) -> egui::Response {
    ui.allocate_ui_with_layout(Vec2::new(width, 0.0), Layout::top_down(Align::Min), |ui| {
        theme::group_frame().show(ui, |ui| {
            // The frame's left and right inner margin 8×2 plus 1 for the stroke.
            ui.set_width((width - 18.0).max(10.0));
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.add(egui::Label::new(RichText::new(title).small().color(MUTED)).truncate());
            ui.add(egui::Label::new(RichText::new(big).size(20.0).strong().color(Color32::WHITE)).truncate());
            ui.add(egui::Label::new(RichText::new(small).small().color(MUTED)).truncate());
        })
        .response
    })
    .inner
}

/// Content of the settlement card: large text "Within the expected range", "Luck ran high / low this run" or "Decisions lost points this run", and below it the decision
/// loss and luck, and how many standard deviations off.
fn verdict_lines(ui: &mut egui::Ui, s: &Settled) {
    let head = match s.cause {
        Some(Cause::Within) => tr("Within the expected range"),
        Some(Cause::Luck { high: true }) => tr("Luck ran high this run"),
        Some(Cause::Luck { high: false }) => tr("Luck ran low this run"),
        Some(Cause::Decisions) => tr("Decisions lost points this run"),
        None => tr("Luck and decisions can't be split"),
    };
    let colour = if s.cause == Some(Cause::Decisions) { theme::WARN } else { Color32::WHITE };
    let r = ui.add(egui::Label::new(RichText::new(head).size(16.0).strong().color(colour)).wrap());
    let kind = s.kind;
    let luck = s.luck.map_or_else(|| "\u{2014}".to_string(), |l| with_unit(kind, signed(kind, l)));
    ui.add(egui::Label::new(RichText::new(trf!("Decision loss {} \u{b7} luck {}", with_unit(kind, amount(kind, s.loss)), luck)).small().color(theme::TEXT)).wrap());
    if let Some(z) = s.z {
        ui.add(egui::Label::new(RichText::new(trf!("{} standard deviations from expected", format!("{z:+.2}"))).small().color(MUTED)).wrap());
    }
    // Tartarus: the expectation plus the advice's usual gain over the playouts, what the verdict is judged on.
    if let (Some(e), Some(z)) = (s.expected_net, s.z_net) {
        ui.add(egui::Label::new(RichText::new(trf!("Expected with the usual gain {}: {} standard deviations", with_unit(kind, amount(kind, e)), format!("{z:+.2}"))).small().color(MUTED)).wrap());
    }
    // The loss is the move made against the re-solved best: the advice of the moment's own gaps count too.
    if s.cause == Some(Cause::Decisions) {
        ui.add(egui::Label::new(RichText::new(tr("Includes gaps between the advice then and the re-solve: not always a mistake")).small().color(MUTED)).wrap());
    }
    // Vorax: reaching 840k, its chance at the start and whether the game reached it.
    if let Some(reach) = s.reach {
        let did = if reach.unsure {
            tr("unsure")
        } else if reach.reached {
            tr("reached")
        } else {
            tr("not reached")
        };
        let line = trf!("Reach {}: {}% at the start, actually {}", k_amount(reach.target as f64), format!("{:.0}", reach.chance * 100.0), did);
        // Same colours as the Vorax result card: unsure is a warning, not a plain miss.
        let colour = if reach.unsure {
            theme::WARN
        } else if reach.reached {
            theme::GOOD
        } else {
            theme::TEXT
        };
        ui.add(egui::Label::new(RichText::new(line).small().color(colour)).wrap());
    }
    let (noise, n) = s.noise;
    let mut hover = tr("Luck = actual - expected + decision loss. Decision loss: the best move's mean minus the mean of the move made, re-solved on the logged board; a gap within twice its paired standard error is noise and not counted.").to_string();
    if n > 0 {
        hover.push_str(&trf!("\nWithin the noise: {} in {} moves", amount(kind, noise), n));
    }
    hover.push_str(tr("\nThe decision loss compares the move made with the re-solved best, so it also counts where Ascendant's advice at the time differed from the re-solve (such as advice that came late under heavy load): it is not always the player's mistake."));
    r.on_hover_text(hover);
}

/// The icons of the riot row ([`riot_row`]).
#[derive(Clone, Copy)]
enum RiotIcon {
    /// The riot battle was fought: two crossed swords.
    Fought,
    /// The riot was abandoned: a ring with a slash.
    Abandoned,
    /// The special boss (the Tartarus warden): a helmet with a barred visor and an eye slit (2026-11-10, the player's
    /// pick of five; a horned head before looked like a goblin); green when killed, the warning colour when it got
    /// away, dim when none came.
    Boss { came: bool, killed: bool },
    /// A chest: red, colour (three stripes) or medium (wood).
    Chest(ChestKind),
}

#[derive(Clone, Copy)]
enum ChestKind {
    Red,
    Color,
    Medium,
}

/// Draws one riot icon in a 16 x 14 box, with its tooltip.
fn riot_icon(ui: &mut egui::Ui, icon: RiotIcon, tip: &str) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(16.0, 14.0), Sense::hover());
    let p = ui.painter();
    let c = rect.center();
    match icon {
        RiotIcon::Fought => {
            let stroke = Stroke::new(1.6, theme::TEXT);
            for (from, to) in [(Vec2::new(-5.5, 5.5), Vec2::new(5.0, -5.0)), (Vec2::new(5.5, 5.5), Vec2::new(-5.0, -5.0))] {
                p.line_segment([c + from, c + to], stroke);
                // The guard, across the blade near the hilt.
                let along = (to - from).normalized();
                let guard = c + from + along * 3.0;
                let across = Vec2::new(-along.y, along.x) * 2.5;
                p.line_segment([guard - across, guard + across], stroke);
            }
        }
        RiotIcon::Abandoned => {
            let stroke = Stroke::new(1.5, MUTED);
            p.circle_stroke(c, 5.5, stroke);
            p.line_segment([c + Vec2::new(-3.9, 3.9), c + Vec2::new(3.9, -3.9)], stroke);
        }
        RiotIcon::Boss { came, killed } => {
            let colour = match (came, killed) {
                (false, _) => Color32::from_gray(80),
                (true, true) => theme::GOOD,
                (true, false) => theme::WARN,
            };
            // A dome over the face plate; on the plate a dark eye slit, three short visor bars under it, and the brow.
            let hole = theme::BG;
            let (top, r) = (c + Vec2::new(0.0, -0.5), 6.0);
            let dome: Vec<Pos2> = (0..=12).map(|i| top + r * Vec2::angled(std::f32::consts::PI * (1.0 + i as f32 / 12.0))).collect();
            p.add(Shape::convex_polygon(dome, colour, Stroke::NONE));
            p.rect_filled(Rect::from_min_max(Pos2::new(top.x - r, top.y), Pos2::new(top.x + r, top.y + 7.0)), 1.5, colour);
            p.line_segment([Pos2::new(top.x - r + 1.2, top.y + 2.4), Pos2::new(top.x + r - 1.2, top.y + 2.4)], Stroke::new(2.0, hole));
            for dx in [-0.5 * r, 0.0, 0.5 * r] {
                p.line_segment([Pos2::new(top.x + dx, top.y + 4.0), Pos2::new(top.x + dx, top.y + 6.0)], Stroke::new(1.2, hole));
            }
            p.line_segment([Pos2::new(top.x - r, top.y), Pos2::new(top.x + r, top.y)], Stroke::new(1.0, hole));
        }
        RiotIcon::Chest(kind) => {
            let body = Rect::from_center_size(c + Vec2::new(0.0, 1.5), Vec2::new(14.0, 9.0));
            let lid = Rect::from_min_max(body.min - Vec2::new(0.0, 3.0), Pos2::new(body.max.x, body.min.y + 1.0));
            let wood = Color32::from_rgb(170, 125, 70);
            match kind {
                ChestKind::Red => {
                    p.rect_filled(lid, 2.0, tier_colour(4));
                    p.rect_filled(body, 1.0, tier_colour(4).gamma_multiply(0.8));
                }
                ChestKind::Color => {
                    p.rect_filled(lid, 2.0, tier_colour(5));
                    let w = body.width() / 3.0;
                    for (k, colour) in [tier_colour(5), tier_colour(1), tier_colour(3)].into_iter().enumerate() {
                        let x = body.min.x + k as f32 * w;
                        p.rect_filled(Rect::from_min_max(Pos2::new(x, body.min.y), Pos2::new(x + w, body.max.y)), 0.0, colour);
                    }
                }
                ChestKind::Medium => {
                    p.rect_filled(lid, 2.0, wood);
                    p.rect_filled(body, 1.0, wood.gamma_multiply(0.8));
                }
            }
            p.rect_stroke(lid.union(body), 1.5, Stroke::new(1.0, Color32::from_gray(30)), StrokeKind::Inside);
            p.rect_filled(Rect::from_center_size(Pos2::new(c.x, body.min.y + 1.0), Vec2::new(3.0, 3.5)), 0.5, Color32::from_rgb(235, 205, 90));
        }
    }
    response.on_hover_text(tip)
}

/// The riot battle after a Tartarus run (the log's riot row, see [`settle::riot_of`]) as one row of icons: fought or
/// abandoned, the special boss, and each kind of chest with its count (counted by the battle's drop lines, see
/// `gamelog::chest_kind`), every icon with its name on hover. Nothing until the battle is over (it ends about a
/// minute after the run, the row then appears), and nothing for a run logged before the row existed. It wraps in a
/// narrow window.
fn riot_row(ui: &mut egui::Ui, s: &Settled) {
    let Some(r) = s.riot.filter(|_| s.kind == RunKind::Tartarus) else { return };
    ui.add_space(3.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 3.0;
        ui.label(RichText::new(tr("Riot")).small().color(MUTED))
            .on_hover_text(tr("The riot battle after the run, from the game's log: the chests of the inmates, counted by the drop lines at the battle's end. The board's task chest and the special boss's chest are not counted."));
        if !r.fought {
            riot_icon(ui, RiotIcon::Abandoned, tr("Riot: abandoned"));
            ui.label(RichText::new(tr("abandoned")).small().color(MUTED));
            return;
        }
        riot_icon(ui, RiotIcon::Fought, tr("Riot: fought"));
        let boss = match (r.boss, r.boss_killed) {
            (false, _) => tr("No special boss"),
            (true, true) => tr("Special boss: came and was killed (its boss chest is not counted as a colour chest)"),
            (true, false) => tr("Special boss: came, not killed"),
        };
        riot_icon(ui, RiotIcon::Boss { came: r.boss, killed: r.boss_killed }, boss);
        ui.add_space(4.0);
        let chests = [
            (ChestKind::Color, r.color, trf!("Colour chests: {} (the task's chest and the boss chest not counted)", r.color)),
            (ChestKind::Red, r.red, trf!("Red chests: {} (the task's chest not counted)", r.red)),
            (ChestKind::Medium, r.medium, trf!("Medium chests: {} (the warden's hoards below red)", r.medium)),
        ];
        for (kind, n, tip) in chests {
            riot_icon(ui, RiotIcon::Chest(kind), &tip);
            ui.label(RichText::new(n.to_string()).small().color(theme::TEXT)).on_hover_text(&tip);
            ui.add_space(3.0);
        }
    });
}

/// The "Largest step": which day and screen, what was actually picked, the re-solved best, the advice at the
/// time, how big the difference is, and whether it is within the error;
/// the last line of small text says how many steps were re-solved, how many playouts per option, and how many
/// steps could not tell what was picked.
fn largest_step(ui: &mut egui::Ui, s: &Settled) {
    theme::heading(ui, tr("Largest step"));
    theme::group_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 2.0;
        match &s.worst {
            Some(w) => {
                let when = match s.kind {
                    RunKind::Tartarus => trf!("Day {} \u{b7} {}", w.day, w.screen),
                    RunKind::Vorax => trf!("Round {} \u{b7} {}", w.day, w.screen),
                };
                ui.label(RichText::new(when).small().color(MUTED));
                // The name column has a fixed narrow width and the option name uses the remaining width
                // (truncated when too long, hover to read the full text).
                let name_w = if lang::zh() { 56.0 } else { 52.0 };
                for (name, text) in [(tr("Made"), &w.made), (tr("Best"), &w.best), (tr("Advised"), &w.advice)] {
                    ui.horizontal(|ui| {
                        let h = ui.spacing().interact_size.y;
                        ui.allocate_ui_with_layout(Vec2::new(name_w, h), Layout::left_to_right(Align::Center), |ui| {
                            ui.set_width(name_w);
                            ui.add(egui::Label::new(RichText::new(name).small().color(MUTED)).truncate());
                        });
                        ui.add(egui::Label::new(RichText::new(text.as_str()).color(theme::TEXT)).truncate().show_tooltip_when_elided(false)).on_hover_text(text.as_str());
                    });
                }
                let (word, colour) = if w.noise { (tr("within the noise"), MUTED) } else { (tr("beyond the noise"), theme::WARN) };
                let (line, gap) = match s.kind {
                    RunKind::Tartarus => (trf!("{} less expected loot \u{b7} {}", format!("{:.3}", w.loss), word), (format!("{:.3}", w.loss), format!("{:.3}", w.gap_se))),
                    RunKind::Vorax => (trf!("{} less expected activity \u{b7} {}", k_amount(w.loss), word), (k_amount(w.loss), k_amount(w.gap_se))),
                };
                ui.add(egui::Label::new(RichText::new(line).small().color(colour)).wrap()).on_hover_text(trf!("Gap {} \u{b1} {} (paired standard error)", gap.0, gap.1));
            }
            None => {
                let none = if s.kind == RunKind::Vorax { tr("No move lost expected activity") } else { tr("No move lost expected loot") };
                ui.label(RichText::new(none).color(theme::GOOD));
            }
        }
        let line = trf!("{} moves re-solved, {} playouts per option; {} not told from the log", s.scored, s.rollouts, s.unknown);
        ui.add(egui::Label::new(RichText::new(line).small().color(MUTED)).wrap());
    });
}

/// One row of the settings page: the name on the left (at most half the width, truncated when too long, hover
/// to read the full text), the controls on the right (laid out right to left, using the remaining width). At a
/// window width of 200 this keeps the name from being covered by the controls.
fn row<R>(ui: &mut egui::Ui, label: &str, control: impl FnOnce(&mut egui::Ui) -> R) -> R {
    row_with(ui, label, 0.0, control)
}

/// Like [`row`], but at least `keep` is left on the right for the controls: the name column gives up width
/// first (a truncated name can be hovered to read the full text).
fn row_with<R>(ui: &mut egui::Ui, label: &str, keep: f32, control: impl FnOnce(&mut egui::Ui) -> R) -> R {
    ui.horizontal(|ui| {
        let h = ui.spacing().interact_size.y;
        let full = ui.available_width();
        let width = (full * 0.48).min(140.0).min(full - keep - ui.spacing().item_spacing.x).max(24.0);
        ui.allocate_ui_with_layout(Vec2::new(width, h), Layout::left_to_right(Align::Center), |ui| {
            ui.set_width(width);
            ui.add(egui::Label::new(label).truncate());
        });
        ui.with_layout(Layout::right_to_left(Align::Center), control).inner
    })
    .inner
}

/// Whether a click in the game while the mark shows `action` on `screen` hides the mark until a reading changes:
/// not for picking an instrument or a treasure card, which only selects it (2026-11-07, the mark went 6.8 s before
/// "Confirm"); every other move (a reroll on those screens too) is played by its click.
fn click_hides(screen: Screen, action: &Action) -> bool {
    !(matches!(screen, Screen::InstrumentPick | Screen::TreasurePick) && matches!(action, Action::Choose { .. }))
}

/// How long after a click hid the mark the game's log may take to say the move was played before the mark comes back
/// ([`click_unconfirmed`]). Measured on the six runs of 2026-11-07 17:53 to 18:20 UTC (the mark hidden on the
/// release, as now): 109 of 113 hides had the move logged within 0.11 s (median 0.05 s); the other four came 0.49,
/// 0.51, 1.6 and 1.8 s on, likely a second click. On 513 earlier runs, hidden on the press, 694 of 722 within 0.5 s.
/// 0.6 s is over five times the usual worst; a move logged later still hides the mark (`played`).
const CONFIRM_WAIT: Duration = Duration::from_millis(600);
/// A move logged this long before the click was taken (the look comes up to 50 ms after the release, the log 30 to
/// 130 ms after the game) still confirms it, when it came after the screen clicked on was read (so not the move before).
const CONFIRM_EARLY: Duration = Duration::from_millis(300);

/// The game's log lines that say a move was played by a click on the advice's target: a door (`Moved`), a decision
/// card (`Decision`, `Consume`), a cell (`CellPicked`, `Bite`), the chain's End (`ChainEnd`) or a re-roll (`Reroll`).
fn confirms_click(signal: super::gamelog::Signal) -> bool {
    use super::gamelog::Signal;
    matches!(signal, Signal::Moved | Signal::Decision | Signal::Consume | Signal::CellPicked | Signal::Bite | Signal::ChainEnd | Signal::Reroll)
}

/// Whether a click that hid the mark at `clicked` on `screen`, whose reading was read at `read`, was not played: the
/// game's log is followed (`log_live`, its light green), the screen logs its moves ([`confirms_click`]; on 2026-11-07
/// every Patrol, Manage, Interrogation, cell pick, Consume target and instrument re-roll click the game played logged
/// one), none was logged since (`move_seen`: after the reading, and at most [`CONFIRM_EARLY`] before the click), and
/// [`CONFIRM_WAIT`] has gone by. The other screens (the treasure pick, popups, the theme scroll, prison status) log no
/// such line for their clicks, or not one known to come every time: their mark stays hidden as before.
/// Accepted as they are (the 2026-11-08 review): a move logged later than CONFIRM_WAIT (4 of 113 hides on 2026-11-07,
/// likely a second click) brings the mark back for a moment until it comes (`played` then hides it); and a click on
/// the old box in the one frame the advice changes still counts for the new advice's screen.
fn click_unconfirmed(screen: Screen, (clicked, read): (Instant, Instant), move_seen: Option<Instant>, now: Instant, log_live: bool) -> bool {
    let logs_moves = matches!(screen, Screen::Patrol | Screen::Manage | Screen::Interrogation | Screen::CellPick | Screen::ChainTarget | Screen::InstrumentPick);
    let confirmed = move_seen.is_some_and(|seen| seen >= read && seen + CONFIRM_EARLY >= clicked);
    log_live && logs_moves && !confirmed && now.saturating_duration_since(clicked) >= CONFIRM_WAIT
}

/// Whether a click (screen px, see [`mark::Clicks`]) is on the mark: pressed, held and released all in the game's
/// rect `game` and on the same one of the target read, `read`, and the spot the box holds, `drawn` (it keeps where it
/// was first marked, see [`Mark::spot`]), with the margin of [`mark::hit_rect`]. The box around every position seen
/// while held lying in it, so does every position. Both spots are `[x, y, half_w, half_h, unit]` in screen px.
fn on_mark(click: mark::Click, game: [i32; 4], read: [f32; 5], drawn: Option<[f32; 5]>) -> bool {
    let on = |spot: [f32; 5], p: (i32, i32)| mark::inside(game, p) && mark::inside(mark::hit_rect(spot), p);
    [Some(read), drawn].into_iter().flatten().any(|spot| [click.min, click.max, click.down, click.up].into_iter().all(|p| on(spot, p)))
}

/// The expected loot of the move at `option` in the session's ranking ([`Session::loot`]: the day's first bite left
/// alone takes the manage screen's value) as the window writes it, `6.74`: the board's advice line, the top three and
/// the value on the game; None without a value (the window then writes a dash).
fn loot_text(session: &Session, option: Option<usize>) -> Option<String> {
    session.loot(option).map(|mean| format!("{mean:.2}"))
}

/// A move's label without its bracketed details, which the board marks show.
fn short(label: &str) -> &str {
    label.split_once(" (").map_or(label, |(name, _)| name)
}

/// A move's label as the window shows it: a Consume names only its cell and
/// a swap is just "Replace".
fn move_name(action: &Action, label: &str) -> String {
    match action {
        Action::Devour { to } => trf!("Consume {}", cell_label(*to)),
        Action::Swap => tr("Replace").to_string(),
        _ => label.to_string(),
    }
}

/// A rarity's column head in the correction window: its first three
/// letters, or the first character of a Chinese name.
fn tier_short(tier: i32) -> String {
    let name = tier_name(tier);
    let n = if name.is_ascii() { 3 } else { 1 };
    name.chars().take(n).collect()
}

/// The model cell a move acts on: the cell patrolled, devoured or picked,
/// or the Warden's own for a swap.
fn action_cell(action: &Action, warden: i64) -> Option<usize> {
    match action {
        Action::Patrol { cell: n } | Action::Devour { to: n } | Action::Chain { to: n } | Action::Choose { id: ChoiceId::Cell(n) } => Some(*n),
        Action::Swap => usize::try_from(warden).ok(),
        _ => None,
    }
}

/// The file of the affixes learned from the game log, in the app's folder.
fn learned_path() -> Option<PathBuf> {
    settings::app_dir().map(|d| d.join("affix-learned.tsv"))
}

/// Small icons on the navigation rail tab buttons.
#[derive(Clone, Copy)]
enum Icon {
    /// Board: the cell ring (small squares around the edge, empty in the middle).
    Board,
    /// Run: a flag.
    Run,
    /// Settings: a gear (until 2026-11-10 three sliders, which looked too much like the Affixes' list right above).
    Settings,
    /// Affixes: a map folded in three (until 2026-11-10 a two-line list, too close to the Settings' sliders).
    Affix,
}

/// A tab button of the navigation rail: primary-colour background and white icon when selected; a slightly
/// lighter background on hover. When `badge` is greater than 0, a yellow dot with a number is drawn at the top
/// right (the Board's warning count).
fn rail_button(ui: &mut egui::Ui, size: Vec2, icon: Icon, selected: bool, badge: usize) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, 6.0, ACCENT);
    } else if response.hovered() {
        p.rect_filled(rect, 6.0, Color32::from_rgb(0x2a, 0x2f, 0x3a));
    }
    let colour = if selected || response.hovered() { Color32::WHITE } else { MUTED };
    let c = rect.center();
    let stroke = Stroke::new(1.3, colour);
    match icon {
        Icon::Board => {
            let (cell, gap) = (3.0, 1.2);
            let step = cell + gap;
            let origin = c - Vec2::splat(2.0 * step - gap / 2.0);
            for n in 0..16 {
                let (col, row) = ring(n);
                // The small icon is drawn only 4×4: squeeze 6 columns into 4 (columns 0 and 5 are the edges,
                // take 1 and 4 in the middle).
                let col = match col {
                    0 => 0,
                    5 => 3,
                    1 | 2 => 1,
                    _ => 2,
                };
                let at = origin + Vec2::new(col as f32 * step, row as f32 * step);
                p.rect_filled(Rect::from_min_size(at, Vec2::splat(cell)), 0.8, colour);
            }
        }
        Icon::Run => {
            let pole = c.x - 4.5;
            p.line_segment([Pos2::new(pole, c.y - 7.0), Pos2::new(pole, c.y + 7.0)], stroke);
            let flag = vec![Pos2::new(pole + 0.5, c.y - 7.0), Pos2::new(pole + 9.5, c.y - 4.5), Pos2::new(pole + 0.5, c.y - 1.5)];
            p.add(Shape::convex_polygon(flag, colour, Stroke::NONE));
        }
        Icon::Settings => {
            // Eight teeth: the outline goes root, tip, tip, root around each tooth; a round hole in the middle.
            let teeth = 8;
            let (tip, root) = (7.2, 5.2);
            let step = std::f32::consts::TAU / teeth as f32;
            let half = step / 4.0;
            let mut outline = Vec::with_capacity(teeth * 4);
            for k in 0..teeth {
                let a = k as f32 * step;
                for (da, r) in [(-half * 1.1, root), (-half * 0.55, tip), (half * 0.55, tip), (half * 1.1, root)] {
                    outline.push(c + r * Vec2::angled(a + da));
                }
            }
            p.add(Shape::closed_line(outline, stroke));
            p.circle_stroke(c, 2.2, stroke);
        }
        Icon::Affix => {
            // A map folded in three: zigzag top and bottom edges and the two folds.
            let (x1, x2) = (c.x - 2.7, c.x + 2.7);
            let outline = vec![
                Pos2::new(c.x - 8.0, c.y - 5.0),
                Pos2::new(x1, c.y - 7.0),
                Pos2::new(x2, c.y - 5.0),
                Pos2::new(c.x + 8.0, c.y - 7.0),
                Pos2::new(c.x + 8.0, c.y + 5.0),
                Pos2::new(x2, c.y + 7.0),
                Pos2::new(x1, c.y + 5.0),
                Pos2::new(c.x - 8.0, c.y + 7.0),
            ];
            p.add(Shape::closed_line(outline, stroke));
            let fold = Stroke::new(1.0, colour);
            p.line_segment([Pos2::new(x1, c.y - 7.0), Pos2::new(x1, c.y + 5.0)], fold);
            p.line_segment([Pos2::new(x2, c.y - 5.0), Pos2::new(x2, c.y + 7.0)], fold);
        }
    }
    if badge > 0 {
        // The badge text is at least 11: the circle grows with it to a radius of 7.
        let at = rect.right_top() + Vec2::new(-3.0, 4.0);
        p.circle_filled(at, 7.0, theme::WARN);
        p.text(at, Align2::CENTER_CENTER, badge.min(9).to_string(), FontId::proportional(MIN_TEXT), Color32::BLACK);
    }
    response
}

/// The Read button: a circular arrow.
fn read_button(ui: &mut egui::Ui, size: Vec2) -> egui::Response {
    let response = ui.add_sized(size, egui::Button::new(""));
    let colour = ui.style().interact(&response).fg_stroke.color;
    let (c, r) = (response.rect.center(), 5.5);
    let at = |a: f32| c + r * Vec2::angled(a);
    let (from, to) = (30f32.to_radians(), 330f32.to_radians());
    let arc: Vec<Pos2> = (0..=20).map(|k| at(from + (to - from) * k as f32 / 20.0)).collect();
    let p = ui.painter();
    p.add(Shape::line(arc, Stroke::new(1.5, colour)));
    let (end, along, out) = (at(to), Vec2::angled(to + std::f32::consts::FRAC_PI_2), Vec2::angled(to));
    p.add(Shape::convex_polygon(vec![end + along * 3.0, end + out * 3.0, end - out * 3.0], colour, Stroke::NONE));
    response
}

/// The click-through light in the navigation rail, laid out as the Auto button below it: a dot and "Thru" (1.0.4: the
/// English "Through" was cut to "hroug" in the rail; the smallest text size stays, so a shorter word), the
/// dot green while click-through is on and unlit while off.
fn through_button(ui: &mut egui::Ui, size: Vec2, on: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let visuals = ui.style().interact(&response);
    let p = ui.painter();
    let fill = if on { ACCENT.gamma_multiply(0.35) } else { visuals.weak_bg_fill };
    p.rect_filled(rect, 5.0, fill);
    p.rect_stroke(rect, 5.0, if on { Stroke::new(1.0, ACCENT) } else { visuals.bg_stroke }, StrokeKind::Inside);
    p.circle_filled(rect.center_top() + Vec2::new(0.0, 8.5), 3.5, if on { theme::GOOD } else { Color32::from_gray(85) });
    let text = if on { Color32::WHITE } else { visuals.fg_stroke.color };
    p.text(rect.center_bottom() - Vec2::new(0.0, 4.0), Align2::CENTER_BOTTOM, tr("Thru"), FontId::proportional(MIN_TEXT), text);
    response
}

/// The Auto button: a dot and "Auto", green while auto read is on.
/// Changed to a vertical layout in the navigation rail: a dot on top and the small text "Auto" below; when on,
/// the background takes the primary colour and the dot is green.
fn auto_button(ui: &mut egui::Ui, size: Vec2, on: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let visuals = ui.style().interact(&response);
    let p = ui.painter();
    let fill = if on { ACCENT.gamma_multiply(0.35) } else { visuals.weak_bg_fill };
    p.rect_filled(rect, 5.0, fill);
    p.rect_stroke(rect, 5.0, if on { Stroke::new(1.0, ACCENT) } else { visuals.bg_stroke }, StrokeKind::Inside);
    p.circle_filled(rect.center_top() + Vec2::new(0.0, 8.5), 3.5, if on { theme::GOOD } else { Color32::from_gray(85) });
    let text = if on { Color32::WHITE } else { visuals.fg_stroke.color };
    p.text(rect.center_bottom() - Vec2::new(0.0, 4.0), Align2::CENTER_BOTTOM, tr("Auto"), FontId::proportional(MIN_TEXT), text);
    response
}
// Rarity colours as the cells glow: white, blue, purple, amber, red; Chromatic pink.
fn tier_colour(tier: i32) -> Color32 {
    match tier {
        0 => Color32::from_rgb(200, 200, 200),
        1 => Color32::from_rgb(80, 140, 255),
        2 => Color32::from_rgb(180, 100, 255),
        3 => Color32::from_rgb(255, 170, 40),
        4 => Color32::from_rgb(240, 70, 60),
        _ => Color32::from_rgb(255, 120, 220),
    }
}

/// The beast's letter on its tiles; in Chinese the last character of its
/// name ("象" of a Chinese "...猛瑪象"), when the zh table has the name.
fn initial(beast: Beast) -> String {
    let name = beast_name(beast);
    if lang::zh() && !name.is_ascii() {
        return name.chars().last().map(String::from).unwrap_or_default();
    }
    match beast {
        Beast::Mammoth => "M",
        Beast::Lion => "L",
        Beast::Wolf => "W",
        Beast::Rat => "R",
    }
    .to_string()
}

/// Column and row of model cell `n` on the screen's 6x4 ring, clockwise from
/// the Interrogation Room at the top left.
fn ring(n: usize) -> (usize, usize) {
    match n {
        0..=5 => (n, 0),
        6 | 7 => (5, n - 5),
        8..=13 => (13 - n, 3),
        _ => (0, 16 - n),
    }
}

/// What a cell holds, for its tooltip.
fn describe(cell: &CellModel) -> String {
    let inmate = |t: &i64, b: &Beast| inmate_name(*t as i32, *b);
    let marker = |e: &Option<EventMark>| if e.is_some() { trf!(", {} marker", incident_term()) } else { String::new() };
    match cell {
        CellModel::Office => tr("Interrogation Room").to_string(),
        CellModel::Sealed { event } => format!("{}{}", tr("Locked"), marker(event)),
        CellModel::Empty { event } => format!("{}{}", tr("Empty"), marker(event)),
        CellModel::Inmate { beast: b, tier: t, event, .. } => format!("{}{}", inmate(t, b), marker(event)),
        CellModel::Nest { due: Some(n) } => trf!("Nest, due in {} days", n),
        CellModel::Nest { due: None } => tr("Nest, due tomorrow").to_string(),
        CellModel::Scouted { beast: b, tier: t, event } => format!("{}{}", trf!("Scouted: {}", inmate(t, b)), marker(event)),
        CellModel::Scratch { tier: t, event } => format!("{}{}", trf!("Locked, {} behind the door", tier_name(*t as i32)), marker(event)),
    }
}

/// The model cells the board marks: the pick's and those of the other good
/// picks.
struct Marks {
    pick: Option<usize>,
    near: Vec<usize>,
}

/// The board as the screen's ring of tiles, with the reading warnings of a
/// cell marked on it and in its tooltip and the cells of `marks` marked;
/// returns the screen cell clicked and the courtyard inside the ring.
///
/// Minimum cell width 22: at a window width of 200 the content is only about 150, so six cells just fit; the
/// text shrinks with the cell.
fn board(ui: &mut egui::Ui, m: &Model, warned: &[(usize, &str)], marks: &Marks, fit: Option<f32>) -> (Option<u8>, Rect) {
    let (area, w, tile) = ring_area(ui, fit);
    let at = |col: usize, row: usize| Rect::from_min_size(area.min + Vec2::new(col as f32 * (w + GAP), row as f32 * (tile.y + GAP)), tile);
    let court = at(1, 1).union(at(4, 2));
    let radius = (w * 0.14).clamp(3.0, 6.0);
    let mut clicked = None;
    for (n, cell) in m.cells.iter().enumerate().take(16) {
        let (col, row) = ring(n);
        let rect = at(col, row);
        let sense = if n == 0 { Sense::hover() } else { Sense::click() };
        let response = ui.interact(rect, egui::Id::unique(("ascendant-cell", n)), sense);
        let mut tip = format!("{} {}", cell_label(n), describe(cell));
        for (_, text) in warned.iter().filter(|(c, _)| *c == n) {
            tip = format!("{tip}\n{text}");
        }
        let response = response.on_hover_text(if n == 0 { tip } else { format!("{tip}\n{}", tr("Click to correct")) });
        if response.clicked() {
            clicked = Some(n as u8 + 1);
        }
        let p = ui.painter();
        tile_shape(p, rect, radius, cell, n, response.hovered());
        if marks.pick == Some(n) {
            p.rect_stroke(rect, radius, Stroke::new(2.0, ACCENT), StrokeKind::Inside);
        }
        if marks.near.contains(&n) {
            let r = (w * 0.07).clamp(2.0, 3.0);
            p.circle_filled(rect.left_top() + Vec2::splat(r + 3.0), r, ACCENT.gamma_multiply(0.85));
        }
        if warned.iter().any(|(c, _)| *c == n) {
            // The `?` is at least 11: the dot radius is at least 6.5.
            let r = (w * 0.17).clamp(6.5, 8.0);
            let c = rect.left_bottom() + Vec2::new(r + 1.0, -r - 1.0);
            p.circle_filled(c, r, theme::WARN);
            p.text(c, Align2::CENTER_CENTER, "?", FontId::proportional((r * 1.6).max(MIN_TEXT)), Color32::BLACK);
        }
        if m.warden == n as i64 {
            p.rect_stroke(rect, radius, Stroke::new(2.5, WARDEN), StrokeKind::Outside);
        }
    }
    (clicked, court)
}

/// The area the cell ring occupies (display only, Sense::hover), the cell width and cell size: minimum cell
/// width 22, at a window width of 200 the content is only about 150, so six cells just fit. Shared by [`board`]
/// and [`empty_board`], so the ring is the same size with or without a board.
fn ring_area(ui: &mut egui::Ui, fit: Option<f32>) -> (Rect, f32, Vec2) {
    let tile = match fit {
        // Full layout: cell width 22 to 72; height at least 28, so that when a larger font makes the cells
        // narrower, the text in the middle and the 11-point cell number at the bottom right still fit one above
        // the other.
        None => {
            let w = ((ui.available_width() - 5.0 * GAP) / 6.0).clamp(22.0, 72.0);
            Vec2::new(w, (w * 0.8).max(28.0))
        }
        // Compact layout: the width is filled, and the height is `fit` (the available height of the whole ring)
        // divided into four rows, between 28 and 44 (flat rectangles).
        Some(fit) => {
            let w = ((ui.available_width() - 5.0 * GAP) / 6.0).max(22.0);
            Vec2::new(w, ((fit - 3.0 * GAP) / 4.0).clamp(28.0, 44.0))
        }
    };
    let w = tile.x;
    let (area, _) = ui.allocate_exact_size(Vec2::new(6.0 * w + 5.0 * GAP, 4.0 * tile.y + 3.0 * GAP), Sense::hover());
    (area, w, tile)
}

/// The cell ring when no board has been read yet or the run is over: 16 cells drawn as outlines only, with the
/// same position and size as [`board`], so the warning area below does not shrink upward. Display only, cannot
/// be clicked.
fn empty_board(ui: &mut egui::Ui, fit: Option<f32>) -> Rect {
    let (area, w, tile) = ring_area(ui, fit);
    let radius = (w * 0.14).clamp(3.0, 6.0);
    let at = |col: usize, row: usize| Rect::from_min_size(area.min + Vec2::new(col as f32 * (w + GAP), row as f32 * (tile.y + GAP)), tile);
    let p = ui.painter();
    for n in 0..16 {
        let (col, row) = ring(n);
        let rect = at(col, row);
        p.rect_filled(rect, radius, Color32::from_rgb(0x1a, 0x1c, 0x22));
        p.rect_stroke(rect, radius, Stroke::new(1.0, DIVIDER), StrokeKind::Inside);
    }
    at(1, 1).union(at(4, 2))
}

/// A child Ui in the courtyard (it takes no space in the outer layout, see [`egui::Ui::new_child`]): the
/// compact layout draws the top three here.
fn court_ui(ui: &mut egui::Ui, court: Rect) -> egui::Ui {
    ui.new_child(egui::UiBuilder::new().max_rect(court.shrink2(Vec2::new(4.0, 0.0))).layout(Layout::top_down(Align::Min)))
}

/// The compact advice card (one line, see [`theme::advice_line`]): large advice text or "Solving..." / `—`,
/// small text, label.
fn advice_line(ui: &mut egui::Ui, pick: Option<&str>, line: &str, line_colour: Color32, verdict: Option<Verdict>, solving: bool) -> (Rect, bool) {
    let (big, colour) = match pick {
        Some(pick) => (pick.to_string(), Color32::WHITE),
        None if solving => (tr("Solving...").to_string(), MUTED),
        None => ("\u{2014}".to_string(), MUTED),
    };
    theme::advice_line(ui, &big, colour, line, line_colour, verdict, (theme::CARD, theme::CARD))
}

/// The courtyard (the middle of the ring): objective progress and the last-reading mark, centred; while
/// computing, a spinner next to the first line (just the spinner when there is no text). The advice text is
/// written only on the advice card above and is not repeated here.
fn courtyard(ui: &egui::Ui, court: Rect, lines: &[String], solving: bool) {
    let p = ui.painter().with_clip_rect(court);
    let small = egui::TextStyle::Small.resolve(ui.style());
    let spin = (court.height() * 0.35).clamp(10.0, 14.0);
    let wrap = court.width() - 8.0 - spin;
    let galleys: Vec<_> = lines
        .iter()
        .map(|text| {
            let mut job = egui::text::LayoutJob::simple(text.clone(), small.clone(), MUTED, wrap);
            job.halign = Align::Center;
            p.layout_job(job)
        })
        .collect();
    let gap = 2.0;
    let height = if galleys.is_empty() { spin } else { galleys.iter().map(|g| g.rect.height() + gap).sum::<f32>() - gap };
    let mut y = court.center().y - height / 2.0;
    if galleys.is_empty() {
        if solving {
            egui::Spinner::new().size(spin).color(ACCENT).paint_at(ui, Rect::from_center_size(court.center(), Vec2::splat(spin)));
        }
        return;
    }
    for (k, galley) in galleys.into_iter().enumerate() {
        let spinner = solving && k == 0;
        let (size, extra) = (galley.rect.size(), if spinner { spin + 4.0 } else { 0.0 });
        let left = court.center().x - (size.x + extra) / 2.0;
        let pos = Pos2::new(left, y) - galley.rect.min.to_vec2();
        if spinner {
            let r = Rect::from_min_size(Pos2::new(left + size.x + 4.0, y + (size.y - spin) / 2.0), Vec2::splat(spin));
            egui::Spinner::new().size(spin).color(ACCENT).paint_at(ui, r);
        }
        p.galley(pos, galley, MUTED);
        y += size.y + gap;
    }
}

/// One cell: a small rounded square with a uniform colour scheme (an inmate's fill and outline use the rarity
/// colour, as in the game; other cells are grey-blue).
fn tile_shape(p: &egui::Painter, rect: Rect, radius: f32, cell: &CellModel, n: usize, hovered: bool) {
    let big = FontId::proportional((rect.height() * 0.45).max(MIN_TEXT));
    // When the cell number fits (see below), the text in the middle moves up a little so it does not overlap
    // the cell number at the bottom right.
    let numbered = rect.width() >= 24.0 && rect.height() >= 26.0;
    let (fill, edge) = match cell {
        CellModel::Inmate { tier, .. } | CellModel::Scouted { tier, .. } | CellModel::Scratch { tier, .. } => {
            let c = tier_colour(*tier as i32);
            (c.gamma_multiply(0.3), c.gamma_multiply(0.9))
        }
        CellModel::Office => (theme::CARD, BAR_IDLE),
        CellModel::Empty { .. } => (Color32::from_rgb(0x23, 0x26, 0x2e), DIVIDER),
        _ => (Color32::from_rgb(0x1a, 0x1c, 0x22), DIVIDER),
    };
    p.rect_filled(rect, radius, fill);
    let edge = if hovered { Stroke::new(1.5, Color32::WHITE) } else { Stroke::new(1.0, edge) };
    p.rect_stroke(rect, radius, edge, StrokeKind::Inside);
    let centre = if numbered { rect.center() - Vec2::new(0.0, rect.height() * 0.14) } else { rect.center() };
    let text = |text: String, font: FontId, colour: Color32| {
        p.text(centre, Align2::CENTER_CENTER, text, font, colour);
    };
    match cell {
        CellModel::Inmate { beast, .. } => text(initial(*beast), big, Color32::WHITE),
        CellModel::Scouted { beast, .. } => text(format!("{}?", initial(*beast)), big, Color32::from_gray(220)),
        CellModel::Scratch { .. } => text("?".to_string(), big, Color32::from_gray(220)),
        CellModel::Nest { due } => text(due.map_or("N".to_string(), |d| format!("N{d}")), big, Color32::from_gray(200)),
        CellModel::Office => text(tr("IR").to_string(), FontId::proportional((rect.height() * 0.35).max(MIN_TEXT)), Color32::from_gray(200)),
        CellModel::Sealed { .. } => padlock(p, centre, rect.height() * 0.42),
        CellModel::Empty { .. } => {}
    }
    if cell.event().is_some() {
        p.text(rect.right_top() + Vec2::new(-4.0, 1.0), Align2::RIGHT_TOP, "!", FontId::proportional((rect.height() * 0.4).max(MIN_TEXT)), MARKER);
    }
    // The cell number uses the smallest size 11: when the cell is too small to fit it (a very narrow window or
    // a larger font) it is not written, so it does not cover the text in the middle; the cell number is in the
    // hover tooltip.
    if numbered {
        p.text(rect.right_bottom() - Vec2::new(3.0, 0.0), Align2::RIGHT_BOTTOM, cell_label(n), FontId::proportional(MIN_TEXT), MUTED);
    }
}

// A padlock of height `h` centred on `c`.
fn padlock(p: &egui::Painter, c: Pos2, h: f32) {
    let colour = Color32::from_gray(150);
    let body = Rect::from_center_size(c + Vec2::new(0.0, h * 0.2), Vec2::new(h * 0.8, h * 0.55));
    let r = h * 0.25;
    let top = Pos2::new(c.x, body.top());
    let shackle: Vec<Pos2> = (0..=12)
        .map(|k| {
            let a = std::f32::consts::PI * k as f32 / 12.0;
            Pos2::new(top.x - r * a.cos(), top.y - r * a.sin())
        })
        .collect();
    p.add(egui::Shape::line(shackle, Stroke::new(2.0, colour)));
    p.rect_filled(body, 2.0, colour);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn option(mean: f64, gap_se: f64) -> SolveOption {
        SolveOption { label: String::new(), action: Action::Swap, mean, se: 0.01, gap_se, n: 100, best: 0.0, policy: false }
    }

    /// Under click-through the wheel scrolls a page taller than the window: its scroll area keeps the mouse, on every
    /// tab but the board; a page that fits keeps passing clicks on its empty parts to the game.
    #[test]
    fn the_wheel_scrolls_long_pages_under_click_through() {
        let view = Rect::from_min_size(egui::pos2(60.0, 40.0), egui::vec2(400.0, 300.0));
        assert_eq!(wheel_area(Tab::Settings, 900.0, view), Some(view));
        assert_eq!(wheel_area(Tab::Run, 900.0, view), Some(view));
        assert_eq!(wheel_area(Tab::Settings, 300.0, view), None, "fits: nothing to scroll");
        assert_eq!(wheel_area(Tab::Board, 900.0, view), None, "the board leaves its empty parts to the game");
        assert_eq!(client_pixels(1.5, view), [90, 60, 690, 510]);
    }

    #[test]
    fn verdict_uses_twice_the_paired_error() {
        // First rank against second uses the second's gap_se: gap 0.10, twice the standard error 0.06, so
        // clearly better.
        assert_eq!(verdict(&[option(1.0, 0.0), option(0.9, 0.03)], Some(0), false), Some(Verdict::Clear));
        // Gap 0.10, twice the standard error 0.2: either is fine.
        assert_eq!(verdict(&[option(1.0, 0.0), option(0.9, 0.1)], Some(0), false), Some(Verdict::Either));
        // The advice is not the first rank: compare with the first using its own gap_se; the gap is negative,
        // so either is fine.
        assert_eq!(verdict(&[option(1.0, 0.0), option(0.99, 0.02)], Some(1), false), Some(Verdict::Either));
        // Only one option, still computing, no ranking rows, or a standard error that is not finite: no label.
        assert_eq!(verdict(&[option(1.0, 0.0)], Some(0), false), None);
        assert_eq!(verdict(&[option(1.0, 0.0), option(0.5, 0.01)], Some(0), true), None);
        assert_eq!(verdict(&[option(1.0, 0.0), option(0.5, 0.01)], None, false), None);
        assert_eq!(verdict(&[option(1.0, 0.0), option(0.5, f64::INFINITY)], Some(0), false), None);
    }

    #[test]
    fn charges_drive_the_progress_button() {
        let c = preview_charges("vorax:48,tartarus:46");
        assert_eq!((c.vorax, c.tartarus), (Some(48), Some(46)));
        assert_eq!(worst_fill(&c), Some(Fill::Full));
        assert_eq!(worst_fill(&preview_charges("vorax:47,tartarus:12")), Some(Fill::Near));
        assert_eq!(worst_fill(&preview_charges("tartarus:12")), Some(Fill::Normal));
        assert_eq!(worst_fill(&gamelog::Charges::default()), None);
        // One line per item and the last line is the hint; items with no value are written `—`.
        let hover = progress_hover(&preview_charges("vorax:36"));
        assert_eq!(hover.lines().count(), CHARGE_ITEMS.len() + 1);
        assert!(hover.lines().nth(1).unwrap().ends_with('\u{2014}'));
    }

    #[test]
    fn reason_when_the_advice_is_not_first() {
        let ranking = [option(5.0, 0.0), option(4.95, 0.05), option(4.0, 0.05)];
        let kept = Some(tr("Simulating \u{b7} the settled advice is kept"));
        // No advice (Solving...): no reason is written, however many ranking rows there are.
        assert_eq!(advice_reason(&ranking, None, true, 100, true), None);
        assert_eq!(advice_reason(&ranking, None, false, 900, true), None);
        // The advice is the 1st rank, or there are fewer than two ranking rows: nothing is written.
        assert_eq!(advice_reason(&ranking, Some(Some(0)), false, 900, true), None);
        assert_eq!(advice_reason(&ranking[..1], Some(Some(0)), false, 900, true), None);
        // Simulating with advice before SHOW_MIN playouts: it can only be one settled earlier (held / the first
        // bite of the manage screen).
        assert_eq!(advice_reason(&ranking, Some(Some(1)), true, 100, true), kept);
        assert_eq!(advice_reason(&ranking, Some(None), true, 100, true), kept);
        // Gap within the error: write within the error (the same whether simulating past SHOW_MIN or finished).
        assert_eq!(advice_reason(&ranking, Some(Some(1)), false, 900, true), Some(tr("Within error of #1")));
        assert_eq!(advice_reason(&ranking, Some(Some(1)), true, 600, true), Some(tr("Within error of #1")));
        // Simulating past SHOW_MIN with the gap beyond the error: the one settled earlier.
        assert_eq!(advice_reason(&ranking, Some(Some(2)), true, 600, true), kept);
        // Simulation finished with the gap beyond the error (for example the cage fight's chain plan): the
        // reason cannot be told, nothing is written.
        assert_eq!(advice_reason(&ranking, Some(Some(2)), false, 900, true), None);
        assert_eq!(advice_reason(&ranking, Some(None), false, 900, true), None);
        // Without early showing (not a Cage Fight run), an advice on show while simulating is the settled one,
        // however many playouts ran.
        assert_eq!(advice_reason(&ranking, Some(Some(1)), true, 600, false), kept);
        assert_eq!(advice_reason(&ranking, Some(Some(1)), false, 900, false), Some(tr("Within error of #1")));
    }

    #[test]
    fn template_words_are_dropped_from_a_row() {
        // "Take " goes: the tick icon says it.
        assert_eq!(strip_template("Take Maddened Beast's Barbed Collar", "Take {}"), "Maddened Beast's Barbed Collar");
        assert_eq!(strip_template("Confirm Riot Drill", "Confirm {}"), "Riot Drill");
        // A label without them, or with nothing after them, stays whole.
        assert_eq!(strip_template("Instigate", "Take {}"), "Instigate");
        assert_eq!(strip_template("Take ", "Take {}"), "Take ");
    }

    /// Every move the solver offers on the preview boards gets an icon and the cell it acts on, or a short text.
    #[test]
    fn each_move_has_an_icon_and_its_cell() {
        let rows = |which: &str| {
            let reading = preview_reading(which);
            let (st, problems) = crate::board::build_state(&reading.model, 1);
            assert!(problems.is_empty(), "{which}: {problems:?}");
            crate::solver::solve::candidates(&st).into_iter().map(|c| move_row(&c.action, &c.label, reading.model.warden)).collect::<Vec<_>>()
        };
        let patrol = rows("patrol");
        assert_eq!(patrol.len(), 3);
        for cell in [10, 3, 6] {
            assert!(patrol.contains(&(MoveIcon::GoTo, MoveTail::Cell(cell))), "{patrol:?}");
        }
        let manage = rows("manage");
        // A Replace names the Warden's cell, which the old "Replace" text left out.
        assert!(manage.contains(&(MoveIcon::Replace, MoveTail::Cell(10))), "{manage:?}");
        assert!(manage.iter().any(|(icon, tail)| *icon == MoveIcon::Consume && matches!(tail, MoveTail::Cell(_))), "{manage:?}");
        assert!(manage.contains(&(MoveIcon::EndDay, MoveTail::Text("End the day".to_string()))), "{manage:?}");
        let pick = rows("pick");
        assert!(pick.contains(&(MoveIcon::Reroll, MoveTail::Text("Re-roll".to_string()))), "{pick:?}");
        let names: Vec<String> = Tool::ALL.iter().take(3).map(|&t| instrument(t).to_string()).collect();
        for name in names {
            assert!(pick.contains(&(MoveIcon::Take, MoveTail::Text(name.clone()))), "{name}: {pick:?}");
        }
        // The moves without a preview board.
        assert_eq!(move_row(&Action::Chain { to: 7 }, "Chain on into 08", 2), (MoveIcon::Chain, MoveTail::Cell(7)));
        assert_eq!(move_row(&Action::EndChain, "End the chain", 2), (MoveIcon::EndChain, MoveTail::Text("End the chain".to_string())));
        assert_eq!(move_row(&Action::End, "Finish the run", 2).0, MoveIcon::Finish);
        assert_eq!(move_row(&Action::Hunt { cell: 5 }, "x", 2), (MoveIcon::Hunt, MoveTail::Cell(5)));
        let prey = format!("{}: mark the Rare Alpha Wolf in 06 as prey", hunt_name());
        assert_eq!(move_row(&Action::Choose { id: ChoiceId::Cell(5) }, &prey, 2), (MoveIcon::Hunt, MoveTail::Cell(5)));
        assert_eq!(move_row(&Action::Choose { id: ChoiceId::Cell(5) }, "Move to 06", 2), (MoveIcon::GoTo, MoveTail::Cell(5)));
        assert_eq!(move_row(&Action::Event, "Confirm Riot Drill", 2), (MoveIcon::Confirm, MoveTail::Text("Riot Drill".to_string())));
        assert_eq!(move_row(&Action::Swap, "Replace the Normal Rat in 03", -1).1, MoveTail::Text("Replace".to_string()));
    }

    /// The three tooltips of a row: the icon's meaning, the small tile's cell, and (unchanged) the full move.
    #[test]
    fn row_tooltips_say_the_icon_and_the_cell() {
        assert_eq!(MoveIcon::GoTo.meaning(), "Go to (move to this cell)");
        assert_eq!(lang::tr_in(Lang::ZhTw, "Go to (move to this cell)"), "去（移動到這格）");
        let m = preview_reading("patrol").model;
        assert_eq!(cell_tip(10, &m.cells[10]), format!("Cell 11: {}", inmate_name(1, Beast::Mammoth)));
        assert_eq!(cell_tip(0, &m.cells[0]), "Cell 01: Interrogation Room");
        assert_eq!(lang::tr_in(Lang::ZhTw, "Cell {}: {}"), "{} 號：{}");
        // Every meaning has its Chinese, with full-width brackets.
        for icon in [MoveIcon::GoTo, MoveIcon::Consume, MoveIcon::Chain, MoveIcon::EndChain, MoveIcon::Replace, MoveIcon::Reroll, MoveIcon::Take, MoveIcon::EndDay, MoveIcon::Finish] {
            let en: &'static str = Box::leak(icon.meaning().into_boxed_str());
            let zh = lang::tr_in(Lang::ZhTw, en);
            assert!(zh != en && zh.contains('（') && zh.ends_with('）'), "{en}: {zh}");
        }
        for en in ["{} (mark the Inmate in this cell as prey)", "Confirm (go on with this {})"] {
            assert!(lang::tr_in(Lang::ZhTw, en) != en, "{en}");
        }
    }

    #[test]
    fn popups_fit_the_window() {
        // Window content width 264 (296 minus the left navigation rail): tooltip width 248; at most 360 for a
        // wide window; at least 120 when very narrow.
        assert_eq!(popup_width(264.0), 248.0);
        assert_eq!(popup_width(389.0), 360.0);
        assert_eq!(popup_width(100.0), 120.0);
    }

    #[test]
    fn columns_fit_the_narrow_window() {
        // Window width 200: the content is about 152, and four columns plus spacing are exactly the content
        // width.
        let c = Cols::new(152.0, 4.0);
        assert!((c.rank + c.label + c.num + c.gap + 3.0 * 4.0 - 152.0).abs() < 0.01);
        assert!(c.label >= 50.0);
    }

    /// Narrower and narrower: the heading stops widening the expected loot column, then the gap column goes, then
    /// the name column keeps only the icon; the columns always add up to the width, so the numbers stay under their
    /// headings and inside the window.
    #[test]
    fn columns_give_way_in_order() {
        let need = || Need { num: 30.0, gap: 30.0, head: 80.0, loot: 52.0, cell: 36.0, icon: 15.0 };
        let sum = |c: &Cols| c.rank + c.label + c.num + c.gap + if c.gap > 0.0 { 3.0 } else { 2.0 } * 4.0;
        let wide = Cols::fit(200.0, 4.0, need());
        assert_eq!((wide.num, wide.gap), (46.0, 30.0), "the heading widens the expected loot");
        let elided = Cols::fit(130.0, 4.0, need());
        assert_eq!((elided.num, elided.gap), (30.0, 30.0), "the heading elided");
        assert!(elided.label >= 36.0);
        let no_gap = Cols::fit(100.0, 4.0, need());
        assert_eq!(no_gap.gap, 0.0, "the gap column gone, the cell number kept");
        assert!(no_gap.label >= 36.0);
        let alone = Cols::fit(115.0, 4.0, need());
        assert_eq!((alone.num, alone.gap), (52.0, 0.0), "without the gap column the expected loot's heading fits alone");
        let icon = Cols::fit(70.0, 4.0, need());
        assert!(icon.gap == 0.0 && icon.label >= 15.0 && icon.label < 36.0, "the icon alone");
        for (c, w) in [(&wide, 200.0), (&elided, 130.0), (&no_gap, 100.0), (&alone, 115.0), (&icon, 70.0)] {
            assert!((sum(c) - w).abs() < 0.01, "adds up to {w}");
        }
    }

    /// The run check's lines: the status bar's says a click opens it (both languages), the window's names no other
    /// place, health.txt keeps the old one; all clear is the same everywhere.
    #[test]
    fn run_check_lines_are_translated() {
        use crate::lang::{Lang, tr_in};
        for key in ["Run check: {} to look at, click here to see them.", "Run check: {} to look at.", "Click to see the run check", "Run check", "Run {}", "See"] {
            assert_ne!(tr_in(Lang::ZhTw, key), key, "{key}");
        }
        assert_eq!(tr_in(Lang::ZhTw, "Run check: {} to look at, click here to see them."), "本局檢查：有 {} 個問題，點這裡查看。");
        let found = preview_health(true);
        let clear = preview_health(false);
        let (status, window, text) = (found.status_line(), found.window_summary(), found.text());
        assert!(status != window && !text.starts_with(&status) && !text.starts_with(&window), "{status} / {window} / {text}");
        assert_eq!((clear.status_line(), clear.window_summary()), (clear.summary(), clear.summary()));
    }

    /// Colour of the gap: grey within 2 paired standard errors (about the same), the normal text colour beyond;
    /// grey when the standard error cannot be computed yet.
    #[test]
    fn gap_within_twice_the_paired_error_is_grey() {
        assert!(within_error(-0.12, 0.07));
        assert!(within_error(-0.14, 0.07));
        assert!(!within_error(-0.15, 0.07));
        assert!(within_error(-0.5, f64::INFINITY));
    }

    /// The status bar's "+N" takes the colour of the most serious item it folds away: red for an error, yellow for
    /// the game log light red, progress full or a run check with issues, blue for the rest.
    #[test]
    fn the_more_button_takes_the_worst_folded_colour() {
        let notice = || StatusItem::Notice(NoticeKind::Install);
        let health = |issues: Vec<String>| StatusItem::Health { summary: String::new(), issues };
        assert_eq!(more_colour(&[notice(), StatusItem::Auto(String::new()), health(vec![])]), ACCENT);
        assert_eq!(more_colour(&[notice(), StatusItem::Charge(String::new())]), theme::WARN);
        assert_eq!(more_colour(&[StatusItem::Log(String::new())]), theme::WARN);
        assert_eq!(more_colour(&[notice(), health(vec!["x".to_string()])]), theme::WARN);
        let elevated = StatusItem::Notice(NoticeKind::Elevated { text: String::new(), waiting: false });
        assert_eq!(more_colour(&[elevated, notice()]), theme::WARN, "the administrator notice folded away: yellow");
        assert_eq!(more_colour(&[StatusItem::Charge(String::new()), StatusItem::Error(String::new())]), theme::ERROR);
    }

    /// "Restore defaults": the kept fields equal the original ones and every other field equals its default.
    /// Compared field by field through JSON, so fields added to Settings later are checked too.
    #[test]
    fn restoring_defaults_keeps_language_install_and_privacy() {
        const KEPT: [&str; 13] = ["language", "offer_install", "desktop_shortcut", "check_updates", "updated_seen", "share_logs", "shared_from", "install_id", "tools", "tools_days", "theme", "run_log", "keep_all_logs"];
        let custom: Settings = serde_json::from_str(
            r#"{"read_hotkey":"F1","auto_hotkey":"F2","click_through":true,"start_as_admin":true,"opacity":0.7,
            "window_title":"Game","rollouts":800,"tools":["lion_trap"],"tools_days":12,"theme":3,"auto_read":false,
            "auto_place":false,"window_pos":[10,20],"window_size":[300.0,600.0],"float_pos":{"tools":[5,6]},"run_log":false,"keep_all_logs":true,"observe":true,
            "check_updates":false,"updated_seen":"1.0.1","offer_install":false,"share_logs":true,"shared_from":"2026-11-01_00-00-00",
            "install_id":"abc","desktop_shortcut":true,"highlight":true,"show_value":false,"language":"zh-TW","vorax_enabled":false,
            "vorax_rollouts":1024,"vorax_learn":false,"vorax_strips":true,"vorax_give_up_hint":true,"font_scale":1.3,"layout":"compact",
            "game_log":"D:/Logs/UE_game.log"}"#,
        )
        .unwrap();
        // the affix page's settings, off their defaults
        let custom = {
            let mut c = custom;
            c.affix_frames = false;
            c.affix_frame_style = crate::affix::FrameStyle::Bar;
            c.affix_grades.insert("3500300".into(), crate::affix::Grade::None);
            c
        };
        let json = |s: &Settings| serde_json::to_value(s).unwrap();
        let (custom_v, default_v, restored_v) = (json(&custom), json(&Settings::default()), json(&restored(&custom)));
        let fields = default_v.as_object().unwrap();
        for (key, default) in fields {
            // Every field of the test data differs from the default, so the comparison is meaningful (a new
            // field must be added to the JSON above).
            assert_ne!(&custom_v[key], default, "{key} 和預設相同");
            // the user's affix grades are kept too
            let kept = KEPT.contains(&key.as_str()) || key == "affix_grades";
            let want = if kept { &custom_v[key] } else { default };
            assert_eq!(&restored_v[key], want, "{key}");
        }
    }

    /// "Updated to X.Y.Z": shown for the running version's saved notes; its "x" or the end of a game (both through
    /// `dismiss_updated`) records the version in the settings, so it stays gone, after a restart too (the settings
    /// are saved, and "Restore defaults" keeps it); a later update shows its own again.
    #[test]
    fn updated_notice_goes_after_a_dismissal_or_a_game() {
        let running = "1.2.0";
        let saved = update::SavedNotes { version: running.into(), notes: "- one".into() };
        let mut settings = Settings::default();
        let shows = |s: &Settings| update::show_updated(Some(&saved), running, &s.updated_seen);
        assert!(shows(&settings));
        // A game ends (or the "x" is pressed): recorded once, then nothing more to save.
        assert!(dismiss_updated(&mut settings, Some(&saved)));
        assert!(!shows(&settings));
        assert!(!dismiss_updated(&mut settings, Some(&saved)));
        // Saved and read back as a restart would.
        let reread: Settings = serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert!(!shows(&reread) && !shows(&restored(&reread)));
        // Nothing saved: a game changes nothing.
        let mut fresh = Settings::default();
        assert!(!dismiss_updated(&mut fresh, None) && fresh.updated_seen.is_empty());
        // The next update's notes show again.
        let next = update::SavedNotes { version: "1.3.0".into(), notes: "- two".into() };
        assert!(update::show_updated(Some(&next), "1.3.0", &settings.updated_seen));
    }

    /// "What's new" of both notices opens the window in Ascendant (no page, no browser): from a newer release, with its
    /// verified notes on top; it is off when there is nothing to show or while the update runs.
    #[test]
    fn whats_new_shows_the_verified_notes() {
        let whats_new = |kind: &NoticeKind| {
            notice_choices(kind).into_iter().find_map(|(_, enabled, act)| match act {
                NoticeAct::WhatsNew(pending) => Some((enabled, pending)),
                _ => None,
            })
        };
        let updated = NoticeKind::Updated { version: "1.0.0".into(), notes: "- kept" };
        assert_eq!(whats_new(&updated), Some((true, false)));
        let available = NoticeKind::Update { version: "1.1.0".into(), notes: "- new", history: true, updating: None, checking: true };
        assert_eq!(whats_new(&available), Some((true, true)));
        let running = NoticeKind::Update { version: "1.1.0".into(), notes: "- new", history: true, updating: Some(update::Step::Download), checking: false };
        assert_eq!(whats_new(&running).map(|w| w.0), Some(false));
        // A release without notes still lists the built-in ones; with none at all there is nothing to show.
        let bare = NoticeKind::Update { version: "1.1.0".into(), notes: "", history: true, updating: None, checking: false };
        assert_eq!(whats_new(&bare), Some((true, false)));
        let nothing = NoticeKind::Update { version: "1.1.0".into(), notes: "", history: false, updating: None, checking: false };
        assert_eq!(whats_new(&nothing).map(|w| w.0), Some(false));
        assert_eq!(whats_new(&NoticeKind::Updated { version: "1.0.0".into(), notes: "" }).map(|w| w.0), Some(false));
    }

    /// "What's new" and the instruments window share one spot: opening one closes the other, and closing the
    /// instruments window leaves "What's new" closed.
    #[test]
    fn whats_new_and_the_instruments_window_take_turns() {
        let mut floats = Floats::default();
        floats.open_whats_new(WhatsNew::new(None));
        assert!(floats.whats_new.is_some() && !floats.tools);
        floats.toggle_tools();
        assert!(floats.whats_new.is_none() && floats.tools, "the instruments window closes What's new");
        floats.open_whats_new(WhatsNew::new(None));
        assert!(floats.whats_new.is_some() && !floats.tools, "What's new closes the instruments window");
        floats.toggle_tools();
        floats.toggle_tools();
        assert!(floats.whats_new.is_none() && !floats.tools);
        // The run check's window: one at a time with the others.
        floats.open_health();
        assert!(floats.health && floats.whats_new.is_none() && !floats.tools);
        floats.toggle_tools();
        assert!(!floats.health && floats.tools, "the instruments window closes the run check");
        floats.open_health();
        assert!(floats.health && !floats.tools, "the run check closes the instruments window");
        floats.open_whats_new(WhatsNew::new(None));
        assert!(!floats.health && floats.whats_new.is_some(), "What's new closes the run check");
    }

    /// The small windows of their own (1.1.0): distinct ids and settings keys, a fixed size within the old floating
    /// width; with no place saved one opens over the main window's page, centred across it under the status bar; only
    /// a change of more than 2 pixels after the first sight is a move to save.
    #[test]
    fn small_windows_place_and_move() {
        let all = [Float::WhatsNew, Float::Health, Float::Tools];
        for (k, a) in all.iter().enumerate() {
            assert!(a.size().x <= 360.0 && a.size().y >= 200.0);
            for b in &all[k + 1..] {
                assert_ne!(a.id(), b.id());
                assert_ne!(a.key(), b.key());
            }
        }
        let main = Rect::from_min_size(Pos2::new(1700.0, 200.0), Vec2::new(312.0, 559.0));
        let at = float_default_at(main, Float::Tools.size());
        assert_eq!(at, Pos2::new(1700.0 + RAIL / 2.0 + (312.0 - 250.0) / 2.0, 230.0));
        assert!(!float_moved(None, [10, 10]), "the first sight is no move");
        assert!(!float_moved(Some([10, 10]), [12, 8]));
        assert!(float_moved(Some([10, 10]), [13, 10]));
        assert!(float_moved(Some([10, 10]), [10, -300]));
    }

    /// Esc closes "What's new" only when no text box has the keyboard and no hotkey is being recorded.
    #[test]
    fn esc_is_the_text_box_or_the_recording_first() {
        assert!(esc_closes(true, false));
        assert!(!esc_closes(true, true), "a text box or a recording has it");
        assert!(!esc_closes(false, false));
    }

    /// Recording a hotkey: Esc cancels, the other hotkey's key and a key that types are refused with a reason, a
    /// usable key is tried.
    #[test]
    fn recorded_keys_are_judged() {
        let auto = ("F8", Which::Auto);
        assert_eq!(judge(Recorded::Cancel, auto), Judged::Cancel);
        assert_eq!(judge(Recorded::Key("Ctrl+Shift+R".into()), auto), Judged::Try("Ctrl+Shift+R".into()));
        assert_eq!(judge(Recorded::Key("F9".into()), auto), Judged::Try("F9".into()));
        // The auto read key, however written.
        assert!(matches!(judge(Recorded::Key("F8".into()), auto), Judged::Refuse(why) if why.full.contains("F8")));
        assert!(matches!(judge(Recorded::Key("Ctrl+Alt+5".into()), ("alt+ctrl+5", Which::Read)), Judged::Refuse(_)));
        assert!(matches!(judge(Recorded::Typing("A".into()), auto), Judged::Refuse(why) if why.short.contains('A') && why.full.contains('A')));
        assert!(matches!(judge(Recorded::Unusable, auto), Judged::Refuse(_)));
        assert!(matches!(judge(Recorded::Clipboard, auto), Judged::Refuse(_)));
    }

    /// On a Cage Fight's target screen the value sits over the first badge's cell (the next bite), in screen px.
    #[test]
    fn the_value_goes_over_the_first_badge() {
        let spots = [(1, (900.0, 500.0, 65.0, 65.0)), (2, (1200.0, 500.0, 65.0, 65.0))];
        let game = Some([100, 50, 2560, 1440]);
        assert_eq!(first_badge_spot(&spots, game), Some(([1000.0, 550.0, 65.0, 65.0, 1440.0 / 1080.0], 50)));
        assert_eq!(first_badge_spot(&[], game), None);
        assert_eq!(first_badge_spot(&spots, None), None, "the game behind or gone");
    }

    /// A click selecting an instrument or treasure card keeps the mark; a reroll there, and a click on any other screen,
    /// hides it.
    #[test]
    fn picking_a_card_keeps_the_mark() {
        use crate::engine::ChoiceId;
        let card = Action::Choose { id: ChoiceId::Cell(3) };
        assert!(!click_hides(Screen::InstrumentPick, &card), "an instrument card picked, not confirmed");
        assert!(!click_hides(Screen::TreasurePick, &card), "a treasure card picked, not confirmed");
        assert!(click_hides(Screen::InstrumentPick, &Action::Reroll));
        assert!(click_hides(Screen::TreasurePick, &Action::Reroll));
        assert!(click_hides(Screen::CellPick, &card), "a cell pick is played by its click");
        assert!(click_hides(Screen::Manage, &Action::Devour { to: 4 }));
        assert!(click_hides(Screen::Patrol, &Action::Patrol { cell: 7 }));
        assert!(click_hides(Screen::ChainTarget, &Action::Chain { to: 9 }));
    }

    /// Only a click on the mark counts (2026-11-07: a click anywhere in the game hid it). The manage screen's Hunt
    /// button at 1440p, mark_at 1511, 1126, 94 x 94 in that run's log, the game at (100, 50): a click on it or its box
    /// counts, on the next decision button (End, 460 px along), on the board or off the game does not; the box drawn a
    /// few px off the read (see `Mark::spot`) counts too.
    #[test]
    fn only_a_click_on_the_mark_hides_it() {
        let u = 4.0 / 3.0;
        let game = [100, 50, 2560, 1440];
        let read = [100.0 + 1511.0, 50.0 + 1126.0, 94.0, 94.0, u];
        let at = |x: i32, y: i32| mark::Click::at((100 + x, 50 + y));
        assert!(on_mark(at(1511, 1126), game, read, None), "the button");
        assert!(on_mark(at(1511 + 94 + 16, 1126), game, read, None), "the box's line");
        assert!(!on_mark(at(1970, 1126), game, read, None), "End, the next button");
        assert!(!on_mark(at(1280, 600), game, read, None), "the board");
        assert!(!on_mark(at(1511, 1000), game, read, None), "above the box");
        assert!(on_mark(at(1511, 1000), game, read, Some([100.0 + 1511.0, 50.0 + 1100.0, 94.0, 94.0, u])), "the box held 26 px higher");
        let top_cell = [100.0 + 1000.0, 50.0 + 60.0, 87.0, 87.0, u];
        assert!(!on_mark(mark::Click::at((1100, 40)), game, top_cell, None), "above the game's top, inside the margin");
    }

    /// A click the game's log does not follow with a move within CONFIRM_WAIT was not played (a way to cancel it the
    /// clicks do not know): the mark comes back, on the screens whose clicks log their moves. A move logged after the
    /// click, or just before it, keeps it hidden; on the treasure pick it stays hidden as before.
    #[test]
    fn an_unplayed_click_brings_the_mark_back() {
        // The reading at t0, the click at t0 + 1 s; `later(ms)` after the click.
        let read = Instant::now();
        let at = read + Duration::from_secs(1);
        let later = |ms: u64| at + Duration::from_millis(ms);
        let unconfirmed = |screen, seen, now| click_unconfirmed(screen, (at, read), seen, now, true);
        assert!(!unconfirmed(Screen::Patrol, None, later(100)), "not yet");
        assert!(unconfirmed(Screen::Patrol, None, later(600)), "no move logged: back");
        assert!(!unconfirmed(Screen::Patrol, Some(later(50)), later(600)), "the door's Moved");
        assert!(!unconfirmed(Screen::Manage, Some(at - Duration::from_millis(100)), later(900)), "logged just before the look");
        assert!(unconfirmed(Screen::Manage, Some(at - Duration::from_millis(400)), later(900)), "too long before the click");
        // A move logged within CONFIRM_EARLY of the click but before the screen was read is the move before.
        assert!(click_unconfirmed(Screen::Manage, (read + Duration::from_millis(100), read), Some(read - Duration::from_millis(50)), later(900), true), "before the reading");
        for screen in [Screen::Manage, Screen::Interrogation, Screen::CellPick, Screen::ChainTarget, Screen::InstrumentPick] {
            assert!(unconfirmed(screen, None, later(700)), "{screen:?}");
        }
        assert!(!unconfirmed(Screen::TreasurePick, None, later(5000)), "no reliable line: stays hidden");
        // The game's log not followed (auto read off, or its light red): no line can come, so it stays hidden as in 1.0.3.
        assert!(!click_unconfirmed(Screen::Patrol, (at, read), None, later(5000), false), "log not followed");
        use super::super::gamelog::Signal;
        assert!(confirms_click(Signal::Moved) && confirms_click(Signal::Bite) && confirms_click(Signal::Reroll));
        assert!(!confirms_click(Signal::CardsUp(3, 0)) && !confirms_click(Signal::Prison) && !confirms_click(Signal::ObjectiveDone));
    }

    /// A click counts as a button's (2026-11-08, the user: pressed on the box and let go elsewhere, the game played
    /// nothing): pressed and released on the box hides it, either end off it does not; a click quicker than the 50 ms
    /// looks, seen only as "pressed since", takes the cursor then for both ends. The Hunt button of the test above.
    #[test]
    fn a_click_hides_the_mark_only_pressed_and_released_on_it() {
        use mark::{Clicks, Look};
        let u = 4.0 / 3.0;
        let game = [100, 50, 2560, 1440];
        let read = [100.0 + 1511.0, 50.0 + 1126.0, 94.0, 94.0, u];
        let (on_box, off_box) = (Some((100 + 1511, 50 + 1126)), Some((100 + 1970, 50 + 1126)));
        let held = |at| Look { down: true, pressed: true, other: false, at };
        let still = |at| Look { down: true, pressed: false, other: false, at };
        let up = |at| Look { down: false, pressed: false, other: false, at };
        let hides = |looks: &[Look]| {
            let mut clicks = Clicks::default();
            looks.iter().any(|&look| clicks.next(look).is_some_and(|click| on_mark(click, game, read, None)))
        };
        assert!(hides(&[held(on_box), still(on_box), up(on_box)]), "pressed and released on the box");
        assert!(!hides(&[held(on_box), still(off_box), up(off_box)]), "pressed on the box, released off it");
        assert!(!hides(&[held(off_box), still(on_box), up(on_box)]), "pressed off the box, released on it");
        assert!(hides(&[up(None), Look { down: false, pressed: true, other: false, at: on_box }]), "a quick click on the box");
        assert!(!hides(&[Look { down: false, pressed: true, other: false, at: off_box }]), "a quick click off it");
        // 2026-11-08 (the user): the right button pressed while the left is held on the box, then both let go: the
        // game dropped the pick. The middle one counts the same; so does one by the look the left comes up.
        let other = |look: Look| Look { other: true, ..look };
        assert!(!hides(&[held(on_box), other(still(on_box)), up(on_box)]), "the right button pressed while held");
        assert!(!hides(&[held(on_box), still(on_box), other(up(on_box))]), "the right button pressed by the release");
        assert!(!hides(&[other(held(on_box)), up(on_box)]), "pressed with the left");
        assert!(!hides(&[other(Look { down: false, pressed: true, other: false, at: on_box })]), "with a quick click");
        assert!(hides(&[other(up(on_box)), held(on_box), up(on_box)]), "a right click before the left one does not count");
        assert!(!hides(&[held(on_box), up(None)]), "released on this app's window");
        assert!(!hides(&[held(on_box), still(on_box)]), "still held: not yet");
        // 2026-11-08 (the user, a day 1 patrol): pressed on the box, moved off and back, released on it: the game
        // played nothing, as a button cancels once the cursor leaves it.
        assert!(!hides(&[held(on_box), still(off_box), still(on_box), up(on_box)]), "off the box and back while held");
        // Small moves on the box while held: still a click on it.
        let near = |dx: i32, dy: i32| Some((100 + 1511 + dx, 50 + 1126 + dy));
        assert!(hides(&[held(on_box), still(near(30, -20)), still(near(-40, 50)), up(near(10, 10))]), "moved a little on the box");
        assert!(!hides(&[held(on_box), still(None), up(on_box)]), "held over this app's window a while");
        // Released on the box and a click went by unseen between two looks: the first click is the one that counts.
        let mut clicks = Clicks::default();
        assert_eq!(clicks.next(held(off_box)), None);
        let (a, b) = (off_box.unwrap(), on_box.unwrap());
        assert_eq!(clicks.next(Look { down: false, pressed: true, other: false, at: on_box }), Some(mark::Click { down: a, up: b, min: (a.0.min(b.0), a.1.min(b.1)), max: (a.0.max(b.0), a.1.max(b.1)) }));
        assert_eq!(clicks.next(up(on_box)), None, "nothing left");
    }

    /// A move left alone is not played out (n = 0, mean 0): with no manage screen's value to take it has none, on the
    /// game or in the window (the session's tests cover the value taken).
    #[test]
    fn a_move_not_played_out_has_no_value() {
        let mut s = Session::new(100, Vec::new(), None, Some(10));
        s.ranking = vec![SolveOption { label: String::new(), action: Action::Devour { to: 4 }, mean: 0.0, se: 0.0, gap_se: 0.0, n: 0, best: 0.0, policy: true }];
        assert_eq!(loot_text(&s, Some(0)), None);
    }

    /// The value on the game is the board's own "Expected loot" number for the advised move: the same function
    /// writes both, two decimals of that move's mean (not the first row's).
    #[test]
    fn the_value_on_the_game_is_the_boards_number() {
        let option = |mean: f64| SolveOption { label: String::new(), action: Action::Swap, mean, se: 0.1, gap_se: 0.1, n: 1600, best: 0.0, policy: false };
        let mut s = Session::new(100, Vec::new(), None, Some(10));
        s.ranking = vec![option(6.8123), option(6.7406), option(5.0)];
        assert_eq!(loot_text(&s, Some(1)).as_deref(), Some("6.74"));
        assert_eq!(loot_text(&s, Some(1)), Some(format!("{:.2}", s.ranking[1].mean)), "the top three's number");
        assert_eq!(loot_text(&s, None), None);
        assert_eq!(loot_text(&s, Some(9)), None);
        s.ranking.clear();
        assert_eq!(loot_text(&s, Some(0)), None);
    }

    /// Every file in data/release-notes is built in under its version with the same text, the running version has
    /// one, and each follows the RELEASE-NOTES.txt rules: UTF-8 without a byte order mark, LF only, at most 8000
    /// characters, no control characters but line feeds and no hidden format characters, not blank.
    #[test]
    fn built_in_release_notes_are_complete_and_plain() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("data").join("release-notes");
        let mut files = Vec::new();
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "txt") {
                files.push(path);
            }
        }
        assert_eq!(files.len(), RELEASE_NOTES.len(), "every data/release-notes/*.txt must be listed in RELEASE_NOTES");
        for path in &files {
            let version = path.file_stem().unwrap().to_str().unwrap();
            assert!(update::semver(version).is_some(), "{} is not X.Y.Z.txt", path.display());
            let bytes = std::fs::read(path).unwrap();
            let text = std::str::from_utf8(&bytes).unwrap_or_else(|e| panic!("{}: not UTF-8: {e}", path.display()));
            let built_in = RELEASE_NOTES.iter().find(|(v, _)| *v == version).unwrap_or_else(|| panic!("{version} is not in RELEASE_NOTES"));
            assert_eq!(built_in.1, text, "{version}: RELEASE_NOTES must include_str! its own file");
            assert!(!text.starts_with('\u{FEFF}'), "{version}: byte order mark");
            assert!(text.chars().count() <= 8000, "{version}: over 8000 characters");
            assert!(!text.trim().is_empty(), "{version}: blank");
            for c in text.chars() {
                assert!(c == '\n' || !c.is_control(), "{version}: control character U+{:04X}", c as u32);
                assert!(!update::is_hidden_format(c), "{version}: hidden format character U+{:04X}", c as u32);
            }
        }
        let running = env!("CARGO_PKG_VERSION");
        assert!(RELEASE_NOTES.iter().any(|(v, _)| *v == running), "data/release-notes/{running}.txt is missing");
    }

    /// The list is newest first by semantic version, the built-in text wins over notes an update kept for the same
    /// version, and saved notes of a version with none built in are listed too.
    #[test]
    fn release_notes_order_and_precedence() {
        let saved = |v: &str, n: &str| update::SavedNotes { version: v.into(), notes: n.into() };
        let mut list = vec![saved("1.0.9", "a"), saved("0.9.0", "b"), saved("1.0.10", "c"), saved("1.1.0", "d")];
        newest_first(&mut list);
        assert_eq!(list.iter().map(|n| n.version.as_str()).collect::<Vec<_>>(), ["1.1.0", "1.0.10", "1.0.9", "0.9.0"]);
        let (version, text) = RELEASE_NOTES[0];
        let shown = release_notes(Some(&saved(version, "downloaded, different")));
        assert_eq!(shown.iter().find(|n| n.version == version).map(|n| n.notes.as_str()), Some(update::plain_notes(text).as_str()));
        assert_eq!(shown.len(), RELEASE_NOTES.len());
        let extra = release_notes(Some(&saved("999.0.0", "- later")));
        assert_eq!(extra[0], saved("999.0.0", "- later"));
    }

    /// Only one version starts unfolded: a release not installed yet, else the running version, else the newest.
    #[test]
    fn whats_new_unfolds_one_version() {
        assert_eq!(unfolded_notes(&[("1.0.0", false), ("0.9.0", false)], "1.0.0"), Some(0));
        assert_eq!(unfolded_notes(&[("1.1.0", true), ("1.0.0", false), ("0.9.0", false)], "1.0.0"), Some(0));
        assert_eq!(unfolded_notes(&[("1.0.1", false), ("1.0.0", false)], "1.0.0"), Some(1));
        assert_eq!(unfolded_notes(&[("1.0.1", false), ("1.0.0", false)], "2.0.0"), Some(0));
        assert_eq!(unfolded_notes(&[], "1.0.0"), None);
    }

    /// Whether `c` is a CJK character (ideographs, kana, hangul, CJK punctuation and full-width forms).
    fn cjk(c: char) -> bool {
        matches!(c, '\u{3000}'..='\u{30FF}' | '\u{3400}'..='\u{4DBF}' | '\u{4E00}'..='\u{9FFF}' | '\u{AC00}'..='\u{D7AF}' | '\u{F900}'..='\u{FAFF}' | '\u{FF00}'..='\u{FFEF}')
    }

    /// Each built-in version's notes have exactly a `[[zh-TW]]` and an `[[en]]` section, the first with Chinese text,
    /// the second with no CJK at all; the interface language picks the section.
    #[test]
    fn built_in_notes_split_by_language() {
        let (zh_key, en_key) = (lang::Lang::ZhTw.key(), lang::Lang::En.key());
        for (version, text) in RELEASE_NOTES {
            let markers: Vec<&str> = text.lines().filter_map(notes_marker).collect();
            assert_eq!(markers, [zh_key, en_key], "{version}: the zh-TW and en sections");
            // Text before the first marker is never shown, so the file must start with one.
            assert_eq!(text.lines().find(|l| !l.trim().is_empty()).and_then(notes_marker), Some(zh_key), "{version}: starts with the zh-TW marker");
            let (zh, en) = (notes_in(text, zh_key), notes_in(text, en_key));
            assert!(!zh.is_empty() && !en.is_empty() && zh != en, "{version}: two sections");
            assert!(zh.chars().any(cjk), "{version}: the Chinese section has Chinese");
            assert!(!en.chars().any(cjk), "{version}: the English section has no CJK: {:?}", en.chars().find(|&c| cjk(c)));
            assert!(!zh.contains("[[") && !en.contains("[["), "{version}: marker lines are not shown");
        }
    }

    /// The section in the language's key, trimmed, without its marker line; else `en`, else the first; text before
    /// the first marker is not shown; notes without markers, or with only blank sections, are shown whole.
    #[test]
    fn notes_in_takes_the_language_section() {
        let both = "\n[[zh-TW]]\n\n中文一\n中文二\n\n  [[en]]  \n\nEnglish one\nEnglish two\n\n";
        assert_eq!(notes_in(both, "zh-TW"), "中文一\n中文二");
        assert_eq!(notes_in(both, "en"), "English one\nEnglish two");
        assert_eq!(notes_in("[[zh-TW]]\r\n中文\r\n[[en]]\r\nEnglish", "en"), "English");
        // A language without a section falls back to en, then to the first section; the key matches exactly.
        assert_eq!(notes_in(both, "ja"), "English one\nEnglish two");
        assert_eq!(notes_in(both, "zh-tw"), "English one\nEnglish two");
        assert_eq!(notes_in("[[zh-TW]]\n中文\n[[fr]]\nFrançais", "en"), "中文");
        // An unknown marker is a section of its own and does not spill into the others.
        let three = "[[zh-TW]]\n中文\n[[ja]]\n日本語\n[[en]]\nEnglish";
        assert_eq!((notes_in(three, "zh-TW"), notes_in(three, "en"), notes_in(three, "ja")), ("中文", "English", "日本語"));
        // Text before the first marker is not shown.
        assert_eq!(notes_in("intro\n[[en]]\nEnglish", "en"), "English");
        // No markers (or a line that is not exactly one), or only blank sections: the whole text.
        for whole in ["- one\n- two", "中文\n----\nEnglish", "[[en]] English\nmore", "[en]\nEnglish", "[[]]\nx", "[[zh-TW]]\n\n[[en]]\n  \n"] {
            assert_eq!(notes_in(whole, "zh-TW"), whole);
            assert_eq!(notes_in(whole, "en"), whole);
        }
    }

    /// The hourly check never takes away an update the status bar already offers (not even when it answers "none"),
    /// and what it finds itself is offered quietly (the red dot).
    #[test]
    fn hourly_check_keeps_an_offered_update() {
        let release = |minor: u64| update::Release {
            version: [1, minor, 0],
            notes: String::new(),
            exe: update::Asset { url: String::new(), size: 0, digest: None },
            sums: update::Asset { url: String::new(), size: 0, digest: None },
            sig: update::Asset { url: String::new(), size: 0, digest: None },
            signed: None,
        };
        // Offered in the status bar (found at start or by "Check now"): kept, also through a "none".
        assert_eq!(quiet_release(Some(release(1)), false, None), (Some(release(1)), false));
        assert_eq!(quiet_release(Some(release(1)), false, Some(release(1))), (Some(release(1)), false));
        assert_eq!(quiet_release(Some(release(1)), false, Some(release(2))), (Some(release(2)), false));
        // Found by the hourly check: quiet; a quiet one goes with a "none" (only the dot goes).
        assert_eq!(quiet_release(None, false, Some(release(1))), (Some(release(1)), true));
        assert_eq!(quiet_release(Some(release(1)), true, Some(release(2))), (Some(release(2)), true));
        assert_eq!(quiet_release(Some(release(1)), true, None), (None, true));
        assert_eq!(quiet_release(None, true, None), (None, true));
    }

    /// Layout switching: hysteresis (stays as it was within 10 points above and below the threshold) and no
    /// switching while dragging; full and compact are fixed; an old settings file without the layout field
    /// means auto.
    #[test]
    fn layout_switches_with_hysteresis() {
        let auto = LayoutPref::Auto;
        // A 421×351 window: compact; the default 296×520: full.
        assert!(next_compact(auto, false, 351.0, false));
        assert!(!next_compact(auto, false, 520.0, false));
        // The full layout must be below 460 to switch to compact.
        assert!(!next_compact(auto, false, 465.0, false));
        assert!(next_compact(auto, false, 459.0, false));
        // The compact layout must be above 480 to switch back to full; in between it stays compact.
        assert!(next_compact(auto, true, 475.0, false));
        assert!(next_compact(auto, true, 480.0, false));
        assert!(!next_compact(auto, true, 481.0, false));
        // No switching while dragging, however tall or short.
        assert!(!next_compact(auto, false, 300.0, true));
        assert!(next_compact(auto, true, 900.0, true));
        // Full and compact are fixed.
        assert!(!next_compact(LayoutPref::Full, true, 300.0, false));
        assert!(next_compact(LayoutPref::Compact, false, 900.0, false));
        let s: Settings = serde_json::from_str(r#"{"language": "zh-TW"}"#).unwrap();
        assert_eq!(layout_pref(&s.layout), auto);
        assert_eq!(layout_pref("full"), LayoutPref::Full);
        assert_eq!(layout_pref("nonsense"), auto);
        assert_eq!(layout_pref(&restored(&Settings { layout: "compact".into(), ..Settings::default() }).layout), auto);
    }

    /// An old settings file without the font size: read as standard and does not fail to load; a corrupted
    /// value or one not among the options takes the closest option.
    #[test]
    fn the_log_light_follows_the_status() {
        use gamelog::{LogSource, LogStatus as S};
        let path = PathBuf::from(r"C:\Games\UE_game.log");
        let source = LogSource::GameFolder;
        let cases = [
            (Some(S::Following { path: path.clone(), source: source.clone() }), Lamp::Green),
            (Some(S::Searching), Lamp::Yellow),
            (Some(S::GameClosed { path: path.clone(), source: source.clone() }), Lamp::Yellow),
            (Some(S::Stale { path: path.clone(), source: source.clone() }), Lamp::Red),
            (Some(S::Missing), Lamp::Red),
            (Some(S::ManualMissing(path.clone())), Lamp::Red),
            (None, Lamp::Grey),
        ];
        for (status, lamp) in &cases {
            assert_eq!(log_lamp(status.as_ref()), *lamp, "{status:?}");
            // Only a red light has a reminder for the status bar, and its tooltip carries the note.
            assert_eq!(log_alert(status.as_ref()).is_some(), *lamp == Lamp::Red, "{status:?}");
            assert_eq!(log_hover(status.as_ref()).contains(tr(LOG_NOTE)), *lamp == Lamp::Red, "{status:?}");
        }
        // The path shows for the statuses that name one.
        assert_eq!(log_path(cases[0].0.as_ref()), Some(path.clone()));
        assert_eq!(log_path(Some(&S::Searching)), None);
        // Every preview name maps to its status.
        for (name, lamp) in [("following", Lamp::Green), ("searching", Lamp::Yellow), ("closed", Lamp::Yellow), ("stale", Lamp::Red), ("missing", Lamp::Red), ("manual-missing", Lamp::Red), ("off", Lamp::Grey)] {
            assert_eq!(log_lamp(preview_log_status(name).as_ref()), lamp, "{name}");
        }
    }

    #[test]
    fn review_amounts_take_their_game_units() {
        assert_eq!(amount(RunKind::Tartarus, 0.527), "0.53");
        assert_eq!(amount(RunKind::Vorax, 812_345.0), "812k");
        assert_eq!(amount(RunKind::Vorax, 1_234.0), "1.2k");
        assert_eq!(amount(RunKind::Vorax, 850.0), "850");
        assert_eq!(signed(RunKind::Vorax, -12_300.0), "-12k");
        assert_eq!(signed(RunKind::Tartarus, 1.28), "+1.28");
    }

    #[test]
    fn text_size_reads_old_settings_as_standard() {
        let s: Settings = serde_json::from_str(r#"{"rollouts": 300, "language": "zh-TW"}"#).unwrap();
        assert_eq!(s.font_scale, 1.0);
        assert_eq!(text_scale(s.font_scale), 1.0);
        assert_eq!(text_scale(1.15), 1.15);
        assert_eq!(text_scale(1.2), 1.15);
        assert_eq!(text_scale(5.0), 1.3);
        assert_eq!(text_scale(0.0), 1.0);
        assert_eq!(text_scale(f32::NAN), 1.0);
    }

    /// The JhengHei font collection: the UI face is face 1 (verified with msjh.ttc on this Windows 10 machine);
    /// skipped on an environment without this file.
    #[test]
    fn jhenghei_ui_is_found_by_name() {
        let fonts = std::env::var_os("WINDIR").map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from).join("Fonts");
        let Ok(bytes) = std::fs::read(fonts.join("msjh.ttc")) else { return };
        assert_eq!(face_index(&bytes, "Microsoft JhengHei UI"), Some(1));
        assert_eq!(face_index(&bytes, "Microsoft JhengHei"), Some(0));
        assert_eq!(face_index(&bytes, "No Such Font"), None);
        assert_eq!(face_index(b"not a font", "Microsoft JhengHei UI"), None);
    }
}
