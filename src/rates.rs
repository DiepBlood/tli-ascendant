//! Rate reconciliation: `ascendant-cli rates` scans the run logs (runs\<UTC>\log.jsonl), infers the measured counts
//! and proportions of the random events from adjacent board reads, adds a 95% confidence interval (Wilson), lists them
//! next to the engine's current assumptions (`Params::default()`), and flags the items that fall outside the interval.
//!
//! How each is inferred (every sample requires 2 consecutive equal reads both before and after, to avoid misreads
//! during animations):
//! - Door opening: on the Patrol reads, today's door that is sealed and has no mark; after `Moved`, the inmate read
//!   once the Warden stands on it; discarded when an event (`Incident`) happened in between, when holding the Dog
//!   Tag, or when holding the Blacklist with few doors left.
//! - Replace: `Decision` pressed on the Manage screen while the Replace card is still there (no consume, bite or move
//!   within 0.5 s before or after); later the same day the same cell reads with the Replace card gone or a different
//!   inmate.
//! - Devour re-roll: the eater in the Warden's cell before and after a `Bite`; discarded for the Cage Fight theme (no
//!   re-roll), a rise to Chromatic, no rarity rise (a stale frame from before the bite), or holding the Guillotine or
//!   Maul.
//! - Hunt: after the consecutive prey pick screens following a Decision, whether the Manage screen returns the same
//!   day.

use crate::app::latency::log_millis;
use crate::board::{CellModel, ChoiceKind, Model};
use crate::engine::data::{Beast, CAGE_FIGHT, RAINBOW, Tool, data};
use crate::engine::state::Params;
use crate::engine::text::{beast_name, tier_name};
use crate::lang::zh;
use serde::Deserialize;
use serde_json::Value;
use std::fmt::Write;
use std::path::Path;

/// Upper limit on the gap between other signals of the same action after an action is pressed (`Consume` and
/// `Decision`, `Moved` and `Decision`).
pub(crate) const SAME_CLICK_MS: i64 = 500;

/// The fields used from one log row.
#[derive(Deserialize)]
struct Line {
    at: String,
    kind: String,
    #[serde(default)]
    signal: Option<String>,
    #[serde(default)]
    screen: Option<String>,
    #[serde(default)]
    theme: Option<String>,
    #[serde(default)]
    model: Option<Value>,
}

/// Game log signals; those not needed are folded into Other.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Signal {
    Moved,
    Decision,
    Consume,
    Bite,
    ChainEnd,
    Incident,
    CardsUp,
    ObjectiveDone,
    Vaulted,
    TreasureOpen,
    Other,
}

impl Signal {
    fn parse(s: &str) -> Signal {
        match s {
            "Moved" => Signal::Moved,
            "Decision" => Signal::Decision,
            "Consume" => Signal::Consume,
            "Bite" => Signal::Bite,
            "ChainEnd" => Signal::ChainEnd,
            "ObjectiveDone" => Signal::ObjectiveDone,
            "Vaulted" => Signal::Vaulted,
            "TreasureOpen" => Signal::TreasureOpen,
            _ if s.starts_with("Incident(") => Signal::Incident,
            _ if s.starts_with("CardsUp(") => Signal::CardsUp,
            _ => Signal::Other,
        }
    }

    /// Signals that do not change the board: a card rising, an objective completed.
    pub(crate) fn passive(self) -> bool {
        matches!(self, Signal::CardsUp | Signal::ObjectiveDone)
    }
}

/// One board read.
pub struct Read {
    pub at: String,
    pub screen: String,
    pub model: Model,
    /// The run's theme the session held at the read (the data's English name), which the model lacks.
    pub theme: Option<String>,
}

/// One row of the log: a board read or a game signal; ms is the time (milliseconds).
// A read holds a whole board; the signals are few next to the reads, so the size difference costs little.
#[allow(clippy::large_enum_variant)]
pub enum Event {
    Read(Read),
    Signal { at: String, signal: Signal },
}

impl Event {
    pub(crate) fn at(&self) -> &str {
        match self {
            Event::Read(r) => &r.at,
            Event::Signal { at, .. } => at,
        }
    }

    pub(crate) fn ms(&self) -> i64 {
        log_millis(self.at()).unwrap_or(0)
    }

    pub(crate) fn signal(&self) -> Option<Signal> {
        match self {
            Event::Signal { signal, .. } => Some(*signal),
            Event::Read(_) => None,
        }
    }

    pub(crate) fn read(&self) -> Option<&Read> {
        match self {
            Event::Read(r) => Some(r),
            Event::Signal { .. } => None,
        }
    }
}

/// The log of one run.
pub struct Run {
    pub name: String,
    pub events: Vec<Event>,
    /// Cage Fight theme: devouring does not re-roll the beast.
    pub cage_fight: bool,
}

/// Reads one run's log.jsonl: keeps only the read and game rows, skipping bad rows.
pub fn parse_run(name: &str, text: &str) -> Run {
    let cage = data().theme(CAGE_FIGHT).map(|t| t.name.as_str());
    let mut run = Run { name: name.to_string(), events: Vec::new(), cage_fight: false };
    for line in text.lines() {
        if !line.contains("\"kind\":\"read\"") && !line.contains("\"kind\":\"game\"") {
            continue;
        }
        let Ok(l) = serde_json::from_str::<Line>(line) else { continue };
        match l.kind.as_str() {
            "game" => {
                let Some(s) = l.signal else { continue };
                run.events.push(Event::Signal { at: l.at, signal: Signal::parse(&s) });
            }
            "read" => {
                let Some(model) = l.model else { continue };
                if l.theme.is_some() && l.theme.as_deref() == cage {
                    run.cage_fight = true;
                }
                run.events.push(Event::Read(Read { at: l.at, screen: l.screen.unwrap_or_default(), model: Model::from(model), theme: l.theme }));
            }
            _ => {}
        }
    }
    run
}

/// An inmate: beast and rarity.
pub type Who = (Beast, i64);

/// The inmate in cell n (the marks and tusk that were read are checked separately).
fn inmate(m: &Model, n: i64) -> Option<Who> {
    match usize::try_from(n).ok().and_then(|n| m.cells.get(n)) {
        Some(CellModel::Inmate { beast, tier, .. }) => Some((*beast, *tier)),
        _ => None,
    }
}

/// The last stretch of 2 or more consecutive equal (non-None) values, and the position of the last one in that stretch.
pub(crate) fn last_stable<T: PartialEq + Copy>(vals: &[Option<T>]) -> Option<(T, usize)> {
    let mut end = vals.len();
    while end > 0 {
        let j = end - 1;
        let Some(v) = vals[j] else {
            end = j;
            continue;
        };
        let mut k = j;
        while k > 0 && vals[k - 1] == Some(v) {
            k -= 1;
        }
        if j - k + 1 >= 2 {
            return Some((v, j));
        }
        end = k;
    }
    None
}

/// Door opening: the rarity of the inmate behind today's door.
#[derive(Clone, Debug)]
pub struct Door {
    pub run: String,
    /// The time of the `Moved` signal.
    pub at: String,
    pub cell: usize,
    pub tier: i64,
    pub hook: bool,
}

/// Replace: the one replaced out and the one replaced in.
#[derive(Clone, Debug)]
pub struct Swap {
    pub run: String,
    /// The time of the `Decision` signal.
    pub at: String,
    pub cell: usize,
    pub old: Who,
    pub new: Who,
    /// Holding the Pestle: the incoming one is the same beast, excluded from the beast distribution.
    pub pestle: bool,
    /// Holding the Scapegoat's Guillotine, Burning Stake or Iron Maiden, or the replaced one had a tusk: rarity
    /// follows other rules, excluded from the rarity distribution.
    pub tier_tools: bool,
    /// Holding the Noose: a Replace never brings in a Normal or Magic inmate.
    pub rope: bool,
}

/// One devour bite: the eater before and after the bite.
#[derive(Clone, Debug)]
pub struct BiteRoll {
    pub run: String,
    /// The time of the `Bite` signal.
    pub at: String,
    pub cell: usize,
    pub old: Who,
    pub new: Who,
}

/// Hunt: whether the day ends there.
#[derive(Clone, Debug)]
pub struct Hunt {
    pub run: String,
    /// The time of the last prey pick read.
    pub at: String,
    pub day: i64,
    pub ended: bool,
}

/// All samples and the discard counts.
#[derive(Default)]
pub struct Samples {
    pub runs: usize,
    pub runs_read: usize,
    pub doors: Vec<Door>,
    pub swaps: Vec<Swap>,
    pub bites: Vec<BiteRoll>,
    pub hunts: Vec<Hunt>,
    /// Number of door openings discarded because of an event, Dog Tag or Blacklist.
    pub doors_skipped: usize,
    /// Number of bites under the Cage Fight theme (no re-roll, not counted).
    pub bites_cage: usize,
    /// Number of bites discarded because of a rise to Chromatic, no rarity rise, or holding the Guillotine or Maul.
    pub bites_skipped: usize,
}

impl Samples {
    pub fn merge(&mut self, o: Samples) {
        self.runs += o.runs;
        self.runs_read += o.runs_read;
        self.doors.extend(o.doors);
        self.swaps.extend(o.swaps);
        self.bites.extend(o.bites);
        self.hunts.extend(o.hunts);
        self.doors_skipped += o.doors_skipped;
        self.bites_cage += o.bites_cage;
        self.bites_skipped += o.bites_skipped;
    }
}

/// The reads from `i` forward up to the next signal that changes the board (signals for which `skip` is true are
/// skipped), and the signal that stopped the scan.
pub(crate) fn reads_after(ev: &[Event], i: usize, skip: impl Fn(Signal, i64) -> bool) -> (Vec<&Read>, Option<Signal>) {
    let mut out = Vec::new();
    for e in &ev[i + 1..] {
        match e {
            Event::Read(r) => out.push(r),
            Event::Signal { signal, .. } if signal.passive() || skip(*signal, e.ms()) => {}
            Event::Signal { signal, .. } => return (out, Some(*signal)),
        }
    }
    (out, None)
}

/// The reads from `i` backward to after the previous signal that changes the board, in time order.
pub(crate) fn reads_before(ev: &[Event], i: usize) -> Vec<&Read> {
    let mut out: Vec<&Read> = Vec::new();
    for e in ev[..i].iter().rev() {
        match e {
            Event::Read(r) => out.push(r),
            Event::Signal { signal, .. } if signal.passive() => {}
            Event::Signal { .. } => break,
        }
    }
    out.reverse();
    out
}

/// All samples of one run.
pub fn collect(run: &Run) -> Samples {
    let mut s = Samples { runs: 1, runs_read: usize::from(run.events.iter().any(|e| e.read().is_some())), ..Samples::default() };
    let ev = &run.events;
    for (i, e) in ev.iter().enumerate() {
        match e.signal() {
            Some(Signal::Moved) => door(run, i, &mut s),
            Some(Signal::Decision) => swap(run, i, &mut s),
            Some(Signal::Bite) => bite(run, i, &mut s),
            _ => {}
        }
    }
    hunts(run, &mut s);
    s
}

/// Patrol door opening: today's door as of the last Patrol read before `Moved`, and the inmate read once the Warden
/// stands on it.
fn door(run: &Run, i: usize, s: &mut Samples) {
    let ev = &run.events;
    let Some(pre) = ev[..i].iter().rev().take_while(|e| e.signal() != Some(Signal::Moved)).filter_map(Event::read).find(|r| r.screen == "Patrol") else {
        return;
    };
    let m = &pre.model;
    if m.dest.is_empty() {
        return;
    }
    let t0 = ev[i].ms();
    // A `Decision` arriving together with `Moved` does not count as the next action.
    let (reads, stop) = reads_after(ev, i, |sig, t| sig == Signal::Decision && t - t0 <= SAME_CLICK_MS);
    let vals: Vec<Option<(usize, Who)>> = reads
        .iter()
        .map(|r| {
            let q = &r.model;
            let w = usize::try_from(q.warden).ok()?;
            let ok = matches!(r.screen.as_str(), "Manage" | "Unknown") && q.day == m.day && q.warden != m.warden && m.dest.contains(&w);
            // A cell with a mark triggers an event, so it does not count.
            let clean = matches!(q.cells.get(w), Some(CellModel::Inmate { event: None, .. }));
            if ok && clean { inmate(q, q.warden).map(|who| (w, who)) } else { None }
        })
        .collect();
    let Some(((w, (_, tier)), k)) = last_stable(&vals) else { return };
    if !matches!(m.cells.get(w), Some(CellModel::Sealed { event: None })) {
        return;
    }
    let post = &reads[k].model;
    let blacklist = data().tool(Tool::Blacklist).max_sealed.unwrap_or(0);
    let sealed = post.cells.iter().filter(|c| matches!(c, CellModel::Sealed { .. })).count();
    // Events, the Dog Tag (rarity rises after the door opens) and the Blacklist (rarity rises when few doors are
    // left; leave 1 door of slack) change the rarity.
    if stop == Some(Signal::Incident) || m.tools.contains(&Tool::Dogtag) || (m.tools.contains(&Tool::Blacklist) && sealed <= blacklist + 1) {
        s.doors_skipped += 1;
        return;
    }
    s.doors.push(Door { run: run.name.clone(), at: ev[i].at().to_string(), cell: w, tier, hook: m.tools.contains(&Tool::Hook) });
}

/// Replace: a Decision pressed on the Manage screen while the Replace card is still there, after which the inmate in
/// the same cell changes or the Replace card disappears.
fn swap(run: &Run, i: usize, s: &mut Samples) {
    if let Some(x) = replace_at(run, i) {
        s.swaps.push(x);
    }
}

/// If the Decision at row `i` is a Replace, returns the one replaced out and the one replaced in (shared by `swap`
/// and `audit`).
pub(crate) fn replace_at(run: &Run, i: usize) -> Option<Swap> {
    let ev = &run.events;
    let t = ev[i].ms();
    // A Decision accompanying a consume, bite or move is not a Replace.
    let near = ev.iter().any(|e| matches!(e.signal(), Some(Signal::Consume | Signal::Bite | Signal::Moved | Signal::ChainEnd)) && (e.ms() - t).abs() <= SAME_CLICK_MS);
    if near {
        return None;
    }
    let before = reads_before(ev, i);
    let last = before.last().filter(|r| r.screen == "Manage" && !r.model.swap_used)?;
    let (w, day) = (last.model.warden, last.model.day);
    let pre_vals: Vec<Option<Who>> =
        before.iter().map(|r| (r.screen == "Manage" && r.model.warden == w && r.model.day == day).then(|| inmate(&r.model, w)).flatten()).collect();
    let (old, k) = last_stable(&pre_vals)?;
    let (after, _) = reads_after(ev, i, |_, _| false);
    // Hunt, interrogation, reward pick, instrument pick and chained-consume screens: not a Replace.
    if after.iter().any(|r| matches!(r.screen.as_str(), "CellPick" | "Interrogation" | "TreasurePick" | "InstrumentPick" | "ChainTarget")) {
        return None;
    }
    let here: Vec<&&Read> = after.iter().filter(|r| matches!(r.screen.as_str(), "Manage" | "Unknown") && r.model.warden == w && r.model.day == day).collect();
    let card_gone = here.iter().any(|r| r.screen == "Manage" && r.model.swap_used);
    let post_vals: Vec<Option<Who>> = here.iter().map(|r| inmate(&r.model, w)).collect();
    let (new, _) = last_stable(&post_vals)?;
    if new == old && !card_gone {
        return None;
    }
    let pre = &before[k].model;
    let has = |t: Tool| pre.tools.contains(&t);
    let ivory = pre.cells.get(w as usize).is_some_and(CellModel::ivory);
    Some(Swap {
        run: run.name.clone(),
        at: ev[i].at().to_string(),
        cell: w as usize,
        old,
        new,
        pestle: has(Tool::Pestle),
        tier_tools: has(Tool::Scapegoat) || has(Tool::Stake) || has(Tool::Maiden) || ivory,
        rope: has(Tool::Rope),
    })
}

/// One devour bite: the eater in the Warden's cell before and after the Bite.
fn bite(run: &Run, i: usize, s: &mut Samples) {
    let ev = &run.events;
    let before = reads_before(ev, i);
    let Some(last) = before.iter().rev().find(|r| matches!(r.screen.as_str(), "Manage" | "ChainTarget")) else { return };
    let (w, day) = (last.model.warden, last.model.day);
    let pre_vals: Vec<Option<Who>> = before
        .iter()
        .map(|r| (matches!(r.screen.as_str(), "Manage" | "ChainTarget") && r.model.warden == w && r.model.day == day).then(|| inmate(&r.model, w)).flatten())
        .collect();
    let Some((old, _)) = last_stable(&pre_vals) else { return };
    let (after, stop) = reads_after(ev, i, |_, _| false);
    // Later reads look at the same cell (the next morning's patrol still finds the eater in its old cell); on a
    // covered screen the board may be stale.
    let here: Vec<&&Read> =
        after.iter().filter(|r| matches!(r.screen.as_str(), "Manage" | "ChainTarget" | "Unknown" | "Patrol") && (r.model.day == day || r.model.day == day + 1)).collect();
    let post_vals: Vec<Option<Who>> = here.iter().map(|r| inmate(&r.model, w)).collect();
    let Some((new, _)) = last_stable(&post_vals) else { return };
    if run.cage_fight {
        s.bites_cage += 1;
        return;
    }
    // An eater that rises to Chromatic goes into the vault, and later reads are often stale frames from before the
    // bite: discard when there is a vault signal or the cell is empty. Every bite raises rarity by at least 1 tier,
    // so an unchanged rarity means a read that has not updated yet; Oppress (Fanged Guillotine) and the Maul keep the
    // rarity from rising, so those are discarded as well.
    let tools = &last.model.tools;
    let gone = post_vals.iter().any(Option::is_none);
    if matches!(stop, Some(Signal::Vaulted | Signal::TreasureOpen))
        || gone
        || tools.contains(&Tool::Guillotine)
        || tools.contains(&Tool::Maul)
        || new.1 <= old.1
        || new.1 >= i64::from(RAINBOW)
    {
        s.bites_skipped += 1;
        return;
    }
    s.bites.push(BiteRoll { run: run.name.clone(), at: ev[i].at().to_string(), cell: w as usize, old, new });
}

/// Hunt: 2 or more consecutive prey pick reads after a Decision; look at whether the Manage screen still appears the
/// same day after the last one. The day number on the pick screen may already have turned to the next day, so the day
/// is taken from the first read.
fn hunts(run: &Run, s: &mut Samples) {
    let ev = &run.events;
    for (i, e) in ev.iter().enumerate() {
        if e.signal() != Some(Signal::Decision) {
            continue;
        }
        let mut picks: Vec<(usize, i64)> = Vec::new();
        for (k, x) in ev.iter().enumerate().skip(i + 1) {
            match x {
                Event::Read(r) if r.screen == "CellPick" && r.model.choice.kind == ChoiceKind::Prey => picks.push((k, r.model.day)),
                Event::Read(_) => {}
                Event::Signal { signal, .. } if signal.passive() => {}
                Event::Signal { .. } => break,
            }
        }
        let Some(&(_, day)) = picks.first().filter(|_| picks.len() >= 2) else { continue };
        let Some(&(j, _)) = picks.iter().rev().find(|p| p.1 == day) else { continue };
        let ended = ev[j + 1..].iter().filter_map(Event::read).find_map(|r| {
            if r.model.day > day {
                Some(true)
            } else if r.model.day == day && r.screen == "Manage" {
                Some(false)
            } else {
                None
            }
        });
        if let Some(ended) = ended {
            s.hunts.push(Hunt { run: run.name.clone(), at: ev[j].at().to_string(), day, ended });
        }
    }
}

/// Scans the log.jsonl of every folder under `root`.
pub fn scan(root: &Path) -> Result<Samples, String> {
    use rayon::prelude::*;
    let mut dirs: Vec<_> = std::fs::read_dir(root).map_err(|e| format!("{}: {e}", root.display()))?.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.join("log.jsonl").is_file()).collect();
    dirs.sort();
    let parts: Vec<Samples> = dirs
        .par_iter()
        .map(|d| {
            let name = d.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let text = std::fs::read_to_string(d.join("log.jsonl")).unwrap_or_default();
            collect(&parse_run(&name, &text))
        })
        .collect();
    let mut all = Samples::default();
    for p in parts {
        all.merge(p);
    }
    Ok(all)
}

/// Wilson 95% confidence interval.
pub fn wilson(k: usize, n: usize) -> (f64, f64) {
    if n == 0 {
        return (0.0, 1.0);
    }
    let (k, n, z) = (k as f64, n as f64, 1.96f64);
    let p = k / n;
    let d = 1.0 + z * z / n;
    let c = (p + z * z / (2.0 * n)) / d;
    let h = z * (p * (1.0 - p) / n + z * z / (4.0 * n * n)).sqrt() / d;
    ((c - h).max(0.0), (c + h).min(1.0))
}

/// One comparison row: item, sample count, hit count, engine probability.
struct Row {
    label: String,
    n: usize,
    k: usize,
    engine: f64,
}

/// Picks the text by language.
pub(crate) fn t(en: &'static str, zh_tw: &'static str) -> &'static str {
    if zh() { zh_tw } else { en }
}

/// Display width: CJK and full-width characters count as 2.
pub(crate) fn width(s: &str) -> usize {
    s.chars().map(|c| if ('\u{1100}'..='\u{115f}').contains(&c) || ('\u{2e80}'..='\u{a4cf}').contains(&c) || ('\u{ac00}'..='\u{d7a3}').contains(&c) || ('\u{f900}'..='\u{faff}').contains(&c) || ('\u{fe30}'..='\u{fe4f}').contains(&c) || ('\u{ff00}'..='\u{ff60}').contains(&c) || ('\u{ffe0}'..='\u{ffe6}').contains(&c) { 2 } else { 1 }).sum()
}

pub(crate) fn pad(s: &str, w: usize) -> String {
    format!("{s}{}", " ".repeat(w.saturating_sub(width(s))))
}

/// The probability of item i after normalizing the weights.
fn share(w: &[f64], i: usize) -> f64 {
    let sum: f64 = w.iter().sum();
    if sum > 0.0 { w[i] / sum } else { 0.0 }
}

fn tier_label(t: i64) -> &'static str {
    tier_name(t as i32)
}

/// All sections of the comparison table: (title, rows).
fn sections(s: &Samples) -> Vec<(String, Vec<Row>)> {
    let p = Params::default();
    let tiers = RAINBOW as usize;
    let tw: Vec<f64> = p.tier_weights.iter().map(|&x| f64::from(x)).collect();
    let bw: Vec<f64> = p.beast_weights.iter().map(|&x| f64::from(x)).collect();
    let rw: Vec<f64> = p.replace_weights().iter().map(|&x| f64::from(x)).collect();
    let mut out = Vec::new();

    // 1. The new beast after a Replace, grouped by the original beast.
    let mut rows = Vec::new();
    for b in Beast::ALL {
        let group: Vec<&Swap> = s.swaps.iter().filter(|x| !x.pestle && x.old.0 == b).collect();
        for c in Beast::ALL {
            let engine = if p.replace_new_beast {
                let mut w = rw.clone();
                w[b.index()] = 0.0;
                share(&w, c.index())
            } else {
                share(&rw, c.index())
            };
            let label = format!("{} → {}", beast_name(b), beast_name(c));
            rows.push(Row { label, n: group.len(), k: group.iter().filter(|x| x.new.0 == c).count(), engine });
        }
    }
    out.push((t("Replace: new beast, by the replaced beast (no Pestle)", "換人：新野獸，依原野獸分組（不含研杵）").to_string(), rows));

    // 2. The new rarity after a Replace: the overall distribution, and up / same / down relative to the old rarity.
    let plain: Vec<&Swap> = s.swaps.iter().filter(|x| !x.tier_tools && !x.rope).collect();
    let roped: Vec<&Swap> = s.swaps.iter().filter(|x| !x.tier_tools && x.rope).collect();
    let mut rope_w = tw.clone();
    rope_w[0] = 0.0;
    rope_w[1] = 0.0;
    let mut rows = Vec::new();
    for tier in 0..tiers {
        rows.push(Row { label: tier_label(tier as i64).to_string(), n: plain.len(), k: plain.iter().filter(|x| x.new.1 == tier as i64).count(), engine: share(&tw, tier) });
    }
    out.push((t("Replace: new rarity (no rarity instruments, no Noose)", "換人：新階級（不含改階級的刑具、絞索）").to_string(), rows));
    let mut rows = Vec::new();
    for tier in 0..tiers {
        rows.push(Row { label: tier_label(tier as i64).to_string(), n: roped.len(), k: roped.iter().filter(|x| x.new.1 == tier as i64).count(), engine: share(&rope_w, tier) });
    }
    out.push((t("Replace: new rarity holding the Hangman's Noose", "換人：新階級（持絞索）").to_string(), rows));
    let mut rows = Vec::new();
    for old in 0..tiers {
        let group: Vec<&&Swap> = plain.iter().filter(|x| x.old.1 == old as i64).collect();
        if group.is_empty() {
            continue;
        }
        let total: f64 = tw.iter().sum();
        let up = tw[old + 1..].iter().sum::<f64>() / total;
        let same = tw[old] / total;
        let down = tw[..old].iter().sum::<f64>() / total;
        let name = tier_label(old as i64);
        let k = |f: &dyn Fn(i64) -> bool| group.iter().filter(|x| f(x.new.1)).count();
        rows.push(Row { label: format!("{name} {}", t("up", "升")), n: group.len(), k: k(&|n| n > old as i64), engine: up });
        rows.push(Row { label: format!("{name} {}", t("same", "同")), n: group.len(), k: k(&|n| n == old as i64), engine: same });
        rows.push(Row { label: format!("{name} {}", t("down", "降")), n: group.len(), k: k(&|n| n < old as i64), engine: down });
    }
    out.push((t("Replace: new rarity against the old, by the old (as above)", "換人：新階級相對舊階級，依舊階級分組（同上）").to_string(), rows));

    // 3. The rarity from Patrol door opening, with and without the Sorting Hook.
    let mut hook_w = tw.clone();
    if hook_w[1..].iter().any(|&x| x > 0.0) {
        hook_w[0] = 0.0;
    }
    for (hook, title, weights) in [
        (false, t("Patrol door: rarity behind it (no Sorting Hook)", "巡邏開門：門後囚犯階級（無分類鉤）"), &tw),
        (true, t("Patrol door: rarity behind it (holding the Sorting Hook)", "巡邏開門：門後囚犯階級（持分類鉤）"), &hook_w),
    ] {
        let group: Vec<&Door> = s.doors.iter().filter(|d| d.hook == hook).collect();
        let rows = (0..tiers).map(|tier| Row { label: tier_label(tier as i64).to_string(), n: group.len(), k: group.iter().filter(|d| d.tier == tier as i64).count(), engine: share(weights, tier) }).collect();
        out.push((title.to_string(), rows));
    }

    // 4. Devour re-roll: the proportion that stays the same beast, and the new beast distribution.
    let mut rows = Vec::new();
    for b in Beast::ALL {
        let group: Vec<&BiteRoll> = s.bites.iter().filter(|x| x.old.0 == b).collect();
        rows.push(Row { label: format!("{} {}", beast_name(b), t("stays", "仍同種")), n: group.len(), k: group.iter().filter(|x| x.new.0 == b).count(), engine: share(&bw, b.index()) });
    }
    rows.push(Row { label: t("all stay", "全部仍同種").to_string(), n: s.bites.len(), k: s.bites.iter().filter(|x| x.new.0 == x.old.0).count(), engine: same_beast_odds(s, &bw) });
    out.push((t("Consume: the eater's beast after a bite stays (no Cage Fight)", "吞噬重抽：咬後吃者仍同種（不含籠鬥）").to_string(), rows));
    let rows = Beast::ALL.iter().map(|&c| Row { label: beast_name(c).to_string(), n: s.bites.len(), k: s.bites.iter().filter(|x| x.new.0 == c).count(), engine: share(&bw, c.index()) }).collect();
    out.push((t("Consume: the eater's new beast", "吞噬重抽：咬後吃者的野獸").to_string(), rows));

    // 5. Hunt ending the day.
    let rows = vec![Row { label: t("the day ends", "當天結束").to_string(), n: s.hunts.len(), k: s.hunts.iter().filter(|h| h.ended).count(), engine: if p.hunt_ends_day { 1.0 } else { 0.0 } }];
    out.push((t("Hunt", "獵殺").to_string(), rows));
    out
}

/// The engine probability of staying the same beast after a devour, averaged with weights by the samples' original
/// beast.
fn same_beast_odds(s: &Samples, bw: &[f64]) -> f64 {
    if s.bites.is_empty() {
        return share(bw, 0);
    }
    s.bites.iter().map(|x| share(bw, x.old.0.index())).sum::<f64>() / s.bites.len() as f64
}

/// The full text of the comparison table; with `each`, every sample is appended (for checking).
pub fn report(s: &Samples, each: bool) -> String {
    let mut o = String::new();
    let _ = writeln!(
        o,
        "{} {}, {} {}; {} {}, {} {}, {} {}, {} {}",
        t("folders", "資料夾"),
        s.runs,
        t("with reads", "有讀取"),
        s.runs_read,
        t("doors", "開門"),
        s.doors.len(),
        t("replaces", "換人"),
        s.swaps.len(),
        t("bites", "吞噬口數"),
        s.bites.len(),
        t("hunts", "獵殺"),
        s.hunts.len()
    );
    let _ = writeln!(
        o,
        "{} {}; {} {}, {} {}",
        t("doors left out (incident, Dog Tag, Blacklist)", "開門捨棄（事件、狗牌、黑名單）"),
        s.doors_skipped,
        t("bites in Cage Fight", "籠鬥主題的口數"),
        s.bites_cage,
        t("bites left out (to Chromatic, rarity not risen, Fanged Guillotine or Maul)", "吞噬捨棄（升到綺彩、階級沒升、持斷頭台或巨槌）"),
        s.bites_skipped
    );
    let heads = [t("item", "項目"), "n", t("observed", "實測"), "95% CI", t("engine", "引擎"), t("fits", "符合")];
    let label_w = sections(s).iter().flat_map(|(_, rows)| rows.iter().map(|r| width(&r.label))).max().unwrap_or(0).max(width(heads[0]));
    for (title, rows) in sections(s) {
        let _ = writeln!(o, "\n== {title} ==");
        let _ = writeln!(o, "{}  {:>5}  {}  {:<15}  {}  {}", pad(heads[0], label_w), heads[1], pad(heads[2], 7), heads[3], pad(heads[4], 7), heads[5]);
        for r in rows {
            // Summing an empty set gives -0.0: adding 0.0 makes it print as 0.0.
            let engine = 100.0 * r.engine + 0.0;
            if r.n == 0 {
                let _ = writeln!(o, "{}  {:>5}  {:>7}  {:<15}  {:>6.1}%  -", pad(&r.label, label_w), 0, "-", "-", engine);
                continue;
            }
            let (lo, hi) = wilson(r.k, r.n);
            let fits = r.engine >= lo - 1e-9 && r.engine <= hi + 1e-9;
            let ci = format!("{:.1}-{:.1}%", 100.0 * lo, 100.0 * hi);
            let _ = writeln!(
                o,
                "{}  {:>5}  {:>6.1}%  {:<15}  {:>6.1}%  {}",
                pad(&r.label, label_w),
                r.n,
                100.0 * r.k as f64 / r.n as f64,
                ci,
                engine,
                if fits { t("yes", "是") } else { t("no", "否") }
            );
        }
    }
    let _ = writeln!(
        o,
        "\n{}",
        t(
            "n: samples in the group; observed: share of the item; fits: the engine's value is inside the 95% Wilson interval.",
            "n：該組樣本數；實測：該項的比例；符合：引擎值落在 95% Wilson 區間內。"
        )
    );
    if each {
        let who = |w: &Who| format!("{}{}", beast_name(w.0), w.1);
        let _ = writeln!(o, "\n-- doors --");
        for d in &s.doors {
            let _ = writeln!(o, "{} {} cell{:02} tier{} hook={}", d.run, &d.at[11..23.min(d.at.len())], d.cell + 1, d.tier, d.hook);
        }
        let _ = writeln!(o, "\n-- replaces --");
        for x in &s.swaps {
            let _ = writeln!(o, "{} {} cell{:02} {} -> {} pestle={} tier_tools={} rope={}", x.run, &x.at[11..23.min(x.at.len())], x.cell + 1, who(&x.old), who(&x.new), x.pestle, x.tier_tools, x.rope);
        }
        let _ = writeln!(o, "\n-- bites --");
        for x in &s.bites {
            let _ = writeln!(o, "{} {} cell{:02} {} -> {}", x.run, &x.at[11..23.min(x.at.len())], x.cell + 1, who(&x.old), who(&x.new));
        }
        let _ = writeln!(o, "\n-- hunts --");
        for h in &s.hunts {
            let _ = writeln!(o, "{} {} day{} ended={}", h.run, &h.at[11..23.min(h.at.len())], h.day, h.ended);
        }
    }
    o
}
