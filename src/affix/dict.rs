//! The map page's affixes (Netherrealm Resonance) and matching of read text to them.
//!
//! The table holds one line per sentence: the user decided that one sentence has one grade, whatever its id,
//! bullet colour or rolled numbers. Since 1.2.0 no sentence has a grade of ours: one the user has not graded is framed
//! in the colour the game shows it in (the user, 2026-11-11: "只加文字，原本內建的也都照遊戲的顏色"). The table's grade
//! column keeps the grades it set before ([`Sentence::before`]), for keeping a user's grades as they were
//! ([`keep_grades_before`]); `game` marks a sentence that had none (learned from the game log then). Text is compared after `key`, which turns every number into `#` and keeps only
//! letters, `#` and CJK characters, so "+15% additional Damage" and "-30% additional damage" share one key.

use std::collections::BTreeMap;

/// How strongly the frame warns about an affix.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Grade {
    Danger,
    Warn,
    None,
}

/// The user's grades in place of the defaults (saved in the settings): written under a sentence's [`Sentence::key`],
/// read under any of its ids ([`Sentence::user_grade`]).
pub type Grades = BTreeMap<String, Grade>;

impl Grade {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "danger" => Some(Self::Danger),
            "warn" => Some(Self::Warn),
            "none" => Some(Self::None),
            _ => None,
        }
    }
}

/// One affix sentence of the table.
#[derive(Clone, Debug)]
pub struct Sentence {
    /// Game ids of the sentence (several when the same text comes in several strengths); `ocr-*` when the
    /// affix was only seen on screen.
    pub ids: Vec<String>,
    /// The user's grade when [`Sentence::graded`]; else the table's grade from before 1.2.0 (not a frame's: an
    /// ungraded sentence is framed by the game's colour, [`Sentence::by_game_colour`]).
    pub grade: Grade,
    /// The grade the table set before 1.2.0 (None: `game` in the table, or learned): what a user who had not graded
    /// it saw then ([`keep_grades_before`]).
    pub before: Option<Grade>,
    /// English and Chinese templates (`#` = a rolled number); empty when not seen in that language yet.
    pub en: String,
    pub zh: String,
    en_key: String,
    zh_key: String,
    /// Other wordings of the affix the game writes for another of its ids, either language: a learned text unlike the
    /// sentence's own in that language (5422003 "怪物 #% 腐蝕抗性" joined 5122006 "#% 怪物腐蝕抗性" by their English,
    /// 2026-11-11: its Chinese was dropped and every read of it was unknown). One sentence, one grade, every wording
    /// matched ([`Sentence::keys`]).
    pub alt: Vec<String>,
    alt_keys: Vec<String>,
    /// Learned from the game log, not in the default table ([`Dict::with_learned`]): framed by the game's colour on
    /// the panel until the user grades it (`affix::read`, the user's choice of 2026-11-09).
    pub learned: bool,
    /// The user graded it ([`Dict::with_grades`]).
    pub graded: bool,
    /// A learned affix of another id family joined it by its words alone ([`Dict::with_learned`]): two affixes the
    /// game writes the same, which the game may colour apart (5422009 white and 5634005 red, both "稀有怪物受到 # 米外的傷害
    /// 額外 #%", 2026-11-10). Framed by the game's colour until the user grades it, as a learned one.
    pub twin: bool,
}

impl Sentence {
    /// Framed by the game's colour on the panel rather than by a grade: not graded by the user (since 1.2.0 every
    /// sentence, see the module notes).
    pub fn by_game_colour(&self) -> bool {
        !self.graded
    }

    /// How the sentence is framed: the user's grade, or None for the game's colour. Two sentences framed alike leave a
    /// text between them nothing to tell apart ([`Dict::lookup`]).
    pub fn frame(&self) -> Option<Grade> {
        self.graded.then_some(self.grade)
    }
}

impl Sentence {
    /// The name the user's grade is saved under: the sentence's first id (the table keeps a sentence's ids in order,
    /// and a new id joins at the end).
    pub fn key(&self) -> &str {
        &self.ids[0]
    }

    /// Every key the sentence is matched by: its English and Chinese ones (those it has) and its other wordings'
    /// ([`Sentence::alt`]).
    fn keys(&self) -> impl Iterator<Item = &String> {
        [&self.en_key, &self.zh_key].into_iter().chain(&self.alt_keys).filter(|k| !k.is_empty())
    }

    /// Whether `k` is one of the sentence's keys.
    fn has_key(&self, k: &str) -> bool {
        !k.is_empty() && self.keys().any(|x| x == k)
    }

    /// Known only from the screen: every id is `ocr-*` (its texts were read off the screen, not the game's own).
    fn screen_only(&self) -> bool {
        self.ids.iter().all(|id| id.starts_with("ocr-"))
    }

    /// The user's grade of the sentence: the grade saved under the first of its ids that has one (a sentence that
    /// gained ids, or was folded with another, keeps a grade saved under any of them).
    pub fn user_grade(&self, grades: &Grades) -> Option<Grade> {
        self.ids.iter().find_map(|id| grades.get(id).copied())
    }
}

/// The result of looking up one affix entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// Matched one sentence (or several that share a grade); `exact` when the keys are equal.
    Found { index: usize, grade: Grade, exact: bool },
    /// Close to sentences of different grades: no frame rather than a wrong one.
    Ambiguous,
    /// Not in the table.
    Unknown,
}

/// How close one text came to the matching thresholds, see [`Dict::margin`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Margin {
    /// Edit distance to the nearest sentence.
    pub best: Option<usize>,
    /// The fuzzy limit for this text (3 for Latin text, 1 for CJK text).
    pub allowed: usize,
    /// Edit distance to the nearest sentence of another grade than the nearest one.
    pub rival: Option<usize>,
    /// Characters of the text's key.
    pub chars: usize,
}

const BUILTIN: &str = include_str!("../../data/affix-grades.tsv");

/// Shortest key a cut-off entry needs before a prefix match is tried.
const MIN_PREFIX: usize = 20;
/// Share of a sentence's key a cut-off entry has to show to match it by its start.
const MIN_PREFIX_SHARE: f64 = 0.6;
/// A Chinese text at least this long (key characters) may be two edits off its sentence, when every sentence of
/// another grade is at least [`CJK_LONG_RIVAL`] edits further away (see [`Dict::lookup`]).
const CJK_LONG: usize = 20;
const CJK_LONG_RIVAL: usize = 8;
/// Two sentences whose English keys are this close are one pattern with another keyword (inflict Frosted / inflict
/// Rot: 6 edits).
const EN_SIBLING: usize = 12;

#[derive(Clone, Debug)]
pub struct Dict {
    sentences: Vec<Sentence>,
}

impl Dict {
    /// The default table shipped with the program.
    pub fn builtin() -> Self {
        Self::parse(BUILTIN).expect("data/affix-grades.tsv is well formed")
    }

    /// The table with the affixes learned from the game log ([`super::learned`]), in the list's order. A learned affix
    /// joins the first of these that fits:
    /// - the sentence that has its id;
    /// - the sentence whose text (either language) is its text (one sentence, one grade, whatever its strength);
    /// - the one sentence known only from the screen whose text is within the matching's fuzzy limits of its text (the
    ///   table's `ocr-*` sentences were read off the screen and may differ from the game's own words by a letter or
    ///   two; a new sentence beside one would make reading it ambiguous);
    /// - else a new sentence, not framed ([`Grade::None`], [`Sentence::learned`]), after the table's.
    ///
    /// Joining adds the id and fills the languages the sentence lacks; a sentence known only from the screen takes the
    /// game's words in place of the read ones; other words in a language the sentence has are kept as another wording of
    /// it ([`Sentence::alt`]), unless another sentence has them. When the text filled in is another sentence's already (or near a
    /// screen-only sentence's), the two are one affix and are folded into one (ids and texts together; a learned
    /// sentence folded with a table one takes the table's grade), unless their words in the other language differ
    /// (two affixes the translation does not tell apart: both kept). Grades follow any of a sentence's ids
    /// ([`Dict::with_grades`]), so a fold or a join keeps the grade the user gave. A fold removes a sentence, so the
    /// indices after it move: a table built with a longer list is a new table.
    pub fn with_learned(mut self, learned: &[super::learned::Learned]) -> Self {
        for l in learned {
            let (en_key, zh_key) = (key(&l.en), key(&l.zh));
            if en_key.is_empty() && zh_key.is_empty() {
                continue;
            }
            let by_id = self.sentences.iter().position(|s| s.ids.contains(&l.id));
            let found = by_id
                .or_else(|| self.sentences.iter().position(|s| s.has_key(&en_key) || s.has_key(&zh_key)))
                .or_else(|| self.near_screen(&en_key, &zh_key, None));
            match found {
                Some(i) => {
                    // joined by its words, not its id, to a sentence of game ids none of its family: two affixes
                    let twin = by_id.is_none() && other_family(&self.sentences[i].ids, &l.id);
                    let i = self.absorb(i, l, en_key, zh_key);
                    self.sentences[i].twin |= twin;
                }
                None => self.sentences.push(Sentence {
                    ids: vec![l.id.clone()],
                    grade: Grade::None,
                    before: None,
                    en: l.en.clone(),
                    zh: l.zh.clone(),
                    en_key,
                    zh_key,
                    alt: Vec::new(),
                    alt_keys: Vec::new(),
                    learned: true,
                    graded: false,
                    twin: false,
                }),
            }
        }
        self
    }

    /// The one sentence known only from the screen (`ocr-*` ids), other than `skip`, whose text in a language is
    /// within the fuzzy limits of [`Dict::lookup`] of the given key in that language (Latin text 3 edits, CJK text 1,
    /// at least 0.93 alike); None when there is none or more than one. Sentences with game ids hold the game's own
    /// words: a text near theirs but not equal is another affix (Fire / Cold), left apart.
    fn near_screen(&self, en_key: &str, zh_key: &str, skip: Option<usize>) -> Option<usize> {
        let close = |k: &str, t: &str| {
            if k.is_empty() || t.is_empty() {
                return false;
            }
            let n = k.chars().count();
            let cjk = k.chars().filter(|c| is_cjk(*c)).count() * 2 > n;
            let d = levenshtein(k, t);
            d <= if cjk { 1 } else { 3 } && 1.0 - d as f64 / n.max(t.chars().count()) as f64 >= 0.93
        };
        let mut hits = (0..self.sentences.len()).filter(|&i| {
            let s = &self.sentences[i];
            Some(i) != skip && s.screen_only() && (close(en_key, &s.en_key) || close(zh_key, &s.zh_key))
        });
        match (hits.next(), hits.next()) {
            (Some(i), None) => Some(i),
            _ => None,
        }
    }

    /// Learned affix `l` joins sentence `i` (see [`Dict::with_learned`]); the sentence's index after it (a sentence
    /// folded into it before it moves it up).
    fn absorb(&mut self, mut i: usize, l: &super::learned::Learned, en_key: String, zh_key: String) -> usize {
        let screen_only = self.sentences[i].screen_only();
        if !self.sentences[i].ids.contains(&l.id) {
            self.sentences[i].ids.push(l.id.clone());
        }
        for (zh, text, k) in [(false, &l.en, en_key), (true, &l.zh, zh_key)] {
            // a text with no letters left in its key (only marks) matches nothing
            if text.is_empty() || k.is_empty() {
                continue;
            }
            let lang_key = |s: &Sentence| if zh { s.zh_key.clone() } else { s.en_key.clone() };
            let other_key = |s: &Sentence| if zh { s.en_key.clone() } else { s.zh_key.clone() };
            let have = lang_key(&self.sentences[i]);
            if have == k && screen_only {
                // the same words as read off the screen: the game's own text shown in their place
                let s = &mut self.sentences[i];
                if zh { s.zh = text.clone() } else { s.en = text.clone() }
                continue;
            }
            if have == k || self.sentences[i].alt_keys.contains(&k) {
                continue;
            }
            if !have.is_empty() && !screen_only {
                // Another wording of the affix (the game's own, for this id): kept as one, matched as the sentence's
                // own. Not when another sentence has these words: two affixes that read the same, left to that one.
                if !self.sentences.iter().enumerate().any(|(j, s)| j != i && s.has_key(&k)) {
                    let s = &mut self.sentences[i];
                    s.alt.push(text.clone());
                    s.alt_keys.push(k);
                }
                continue;
            }
            // The same words are another sentence's (or near a screen-only one's): one affix, folded into this one. Not
            // when the two sentences have different words in the other language: two affixes that read the same in
            // this one (the translation does not tell them apart); both stay, each with its own words and grade.
            let (en_k, zh_k) = if zh { ("", k.as_str()) } else { (k.as_str(), "") };
            let twin = (0..self.sentences.len())
                .find(|&j| j != i && lang_key(&self.sentences[j]) == k)
                .or_else(|| self.near_screen(en_k, zh_k, Some(i)))
                .filter(|&j| {
                    let (a, b) = (other_key(&self.sentences[i]), other_key(&self.sentences[j]));
                    a.is_empty() || b.is_empty() || a == b
                });
            if let Some(j) = twin {
                let other = self.sentences.remove(j);
                if j < i {
                    i -= 1;
                }
                let s = &mut self.sentences[i];
                for id in other.ids {
                    if !s.ids.contains(&id) {
                        s.ids.push(id);
                    }
                }
                if s.en.is_empty() {
                    (s.en, s.en_key) = (other.en, other.en_key);
                }
                if s.zh.is_empty() {
                    (s.zh, s.zh_key) = (other.zh, other.zh_key);
                }
                for (text, k) in other.alt.into_iter().zip(other.alt_keys) {
                    if !s.alt_keys.contains(&k) {
                        s.alt.push(text);
                        s.alt_keys.push(k);
                    }
                }
                if s.learned && !other.learned {
                    (s.grade, s.before, s.learned) = (other.grade, other.before, false);
                }
                s.twin |= other.twin;
            }
            // These words are now this sentence's own: another sentence that kept them as another wording lets them go.
            // A sentence's own words win over another's other wording, as when the other wording comes second (it is
            // not kept, above); without this the result hung on the order learned: Z1 learned first as B's other
            // wording, then as A's own, matched both, and the read was ambiguous or the wrong one (review of 1.3.0).
            for (j, other) in self.sentences.iter_mut().enumerate() {
                if j != i
                    && let Some(p) = other.alt_keys.iter().position(|x| *x == k)
                {
                    other.alt.remove(p);
                    other.alt_keys.remove(p);
                }
            }
            let s = &mut self.sentences[i];
            if zh {
                (s.zh, s.zh_key) = (text.clone(), k);
            } else {
                (s.en, s.en_key) = (text.clone(), k);
            }
        }
        i
    }

    /// The table with the user's grades in place of the defaults, found by any of a sentence's ids (the first id the
    /// user graded wins; ids the table does not have are left out).
    pub fn with_grades(mut self, grades: &Grades) -> Self {
        for s in &mut self.sentences {
            if let Some(g) = s.user_grade(grades) {
                (s.grade, s.graded) = (g, true);
            }
        }
        self
    }

    /// Parses "grade<TAB>ids<TAB>English<TAB>Chinese" lines; `#` lines are comments.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut sentences = Vec::new();
        for (n, line) in text.lines().enumerate() {
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }
            let p: Vec<&str> = line.split('\t').collect();
            if p.len() != 4 {
                return Err(format!("line {}: {} fields", n + 1, p.len()));
            }
            // `game`: no grade of ours before 1.2.0 either
            let before = if p[0] == "game" { None } else { Some(Grade::parse(p[0]).ok_or_else(|| format!("line {}: grade {:?}", n + 1, p[0]))?) };
            let grade = before.unwrap_or(Grade::None);
            let (en, zh) = (p[2].trim().to_string(), p[3].trim().to_string());
            if en.is_empty() && zh.is_empty() {
                return Err(format!("line {}: no text", n + 1));
            }
            sentences.push(Sentence {
                ids: p[1].split(',').map(str::to_string).collect(),
                grade,
                before,
                en_key: key(&en),
                zh_key: key(&zh),
                alt: Vec::new(),
                alt_keys: Vec::new(),
                en,
                zh,
                learned: false,
                graded: false,
                twin: false,
            });
        }
        Ok(Self { sentences })
    }

    pub fn sentences(&self) -> &[Sentence] {
        &self.sentences
    }

    /// Finds the sentence of one affix entry read from the screen.
    ///
    /// `cut` marks an entry whose end may be missing (the last entry of a panel taller than the screen area);
    /// it may then match the start of a sentence. Fuzzy matches are kept tight because similar affixes differ by
    /// one word (Fire / Cold, deal / take, 致殘 / 燒灼): at most 3 edits for Latin text and 1 for CJK text.
    /// Any sentence framed otherwise ([`Sentence::frame`]: a grade of the user's, or another one) close to the text
    /// makes the result `Ambiguous`, whether or not it passed
    /// the fuzzy limits itself: within two edits of the best match, or within one edit of an exact match. A Chinese
    /// text of at least [`CJK_LONG`] characters two edits off its nearest sentence matches it too when every sentence
    /// of another grade is at least [`CJK_LONG_RIVAL`] edits further.
    pub fn lookup(&self, text: &str, cut: bool) -> Lookup {
        let k = key(text);
        if k.is_empty() {
            return Lookup::Unknown;
        }
        let n = k.chars().count();
        let cjk = k.chars().filter(|c| is_cjk(*c)).count() * 2 > n;
        let allowed = if cjk { 1 } else { 3 };
        // every sentence's distance to the text (its nearer language), and whether it passes the fuzzy limits
        let mut all: Vec<(usize, usize, bool)> = Vec::with_capacity(self.sentences.len());
        for (i, s) in self.sentences.iter().enumerate() {
            let best = s
                .keys()
                .map(|t| {
                    let d = levenshtein(&k, t);
                    let longest = n.max(t.chars().count());
                    (d, d == 0 || (d <= allowed && 1.0 - d as f64 / longest as f64 >= 0.93))
                })
                .min_by_key(|&(d, pass)| (d, !pass));
            if let Some((d, pass)) = best {
                all.push((d, i, pass));
            }
        }
        all.sort();
        if let Some(&(d, i, _)) = all.iter().find(|x| x.2) {
            let (grade, frame) = (self.sentences[i].grade, self.sentences[i].frame());
            let window = if d == 0 { 1 } else { d + 2 };
            if all.iter().any(|&(d2, j, _)| j != i && d2 <= window && self.sentences[j].frame() != frame) {
                return Lookup::Ambiguous;
            }
            return Lookup::Found { index: i, grade, exact: d == 0 };
        }
        // A long Chinese text two edits off one sentence: OCR reading a keyword wrong (凍僵 as 冫崬僵, 麻痺 dropped;
        // 2026-11-09). Only when that sentence plainly wins, else no frame rather than a wrong one:
        // - not two neighbouring characters replaced in a text of the same length: that is another keyword in the
        //   same sentence pattern (施加凍僵 / 施加腐爛 are two edits apart), not a misread one;
        // - every other sentence at least one edit further, and every sentence of another grade [`CJK_LONG_RIVAL`]
        //   further (凍僵 read as a bare 冫 is two off 凍僵 and two off 腐爛: neither);
        // - no sentence with no Chinese yet that may be this one's sibling in the pattern (its English within
        //   [`EN_SIBLING`] edits of this sentence's): its Chinese, unknown, might be the text read (3500302 had none
        //   in the table, and its text was then two edits off 3500304's); a sentence with no English can not be
        //   checked so, and gets no such match.
        if cjk
            && n >= CJK_LONG
            && let Some(&(2, i, _)) = all.first()
            && !self.sentences[i].keys().any(|t| swapped_pair(&k, t))
            && all.get(1).is_none_or(|&(d2, _, _)| d2 > 2)
            && !self.sentences[i].en_key.is_empty()
        {
            let (grade, frame) = (self.sentences[i].grade, self.sentences[i].frame());
            let rivals_far = all.iter().all(|&(d2, j, _)| j == i || self.sentences[j].frame() == frame || d2 >= 2 + CJK_LONG_RIVAL);
            let en = &self.sentences[i].en_key;
            let unknown_sibling = self
                .sentences
                .iter()
                .enumerate()
                .any(|(j, s)| j != i && s.zh_key.is_empty() && !s.en_key.is_empty() && levenshtein(en, &s.en_key) <= EN_SIBLING);
            if rivals_far && !unknown_sibling {
                return Lookup::Found { index: i, grade, exact: false };
            }
        }
        if cut && n >= MIN_PREFIX {
            return self.lookup_start(&k, cjk);
        }
        Lookup::Unknown
    }

    /// The one sentence a text read from the screen equals, when [`Dict::lookup`] leaves the text ambiguous only for
    /// sentences of another grade one character away, each by a character the OCR is not known to swap
    /// ([`OCR_SWAPS`]): 玩家低魔時 / 低血時 / 滿魔時額外受到 #% 傷害 (5521013, 5521015, 5521014, 2026-11-10). Such a
    /// text is taken for that sentence once it reads the same twice (`read::ExactSeen`). None when no sentence equals
    /// it or two do, or when a sentence of another grade is a character added or dropped away (the OCR drops
    /// characters) or a known swap away.
    pub fn exact_only(&self, text: &str) -> Option<usize> {
        let k = key(text);
        if k.is_empty() {
            return None;
        }
        let equal: Vec<usize> = (0..self.sentences.len()).filter(|&i| self.sentences[i].has_key(&k)).collect();
        let [i] = equal[..] else { return None };
        let frame = self.sentences[i].frame();
        let read: Vec<char> = k.chars().collect();
        for (j, s) in self.sentences.iter().enumerate() {
            if j == i || s.frame() == frame {
                continue;
            }
            for t in s.keys() {
                // an exact match is ambiguous only for sentences of another grade within one edit
                if levenshtein(&k, t) > 1 {
                    continue;
                }
                let other: Vec<char> = t.chars().collect();
                if other.len() != read.len() {
                    return None;
                }
                let (a, b) = read.iter().zip(&other).find(|(a, b)| a != b).map(|(a, b)| (*a, *b))?;
                if swappable(a, b) {
                    return None;
                }
            }
        }
        Some(i)
    }

    /// The user's grade of the sentence a text not found is nearest, when it is within two edits beyond the fuzzy limit
    /// and the user graded it ([`Sentence::graded`]) otherwise than the table did before 1.2.0 ([`Sentence::before`]): a misread of an affix the user graded keeps the user's grade,
    /// not the game's colour (a red affix the user set to not framed stays not framed; review of 2026-11-09). Sentences
    /// as near as the nearest must all be graded by the user alike, else none (the game's colour decides).
    pub fn near_user_grade(&self, text: &str) -> Option<Grade> {
        let k = key(text);
        let n = k.chars().count();
        if n == 0 {
            return None;
        }
        let allowed = if k.chars().filter(|c| is_cjk(*c)).count() * 2 > n { 1 } else { 3 };
        let near: Vec<(usize, &Sentence)> = self
            .sentences
            .iter()
            .filter_map(|s| s.keys().map(|t| levenshtein(&k, t)).min().map(|d| (d, s)))
            .collect();
        let d = near.iter().map(|(d, _)| *d).min()?;
        // a grade equal to the table's from before 1.2.0 counts as none: such grades were not saved then (equal to the
        // default), and the ones kept since (`keep_grades_before`) must not frame misreads otherwise than before
        let mut nearest = near.iter().filter(|(e, _)| *e == d).map(|(_, s)| (s.graded && s.before != Some(s.grade)).then_some(s.grade));
        let grade = nearest.next().flatten()?;
        (d <= allowed + 2 && nearest.all(|g| g == Some(grade))).then_some(grade)
    }

    /// How close a text comes to the matching thresholds (for checks): the nearest sentence's edit distance with the
    /// fuzzy limit for this text, and the distance of the nearest sentence of another grade than that one.
    pub fn margin(&self, text: &str) -> Margin {
        let k = key(text);
        let n = k.chars().count();
        let cjk = n > 0 && k.chars().filter(|c| is_cjk(*c)).count() * 2 > n;
        let dist: Vec<usize> = self
            .sentences
            .iter()
            .map(|s| s.keys().map(|t| levenshtein(&k, t)).min().unwrap_or(usize::MAX))
            .collect();
        let best = (0..dist.len()).min_by_key(|&i| dist[i]);
        let rival = best.and_then(|b| (0..dist.len()).filter(|&j| self.sentences[j].frame() != self.sentences[b].frame()).map(|j| dist[j]).min());
        Margin { best: best.map(|b| dist[b]), allowed: if cjk { 1 } else { 3 }, rival, chars: n }
    }

    /// A cut entry: the one sentence the text is the start of, when the text covers at least
    /// [`MIN_PREFIX_SHARE`] of it. Several sentences, or too short a start, give no frame: an affix not in the
    /// table may start the same way ("Rare monsters gain ..." begins several affixes).
    fn lookup_start(&self, k: &str, cjk: bool) -> Lookup {
        let n = k.chars().count();
        let limit = if cjk { 0 } else { 1 };
        let starts: Vec<(usize, usize)> = (0..self.sentences.len())
            .filter_map(|i| {
                let s = &self.sentences[i];
                s.keys()
                    .filter(|t| prefix_distance(k, t) <= limit)
                    .map(|t| t.chars().count())
                    .min()
                    .map(|len| (i, len))
            })
            .collect();
        match starts.as_slice() {
            [] => Lookup::Unknown,
            [(i, len)] if n as f64 >= *len as f64 * MIN_PREFIX_SHARE => Lookup::Found { index: *i, grade: self.sentences[*i].grade, exact: false },
            [_] => Lookup::Unknown,
            _ => Lookup::Ambiguous,
        }
    }
}

/// Whether `a` and `b` are as long and differ in exactly two neighbouring characters (another two-character keyword).
fn swapped_pair(a: &str, b: &str) -> bool {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.len() != b.len() {
        return false;
    }
    let diff: Vec<usize> = (0..a.len()).filter(|&i| a[i] != b[i]).collect();
    matches!(diff.as_slice(), [x, y] if y - x == 1)
}

fn is_cjk(c: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&c)
}

fn is_digit(c: char) -> bool {
    c.is_ascii_digit() || ('\u{ff10}'..='\u{ff19}').contains(&c)
}

/// Comparable form of an affix text: numbers become `#`, then only `a-z`, `#` and CJK characters stay.
///
/// Also undoes two OCR habits: a leading bullet read as "+ " is dropped (but "+15%" is kept), and 一 before a
/// number (a minus sign read as the CJK character one) becomes "-".
pub fn key(text: &str) -> String {
    let mut t = text.trim();
    let mut chars = t.chars();
    if chars.next() == Some('+') {
        let rest = chars.as_str();
        let after = rest.trim_start();
        let spaced = after.len() < rest.len();
        if spaced && !after.is_empty() && !after.starts_with(is_digit) {
            t = after;
        }
    }
    let src: Vec<char> = t.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < src.len() {
        let c = src[i];
        let sign = matches!(c, '+' | '-' | '\u{2014}' | '\u{2013}') || (c == '一' && next_digit(&src, i + 1));
        let start = if sign && next_digit(&src, i + 1) { Some(i + 1) } else if is_digit(c) { Some(i) } else { None };
        if let Some(mut j) = start {
            // skip spaces between 一 and the number, as the source pattern allows
            while j < src.len() && src[j].is_whitespace() {
                j += 1;
            }
            loop {
                while j < src.len() && is_digit(src[j]) {
                    j += 1;
                }
                // digits split by spaces are one number: the OCR reads 15 as "1 5" at times (2026-11-11). No game text
                // has two numbers with only spaces between them (a test checks the table), so none is joined wrongly.
                let mut k = j;
                while k < src.len() && src[k].is_whitespace() {
                    k += 1;
                }
                if k == j || !next_digit(&src, k) {
                    break;
                }
                j = k;
            }
            if j + 1 < src.len() && src[j] == '.' && is_digit(src[j + 1]) {
                j += 1;
                while j < src.len() && is_digit(src[j]) {
                    j += 1;
                }
            }
            out.push('#');
            i = j;
            continue;
        }
        let c = game_char(c);
        for l in c.to_lowercase() {
            if l.is_ascii_lowercase() || l == '#' || is_cjk(l) {
                out.push(l);
            }
        }
        i += 1;
    }
    out
}

/// Keeps the grades a user saw before 1.2.0 as theirs, once ungraded sentences went to the game's colour: each sentence
/// of `dict` (the built-in table with the learned affixes, without the user's grades) that the table framed by its own
/// grade then ([`Sentence::before`]; not a learned one nor a twin, framed by the game's colour already) and that the
/// user has not graded gets that grade saved under its key. Every sentence is then framed for that user as before.
/// Returns how many grades were added.
pub fn keep_grades_before(dict: &Dict, grades: &mut Grades) -> usize {
    let mut added = 0;
    for s in dict.sentences() {
        if s.learned || s.twin || s.user_grade(grades).is_some() {
            continue;
        }
        if let Some(g) = s.before {
            grades.insert(s.key().to_string(), g);
            added += 1;
        }
    }
    added
}

/// Whether game id `id` is of none of the families of `ids` (the id's first four digits: 5634 and 5422 are two kinds
/// of affix), with at least one game id among them (`ocr-*` ids are not the game's).
fn other_family(ids: &[String], id: &str) -> bool {
    let family = |s: &str| s.get(..4).map(str::to_string);
    let mut game = ids.iter().filter(|s| !s.starts_with("ocr-")).peekable();
    game.peek().is_some() && game.all(|s| family(s) != family(id))
}

/// Characters the OCR reads in place of the game's own, never in the game's affix texts (`key` puts the game's back):
/// 燜 for 爛 (腐爛, Rot: 3500304 read so on six visits of 2026-11-10, then too near 凍僵 of another grade to frame) and
/// the simplified 虚 for 虛 (虛弱, Weakened); on 1080p frames (2026-11-10 04:43) 撉 for 擊 (被擊敗時), 棸 for 聚 (聚能祝福)
/// and 褔 for 福 (祝福). A test checks the built-in table holds none of them.
const OCR_LOOKALIKES: &[(char, char)] = &[('燜', '爛'), ('虚', '虛'), ('撉', '擊'), ('棸', '聚'), ('褔', '福')];

/// Characters the OCR was seen to read in place of others on the saved reads the game log confirmed (2026-11-10: 口 for
/// 和 16 times, 目 for 盲 7 times): with [`OCR_LOOKALIKES`], the swaps [`Dict::exact_only`] does not trust. 魔, 血, 滿
/// and 低 were never swapped.
const OCR_SWAPS: &[(char, char)] = &[('口', '和'), ('目', '盲')];

/// Whether the OCR is known to read `a` for `b` or `b` for `a` ([`OCR_SWAPS`], [`OCR_LOOKALIKES`]).
fn swappable(a: char, b: char) -> bool {
    OCR_SWAPS.iter().chain(OCR_LOOKALIKES).any(|&(x, y)| (x == a && y == b) || (x == b && y == a))
}

/// The game's character for one the OCR reads in its place ([`OCR_LOOKALIKES`]), else the character itself.
pub(crate) fn game_char(c: char) -> char {
    OCR_LOOKALIKES.iter().find(|&&(read, _)| read == c).map_or(c, |&(_, game)| game)
}

/// True when a digit follows at `i`, allowing spaces in between.
fn next_digit(src: &[char], mut i: usize) -> bool {
    while i < src.len() && src[i].is_whitespace() {
        i += 1;
    }
    i < src.len() && is_digit(src[i])
}

/// Last row of the edit-distance table of `a` against `b`: entry j = distance from all of `a` to `b[..j]`.
fn distance_row(a: &str, b: &str) -> Vec<usize> {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev
}

/// Edit distance over characters.
pub fn levenshtein(a: &str, b: &str) -> usize {
    *distance_row(a, b).last().unwrap()
}

/// Edit distance between `a` and the closest prefix of `b` (how far `a` is from being the start of `b`).
fn prefix_distance(a: &str, b: &str) -> usize {
    if b.is_empty() {
        return usize::MAX;
    }
    *distance_row(a, b).iter().min().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_masks_numbers_and_signs() {
        assert_eq!(key("+15% additional Damage"), key("-30% additional damage"));
        assert_eq!(key("+15% additional Damage"), "#additionaldamage");
        assert_eq!(key("#% additional Damage"), "#additionaldamage");
        assert_eq!(key("Interval: 0.5s"), "interval#s");
    }

    #[test]
    fn key_undoes_ocr_habits() {
        // bullet read as "+ " is dropped, a real "+15%" is kept as a number
        assert_eq!(key("+ Rare monsters gain"), "raremonstersgain");
        assert_eq!(key("+ -15% additional Area Damage"), "#additionalareadamage");
        assert_eq!(key("玩家受到致盲效果時 , 攻擊和施法速度額外一46%"), "玩家受到致盲效果時攻擊和施法速度額外#");
        assert_eq!(key("一12%玩家的腐蝕抗性上限"), "#玩家的腐蝕抗性上限");
        // 一 not before a number stays a CJK character
        assert_eq!(key("每一次"), "每一次");
        // characters the OCR reads in place of the game's (2026-11-10)
        assert_eq!(key("魔 法 和 稀 有 怪 物 被 擊 敗 時 , 100 % 機 率 對 玩家 施 加 腐 燜 , 持 續 5 秒"), key("魔法和稀有怪物被擊敗時，#% 機率對玩家施加腐爛，持續 # 秒"));
        assert_eq!(key("獲得 8 秒虚弱"), key("獲得 # 秒虛弱"));
        // 1080p misreads (2026-11-10 04:43)
        assert_eq!(key("魔 法 和 稀 有 怪 物 被 撉 敗 時"), key("魔法和稀有怪物被擊敗時"));
        assert_eq!(key("棸能褔"), key("聚能福"));
    }

    /// The OCR reads 15 as "1 5" and 12 as "1 2" at times: one number, so these short affixes are found exactly (they
    /// were unknown, or matched only near, on every such read up to 2026-11-11). Numbers kept apart by anything but
    /// spaces stay two, and no text of the table has two numbers with only spaces between them.
    #[test]
    fn digits_split_by_spaces_are_one_number() {
        let d = Dict::builtin();
        let found = |text: &str, id: &str| match d.lookup(text, false) {
            Lookup::Found { index, exact: true, .. } => assert!(d.sentences()[index].ids.iter().any(|x| x == id), "{text:?}: {:?}", d.sentences()[index].ids),
            other => panic!("{text:?}: {other:?}"),
        };
        found("玩 家 額 外 一 1 5 % 傷 害", "5112001");
        found("玩 家 額 外 一 1 5 % 投 射 物 傷 害", "5112009");
        found("怪 物 對 生 命 造 成 額 外 + 1 5 % 傷 害", "5121009");
        found("怪 物 對 護 盾 造 成 額 外 + 1 5 % 傷 害", "5121010");
        found("玩 家 生 命 返 還 與 護 盾 返 還 一 1 2 %", "5311009");
        found("玩 家 受 詛 咒 效 果 時 , 每 2 秒 造 成 的 傷 害 額外 一 1 2 % , 最 高 疊 加 3 次", "5212008");
        found("稀 有 怪 物 每 被 擊 中 1 次 , 獲 得 3 % 避 免 傷 害 機率 ( 最 多 疊 加 1 5 次 )", "5422014");
        assert_eq!(key("+1 5% additional Damage"), "#additionaldamage");
        assert_eq!(key("1 2 . 5 秒"), "##秒", "a decimal point with spaces around it is not joined");
        assert_eq!(key("2s 3s"), "#s#s");
        assert_eq!(key("1 , 5"), "##");
        let two_numbers = |t: &str| {
            let c: Vec<char> = t.chars().collect();
            let num = |x: char| x == '#' || is_digit(x);
            (0..c.len()).any(|i| num(c[i]) && c[i + 1..].iter().position(|x| !x.is_whitespace()).is_some_and(|n| n > 0 && num(c[i + 1 + n])))
        };
        assert!(two_numbers("# #") && !two_numbers("#s #"));
        for s in d.sentences() {
            for t in [&s.en, &s.zh].into_iter().chain(&s.alt) {
                assert!(!two_numbers(t), "two numbers with only spaces between them in {t:?}");
            }
        }
    }

    /// A table sentence with English only takes the Chinese learned for its id as its own (not another wording): 5521026
    /// "稀有怪物每秒施加 # 加劇值" is found once learned, unknown before (2026-11-12: unknown on the visits before the
    /// game log had taught its Chinese, found on the next one).
    #[test]
    fn learned_chinese_fills_a_sentence_with_english_only() {
        use super::super::learned::Learned;
        let read = "稀 有 怪 物 每 秒 施 加 3 加 劇 值";
        let d = Dict::builtin();
        let i = d.sentences().iter().position(|s| s.ids == ["5521026"]).expect("5521026");
        assert!(d.sentences()[i].zh.is_empty() && !d.sentences()[i].en.is_empty(), "the table has its English only");
        assert_eq!(d.lookup(read, false), Lookup::Unknown);
        let en = d.sentences()[i].en.clone();
        let learned = d.with_learned(&[Learned { id: "5521026".into(), en: en.clone(), zh: "稀有怪物每秒施加 # 加劇值".into() }]);
        let s = &learned.sentences()[i];
        assert_eq!((s.zh.as_str(), s.alt.len(), s.en == en), ("稀有怪物每秒施加 # 加劇值", 0, true));
        assert!(matches!(learned.lookup(read, false), Lookup::Found { index, exact: true, .. } if index == i));
    }

    /// A sentence's own words in a language win over another sentence's other wording (`absorb`, review of 1.3.0): the
    /// result does not hang on which was learned first.
    #[test]
    fn the_order_learned_gives_one_table() {
        use super::super::learned::Learned;
        // B (5122006) gets Z1 as another wording; A, English only, gets Z1 as its own Chinese.
        let to_b = Learned { id: "5122006".into(), en: String::new(), zh: "怪物 #% 腐蝕抗性".into() };
        let a = Learned { id: "5999999".into(), en: "#% Monster Something Else".into(), zh: String::new() };
        let to_a = Learned { id: "5999999".into(), en: String::new(), zh: "怪物 #% 腐蝕抗性".into() };
        let words = |d: &Dict| d.sentences().iter().map(|s| (s.ids.clone(), s.en.clone(), s.zh.clone(), s.alt.clone())).collect::<Vec<_>>();
        let first = Dict::builtin().with_learned(&[to_b.clone(), a.clone(), to_a.clone()]);
        let second = Dict::builtin().with_learned(&[a, to_a, to_b]);
        assert_eq!(words(&first), words(&second), "the same table either way");
        let at = first.sentences().iter().position(|s| s.ids == ["5999999"]).expect("A");
        assert_eq!(first.sentences()[at].zh, "怪物 #% 腐蝕抗性");
        let b = first.sentences().iter().find(|s| s.ids.contains(&"5122006".to_string())).expect("B");
        assert!(b.alt.is_empty(), "{:?}", b.alt);
        for d in [&first, &second] {
            assert!(matches!(d.lookup("怪 物 + 47 % 腐 蝕 抗 性", false), Lookup::Found { index, .. } if index == at));
        }
    }

    /// 5422003, learned as "#% Monster Erosion Resistance" / "怪物 #% 腐蝕抗性", joins the table's 5122006 ("#% 怪物腐蝕抗性")
    /// by its English: its Chinese is kept as another wording, so the panel's "怪物 +47% 腐蝕抗性" is found (it was unknown
    /// on every read, 2026-11-11), both ids share one grade, and the English read stays sure once the user grades either.
    #[test]
    fn another_wording_of_a_learned_affix_is_kept() {
        use super::super::learned::Learned;
        let erosion = Learned { id: "5422003".into(), en: "#% Monster Erosion Resistance".into(), zh: "怪物 #% 腐蝕抗性".into() };
        let table = Dict::builtin();
        let d = table.clone().with_learned(std::slice::from_ref(&erosion));
        let i = d.sentences().iter().position(|s| s.ids.contains(&"5122006".to_string())).expect("5122006");
        let s = &d.sentences()[i];
        assert_eq!(s.ids, ["5122006", "5422003"], "one sentence, both ids");
        assert_eq!((s.zh.as_str(), s.alt.as_slice()), ("#% 怪物腐蝕抗性", &["怪物 #% 腐蝕抗性".to_string()][..]));
        assert_eq!(d.sentences().len(), table.sentences().len(), "no sentence added");
        let (panel, table_words, english) = ("怪 物 + 47 % 腐 蝕 抗 性", "+47% 怪物腐蝕抗性", "+47% Monster Erosion Resistance");
        assert_eq!(table.lookup(panel, false), Lookup::Unknown, "without the learned wording");
        for text in [panel, table_words, english] {
            assert!(matches!(d.lookup(text, false), Lookup::Found { index, exact: true, .. } if index == i), "{text}");
        }
        for id in ["5122006", "5422003"] {
            let graded = d.clone().with_grades(&Grades::from([(id.to_string(), Grade::Danger)]));
            for text in [panel, table_words, english] {
                assert!(
                    matches!(graded.lookup(text, false), Lookup::Found { index, grade: Grade::Danger, exact: true } if index == i),
                    "graded under {id}: {text} {:?}",
                    graded.lookup(text, false)
                );
            }
            assert_eq!(graded.exact_only(panel), Some(i), "graded under {id}");
            // a character of the other wording dropped: not found, but the user's grade frames it
            assert_eq!(graded.lookup("怪物+47%腐蝕抗", false), Lookup::Unknown);
            assert_eq!(graded.near_user_grade("怪物+47%腐蝕抗"), Some(Grade::Danger), "graded under {id}");
        }
        // learned twice: one wording
        let twice = table.clone().with_learned(&[erosion.clone(), erosion]);
        assert_eq!(twice.sentences()[i].alt.len(), 1);
        // words another sentence has are that sentence's: not kept as another wording
        let pierce = table.sentences().iter().position(|s| s.ids.contains(&"5521018".to_string())).expect("5521018");
        let other = table.clone().with_learned(&[Learned { id: "5422098".into(), en: "#% Monster Erosion Resistance".into(), zh: table.sentences()[pierce].zh.clone() }]);
        assert!(other.sentences()[i].alt.is_empty(), "{:?}", other.sentences()[i].alt);
        assert!(matches!(other.lookup("稀有怪物 +10% 腐蝕抗性穿透", false), Lookup::Found { index, .. } if index == pierce));
    }

    /// No two sentences of the built-in table have the same words in a language: an affix the game writes alike under
    /// two ids is one sentence with both ids, one grade (the user, 2026-11-10: "完全相同的詞綴等級可以改為相同"). Two
    /// rows alike made every read of them unsure once the user graded one (5634001 and 5521035, 稀有怪物距離玩家#米時強制
    /// 閃現…, 39 reads of five visits); the same id twice would grade it twice.
    #[test]
    fn the_table_has_no_two_sentences_alike() {
        let d = Dict::builtin();
        let mut seen: std::collections::HashMap<(bool, &str), &[String]> = std::collections::HashMap::new();
        let mut ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for s in d.sentences() {
            for (zh, k) in [(false, s.en_key.as_str()), (true, s.zh_key.as_str())] {
                if !k.is_empty() {
                    assert!(seen.insert((zh, k), &s.ids).is_none(), "{:?} and {:?} read alike: {k}", seen[&(zh, k)], s.ids);
                }
            }
            for id in &s.ids {
                assert!(ids.insert(id), "{id} in two rows");
            }
        }
        let i = d.sentences().iter().position(|s| s.ids.contains(&"5521035".to_string())).expect("5521035");
        assert_eq!(d.sentences()[i].ids, ["5634001", "5521035"]);
        let graded = d.clone().with_grades(&Grades::from([("5634001".to_string(), Grade::None)]));
        assert!(matches!(graded.lookup("稀有怪物距離玩家8米時強制閃現到玩家附近;魔法和稀有怪物+50%機率在擊中時造成4秒致盲和癱瘓", false), Lookup::Found { grade: Grade::None, .. }), "one grade for both");
    }

    /// A text equal to one sentence and one character from sentences of another grade is that sentence when the
    /// characters are not a known OCR swap: 低魔 / 低血 / 滿魔. Not when no sentence equals it, when the other is a known
    /// swap away (和 / 口), or a character added or dropped away.
    #[test]
    fn an_exact_text_one_safe_character_from_others_is_that_sentence() {
        use super::super::learned::Learned;
        // framed alike (none graded) nothing is unsure; the user's warning on 低血 makes 低魔 unsure
        let d = Dict::builtin().with_learned(&[Learned { id: "5521013".into(), en: String::new(), zh: "玩家低魔時額外受到 #% 傷害".into() }]);
        assert!(matches!(d.lookup("玩 家 低 魔 時 額 外 受 到 30 % 傷 害", false), Lookup::Found { .. }));
        let d = d.with_grades(&Grades::from([("5521015".to_string(), Grade::Warn)]));
        let low_mana = d.lookup("玩 家 低 魔 時 額 外 受 到 30 % 傷 害", false);
        assert_eq!(low_mana, Lookup::Ambiguous, "the lookup alone: unsure");
        let i = d.exact_only("玩 家 低 魔 時 額 外 受 到 30 % 傷 害").expect("低魔 is 5521013");
        assert!(d.sentences()[i].ids.contains(&"5521013".to_string()));
        let j = d.exact_only("玩家低血時額外受到30%傷害").expect("低血 is 5521015");
        assert!(d.sentences()[j].ids.contains(&"5521015".to_string()));
        assert_eq!(d.exact_only("玩家低怒時額外受到30%傷害"), None, "no sentence equals it");
        // a sentence of another grade a known swap away (和 / 口) or a character away by length
        let apart = Grades::from([("1".to_string(), Grade::Danger), ("2".to_string(), Grade::None), ("3".to_string(), Grade::Warn)]);
        let swap = Dict::parse("danger\t1\t\t施加致盲和癱瘓 #%\nnone\t2\t\t施加致盲口癱瘓 #%\n").unwrap().with_grades(&apart);
        assert_eq!(swap.exact_only("施加致盲口癱瘓 5%"), None, "和 read as 口 is known");
        let dropped = Dict::parse("danger\t1\t\t玩家受到額外傷害 #%\nnone\t2\t\t玩家受到傷害 #%\nwarn\t3\t\t玩家受到額外的傷害 #%\n").unwrap().with_grades(&apart);
        assert_eq!(dropped.exact_only("玩家受到額外傷害 5%"), None, "a character added: the OCR drops characters");
    }

    /// How each id was framed before 1.2.0 (the table's own grade, the game's colour for a learned affix or a twin, the
    /// user's grade over both) and after it with the grades kept (the user's grade, else the game's colour), for every
    /// id of either table. None: the game's colour.
    fn frames_before_and_after(old: &Dict, new: &Dict, grades: &Grades, kept: &Grades) -> Vec<(String, Option<Grade>, Option<Grade>)> {
        let before = |id: &str| {
            old.sentences().iter().find(|s| s.ids.iter().any(|i| i == id)).and_then(|s| {
                s.user_grade(grades).or(if s.learned || s.twin { None } else { s.before })
            })
        };
        let after = |id: &str| new.sentences().iter().find(|s| s.ids.iter().any(|i| i == id)).and_then(|s| s.user_grade(kept));
        let ids: std::collections::BTreeSet<&String> = old.sentences().iter().chain(new.sentences()).flat_map(|s| &s.ids).collect();
        ids.into_iter().map(|id| (id.clone(), before(id), after(id))).collect()
    }

    /// Keeping the grades from before 1.2.0: every id is framed as before (the table's grades become the user's where
    /// the user had none; learned affixes and twins stay with the game's colour); a second run adds nothing.
    #[test]
    fn the_grades_from_before_are_kept() {
        use super::super::learned::Learned;
        let learned = [
            Learned { id: "9999001".into(), en: String::new(), zh: "一條還沒見過的詞綴，額外 #% 測試".into() },
            Learned { id: "5422009".into(), en: String::new(), zh: "稀有怪物受到 # 米外的傷害額外 #%".into() },
        ];
        let table = Dict::builtin().with_learned(&learned);
        let grades = Grades::from([("5634001".to_string(), Grade::None), ("9999001".to_string(), Grade::Warn)]);
        let mut kept = grades.clone();
        let added = keep_grades_before(&table, &mut kept);
        let fixed = table.sentences().iter().filter(|s| !s.learned && !s.twin && s.before.is_some() && s.user_grade(&grades).is_none()).count();
        assert_eq!(added, fixed);
        for (id, was, now) in frames_before_and_after(&table, &table, &grades, &kept) {
            assert_eq!(was, now, "{id}");
        }
        let twin = table.sentences().iter().find(|s| s.ids.iter().any(|i| i == "5422009")).unwrap();
        assert!(twin.twin && twin.user_grade(&kept).is_none(), "a twin stays with the game's colour");
        assert_eq!(keep_grades_before(&table, &mut kept), 0, "once");
    }

    /// [`the_grades_from_before_are_kept`] on the user's own files (ignored): the table before 1.2.0
    /// (`ASCENDANT_OLD_TABLE`), the learned list (`ASCENDANT_AFFIX_LEARNED`) and the settings (`ASCENDANT_SETTINGS`).
    /// Every id of either table is framed as before; prints how many grades were added.
    #[test]
    #[ignore]
    fn the_users_grades_are_kept() {
        let var = |v: &str| std::env::var_os(v).unwrap_or_else(|| panic!("set {v}"));
        let old = Dict::parse(&std::fs::read_to_string(var("ASCENDANT_OLD_TABLE")).unwrap()).unwrap();
        let learned = super::super::learned::load(std::path::Path::new(&var("ASCENDANT_AFFIX_LEARNED"))).expect("learned list");
        let settings: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(var("ASCENDANT_SETTINGS")).unwrap()).unwrap();
        let grades: Grades = serde_json::from_value(settings["affix_grades"].clone()).unwrap_or_default();
        let (old, new) = (old.with_learned(&learned), Dict::builtin().with_learned(&learned));
        let mut kept = grades.clone();
        let added = keep_grades_before(&new, &mut kept);
        let rows = frames_before_and_after(&old, &new, &grades, &kept);
        let changed: Vec<_> = rows.iter().filter(|(_, was, now)| was != now).collect();
        println!("{} ids, {} grades of the user's, {added} added, {} framed otherwise {changed:?}", rows.len(), grades.len(), changed.len());
        assert!(changed.is_empty());
    }

    /// The characters `key` takes for misreads are in none of the built-in table's texts, English or Chinese (a game
    /// text holding one would be changed too). The learned list is the user's: checked by
    /// `the_ocr_lookalikes_are_in_no_learned_text` (ignored, `ASCENDANT_AFFIX_LEARNED`).
    #[test]
    fn the_ocr_lookalikes_are_in_no_game_text() {
        for s in Dict::builtin().sentences() {
            for (read, _) in OCR_LOOKALIKES {
                assert!(!s.en.contains(*read) && !s.zh.contains(*read), "{read} in {:?} / {:?}", s.en, s.zh);
            }
        }
    }

    /// [`the_ocr_lookalikes_are_in_no_game_text`] for the learned list of `ASCENDANT_AFFIX_LEARNED` (ignored).
    #[test]
    #[ignore]
    fn the_ocr_lookalikes_are_in_no_learned_text() {
        let path = std::env::var_os("ASCENDANT_AFFIX_LEARNED").expect("set ASCENDANT_AFFIX_LEARNED");
        let text = std::fs::read_to_string(path).expect("the learned list");
        let mut lines = 0;
        for line in text.lines().filter(|l| !l.starts_with('#')) {
            lines += 1;
            for (read, _) in OCR_LOOKALIKES {
                assert!(!line.contains(*read), "{read} in {line:?}");
            }
        }
        assert!(lines > 0, "an empty learned list");
        println!("{lines} learned lines, no lookalike");
    }

    /// A learned affix of another id family that joins a table sentence by its words alone makes it a twin, framed by
    /// the game's colour until graded; one of the same family, or joined by its id, does not.
    #[test]
    fn a_learned_twin_of_another_family_follows_the_games_colour() {
        use super::super::learned::Learned;
        let d = Dict::builtin();
        let (i, zh) = d.sentences().iter().enumerate().find(|(_, s)| s.ids == ["5634005"]).map(|(i, s)| (i, s.zh.clone())).expect("5634005");
        let twin = d.clone().with_learned(&[Learned { id: "5422009".into(), en: String::new(), zh: zh.clone() }]);
        let s = &twin.sentences()[i];
        assert!(s.ids.contains(&"5422009".to_string()) && s.twin && s.by_game_colour(), "{s:?}");
        let graded = twin.clone().with_grades(&Grades::from([("5634005".to_string(), Grade::Danger)]));
        assert!(!graded.sentences()[i].by_game_colour(), "graded by the user: its grade");
        let same = d.clone().with_learned(&[Learned { id: "5634099".into(), en: String::new(), zh }]);
        assert!(!same.sentences()[i].twin, "the same family: one affix in several strengths");
        let by_id = d.with_learned(&[Learned { id: "5634005".into(), en: String::new(), zh: "某個中文句子 #%".into() }]);
        assert!(!by_id.sentences()[i].twin, "joined by its id");
        assert!(other_family(&["5634005".into()], "5422009") && !other_family(&["ocr-zh-1".into()], "5422009"));
    }

    /// Learned affixes: a known id fills its other language; a known text adds its id to that sentence; anything else
    /// is a new sentence after the table's, not framed and marked learned.
    #[test]
    fn learned_affixes_join_the_table() {
        use super::super::learned::Learned;
        let d = Dict::builtin();
        let n = d.sentences().len();
        let en_only = d.sentences().iter().position(|s| s.zh.is_empty() && !s.en.is_empty()).unwrap();
        let id = d.sentences()[en_only].key().to_string();
        let haste = d.sentences().iter().position(|s| s.en.starts_with("Upon receiving damage, Rare monsters have")).unwrap();
        let learned = vec![
            Learned { id: id.clone(), en: String::new(), zh: "某個中文句子 #%".into() },
            Learned { id: "5999999".into(), en: d.sentences()[haste].en.clone(), zh: String::new() },
            Learned { id: "5888888".into(), en: "#% a new affix".into(), zh: String::new() },
            Learned { id: "5777777".into(), en: String::new(), zh: String::new() },
        ];
        let m = d.clone().with_learned(&learned);
        assert_eq!(m.sentences()[en_only].zh, "某個中文句子 #%");
        assert!(m.sentences()[haste].ids.contains(&"5999999".to_string()));
        assert_eq!(m.sentences().len(), n + 1);
        let new = &m.sentences()[n];
        assert_eq!((new.key(), new.grade, new.learned, new.en.as_str()), ("5888888", Grade::None, true, "#% a new affix"));
        // a learned sentence is found like any other
        assert!(matches!(m.lookup("+30% a new affix", false), Lookup::Found { index, .. } if index == n));
    }

    /// A text not found but near a sentence the user graded (within two edits beyond the fuzzy limit) has the user's
    /// grade, also "not framed"; near an ungraded sentence, or far from all, none (the game's colour then frames it).
    #[test]
    fn a_misread_of_a_graded_affix_keeps_the_users_grade() {
        let rows = "danger\t3500302\tWhen Magic and Rare monsters are defeated, there is a #% chance to inflict Frosted on you. Lasts for #s\t魔法和稀有怪物被擊敗時，#% 機率對玩家施加凍僵，持續 # 秒\n\
                    warn\t5521014\tThe player #% additional damage taken when at Full Mana\t玩家滿魔時額外受到 #% 傷害\n";
        let misread = "魔法和稀有怪物被擊敗時，30% 機率對玩家施加冫崬，持續 5 秒";
        let d = Dict::parse(rows).unwrap();
        assert_eq!(d.near_user_grade(misread), None, "not graded by the user");
        let mine = Dict::parse(rows).unwrap().with_grades(&Grades::from([("3500302".to_string(), Grade::None)]));
        assert_eq!(mine.near_user_grade(misread), Some(Grade::None), "the user set it to not framed");
        let kept = Dict::parse(rows).unwrap().with_grades(&Grades::from([("3500302".to_string(), Grade::Danger)]));
        assert_eq!(kept.near_user_grade(misread), None, "the table's grade from before 1.2.0, kept: not the user's pick");
        assert_eq!(mine.near_user_grade("Something never seen in the game at all"), None, "far from all");
        // two sentences as near: the user's grade only when both are graded alike
        let twins = "danger\ta\t\t玩家額外受到甲類傷害增加百分之十\nwarn\tb\t\t玩家額外受到乙類傷害增加百分之十\n";
        let misread = "玩家額外受到丙類傷害增加百分之十";
        let one = Dict::parse(twins).unwrap().with_grades(&Grades::from([("a".to_string(), Grade::None)]));
        assert_eq!(one.near_user_grade(misread), None, "one of the two not graded by the user");
        let both = Dict::parse(twins).unwrap().with_grades(&Grades::from([("a".to_string(), Grade::None), ("b".to_string(), Grade::None)]));
        assert_eq!(both.near_user_grade(misread), Some(Grade::None), "both graded alike");
        let apart = Dict::parse(twins).unwrap().with_grades(&Grades::from([("a".to_string(), Grade::None), ("b".to_string(), Grade::Danger)]));
        assert_eq!(apart.near_user_grade(misread), None, "graded apart");
    }

    /// Sentences first read off English screenshots (`ocr-en-*`) whose game ids the logs named since (2026-11-09: the
    /// English client's AffixInfos, the text the same word for word) are in the table by those ids, with their grades;
    /// the Chinese text learned from the log joins them (the table's sentence, not a new one framed by the game's colour).
    #[test]
    fn screen_only_sentences_take_their_game_ids() {
        let d = Dict::builtin();
        let by_id = |id: &str| d.sentences().iter().find(|s| s.ids.iter().any(|x| x == id));
        let blinded = by_id("5212012").expect("5212012 in the table");
        assert_eq!(blinded.grade, Grade::Warn);
        assert!(blinded.en.starts_with("You will be Blinded for # s when you have lost #% of Life"));
        let elemental = by_id("5121002").expect("5121002 in the table");
        assert_eq!((elemental.grade, elemental.en.as_str()), (Grade::None, "#% additional Elemental Damage for monsters"));
        assert!(by_id("ocr-en-8").is_none() && by_id("ocr-en-3").is_none(), "the screen ids gone");
        let learned = [super::super::learned::Learned {
            id: "5212012".into(),
            en: blinded.en.clone(),
            zh: "玩家在最近消耗 #% 生命，或受到高於 #% 生命和護盾總值的傷害時，獲得 # 秒致盲。".into(),
        }];
        let with = Dict::builtin().with_learned(&learned);
        let s = with.sentences().iter().find(|s| s.ids.iter().any(|x| x == "5212012")).expect("still there");
        assert!(!s.zh.is_empty() && s.by_game_colour() && s.before == Some(Grade::Warn), "the table's sentence with the learned Chinese");
        assert_eq!(with.sentences().len(), d.sentences().len(), "not a new sentence");
    }

    /// A learned affix not graded by the user is framed by the game's colour (`affix::read`); once the user grades it,
    /// even as not framed, by that grade; the table's own sentences always by theirs.
    #[test]
    fn a_learned_affix_follows_the_games_colour_until_graded() {
        let learned = [super::super::learned::Learned { id: "9999001".into(), en: String::new(), zh: "一條還沒見過的詞綴，額外 #% 測試".into() }];
        let d = Dict::builtin().with_learned(&learned);
        let s = d.sentences().iter().find(|s| s.ids.contains(&"9999001".to_string())).expect("learned");
        assert!(s.learned && s.by_game_colour(), "learned, not graded: the game's colour");
        let graded = Dict::builtin().with_learned(&learned).with_grades(&Grades::from([("9999001".to_string(), Grade::None)]));
        let s = graded.sentences().iter().find(|s| s.ids.contains(&"9999001".to_string())).expect("learned");
        assert!(!s.by_game_colour(), "graded by the user, even as not framed: the user's grade");
        assert!(Dict::builtin().sentences().iter().all(Sentence::by_game_colour), "the table's own too, since 1.2.0");
    }

    /// The user's grades replace the defaults by the sentence's first id; unknown names change nothing.
    #[test]
    fn user_grades_replace_the_defaults() {
        let d = Dict::builtin();
        let s = &d.sentences()[0];
        let (name, was) = (s.key().to_string(), s.grade);
        let other = if was == Grade::None { Grade::Danger } else { Grade::None };
        let grades = Grades::from([(name.clone(), other), ("no-such-id".to_string(), Grade::Warn)]);
        let mine = d.clone().with_grades(&grades);
        assert_eq!(mine.sentences()[0].grade, other);
        assert_eq!(mine.sentences().len(), d.sentences().len());
        assert_eq!(mine.sentences()[1..].iter().map(|s| s.grade).collect::<Vec<_>>(), d.sentences()[1..].iter().map(|s| s.grade).collect::<Vec<_>>());
    }

    /// A learned text a letter or two from a screen-only sentence's joins it and replaces the read words; near a
    /// sentence with game ids it stays apart (another affix).
    #[test]
    fn learned_text_near_a_screen_sentence_joins_it() {
        use super::super::learned::Learned;
        let d = Dict::builtin();
        let n = d.sentences().len();
        let regain = d.sentences().iter().position(|s| s.ids == ["ocr-en-4"]).unwrap();
        let near = "#% Life Regen and Energy Shield Regain for you";
        let m = d.clone().with_learned(&[Learned { id: "5300001".into(), en: near.into(), zh: String::new() }]);
        assert_eq!(m.sentences().len(), n);
        let s = &m.sentences()[regain];
        assert_eq!((s.ids.clone(), s.en.as_str(), s.grade, s.learned), (vec!["ocr-en-4".to_string(), "5300001".to_string()], near, Grade::Warn, false));
        let frosted = d.sentences().iter().position(|s| s.ids.contains(&"3500302".to_string())).unwrap();
        let other = d.sentences()[frosted].en.replace("Frosted", "Frosten");
        let m = d.clone().with_learned(&[Learned { id: "5300002".into(), en: other.clone(), zh: String::new() }]);
        assert_eq!(m.sentences().len(), n + 1);
        assert_eq!(m.sentences()[frosted].ids, ["3500302"]);
        assert_eq!((m.sentences()[n].en.as_str(), m.sentences()[n].learned), (other.as_str(), true));
    }

    /// The other language filled in is a screen-only sentence's (the same words, or a character off): the two are one
    /// affix, folded into one; a grade saved under either id holds; a learned sentence folded with a table one takes
    /// the table's grade.
    #[test]
    fn one_affix_in_two_sentences_is_folded() {
        use super::super::learned::Learned;
        let d = Dict::builtin();
        let n = d.sentences().len();
        let at = |m: &Dict, id: &str| m.sentences().iter().position(|s| s.ids.iter().any(|i| i == id));
        // the screen's sentence sits with its game id in the table since 1.2.0; a grade under either id reads
        assert_eq!(d.sentences()[at(&d, "3500300").unwrap()].ids, ["3500300", "ocr-zh-2"]);
        let zh = "魔法和稀有怪物受到傷害時，<p>12</p>%機率對玩家施加燒灼，持續<p>3</p>秒，間隔<p>2</p>秒";
        let t = super::super::learned::template(zh).unwrap();
        let m = d.clone().with_learned(&[Learned { id: "3500300".into(), en: String::new(), zh: t.clone() }]);
        assert_eq!(m.sentences().len(), n);
        let mine = m.with_grades(&Grades::from([("ocr-zh-2".to_string(), Grade::Warn)]));
        assert_eq!(mine.sentences()[at(&mine, "3500300").unwrap()].grade, Grade::Warn);
        // a learned sentence (5300003) meets a screen sentence through a later id's Chinese text: one sentence
        let screen = Dict::parse(&format!("danger\tocr-zh-9\t\t{t}\n")).unwrap();
        let learned = [
            Learned { id: "5300003".into(), en: "#% a brand new affix".into(), zh: String::new() },
            Learned { id: "5300004".into(), en: "#% a brand new affix".into(), zh: t },
        ];
        let m = screen.with_learned(&learned);
        assert_eq!(m.sentences().len(), 1);
        let s = &m.sentences()[at(&m, "5300003").unwrap()];
        assert_eq!((s.ids.len(), s.before, s.learned), (3, Some(Grade::Danger), false));
        assert!(at(&m, "ocr-zh-9") == at(&m, "5300004"));
    }

    /// Review cases (2026-11-08): two sentences with different English are not folded for the same Chinese (both kept,
    /// the English of each intact); a text whose key is empty (marks only) joins nothing; a screen-only sentence whose
    /// text equals the log's (by key) shows the log's words.
    #[test]
    fn folding_keeps_affixes_apart_when_the_other_language_differs() {
        use super::super::learned::Learned;
        let d = Dict::builtin();
        let n = d.sentences().len();
        let both = d.sentences().iter().position(|s| !s.en.is_empty() && !s.zh.is_empty() && !s.screen_only()).unwrap();
        let en_only = d.sentences().iter().position(|s| !s.en.is_empty() && s.zh.is_empty() && !s.screen_only()).unwrap();
        let (zh, en_b) = (d.sentences()[both].zh.clone(), d.sentences()[both].en.clone());
        let m = d.clone().with_learned(&[Learned { id: d.sentences()[en_only].key().into(), en: String::new(), zh: zh.clone() }]);
        assert_eq!(m.sentences().len(), n);
        assert_eq!((m.sentences()[en_only].zh.as_str(), m.sentences()[both].en.as_str()), (zh.as_str(), en_b.as_str()));
        // marks only: nothing joins, nothing is added (a sentence with no Chinese yet is not this text's twin)
        let en = d.sentences()[en_only].en.clone();
        let m = d.clone().with_learned(&[Learned { id: d.sentences()[en_only].key().into(), en, zh: "- …".into() }]);
        assert_eq!((m.sentences().len(), m.sentences()[en_only].zh.as_str()), (n, ""));
        let m = d.clone().with_learned(&[Learned { id: "5300020".into(), en: "- …".into(), zh: String::new() }]);
        assert_eq!(m.sentences().len(), n);
        // the screen's words gave way to the log's in the table (1.2.0): the game id first, the screen id kept
        let tenacity = d.sentences().iter().position(|s| s.ids == ["5311017", "ocr-zh-1"]).unwrap();
        let log = "玩家 #% 堅韌祝福持續時間";
        assert_eq!(key(log), d.sentences()[tenacity].zh_key);
        let m = d.clone().with_learned(&[Learned { id: "5311017".into(), en: "#% Tenacity Blessing Duration for you".into(), zh: log.into() }]);
        let s = &m.sentences()[tenacity];
        assert_eq!((s.ids.clone(), s.zh.as_str(), s.en.as_str()), (vec!["5311017".to_string(), "ocr-zh-1".to_string()], log, "#% Tenacity Blessing Duration for you"));
    }

    /// A grade saved under any of a sentence's ids is the user's grade (the first id that has one wins).
    #[test]
    fn user_grade_by_any_id() {
        use super::super::learned::Learned;
        let d = Dict::builtin().with_learned(&[Learned { id: "5300005".into(), en: Dict::builtin().sentences()[0].en.clone(), zh: String::new() }]);
        let s = &d.sentences()[0];
        assert_eq!(s.ids.last().map(String::as_str), Some("5300005"));
        assert_eq!(s.user_grade(&Grades::from([("5300005".to_string(), Grade::Warn)])), Some(Grade::Warn));
        assert_eq!(s.user_grade(&Grades::from([("5300005".to_string(), Grade::Warn), (s.key().to_string(), Grade::None)])), Some(Grade::None));
        assert_eq!(s.user_grade(&Grades::new()), None);
    }

    #[test]
    fn builtin_table_loads() {
        let d = Dict::builtin();
        // 2026-11-10: two pairs of rows alike made one each (5634001 with 5521035, 5422013 with ocr-zh-3); 1.2.0: the
        // user's 45 learned affixes added (`game`), 3500300 with ocr-zh-2; the grades kept are the ones from before
        assert_eq!(d.sentences().len(), 142);
        let before = |g: Option<Grade>| d.sentences().iter().filter(|s| s.before == g).count();
        assert_eq!((before(Some(Grade::Danger)), before(Some(Grade::Warn)), before(Some(Grade::None)), before(None)), (14, 40, 43, 45));
        assert!(d.sentences().iter().all(|s| s.by_game_colour() && s.frame().is_none()), "framed by the game's colour");
    }

    #[test]
    fn lookup_exact_fuzzy_and_cut() {
        let d = Dict::builtin();
        let haste = "Upon receiving damage, Rare monsters have a 50% chance to gain +77% Attack, Cast, and Movement Speed. Interval: 0.2s.";
        assert!(matches!(d.lookup(haste, false), Lookup::Found { grade: Grade::Danger, exact: true, .. }));
        // one OCR slip still matches
        let slip = haste.replace("Movement", "Movernent");
        assert!(matches!(d.lookup(&slip, false), Lookup::Found { grade: Grade::Danger, exact: false, .. }));
        // the start of a sentence matches only when the entry may be cut, and shows most of it
        let start = "Upon receiving damage, Rare monsters have a 50% chance to gain +77% Attack, Cast, and";
        assert_eq!(d.lookup(start, false), Lookup::Unknown);
        assert!(matches!(d.lookup(start, true), Lookup::Found { grade: Grade::Danger, .. }));
        assert_eq!(d.lookup("Upon receiving damage, Rare monsters have a 50% chance", true), Lookup::Unknown);
        assert_eq!(d.lookup("Something never seen in the game", false), Lookup::Unknown);
    }

    /// A short cut entry that only starts like a sentence of the table gets no frame: an affix not in the table may
    /// start the same way (found by the independent check of 2026-11-08).
    #[test]
    fn cut_entries_need_most_of_one_sentence() {
        let d = Dict::builtin();
        assert_eq!(d.lookup("Rare monsters gain 30%", true), Lookup::Unknown);
        assert_eq!(d.lookup("Rare monsters have a 30% chance to", true), Lookup::Unknown);
    }

    /// A sentence of another grade close to the text makes it ambiguous even when that sentence is outside the
    /// fuzzy limits itself (CJK texts allow one edit; 致殘 / 燒灼 are two apart).
    #[test]
    fn near_rivals_of_another_grade_are_ambiguous() {
        let table = "danger\ta\t\t稀有怪物受到傷害時，#% 機率對玩家施加致殘，持續 # 秒，間隔 # 秒\n\
                     warn\tb\t\t稀有怪物受到傷害時，#% 機率對玩家施加燒灼，持續 # 秒，間隔 # 秒\n\
                     warn\tc\t#% Max Fire Resistance for you\t\n\
                     none\td\t#% Max Cold Resistance for you\t\n";
        // since 1.2.0 sentences not graded are all framed by the game's colour: nothing to tell apart
        let plain = Dict::parse(table).unwrap();
        assert!(matches!(plain.lookup("-20% Max Cire Resistance for you", false), Lookup::Found { .. }), "framed alike: found");
        // the user's grades make them differ
        let grades = Grades::from([("a".to_string(), Grade::Danger), ("b".to_string(), Grade::Warn), ("c".to_string(), Grade::Warn), ("d".to_string(), Grade::None)]);
        let d = plain.with_grades(&grades);
        // one character off 燒灼: the danger sentence is 2 edits away from the read text
        assert_eq!(d.lookup("稀有怪物受到傷害時，100% 機率對玩家施加燒約，持續 5 秒，間隔 0.5 秒", false), Lookup::Ambiguous);
        // an exact read stays found: the rival is two edits away
        assert!(matches!(d.lookup("稀有怪物受到傷害時，100% 機率對玩家施加燒灼，持續 5 秒，間隔 0.5 秒", false), Lookup::Found { grade: Grade::Warn, exact: true, .. }));
        assert_eq!(d.lookup("-20% Max Cire Resistance for you", false), Lookup::Ambiguous);
        assert_eq!(d.lookup("-20% Max Fold Resistance for you", false), Lookup::Ambiguous);
        assert!(matches!(d.lookup("-20% Max Fire Resistance for you", false), Lookup::Found { grade: Grade::Warn, exact: true, .. }));
    }

    /// A long Chinese text two edits off a sentence (a keyword read wrong: 凍僵 as 冫崬僵) matches it when it plainly
    /// wins; not when a sentence of another grade is near (致殘 / 燒灼 both dropped reads as either), not a short
    /// text, not three edits off, and not for a sentence with no English to check its pattern's siblings by.
    #[test]
    fn long_chinese_two_edits_off_still_match() {
        let table = "danger\ta\tWhen Magic and Rare monsters are defeated, there is a #% chance to inflict Frosted on you. Lasts for #s\t魔法和稀有怪物被擊敗時，#% 機率對玩家施加凍僵，持續 # 秒\n\
                     warn\tb\t\t玩家滿魔時額外受到 #% 傷害\n\
                     danger\tc\t\t稀有怪物受到傷害時，#% 機率對玩家施加致殘，持續 # 秒，間隔 # 秒\n\
                     warn\td\t\t稀有怪物受到傷害時，#% 機率對玩家施加燒灼，持續 # 秒，間隔 # 秒\n\
                     warn\te\t#% Max Fire Resistance for you\t\n";
        let d = Dict::parse(table).unwrap();
        // 2026-11-09, sample-0002 of 18-05-20 with the underline's edges painted: two edits off, the next 15 off
        assert!(matches!(d.lookup("魔法和稀有怪物被擊敗時，30% 機率對玩家施加冫崬僵，持續 5 秒", false), Lookup::Found { grade: Grade::Danger, exact: false, .. }));
        // both keyword characters gone: as near the danger sentence as the warning one
        assert_eq!(d.lookup("稀有怪物受到傷害時，100% 機率對玩家施加，持續 5 秒，間隔 0.5 秒", false), Lookup::Unknown);
        // short: one edit at most, as before
        assert_eq!(d.lookup("玩家滿魔時受到 +30% 傷", false), Lookup::Unknown);
        // three edits off: still unknown
        assert_eq!(d.lookup("魔法和稀有怪物被擊時，30% 機率對玩家施加冫崬僵，持續 5 秒", false), Lookup::Unknown);
        // the same sentence with no English: its siblings can not be checked
        let no_en = Dict::parse(&table.replace("When Magic and Rare monsters are defeated, there is a #% chance to inflict Frosted on you. Lasts for #s", "")).unwrap();
        assert_eq!(no_en.lookup("魔法和稀有怪物被擊敗時，30% 機率對玩家施加冫崬僵，持續 5 秒", false), Lookup::Unknown);
    }

    /// The 3500300 family shares one pattern with another keyword (凍僵, 腐爛, 麻痺...): one never matches another by
    /// the two-edit allowance; a keyword read wrong matches its own only when it plainly wins (冫崬僵 does, a bare 冫,
    /// two off both 凍僵 and 腐爛, does not); and while one's Chinese is not known yet (3500302 in the table of
    /// 2026-11-08, English only), a text near a sibling is not taken for that sibling.
    #[test]
    fn keywords_of_one_pattern_are_not_mixed_up() {
        let frost = "danger\t3500302\tWhen Magic and Rare monsters are defeated, there is a #% chance to inflict Frosted on you. Lasts for #s\t魔法和稀有怪物被擊敗時，#% 機率對玩家施加凍僵，持續 # 秒\n";
        let rows = format!(
            "danger\t3500301\tWhen Magic and Rare monsters are defeated, there is a #% chance to inflict Numbed on you. Lasts for #s. #% additional Numbed Effect for you\t魔法和稀有怪物被擊敗時，#% 機率對玩家施加麻痺，持續 # 秒；玩家額外 #% 受到的麻痺效果\n\
             {frost}\
             danger\t3500304\tWhen Magic and Rare monsters are defeated, there is a #% chance to inflict Rot on you. Lasts for #s\t魔法和稀有怪物被擊敗時，#% 機率對玩家施加腐爛，持續 # 秒\n"
        );
        let id_in = |d: &Dict, text: &str| match d.lookup(text, false) {
            Lookup::Found { index, .. } => Some(d.sentences()[index].ids[0].clone()),
            _ => None,
        };
        let d = Dict::parse(&rows).unwrap();
        let id = |text: &str| id_in(&d, text);
        assert_eq!(id("魔法和稀有怪物被擊敗時，30% 機率對玩家施加凍僵，持續 5 秒").as_deref(), Some("3500302"));
        assert_eq!(id("魔法和稀有怪物被擊敗時，30% 機率對玩家施加腐爛，持續 5 秒").as_deref(), Some("3500304"));
        // 2026-11-09 samples with the underline's edges painted: 凍僵 as 冫崬僵, 麻痺 as 麻瘣; and 麻痺 dropped
        assert_eq!(id("魔法和稀有怪物被擊敗時，30% 機率對玩家施加冫崬僵，持續 5 秒").as_deref(), Some("3500302"));
        assert_eq!(id("魔法和稀有怪物被擊敗時，100% 機率對玩家施加麻瘣，持續 5 秒；玩家額外 -50% 受到的麻痺效果").as_deref(), Some("3500301"));
        assert_eq!(id("魔法和稀有怪物被擊敗時，100% 機率對玩家施加，持續 5 秒；玩家額外 -50% 受到的麻痺效果").as_deref(), Some("3500301"));
        // the reading before: a bare 冫, two edits off 凍僵 and 腐爛 alike
        assert_eq!(id("魔法和稀有怪物被擊敗時，30% 機率對玩家施加冫，持續 5 秒"), None);
        // 3500302 with English only: its text is not 3500304's, nor is a misread one
        let no_frost = Dict::parse(&rows.replace(
            frost,
            "danger\t3500302\tWhen Magic and Rare monsters are defeated, there is a #% chance to inflict Frosted on you. Lasts for #s\t\n",
        ))
        .unwrap();
        assert_eq!(id_in(&no_frost, "魔法和稀有怪物被擊敗時，30% 機率對玩家施加凍僵，持續 5 秒"), None);
        assert_eq!(id_in(&no_frost, "魔法和稀有怪物被擊敗時，30% 機率對玩家施加冫，持續 5 秒"), None, "two off 腐爛, its sibling unknown");
        assert_eq!(id_in(&no_frost, "魔法和稀有怪物被擊敗時，30% 機率對玩家施加腐爛，持續 5 秒").as_deref(), Some("3500304"), "exact still");
    }
}
