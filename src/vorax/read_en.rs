//! Reading the English game client (feature `en-vorax`, on by default since 1.1.0).
//! [`super`] reads the Traditional Chinese client and hands over here, behind [`english`], where the English client
//! differs:
//! - its digits are drawn in another face ([`super::super::templates_en`], learned by [`learn`]);
//! - its titles, type texts, result screen and card names are English (the name table's `en`), long names wrap onto
//!   two lines and the type texts of neighbouring cards are often read as one line;
//! - names are cut at either end by the neighbouring cards, and a neighbour's letters run into them.
//!
//! Measured on 8 English sample games of 2026-11-07 (home/vorax 11-35-12 to 15-47-37, 825 frames, two sets of 30
//! hand-labelled frames): english-reader/report*.md beside the samples' analysis.

use super::*;
use std::cell::Cell;
use std::collections::BTreeSet;
use std::ops::RangeInclusive;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

static ENGLISH: AtomicBool = AtomicBool::new(false);
/// How many times [`set_english`] changed the client read (diagnostics: the run log's `vorax_read` rows carry it, so a
/// switch of the reader between two reads shows).
static SETS: AtomicU32 = AtomicU32::new(0);

thread_local! {
    static ENGLISH_HERE: Cell<Option<bool>> = const { Cell::new(None) };
}

/// The game client is the English one. Off (the Traditional Chinese client) unless set: every English branch of the
/// reader sits behind it, so the Traditional Chinese reading does not change. Set by the app for the client it reads
/// and by the command line's `--client` ([`set_english`], every thread); tests set it for their own thread only
/// ([`set_english_here`]), so tests of either client run side by side.
pub fn english() -> bool {
    ENGLISH_HERE.with(Cell::get).unwrap_or_else(|| ENGLISH.load(Ordering::Relaxed))
}

/// [`english`] for every thread that has not set its own.
pub fn set_english(on: bool) {
    if ENGLISH.swap(on, Ordering::Relaxed) != on {
        SETS.fetch_add(1, Ordering::Relaxed);
    }
}

/// The number of times [`set_english`] changed the client read so far (diagnostics).
pub fn english_sets() -> u32 {
    SETS.load(Ordering::Relaxed)
}

/// [`english`] for this thread only (None: as every thread, [`set_english`]).
pub fn set_english_here(on: Option<bool>) {
    ENGLISH_HERE.with(|c| c.set(on));
}

/// The English client's templates of the field whose Traditional Chinese templates are `zh`.
pub(super) fn templates(zh: &'static [Template]) -> &'static [Template] {
    use super::super::templates_en as en;
    let pairs: [(&'static [Template], &'static [Template]); 7] =
        [(templates::LABEL, en::LABEL), (templates::PANEL, en::PANEL), (templates::REWARD, en::REWARD), (templates::RESULT, en::RESULT), (templates::ROUND, en::ROUND), (templates::SLASH, en::SLASH), (templates::REROLL, en::REROLL)];
    pairs.iter().find(|(z, _)| std::ptr::eq(*z, zh)).map_or(zh, |(_, e)| e)
}

/// The final activity's digits are a pixel shorter than the Traditional Chinese client's (19 to 28 high in the
/// sample games, 20 to 28 there).
pub(super) const PANEL_HEIGHTS: RangeInclusive<i32> = 17..=28;

/// The type text (normalized): `NormalPotion`, `MagicPotion`, `RarePotion`, `ULTPotion` (Rousing Salts,
/// 2026-11-07_17-59-47), `NormalSurgical Protocol` (and magic, rare), `Surgical Instrument`. OCR often cuts the end
/// (`MagicSurgical Prot`, `Surgical Instrume`) or reads `l` as `I`, so stems are looked at; "Surgical" is in both the
/// protocols and the instruments, so the protocol is looked for first.
pub(super) fn type_of(t: &str) -> (Option<Category>, Option<PotionRarity>) {
    let category = if t.contains("prot") {
        Some(Category::Protocol)
    } else if t.contains("instrum") || t.contains("strume") {
        Some(Category::Instrument)
    } else if t.contains("potio") {
        Some(Category::Potion)
    } else {
        None
    };
    let rarity = if t.contains("normal") {
        Some(PotionRarity::Normal)
    } else if t.contains("magic") {
        Some(PotionRarity::Magic)
    } else if t.contains("rare") {
        Some(PotionRarity::Rare)
    } else if t.contains("ult") || t.contains("supreme") {
        Some(PotionRarity::Supreme)
    } else {
        None
    };
    (category, if category == Some(Category::Potion) { rarity } else { None })
}

/// The title (normalized): `Select a Potion`, `Select a Surgical Instrument`, `Select a Surgical Protocol` (OCR
/// garbles the middle: `SelectåSüiÉiCåVProtocol`).
pub(super) fn title_of(t: &str) -> Option<Category> {
    t.contains("select").then(|| type_of(t).0).flatten()
}

/// The cards of `category` with an English name, normalized and without digits (the name read loses its digits,
/// [`super::match_name`], so `F1 Gen: Parasite Egg` is compared as `fgenparasiteegg`), built once per category: the
/// name table does not change after its first use. A name that is not all ASCII once normalized (a Traditional
/// Chinese key standing in for an unknown English name) is left out: the English client does not print it, and
/// [`longer_shown`] cuts names by bytes.
pub(super) fn candidates(category: Category) -> &'static [(usize, String)] {
    &cached(category).names
}

/// The candidates of a category ([`candidates`]) and the letters a neighbouring card's name line of the category
/// may show next to a name ([`core_apart`]).
struct Cached {
    names: Vec<(usize, String)>,
    /// The first [`EDGE`] letters of a line starting at a word of a name (normalized as the names): what a right
    /// neighbour's line shows after a name.
    starts: Vec<String>,
    /// The last [`EDGE`] letters of a line ending after a word of a name: what a left neighbour's line shows before
    /// a name.
    ends: Vec<String>,
}

fn cached(category: Category) -> &'static Cached {
    static CACHED: [OnceLock<Cached>; 3] = [const { OnceLock::new() }; 3];
    let k = match category {
        Category::Potion => 0,
        Category::Instrument => 1,
        Category::Protocol => 2,
    };
    CACHED[k].get_or_init(|| {
        let cards: Vec<(usize, &CardName)> = names().cards.iter().enumerate().filter(|(_, c)| c.category == category && no_digits(&norm(c.printed_en())).is_ascii()).collect();
        let names: Vec<(usize, String)> = cards.iter().map(|&(i, c)| (i, no_digits(&norm(c.printed_en())))).filter(|(_, z)| !z.is_empty()).collect();
        let (mut starts, mut ends): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
        let alnum = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric());
        for (_, c) in &cards {
            let en = c.printed_en();
            // Word bounds: where a run of ASCII letters or digits starts or ends.
            for (at, ch) in en.char_indices().filter(|(_, ch)| ch.is_ascii_alphanumeric()) {
                let next = at + ch.len_utf8();
                if !alnum(en[..at].chars().next_back()) {
                    let rest = no_digits(&norm(&en[at..]));
                    if let Some(p) = rest.get(..EDGE).map(str::to_string).filter(|p| !starts.contains(p)) {
                        starts.push(p);
                    }
                }
                if !alnum(en[next..].chars().next()) {
                    let head = no_digits(&norm(&en[..next]));
                    if let Some(p) = head.get(head.len().saturating_sub(EDGE)..).filter(|p| p.len() == EDGE).map(str::to_string).filter(|p| !ends.contains(p)) {
                        ends.push(p);
                    }
                }
            }
        }
        Cached { names, starts, ends }
    })
}

/// `t` without its ASCII digits.
fn no_digits(t: &str) -> String {
    t.chars().filter(|c| !c.is_ascii_digit()).collect()
}

/// How many letters an English name may be off: up to 4 exact, then one in five or six.
pub(super) fn allowed(len: usize) -> usize {
    match len {
        0..=4 => 0,
        5..=9 => 1,
        10..=15 => 2,
        _ => 3,
    }
}

/// The match of an English name read (normalized), `clipped` when the name line is off the card's centre (covered
/// at one side): the name read nearly whole and within [`allowed`] of one card, else a part of it
/// ([`partial`]). A name that is all inside another card's name of the category (`Xenopurge Powder` in `Potent
/// Xenopurge Powder`, `Xeno-Hormone` in `Targeted Xeno-Hormone`, `Potion Kit (Small)` in `Rare Potion Kit (Small)`)
/// is taken only read exactly, with nothing before or after it, from a line not covered; one that starts another
/// (`Xenopanacea` of `Xenopanacea: Brood`, the second line not read) never. A name read near, not exactly, must not
/// rest on a neighbour's letters at its ends ([`core_apart`]): `fgenparasite` + `eg` of the neighbour's `Egg` is a
/// letter off `F1 Gen: Parasite Egg`, though `F2 Gen: Parasite Pupae` was read.
pub(super) fn match_text(t: &str, category: Category, clipped: bool) -> Option<(usize, How)> {
    let cands = candidates(category);
    let mut scored: Vec<(usize, usize, &str)> = cands.iter().map(|(i, z)| (*i, distance(t, z), z.as_str())).collect();
    scored.sort_by_key(|&(_, d, _)| d);
    if let Some(&(i, d, z)) = scored.first() {
        let unique = scored.iter().skip(1).all(|s| s.1 > d + 1);
        // Candidates are ASCII; the text read may not be (`Surøiral`): count characters.
        let letters: Vec<char> = t.chars().collect();
        let whole = letters.len() + 1 >= z.len();
        let seam_ok = t == z || core_apart(&letters, 0, letters.len(), i, cands);
        if unique && whole && d <= allowed(z.len()) && named_ok(z, cands) && seam_ok && (!inside_another(z, cands) || (d == 0 && t == z && !clipped)) {
            return Some((i, How::Name));
        }
    }
    partial(t, cands, clipped)
}

/// The run `letters[start..start + len]` of the text read is card `i`'s alone (part of no other name of `cands`)
/// without the [`EDGE`] letters at an end of the text that a neighbouring card's name line may show there: before
/// the text, the last letters of a line ending after a word of a name of the category (`re` of `Tincture`), after it
/// the first letters of one starting at a word (`pe` of `Pest`). Nothing left counts as apart.
fn core_apart(letters: &[char], start: usize, len: usize, i: usize, cands: &[(usize, String)]) -> bool {
    let edges = cached(names().cards[i].category);
    let pair = |at: usize| letters.get(at..at + EDGE).map(|p| p.iter().collect::<String>());
    let cut_front = start == 0 && pair(0).is_some_and(|p| edges.ends.contains(&p));
    let cut_back = start + len == letters.len() && pair(letters.len().saturating_sub(EDGE)).is_some_and(|p| edges.starts.contains(&p));
    let from = start + if cut_front { EDGE } else { 0 };
    let to = (start + len).saturating_sub(if cut_back { EDGE } else { 0 });
    if from >= to {
        return true;
    }
    let core: String = letters[from..to].iter().collect();
    !cands.iter().any(|(j, o)| *j != i && o.contains(&core))
}

/// How many letters at either end of the text read may be a neighbour's (its last letters before a name covered at
/// the left, its first after one covered at the right): a card is not taken on them alone ([`core_apart`]). One
/// letter is left to [`MARGIN`].
const EDGE: usize = 2;

/// `z` is part of another candidate's name.
fn inside_another(z: &str, cands: &[(usize, String)]) -> bool {
    cands.iter().any(|(_, o)| o.len() > z.len() && o.contains(z))
}

/// `z` does not start another candidate's name (`Xenopanacea` of `Xenopanacea: Brood`): such a name read may be the
/// first line of the longer one.
fn named_ok(z: &str, cands: &[(usize, String)]) -> bool {
    !cands.iter().any(|(_, o)| o.len() > z.len() && o.starts_with(z))
}

/// The partial match: the longest run of at least [`RUN`] letters of the text read that is part of exactly one card
/// name of the category. English names are cut at either end (covered by the neighbouring card on the left,
/// `al Solution - Agito`, or on the right, `Anesthetic Tincture - Aember`), so a prefix alone does not do. A
/// neighbour's last or first letters run into a name (`e` + `Leech Solution` fits only `Pure Leech Solution` though
/// `Compound Leech Solution` was read): the run must be [`MARGIN`] letters longer than any part of it another card
/// holds,
/// and it must not rest on a neighbour's letters at the text's ends ([`core_apart`]: `re` of `Tincture` +
/// `Leech Solution` is a run only `Pure Leech Solution` has, two letters longer than the part `Compound Leech
/// Solution` holds). A longer name around another card's whole name must show the seam ([`longer_shown`]). Runs
/// fitting cards of one family are settled by the archetype read ([`family`]).
pub(super) fn partial(t: &str, cands: &[(usize, String)], clipped: bool) -> Option<(usize, How)> {
    let (len, hits) = run_hits(t, cands);
    let ok = |i: usize| {
        let z = cands.iter().find(|(j, _)| *j == i).map_or("", |(_, z)| z.as_str());
        // The run itself (one of them) is not mostly another card's: the part of it any other name holds is MARGIN
        // letters shorter, and it is still this card's alone without a neighbour's letters at the ends of the text.
        let letters: Vec<char> = t.chars().collect();
        let apart = (0..=letters.len().saturating_sub(len)).filter(|&s| z.contains(&letters[s..s + len].iter().collect::<String>())).any(|s| {
            let r: String = letters[s..s + len].iter().collect();
            let shared = cands.iter().filter(|(j, _)| *j != i).map(|(_, o)| longest_common(&r, o)).max().unwrap_or(0);
            len >= shared + MARGIN && core_apart(&letters, s, len, i, cands)
        });
        apart && contained_ok(i, cands, clipped) && longer_shown(i, cands, t) && named_ok(z, cands)
    };
    match hits[..] {
        [] => None,
        [i] => ok(i).then_some((i, How::Partial)),
        ref hits => family(hits, t).filter(|&i| contained_ok(i, cands, clipped) && longer_shown(i, cands, t)).map(|i| (i, How::Partial)),
    }
}

/// The fewest letters [`partial`] takes.
const RUN: usize = 8;

/// How many letters longer the run [`partial`] takes must be than any part of it another card's name holds: a
/// neighbour's letter running into the name does not make it another card (the reviewer's 70 seams of 2026-11-08,
/// one letter each, are in the tests). Two letters run in (the reviewer's 32 seams, `re` + `Leech Solution`) are
/// caught by [`core_apart`] at the ends of the text, and two letters of another name before a whole name
/// (`nt Xenopurge`) by [`longer_shown`]. A name cut two letters past the part another card shares is read only
/// when no neighbour's line could show those two letters (`Spinal Solution - Agi`: no line of a potion name starts
/// `gi`; `Twin Hormone - Pe` is not read, `Pest` and `Petrified` start `pe`).
const MARGIN: usize = 2;

/// The length of the longest run of `t` that `z` holds.
fn longest_common(t: &str, z: &str) -> usize {
    let letters: Vec<char> = t.chars().collect();
    (1..=letters.len()).rev().find(|&len| (0..=letters.len() - len).any(|s| z.contains(&letters[s..s + len].iter().collect::<String>()))).unwrap_or(0)
}

/// The cards whose names hold the longest run of at least [`RUN`] letters of `t` that any of them holds, and its
/// length.
fn run_hits(t: &str, cands: &[(usize, String)]) -> (usize, Vec<usize>) {
    let letters: Vec<char> = t.chars().collect();
    for len in (RUN..=letters.len()).rev() {
        let mut hits: Vec<usize> = Vec::new();
        for start in 0..=letters.len() - len {
            let run: String = letters[start..start + len].iter().collect();
            for (i, z) in cands {
                if z.contains(&run) && !hits.contains(i) {
                    hits.push(*i);
                }
            }
        }
        if !hits.is_empty() {
            return (len, hits);
        }
    }
    (0, Vec::new())
}

/// Card `i` may be taken from a name line covered at one side (`clipped`) only when its name is not all inside
/// another card's name of the category.
fn contained_ok(i: usize, cands: &[(usize, String)], clipped: bool) -> bool {
    let Some((_, z)) = cands.iter().find(|(j, _)| *j == i) else { return true };
    !clipped || !inside_another(z, cands)
}

/// Card `i` taken by a part of its name may be a longer name around another card's whole name (`Potent Xenopurge
/// Powder` around `Xenopurge Powder`): then the text read must show where the two meet (`tentx`, the last letters
/// of `Potent` and the first of `Xenopurge`), not only letters both share. The neighbouring card's `Ointment` ran
/// into `Xenopurge POW(` as `nt Xenopurge` in 2026-11-07_15-41-02 sample-0059, a run only `Potent Xenopurge Powder`
/// has.
fn longer_shown(i: usize, cands: &[(usize, String)], t: &str) -> bool {
    let Some((_, zi)) = cands.iter().find(|(j, _)| *j == i) else { return true };
    cands.iter().filter(|(j, z)| *j != i && z.len() < zi.len() && zi.contains(z.as_str())).all(|(_, z)| {
        let at = zi.find(z.as_str()).unwrap_or(0);
        let (pre, post) = (&zi[..at], &zi[at + z.len()..]);
        let seam = if pre.is_empty() { format!("{}{}", &z[z.len() - 1..], &post[..post.len().min(4)]) } else { format!("{}{}", &pre[pre.len().saturating_sub(4)..], &z[..1]) };
        t.contains(&seam)
    })
}

/// Of the cards `hits` of one family (`Slender-Limb Powder - Pest` and `- Bone Sentry`), the one whose archetype the
/// text read (normalized) shows ([`SUFFIX_STEMS`]); None when the hits are not one family or not exactly one shows.
fn family(hits: &[usize], t: &str) -> Option<usize> {
    let parts: Vec<(&str, &str)> = hits.iter().map(|&i| names().cards[i].printed_en().rsplit_once(" - ")).collect::<Option<Vec<_>>>()?;
    if parts.iter().any(|p| p.0 != parts[0].0) {
        return None;
    }
    let shown: Vec<usize> = hits.iter().zip(&parts).filter(|(_, (_, suffix))| suffix_shown(suffix, t)).map(|(&i, _)| i).collect();
    match shown[..] {
        [i] => Some(i),
        _ => None,
    }
}

/// The parts of each archetype a card name read cut or garbled may still show: `Bo Sentry` and `- entrv` for Bone
/// Sentry, `Aember` for Aemberon. None of them is in another archetype's name.
const SUFFIX_STEMS: [(&str, &[&str]); 4] = [("pest", &["pest"]), ("bonesentry", &["bonesentr", "sentr", "entr"]), ("agito", &["agito", "agit"]), ("aemberon", &["aemberon", "aember", "ember"])];

/// The archetype `suffix` (as the card name prints it) shows in normalized text `t` ([`SUFFIX_STEMS`]).
fn suffix_shown(suffix: &str, t: &str) -> bool {
    let want = norm(suffix);
    SUFFIX_STEMS.iter().filter(|(a, _)| want.starts_with(a)).any(|(_, stems)| stems.iter().any(|s| t.contains(s)))
}

/// The archetype words as the effect texts print them, by stem: `Pests`, `Bone Sentries`, `Agitos`/`Agitoes`,
/// `Aemberons`.
const ARCHETYPES: [&str; 4] = ["pest", "bonesentr", "agito", "aemberon"];

/// The family card whose name read shows only its family (`Spinal Solution -A`, `Slender-Lirfib Powde`): of the
/// family's cards one of the reads `tries` fits ([`run_hits`]), the one whose archetype the effect text prints,
/// alone of the four (`Add 1 group(s) of Agitoes`). None otherwise, and always for the Traditional Chinese client.
pub(super) fn family_by_effect(tries: &[Vec<Line>], effect: &str, category: Category) -> Option<usize> {
    if !english() {
        return None;
    }
    let e = norm(effect);
    let shown: Vec<&str> = ARCHETYPES.iter().copied().filter(|a| e.contains(a)).collect();
    let [one] = shown[..] else { return None };
    // The effect text up to its first digit: the name's second line when the name wraps (`Spinal Solution -` over
    // `Aemberon`), before the effect itself ("Add 1 group(s)", "Select 1 group(s)").
    let opening = norm(effect.split(|c: char| c.is_ascii_digit()).next().unwrap_or(""));
    tries.iter().find_map(|t| {
        let text = joined(t);
        let dash = text.trim_end().ends_with(" -");
        let (_, mut hits) = run_hits(&no_digits(&norm(&text)), candidates(category));
        // A name read with no run of a card's name: the OCR's look-alike letters folded together, and only with the
        // archetype said before the effect or after a dash. Spinal Solution - Aemberon reads as `SpinatSöfution`,
        // `Spinal S6fution`, `binatSöfution` (9 times in 6 English games, 2026-11-07/08), 5 to 7 letters of a run.
        if hits.is_empty() && (dash || opening.contains(one)) {
            hits = run_hits(&folded(&text), &folded_candidates(category)).1;
            hits.retain(|&i| names().cards[i].printed_en().contains(" - "));
        }
        // A name read up to " -" has an archetype suffix after it: the family's cards without one are not it
        // (2026-11-07_14-47-35 sample-0042, `Spinal Solution -`: Gray Matter, Hollow and Petrified Spinal Solution held
        // the run too, and with them in the family check failed before the effect text's lone Aemberon was looked at).
        // A space before the dash, so a hyphen inside a name read cut (`Targeted Xeno-`) does not count.
        if dash {
            hits.retain(|&i| names().cards[i].printed_en().contains(" - "));
        }
        let parts: Vec<(&str, &str)> = hits.iter().map(|&i| names().cards[i].printed_en().rsplit_once(" - ")).collect::<Option<Vec<_>>>()?;
        if hits.len() < 2 || parts.iter().any(|p| p.0 != parts[0].0) {
            return None;
        }
        let fit: Vec<usize> = hits.iter().zip(&parts).filter(|(_, (_, suffix))| norm(suffix).starts_with(one)).map(|(&i, _)| i).collect();
        match fit[..] {
            [i] => Some(i),
            _ => None,
        }
    })
}

/// `s` with the letters the OCR mistakes for one another folded together: t f i j 1 | as l, ö ó ò 6 0 as o (folded
/// before [`norm`] and [`no_digits`], so a 6 read for an o is kept). Only for [`family_by_effect`]'s last resort.
fn folded(s: &str) -> String {
    let f: String = s
        .chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            't' | 'f' | 'i' | 'j' | '1' | '|' | 'l' => 'l',
            'ö' | 'ó' | 'ò' | '6' | '0' | 'o' => 'o',
            c => c,
        })
        .collect();
    no_digits(&norm(&f))
}

/// [`candidates`] folded ([`folded`]).
fn folded_candidates(category: Category) -> Vec<(usize, String)> {
    names().cards.iter().enumerate().filter(|(_, c)| c.category == category && c.printed_en().is_ascii()).map(|(i, c)| (i, folded(c.printed_en()))).filter(|(_, z)| !z.is_empty()).collect()
}

/// [`super::kit_ok`]: `Potion Kit (Large)`, `Potion Kit (Small)`, `Rare Potion Kit (Small)`. The size needs `large` or
/// `small` in the name read, or the effect text to print only 5 or only 3; rare needs `rare` in the name read or the
/// effect text, and a kit that is not rare must have neither.
pub(super) fn kit_ok(card: &CardName, name_text: &str, effect: &str) -> bool {
    let en = norm(card.printed_en());
    if !en.contains("potionkit") {
        return true;
    }
    let (n, e) = (norm(name_text), norm(effect));
    let size = if en.contains("large") { n.contains("large") || (e.contains('5') && !e.contains('3')) } else { n.contains("small") || (e.contains('3') && !e.contains('5')) };
    let rare_seen = n.contains("rare") || e.contains("rare");
    size && rare_seen == en.starts_with("rare")
}

/// [`super::species_ok`]: a name ending in ` - <archetype>` (`Twin Hormone - Pest`) needs that archetype in the name
/// read ([`suffix_shown`]), or the effect text to print that archetype and no other.
pub(super) fn species_ok(card: &CardName, name_text: &str, effect: &str) -> bool {
    let Some((_, suffix)) = card.printed_en().rsplit_once(" - ") else { return true };
    let Some(want) = ARCHETYPES.iter().copied().find(|a| norm(suffix).starts_with(a)) else { return true };
    if suffix_shown(suffix, &norm(name_text)) {
        return true;
    }
    let e = norm(effect);
    let shown: Vec<&str> = ARCHETYPES.iter().copied().filter(|a| e.contains(a)).collect();
    shown == [want]
}

/// [`super::both_result_buttons`] (normalized): "End Surgery" and "Start Surgery".
pub(super) fn both_result_buttons(b: &str) -> bool {
    b.contains("startsurgery") && b.contains("endsurgery")
}

/// [`super::wide_result_label`] (normalized): "Ultimate Activity".
pub(super) fn wide_result_label(n: &str) -> bool {
    n.contains("activity")
}

/// The result screen's ultimate activity read by OCR, for when its digits do not read by template: the English
/// client's are a size bigger than the panel's and the samples showed only a few values, so the learned templates
/// read 1 of 25 result frames (2026-11-08). The box is the digits' own span (`glyphs`, those of [`RESULT_VALUE`])
/// with 8 pixels each side, rows 128 to 176: the gold flourishes either side of the box, read with a five-digit
/// value, made `94481` read `9448Y__-`. Taken only when what is left after trimming non-digits off both ends is all
/// digits, as many as the glyphs: on the 25 result frames of the 8 sample games, 24 read right, none wrong, and one
/// (a frame fading in, `_6_12849`) not taken.
pub(super) fn result_by_ocr(text: &Text, glyphs: &[Glyph]) -> Option<i64> {
    let (x0, x1) = (glyphs.iter().map(|g| g.x0).min()?, glyphs.iter().map(|g| g.x1).max()?);
    // The engine now and then reads nothing in a box (2026-11-07_15-47-37 sample-0085): the same digits are read in
    // a taller box and at 3x as well, in one batch, and the first read that passes is taken.
    let (box1, box2) = (Rect::new(x0 - 8, 128, x1 - x0 + 16, 48), Rect::new(x0 - 8, 120, x1 - x0 + 16, 64));
    text.batch(&[(box1, 2.0), (box2, 2.0), (box1, 3.0)]).iter().find_map(|lines| {
        let read = joined(lines);
        let digits = read.trim_matches(|c: char| !c.is_ascii_digit());
        (digits.len() == glyphs.len() && digits.chars().all(|c| c.is_ascii_digit())).then(|| digits.parse().ok()).flatten()
    })
}

/// The result screen (label and buttons normalized): "Ultimate Activity" and "Start Surgery" or "End Surgery".
pub(super) fn is_result(label: &str, buttons: &str) -> bool {
    label.contains("ultimateactivity") && ["startsurgery", "endsurgery"].iter().any(|b| buttons.contains(b))
}

/// [`super::card_anchors`] with the type texts of neighbouring cards read as one line cut apart first
/// ([`split_types`]).
pub(super) fn card_anchors(types: &[Line]) -> Vec<(i32, i32, String)> {
    let split: Vec<Line> = types.iter().flat_map(split_types).collect();
    anchors_of(&split)
}

/// The type texts of neighbouring cards read as one line: the three protocols' `RareSurgicaI Protocol MagicSurgical
/// Prot NormalSurøiral` (2026-11-07 samples, 20 protocol offers read as a single card). The line is cut before each
/// word that starts a type text (`Normal`, `Magic`, `Rare`, `ULT`, `Supreme`, or a second `Surgical` after an
/// instrument's), and each part is placed by its share of the line's characters (the letters are about the same
/// width). A line with one type text is kept as it is.
fn split_types(l: &Line) -> Vec<Line> {
    let chars: Vec<char> = l.text.chars().collect();
    let mut words: Vec<(usize, usize)> = Vec::new();
    let mut k = 0;
    while k < chars.len() {
        if chars[k].is_whitespace() {
            k += 1;
            continue;
        }
        let start = k;
        while k < chars.len() && !chars[k].is_whitespace() {
            k += 1;
        }
        words.push((start, k));
    }
    let word = |&(a, b): &(usize, usize)| chars[a..b].iter().collect::<String>().to_lowercase();
    let mut parts: Vec<Vec<(usize, usize)>> = Vec::new();
    for w in &words {
        let lw = word(w);
        let starts_type = ["normal", "magic", "rare", "ult", "supreme"].iter().any(|r| lw.starts_with(r));
        let done = parts.last().is_some_and(|p| {
            let t: String = p.iter().map(word).collect();
            t.contains("prot") || t.contains("potio") || t.contains("instrum") || t.contains("strume")
        });
        match parts.last_mut() {
            Some(p) if !(starts_type || (done && lw.starts_with("surgical"))) => p.push(*w),
            _ => parts.push(vec![*w]),
        }
    }
    if parts.len() < 2 {
        return vec![l.clone()];
    }
    let total = chars.len().max(1) as i32;
    parts
        .iter()
        .map(|p| {
            let (a, b) = (p[0].0 as i32, p[p.len() - 1].1 as i32);
            let text = chars[p[0].0..p[p.len() - 1].1].iter().collect();
            let (x0, x1) = (l.rect.x + l.rect.w * a / total, l.rect.x + l.rect.w * b / total);
            Line { text, rect: Rect::new(x0, l.rect.y, (x1 - x0).max(1), l.rect.h) }
        })
        .collect()
}

/// A long name printed on two lines (`Anesthetic Tincture -` over `Agito`, `Holistic Acupuncture` over `Therapy`):
/// each line alone fits two cards of a family or none, so two rows in a row are read together as well.
pub(super) fn two_rows<'a>(rows: &[Vec<&'a Line>]) -> Vec<Vec<&'a Line>> {
    rows.windows(2).map(|w| w[0].iter().chain(w[1].iter()).copied().collect()).collect()
}

/// [`super::settle`] for the English client: `singles` (one per row, top to bottom) and `merged` (two rows read
/// together, [`two_rows`]). A row under another that reads a name all inside another card's name (`Xeno-Hormone`
/// under `geted`, the rest of `Targeted` covered by the left neighbour) may be the second line of the longer name:
/// it is not taken.
pub(super) fn settle_rows(singles: Vec<Result<(usize, How), String>>, merged: Vec<Result<(usize, How), String>>) -> Result<(usize, How), String> {
    let mut tries = singles;
    for t in tries.iter_mut().skip(1) {
        if let Ok((i, How::Name)) = *t {
            let card = &names().cards[i];
            if inside_another(&no_digits(&norm(card.printed_en())), candidates(card.category)) {
                *t = Err(format!("對到「{}」，但上面還有一行，可能是較長卡名的第二行", card.zh.as_deref().unwrap_or(card.printed_en())));
            }
        }
    }
    tries.extend(merged);
    if let Some(longer) = longest_name(&tries) {
        return longer;
    }
    super::settle(tries)
}

/// Of the names read from one card's rows, one read whole inside another read whole or in part (`Xeno-Hormone` on
/// one row, `Targeted Xeno-Hormone` read over two, or `geted Xeno-Hormone`, a part only it has with the seam
/// shown): the longer, which holds the shorter, is the card.
pub(super) fn longest_name(tries: &[Result<(usize, How), String>]) -> Option<Result<(usize, How), String>> {
    let read: Vec<(usize, How)> = tries.iter().filter_map(|t| match *t {
        Ok((i, how @ (How::Name | How::Partial))) => Some((i, how)),
        _ => None,
    }).collect();
    let en = |i: usize| norm(names().cards[i].printed_en());
    read.iter().copied().find(|&(i, _)| read.iter().any(|&(j, how)| how == How::Name && j != i && en(i).len() > en(j).len() && en(i).contains(&en(j)))).map(Ok)
}

/// [`super::name_in_effect`]: the whole names of the category (digits dropped on both sides) that start the effect
/// text, within its first two letters; the longest, when all the others are inside it. A name all inside another
/// card's name (`Xeno-Hormone`, `Xenopurge Powder`) or starting one (`Xenopanacea`) is not taken: the effect text's
/// area may hold only the name's second line, or only its first.
pub(super) fn name_in_effect(effect: &str, category: Category) -> Option<usize> {
    let e = no_digits(&norm(effect));
    let cands = candidates(category);
    let hits: Vec<&(usize, String)> = cands.iter().filter(|(_, z)| e.find(z.as_str()).is_some_and(|at| e[..at].chars().count() <= 2)).collect();
    let longest = hits.iter().max_by_key(|(_, z)| z.len())?;
    let alone = hits.iter().all(|(_, z)| longest.1.contains(z.as_str()));
    (alone && !inside_another(&longest.1, cands) && named_ok(&longest.1, cands)).then_some(longest.0)
}

/// Lower-case ASCII letters only (digits, spaces and punctuation dropped): how [`word_candidates`] and
/// [`by_effect_words`] compare names read.
fn letters(s: &str) -> String {
    s.to_lowercase().chars().filter(char::is_ascii_lowercase).collect()
}

/// The words of a card's effect text as [`by_effect_words`] compares them: the text lower-cased, the name read cut off
/// its front when it starts it, split into runs of three or more ASCII letters and runs of digits; plus the type
/// text's rarity as `type:<rarity>` (`MagicPotion` gives `type:magic`).
pub(super) fn effect_words(raw_name: &str, raw_effect: &str, raw_type: &str) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = effect_tokens(raw_name, raw_effect).into_iter().collect();
    let ty = letters(raw_type);
    for rarity in ["normal", "magic", "rare", "ult", "supreme"] {
        if ty.starts_with(rarity) {
            out.insert(format!("type:{rarity}"));
        }
    }
    out
}

/// The words of [`effect_words`] in the order the effect text prints them, the rarity left out.
fn effect_tokens(raw_name: &str, raw_effect: &str) -> Vec<String> {
    let (name, mut effect) = (raw_name.to_lowercase(), raw_effect.to_lowercase());
    if !name.is_empty() && effect.starts_with(&name) {
        effect = effect[name.len()..].to_string();
    }
    let mut out = Vec::new();
    let chars: Vec<char> = effect.chars().collect();
    let mut k = 0;
    while k < chars.len() {
        let class = |c: char| if c.is_ascii_lowercase() { 1 } else if c.is_ascii_digit() { 2 } else { 0 };
        let kind = class(chars[k]);
        let start = k;
        while k < chars.len() && kind != 0 && class(chars[k]) == kind {
            k += 1;
        }
        if kind == 0 {
            k += 1;
        } else if kind == 2 || k - start >= 3 {
            out.push(chars[start..k].iter().collect());
        }
    }
    out
}

/// The fewest letters of a name read that [`word_candidates`] matches.
const WORD_RUN: usize = 6;

/// The cards of `category` a name read may be: those holding the longest run (at least [`WORD_RUN`] letters) of the
/// letters read; also with two letters cut off the front, the back or both (a neighbour's letters, [`EDGE`]), all
/// of them together. `Twin Hormone - Pe` gives both Twin Hormones, `Bile Solution` both Bile Solutions.
pub(super) fn word_candidates(raw_name: &str, category: Category) -> Vec<usize> {
    let t: Vec<char> = letters(raw_name).chars().collect();
    let pool: Vec<(usize, String)> = names().cards.iter().enumerate().filter(|(_, n)| n.category == category && !n.en.is_empty()).map(|(i, n)| (i, letters(n.printed_en()))).collect();
    let cut = |a: usize, b: usize| t.get(a..t.len().saturating_sub(b)).map(|s| s.iter().collect::<String>()).unwrap_or_default();
    // Each cut's longest run held by some card, and the cards holding it.
    let mut found: Vec<(usize, Vec<usize>)> = Vec::new();
    for text in [cut(0, 0), cut(EDGE, 0), cut(0, EDGE), cut(EDGE, EDGE)] {
        let text: Vec<char> = text.chars().collect();
        for len in (WORD_RUN..=text.len()).rev() {
            let hits: Vec<usize> =
                pool.iter().filter(|(_, z)| (0..=text.len() - len).any(|s| z.contains(&text[s..s + len].iter().collect::<String>()))).map(|(i, _)| *i).collect();
            if !hits.is_empty() {
                found.push((len, hits));
                break;
            }
        }
    }
    // Only the cuts whose run is within EDGE letters of the longest: a cut that kept far fewer letters widens the cards
    // past what the name read shows (2026-11-08_09-50-14 learn-0027, `in Hormone - Pe:`: whole, its 11 letters are in
    // Twin Hormone - Pest alone, but cut at both ends `hormone` added Viviparous Hormone, which prints "Pests" too, and
    // with five cards open the card went unread).
    let longest = found.iter().map(|(len, _)| *len).max().unwrap_or(0);
    let out: BTreeSet<usize> = found.into_iter().filter(|(len, _)| len + EDGE >= longest).flat_map(|(_, hits)| hits).collect();
    out.into_iter().collect()
}

/// A word counts for a candidate when at least this share of its known effect texts hold it...
const WORD_KEEP: f32 = 0.6;
/// ...and less than this share of every other candidate's.
const WORD_LEAK: f32 = 0.1;

/// For each of `cands`, the words of its effect text no other of them prints ([`WORD_KEEP`], [`WORD_LEAK`], from
/// [`super::super::templates_en_effects::EFFECT_WORDS`]), words of card names left out (the name's own line runs into
/// the effect text's area). None when a candidate has fewer than two known effect texts.
fn distinguishing(cands: &[usize]) -> Option<Vec<BTreeSet<&'static str>>> {
    use super::super::templates_en_effects::{EFFECT_WORDS, Words};
    static NAME_WORDS: OnceLock<BTreeSet<String>> = OnceLock::new();
    let name_words = NAME_WORDS.get_or_init(|| {
        let mut w = BTreeSet::new();
        for n in &names().cards {
            // Words only: a name's digits (`F1 Gen`) must not hide the counts that tell cards apart.
            w.extend(effect_words("", n.printed_en(), "").into_iter().filter(|x| x.starts_with(|c: char| c.is_ascii_lowercase())));
        }
        w
    });
    let known: Vec<(u16, Words)> =
        cands.iter().map(|&i| EFFECT_WORDS.iter().find(|(en, _, _)| *en == names().cards[i].en).map(|&(_, n, words)| (n, words))).collect::<Option<_>>()?;
    if known.iter().any(|&(n, _)| n < 2) {
        return None;
    }
    let share = |(n, words): (u16, Words), w: &str| words.iter().find(|(x, _)| *x == w).map_or(0.0, |&(_, c)| f32::from(c) / f32::from(n));
    Some(
        known
            .iter()
            .enumerate()
            .map(|(k, &mine)| {
                mine.1
                    .iter()
                    .map(|&(w, _)| w)
                    .filter(|w| share(mine, w) >= WORD_KEEP && !name_words.contains(*w))
                    .filter(|w| known.iter().enumerate().all(|(j, &other)| j == k || share(other, w) < WORD_LEAK))
                    .collect()
            })
            .collect(),
    )
}

/// The card `cards[k]`, not recognized by its name, is when the name read leaves a few cards open
/// ([`word_candidates`]) and its effect and type text print the distinguishing words ([`distinguishing`]) of exactly
/// one of them: `Twin Hormone - Pe` with `Pests` in the effect is Twin Hormone - Pest, `Xenopurge Powd` with `127` is
/// Xenopurge Powder (Potent prints 154), `Bile Solution` with `random` is Green Bile Solution. The words the cards
/// beside it print are left out first (overlapping cards' text runs into the effect text's area: a neighbour's
/// `+73` read in a Spinal Solution's), except a neighbour whose name read is the same (three Xeno-Hormones). None
/// when no candidate's words or more than one's are there. Measured on the 8 sample games of 2026-11-07, each game
/// judged by the other seven's words: 43 of the 57 unread cards with candidates named, all 43 right by eye; on 2,276
/// cut-name cases made from cards read by their whole name, 1,824 named and 0 wrong.
pub(super) fn by_effect_words(cards: &[Card], k: usize) -> Option<usize> {
    let card = &cards[k];
    if card.index.is_some() {
        return None;
    }
    let cands = word_candidates(&card.raw_name, card.category);
    if cands.len() < 2 {
        return None;
    }
    let mut seen = effect_words(&card.raw_name, &card.raw_effect, &card.raw_type);
    let tokens = effect_tokens(&card.raw_name, &card.raw_effect);
    let mut order: Vec<usize> = (0..cards.len()).collect();
    order.sort_by_key(|&j| cards[j].x);
    let at = order.iter().position(|&j| j == k)?;
    for side in [at.checked_sub(1), Some(at + 1)].into_iter().flatten() {
        let Some(&j) = order.get(side) else { continue };
        let n = &cards[j];
        if n.en.is_none() && letters(&n.raw_name) == letters(&card.raw_name) {
            continue;
        }
        // A neighbour's word is taken as seeping in from the neighbour's text, unless it is an archetype word this card
        // prints right after a word the neighbour does not print: 2026-11-08_07-42-56 frame-0020, `Twin Hormone - Pe`
        // beside `Eggshell Powder` ("If they are the Pests"), lost its own "mutate them into Pests" and with it the
        // only word telling the Twin Hormones apart, so the card went unread until a tooltip named it. Other words still
        // go: the first word of a run seeping in follows one of this card's own (06-46-26 learn-0008, `Spinal Solution
        // - Aemberon` ending "...their Activity into with a" from the card beside it, was named by that `into` as Gray
        // Matter Spinal Solution when every word was let through).
        let theirs = effect_words(&n.raw_name, &n.raw_effect, &n.raw_type);
        for w in &theirs {
            let archetype = ["pest", "agito", "aemberon", "sentr"].iter().any(|a| w.starts_with(a));
            let own = archetype && tokens.windows(2).any(|p| p[1] == *w && !theirs.contains(&p[0]));
            if !own {
                seen.remove(w);
            }
        }
    }
    let words = distinguishing(&cands)?;
    let hits: Vec<usize> = cands.iter().zip(&words).filter(|(_, ws)| ws.iter().any(|w| seen.contains(*w))).map(|(&i, _)| i).collect();
    match hits[..] {
        [i] if suffix_agrees(&card.raw_name, names().cards[i].printed_en()) => Some(i),
        _ => None,
    }
}

/// The archetype suffix the name read starts to show agrees with the card's: `Twin Hormone - Pe` is not `Twin Hormone -
/// Bone Sentry`. The first word after the name's last `-` is looked at when it starts like an archetype (`p` Pest, `b`
/// Bone Sentry, `a` and the next letter Agito or Aemberon); anything else, or a card with no suffix, agrees. Only that
/// word: the next line runs in after it (`Spinal Solution -A Scl`, the `Scl` of "Select"). Guards [`by_effect_words`]
/// against an archetype word kept from a neighbour's text naming the family's other card (the independent review of
/// 2026-11-08).
fn suffix_agrees(raw_name: &str, card_en: &str) -> bool {
    let Some((_, suffix)) = card_en.rsplit_once(" - ") else { return true };
    let Some((_, shown)) = raw_name.rsplit_once('-') else { return true };
    let shown = shown.split_whitespace().next().unwrap_or("");
    let (shown, suffix): (Vec<char>, Vec<char>) = (letters(shown).chars().collect(), letters(suffix).chars().collect());
    match shown[..] {
        [] => true,
        ['a', second, ..] => suffix.first() == Some(&'a') && suffix.get(1) == Some(&second),
        [first, ..] if matches!(first, 'p' | 'b' | 'a') => suffix.first() == Some(&first),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx(en: &str) -> usize {
        names().cards.iter().position(|n| n.en == en).unwrap()
    }

    /// A potion as read: unknown, or named by its whole name.
    fn potion(x: i32, en: Option<&str>, name: &str, ty: &str, effect: &str) -> Card {
        let mut c = Card { category: Category::Potion, index: None, id: None, en: None, zh: None, how: None, raw_name: name.into(), raw_type: ty.into(), raw_effect: effect.into(), why: None, x, dy: 0, raised: false };
        if let Some(en) = en {
            c.set(idx(en), How::Name);
        }
        c
    }

    /// [`effect_words`]: the name read cut off the front, words of three letters or more and numbers, the rarity.
    #[test]
    fn effect_words_are_words_numbers_and_the_rarity() {
        let w = effect_words("Xenopurge P", "Xenopurge Powd Select 1 group(s) monsters and +127 heir", "MagicPotion");
        let want: BTreeSet<String> = ["owd", "select", "1", "group", "monsters", "and", "127", "heir", "type:magic"].iter().map(|s| s.to_string()).collect();
        assert_eq!(w, want);
    }

    /// [`word_candidates`]: a name cut at its end or by a neighbour's letters leaves the cards it may be.
    #[test]
    fn a_cut_name_leaves_the_cards_it_may_be() {
        let mut twins = vec![idx("Twin Hormone - Pest"), idx("Twin Hormone - Bone Sentry")];
        twins.sort();
        assert_eq!(word_candidates("Twin Hormone - Pe", Category::Potion), twins);
        let mut biles = vec![idx("Green Bile Solution"), idx("Viscous Bile Solution")];
        biles.sort();
        assert_eq!(word_candidates("Bile Solution", Category::Potion), biles);
        assert_eq!(word_candidates("Green Bile Solution", Category::Potion), vec![idx("Green Bile Solution")]);
        // `Twin` cut to `in`: the cut at both ends (`hormone`) is far shorter than the whole and does not count.
        assert_eq!(word_candidates("in Hormone - Pe:", Category::Potion), twins);
    }

    /// [`suffix_agrees`]: the suffix the name read starts to show against the card's archetype.
    #[test]
    fn a_shown_suffix_must_agree_with_the_card() {
        assert!(suffix_agrees("Twin Hormone - Pe", "Twin Hormone - Pest"));
        assert!(!suffix_agrees("Twin Hormone - Pe", "Twin Hormone - Bone Sentry"));
        assert!(suffix_agrees("T vuln HormOne - B Sentry", "Twin Hormone - Bone Sentry"));
        assert!(suffix_agrees("Spinal Solution - Ag", "Spinal Solution - Agito"));
        assert!(!suffix_agrees("Spinal Solution - Ae", "Spinal Solution - Agito"));
        assert!(suffix_agrees("Spinal Solution - A", "Spinal Solution - Agito"), "one letter does not tell the two apart");
        assert!(suffix_agrees("inal Solution -A Scl", "Spinal Solution - Agito"), "the next line's `Scl` is not the suffix");
        // Nothing shown after the dash, letters no archetype starts with, or a card with no suffix: no say.
        assert!(suffix_agrees("Spinal Solution -", "Spinal Solution - Agito"));
        assert!(suffix_agrees("lender-Litfb Powder - entrv", "Slender-Limb Powder - Bone Sentry"));
        assert!(suffix_agrees("Bile Solution", "Green Bile Solution"));
        assert!(suffix_agrees("Twin Hormone - Pe", "Viviparous Hormone"));
    }

    /// [`by_effect_words`] on hands of the English samples, raw texts as read.
    #[test]
    fn a_cut_name_is_named_by_the_words_only_one_card_prints() {
        // 2026-11-07_11-35-12 sample-0019: `Pests` names the Twin Hormone.
        let hand = vec![
            potion(1184, None, "Twin Hormone - Pe", "NormalPotion", "Twin Hormone - P Select 1 group(s) monsters, mutat them into Pests, add 1 group(s) o"),
            potion(1396, Some("Green Bile Solution"), "Green Bite Solution", "MagicPotion", "Green Bile Solution Select 1-2 groups of monsters, mutate them into random Magic monsters, an"),
            potion(1622, Some("Bewitching Tincture"), "Bewitching Tincture", "MagicPotio", "Bewitching Tincture Select 1 group(s) of monsters to awaken into monsters that are 1 tier(s) higher in rarity"),
        ];
        assert_eq!(by_effect_words(&hand, 0), Some(idx("Twin Hormone - Pest")));
        assert_eq!(by_effect_words(&hand, 1), None, "a card named by its name is left as it is");
        // 2026-11-07_15-41-02 sample-0076: Green Bile Solution, not Viscous.
        let hand = vec![
            potion(1205, None, "Bile Solution", "", "sen Bile Solution ect 1-2 groups of onsters, mutate lem into random gic monsters, and"),
            potion(1419, Some("Slender-Limb Powder - Pest"), "Powder - Pest", "NormalPotion", "Pest Add 1 group(s) of Pests and +73 to their 4>Quantity"),
            potion(1677, Some("Eggshell Powder"), "Eggshell Powder", "MagicPotion", "Eggshell Powder +25 A>Quantity to 1 random group(s) of monster. If they are Pests, add 1"),
        ];
        assert_eq!(by_effect_words(&hand, 0), Some(idx("Green Bile Solution")));
        // 2026-11-07_14-47-35 sample-0023, the five-card fan: `127` is Xenopurge Powder (Potent prints 154).
        let fan = |last: &str| {
            vec![
                potion(920, Some("Slough Solution"), "Slough Solution", "", "Slough Solution lect 1-2 groups of onsters and fuse into 1 group(s) o Pests"),
                potion(1178, Some("Mutagenic Powder"), "Mutagenic Pow e", "MagicPotion", "elect 1-2 groups onsters and +52 their 4>Quantity utate them into R monsters"),
                potion(1391, Some("Osteolytic Ointment"), ")steolytlc Ointmeni 'hen vou have", "RarePotion", ")steolytic Ointmeni hen you have at le I group(s) of Bo entries, remove up group(s) of monst"),
                potion(1609, Some("Potent Xenopurge Powder"), "Powder", "MagicPotion", "Powder Select 1 group(s) monsters and +15 heir 4>Quantity, t remove 2 group(s onsters of differ"),
                potion(1815, None, "Xenopurge P", "", last),
            ]
        };
        assert_eq!(by_effect_words(&fan("Xenopurge Powd Select 1 group(s) monsters and +127 heir 4>Quantity, th remove 1 group(s)"), 4), Some(idx("Xenopurge Powder")));
        // Without the counts the two Xenopurges are left open.
        assert_eq!(by_effect_words(&fan("Xenopurge Powd Select group(s) monsters and heir Quantity, th remove group(s)"), 4), None);
        // A word the card beside it prints does not count: a neighbour's `127` run into this card's text.
        let mut leak = fan("Xenopurge Powd Select group(s) monsters and +127 heir Quantity");
        leak[3] = potion(1609, None, "Spinal Solution", "", "Spinal Solution Add 1 group(s) +127 Agitoes");
        assert_eq!(by_effect_words(&leak, 4), None);
        // 2026-11-08_07-42-56 frame-0020: the neighbour prints "the Pests" too, but this card's "into Pests" is its own.
        let hand = vec![
            potion(1215, Some("Eggshell Powder"), "Eggshell Powder Tw", "aoicpotion", "Eggshell Powder +25 4>Quantity to random group(s) monster. If they a the O Pests, add 1"),
            potion(1418, None, "Twin Hormone - Pe.'", "NormalPotion", "Twin Hormone - Pel Select 1 group(s) monsters, mutat them into Pests, add 1 group(s) o ad"),
            potion(1630, Some("Osteogenic Powder"), "Osteogenic Powder", "MagicPotion", "Osteogenic Powder +40 4>Quantity to 1 random group(s) of monsters. If they are Bone Sentries, +127"),
        ];
        assert_eq!(by_effect_words(&hand, 1), Some(idx("Twin Hormone - Pest")));
        // 2026-11-08_09-50-14 learn-0027: `Twin` cut to `in`, named by "Pests" once Viviparous Hormone is not a candidate.
        let cut = vec![
            potion(1203, Some("Bewitching Tincture"), "Select 1 group(s) of", "MagicPotion", "Select 1 group(s) of monsters to awaken into monsters that are 1 tier(s) higher in rarity"),
            potion(1418, None, "in Hormone - Pe:", "NormalPotion", "in Hormone - P lect 1 group(s) onsters, mutat ty into Pests, th dd 1 group(s) o"),
            potion(1630, Some("Xenopurge Powder"), "Xenopurge Powder", "MagicPotion", "Xenopurge Powder Select 1 group(s) of monsters and +127 to their Quantity, then remove 1 group(s) of"),
        ];
        assert_eq!(by_effect_words(&cut, 1), Some(idx("Twin Hormone - Pest")));
        // A word the neighbour prints in the same place still goes: "the Pests" after a word it prints as well.
        let mut seeped = hand.clone();
        seeped[1].raw_effect = "Twin Hormone - Pel Select 1 group(s) monsters, mutat them the Pests, add 1 group(s) o ad".into();
        assert_eq!(by_effect_words(&seeped, 1), None);
        // 2026-11-08_06-46-26 learn-0008: "into with a" seeps in from the card beside it; its `into` is not taken.
        let hand = vec![
            potion(1205, None, "Binal SOtution nes on", "NormalPotion", "Aemberon d 1 group(s) o emberons and mons their V Activit into with a"),
            potion(1416, Some("Anesthetic Tincture - Aemberon"), "Anesthetic Tincture - Aemberon", "NormalPotion", "Anesthetic Tincture - Aemberon Select 1 group(s) of monsters to mutate into Aemberons, with a 50% chance to"),
            potion(1629, Some("Osteogenic Powder"), "Osteogenic Powder", "MagicPotion", "Osteogenic Powder +40 Quantity to 1 random group(s) of monsters. If they are Bone Sentries, +127"),
        ];
        assert_ne!(by_effect_words(&hand, 0), Some(idx("Gray Matter Spinal Solution")));
    }

    /// The English client's reading for the rest of this thread; the Traditional Chinese one again when dropped.
    struct English;

    impl English {
        fn on() -> Self {
            set_english_here(Some(true));
            English
        }
    }

    impl Drop for English {
        fn drop(&mut self) {
            set_english_here(None);
        }
    }

    fn en_of(m: Option<(usize, How)>) -> Option<String> {
        m.map(|(i, _)| names().cards[i].en.clone())
    }

    /// The titles and type texts as the OCR read them in the sample games, garbled and cut; off, the same text says
    /// nothing (the Traditional Chinese reading is unchanged).
    #[test]
    fn english_titles_and_type_texts() {
        assert_eq!((super::super::title_of("Select a Potion"), super::super::type_of("MagicPotion")), (None, (None, None)));
        let _en = English::on();
        let (title, ty) = (super::super::title_of, super::super::type_of);
        assert_eq!(title("Select a Potion"), Some(Category::Potion));
        assert_eq!(title("SelectåSüiÉiCåVProtocol"), Some(Category::Protocol));
        assert_eq!(title("Select a Surgical Instrument"), Some(Category::Instrument));
        assert_eq!(title("Ultimate Activity"), None);
        assert_eq!(ty("Magicpotion"), (Some(Category::Potion), Some(PotionRarity::Magic)));
        assert_eq!(ty("NormalPotion"), (Some(Category::Potion), Some(PotionRarity::Normal)));
        assert_eq!(ty("ULTPotion"), (Some(Category::Potion), Some(PotionRarity::Supreme)));
        assert_eq!(ty("RareSurgicaI Protocol"), (Some(Category::Protocol), None));
        assert_eq!(ty("MagicSurgical Prot"), (Some(Category::Protocol), None));
        assert_eq!(ty("Surgical Instrume"), (Some(Category::Instrument), None));
        assert!(super::super::both_result_buttons("End Surgery Start Surgery"));
    }

    fn m(t: &str, category: Category, clipped: bool) -> Option<String> {
        en_of(match_name(t, category, clipped, None, None))
    }

    /// [`folded`]: no two printed names of a category fold to one (folding must not make two cards one), and the
    /// reads it is for fold onto the name.
    #[test]
    fn folded_names_stay_apart() {
        for category in [Category::Potion, Category::Instrument, Category::Protocol] {
            let mut f: Vec<String> = folded_candidates(category).into_iter().map(|(_, z)| z).collect();
            let all = f.len();
            f.sort();
            f.dedup();
            assert_eq!(f.len(), all, "{category:?}");
        }
        assert!(folded("Spinal Solution - Aemberon").contains(&folded("SpinatSöfution")));
        assert!(folded("Spinal Solution - Aemberon").contains(&folded("S6fution")));
    }

    /// Names read whole, a letter off, or cut at either end (the reads of the hand-labelled sample frames): the run
    /// of 8 or more letters in one card name only; a run two cards of one family share gives the one whose
    /// archetype shows, or none.
    #[test]
    fn english_names_whole_misread_and_cut() {
        let _en = English::on();
        let p = |t: &str, clipped: bool| m(t, Category::Potion, clipped);
        assert_eq!(p("Anesthetic Tincture - Aemberon", false).as_deref(), Some("Anesthetic Tincture - Aemberon"));
        assert_eq!(p("Digestive Enzym Solution", false).as_deref(), Some("Digestive Enzyme Solution"));
        assert_eq!(p("Anesthetic Tincture - Aember", true).as_deref(), Some("Anesthetic Tincture - Aemberon"));
        assert_eq!(p("al Solution - Agito", true).as_deref(), Some("Spinal Solution - Agito"));
        assert_eq!(p("Gray Matter Spinal Solutio", false).as_deref(), Some("Gray Matter Spinal Solution"));
        assert_eq!(p("Powder - Pest", true).as_deref(), Some("Slender-Limb Powder - Pest"));
        assert_eq!(p("lender-Litfb Powder - entrv", true).as_deref(), Some("Slender-Limb Powder - Bone Sentry"), "the family by its archetype");
        assert_eq!(p("Ion - Slender-Limb Pow n pest", true).as_deref(), Some("Slender-Limb Powder - Pest"));
        assert_eq!(p("Slender-Limb Powder -", true), None, "Pest or Bone Sentry");
        assert_eq!(p("Spinal SOIutlon - Aemberon", false).as_deref(), Some("Spinal Solution - Aemberon"));
        assert_eq!(p("esthetic Tincture Agito", true).as_deref(), Some("Anesthetic Tincture - Agito"));
        let i = |t: &str| m(t, Category::Instrument, true);
        assert_eq!(i("an Iron Bone Saw").as_deref(), Some("Pig Iron Bone Saw"));
        assert_eq!(i("rtex Black Goat Gut Su").as_deref(), Some("Black Goat Gut Suture"));
        let r = |t: &str| m(t, Category::Protocol, false);
        assert_eq!(r("Holistic Acupuncture").as_deref(), Some("Holistic Acupuncture Therapy"));
        assert_eq!(r("Yellow Bile Therapy").as_deref(), Some("Yellow Bile Therapy"));
        assert_eq!(r("Epidermal Grattlt Experiment").as_deref(), Some("Epidermal Grafting Experiment"));
    }

    /// A card the English client prints under another name than the solver's (`enClient` in the name file): matched
    /// by what the client prints (2026-11-08_09-50-14 round 7, the ULT potion went unread), named by the solver's.
    #[test]
    fn english_client_names_are_matched() {
        let _en = English::on();
        let p = |t: &str, clipped: bool| m(t, Category::Potion, clipped);
        assert_eq!(p("The Holiest Water", false).as_deref(), Some("Purest Holy Water"));
        assert_eq!(p("The Holiest Wate", true).as_deref(), Some("Purest Holy Water"));
        let effect = super::super::name_in_effect("The Holiest Water All Agitoes awaken into monsters that are 2 tier(s) higher", Category::Potion);
        assert_eq!(effect.map(|i| names().cards[i].en.as_str()), Some("Purest Holy Water"));
    }

    /// A name all inside another card's (the reviewer's cases of 2026-11-08): read exactly, from a line not covered,
    /// it is that card; with letters before it, read near (a letter off), or from a covered line, it is not taken;
    /// a name starting another one (`Xenopanacea` of `Xenopanacea: Brood`) is never taken from one line.
    #[test]
    fn english_names_inside_other_names() {
        let _en = English::on();
        let p = |t: &str, clipped: bool| m(t, Category::Potion, clipped);
        assert_eq!(p("Xenopurge Powder", false).as_deref(), Some("Xenopurge Powder"));
        assert_eq!(p("Xenopurge Powder", true), None, "covered: may be Potent Xenopurge Powder");
        assert_eq!(p("t Xenopurge Powder", true), None, "Potent's seam not shown");
        assert_eq!(p("nt Xenopurge Powder", false), None);
        assert_eq!(p("nt Xenopurge POW(", false), None, "`Ointment`'s tail: not Potent either");
        assert_eq!(p("tent Xenopurge Pow", true).as_deref(), Some("Potent Xenopurge Powder"));
        assert_eq!(p("Potent Xenopurge Powder", false).as_deref(), Some("Potent Xenopurge Powder"));
        assert_eq!(p("d Xeno- Hormone", false), None);
        assert_eq!(p("Xeno-Hormone", false).as_deref(), Some("Xeno-Hormone"));
        assert_eq!(p("Targeted Xeno- Hormone", false).as_deref(), Some("Targeted Xeno-Hormone"));
        assert_eq!(p("Xenopanacea:", false), None, "the first line of a longer name");
        assert_eq!(p("Xenopanacea", false), None);
        assert_eq!(p("Rare Potion Kit (Small)", false).as_deref(), Some("Rare Potion Kit (Small)"));
        assert_eq!(p("e Potion Kit (Small)", true), None, "Rare's seam not shown");
    }

    /// Rows of one card read whole: the longer name around the shorter is the card (`Xeno-Hormone` on one row,
    /// `Targeted Xeno-Hormone` over two).
    #[test]
    fn the_longer_whole_name_wins() {
        let idx = |en: &str| names().cards.iter().position(|c| c.en == en).unwrap();
        let tries = [Ok((idx("Xeno-Hormone"), How::Name)), Err("x".to_string()), Ok((idx("Targeted Xeno-Hormone"), How::Name))];
        assert_eq!(longest_name(&tries), Some(Ok((idx("Targeted Xeno-Hormone"), How::Name))));
        let apart = [Ok((idx("Xeno-Hormone"), How::Name)), Ok((idx("Mutagenic Powder"), How::Name))];
        assert_eq!(longest_name(&apart), None);
    }

    /// `Targeted Xeno-Hormone` wrapped onto two lines, the first covered by the left neighbour (only `geted` read)
    /// and the second, `Xeno-Hormone`, read whole: the two rows read together give the longer card, the second row
    /// alone the shorter one, which must not win. The card is `Targeted Xeno-Hormone` or unread, never
    /// `Xeno-Hormone`; with nothing of the first line matched it is unread.
    #[test]
    fn a_covered_first_line_does_not_leave_the_shorter_name() {
        let _en = English::on();
        let read = |t: &str, clipped: bool| match_name(t, Category::Potion, clipped, None, None).ok_or_else(|| "名稱表裡沒有相符的繁中名".to_string());
        let idx = |en: &str| names().cards.iter().position(|c| c.en == en).unwrap();
        let (short, long) = (idx("Xeno-Hormone"), idx("Targeted Xeno-Hormone"));
        let singles = vec![read("geted", true), read("Xeno-Hormone", false)];
        assert_eq!(singles[1], Ok((short, How::Name)), "the second row alone");
        let merged = vec![read("geted Xeno-Hormone", true)];
        assert_eq!(merged[0], Ok((long, How::Partial)), "the two rows together");
        assert_eq!(settle_rows(singles, merged).map(|(i, _)| i), Ok(long));
        // The first line read as nothing that fits: unread rather than the shorter name.
        let singles = vec![read("xx", true), read("Xeno-Hormone", false)];
        let merged = vec![read("xx Xeno-Hormone", true)];
        assert!(settle_rows(singles, merged).is_err());
        // A name on its own row, nothing above it: still read.
        assert_eq!(settle_rows(vec![read("Xeno-Hormone", false)], Vec::new()), Ok((short, How::Name)));
    }

    /// The effect text's area reaching up into the name line: a name all inside another card's name
    /// (`Xeno-Hormone` of `Targeted Xeno-Hormone`, `Xenopurge Powder` of `Potent Xenopurge Powder`), with or
    /// without one or two letters before it, or one starting another (`Xenopanacea` of `Xenopanacea: Brood`) may be
    /// the end or the start of the longer name: it neither decides the card nor settles reads that disagree.
    #[test]
    fn a_name_inside_another_does_not_come_from_the_effect() {
        let _en = English::on();
        let idx = |en: &str| names().cards.iter().position(|c| c.en == en).unwrap();
        let effect = |e: &str| super::super::name_in_effect(e, Category::Potion).map(|i| names().cards[i].en.clone());
        for e in ["Xeno-Hormone Select 1 group(s) of monsters", "d Xeno-Hormone Select 1 group(s)", "nt Xenopurge Powder Remove 1 group(s)", "Xenopurge Powder Remove", "Xenopanacea Add 1 group(s)"] {
            assert_eq!(effect(e), None, "{e}");
        }
        assert_eq!(effect("Targeted Xeno-Hormone Select 1 group(s)").as_deref(), Some("Targeted Xeno-Hormone"));
        assert_eq!(effect("Potent Xenopurge Powder Remove 1 group(s)").as_deref(), Some("Potent Xenopurge Powder"));
        assert_eq!(effect("Mutagenic Powder Select 1 group(s)").as_deref(), Some("Mutagenic Powder"));
        let hits = [(idx("Xeno-Hormone"), How::Name), (idx("Targeted Xeno-Hormone"), How::Partial)];
        assert_eq!(super::super::effect_among(&hits, "eno-Hormone Select 1", Category::Potion), None);
        assert_eq!(super::super::effect_among(&hits, "Xeno-Hormone Select 1", Category::Potion), None);
    }

    /// The reviewer's 70 seams (2026-11-08): a name cut at one side with one letter of the neighbour's text run into
    /// it, which before fitted only the wrong card. None of them is taken for that card now.
    #[test]
    fn a_neighbours_letter_does_not_make_another_card() {
        let _en = English::on();
        let seams: [(&str, &str, &str); 70] = [
            ("potion", "eenzymesolution", "Digestive Enzyme Solution"),
            ("potion", "gointment", "Cleansing Ointment"),
            ("potion", "cointment", "Osteolytic Ointment"),
            ("potion", "rspinalsolution", "Gray Matter Spinal Solution"),
            ("potion", "wspinalsolution", "Hollow Spinal Solution"),
            ("potion", "ealsolution", "Leptomeningeal Solution"),
            ("potion", "ngtincture", "Bewitching Tincture"),
            ("potion", "agenicpowder", "Mutagenic Powder"),
            ("potion", "igenicpowder", "Vivigenic Powder"),
            ("potion", "dspinalsolution", "Petrified Spinal Solution"),
            ("potion", "wspinalsolution", "Hollow Spinal Solution"),
            ("potion", "ealsolution", "Leptomeningeal Solution"),
            ("potion", "dspinalsolution", "Petrified Spinal Solution"),
            ("potion", "rspinalsolution", "Gray Matter Spinal Solution"),
            ("potion", "ealsolution", "Leptomeningeal Solution"),
            ("potion", "sbilesolution", "Viscous Bile Solution"),
            ("potion", "eleechsolution", "Pure Leech Solution"),
            ("potion", "ghsolution", "Slough Solution"),
            ("potion", "compoundr", "Compound Revitalizing Pill"),
            ("potion", "cenzymesolution", "Probiotic Enzyme Solution"),
            ("potion", "agenicpowder", "Mutagenic Powder"),
            ("potion", "ogenicpowder", "Osteogenic Powder"),
            ("potion", "dleechsolution", "Compound Leech Solution"),
            ("potion", "ghsolution", "Slough Solution"),
            ("potion", "ogtincture", "Brain Fog Tincture"),
            ("potion", "igenicpowder", "Vivigenic Powder"),
            ("potion", "ogenicpowder", "Osteogenic Powder"),
            ("potion", "nbilesolution", "Green Bile Solution"),
            ("potion", "slenderlimbpowderb", "Slender-Limb Powder - Bone Sentry"),
            ("potion", "twinhormoneb", "Twin Hormone - Bone Sentry"),
            ("potion", "spinalsolutionae", "Spinal Solution - Aemberon"),
            ("potion", "anesthetictinctureae", "Anesthetic Tincture - Aemberon"),
            ("potion", "ebonesentry", "Twin Hormone - Bone Sentry"),
            ("potion", "slenderlimbpowderp", "Slender-Limb Powder - Pest"),
            ("potion", "rbonesentry", "Slender-Limb Powder - Bone Sentry"),
            ("potion", "twinhormonep", "Twin Hormone - Pest"),
            ("potion", "naemberon", "Spinal Solution - Aemberon"),
            ("potion", "anesthetictinctureag", "Anesthetic Tincture - Agito"),
            ("potion", "eaemberon", "Anesthetic Tincture - Aemberon"),
            ("potion", "spinalsolutionag", "Spinal Solution - Agito"),
            ("potion", "potionkitl", "Potion Kit (Large)"),
            ("potion", "compoundl", "Compound Leech Solution"),
            ("potion", "lholywater", "Ritual Holy Water: Flagellation"),
            ("potion", "xenopanaceab", "Xenopanacea: Brood"),
            ("potion", "xenopanaceao", "Xenopanacea: Osteotrophy"),
            ("potion", "dissectioniv", "Dissection I: Vermectomy"),
            ("potion", "dissectioniii", "Dissection III: Craniotomy"),
            ("potion", "dissectioniv", "Dissection I: Vermectomy"),
            ("potion", "dissectioniid", "Dissection II: Daemonectomy"),
            ("potion", "dissectioniv", "Dissection I: Vermectomy"),
            ("potion", "dissectioninfusioni", "Dissection Infusion: Intact Skeleton"),
            ("potion", "dissectioniv", "Dissection I: Vermectomy"),
            ("potion", "dissectioninfusionm", "Dissection Infusion: Morbid Corpse"),
            ("potion", "incubationelixirhollowm", "Incubation Elixir: Hollow Matriarch"),
            ("potion", "tbutterfly", "F3 Gen: Emergent Butterfly Swarm"),
            ("potion", "incubationelixirhollowc", "Incubation Elixir: Hollow Cocoon"),
            ("instrument", "aberrantst", "Aberrant Stinger"),
            ("instrument", "rrestraint", "Tanned Leather Restraint"),
            ("instrument", "aberrantsp", "Aberrant Sprout"),
            ("instrument", "erestraint", "Rawhide Restraint"),
            ("protocol", "cturetherapy", "Holistic Acupuncture Therapy"),
            ("protocol", "letherapy", "Yellow Bile Therapy"),
            ("protocol", "pturetherapy", "Cyst Rupture Therapy"),
            ("protocol", "letherapy", "Yellow Bile Therapy"),
            ("protocol", "gexperiment", "Epidermal Grafting Experiment"),
            ("protocol", "nexperiment", "Muscle Relaxant Injection Experiment"),
            ("protocol", "eexperiment", "Immunosuppressive Experiment"),
            ("protocol", "gexperiment", "Epidermal Grafting Experiment"),
            ("protocol", "eexperiment", "Immunosuppressive Experiment"),
            ("protocol", "nexperiment", "Muscle Relaxant Injection Experiment"),
        ];
        for (category, t, wrong) in seams {
            let category = match category {
                "potion" => Category::Potion,
                "instrument" => Category::Instrument,
                _ => Category::Protocol,
            };
            for clipped in [false, true] {
                assert_ne!(m(t, category, clipped).as_deref(), Some(wrong), "{t} ({clipped})");
            }
        }
    }

    /// The reviewer's 32 two-letter seams (2026-11-08): a name cut at one side with two letters of the neighbour's
    /// text run into it (`re` of `Tincture` + `Leech Solution`, `er` of `Powder` + `Spinal Solution`), which with
    /// [`MARGIN`] alone fitted only the wrong card. Each is read as the card shown or not at all.
    #[test]
    fn two_neighbour_letters_do_not_make_another_card() {
        let _en = English::on();
        let seams: [(&str, &str, &str, &str); 32] = [
            ("instrument", "aberrantspr", "Aberrant Stinger", "Aberrant Sprout"),
            ("potion", "compoundle", "Compound Revitalizing Pill", "Compound Leech Solution"),
            ("potion", "dissectioninfusionin", "Dissection Infusion: Morbid Corpse", "Dissection Infusion: Intact Skeleton"),
            ("potion", "erbonesentry", "Twin Hormone - Bone Sentry", "Slender-Limb Powder - Bone Sentry"),
            ("instrument", "errestraint", "Rawhide Restraint", "Tanned Leather Restraint"),
            ("potion", "erspinalsolution", "Hollow Spinal Solution", "Gray Matter Spinal Solution"),
            ("potion", "erspinalsolution", "Petrified Spinal Solution", "Gray Matter Spinal Solution"),
            ("potion", "f1genparasitepu", "F1 Gen: Parasite Egg", "F2 Gen: Parasite Pupae"),
            ("potion", "f1genverminpu", "F1 Gen: Vermin Egg Cluster", "F2 Gen: Vermin Pupal Cluster"),
            ("potion", "f2genparasiteeg", "F2 Gen: Parasite Pupae", "F1 Gen: Parasite Egg"),
            ("potion", "f2genvermineg", "F2 Gen: Vermin Pupal Cluster", "F1 Gen: Vermin Egg Cluster"),
            ("potion", "gealsolution", "Gray Matter Spinal Solution", "Leptomeningeal Solution"),
            ("potion", "gealsolution", "Hollow Spinal Solution", "Leptomeningeal Solution"),
            ("potion", "gealsolution", "Petrified Spinal Solution", "Leptomeningeal Solution"),
            ("potion", "ggcluster", "F2 Gen: Vermin Pupal Cluster", "F1 Gen: Vermin Egg Cluster"),
            ("potion", "hollowco", "Hollow Spinal Solution", "Incubation Elixir: Hollow Cocoon"),
            ("potion", "icenzymesolution", "Digestive Enzyme Solution", "Probiotic Enzyme Solution"),
            ("potion", "icointment", "Cleansing Ointment", "Osteolytic Ointment"),
            ("potion", "incubationelixirhollowco", "Incubation Elixir: Hollow Matriarch", "Incubation Elixir: Hollow Cocoon"),
            ("potion", "nebonesentry", "Slender-Limb Powder - Bone Sentry", "Twin Hormone - Bone Sentry"),
            ("potion", "ntbutterfly", "F3 Gen: Parasitic Butterfly", "F3 Gen: Emergent Butterfly Swarm"),
            ("potion", "onaemberon", "Anesthetic Tincture - Aemberon", "Spinal Solution - Aemberon"),
            ("potion", "onectomy", "Dissection I: Vermectomy", "Dissection II: Daemonectomy"),
            ("protocol", "onexperiment", "Epidermal Grafting Experiment", "Muscle Relaxant Injection Experiment"),
            ("protocol", "onexperiment", "Immunosuppressive Experiment", "Muscle Relaxant Injection Experiment"),
            ("potion", "reaemberon", "Spinal Solution - Aemberon", "Anesthetic Tincture - Aemberon"),
            ("potion", "releechsolution", "Compound Leech Solution", "Pure Leech Solution"),
            ("potion", "rmectomy", "Dissection II: Daemonectomy", "Dissection I: Vermectomy"),
            ("potion", "slenderlimbpowderpe", "Slender-Limb Powder - Bone Sentry", "Slender-Limb Powder - Pest"),
            ("potion", "spinalpo", "Spinal Solution - Aemberon", "Fresh Spinal Powder"),
            ("potion", "spinalpo", "Spinal Solution - Agito", "Fresh Spinal Powder"),
            ("potion", "twinhormonepe", "Twin Hormone - Bone Sentry", "Twin Hormone - Pest"),
        ];
        for (category, t, card, wrong) in seams {
            let category = match category {
                "potion" => Category::Potion,
                "instrument" => Category::Instrument,
                _ => Category::Protocol,
            };
            for clipped in [false, true] {
                let got = m(t, category, clipped);
                assert!(got.is_none() || got.as_deref() == Some(card), "{t} ({clipped}): {got:?}, not {wrong}");
            }
        }
        // Cut two letters past the part another card shares: not read where a neighbour's line could show the same
        // two letters (`Pest`, `Petrified`), read where none could (no line of a potion name starts `gi`).
        assert_eq!(m("Twin Hormone - Pe", Category::Potion, true), None);
        assert_eq!(m("Spinal Solution - Agi", Category::Potion, true).as_deref(), Some("Spinal Solution - Agito"));
        assert_eq!(m("OW Spinal Solution", Category::Potion, true), None, "`ow` of a neighbour's `Hollow`");
        // The generation's digit is dropped from the name read and from the names alike (`F1 Gen` is `fgen`).
        assert_eq!(m("F1 Gen: Parasite Egg", Category::Potion, false).as_deref(), Some("F1 Gen: Parasite Egg"));
        assert_eq!(m("F2 Gen: Vermin Pupal Cluster", Category::Potion, false).as_deref(), Some("F2 Gen: Vermin Pupal Cluster"));
        assert_eq!(m("Gen: Vermin Pupal", Category::Potion, true).as_deref(), Some("F2 Gen: Vermin Pupal Cluster"));
        assert!(candidates(Category::Potion).iter().all(|(_, z)| z.is_ascii() && !z.chars().any(|c| c.is_ascii_digit())));
    }

    /// The archetype suffix and potion kits: the suffix read, or the effect text printing that archetype alone; the
    /// kit's size and `Rare`.
    #[test]
    fn english_suffix_and_kit_checks() {
        let _en = English::on();
        let card = |en: &str| names().cards.iter().find(|c| c.en == en).unwrap();
        let pest = card("Twin Hormone - Pest");
        assert!(super::super::species_ok(pest, "Twin Hormone - Pest", ""));
        assert!(super::super::species_ok(pest, "Twin Hormone - Pe", "Select 1 group(s) of monsters, mutate them into Pests, and add 1 group(s) of monsters of the same"));
        assert!(!super::super::species_ok(pest, "Twin Hormone - Pe", "Select 1 group(s) of monsters, mutate them into Bone Sentries"));
        assert!(!super::super::species_ok(pest, "Twin Hormone - Pe", ""));
        assert!(super::super::species_ok(card("Twin Hormone - Bone Sentry"), "Twin HormOne - Bo Sentry", ""));
        assert!(super::super::species_ok(card("Spinal Solution - Agito"), "Spinal Solution - Agito", ""));
        assert!(super::super::species_ok(card("Slender-Limb Powder - Bone Sentry"), "Slender-Limb Powder -", "Add 1 group(s) of Bone Sentries and +73 to their Quantity"));
        let small = card("Potion Kit (Small)");
        assert!(super::super::kit_ok(small, "Potion Kit (Small)", ""));
        assert!(!super::super::kit_ok(small, "Rare Potion Kit (Small)", ""));
        assert!(super::super::kit_ok(card("Rare Potion Kit (Small)"), "wcle Potion Kit (Sma", "more likely to show rare potions, contains 3"));
    }

    /// The family alone, settled by the archetype the effect text prints.
    #[test]
    fn a_family_settled_by_the_effect() {
        let _en = English::on();
        let line = |t: &str| vec![Line { text: t.into(), rect: Rect::new(0, 0, 10, 10) }];
        let by = |name: &str, effect: &str| family_by_effect(&[line(name)], effect, Category::Potion).map(|i| names().cards[i].en.clone());
        assert_eq!(by("Spinal Solution -A", "Add 1 group(s) of Agitoes and +31 to their Activity").as_deref(), Some("Spinal Solution - Agito"));
        assert_eq!(by("Slender-Lirfib Powde", "Add 1 group(s) of Pests and +73 to their Quantity").as_deref(), Some("Slender-Limb Powder - Pest"));
        assert_eq!(by("Slender-Lirfib Powde", "Add 1 group(s) of"), None);
        assert_eq!(by("Slender-Lirfib Powde", "Pests and Bone Sentries"), None);
        assert_eq!(by("Mutagenic Powder", "Pests"), None, "no family");
        // A name read up to " -": the Spinal Solutions without a suffix are not it (2026-11-07_14-47-35 sample-0042).
        assert_eq!(by("Spinal Solution -", "Spinat5mutlon • Aemberon Add 1 group(s) o $ Aemberons and to their V Activit i").as_deref(), Some("Spinal Solution - Aemberon"));
        assert_eq!(by("Spinal Solution", "Add 1 group(s) of Aemberons"), None, "no dash, no archetype before the effect: Gray Matter and the rest stay in");
        // Look-alike letters folded, with the archetype said before the effect (the name's second line) or after a dash.
        assert_eq!(by("SpinatSöfution -", "SpinalSöIUtlon• Aemberon Add 1 group(s) of Aemberons and to their V Activi").as_deref(), Some("Spinal Solution - Aemberon"));
        assert_eq!(by("Spinal S6fution ron", "Aemberon Add 1 group(s) o $Aemberons and to their V Activit").as_deref(), Some("Spinal Solution - Aemberon"));
        assert_eq!(by("binatSöfution ron", "Aemberon d 1 group(s) of emberons and +31 their *Activity m").as_deref(), Some("Spinal Solution - Aemberon"));
        assert_eq!(by("SpinatSöfution", "Select 1 group(s) monsters and +2 their Activity. If they are not Aemberons"), None, "the archetype only inside the effect");
        drop(_en);
        assert_eq!(by("Spinal Solution -A", "Add 1 group(s) of Agitoes"), None, "the Traditional Chinese client");
    }

    /// Three protocols' type texts read as one line are cut into three, each placed by its share of the line; a line
    /// of one type text is kept.
    #[test]
    fn english_type_texts_read_as_one_line() {
        let l = Line { text: "RareSurgicaI Protocol MagicSurgical Prot NormalSurøiral".into(), rect: Rect::new(1080, 1280, 660, 30) };
        let parts = split_types(&l);
        assert_eq!(parts.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(), ["RareSurgicaI Protocol", "MagicSurgical Prot", "NormalSurøiral"]);
        let centres: Vec<i32> = parts.iter().map(|p| p.rect.x + p.rect.w / 2).collect();
        assert!(centres.windows(2).all(|w| w[1] - w[0] > 150), "{centres:?}");
        let ult = Line { text: "MagicPotion ULTPotion NormalPotion".into(), rect: Rect::new(1100, 1280, 600, 30) };
        assert_eq!(split_types(&ult).iter().map(|p| p.text.as_str()).collect::<Vec<_>>(), ["MagicPotion", "ULTPotion", "NormalPotion"]);
        let l = Line { text: "Surgical Instrument Surgical Instrume".into(), rect: Rect::new(1100, 1280, 600, 30) };
        assert_eq!(split_types(&l).len(), 2);
        let one = Line { text: "MagicSurgical Protocol".into(), rect: Rect::new(1300, 1280, 200, 30) };
        assert_eq!(split_types(&one), vec![one.clone()]);
        let _en = English::on();
        assert_eq!(super::super::card_anchors(std::slice::from_ref(&l)).len(), 2);
    }

    /// The English digit templates: every digit of every field (the reroll count shows 0 to 3 only), and the
    /// Traditional Chinese ones are kept.
    #[test]
    fn english_templates_cover_the_digits() {
        use super::super::super::templates_en as en;
        let chars = |ts: &[Template]| ts.iter().map(|t| t.ch).collect::<String>();
        for ts in [en::PANEL, en::REWARD, en::RESULT, en::ROUND] {
            assert_eq!(chars(ts), "0123456789");
        }
        assert_eq!(chars(en::LABEL), "0123456789x");
        assert_eq!(chars(en::SLASH), "/13");
        assert_eq!(chars(en::REROLL), "0123");
        assert!(std::ptr::eq(super::super::tpl(templates::LABEL), templates::LABEL));
        let _en = English::on();
        assert!(std::ptr::eq(super::super::tpl(templates::LABEL), en::LABEL));
        assert!(std::ptr::eq(super::super::tpl(templates::ROUND), en::ROUND));
    }
}

/// How the English digit templates ([`super::super::templates_en`]) were learned: run by hand with the sample games
/// at hand (`EN_DIRS`, `;` separated) and the output file (`EN_TEMPLATES_OUT`):
/// `cargo test --features en-vorax --lib vorax::read::read_en::learn -- --ignored --nocapture`.
/// The glyphs are cut as for the Traditional Chinese templates and labelled by Windows OCR of the same places (a tank
/// label only where its "a x b" multiplies out to the total read; every field from its own glyphs but the reward,
/// pooled with the panel's for want of a "9"; the reroll
/// digit, which the OCR cannot read alone, by the label templates, checked by eye on the 0 to 3 seen). The round's big
/// digits, which the OCR does not read whole, come from [`learn::round_templates`] instead, which replace the ones
/// learned.
#[cfg(test)]
mod learn {
    use super::*;
    use super::super::super::digits::{GH, GW};
    use std::collections::BTreeMap;

    struct Sample {
        field: &'static str,
        ch: char,
        feat: Vec<u8>,
        game: usize,
    }

    fn digits_of(s: &str) -> String {
        s.chars().filter(char::is_ascii_digit).collect()
    }

    fn label(out: &mut Vec<Sample>, field: &'static str, gs: &[Glyph], want: &str, game: usize) {
        if want.is_empty() || gs.len() != want.chars().count() {
            return;
        }
        out.extend(gs.iter().zip(want.chars()).map(|(g, ch)| Sample { field, ch, feat: g.feat.clone(), game }));
    }

    fn learn(samples: &[&Sample]) -> Vec<(char, usize, Vec<u8>)> {
        let mut by: BTreeMap<char, Vec<&Sample>> = BTreeMap::new();
        for s in samples {
            by.entry(s.ch).or_default().push(s);
        }
        by.into_iter().map(|(c, v)| (c, v.len(), (0..GW * GH).map(|k| (v.iter().map(|s| f32::from(s.feat[k])).sum::<f32>() / v.len() as f32).round() as u8).collect())).collect()
    }

    fn nearest(feat: &[u8], ts: &[(char, usize, Vec<u8>)]) -> char {
        ts.iter().map(|(c, _, t)| (*c, feat.iter().zip(t).map(|(&a, &b)| (f32::from(a) - f32::from(b)).abs()).sum::<f32>())).min_by(|a, b| a.1.total_cmp(&b.1)).map_or('?', |b| b.0)
    }

    fn frames() -> Vec<(usize, Rgba, String)> {
        let mut out = Vec::new();
        for (game, dir) in std::env::var("EN_DIRS").unwrap().split(';').filter(|d| !d.is_empty()).enumerate() {
            for l in std::fs::read_to_string(format!("{dir}/log.jsonl")).unwrap().lines() {
                let r: serde_json::Value = serde_json::from_str(l).unwrap();
                if r["kind"] == "vorax_sample" && (r["mode"] == "board" || r["mode"] == "result") {
                    let img = to_reference(&crate::reader::load_image_raw(std::path::Path::new(&format!("{dir}/{}", r["file"].as_str().unwrap()))).unwrap());
                    out.push((game, img, r["mode"].as_str().unwrap().to_string()));
                }
            }
        }
        out
    }

    /// The templates of every field, and the split-half cross-validation (by game) on stdout.
    #[test]
    #[ignore]
    fn learn_templates() {
        let mut samples: Vec<Sample> = Vec::new();
        let mut rerolls: Vec<Vec<u8>> = Vec::new();
        for (game, img, mode) in frames() {
            let text = Text { img: &img, frame: None };
            let ocr = |rect: Rect, k: f32| joined(&text.batch(&[(rect, k)])[0]);
            if mode == "result" {
                label(&mut samples, "RESULT", &text.glyphs(RESULT_VALUE, 20, Ink::Plain), &digits_of(&ocr(Rect::new(1130, 118, 300, 64), 2.0)), game);
                continue;
            }
            let round = ocr(Rect::new(100, 480, 180, 110), 2.0);
            if let Ok(v) = round.split('/').next().map(digits_of).unwrap_or_default().parse::<i32>()
                && round.contains("13")
                && (1..=13).contains(&v)
            {
                label(&mut samples, "SLASH", &text.glyphs(ROUND_OF, 25, Ink::Plain), "/13", game);
                let big: Vec<Glyph> = text.glyphs(ROUND, 30, Ink::Plain).into_iter().filter(|g| g.h() >= 30).collect();
                label(&mut samples, "ROUND", &big, &v.to_string(), game);
            }
            label(&mut samples, "PANEL", &text.glyphs(ULTIMATE, 15, Ink::Plain), &digits_of(&ocr(Rect::new(90, 636, 220, 44), 3.0)), game);
            label(&mut samples, "REWARD", &text.glyphs(REWARD, 14, Ink::Plain), &digits_of(&ocr(Rect::new(90, 736, 220, 44), 3.0)), game);
            if let [g] = &text.glyphs(REROLL, 12, Ink::Label).into_iter().filter(|g| (14..=18).contains(&g.h()) && (1625..=1690).contains(&g.x0)).collect::<Vec<_>>()[..] {
                rerolls.push(g.feat.clone());
            }
            for (i, &x) in TANK_X.iter().enumerate() {
                let vals = ocr(Rect::new(x - 110, 550, 240, 30), 3.0).to_lowercase().replace(['×', '*'], "x");
                let total = digits_of(&ocr(Rect::new(x - 110, 526, 240, 30), 3.0));
                let parts: Vec<String> = vals.split('x').map(digits_of).collect();
                let [a, b] = &parts[..] else { continue };
                let (Ok(an), Ok(bn)) = (a.parse::<u64>(), b.parse::<u64>()) else { continue };
                if total.ends_with(&(an * bn).to_string()) {
                    label(&mut samples, "LABEL", &text.glyphs(tank_total(i), 9, Ink::Label), &(an * bn).to_string(), game);
                    label(&mut samples, "LABEL", &text.glyphs(tank_values(i), 9, Ink::Label), &format!("{a}x{b}"), game);
                }
            }
        }
        let label_ts = learn(&samples.iter().filter(|s| s.field == "LABEL").collect::<Vec<_>>());
        samples.extend(rerolls.iter().map(|f| Sample { field: "REROLL", ch: nearest(f, &label_ts), feat: f.clone(), game: 0 }));
        let mut code = String::new();
        for f in ["LABEL", "PANEL", "REWARD", "RESULT", "ROUND", "SLASH", "REROLL"] {
            // Each field from its own glyphs: the round and result value once pooled the panel's digits as the same
            // face at other sizes, and came out blurred between the sizes (the round's "5" lost its top bar and missed
            // a lossless round 5; the result's "2" was nearest another digit on all 9 of its glyphs). The reward still
            // pools them: its own samples have no "9" (no 190k tier in the eight games), and it is only shown.
            let own: Vec<&Sample> = samples.iter().filter(|s| s.field == f).collect();
            let pool: Vec<&Sample> = if f == "REWARD" { samples.iter().filter(|s| s.field == f || s.field == "PANEL").collect() } else { own.clone() };
            let (mut ok, mut n) = (0, 0);
            for half in 0..2 {
                let ts = learn(&pool.iter().copied().filter(|s| s.field != f || s.game % 2 == half).collect::<Vec<_>>());
                for s in own.iter().filter(|s| s.game % 2 != half) {
                    n += 1;
                    ok += usize::from(nearest(&s.feat, &ts) == s.ch);
                }
            }
            println!("{f}: {} glyphs, cross-validation {ok}/{n}", own.len());
            code.push_str(&format!("\npub static {f}: &[Template] = &[\n"));
            for (c, k, feat) in learn(&pool) {
                code.push_str(&format!("    // '{c}' ({k})\n    Template {{ ch: '{c}', feat: &[{}] }},\n", feat.iter().map(u8::to_string).collect::<Vec<_>>().join(",")));
            }
            code.push_str("];\n");
        }
        std::fs::write(std::env::var("EN_TEMPLATES_OUT").unwrap(), code).unwrap();
    }

    /// [`super::super::super::templates_en::ROUND`] (see the module notes): the round's own big glyphs only, written to
    /// `EN_ROUND_OUT` as that static, to replace the one [`learn_templates`] writes: the OCR reads the big round digits
    /// whole on a third of the frames only (258 glyphs, no "7"). The ROUND shipped in 1.1.0 pooled the left panel's
    /// final activity digits with the round's, another size of another face, and its "5" came out with no top bar:
    /// the round 5 of a lossless capture (2026-11-08, `ascendant-cli capture`) was 42.1 from it, over the limit of 42,
    /// and read no round (the saved JPEGs 40 to 41). Each glyph is labelled by the nearest of the templates before
    /// (under 60 and at least 10 ahead of the next, as for the "7" before it; none of the 777 round glyphs of the
    /// samples was nearest another digit), the round read whole within 1 to 13.
    #[test]
    #[ignore]
    fn round_templates() {
        set_english_here(Some(true));
        let mut by: BTreeMap<char, Vec<Vec<u8>>> = BTreeMap::new();
        for (_, img, mode) in frames() {
            let text = Text { img: &img, frame: None };
            if mode != "board" || chars(&text.glyphs(ROUND_OF, 25, Ink::Plain), tpl(templates::SLASH)) != "/13" {
                continue;
            }
            let big: Vec<Glyph> = text.glyphs(ROUND, 30, Ink::Plain).into_iter().filter(|g| g.h() >= 40).collect();
            let label: Option<String> = big
                .iter()
                .map(|g| match digits::classify(g, tpl(templates::ROUND)) {
                    Some((c, d, second)) if d < 60.0 && second > d + 10.0 => Some(c),
                    _ => None,
                })
                .collect();
            let Some(label) = label.filter(|l| l.parse::<i32>().is_ok_and(|n| (1..=13).contains(&n))) else { continue };
            for (c, g) in label.chars().zip(&big) {
                by.entry(c).or_default().push(g.feat.clone());
            }
        }
        let mut code = String::from("pub static ROUND: &[Template] = &[\n");
        for (c, feats) in &by {
            let mean: Vec<u8> = (0..GW * GH).map(|k| (feats.iter().map(|f| f32::from(f[k])).sum::<f32>() / feats.len() as f32).round() as u8).collect();
            code.push_str(&format!("    // '{c}' ({})\n    Template {{ ch: '{c}', feat: &[{}] }},\n", feats.len(), mean.iter().map(u8::to_string).collect::<Vec<_>>().join(",")));
        }
        code.push_str("];\n");
        std::fs::write(std::env::var("EN_ROUND_OUT").unwrap(), code).unwrap();
    }

    /// [`super::super::super::templates_en_effects::EFFECT_WORDS`]: every sample of `EN_DIRS` read as the app reads it
    /// (the page scale of the samples, 0.668156, and the OCR cache emptied before each, as `vorax-read --list`), and the
    /// effect-text words ([`effect_words`]) of every card read by its whole name ([`How::Name`]) counted by card; the
    /// words in at least a tenth of a card's texts are written to `EN_EFFECTS_OUT`. Run with ASCENDANT_LANG=en.
    #[test]
    #[ignore]
    fn learn_effect_words() {
        set_english_here(Some(true));
        let ui = Some(crate::vorax::ui_scale::from_page_scale(0.668156));
        let mut by: BTreeMap<String, (u16, BTreeMap<String, u16>)> = BTreeMap::new();
        let mut frames = 0;
        for dir in std::env::var("EN_DIRS").unwrap().split(';').filter(|d| !d.is_empty()) {
            for l in std::fs::read_to_string(format!("{dir}/log.jsonl")).unwrap().lines() {
                let r: serde_json::Value = serde_json::from_str(l).unwrap();
                if r["kind"] != "vorax_sample" {
                    continue;
                }
                let img = crate::reader::load_image_raw(std::path::Path::new(&format!("{dir}/{}", r["file"].as_str().unwrap()))).unwrap();
                crate::reader::ocr::clear_cache();
                let reading = read_scaled(&to_reference(&img), Some(&img), ui);
                frames += 1;
                for c in reading.cards.iter().filter(|c| c.how == Some(How::Name) && !c.raw_effect.is_empty()) {
                    let Some(en) = &c.en else { continue };
                    let entry = by.entry(en.clone()).or_default();
                    entry.0 += 1;
                    for w in effect_words(&c.raw_name, &c.raw_effect, &c.raw_type) {
                        *entry.1.entry(w).or_default() += 1;
                    }
                }
            }
        }
        let mut code = [
            "//! The English client's effect-text words by card, for telling apart the cards a cut name leaves open",
            "//! ([`super::read::read_en`]'s `by_effect_words`). Generated by `read_en::learn::learn_effect_words` from the English",
            "//! sample games; feature `en-vorax`, on by default since 1.1.0.",
            "",
            "/// (word, texts holding it) for every word in at least a tenth of a card's effect texts.",
            "pub(crate) type Words = &'static [(&'static str, u16)];",
            "",
            "/// (card's English name, effect texts counted, its words).",
            "pub(crate) static EFFECT_WORDS: &[(&str, u16, Words)] = &[",
        ]
        .join("\n");
        code.push('\n');
        for (en, (n, words)) in &by {
            let kept: Vec<String> = words.iter().filter(|&(_, &c)| c * 10 >= *n).map(|(w, c)| format!("({w:?}, {c})")).collect();
            code.push_str(&format!("    ({en:?}, {n}, &[{}]),\n", kept.join(", ")));
        }
        code.push_str("];\n");
        std::fs::write(std::env::var("EN_EFFECTS_OUT").unwrap(), code).unwrap();
        println!("{frames} frames, {} cards, {} texts", by.len(), by.values().map(|v| v.0 as usize).sum::<usize>());
    }
}
