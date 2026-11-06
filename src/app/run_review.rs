//! The Run tab's review state: the automatic settlements of the Tartarus run and the Vorax game that ended last
//! ("Latest run"), the finished run the player picked to review instead, and the export card, which exports the run
//! on show (one picker for both, see [`RunReview::export_target`]). Settlements run in the background; this module only holds their channels and results, and a settlement is
//! started through the `spawn` the caller passes (the real ones in the window, plain channels in tests).
//!
//! A picked run is settled apart from the automatic ones, which carry on meanwhile: going back to "Latest run" shows
//! the latest result at once, and the board page's end-of-run note never quotes a picked run. Whatever the picked run
//! settles to is shown, an unfinished run or a failure too (the automatic Tartarus settlement drops an unfinished
//! run's result). A settlement cannot be cancelled, so at most one picked run is settled at a time: a run picked
//! meanwhile waits until it is done (only the newest pick waits), and a run already being settled, by a pick or
//! automatically, is not settled twice.

use super::report::{Game, ReportUi, RunEntry, Target, settled_of};
use super::settle::{Outcome, RunKind, Settled};
use crate::lang::tr;
use eframe::egui;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};

/// A settlement running in the background: its run folder and where its result comes.
type Running = Option<(PathBuf, Receiver<Outcome>)>;

#[derive(Default)]
pub struct RunReview {
    /// The automatic Tartarus settlement running, and the last one's result.
    pub settling: Running,
    pub settled: Option<(PathBuf, Outcome)>,
    /// The same two for the last Vorax game, kept apart from Tartarus's: each is started by its own game's end.
    pub vorax_settling: Running,
    pub vorax_settled: Option<(PathBuf, Outcome)>,
    /// When (UTC ms) the Tartarus and the Vorax game under review ended: "Latest run" shows the one that ended last.
    tartarus_at: i64,
    vorax_at: i64,
    /// The run picked in the run picker; None is "Latest run".
    picked: Option<Picked>,
    /// The one settlement of a picked run running (it may be for a run picked before, then its result is dropped).
    running: Running,
    /// The "Export this run" card, and the list of finished runs the run picker shows.
    pub report: ReportUi,
    /// The width and height the review block was last drawn at with a result (or nothing) on show, not a spinner:
    /// while a run settles the block keeps that height, so the export card below does not jump up and back (see
    /// [`RunReview::block_min`]).
    block: (f32, f32),
}

/// A picked run and its result; None while it is being settled or waits for its turn.
struct Picked {
    entry: RunEntry,
    settled: Option<(PathBuf, Outcome)>,
}

/// Takes the result from `slot` once it is in, emptying it; a settlement thread that died gives a failure, so the
/// spinner does not spin forever.
fn take(slot: &mut Running) -> Option<(PathBuf, Outcome)> {
    let (_, rx) = slot.as_ref()?;
    let out = match rx.try_recv() {
        Ok(out) => out,
        Err(TryRecvError::Empty) => return None,
        Err(TryRecvError::Disconnected) => Outcome::Failed(tr("the review stopped unexpectedly").to_string()),
    };
    slot.take().map(|(dir, _)| (dir, out))
}

fn kind_of(game: Game) -> RunKind {
    match game {
        Game::Tartarus => RunKind::Tartarus,
        Game::Vorax => RunKind::Vorax,
    }
}

impl RunReview {
    /// A Tartarus run's automatic settlement started (`rx`), for the run that ended at `at` (UTC ms). It replaces one
    /// still running. A run picked stays picked: the folder also changes when a run starts or is given up, and only
    /// a run's end ([`RunReview::game_ended`]) goes back to "Latest run".
    pub fn start_tartarus(&mut self, dir: PathBuf, rx: Receiver<Outcome>, at: i64) {
        self.settling = Some((dir, rx));
        self.tartarus_at = at;
    }

    /// A Vorax game's automatic settlement started (`rx`); `at` (UTC ms) is when it ended, None to keep the time (the
    /// same game settled again). A new game's folder goes back to "Latest run"; the same game ending again (its
    /// endgame battle ended after its last round) does not.
    pub fn start_vorax(&mut self, dir: PathBuf, rx: Receiver<Outcome>, at: Option<i64>) {
        let last = self.vorax_settling.as_ref().map(|(d, _)| d).or(self.vorax_settled.as_ref().map(|(d, _)| d));
        let new_game = last != Some(&dir);
        self.vorax_settling = Some((dir, rx));
        if let Some(at) = at {
            self.vorax_at = at;
        }
        if new_game {
            self.game_ended();
        }
    }

    /// A game just ended: the Run tab (and so the export) follows the latest run again, as with "Latest run".
    pub fn game_ended(&mut self) {
        self.latest();
    }

    /// Picks `entry` to review (and export) instead of the latest. Its result is taken from the automatic settlement
    /// when that already settled the same run (even while it works on another run now); else it is settled by the next
    /// [`RunReview::poll`], or follows the automatic or picked settlement already running for it.
    pub fn pick(&mut self, entry: RunEntry) {
        let settled = match entry.game {
            Game::Tartarus => &self.settled,
            Game::Vorax => &self.vorax_settled,
        };
        let reuse = match settled {
            Some((dir, out @ Outcome::Settled(_))) if *dir == entry.dir => Some((dir.clone(), out.clone())),
            _ => None,
        };
        self.picked = Some(Picked { entry, settled: reuse });
    }

    /// "Latest run": the automatic review (and export) of the game that ended last again.
    pub fn latest(&mut self) {
        self.picked = None;
    }

    /// Settles the picked run again (after the language was switched); false when none is picked.
    pub fn again(&mut self) -> bool {
        let Some(p) = &mut self.picked else { return false };
        p.settled = None;
        true
    }

    /// Takes the results that came in, and starts the picked run's settlement with `spawn` when nothing settles it:
    /// no picked settlement running (one at a time) and no automatic one for the same folder.
    pub fn poll(&mut self, spawn: &mut impl FnMut(PathBuf, Game) -> Receiver<Outcome>) {
        if let Some((dir, out)) = take(&mut self.settling) {
            self.auto_result(dir, out, Game::Tartarus);
        }
        if let Some((dir, out)) = take(&mut self.vorax_settling) {
            self.auto_result(dir, out, Game::Vorax);
        }
        if let Some((dir, out)) = take(&mut self.running) {
            self.give_picked(&dir, &out);
        }
        if self.running.is_none()
            && let Some(p) = &self.picked
            && p.settled.is_none()
            && !self.auto_settling(&p.entry.dir)
        {
            let dir = p.entry.dir.clone();
            let rx = spawn(dir.clone(), p.entry.game);
            self.running = Some((dir, rx));
        }
    }

    /// An automatic settlement's result: the picked run gets it when it is the same run; then the Tartarus rule (a run
    /// given up does not replace the result held), and the run list is read again (the game's log is written by now).
    fn auto_result(&mut self, dir: PathBuf, out: Outcome, game: Game) {
        self.give_picked(&dir, &out);
        match game {
            Game::Tartarus => {
                // Abandoning a run midway and restarting (no run_end) also changes the log folder: that run was not
                // finished, so it must not overwrite the settlement already held from the previous run.
                if matches!(out, Outcome::Unfinished) && self.settled.is_some() {
                    return;
                }
                self.settled = Some((dir.clone(), out));
            }
            Game::Vorax => self.vorax_settled = Some((dir.clone(), out)),
        }
        self.report.relist();
    }

    /// The picked run takes `out` when it is for that run and it is still waiting for one.
    fn give_picked(&mut self, dir: &Path, out: &Outcome) {
        if let Some(p) = &mut self.picked
            && p.settled.is_none()
            && p.entry.dir == dir
        {
            p.settled = Some((dir.to_path_buf(), out.clone()));
        }
    }

    fn auto_settling(&self, dir: &Path) -> bool {
        [&self.settling, &self.vorax_settling].into_iter().flatten().any(|(d, _)| d == dir)
    }

    /// The game "Latest run" reviews: the one that ended last of the Tartarus run and the Vorax game under review.
    fn latest_kind(&self) -> RunKind {
        let vorax = self.vorax_settling.is_some() || self.vorax_settled.is_some();
        let tartarus = self.settling.is_some() || self.settled.is_some();
        if vorax && (!tartarus || self.vorax_at > self.tartarus_at) { RunKind::Vorax } else { RunKind::Tartarus }
    }

    /// The folder of the run "Latest run" reviews, if any.
    fn latest_dir(&self) -> Option<PathBuf> {
        let (settling, settled) = match self.latest_kind() {
            RunKind::Tartarus => (&self.settling, &self.settled),
            RunKind::Vorax => (&self.vorax_settling, &self.vorax_settled),
        };
        settling.as_ref().map(|(d, _)| d).or(settled.as_ref().map(|(d, _)| d)).cloned()
    }

    /// Which game the Run tab reviews: the picked run's, else the latest's.
    pub fn kind(&self) -> RunKind {
        self.picked.as_ref().map_or_else(|| self.latest_kind(), |p| kind_of(p.entry.game))
    }

    /// The picked run, None for "Latest run".
    pub fn picked(&self) -> Option<&RunEntry> {
        self.picked.as_ref().map(|p| &p.entry)
    }

    /// What the Run tab shows: whether the run on show is still being settled, and its result.
    pub fn shown(&self) -> (bool, &Option<(PathBuf, Outcome)>) {
        match (&self.picked, self.kind()) {
            (Some(p), _) => (p.settled.is_none(), &p.settled),
            (None, RunKind::Tartarus) => (self.settling.is_some(), &self.settled),
            (None, RunKind::Vorax) => (self.vorax_settling.is_some(), &self.vorax_settled),
        }
    }

    /// Whether any settlement or export re-solve is running (the window keeps the language as it is meanwhile).
    pub fn busy(&self) -> bool {
        self.settling.is_some() || self.vorax_settling.is_some() || self.running.is_some() || self.report.busy()
    }

    /// The minimum height of the review block (number cards, settlement card, largest step) at `width`: while the run
    /// on show is being settled, the height of what was on show before it started (at the same width), so the export
    /// card does not move as the pick is made or while it settles; else none, the result's own height (the export
    /// card may move once, as the result comes in).
    pub fn block_min(&self, width: f32) -> f32 {
        if self.shown().0 && (self.block.0 - width).abs() < 0.5 { self.block.1 } else { 0.0 }
    }

    /// The review block was drawn `height` tall at `width`; kept only when it was not a spinner.
    pub fn block_drawn(&mut self, width: f32, height: f32) {
        if !self.shown().0 {
            self.block = (width, height);
        }
    }

    /// Draws the export card, with the review of the run on show when it is a Tartarus run the export can reuse.
    pub fn show_export(&mut self, ui: &mut egui::Ui, install_id: &str, exe: Option<&Path>) {
        let target = self.export_target();
        // The picked run's review when there is one (an export of another run settles its own).
        let picked: Option<&Settled> = self.picked.as_ref().and_then(|p| settled_of(p.settled.as_ref()));
        let settled = picked.or(settled_of(self.settled.as_ref()));
        self.report.show(ui, &target, settled, install_id, exe);
    }

    /// What the export card exports: the run the Run tab shows, the picked one or in "Latest run" the latest's,
    /// when it is in the list of finished runs. Not listed (a game given up, or an unfinished one), or settled as
    /// unfinished: nothing, and the card says so; not listed yet while the list is read or the run is still being
    /// settled: wait (a run still being settled is never called unfinished from the result before it). "Latest run"
    /// before any game ended this session, so no run is on show: the newest finished run, named on the card.
    pub fn export_target(&self) -> Target {
        let runs = self.report.listed_runs();
        let shown = match &self.picked {
            Some(p) => Some(p.entry.dir.clone()),
            None => self.latest_dir(),
        };
        let Some(dir) = shown else {
            return match runs.first() {
                Some(r) => Target::Newest(r.dir.clone(), r.game, r.title()),
                None if self.report.listing() => Target::Waiting,
                None => Target::Nothing,
            };
        };
        let (settling, out) = self.shown();
        // While it settles, `out` is still the result before it.
        if !settling && matches!(out, Some((_, Outcome::Unfinished))) {
            return Target::Unfinished;
        }
        match runs.iter().find(|r| r.dir == dir) {
            Some(r) => Target::Run(r.dir.clone(), r.game),
            None if settling || self.report.listing() => Target::Waiting,
            None => Target::Unfinished,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::Lang;
    use std::sync::mpsc::{Sender, channel};

    fn entry(sub: &str, name: &str, game: Game) -> RunEntry {
        RunEntry { dir: PathBuf::from(format!("C:/app/{sub}/{name}")), name: name.to_string(), game, bytes: 0 }
    }

    fn tartarus(name: &str) -> RunEntry {
        entry("runs", name, Game::Tartarus)
    }

    fn vorax(name: &str) -> RunEntry {
        entry("vorax", name, Game::Vorax)
    }

    fn settled(folder: &str) -> Outcome {
        Outcome::Settled(Box::new(Settled {
            folder: folder.to_string(),
            vaulted: Some(10),
            actual: Some(10.0),
            expected: None,
            loss: 0.0,
            noise: (0.0, 0),
            luck: None,
            z: None,
            cause: None,
            worst: None,
            scored: 1,
            unknown: 0,
            rollouts: 1,
            kind: RunKind::Tartarus,
            reach: None,
        }))
    }

    /// A stand-in for the window's settlement threads: every start is recorded with the sender of its result.
    #[derive(Default)]
    struct Spawns(Vec<(PathBuf, Game, Sender<Outcome>)>);

    impl Spawns {
        fn spawn(&mut self) -> impl FnMut(PathBuf, Game) -> Receiver<Outcome> + '_ {
            |dir, game| {
                let (tx, rx) = channel();
                self.0.push((dir, game, tx));
                rx
            }
        }

        fn dirs(&self) -> Vec<&Path> {
            self.0.iter().map(|(d, ..)| d.as_path()).collect()
        }

        fn send(&self, k: usize, out: Outcome) {
            self.0[k].2.send(out).unwrap();
        }
    }

    fn poll(review: &mut RunReview, spawns: &mut Spawns) {
        review.poll(&mut spawns.spawn());
    }

    /// What the Run tab shows: (still settling, the result).
    fn shown(review: &RunReview) -> (bool, Option<Outcome>) {
        let (settling, out) = review.shown();
        (settling, out.as_ref().map(|(_, o)| o.clone()))
    }

    /// One picker for the review and the export: the export is the run on show, the picked one or the latest; a run
    /// not in the finished list (given up) or settled as unfinished is not exported; before any game ended this
    /// session, "Latest run" exports the newest finished run; while the list is read or the run settles, wait.
    #[test]
    fn the_export_is_the_run_on_show() {
        let (old, new, other) = (tartarus("2026-11-01_00-00-00"), tartarus("2026-11-02_00-00-00"), vorax("2026-11-01_12-00-00"));
        let run = |e: &RunEntry| Target::Run(e.dir.clone(), e.game);
        let mut review = RunReview::default();
        let mut spawns = Spawns::default();
        assert_eq!(review.export_target(), Target::Waiting, "the list not read yet");
        review.report.take_list(vec![]);
        assert_eq!(review.export_target(), Target::Nothing, "no finished run at all");
        review.report.take_list(vec![other.clone(), old.clone()]);
        assert_eq!(review.export_target(), Target::Newest(other.dir.clone(), other.game, other.title()), "Latest run, no game this session: the newest finished, named");

        // A new Tartarus run ended: wait while it settles and the list is read again, then it.
        let (tx, rx) = channel();
        review.start_tartarus(new.dir.clone(), rx, 2);
        assert_eq!(review.export_target(), Target::Waiting);
        tx.send(settled("new")).unwrap();
        poll(&mut review, &mut spawns);
        assert_eq!(review.export_target(), Target::Waiting, "the list is read again");
        review.report.take_list(vec![new.clone(), other.clone(), old.clone()]);
        assert_eq!(review.export_target(), run(&new));

        // Picked, then "Latest run": the export goes with each.
        review.pick(old.clone());
        assert_eq!(review.export_target(), run(&old), "exported even while it settles");
        review.latest();
        assert_eq!(review.export_target(), run(&new));

        // A Vorax game given up: settled, but not in the finished list.
        let given_up = vorax("2026-11-03_00-00-00");
        let (tx, rx) = channel();
        review.start_vorax(given_up.dir.clone(), rx, Some(3));
        tx.send(Outcome::Empty).unwrap();
        poll(&mut review, &mut spawns);
        review.report.take_list(vec![new.clone(), other.clone(), old.clone()]);
        assert_eq!(review.export_target(), Target::Unfinished);
        // A Tartarus run settled as unfinished (the first of the session, so it is held).
        let mut fresh = RunReview::default();
        fresh.report.take_list(vec![old.clone()]);
        let (tx, rx) = channel();
        fresh.start_tartarus(new.dir.clone(), rx, 1);
        tx.send(Outcome::Unfinished).unwrap();
        poll(&mut fresh, &mut spawns);
        assert_eq!(fresh.export_target(), Target::Unfinished);
        // The next run settling: not called unfinished from the result before it.
        let (_tx, rx) = channel();
        fresh.start_tartarus(tartarus("2026-11-04_00-00-00").dir, rx, 2);
        assert_eq!(fresh.export_target(), Target::Waiting);
        fresh.pick(old.clone());
        assert_eq!(fresh.export_target(), run(&old), "pick one above");
    }

    /// Picks made one after another: one picked settlement at a time, only the newest pick waits, and a result for a
    /// run picked before is dropped; whatever the picked run settles to is shown, an unfinished run, another language
    /// and a dead thread too; "Review again" settles it again.
    #[test]
    fn picks_settle_one_at_a_time_and_show_whatever_they_settle_to() {
        let (x, y, z) = (vorax("2026-11-01_00-00-00"), tartarus("2026-11-02_00-00-00"), tartarus("2026-11-03_00-00-00"));
        let mut review = RunReview::default();
        let mut spawns = Spawns::default();
        assert_eq!(shown(&review), (false, None), "Latest run with no game yet");

        review.pick(x.clone());
        assert_eq!((review.kind(), shown(&review)), (RunKind::Vorax, (true, None)), "the spinner at once");
        poll(&mut review, &mut spawns);
        assert_eq!(spawns.dirs(), [x.dir.as_path()]);
        assert!(review.busy());
        review.pick(y.clone());
        poll(&mut review, &mut spawns);
        review.pick(z.clone());
        poll(&mut review, &mut spawns);
        assert_eq!(spawns.dirs(), [x.dir.as_path()], "x still running: y and z wait, nothing more starts");
        assert_eq!((review.kind(), shown(&review)), (RunKind::Tartarus, (true, None)));

        spawns.send(0, Outcome::Empty);
        poll(&mut review, &mut spawns);
        assert_eq!(spawns.dirs(), [x.dir.as_path(), z.dir.as_path()], "x's result dropped, the newest pick (z) starts, y never");
        assert_eq!(shown(&review), (true, None));
        spawns.send(1, Outcome::Unfinished);
        poll(&mut review, &mut spawns);
        assert_eq!(shown(&review), (false, Some(Outcome::Unfinished)), "an unfinished run is shown, not dropped");
        assert!(!review.busy());

        // Another language, then "Review again" settles it again.
        review.pick(y.clone());
        poll(&mut review, &mut spawns);
        let language = Outcome::Language { log: Lang::En, ui: Lang::ZhTw };
        spawns.send(2, language.clone());
        poll(&mut review, &mut spawns);
        assert_eq!(shown(&review), (false, Some(language)));
        assert!(review.again());
        poll(&mut review, &mut spawns);
        assert_eq!(spawns.dirs().last(), Some(&y.dir.as_path()));
        assert_eq!(shown(&review), (true, None));
        // A dead settlement thread is a failure, not a spinner forever.
        spawns.0.pop();
        poll(&mut review, &mut spawns);
        assert!(matches!(shown(&review), (false, Some(Outcome::Failed(_)))));
    }

    /// A run already being settled is not settled twice: picking the run the automatic settlement is working on waits
    /// for that result (shown even when the automatic rule drops it), and picking the run it already settled shows
    /// that result at once.
    #[test]
    fn a_run_already_settling_is_not_settled_again() {
        let (held, given_up) = (tartarus("2026-11-01_00-00-00"), tartarus("2026-11-02_00-00-00"));
        let mut review = RunReview::default();
        let mut spawns = Spawns::default();
        let (tx, rx) = channel();
        review.start_tartarus(held.dir.clone(), rx, 1);
        tx.send(settled("held")).unwrap();
        poll(&mut review, &mut spawns);

        let (tx, rx) = channel();
        review.start_tartarus(given_up.dir.clone(), rx, 2);
        review.pick(given_up.clone());
        poll(&mut review, &mut spawns);
        assert!(spawns.0.is_empty(), "the automatic settlement is already on it");
        assert_eq!(shown(&review), (true, None));
        tx.send(Outcome::Unfinished).unwrap();
        poll(&mut review, &mut spawns);
        assert_eq!(shown(&review), (false, Some(Outcome::Unfinished)), "the pick shows it");
        assert_eq!(review.settled.as_ref().map(|(d, _)| d.clone()), Some(held.dir.clone()), "the automatic one keeps the run held");

        review.pick(held.clone());
        poll(&mut review, &mut spawns);
        assert!(spawns.0.is_empty(), "already settled: taken as it is");
        assert_eq!(shown(&review), (false, Some(settled("held"))));

        // The automatic settlement busy with another run: the run it settled before is still taken as it is.
        let (_tx, rx) = channel();
        review.start_tartarus(tartarus("2026-11-03_00-00-00").dir, rx, 3);
        review.latest();
        review.pick(held.clone());
        poll(&mut review, &mut spawns);
        assert!(spawns.0.is_empty(), "not settled again");
        assert_eq!(shown(&review), (false, Some(settled("held"))));
    }

    /// Back to "Latest run" only when a game really ends: a Vorax game's first end (a new folder) and a Tartarus
    /// run's end ([`RunReview::game_ended`], called when the run is seen over) clear the pick; the same Vorax game
    /// ending again (its endgame battle) and a Tartarus folder change (a run starting or given up) do not. "Latest
    /// run" picked by hand clears it too, and a result for the dropped pick is never shown.
    #[test]
    fn only_a_game_ending_goes_back_to_latest_run() {
        let (a, b, t) = (vorax("2026-11-01_00-00-00"), vorax("2026-11-02_00-00-00"), tartarus("2026-11-03_00-00-00"));
        let mut review = RunReview::default();
        let mut spawns = Spawns::default();
        review.report.take_list(vec![b.clone(), t.clone()]);
        let (_a_tx, rx) = channel();
        review.start_vorax(a.dir.clone(), rx, Some(1));

        review.pick(t.clone());
        let (_tx, rx) = channel();
        review.start_vorax(a.dir.clone(), rx, Some(2));
        assert!(review.picked().is_some(), "the same game ending again keeps the pick");
        let (_tx, rx) = channel();
        review.start_tartarus(tartarus("2026-11-04_00-00-00").dir, rx, 3);
        assert!(review.picked().is_some(), "a Tartarus folder change keeps the pick");
        let (_tx, rx) = channel();
        review.start_vorax(b.dir.clone(), rx, Some(4));
        assert!(review.picked().is_none(), "a new Vorax game ended");
        assert_eq!(review.export_target(), Target::Run(b.dir.clone(), Game::Vorax), "the export is the game that ended");
        assert_eq!(review.kind(), RunKind::Vorax);

        review.pick(t.clone());
        assert_eq!(review.export_target(), Target::Run(t.dir.clone(), Game::Tartarus));
        review.game_ended();
        assert!(review.picked().is_none(), "a Tartarus run ended");
        assert_eq!(review.export_target(), Target::Run(b.dir.clone(), Game::Vorax), "the export goes back to the latest run too");

        review.pick(t.clone());
        poll(&mut review, &mut spawns);
        review.latest();
        assert_eq!(review.picked(), None);
        spawns.send(0, Outcome::Empty);
        poll(&mut review, &mut spawns);
        assert_eq!(review.picked(), None);
        assert_eq!(shown(&review).1, None, "Latest run shows the Vorax game, still settling");
    }

    /// The review block holds the height of the result on show before a settlement started, only while it settles:
    /// once the new result is in, its own height counts, not the tallest seen; at another width nothing is held.
    #[test]
    fn the_review_block_holds_only_while_settling() {
        let mut review = RunReview::default();
        let mut spawns = Spawns::default();
        review.block_drawn(300.0, 400.0);
        assert_eq!(review.block_min(300.0), 0.0, "nothing settling: no hold");
        review.pick(tartarus("2026-11-01_00-00-00"));
        assert_eq!(review.block_min(300.0), 400.0, "picked, settling: the height before");
        review.block_drawn(300.0, 400.0);
        poll(&mut review, &mut spawns);
        review.block_drawn(300.0, 400.0);
        assert_eq!(review.block_min(300.0), 400.0, "still settling");
        assert_eq!(review.block_min(380.0), 0.0, "another width: no hold");
        spawns.send(0, Outcome::Empty);
        poll(&mut review, &mut spawns);
        assert_eq!(review.block_min(300.0), 0.0, "settled: its own height");
        review.block_drawn(300.0, 120.0);
        review.pick(tartarus("2026-11-02_00-00-00"));
        assert_eq!(review.block_min(300.0), 120.0, "the shorter result's height, not the tallest seen");
    }
}
