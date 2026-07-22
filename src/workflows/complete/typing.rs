//! Human-like typing-schedule generator.
//!
//! Given the netlist text and a target total duration, it pre-generates a
//! deterministic (seeded) list of keystroke/pause events whose delays sum to
//! approximately the target. The realism model:
//! - Per-keystroke RHYTHM: an AR(1) "tempo" state makes fast/slow keys cluster,
//!   a log-normal jitter gives a right-skewed tail, digraph biomechanics
//!   (hand-alternation rolls vs same-finger reaches) and a per-WORD speed make
//!   some words fly and symbol/number tokens crawl, a shift-reach penalty,
//!   plus a mild fatigue drift with recovery after breaks.
//! - THINKING in three tiers: in-rhythm micro-hesitations, a pause on the SPACE
//!   before a "hard" next token (the user-requested mid-line think) and rare
//!   in-word freezes, and — on the elastic side — line/block "thinks" weighted
//!   by the next line's complexity (these absorb most of the target duration).
//! - TYPOS: a five-class taxonomy (transpose / substitute / double / drop /
//!   adjacent-double) with a notice latency (the error is caught a few correct
//!   chars later) and an exact backspace-burst + cautious retype, so the final
//!   text is always reconstructed perfectly.
//! - SAVES: `<Esc>:w<CR>` expanded into seeded, human-paced keystrokes (a "should
//!   I save" beat, the mode switch, an occasional "is this right?" glance) and
//!   inserted only at column-0 boundaries, at most `save_max` apart.
//!
//! Keystroke + typo + save time is fixed ("motion"); only the line/block thinks
//! are elastic, so the remaining target time is distributed across them by
//! weight and the total lands near the target without speeding up the typing.

use std::time::Duration;

use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};

/// One step of the macro.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    /// Enter insert mode (`i`). Also the prelude action.
    EnterInsert,
    /// Type a printable character into the buffer.
    Char(char),
    /// Newline (Enter). The ONLY action that advances the line counter.
    Enter,
    /// Backspace (typo corrections).
    Backspace,
    /// `<Esc>` — leave insert mode (emitted at column 0, before a save or a
    /// deferred typo correction).
    Escape,
    /// A normal/command-mode key: relative motions (`k`/`j`/`h`/`l`/`0`/`$`),
    /// `r` + its replacement char, `a`/`i`, `:`, `w`. Sent verbatim; the editor
    /// (and the [`super::nvim`] model) interpret it by mode.
    Key(char),
    /// The command-mode `<CR>` that runs `:w` (the save-accounting hook).
    CmdEnter,
}

/// One scheduled event: an action and the pause to wait *after* it.
#[derive(Clone, Copy, Debug)]
pub struct Event {
    pub action: Action,
    pub delay_after: Duration,
}

/// A fully pre-generated typing plan.
pub struct Plan {
    pub events: Vec<Event>,
    pub total: Duration,
    pub keystrokes: usize,
    pub lines: usize,
    pub saves: usize,
}

/// Tunable knobs for the typing model.
#[derive(Clone, Copy, Debug)]
pub struct TypingConfig {
    /// Median ms per keypress (before the multiplicative rhythm terms).
    pub base_keypress_ms: f64,
    /// Extra ms at an ordinary word gap (space hand-reset).
    pub word_gap_ms: f64,
    /// Probability a line gets a long "big think" before the next one.
    pub big_think_chance: f64,
    /// Probability of a SHORT-context typo (wrong keys + notice-latency +
    /// in-line correction) per eligible char.
    pub typo_chance: f64,
    /// Probability of a LONG-context typo (a length-preserving error left in
    /// place and corrected later, at the end of the section) per eligible char.
    pub long_typo_chance: f64,
    /// Max seconds between saves (the spec's hard 5-minute cap).
    pub save_max_secs: f64,
    /// Fractional slowdown by the end of the run (fatigue). 0.2 = 20% slower.
    pub fatigue: f64,
    /// Lag-1 autocorrelation of the AR(1) tempo state (key-speed clustering).
    pub tempo_phi: f64,
    /// Per-step innovation std (log space) of the tempo walk.
    pub tempo_sigma: f64,
    /// Per-key log-normal multiplicative spread (the right-skewed tail).
    pub jitter_sigma: f64,
    /// Base probability a space gets a between-word planning pause.
    pub pause_on_space_chance: f64,
    /// Hard lower clamp per keystroke (no inhuman sub-floor keys).
    pub interkey_floor_ms: f64,
}

impl Default for TypingConfig {
    fn default() -> Self {
        TypingConfig {
            base_keypress_ms: 96.0,
            word_gap_ms: 68.0,
            big_think_chance: 0.06,
            // 0 by default so tests are deterministic-clean; the workflow turns
            // these on when the user enables typo simulation.
            typo_chance: 0.0,
            long_typo_chance: 0.0,
            save_max_secs: 300.0,
            fatigue: 0.2,
            tempo_phi: 0.78,
            // More variance in the autocorrelated tempo, less in the i.i.d.
            // jitter, so realized key-speed autocorrelation matches humans.
            tempo_sigma: 0.14,
            jitter_sigma: 0.22,
            pause_on_space_chance: 0.10,
            interkey_floor_ms: 38.0,
        }
    }
}

fn ms(v: f64) -> Duration {
    Duration::from_secs_f64((v.max(0.0)) / 1000.0)
}

/// Add `extra_ms` to the most recently pushed event's trailing delay.
fn add_to_last(events: &mut [Event], extra_ms: f64) {
    if let Some(last) = events.last_mut() {
        last.delay_after += ms(extra_ms);
    }
}

/// How "hard to type / think about" a line is, scaling the pause before it.
fn complexity(line: &str) -> f64 {
    let t = line.trim();
    if t.is_empty() {
        return 0.0;
    }
    if t.starts_with('*') {
        return 0.4; // comment — easy
    }
    let tokens = t.split_whitespace().count() as f64;
    let has_expr = t.contains('=')
        || t.contains('{')
        || t.contains('(')
        || t.chars().filter(|c| "*/+^".contains(*c)).count() > 1;
    let directive = t.starts_with('.');
    let mut c = 0.6 + tokens * 0.15 + (t.len() as f64) * 0.008;
    if has_expr {
        c += 1.2;
    }
    if directive {
        c += 0.6;
    }
    c.min(4.0)
}

/// Carried per-keystroke rhythm state for the whole run.
struct RhythmState {
    tempo_log: f64,
    prev_shift: bool,
    prev_char: Option<char>,
    /// Keystrokes since the last real break (drives fatigue recovery).
    since_break: usize,
}

/// Deterministic standard normal via Box-Muller (no unwrap; `u1` floored so
/// `ln` is finite).
fn gauss(rng: &mut StdRng) -> f64 {
    let u1: f64 = rng.random_range(1e-9..1.0);
    let u2: f64 = rng.random_range(0.0..1.0);
    (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
}

/// Static QWERTY position: (hand 0=L/1=R, finger 0=index..3=pinky, row).
fn key_pos(c: char) -> Option<(i32, i32, i32)> {
    match c {
        'q' => Some((0, 3, 0)),
        'w' => Some((0, 2, 0)),
        'e' => Some((0, 1, 0)),
        'r' => Some((0, 0, 0)),
        't' => Some((0, 0, 0)),
        'y' => Some((1, 0, 0)),
        'u' => Some((1, 0, 0)),
        'i' => Some((1, 1, 0)),
        'o' => Some((1, 2, 0)),
        'p' => Some((1, 3, 0)),
        'a' => Some((0, 3, 1)),
        's' => Some((0, 2, 1)),
        'd' => Some((0, 1, 1)),
        'f' => Some((0, 0, 1)),
        'g' => Some((0, 0, 1)),
        'h' => Some((1, 0, 1)),
        'j' => Some((1, 0, 1)),
        'k' => Some((1, 1, 1)),
        'l' => Some((1, 2, 1)),
        'z' => Some((0, 3, 2)),
        'x' => Some((0, 2, 2)),
        'c' => Some((0, 1, 2)),
        'v' => Some((0, 0, 2)),
        'b' => Some((0, 0, 2)),
        'n' => Some((1, 0, 2)),
        'm' => Some((1, 0, 2)),
        '1' => Some((0, 3, -1)),
        '2' => Some((0, 2, -1)),
        '3' => Some((0, 1, -1)),
        '4' => Some((0, 0, -1)),
        '5' => Some((0, 0, -1)),
        '6' => Some((1, 0, -1)),
        '7' => Some((1, 0, -1)),
        '8' => Some((1, 1, -1)),
        '9' => Some((1, 2, -1)),
        '0' => Some((1, 3, -1)),
        _ => None,
    }
}

/// Whether typing `ch` needs a shift press (rough US layout).
fn needs_shift(ch: char) -> bool {
    ch.is_ascii_uppercase() || "~!@#$%^&*()_+{}|:\"<>?".contains(ch)
}

/// Biomechanical digraph multiplier for the `prev`→`ch` transition.
fn digraph(prev: Option<char>, ch: char) -> f64 {
    let p = match prev {
        Some(c) if c != ' ' => c,
        _ => return 1.0,
    };
    let a = p.to_ascii_lowercase();
    let b = ch.to_ascii_lowercase();
    if a == b && b.is_ascii_alphabetic() {
        return 1.35; // double letter
    }
    let cat = |c: char| -> u8 {
        if c.is_ascii_digit() {
            1
        } else if c.is_ascii_alphabetic() {
            0
        } else {
            2
        }
    };
    let (ca, cb) = (cat(a), cat(b));
    if (ca == 0 && cb == 1)
        || (ca == 1 && cb == 0)
        || (ca == 1 && cb == 2)
        || (ca == 2 && cb == 1)
    {
        return 1.45; // letter<->digit / digit<->symbol reach
    }
    if "{}()^=".contains(b) || "{}()^=".contains(a) {
        return 1.20;
    }
    match (key_pos(a), key_pos(b)) {
        (Some((ha, fa, _)), Some((hb, fb, _))) => {
            if ha == hb && fa == fb {
                1.30 // same finger, different key
            } else if ha != hb {
                0.80 // hand alternation (fast roll)
            } else {
                1.0
            }
        }
        _ => 1.0,
    }
}

/// Per-WORD speed multiplier (log-uniform 0.72..1.30 + a content nudge),
/// sampled once when entering the word at `i`.
fn sample_word_speed(chars: &[char], i: usize, rng: &mut StdRng) -> f64 {
    let mut j = i;
    while j < chars.len() && chars.get(j) != Some(&' ') {
        j += 1;
    }
    let t = rng.random_range(0.0..1.0);
    let (lo, hi) = (0.72_f64.ln(), 1.30_f64.ln());
    let mut s = (lo + (hi - lo) * t).exp();
    let mut all_lower_alpha = true;
    let mut has_hard = false;
    let mut len = 0usize;
    let mut k = i;
    while k < j {
        if let Some(&c) = chars.get(k) {
            if !c.is_ascii_lowercase() {
                all_lower_alpha = false;
            }
            if c.is_ascii_digit() || "{}()*=^".contains(c) {
                has_hard = true;
            }
            len += 1;
        }
        k += 1;
    }
    if all_lower_alpha && len <= 4 {
        s *= 0.92;
    }
    if has_hard {
        s *= 1.12;
    }
    s
}

/// Whether the next whitespace token at/after `from` is "hard" (warrants a
/// planning pause on the preceding space).
fn token_is_hard(chars: &[char], from: usize) -> bool {
    let mut s = from;
    while s < chars.len() && chars.get(s) == Some(&' ') {
        s += 1;
    }
    let mut e = s;
    while e < chars.len() && chars.get(e) != Some(&' ') {
        e += 1;
    }
    let len = e.saturating_sub(s);
    if len == 0 {
        return false;
    }
    let mut hard = len > 6;
    let mut prev_digit = false;
    let mut k = s;
    while k < e {
        if let Some(&c) = chars.get(k) {
            if c == '{' || c == '(' || c == '=' {
                hard = true;
            }
            if k + 1 == e && prev_digit && "fpnumkgt".contains(c) {
                hard = true; // engineering unit suffix, e.g. 10k / 1u
            }
            prev_digit = c.is_ascii_digit();
        }
        k += 1;
    }
    hard
}

/// Delay for one committed keystroke `ch`, updating the rhythm state. Used for
/// normal chars and for every keystroke inside a typo correction.
fn keypress_delay(
    ch: char,
    word_speed: f64,
    fatigue: f64,
    cfg: &TypingConfig,
    st: &mut RhythmState,
    rng: &mut StdRng,
) -> f64 {
    st.tempo_log = cfg.tempo_phi * st.tempo_log + cfg.tempo_sigma * gauss(rng);
    let tempo = st.tempo_log.exp();
    let jitter = (cfg.jitter_sigma * gauss(rng)).exp().clamp(0.55, 2.2);
    let dg = if ch == ' ' { 1.0 } else { digraph(st.prev_char, ch) };
    let mut d = cfg.base_keypress_ms * fatigue * word_speed * tempo * dg * jitter;
    let sh = needs_shift(ch);
    if sh && !st.prev_shift {
        d += rng.random_range(28.0..60.0);
    }
    if ch == ' ' {
        // A space is a quick hand-reset, but RIDE the rhythm (tempo/jitter/
        // fatigue) rather than overwriting it with a flat uniform draw.
        d = d * 0.7 + cfg.word_gap_ms * rng.random_range(0.4..1.2);
    } else if rng.random_bool(0.025) {
        d += rng.random_range(110.0..420.0); // in-word micro-stumble
    }
    // Soft floor: clamp to a slightly randomized minimum so the low tail keeps
    // continuous density instead of piling up at one exact integer-ms value.
    let floor = cfg.interkey_floor_ms + rng.random_range(0.0..14.0);
    d = d.max(floor);
    st.prev_shift = sh;
    st.prev_char = Some(ch);
    st.since_break += 1;
    d
}

/// Emit a typo at line index `i`: wrong keys, a notice latency, a backspace
/// burst, then a cautious retype. Returns the index of the next not-yet-committed
/// intended char. The buffer after the block always equals the intended text.
#[allow(clippy::too_many_arguments)]
fn emit_typo(
    events: &mut Vec<Event>,
    fixed_ms: &mut f64,
    keystrokes: &mut usize,
    chars: &[char],
    i: usize,
    word_speed: f64,
    fatigue: f64,
    cfg: &TypingConfig,
    st: &mut RhythmState,
    rng: &mut StdRng,
) -> usize {
    let s_len = chars.len().saturating_sub(i);
    let s0 = chars.get(i).copied();
    let s1 = chars.get(i + 1).copied();
    let c0 = s0.unwrap_or(' ');
    let c1 = s1.unwrap_or(' ');

    // Eligible kinds (0=transpose 1=sub 2=double 3=drop 4=adj-double) + weights.
    let mut cands: Vec<(u8, f64)> = Vec::new();
    if s_len >= 2 && s0.is_some() && s0 != s1 {
        cands.push((0, 0.32));
    }
    cands.push((1, 0.30));
    cands.push((2, 0.16));
    if s_len >= 2 {
        cands.push((3, 0.12));
    }
    cands.push((4, 0.10));
    let total_w: f64 = cands.iter().map(|(_, w)| *w).sum();
    let mut pick = rng.random_range(0.0..total_w.max(1e-9));
    let mut kind = 1u8;
    for (k, w) in &cands {
        if pick < *w {
            kind = *k;
            break;
        }
        pick -= *w;
    }

    // Garble keystrokes + `m` = intended chars the garble represents.
    let (typed, m): (Vec<char>, usize) = match kind {
        0 => (vec![c1, c0], 2),                    // transpose S0,S1
        2 => (vec![c0, c0], 1),                    // doubled
        3 => (Vec::new(), 1),                      // dropped S0
        4 => (vec![c0, typo_char(c0, rng)], 1),    // S0 + stray neighbor
        _ => (vec![typo_char(c0, rng)], 1),        // substitution
    };

    // Notice latency: correct chars typed after the error before running back.
    let u = rng.random_range(0.0..1.0);
    let mut lat = if u < 0.35 {
        0
    } else if u < 0.63 {
        1
    } else if u < 0.81 {
        2
    } else if u < 0.91 {
        3
    } else if u < 0.97 {
        4
    } else {
        5
    };
    let max_lat = s_len.saturating_sub(m);
    if lat > max_lat {
        lat = max_lat;
    }
    if kind == 3 && lat == 0 && max_lat >= 1 {
        lat = 1; // a dropped char must be visible to be noticed
    }

    // 1) garble keystrokes
    for &c in &typed {
        let d = keypress_delay(c, word_speed, fatigue, cfg, st, rng);
        events.push(Event {
            action: Action::Char(c),
            delay_after: ms(d),
        });
        *fixed_ms += d;
        *keystrokes += 1;
    }
    // 2) latency chars (the real intended chars S[m..m+lat], typed unawares)
    for k in 0..lat {
        if let Some(&c) = chars.get(i + m + k) {
            let d = keypress_delay(c, word_speed, fatigue, cfg, st, rng);
            events.push(Event {
                action: Action::Char(c),
                delay_after: ms(d),
            });
            *fixed_ms += d;
            *keystrokes += 1;
        }
    }
    let on_screen = typed.len() + lat;
    // 3) notice pause on the last emitted char
    let np = rng.random_range(250.0..700.0);
    add_to_last(events, np);
    *fixed_ms += np;
    // 4) backspace burst (run all the way back to the error site)
    for _ in 0..on_screen {
        let d = rng.random_range(60.0..110.0);
        events.push(Event {
            action: Action::Backspace,
            delay_after: ms(d),
        });
        *fixed_ms += d;
        *keystrokes += 1;
    }
    // 5) re-orient pause on the last backspace
    let rp = rng.random_range(180.0..520.0);
    add_to_last(events, rp);
    *fixed_ms += rp;
    // 6) retype the exact intended slice S[0..m+lat] cautiously
    st.prev_char = None; // digraph context lost after the run-back
    st.prev_shift = false;
    let retype = m + lat;
    for k in 0..retype {
        if let Some(&c) = chars.get(i + k) {
            let d = keypress_delay(c, word_speed, fatigue, cfg, st, rng) * 1.15;
            events.push(Event {
                action: Action::Char(c),
                delay_after: ms(d),
            });
            *fixed_ms += d;
            *keystrokes += 1;
        }
    }
    i + m + lat
}

/// Minimum per-press delay when a motion key is "held" (≈ OS key-repeat rate).
const KEY_REPEAT_FLOOR_MS: f64 = 26.0;

/// Per-press delay scale within a held motion run: the first press is a full
/// deliberate tap, then as the key is "held" over a long run the delay decays
/// toward the key-repeat rate. Keeps short hops tap-like and long scrolls fast.
fn repeat_scale(i: usize) -> f64 {
    (1.0 - 0.13 * i as f64).max(0.30)
}

/// Humanized delay for a navigation / command / correction keystroke. Rides the
/// SAME AR(1) tempo + log-normal jitter + digraph + shift-reach as ordinary
/// typing (so vim keystrokes feel as human as insertion), minus the word-speed
/// and fatigue terms. `ch` is the printable key, or None for a non-printable
/// such as `<Esc>`.
fn motion_delay(
    ch: Option<char>,
    cfg: &TypingConfig,
    st: &mut RhythmState,
    rng: &mut StdRng,
) -> f64 {
    st.tempo_log = cfg.tempo_phi * st.tempo_log + cfg.tempo_sigma * gauss(rng);
    let tempo = st.tempo_log.exp();
    let jitter = (cfg.jitter_sigma * gauss(rng)).exp().clamp(0.55, 2.2);
    let dg = match (st.prev_char, ch) {
        (Some(p), Some(c)) if p != ' ' => digraph(Some(p), c),
        _ => 1.0,
    };
    let mut d = cfg.base_keypress_ms * tempo * dg * jitter;
    match ch {
        Some(c) => {
            let sh = needs_shift(c);
            if sh && !st.prev_shift {
                d += rng.random_range(28.0..60.0);
            }
            st.prev_shift = sh;
            st.prev_char = Some(c);
        }
        None => {
            st.prev_shift = false;
            st.prev_char = None;
        }
    }
    st.since_break += 1;
    let floor = cfg.interkey_floor_ms + rng.random_range(0.0..14.0);
    d.max(floor)
}

/// Push the human-paced `<Esc>:w<CR>i` save group onto `out`. The keystrokes get
/// the same humanized per-key delays as typing; the pre-save beat and the
/// occasional "is this right?" glance are deliberate thinking pauses.
fn push_save_group(out: &mut Vec<Event>, cfg: &TypingConfig, st: &mut RhythmState, rng: &mut StdRng) {
    add_to_last(out, rng.random_range(220.0..700.0)); // "I should save" beat
    let d = motion_delay(None, cfg, st, rng);
    out.push(Event { action: Action::Escape, delay_after: ms(d) });
    let d = motion_delay(Some(':'), cfg, st, rng);
    out.push(Event { action: Action::Key(':'), delay_after: ms(d) });
    let d = motion_delay(Some('w'), cfg, st, rng);
    out.push(Event { action: Action::Key('w'), delay_after: ms(d) });
    let commit = if rng.random_bool(0.25) {
        rng.random_range(300.0..900.0) // occasional "is this right?" glance
    } else {
        rng.random_range(90.0..220.0)
    };
    out.push(Event { action: Action::CmdEnter, delay_after: ms(commit) });
    let d = motion_delay(Some('i'), cfg, st, rng);
    out.push(Event { action: Action::EnterInsert, delay_after: ms(d) });
}

/// A long-context typo that has been typed into the buffer and is awaiting a
/// deferred, in-place (length-preserving) correction.
struct PendingTypo {
    row: usize,
    col: usize,
    /// The CORRECT characters that should replace the wrong ones at `(row,col)`.
    correct: Vec<char>,
}

/// Push one action with a fixed (motion) delay, updating the running totals.
fn push_act(
    events: &mut Vec<Event>,
    fixed_ms: &mut f64,
    keystrokes: &mut usize,
    action: Action,
    delay_ms: f64,
) {
    events.push(Event {
        action,
        delay_after: ms(delay_ms),
    });
    *fixed_ms += delay_ms;
    *keystrokes += 1;
}

/// A plausible WRONG but same-length bracket/quote for `c` (wrong type), or None.
fn bracket_swap(c: char) -> Option<char> {
    match c {
        '(' => Some(')'),
        ')' => Some('('),
        '{' => Some('('),
        '}' => Some(')'),
        '[' => Some('('),
        ']' => Some(')'),
        '"' => Some('\''),
        '\'' => Some('"'),
        _ => None,
    }
}

/// Garble a whole word at `i` (a word start): replace 2-3 of its letters with
/// neighbor keys, keeping the SAME length, so it reads like a mistyped word that
/// gets re-typed during the deferred correction. Returns `(wrong, correct)` or
/// None if `i` isn't a 3-8 char alphanumeric word start.
fn word_garble(chars: &[char], i: usize, rng: &mut StdRng) -> Option<(Vec<char>, Vec<char>)> {
    let mut end = i;
    while end < chars.len() && chars.get(end) != Some(&' ') {
        end += 1;
    }
    let correct: Vec<char> = chars.get(i..end)?.to_vec();
    let len = correct.len();
    if !(3..=8).contains(&len) || !correct.iter().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    let want = rng.random_range(2..=(3.min(len - 1)).max(2));
    let mut wrong = correct.clone();
    let mut changed = 0usize;
    for idx in 0..len {
        if changed >= want {
            break;
        }
        let need = want - changed;
        let remaining = len - idx;
        if rng.random_range(0.0..1.0) < need as f64 / remaining as f64
            && let Some(&orig) = correct.get(idx)
        {
            let g = typo_char(orig, rng);
            if g != orig
                && let Some(slot) = wrong.get_mut(idx)
            {
                *slot = g;
                changed += 1;
            }
        }
    }
    if changed == 0 || wrong == correct {
        return None;
    }
    Some((wrong, correct))
}

/// Choose a length-preserving long-context typo at char index `i`: a wrong
/// bracket/quote, a neighbor-key letter substitution, an adjacent transposition,
/// or a whole-word garble (re-typed in full during the deferred correction).
/// Returns `(wrong_chars, correct_chars)` of equal length, or None.
fn long_typo_at(chars: &[char], i: usize, rng: &mut StdRng) -> Option<(Vec<char>, Vec<char>)> {
    let c0 = *chars.get(i)?;
    let c1 = chars.get(i + 1).copied();
    let at_word_start = i > 0 && chars.get(i - 1) == Some(&' ');
    // (kind, weight): 0 = bracket/quote swap, 1 = neighbor substitution,
    // 2 = adjacent transposition, 3 = whole-word garble. Quotes/parens are
    // weighted up per the spec.
    let mut cands: Vec<(u8, f64)> = Vec::new();
    if bracket_swap(c0).is_some() {
        cands.push((0, 0.40));
    }
    if c0.is_ascii_alphabetic() {
        cands.push((1, 0.30));
    }
    if let Some(n) = c1
        && c0.is_ascii_alphanumeric()
        && n.is_ascii_alphanumeric()
        && c0 != n
    {
        cands.push((2, 0.18));
    }
    if at_word_start {
        cands.push((3, 0.28));
    }
    let total: f64 = cands.iter().map(|(_, w)| *w).sum();
    if total <= 0.0 {
        return None;
    }
    let mut pick = rng.random_range(0.0..total);
    let mut kind = cands.first().map(|c| c.0).unwrap_or(1);
    for (k, w) in &cands {
        if pick < *w {
            kind = *k;
            break;
        }
        pick -= *w;
    }
    match kind {
        0 => bracket_swap(c0).map(|w| (vec![w], vec![c0])),
        2 => c1.map(|n| (vec![n, c0], vec![c0, n])),
        3 => word_garble(chars, i, rng),
        _ => {
            let w = typo_char(c0, rng);
            if w == c0 { None } else { Some((vec![w], vec![c0])) }
        }
    }
}

/// Emit the keystrokes that defer-correct a pending length-preserving typo the
/// way a human navigates: `<Esc>`, scroll UP with `k` to the typo's line, over
/// to the column with `l`, fix in place with `r{char}` (stepping right for a
/// multi-char fix), then scroll back DOWN with `j` to the bottom and resume
/// insert at end-of-buffer (`$ a`). On entry the cursor is in insert mode at
/// column 0 of the current last line (`cur_row`); on exit it is back in insert
/// mode at end-of-buffer. Every keystroke gets a humanized (rhythm) delay; the
/// "notice", "spot", and "settle" beats are deliberate thinking pauses.
#[allow(clippy::too_many_arguments)]
fn emit_correction(
    events: &mut Vec<Event>,
    fixed_ms: &mut f64,
    keystrokes: &mut usize,
    p: &PendingTypo,
    cur_row: usize,
    cfg: &TypingConfig,
    st: &mut RhythmState,
    rng: &mut StdRng,
) {
    let up = cur_row.saturating_sub(p.row);
    // A single keystroke at full (deliberate) humanized speed.
    let key = |events: &mut Vec<Event>,
               fixed_ms: &mut f64,
               keystrokes: &mut usize,
               st: &mut RhythmState,
               rng: &mut StdRng,
               action: Action,
               ch: Option<char>| {
        let d = motion_delay(ch, cfg, st, rng);
        push_act(events, fixed_ms, keystrokes, action, d);
    };
    // A run of `count` identical motion keys, accelerating like a held key so a
    // long scroll is fast (key-repeat) while a short hop stays tap-like.
    let run = |events: &mut Vec<Event>,
               fixed_ms: &mut f64,
               keystrokes: &mut usize,
               st: &mut RhythmState,
               rng: &mut StdRng,
               ch: char,
               count: usize| {
        for i in 0..count {
            let d = (motion_delay(Some(ch), cfg, st, rng) * repeat_scale(i)).max(KEY_REPEAT_FLOOR_MS);
            push_act(events, fixed_ms, keystrokes, Action::Key(ch), d);
        }
    };

    // Notice the earlier slip (a beat on the preceding event), then <Esc>.
    add_to_last(events, rng.random_range(450.0..1400.0));
    key(events, fixed_ms, keystrokes, st, rng, Action::Escape, None);
    // Scroll up to the error line (held key for long runs). Occasionally
    // overshoot by a line and correct back down, the way a human does.
    let overshoot = if up > 1 && p.row >= 1 && rng.random_bool(0.3) { 1 } else { 0 };
    run(events, fixed_ms, keystrokes, st, rng, 'k', up + overshoot);
    if overshoot > 0 {
        add_to_last(events, rng.random_range(120.0..380.0)); // "went too far"
        run(events, fixed_ms, keystrokes, st, rng, 'j', overshoot);
    }
    // A beat to spot the slip on the line, then go to its start and over.
    add_to_last(events, rng.random_range(200.0..650.0));
    key(events, fixed_ms, keystrokes, st, rng, Action::Key('0'), Some('0'));
    run(events, fixed_ms, keystrokes, st, rng, 'l', p.col);
    // Fix in place with `r{char}`, stepping right between chars (same length).
    let n = p.correct.len();
    for (j, &c) in p.correct.iter().enumerate() {
        key(events, fixed_ms, keystrokes, st, rng, Action::Key('r'), Some('r'));
        key(events, fixed_ms, keystrokes, st, rng, Action::Key(c), Some(c));
        if j + 1 < n {
            key(events, fixed_ms, keystrokes, st, rng, Action::Key('l'), Some('l'));
        }
    }
    // A beat, then scroll back down to the bottom and resume forward typing.
    add_to_last(events, rng.random_range(150.0..450.0));
    run(events, fixed_ms, keystrokes, st, rng, 'j', up);
    key(events, fixed_ms, keystrokes, st, rng, Action::Key('$'), Some('$'));
    key(events, fixed_ms, keystrokes, st, rng, Action::Key('a'), Some('a'));
    add_to_last(events, rng.random_range(250.0..700.0)); // settle back in
}

/// Generate the macro for `text` aimed at `target` total duration.
pub fn generate(text: &str, target: Duration, cfg: &TypingConfig, seed: u64) -> Plan {
    let mut rng = StdRng::seed_from_u64(seed);
    let lines: Vec<&str> = text.lines().collect();
    let total_lines = lines.len().max(1);

    let mut events: Vec<Event> = Vec::new();
    // (index of an elastic pause, distribution weight, per-pause floor ms).
    let mut elastic: Vec<(usize, f64, f64)> = Vec::new();
    let mut fixed_ms = 0.0_f64;
    let mut keystrokes = 0usize;
    let mut st = RhythmState {
        tempo_log: 0.0,
        prev_shift: false,
        prev_char: None,
        since_break: 1000,
    };
    // At most one long-context typo is pending a deferred fix at a time.
    let mut pending: Option<PendingTypo> = None;
    let mut lines_since_pending = 0usize;
    // Current buffer row of the cursor (== number of newlines emitted so far).
    let mut buf_row = 0usize;

    // Prelude: enter insert mode.
    let d = rng.random_range(250.0..700.0);
    events.push(Event {
        action: Action::EnterInsert,
        delay_after: ms(d),
    });
    fixed_ms += d;

    for (li, line) in lines.iter().enumerate() {
        let progress = li as f64 / total_lines as f64;
        let recovery = 0.5 * cfg.fatigue * (1.0 - st.since_break as f64 / 20.0).max(0.0);
        let fatigue = (1.0 + cfg.fatigue * progress.powf(1.3) - recovery).max(0.85);
        let chars: Vec<char> = line.chars().collect();
        let mut word_speed = 1.0_f64;
        let mut i = 0usize;

        while i < chars.len() {
            let ch = match chars.get(i) {
                Some(&c) => c,
                None => break,
            };
            // Entering a new word: (re)sample its speed.
            if ch != ' ' && (i == 0 || chars.get(i - 1) == Some(&' ')) {
                word_speed = sample_word_speed(&chars, i, &mut rng);
            }
            // Typos only when nothing is awaiting a deferred fix, and never on the
            // first char of a line (keeps corrections on-screen and column-0 safe).
            if pending.is_none() && i > 0 {
                // Long-context: a length-preserving error left in place and fixed
                // later, at the end of the section.
                if cfg.long_typo_chance > 0.0
                    && rng.random_bool(cfg.long_typo_chance)
                    && let Some((wrong, correct)) = long_typo_at(&chars, i, &mut rng)
                {
                    for &wc in &wrong {
                        let d = keypress_delay(wc, word_speed, fatigue, cfg, &mut st, &mut rng);
                        events.push(Event {
                            action: Action::Char(wc),
                            delay_after: ms(d),
                        });
                        fixed_ms += d;
                        keystrokes += 1;
                    }
                    i += wrong.len();
                    pending = Some(PendingTypo {
                        row: li,
                        col: i - wrong.len(),
                        correct,
                    });
                    lines_since_pending = 0;
                    continue;
                }
                // Short-context: wrong keys + notice-latency + in-line correction.
                if cfg.typo_chance > 0.0
                    && ch.is_ascii_alphanumeric()
                    && rng.random_bool(cfg.typo_chance)
                {
                    i = emit_typo(
                        &mut events,
                        &mut fixed_ms,
                        &mut keystrokes,
                        &chars,
                        i,
                        word_speed,
                        fatigue,
                        cfg,
                        &mut st,
                        &mut rng,
                    );
                    continue;
                }
            }

            let mut d = keypress_delay(ch, word_speed, fatigue, cfg, &mut st, &mut rng);
            if ch == ' ' {
                // Tier-2 thinking: pause on the space before a hard next token.
                let hard = token_is_hard(&chars, i + 1);
                let p = (cfg.pause_on_space_chance * if hard { 2.2 } else { 0.6 }).clamp(0.0, 0.5);
                if rng.random_bool(p) {
                    let span = (900.0_f64 / 260.0).ln();
                    d += 260.0 * rng.random_range(0.0..span).exp();
                }
            } else if rng.random_bool(0.010) {
                // Tier-2b: rare mid-word freeze (recall the part value).
                d += rng.random_range(350.0..1100.0);
            }
            events.push(Event {
                action: Action::Char(ch),
                delay_after: ms(d),
            });
            fixed_ms += d;
            keystrokes += 1;
            i += 1;
        }

        // Newline. Its pause (the elastic tier-3 "think" before the NEXT line) is
        // weighted by the next line's complexity, occasional small/big thinks,
        // and block boundaries; it absorbs the bulk of the target duration.
        events.push(Event {
            action: Action::Enter,
            delay_after: Duration::ZERO,
        });
        let idx = events.len() - 1;
        buf_row += 1; // cursor is now on a fresh buffer line

        // Heavy-tailed weights: a small base so MOST line breaks get little filler
        // (fast successive lines), while occasional thinks and block boundaries
        // concentrate the bulk of the pause time — bursty, not metronomic.
        let next_complexity = lines.get(li + 1).map(|l| complexity(l)).unwrap_or(1.0);
        let mut weight = 0.3 + 1.6 * next_complexity;
        let mut big = false;
        if rng.random_bool(0.12) {
            weight += rng.random_range(1.5..4.0); // common small think
        }
        if rng.random_bool(cfg.big_think_chance) {
            weight += rng.random_range(4.0..12.0);
            big = true;
            if rng.random_bool(0.15) {
                weight += rng.random_range(8.0..20.0); // rare multi-second stall
            }
        }
        if rng.random_bool(0.02) {
            weight += rng.random_range(28.0..65.0); // rare long break (re-read / step away)
            big = true;
        }
        let next_blank = lines.get(li + 1).map(|l| l.trim().is_empty()).unwrap_or(true);
        if line.trim().is_empty() || next_blank {
            weight += rng.random_range(10.0..26.0); // block boundary
            big = true;
        }
        let floor = rng.random_range(150.0..230.0);
        elastic.push((idx, weight, floor));
        if big {
            st.since_break = 0; // fatigue recovers after a real think
        }
        // A newline resets digraph context.
        st.prev_char = None;
        st.prev_shift = false;

        // Deferred-correction: at a section boundary (or after carrying it for a
        // while), navigate back and fix the pending long-context typo in place.
        if pending.is_some() {
            lines_since_pending += 1;
            let at_boundary = line.trim().is_empty() || next_blank;
            let do_now = lines_since_pending >= 12 || (at_boundary && rng.random_bool(0.7));
            if do_now && let Some(p) = pending.take() {
                emit_correction(
                    &mut events, &mut fixed_ms, &mut keystrokes, &p, buf_row, cfg, &mut st, &mut rng,
                );
                st.prev_char = None;
                st.prev_shift = false;
            }
        }
    }

    // Flush any still-pending correction before finishing, so the final saved
    // buffer is exact. The cursor is at column 0 of the last buffer line.
    if let Some(p) = pending.take() {
        emit_correction(
            &mut events, &mut fixed_ms, &mut keystrokes, &p, buf_row, cfg, &mut st, &mut rng,
        );
    }

    // --- fit total to target by inflating the elastic pauses ---
    let target_ms = target.as_secs_f64() * 1000.0;
    let total_weight: f64 = elastic.iter().map(|(_, w, _)| *w).sum();
    let floor_total: f64 = elastic.iter().map(|(_, _, f)| *f).sum();
    let filler = (target_ms - fixed_ms - floor_total).max(0.0);
    for (idx, w, floor) in &elastic {
        let share = if total_weight > 0.0 {
            filler * (w / total_weight)
        } else {
            0.0
        };
        let jitter = rng.random_range(0.82..1.18);
        if let Some(ev) = events.get_mut(*idx) {
            ev.delay_after = ms((floor + share) * jitter);
        }
    }

    // --- insert saves, never more than save_max apart ---
    //
    // Saves are only safe at a line boundary (column 0): the `<Esc>:w<CR>i`
    // sequence nudges the cursor, which is harmless only when there is nothing
    // to its left. Every line ends with an Enter, so the pause *after* an Enter
    // is a column-0 region; a long "think" pause there is split so a `:w` can
    // slip into the middle of it. But between two Enters (while typing one line)
    // there is NO safe save point, so the worst un-interruptible stretch is the
    // longest single line's typing time. Cap the save threshold below `save_max`
    // by that longest-line time (plus a margin for the save group's own
    // keystrokes), so even if the threshold is reached just before a long line,
    // the next save still lands inside the cap.
    // `.max(1.0)`: a zero/negative cap would make the threshold range empty and
    // panic `random_range`.
    let save_max_ms = cfg.save_max_secs.max(1.0) * 1000.0;
    let mut max_line_ms = 0.0_f64;
    let mut run_ms = 0.0_f64;
    for ev in &events {
        if ev.action == Action::Enter {
            max_line_ms = max_line_ms.max(run_ms);
            run_ms = 0.0;
        } else {
            run_ms += ev.delay_after.as_secs_f64() * 1000.0;
        }
    }
    max_line_ms = max_line_ms.max(run_ms);
    let margin = 2500.0; // headroom for the save group's pre-CmdEnter keystrokes
    let thresh_hi = (save_max_ms - max_line_ms - margin).max(save_max_ms * 0.15);
    let mut thresh_lo = save_max_ms * 0.45;
    if thresh_lo >= thresh_hi {
        thresh_lo = thresh_hi * 0.6; // pathological: a single line near the cap
    }
    let mut out: Vec<Event> = Vec::with_capacity(events.len() + 16);
    let mut since_save = 0.0_f64;
    let mut threshold = rng.random_range(thresh_lo..thresh_hi);
    let mut saves = 0usize;

    for ev in events {
        let pause_ms = ev.delay_after.as_secs_f64() * 1000.0;
        if ev.action == Action::Enter {
            out.push(Event {
                action: Action::Enter,
                delay_after: Duration::ZERO,
            });
            let mut remaining = pause_ms;
            loop {
                let room = (threshold - since_save).max(0.0);
                if remaining <= room {
                    add_to_last(&mut out, remaining);
                    since_save += remaining;
                    break;
                }
                add_to_last(&mut out, room);
                remaining -= room;
                push_save_group(&mut out, cfg, &mut st, &mut rng);
                saves += 1;
                since_save = 0.0;
                threshold = rng.random_range(thresh_lo..thresh_hi);
            }
        } else {
            since_save += pause_ms;
            out.push(ev);
        }
    }
    // Always finish with a save.
    push_save_group(&mut out, cfg, &mut st, &mut rng);
    saves += 1;

    let total: Duration = out.iter().map(|e| e.delay_after).sum();
    Plan {
        events: out,
        total,
        keystrokes,
        lines: total_lines,
        saves,
    }
}

/// Pick a plausible wrong neighbor for a typo (adjacent on a QWERTY row, else a
/// nearby digit/letter).
fn typo_char(c: char, rng: &mut StdRng) -> char {
    const ROWS: [&str; 4] = ["1234567890", "qwertyuiop", "asdfghjkl", "zxcvbnm"];
    let lower = c.to_ascii_lowercase();
    for row in ROWS {
        if let Some(pos) = row.find(lower) {
            let bytes = row.as_bytes();
            let delta: i32 = if rng.random_bool(0.5) { 1 } else { -1 };
            let np = (pos as i32 + delta).clamp(0, bytes.len() as i32 - 1) as usize;
            let nc = bytes.get(np).map(|b| *b as char).unwrap_or(c);
            return if c.is_ascii_uppercase() {
                nc.to_ascii_uppercase()
            } else {
                nc
            };
        }
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Replay through the authoritative [`super::super::nvim`] model — the SAME
    /// mock that powers the runtime self-test — so the tests prove the action
    /// stream reconstructs the text against the exact model the workflow trusts.
    fn replay(plan: &Plan) -> (String, usize) {
        crate::workflows::complete::nvim::simulate(&plan.events)
    }

    /// Worst REAL wall-clock gap (seconds) between consecutive `:w` saves,
    /// counting EVERY intervening delay — including the save group's own
    /// pre-CmdEnter keystrokes (Esc / `:` / `w`) — and the run start before the
    /// first save. This is the strict measure the 5-minute cap must satisfy.
    fn max_real_gap_secs(plan: &Plan) -> f64 {
        let mut since = 0.0_f64;
        let mut worst = 0.0_f64;
        for ev in &plan.events {
            if ev.action == Action::CmdEnter {
                worst = worst.max(since);
                since = ev.delay_after.as_secs_f64();
            } else {
                since += ev.delay_after.as_secs_f64();
            }
        }
        worst
    }

    const SAMPLE: &str = "* comment line\n.title demo\n\nVin in 0 DC 5\nR1 in out {10k*2}\n.control\n  tran 1u 1m\n.endc\n.end\n";

    // A longer, content-varied sample to exercise all typo kinds and digraphs.
    const LONG: &str = "* Rate-of-change monitor with a windowed comparator\n.title roc\nVdd vdd 0 DC 5\nVin in 0 PWL(0 0 10m 0 12m 2 30m 2)\nCd in n1 100n\nRd n1 0 10k\nEamp out 0 n1 0 100\nRref1 vdd a 10k\nRref2 a 0 10k\n.control\n  tran 10u 30m\n  meas tran slope_max MAX v(out)\n  wrdata plots/roc out in\n.endc\n.end\n";

    // Adversarial: a few very long single lines, so one line's UN-interruptible
    // typing accumulation (no save point until the next Enter) is large.
    fn long_line_netlist() -> String {
        let mut line = String::from("R1 in out ");
        for _ in 0..70 {
            line.push_str("10k*2+");
        }
        let mut s = String::new();
        for _ in 0..5 {
            s.push_str(&line);
            s.push('\n');
        }
        s
    }

    #[test]
    fn reconstructs_text_exactly_no_typos() {
        let plan = generate(SAMPLE, Duration::from_secs(120), &TypingConfig::default(), 1);
        let (got, saves) = replay(&plan);
        assert_eq!(got.trim_end_matches('\n'), SAMPLE.trim_end_matches('\n'));
        assert!(saves >= 1, "expected at least one save, got {saves}");
        assert_eq!(saves, plan.saves);
    }

    #[test]
    fn reconstructs_text_exactly_with_typos_many_seeds() {
        // Aggressive typo rate across many seeds and both samples must STILL
        // reconstruct the exact text (exercises all five typo kinds).
        let cfg = TypingConfig {
            typo_chance: 0.25,
            ..TypingConfig::default()
        };
        for seed in 0..200u64 {
            for text in [SAMPLE, LONG] {
                let plan = generate(text, Duration::from_secs(600), &cfg, seed);
                let (got, _) = replay(&plan);
                assert_eq!(
                    got.trim_end_matches('\n'),
                    text.trim_end_matches('\n'),
                    "reconstruction failed at seed {seed}"
                );
            }
        }
    }

    #[test]
    fn reconstructs_with_long_context_typos() {
        // Long-context (deferred, length-preserving) corrections must reconstruct
        // the exact text through the navigate-back-and-replace machinery, across
        // many seeds and both block-boundary and forced-flush paths.
        let cfg = TypingConfig {
            long_typo_chance: 0.05,
            ..TypingConfig::default()
        };
        for seed in 0..200u64 {
            for text in [SAMPLE, LONG] {
                let plan = generate(text, Duration::from_secs(600), &cfg, seed);
                let (got, _) = replay(&plan);
                assert_eq!(
                    got.trim_end_matches('\n'),
                    text.trim_end_matches('\n'),
                    "long-context reconstruction failed at seed {seed}"
                );
            }
        }
    }

    #[test]
    fn reconstructs_with_both_typo_kinds() {
        // Short- and long-context typos together (what the user enables) must
        // still reconstruct exactly — this is the runtime self-test's guarantee.
        let cfg = TypingConfig {
            typo_chance: 0.08,
            long_typo_chance: 0.03,
            ..TypingConfig::default()
        };
        for seed in 0..200u64 {
            for text in [SAMPLE, LONG] {
                let plan = generate(text, Duration::from_secs(900), &cfg, seed);
                let (got, _) = replay(&plan);
                assert_eq!(
                    got.trim_end_matches('\n'),
                    text.trim_end_matches('\n'),
                    "combined-typo reconstruction failed at seed {seed}"
                );
            }
        }
    }

    #[test]
    fn saves_respect_cap_with_typos() {
        // Corrections add keystrokes between line boundaries; the save cap must
        // still hold with both typo kinds active.
        let cfg = TypingConfig {
            typo_chance: 0.05,
            long_typo_chance: 0.03,
            ..TypingConfig::default()
        };
        let cap = cfg.save_max_secs;
        for &mins in &[8u64, 20, 60, 150] {
            for seed in 0..40u64 {
                for text in [SAMPLE, LONG] {
                    let plan = generate(text, Duration::from_secs(mins * 60), &cfg, seed);
                    assert!(
                        max_real_gap_secs(&plan) <= cap,
                        "save gap exceeded cap (mins {mins}, seed {seed})"
                    );
                }
            }
        }
    }

    #[test]
    fn total_lands_within_20pct_when_target_is_feasible() {
        let target = Duration::from_secs(30 * 60);
        for seed in [3u64, 17, 101, 2024] {
            let plan = generate(LONG, target, &TypingConfig::default(), seed);
            let ratio = plan.total.as_secs_f64() / target.as_secs_f64();
            assert!(
                (0.8..=1.2).contains(&ratio),
                "total {:?} not within 20% of {:?} (ratio {ratio:.3}, seed {seed})",
                plan.total,
                target
            );
        }
    }

    #[test]
    fn never_types_faster_than_human_when_target_too_short() {
        let plan = generate(LONG, Duration::from_secs(1), &TypingConfig::default(), 4);
        assert!(plan.total > Duration::from_secs(1));
        let (got, _) = replay(&plan);
        assert_eq!(got.trim_end_matches('\n'), LONG.trim_end_matches('\n'));
    }

    #[test]
    fn saves_respect_cap_real_wallclock() {
        // Sweep targets and seeds on the normal samples; the REAL gap between
        // saves (incl. the save group's own keystrokes) must never exceed the cap.
        let cfg = TypingConfig::default();
        let cap = cfg.save_max_secs;
        for &mins in &[6u64, 10, 20, 40, 90, 180] {
            for seed in 0..40u64 {
                for text in [SAMPLE, LONG] {
                    let plan = generate(text, Duration::from_secs(mins * 60), &cfg, seed);
                    let worst = max_real_gap_secs(&plan);
                    assert!(
                        worst <= cap,
                        "real save gap {worst:.1}s > cap {cap:.1}s (mins {mins}, seed {seed})"
                    );
                }
            }
        }
        // Over a long run there should be several saves.
        let plan = generate(LONG, Duration::from_secs(40 * 60), &cfg, 5);
        assert!(plan.saves >= 2, "expected multiple saves, got {}", plan.saves);
    }

    #[test]
    fn adversarial_long_lines_respect_save_cap() {
        // Long single lines stress the only path that can breach the cap; the
        // adaptive threshold must still keep every real save gap within bounds.
        let cfg = TypingConfig::default();
        let cap = cfg.save_max_secs;
        let text = long_line_netlist();
        for &mins in &[6u64, 10, 20, 40, 120, 300] {
            for seed in 0..80u64 {
                let plan = generate(&text, Duration::from_secs(mins * 60), &cfg, seed);
                let worst = max_real_gap_secs(&plan);
                assert!(
                    worst <= cap,
                    "real save gap {worst:.1}s > cap {cap:.1}s (mins {mins}, seed {seed})"
                );
                // And reconstruction must still be exact for the long lines.
                let (got, _) = replay(&plan);
                assert_eq!(got.trim_end_matches('\n'), text.trim_end_matches('\n'));
            }
        }
    }

    #[test]
    fn deterministic_for_same_seed() {
        let cfg = TypingConfig {
            typo_chance: 0.05,
            ..TypingConfig::default()
        };
        let a = generate(LONG, Duration::from_secs(300), &cfg, 42);
        let b = generate(LONG, Duration::from_secs(300), &cfg, 42);
        assert_eq!(a.events.len(), b.events.len());
        assert_eq!(a.total, b.total);
        assert_eq!(a.keystrokes, b.keystrokes);
        for (x, y) in a.events.iter().zip(b.events.iter()) {
            assert_eq!(x.action, y.action);
            assert_eq!(x.delay_after, y.delay_after);
        }
    }
}
