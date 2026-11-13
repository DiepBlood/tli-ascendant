//! The window switches by itself to the page of a game mode that just came up (1.3.0, the "Switch tabs when a game mode
//! is detected" setting, [`super::settings::Settings::auto_tab`]): Tartarus's prison board or Vorax to the Board tab, a
//! map page with its affix panel to the Affixes tab.
//!
//! Rules (decided 2026-11-12):
//! - only on a mode's start (it was not on before): while the same mode stays, a tab the user picked by hand is left
//!   alone; a mode that ended and starts again switches again;
//! - a mode that goes undetected for less than [`DEBOUNCE`] and is then seen again never ended (a popup over the
//!   board, a page opened over the map page, a loading screen): no new start, so no switch;
//! - never away from the Settings tab (the user may be editing a setting), nor while a text field of the window has
//!   the keyboard; a start held so is spent, not kept for later (that would pull the user away later on);
//! - the switch off: modes are still followed, so turning it on in the middle of one switches nothing until the next.
//!
//! The modes are followed one by one, each with its own start: a map page whose closing line never came (the game
//! log can miss it) keeps the affix mode on, but does not stop a Tartarus start from switching.

use std::time::{Duration, Instant};

/// How long a mode must go undetected to have ended. A Tartarus board read and the map page's log lines come in at
/// most a few times a second, a popup or another page over them often lasts under a second: 1.5 s covers those, and a
/// mode really left (the hideout, a map entered) stays away far longer.
pub const DEBOUNCE: Duration = Duration::from_millis(1500);

/// A game mode the window has a page for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Vorax mode (its intro page, board or result screen; `VoraxView::active`).
    Vorax,
    /// Tartarus's prison board on screen (the last read that told showed it, `Session::take_board_shown`).
    Tartarus,
    /// A normal map's detail page with its affix panel (`AffixLive::shown_page`).
    Affix,
}

impl Mode {
    /// In this order when two start on the same tick.
    const ALL: [Mode; 3] = [Mode::Vorax, Mode::Tartarus, Mode::Affix];

    /// The page the mode switches to.
    pub fn target(self) -> Target {
        match self {
            Mode::Vorax | Mode::Tartarus => Target::Board,
            Mode::Affix => Target::Affix,
        }
    }
}

/// The page to switch to (`Tab::Board` and `Tab::Affix` in ui.rs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Board,
    Affix,
}

/// Which modes are detected on this tick.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Seen {
    pub vorax: bool,
    pub tartarus: bool,
    pub affix: bool,
}

impl Seen {
    fn has(self, mode: Mode) -> bool {
        match mode {
            Mode::Vorax => self.vorax,
            Mode::Tartarus => self.tartarus,
            Mode::Affix => self.affix,
        }
    }
}

/// What may hold a switch back on this tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gate {
    /// The setting is on (and the window is not a preview).
    pub enabled: bool,
    /// The Settings tab is on show.
    pub on_settings: bool,
    /// A text field of the window has the keyboard.
    pub typing: bool,
}

impl Gate {
    fn open(self) -> bool {
        self.enabled && !self.on_settings && !self.typing
    }
}

/// One mode's state: on, and since when it went undetected while on.
#[derive(Clone, Copy, Debug, Default)]
struct Track {
    on: bool,
    lost: Option<Instant>,
}

impl Track {
    /// Takes this tick's detection; true when the mode starts. Ticks need not come often: a mode undetected since
    /// `lost` and detected again [`DEBOUNCE`] or more later ended in between, whether or not a tick came then.
    fn update(&mut self, seen: bool, now: Instant) -> bool {
        let ended = |lost: Option<Instant>| lost.is_some_and(|at| now.saturating_duration_since(at) >= DEBOUNCE);
        if seen {
            let started = !self.on || ended(self.lost);
            self.on = true;
            self.lost = None;
            return started;
        }
        if self.on {
            if self.lost.is_none() {
                self.lost = Some(now);
            } else if ended(self.lost) {
                *self = Track::default();
            }
        }
        false
    }
}

/// The modes followed for the automatic switch.
#[derive(Debug, Default)]
pub struct AutoTab {
    tracks: [Track; 3],
}

impl AutoTab {
    /// Takes the modes detected on this tick; the page to switch to when a mode started and nothing holds the switch.
    pub fn update(&mut self, seen: Seen, now: Instant, gate: Gate) -> Option<Target> {
        let mut started = None;
        for (track, mode) in self.tracks.iter_mut().zip(Mode::ALL) {
            if track.update(seen.has(mode), now) && started.is_none() {
                started = Some(mode);
            }
        }
        started.filter(|_| gate.open()).map(Mode::target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OPEN: Gate = Gate { enabled: true, on_settings: false, typing: false };
    const TARTARUS: Seen = Seen { vorax: false, tartarus: true, affix: false };
    const VORAX: Seen = Seen { vorax: true, tartarus: false, affix: false };
    const AFFIX: Seen = Seen { vorax: false, tartarus: false, affix: true };
    const NONE: Seen = Seen { vorax: false, tartarus: false, affix: false };

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Each mode's page, once on its start, and nothing on the ticks after while it stays.
    #[test]
    fn switches_once_when_a_mode_starts() {
        let t0 = Instant::now();
        for (seen, want) in [(TARTARUS, Target::Board), (VORAX, Target::Board), (AFFIX, Target::Affix)] {
            let mut auto = AutoTab::default();
            assert_eq!(auto.update(NONE, t0, OPEN), None);
            assert_eq!(auto.update(seen, t0 + ms(100), OPEN), Some(want), "{seen:?}");
            for i in 2..50 {
                assert_eq!(auto.update(seen, t0 + ms(100 * i), OPEN), None, "{seen:?} still on");
            }
        }
    }

    /// The user picks another tab while the mode stays (ui.rs switches only on a returned target): nothing pulls them
    /// back, however long it lasts, and a flicker under the debounce changes nothing either.
    #[test]
    fn a_tab_picked_by_hand_stays() {
        let t0 = Instant::now();
        let mut auto = AutoTab::default();
        assert_eq!(auto.update(TARTARUS, t0, OPEN), Some(Target::Board));
        // the user goes to the Run tab; the board stays for ten minutes, with popups of a second over it
        for s in 1..600 {
            let seen = if s % 7 == 0 { NONE } else { TARTARUS };
            assert_eq!(auto.update(seen, t0 + ms(1000 * s), OPEN), None, "second {s}");
        }
    }

    /// A mode that ends (undetected for the debounce) and starts again switches again.
    #[test]
    fn switches_again_after_the_mode_ends_and_restarts() {
        let t0 = Instant::now();
        let mut auto = AutoTab::default();
        assert_eq!(auto.update(AFFIX, t0, OPEN), Some(Target::Affix));
        assert_eq!(auto.update(NONE, t0 + ms(1000), OPEN), None);
        assert_eq!(auto.update(NONE, t0 + ms(2600), OPEN), None, "ended here");
        assert_eq!(auto.update(AFFIX, t0 + ms(30_000), OPEN), Some(Target::Affix), "the next map page");
        // with no tick in between: gone at 31 s, seen again at 33 s, more than the debounce later
        assert_eq!(auto.update(NONE, t0 + ms(31_000), OPEN), None);
        assert_eq!(auto.update(AFFIX, t0 + ms(31_000) + DEBOUNCE, OPEN), Some(Target::Affix), "exactly the debounce later");
    }

    /// Detected, gone and back within the debounce (a popup, a page over the map page, a loading screen): no new start.
    #[test]
    fn a_flicker_under_the_debounce_is_no_new_start() {
        let t0 = Instant::now();
        let mut auto = AutoTab::default();
        assert_eq!(auto.update(VORAX, t0, OPEN), Some(Target::Board));
        let mut at = t0;
        for _ in 0..20 {
            at += ms(200);
            assert_eq!(auto.update(NONE, at, OPEN), None);
            at += DEBOUNCE - ms(300);
            assert_eq!(auto.update(NONE, at, OPEN), None);
            at += ms(50);
            assert_eq!(auto.update(VORAX, at, OPEN), None, "back {:?} after it went", DEBOUNCE - ms(250));
        }
        // just one tick for the gap, back just under the debounce
        at += ms(10);
        assert_eq!(auto.update(NONE, at, OPEN), None);
        assert_eq!(auto.update(VORAX, at + DEBOUNCE - ms(1), OPEN), None);
    }

    /// Never away from Settings, nor while typing; the start is spent then, so leaving Settings later during the same
    /// mode switches nothing. The next start switches as usual.
    #[test]
    fn never_from_settings_or_while_typing() {
        let t0 = Instant::now();
        let settings = Gate { on_settings: true, ..OPEN };
        let typing = Gate { typing: true, ..OPEN };
        for gate in [settings, typing] {
            let mut auto = AutoTab::default();
            assert_eq!(auto.update(TARTARUS, t0, gate), None, "{gate:?}");
            assert_eq!(auto.update(TARTARUS, t0 + ms(500), OPEN), None, "left Settings, the same mode");
            assert_eq!(auto.update(NONE, t0 + ms(1000), OPEN), None);
            assert_eq!(auto.update(TARTARUS, t0 + ms(5000), OPEN), Some(Target::Board), "a new start");
        }
    }

    /// The switch off: nothing switches, and turning it on in the middle of a mode switches nothing until the next.
    #[test]
    fn the_setting_off_switches_nothing() {
        let t0 = Instant::now();
        let off = Gate { enabled: false, ..OPEN };
        let mut auto = AutoTab::default();
        for (i, seen) in [AFFIX, NONE, NONE, TARTARUS, VORAX, NONE, AFFIX].into_iter().enumerate() {
            assert_eq!(auto.update(seen, t0 + ms(2000 * i as u64), off), None, "{seen:?}");
        }
        assert_eq!(auto.update(AFFIX, t0 + ms(13_000), OPEN), None, "on again in the middle of a map page");
        assert_eq!(auto.update(NONE, t0 + ms(14_000), OPEN), None);
        assert_eq!(auto.update(AFFIX, t0 + ms(20_000), OPEN), Some(Target::Affix));
    }

    /// Each mode keeps its own start: Tartarus then Vorax switches twice, a map page left on (its closing line missed)
    /// does not stop a Tartarus start, and a mode starting while another stays switches to its own page.
    #[test]
    fn modes_start_on_their_own() {
        let t0 = Instant::now();
        let mut auto = AutoTab::default();
        assert_eq!(auto.update(AFFIX, t0, OPEN), Some(Target::Affix));
        let both = Seen { tartarus: true, ..AFFIX };
        assert_eq!(auto.update(both, t0 + ms(100), OPEN), Some(Target::Board), "Tartarus with a map page stuck on");
        assert_eq!(auto.update(both, t0 + ms(200), OPEN), None);
        assert_eq!(auto.update(VORAX, t0 + ms(300), OPEN), Some(Target::Board), "Vorax after Tartarus");
        // two starting together: the Board first
        let mut auto = AutoTab::default();
        assert_eq!(auto.update(Seen { vorax: true, tartarus: false, affix: true }, t0, OPEN), Some(Target::Board));
    }

    #[test]
    fn modes_go_to_their_pages() {
        assert_eq!(Mode::Tartarus.target(), Target::Board);
        assert_eq!(Mode::Vorax.target(), Target::Board);
        assert_eq!(Mode::Affix.target(), Target::Affix);
    }
}
