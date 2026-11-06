//! Game state, as src/engine/types.ts, but fixed-size so a state copies with a memcpy.

use crate::stack::StackVec;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[repr(u8)]
pub enum Archetype {
    #[default]
    Pest,
    Aemberon,
    BoneSentry,
    Agito,
}
pub const ARCHETYPES: [Archetype; 4] = [Archetype::Pest, Archetype::Aemberon, Archetype::BoneSentry, Archetype::Agito];

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[repr(u8)]
pub enum Rarity {
    #[default]
    Normal,
    Magic,
    Rare,
    Boss,
}
pub const RARITIES: [Rarity; 4] = [Rarity::Normal, Rarity::Magic, Rarity::Rare, Rarity::Boss];

impl Rarity {
    pub fn index(self) -> usize {
        self as usize
    }
    pub fn next(self) -> Rarity {
        RARITIES[(self.index() + 1).min(3)]
    }
    /// Boss counts as Rare for condition checks only.
    pub fn is_rare_or_boss(self) -> bool {
        matches!(self, Rarity::Rare | Rarity::Boss)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Monster {
    pub archetype: Archetype,
    pub rarity: Rarity,
    pub activity: i64,
    pub quantity: i64,
}

impl Monster {
    #[inline]
    pub fn total(&self) -> i64 {
        self.activity * self.quantity
    }
}

pub const TANK_COUNT: usize = 6;
pub type Tanks = [Option<Monster>; TANK_COUNT];

#[inline]
pub fn total(m: &Option<Monster>) -> i64 {
    m.map_or(0, |m| m.total())
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Phase {
    #[default]
    InstrumentSelect,
    PotionSelect,
    /// an instrument-only round (11–13) waiting for the player to end it
    RoundEnd,
    GameOver,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CardKind {
    #[default]
    Potion,
    Instrument,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Card {
    pub kind: CardKind,
    pub id: u8,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Equipped {
    pub id: u8,
    pub acquired_round: i32,
}

pub const MAX_HAND: usize = 8;
pub const MAX_EQUIPPED: usize = 8;
pub type Hand = StackVec<Card, MAX_HAND>;
pub type Equips = StackVec<Equipped, MAX_EQUIPPED>;
pub type Targets = StackVec<u8, 2>;
/// Tank indices (at most 6).
pub type TankList = StackVec<u8, TANK_COUNT>;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Sandbox {
    pub start_round: i32,
    pub forced_hand: StackVec<u8, MAX_HAND>,
}

/// Rule switches (added by Ascendant, following the Params convention of Ascendant `src/engine/state.rs`): each field is one place
/// where "the real game differs from the TS original". `ORIGINAL` (all false, also `Default`) is the upstream TS original rules,
/// always used by the parity tests; `GAME` is the game rules we established from the real game, used by the solver (advisor) and
/// `data().default_settings`.
///
/// Evidence (2026-11-03, Traditional Chinese client, two games reconstructed frame by frame):
/// - ranked game the second live-play notes (2026-11-03, private) section 4 (main evidence);
/// - tutorial game the first live-play notes (2026-11-03, private) section 4 (the user said afterwards that this game was a tutorial, so it is only corroboration).
///
/// The sample count of each rule and its "provisional / confirmed" status are summarized in section 7 of the Vorax port notes (2026-11-03, private).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rules {
    /// End-of-round instruments resolve in "reverse pick order": the last picked fires first. false = upstream: always sorted by id.
    ///
    /// The pick order follows `acquired_round` (ascending) of `GameState.instruments`; instruments picked in the same round follow the list
    /// order (in the game flow the list is the push order); when the advisor builds a state from the screen see `ScreenState.instruments`.
    ///
    /// Evidence: in 9 rounds (5-13) of the ranked game the order was always Clustered Eggs (picked at 28k) -> Black Goat Gut Suture (8k) -> Chrysalis Specimen
    /// (opening), which differs from both ascending id (1 -> 9 -> 14) and descending id (14 -> 9 -> 1); the round 5 numbers also show that the Chrysalis Specimen
    /// comes after the fusion (the fusion result has no +8). In the tutorial game, 3 times suture (8k) -> Hypertrophic Pituitary (opening) also matches.
    /// It holds for both games and both combinations; other combinations are unverified, so it is **provisional** (but better grounded than upstream's id sort).
    pub end_order_latest_first: bool,
    /// After a Black Goat Gut Suture fusion the archetype becomes random (uniform over the four archetypes). false = upstream: take the leftmost source (triple[0]).
    ///
    /// The card text only says `融合為1組更高稀有度的怪物` (fuse into 1 group of higher rarity) and does not mention archetype. Across two games only 1 of 9 fusions equaled the leftmost source;
    /// the 5 fusions whose three sources were all Pest gave Bone Sentry, Aemberon, Pest, Aemberon, Bone Sentry. The distribution over the 9 was
    /// Agito 3, Bone Sentry 3, Aemberon 2, Pest 1. With only 9 samples, "uniform over four" cannot be told apart from "some archetypes
    /// excluded / weighted by source", so uniform over four is **provisional**.
    pub suture_random_archetype: bool,
    /// The Chrysalis Specimen's X^2 +8 quantity bonuses each pick 1 group at random, independently and with repetition (the same group may be picked more than X times).
    /// false = upstream: repeat X times "pick X distinct groups at random", each group at most X times.
    ///
    /// Card text: `根據當前擁有的〔蠱蟲〕蠱蟲組數X，隨機X組怪物+8〔數量〕數量，重複生效X次` (based on the number X of Pest groups currently owned, X random groups of monsters get +8 quantity, repeated X times).
    /// Read literally (each trigger picks X distinct groups) each group gets at most X times, but in the 13 rounds of the ranked game the total count was always X^2,
    /// and in 7 places the same group exceeded X (e.g. round 4 with X=5, t4 was picked 9 times), so the data rules out the literal reading,
    /// and the data wins. "X^2 independent, repeatable picks" is the simplest rule that fits the data, **provisional**.
    pub chrysalis_with_replacement: bool,
}

impl Rules {
    /// Upstream TS original rules (used by the parity tests).
    pub const ORIGINAL: Rules = Rules { end_order_latest_first: false, suture_random_archetype: false, chrysalis_with_replacement: false };
    /// Game rules (used by the solver and as the default): all three follow the real game.
    pub const GAME: Rules = Rules { end_order_latest_first: true, suture_random_archetype: true, chrysalis_with_replacement: true };
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Settings {
    pub potion_refreshes: i32,
    pub instrument_refreshes: i32,
    /// Rule switches (added by Ascendant); create_game copies them into GameState.
    pub rules: Rules,
}

/// A sandbox start (SandboxSetup): fixed instruments, round and thresholds, optionally fixed tanks and first hand.
#[derive(Clone, Debug, Default)]
pub struct SandboxSetup {
    pub tanks: Option<Tanks>,
    pub instruments: Vec<Equipped>,
    pub start_round: i32,
    pub thresholds_used: (bool, bool),
    pub forced_hand: Vec<u8>,
    pub seed: Option<u32>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct GameState {
    pub tanks: Tanks,
    pub instruments: Equips,
    pub round: i32,
    pub phase: Phase,
    pub hand: Hand,
    pub open_box: Option<u8>,
    pub selected: Option<u8>,
    pub targets: Targets,
    pub potion_refreshes: i32,
    pub instrument_refreshes: i32,
    pub t8000: bool,
    pub t28000: bool,
    pub pending_picks: i32,
    pub rng_state: u32,
    pub sandbox: Option<Sandbox>,
    /// Rule switches (added by Ascendant). Kept in the state, so they travel along when the solver copies the state for each "future".
    pub rules: Rules,
}

impl GameState {
    #[inline]
    pub fn non_empty(&self) -> TankList {
        let mut out = TankList::new();
        for (i, t) in self.tanks.iter().enumerate() {
            if t.is_some() {
                out.push(i as u8);
            }
        }
        out
    }

    #[inline]
    pub fn monsters(&self) -> impl Iterator<Item = &Monster> {
        self.tanks.iter().flatten()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Action {
    SelectCard { index: i64 },
    ToggleTarget { tank: i64 },
    Confirm,
    Refresh,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EventType {
    Add,
    Remove,
    Mutate,
    Awaken,
    Fuse,
    Devour,
    AddOverflow,
}

impl EventType {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "add" => Self::Add,
            "remove" => Self::Remove,
            "mutate" => Self::Mutate,
            "awaken" => Self::Awaken,
            "fuse" => Self::Fuse,
            "devour" => Self::Devour,
            "addOverflow" => Self::AddOverflow,
            _ => return None,
        })
    }
}

/// The parts of a GameEvent the reactive instruments read.
#[derive(Clone, Copy, Debug)]
pub enum GameEvent {
    Add { tank: usize },
    Remove { monster: Monster },
    Mutate { to: Archetype },
    Awaken,
    Fuse,
    Devour,
    AddOverflow,
}

impl GameEvent {
    pub fn kind(&self) -> EventType {
        match self {
            Self::Add { .. } => EventType::Add,
            Self::Remove { .. } => EventType::Remove,
            Self::Mutate { .. } => EventType::Mutate,
            Self::Awaken => EventType::Awaken,
            Self::Fuse => EventType::Fuse,
            Self::Devour => EventType::Devour,
            Self::AddOverflow => EventType::AddOverflow,
        }
    }
}
