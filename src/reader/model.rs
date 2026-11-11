//! A board model from a read S16 screen: the scene gives the cells and the
//! warden, the panels give the step, the day, the doors and the vault. Every
//! low-confidence read and every part the screen does not show becomes a
//! warning for the player to check.

use super::panels::{Panels, Screen};
use super::scene::{CELLS, GLOW_MIN, Scene};
use crate::board::{CellModel, ChoiceKind, EventMark, Model, ObjectiveModel, PreyMark, Step};
use crate::engine::data::{Beast, IRON_LAW, Tool, data};
use crate::engine::devour::eats;
use crate::engine::state::Params;
use crate::engine::text::{beast_name, cell_label, cell_list, incident_term, inmate_name, instrument_term, theme_name, tier_name};
use crate::lang::{tr, trf};

/// Length of an S16 run: the counter starts at 20 days until Riot, except
/// under the Iron Law theme, which extends the run (see [`iron_law_days`]).
pub const RUN_DAYS: i64 = 20;

/// Length of an Iron Law run.
pub fn iron_law_days() -> i64 {
    data().theme(IRON_LAW).and_then(|t| t.value).unwrap_or(RUN_DAYS)
}
/// Padlock, beast, incident and warden reads below this confidence are flagged.
const CONF: f32 = 0.3;
/// Tier reads below this confidence are flagged (tests/reader.rs measures it).
pub const TIER_CONF: f32 = 0.4;

/// The model index of a screen cell number (1 = the Interrogation Room).
fn index(cell: u8) -> usize {
    cell as usize - 1
}

/// Something the reader asks the player to check before trusting the pick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Warning {
    /// The model index of the cell it is about, if any (the warden's cell for a warden read).
    pub cell: Option<usize>,
    pub kind: WarningKind,
    /// The sentence for the command line.
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WarningKind {
    /// A low-confidence or missing read, or a screen the solver can't use as is.
    Read,
    /// The held instruments show as icons only, so the player has to name them.
    HeldToolsUnknown,
    /// A routine line on what was read or kept, such as a pick over the
    /// board or a hidden days counter: nothing to check.
    Note,
    /// The board under a pick may be out of date: the player should open
    /// View Prison Status. Shown on the game too, see
    /// [`crate::app::session::Session::notice`].
    CheckStatus,
}

impl Warning {
    fn read(text: String) -> Warning {
        Warning { cell: None, kind: WarningKind::Read, text }
    }

    pub(crate) fn note(text: String) -> Warning {
        Warning { cell: None, kind: WarningKind::Note, text }
    }

    fn cell(cell: usize, text: String) -> Warning {
        Warning { cell: Some(cell), kind: WarningKind::Read, text }
    }

    fn held_tools_unknown(text: String) -> Warning {
        Warning { cell: None, kind: WarningKind::HeldToolsUnknown, text }
    }
}

impl std::fmt::Display for Warning {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// `under` is the last board read before this frame: an instrument or treasure
/// pick covers the board, so the cells, the warden and the day come from it, or
/// from the run start (every cell locked) without one. On a board frame it
/// gives the cells a tooltip panel covers. `run_days` is the run's length.
pub fn to_model(scene: &Scene, panels: &Panels, held_tools: &[Tool], under: Option<&Model>, run_days: i64) -> (Model, Vec<Warning>) {
    let mut warn = Vec::new();
    let covered = matches!(panels.screen, Screen::InstrumentPick | Screen::TreasurePick);
    let mut m = match under.filter(|_| covered) {
        Some(board) => {
            warn.push(Warning::note(trf!("The pick covers the board: the cells are from the last board read (day {}).", board.day)));
            board.clone()
        }
        None if covered => {
            warn.push(Warning { cell: None, kind: WarningKind::CheckStatus, text: tr("The pick covers the board and no board was read before it: the run start is assumed (every cell locked). For a pick later in the run, read the board first.").to_string() });
            Model::blank()
        }
        None => Model::blank(),
    };
    m.max_days = run_days;
    m.dest.clear();
    if !covered {
        // Only the Blood-Stained Trap's Hunt marks a prey: with the held
        // instruments known and no Trap among them, a mark read is none. A
        // chain's eater, read so mid-bite, was linked to a Lion as its prey,
        // and its rise to come made End chain worth 2.4 Chromatics more for
        // a read (2026-11-02, Claws and Whip held).
        let hunt = held_tools.is_empty() || held_tools.contains(&Tool::Trap);
        read_board(scene, under, &mut m, &mut warn, hunt, held_tools.contains(&Tool::Crown));
    }

    match panels.days {
        Some(left) => m.day = (m.max_days - i64::from(left)).max(1),
        None if covered && under.is_some() => {}
        None => warn.push(Warning::read(tr("The days counter did not read: day 1 assumed.").to_string())),
    }

    match panels.screen {
        Screen::Patrol => {
            m.step = Step::Patrol;
            for (k, door) in panels.doors.iter().enumerate() {
                match door {
                    Some(c) => m.dest.push(index(*c)),
                    None => warn.push(Warning::read(trf!("Card {} did not read: check today's doors.", k + 1))),
                }
            }
            if panels.doors.is_empty() {
                warn.push(Warning::read(tr("No destination card read: check today's doors.").to_string()));
            }
        }
        Screen::Manage => {
            m.step = Step::Manage;
            m.devour_used = !panels.decision.contains(&"Consume");
            m.swap_used = !panels.decision.contains(&"Replace");
            m.hunt_used = !panels.decision.contains(&"Hunt");
        }
        Screen::InstrumentPick => {
            m.step = Step::Choice;
            m.choice.kind = ChoiceKind::Tool;
            m.choice.options = panels.instruments.clone();
            m.choice.rerolls = panels.rerolls;
            if panels.instruments.len() != 3 {
                warn.push(Warning::read(trf!("Read {} of the 3 {}s on offer.", panels.instruments.len(), instrument_term())));
            }
        }
        // The treasure follows the day's consume, which ends the day: as on
        // View Prison Status under it (see below), nothing is left. The board
        // under it keeps a manage screen's flags from before the bite, and a
        // 2026-10-28 pick changed from Three-Headed Thumb Ring to Brass Horn
        // of Strife once View Prison Status was read.
        Screen::TreasurePick => {
            m.step = Step::Choice;
            m.choice.kind = ChoiceKind::Gift;
            m.choice.options.clear();
            m.devour_used = true;
            m.swap_used = true;
            m.hunt_used = true;
        }
        // The Hunt's target pick, played with the Blood-Stained Trap from a
        // Lion's cell, else the Sewer Ratway's cell pick.
        Screen::CellPick => {
            m.step = Step::Choice;
            let lion = usize::try_from(m.warden).ok().and_then(|n| m.cells.get(n)).is_some_and(|c| matches!(c, CellModel::Inmate { beast: Beast::Lion, .. }));
            m.choice.kind = if lion && held_tools.contains(&Tool::Trap) { ChoiceKind::Prey } else { ChoiceKind::Cell };
            m.choice.options.clear();
        }
        Screen::Interrogation => {
            m.step = Step::Choice;
            m.choice.kind = ChoiceKind::Interrogation;
            m.choice.options.clear();
        }
        // The session solves a Consume target pick from the manage board before it.
        Screen::ChainTarget => m.step = Step::Manage,
        // The board under a pick, with no decision on screen: the treasure
        // pick follows the day's consume, so it reads as the manage step with
        // nothing left but to end the day.
        Screen::PrisonStatus => {
            m.step = Step::Manage;
            m.devour_used = true;
            m.swap_used = true;
            m.hunt_used = true;
        }
        Screen::DayBanner => {
            m.step = Step::Manage;
            warn.push(Warning::note(tr("This screen (the day banner) can't be solved: the board is read as the manage step.").to_string()));
        }
        Screen::Popup => {
            m.step = Step::Patrol;
            warn.push(Warning::read(tr("A game dialog covers the board: close it and read again.").to_string()));
        }
        Screen::Theme => {
            m.step = Step::Patrol;
            let name = theme_name(panels.theme).unwrap_or(tr("unknown"));
            warn.push(Warning::note(trf!("The run's theme: {}. Nothing to solve on this screen.", name)));
        }
        Screen::RunEnd => {
            m.step = Step::Patrol;
            warn.push(Warning::note(tr("The riot screen: the run is over, nothing to solve.").to_string()));
        }
        Screen::Unknown => {
            m.step = Step::Patrol;
            warn.push(Warning::read(tr("No destination cards or decision buttons on screen: take the screenshot when they show.").to_string()));
        }
    }

    if !panels.vault.is_empty() {
        m.vault.clear();
    }
    for (k, slot) in panels.vault.iter().enumerate() {
        match slot.beast.filter(|_| slot.conf >= CONF) {
            Some(beast) => m.vault.push(beast),
            None => {
                m.vault.push(Beast::Mammoth);
                warn.push(Warning::read(trf!("Chromatic Prisoner {}: beast not read, {} assumed.", k + 1, beast_name(Beast::Mammoth))));
            }
        }
    }

    // A pick, a tooltip or a dialog can hide the objective panel: the last
    // board's objective stands until the panel reads again.
    m.objective = panels.objective.map(|(id, progress)| ObjectiveModel { id, progress }).or(under.and_then(|b| b.objective));

    m.tools = held_tools.to_vec();
    if held_tools.is_empty() && panels.tool_icons && panels.held.is_none() {
        warn.push(Warning::held_tools_unknown(trf!("The {} panel shows icons the reader can't name: pass them with --tools.", instrument_term())));
    }
    (m, warn)
}

/// The least confidence among the cell reads that `to_model` flags (a
/// door's lock, an incident marker, the glow behind a door, a portrait's
/// beast and rarity), 1 when none is in doubt; tooltip-covered cells are not
/// read. A cell the door-opening flash washes out or a rarity effect hides is 0,
/// so the session reads the board again once the face shows.
pub fn least_sure(scene: &Scene) -> f32 {
    CELLS
        .into_iter()
        .map(|c| scene.cell(c))
        .filter(|r| !r.covered || r.flash || r.shield)
        .map(|r| {
            if r.flash || r.shield {
                return 0.0;
            }
            let mut least = r.locked_conf;
            if r.event {
                least = least.min(r.event_conf);
            }
            if r.locked {
                if r.glow.is_some() {
                    least = least.min(r.glow_conf);
                }
            } else {
                least = least.min(r.beast_conf);
                if r.inmate.is_some() && r.halo >= GLOW_MIN {
                    least = least.min(r.tier_conf);
                }
            }
            least
        })
        .fold(1.0, f32::min)
}

/// The cells and the warden from the board scene. `crown`: the Iron Crown is
/// held, so a Lion may mark any same-rarity inmate as prey.
fn read_board(scene: &Scene, under: Option<&Model>, m: &mut Model, warn: &mut Vec<Warning>, hunt: bool, crown: bool) {
    let mut kept = Vec::new();
    let mut opening = Vec::new();
    let mut shielded = Vec::new();
    let mut unlit = Vec::new();
    for c in CELLS {
        let r = scene.cell(c);
        let n = index(c);
        let at = cell_label(n);
        if r.covered {
            if let Some(board) = under {
                m.cells[n] = board.cells[n].clone();
                if r.flash {
                    opening.push(n);
                } else if r.shield {
                    shielded.push(n);
                } else {
                    kept.push(n);
                }
                continue;
            }
            if r.flash {
                // No board to keep the cell from: a door is the safer guess than an empty cell, which the
                // solver would plan around.
                warn.push(Warning::cell(n, trf!("Check cell {}: the door is still opening, so it is read again.", at)));
                m.cells[n] = CellModel::Sealed { event: r.event.then_some(EventMark::Unknown) };
                continue;
            }
            if r.shield {
                // As for the flash: the face is not read yet, and a door is the safer guess.
                warn.push(Warning::cell(n, trf!("Check cell {}: a rarity effect covers the portrait, so it is read again.", at)));
                m.cells[n] = CellModel::Sealed { event: r.event.then_some(EventMark::Unknown) };
                continue;
            }
            warn.push(Warning::cell(n, trf!("Check cell {}: a tooltip covers it.", at)));
        }
        if r.locked_conf < CONF {
            warn.push(Warning::cell(n, trf!("Check cell {}: read as {} (confidence {}).", at, tr(if r.locked { "locked" } else { "open" }), format!("{:.2}", r.locked_conf))));
        }
        if r.event && r.event_conf < CONF {
            warn.push(Warning::cell(n, trf!("Check cell {}: {} marker read with confidence {}.", at, incident_term(), format!("{:.2}", r.event_conf))));
        }
        let event = r.event.then_some(EventMark::Unknown);
        m.cells[n] = if r.locked {
            match r.glow {
                Some(tier) if r.glow_conf >= TIER_CONF => CellModel::Scratch { tier: i64::from(tier), event },
                Some(tier) => {
                    warn.push(Warning::cell(n, trf!("Check cell {}: the glow behind the locked door read as {} (confidence {}).", at, tier_name(i32::from(tier)), format!("{:.2}", r.glow_conf))));
                    CellModel::Sealed { event }
                }
                None => CellModel::Sealed { event },
            }
        } else {
            match r.inmate {
                Some((beast, tier)) => {
                    // After a reconnect the game draws the portraits with no
                    // rarity glow, so a portrait with none keeps the rarity
                    // the last board read gave the same beast in this cell.
                    let glowless = r.halo < GLOW_MIN;
                    let before = under.and_then(|b| match b.cells[n] {
                        CellModel::Inmate { beast: was, tier, .. } if was == beast => Some(tier),
                        _ => None,
                    });
                    let tier = match before.filter(|_| glowless) {
                        Some(t) => {
                            unlit.push(n);
                            t
                        }
                        None => i64::from(tier),
                    };
                    let name = inmate_name(tier as i32, beast);
                    if r.beast_conf < CONF {
                        warn.push(Warning::cell(n, trf!("Check cell {}: beast read as {} (confidence {}).", at, name, format!("{:.2}", r.beast_conf))));
                    } else if glowless && before.is_none() {
                        warn.push(Warning::cell(n, trf!("Check cell {}: the portrait shows no rarity glow, so it is read as {}.", at, name)));
                    } else if !glowless && r.tier_conf < TIER_CONF {
                        warn.push(Warning::cell(n, trf!("Check cell {}: rarity read as {} (confidence {}).", at, name, format!("{:.2}", r.tier_conf))));
                    }
                    CellModel::Inmate { beast, tier, ivory: r.tusk, prey: None, event }
                }
                None => {
                    if r.beast_conf < CONF {
                        warn.push(Warning::cell(n, trf!("Check cell {}: read as an empty cell (confidence {}).", at, format!("{:.2}", r.beast_conf))));
                    }
                    CellModel::Empty { event }
                }
            }
        };
    }
    if !kept.is_empty() {
        let it = tr(if kept.len() == 1 { "it is" } else { "they are" });
        warn.push(Warning::note(trf!("A tooltip covers {}: {} kept from the last board read.", cell_list(&kept), it)));
    }
    if !opening.is_empty() {
        warn.push(Warning::note(trf!("The door of {} is still opening: kept from the last board read until the portrait shows.", cell_list(&opening))));
    }
    if !shielded.is_empty() {
        warn.push(Warning::note(trf!("A rarity effect covers the portrait in {}: kept from the last board read until the face shows.", cell_list(&shielded))));
    }
    if !unlit.is_empty() {
        warn.push(Warning::note(trf!(
            "The portraits in {} show no rarity glow (the game draws none after a reconnect): the rarities are kept from the last board read.",
            cell_list(&unlit)
        )));
    }

    match scene.warden {
        Some(c) => {
            m.warden = index(c) as i64;
            if scene.warden_conf < CONF {
                warn.push(Warning::cell(index(c), trf!("Check the warden: read at cell {} (confidence {}).", cell_label(index(c)), format!("{:.2}", scene.warden_conf))));
            }
        }
        None => warn.push(Warning::read(tr("The warden's arrow was not found: the warden is put in the Interrogation Room.").to_string())),
    }

    // A Prey crosshair is drawn on the prey only: it goes to a Lion of the
    // same rarity that eats the prey, the warden's first (the Hunt is played
    // from the Lion's cell), else the lowest cell. A Lion holds one mark.
    // While holding the Iron Crown of the Prison King, the Berserk Lion can eat (and so mark) any inmate of the
    // same rarity (see `Diet::can_eat` in engine::devour). Judged by the base food chain alone, the
    // Mammoth or Berserk Lion marked by a Berserk Lion cannot be matched: in the runs of 2026-11-03 and 11-04
    // that held the Crown, this warning appeared 35-72 times per run, and the prey link was
    // lost for the whole stretch.
    for c in CELLS.filter(|&c| hunt && scene.cell(c).prey && !scene.cell(c).covered) {
        let n = index(c);
        let CellModel::Inmate { beast: meal, tier, .. } = m.cells[n] else { continue };
        let free = |l: usize| {
            l != n
                && matches!(&m.cells[l], CellModel::Inmate { beast: Beast::Lion, tier: t, prey: None, .. }
                    if *t == tier && (crown || eats(Beast::Lion).contains(&meal)))
        };
        let lion = usize::try_from(m.warden).ok().filter(|&l| l < m.cells.len() && free(l)).or_else(|| (0..m.cells.len()).find(|&l| free(l)));
        match lion {
            Some(l) => {
                if let CellModel::Inmate { prey, .. } = &mut m.cells[l] {
                    *prey = Some(PreyMark { cell: Some(n as i64), rounds: i64::from(Params::default().prey_rounds) });
                }
            }
            None => warn.push(Warning::cell(n, trf!("Check cell {}: a Prey mark read with no {} to link it to.", cell_label(n), beast_name(Beast::Lion)))),
        }
    }
}
