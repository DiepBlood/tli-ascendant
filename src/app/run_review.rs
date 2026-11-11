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
//!
//! Every run settled this session, by a pick or automatically, is kept in memory (at most [`CACHED`], the oldest
//! dropped first): picking it again shows it at once instead of settling it again, which is slow for Tartarus and
//! gives slightly different numbers each time. Only settled results are kept, never on disk; switching the language
//! drops them all (their text is in the language they were settled in), and "Review again" settles a run again.

use super::report::{Game, ReportUi, RunEntry, Target, settled_of};
use super::gamelog::RiotEnd;
use super::settle::{Outcome, RunKind, Settled};
use crate::lang::tr;
use eframe::egui;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};

/// A settlement running in the background: its run folder, where its result comes, and the language switch it was
/// started after ([`RunReview::language_changed`]).
type Running = Option<(PathBuf, Receiver<Outcome>, u64)>;

/// How many settled runs [`RunReview`] keeps in memory; a settled result is under 1 KB.
const CACHED: usize = 50;

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
    /// The runs settled this session, oldest first, each an `Outcome::Settled` (see the module notes).
    cache: Vec<(PathBuf, Outcome)>,
    /// How many times the language was switched: a settlement started before the last switch is not kept in memory.
    language: u64,
    /// The riot of the run that ended last, told after its settlement started (see [`RunReview::riot_done`]): put in
    /// the settlement when it comes.
    riot: Option<(PathBuf, RiotEnd)>,
}

/// A picked run and its result; None while it is being settled or waits for its turn.
struct Picked {
    entry: RunEntry,
    settled: Option<(PathBuf, Outcome)>,
}

/// Takes the result from `slot` once it is in, emptying it, with the language switch it was started after; a
/// settlement thread that died gives a failure, so the spinner does not spin forever.
fn take(slot: &mut Running) -> Option<(PathBuf, Outcome, u64)> {
    let (_, rx, _) = slot.as_ref()?;
    let out = match rx.try_recv() {
        Ok(out) => out,
        Err(TryRecvError::Empty) => return None,
        Err(TryRecvError::Disconnected) => Outcome::Failed(tr("the review stopped unexpectedly").to_string()),
    };
    slot.take().map(|(dir, _, language)| (dir, out, language))
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
        self.settling = Some((dir, rx, self.language));
        self.tartarus_at = at;
    }

    /// A Vorax game's automatic settlement started (`rx`); `at` (UTC ms) is when it ended, None to keep the time (the
    /// same game settled again). A new game's folder goes back to "Latest run"; the same game ending again (its
    /// endgame battle ended after its last round) does not.
    pub fn start_vorax(&mut self, dir: PathBuf, rx: Receiver<Outcome>, at: Option<i64>) {
        let last = self.vorax_settling.as_ref().map(|(d, ..)| d).or(self.vorax_settled.as_ref().map(|(d, _)| d));
        let new_game = last != Some(&dir);
        self.vorax_settling = Some((dir, rx, self.language));
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

    /// Picks `entry` to review (and export) instead of the latest. A run settled before this session (by a pick or
    /// automatically, even while the automatic settlement works on another run now) is shown at once from memory;
    /// else it is settled by the next [`RunReview::poll`], or follows the automatic or picked settlement already
    /// running for it.
    pub fn pick(&mut self, entry: RunEntry) {
        let reuse = self.cached(&entry.dir).map(|out| (entry.dir.clone(), out.clone()));
        self.picked = Some(Picked { entry, settled: reuse });
    }

    /// "Latest run": the automatic review (and export) of the game that ended last again.
    pub fn latest(&mut self) {
        self.picked = None;
    }

    /// "Review again" for the run in `dir`: it is dropped from memory, so it is settled anew; the picked run is
    /// settled again by the next [`RunReview::poll`]. False when none is picked (the caller starts the automatic
    /// settlement again).
    pub fn again(&mut self, dir: &Path) -> bool {
        self.cache.retain(|(d, _)| d != dir);
        let Some(p) = &mut self.picked else { return false };
        p.settled = None;
        true
    }

    /// The language was switched: the runs kept in memory were settled in the one before, so they are dropped and
    /// a run picked from now on is settled again in the new one.
    pub fn language_changed(&mut self) {
        self.cache.clear();
        self.language += 1;
    }

    /// The riot battle after the Tartarus run in `dir` is over ([`super::gamelog::Signal::RiotEnd`], about a minute
    /// after the run's settlement started): every settlement of that run held puts it in, and one still running gets
    /// it when it comes. Later settlements of the run read it from its log.
    pub fn riot_done(&mut self, dir: &Path, end: RiotEnd) {
        self.riot = Some((dir.to_path_buf(), end));
        let put = |(d, out): &mut (PathBuf, Outcome)| {
            if d == dir
                && let Outcome::Settled(s) = out
            {
                s.riot = Some(end);
            }
        };
        if let Some(held) = self.settled.as_mut() {
            put(held);
        }
        if let Some(held) = self.picked.as_mut().and_then(|p| p.settled.as_mut()) {
            put(held);
        }
        self.cache.iter_mut().for_each(put);
    }

    /// Puts the riot told before into `out` when it is the settlement of that run and has none (it read the log before
    /// the riot row was written).
    fn with_riot(&self, dir: &Path, out: &mut Outcome) {
        if let (Some((d, end)), Outcome::Settled(s)) = (&self.riot, out)
            && d == dir
            && s.riot.is_none()
            && s.kind == RunKind::Tartarus
        {
            s.riot = Some(*end);
        }
    }

    /// The result kept in memory for the run in `dir`.
    fn cached(&self, dir: &Path) -> Option<&Outcome> {
        self.cache.iter().find(|(d, _)| d == dir).map(|(_, out)| out)
    }

    /// Keeps a settlement's result in memory as the run's latest: a settled one replaces the run's entry as the
    /// newest (the oldest dropped past [`CACHED`]), anything else only drops the entry (a picked run whose
    /// settlement now says another language must not show the old result again).
    /// A settlement started before the last language switch (`language` is not the current count) is not kept: its
    /// text is in the language before.
    fn remember(&mut self, dir: &Path, out: &Outcome, language: u64) {
        self.cache.retain(|(d, _)| d != dir);
        if matches!(out, Outcome::Settled(_)) && language == self.language {
            if self.cache.len() >= CACHED {
                self.cache.remove(0);
            }
            self.cache.push((dir.to_path_buf(), out.clone()));
        }
    }

    /// Takes the results that came in, and starts the picked run's settlement with `spawn` when nothing settles it:
    /// no picked settlement running (one at a time) and no automatic one for the same folder.
    pub fn poll(&mut self, spawn: &mut impl FnMut(PathBuf, Game) -> Receiver<Outcome>) {
        if let Some((dir, mut out, language)) = take(&mut self.settling) {
            self.with_riot(&dir, &mut out);
            self.auto_result(dir, out, language, Game::Tartarus);
        }
        if let Some((dir, out, language)) = take(&mut self.vorax_settling) {
            self.auto_result(dir, out, language, Game::Vorax);
        }
        if let Some((dir, mut out, language)) = take(&mut self.running) {
            self.with_riot(&dir, &mut out);
            // Kept even when the run was picked before (its result is dropped from the pick, not lost).
            self.remember(&dir, &out, language);
            self.give_picked(&dir, &out);
        }
        if self.running.is_none()
            && let Some(p) = &self.picked
            && p.settled.is_none()
            && !self.auto_settling(&p.entry.dir)
        {
            let dir = p.entry.dir.clone();
            let rx = spawn(dir.clone(), p.entry.game);
            self.running = Some((dir, rx, self.language));
        }
    }

    /// An automatic settlement's result: the picked run gets it when it is the same run; then the Tartarus rule (a run
    /// given up does not replace the result held), and the run list is read again (the game's log is written by now).
    fn auto_result(&mut self, dir: PathBuf, out: Outcome, language: u64, game: Game) {
        self.remember(&dir, &out, language);
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
        [&self.settling, &self.vorax_settling].into_iter().flatten().any(|(d, ..)| d == dir)
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
        settling.as_ref().map(|(d, ..)| d).or(settled.as_ref().map(|(d, _)| d)).cloned()
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
            expected_net: None,
            z_net: None,
            cause: None,
            worst: None,
            scored: 1,
            unknown: 0,
            rollouts: 1,
            kind: RunKind::Tartarus,
            reach: None,
            riot: None,
        }))
    }

    fn with_riot(out: Outcome, end: RiotEnd) -> Outcome {
        match out {
            Outcome::Settled(s) => Outcome::Settled(Box::new(Settled { riot: Some(end), ..*s })),
            other => other,
        }
    }

    /// The riot ends about a minute after the run's settlement started: told after the result came, the result on
    /// show and the one in memory take it; told while the settlement still runs, its result takes it when it comes.
    /// Another run's riot changes nothing.
    #[test]
    fn the_riot_goes_into_the_runs_settlement() {
        let (run, next) = (tartarus("2026-11-09_20-14-26"), tartarus("2026-11-09_20-16-20"));
        let end = RiotEnd { fought: true, red: 1, color: 3, medium: 7, ..RiotEnd::default() };
        let mut review = RunReview::default();
        let mut spawns = Spawns::default();
        let (tx, rx) = channel();
        review.start_tartarus(run.dir.clone(), rx, 1);
        tx.send(settled(&run.name)).unwrap();
        poll(&mut review, &mut spawns);
        assert_eq!(shown(&review), (false, Some(settled(&run.name))));
        review.riot_done(&next.dir, end);
        assert_eq!(shown(&review), (false, Some(settled(&run.name))), "another run's riot");
        review.riot_done(&run.dir, end);
        assert_eq!(shown(&review), (false, Some(with_riot(settled(&run.name), end))));
        review.pick(run.clone());
        assert_eq!(shown(&review), (false, Some(with_riot(settled(&run.name), end))), "from memory with it");
        review.latest();

        // Told while the settlement runs: its result takes it.
        let (tx, rx) = channel();
        review.start_tartarus(next.dir.clone(), rx, 2);
        let abandoned = RiotEnd::default();
        review.riot_done(&next.dir, abandoned);
        assert!(shown(&review).0);
        tx.send(settled(&next.name)).unwrap();
        poll(&mut review, &mut spawns);
        assert_eq!(shown(&review), (false, Some(with_riot(settled(&next.name), abandoned))));
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
        assert!(review.again(&y.dir));
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

    /// Picks `e` and, when that starts a settlement, answers it with a settled result; how many settlements the pick
    /// started (0 when it was shown from memory or followed one already running).
    fn pick_settled(review: &mut RunReview, spawns: &mut Spawns, e: &RunEntry) -> usize {
        let before = spawns.0.len();
        review.pick(e.clone());
        poll(review, spawns);
        let started = spawns.0.len() - before;
        if started > 0 {
            spawns.send(spawns.0.len() - 1, settled(&e.name));
            poll(review, spawns);
        }
        started
    }

    /// A run settled once this session, by a pick or automatically, is shown at once when picked again: A, then B,
    /// then A again settles nothing more, and an automatic result is kept too after the automatic settlement moved
    /// on to other runs.
    #[test]
    fn a_run_picked_again_is_shown_from_memory() {
        let (a, b, auto, next) = (tartarus("2026-11-01_00-00-00"), vorax("2026-11-02_00-00-00"), tartarus("2026-11-03_00-00-00"), tartarus("2026-11-04_00-00-00"));
        let mut review = RunReview::default();
        let mut spawns = Spawns::default();
        assert_eq!(pick_settled(&mut review, &mut spawns, &a), 1);
        assert_eq!(pick_settled(&mut review, &mut spawns, &b), 1);
        assert_eq!(pick_settled(&mut review, &mut spawns, &a), 0, "A again: not settled again");
        assert_eq!(shown(&review), (false, Some(settled(&a.name))), "shown at once, not a spinner");
        assert_eq!(pick_settled(&mut review, &mut spawns, &b), 0, "B again too");
        assert_eq!(shown(&review), (false, Some(settled(&b.name))));
        assert_eq!(spawns.dirs(), [a.dir.as_path(), b.dir.as_path()]);

        // An automatic result, then the automatic settlement settles two more runs: still kept.
        review.latest();
        for (k, run) in [&auto, &next, &tartarus("2026-11-05_00-00-00")].into_iter().enumerate() {
            let (tx, rx) = channel();
            review.start_tartarus(run.dir.clone(), rx, k as i64 + 1);
            tx.send(settled(&run.name)).unwrap();
            poll(&mut review, &mut spawns);
        }
        assert_eq!(pick_settled(&mut review, &mut spawns, &auto), 0, "settled automatically before: not settled again");
        assert_eq!(shown(&review), (false, Some(settled(&auto.name))));
        assert_eq!(spawns.0.len(), 2);

        // Only settled results are kept: a run that settled to another language is settled again when picked.
        let other = tartarus("2026-11-06_00-00-00");
        review.pick(other.clone());
        poll(&mut review, &mut spawns);
        spawns.send(2, Outcome::Language { log: Lang::En, ui: Lang::ZhTw });
        poll(&mut review, &mut spawns);
        review.latest();
        review.pick(other.clone());
        poll(&mut review, &mut spawns);
        assert_eq!(spawns.0.len(), 4, "not kept: settled again");
    }

    /// At most CACHED runs are kept: one more drops the oldest, which is settled again when picked, while the
    /// others are still shown from memory.
    #[test]
    fn memory_keeps_the_newest_runs() {
        let runs: Vec<RunEntry> = (0..=CACHED).map(|k| tartarus(&format!("2026-11-01_00-00-{k:02}"))).collect();
        let mut review = RunReview::default();
        let mut spawns = Spawns::default();
        for run in &runs {
            assert_eq!(pick_settled(&mut review, &mut spawns, run), 1);
        }
        assert_eq!(review.cache.len(), CACHED);
        assert_eq!(pick_settled(&mut review, &mut spawns, &runs[1]), 0, "the second oldest is still kept");
        assert_eq!(pick_settled(&mut review, &mut spawns, &runs[CACHED]), 0, "the newest too");
        assert_eq!(pick_settled(&mut review, &mut spawns, &runs[0]), 1, "the oldest was dropped: settled again");
        assert_eq!(review.cache.len(), CACHED);
        assert_eq!(pick_settled(&mut review, &mut spawns, &runs[0]), 0, "and kept again");
    }

    /// Switching the language drops every run kept (their text is in the language before), and "Review again"
    /// settles a kept run anew, picked or not.
    #[test]
    fn a_language_switch_or_review_again_settles_again() {
        let (a, b) = (tartarus("2026-11-01_00-00-00"), tartarus("2026-11-02_00-00-00"));
        let mut review = RunReview::default();
        let mut spawns = Spawns::default();
        assert_eq!(pick_settled(&mut review, &mut spawns, &a), 1);
        assert_eq!(pick_settled(&mut review, &mut spawns, &b), 1);
        review.language_changed();
        assert_eq!(pick_settled(&mut review, &mut spawns, &a), 1, "a language switch: settled again");
        assert_eq!(pick_settled(&mut review, &mut spawns, &b), 1);

        // A settlement running across a language switch (the window does not allow one, but whatever): its result
        // is shown, not kept, so the run is settled again in the new language when picked again.
        let c = tartarus("2026-11-03_00-00-00");
        review.pick(c.clone());
        poll(&mut review, &mut spawns);
        review.language_changed();
        spawns.send(spawns.0.len() - 1, settled(&c.name));
        poll(&mut review, &mut spawns);
        assert_eq!(shown(&review), (false, Some(settled(&c.name))), "shown");
        assert_eq!(pick_settled(&mut review, &mut spawns, &a), 1);
        assert_eq!(pick_settled(&mut review, &mut spawns, &c), 1, "not kept: settled again");
        let automatic = tartarus("2026-11-04_00-00-00");
        let (tx, rx) = channel();
        review.start_tartarus(automatic.dir.clone(), rx, 9);
        review.language_changed();
        tx.send(settled(&automatic.name)).unwrap();
        poll(&mut review, &mut spawns);
        assert_eq!(pick_settled(&mut review, &mut spawns, &automatic), 1, "an automatic one neither");

        // "Review again" on the picked run, shown from memory: settled again, a spinner meanwhile.
        assert_eq!(pick_settled(&mut review, &mut spawns, &a), 1, "the second switch dropped it");
        assert_eq!(pick_settled(&mut review, &mut spawns, &a), 0);
        assert!(review.again(&a.dir));
        assert_eq!(shown(&review), (true, None));
        poll(&mut review, &mut spawns);
        assert_eq!(spawns.dirs().last(), Some(&a.dir.as_path()), "settled again although it was kept");
        assert_eq!(spawns.0.len(), 10);
        spawns.send(9, settled(&a.name));
        poll(&mut review, &mut spawns);
        assert_eq!(shown(&review), (false, Some(settled(&a.name))));
        // "Review again" on "Latest run" (the caller restarts the automatic settlement): the run is dropped too.
        review.latest();
        assert!(!review.again(&b.dir));
        assert_eq!(pick_settled(&mut review, &mut spawns, &b), 1, "dropped by Review again");
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
