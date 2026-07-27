//! Human-like motion and timing.
//!
//! The defining requirement of Golem: clicks and movement must look like a
//! person, not the instantaneous "taps" you get from raw DevTools input. This
//! module is pure (no IO) so it can be unit-tested and reused by *both* the CDP
//! backend (dispatching `Input.dispatchMouseEvent` per path point) and the
//! native backend (moving the OS cursor per path point).
//!
//! The model:
//! - Mouse paths follow a cubic Bézier with two randomized control points
//!   offset perpendicular to the straight line, so the cursor arcs.
//! - Step count scales with distance; per-step delay is eased (slow-fast-slow)
//!   with multiplicative jitter and occasional micro-pauses.
//! - An optional slight overshoot-and-correct near the target.
//! - Typing uses a per-character delay distribution with occasional longer
//!   "thinking" pauses. (No simulated typos — correctness of typed text
//!   matters far more than realism for these workflows.)

use std::time::Duration;

use rand::{Rng, RngExt};
use serde::{Deserialize, Serialize};

use crate::geometry::Point;

/// Tunable knobs for the humanizer. Serialized as part of `Settings` so the
/// user can slow things down or tighten jitter from the GUI.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct HumanizeConfig {
    /// Overall speed multiplier. `1.0` = baseline; `2.0` = twice as fast
    /// (delays halved); `0.5` = half speed.
    pub speed: f64,
    /// How far control points may bow away from the straight line, as a
    /// fraction of the move distance. `0.0` = straight line.
    pub curve: f64,
    /// Positional jitter in pixels applied to intermediate path points.
    pub jitter_px: f64,
    /// Probability `[0,1]` of a small overshoot + correction near the target.
    pub overshoot_chance: f64,
    /// Baseline milliseconds the pointer spends per path step (before easing
    /// and jitter). Movement total ≈ steps * this / speed.
    pub step_ms: f64,
    /// Mean milliseconds between keystrokes (before jitter), divided by speed.
    pub keypress_ms: f64,
    /// Minimum / maximum path steps regardless of distance.
    pub min_steps: usize,
    pub max_steps: usize,
}

impl Default for HumanizeConfig {
    fn default() -> Self {
        HumanizeConfig {
            speed: 1.0,
            curve: 0.18,
            jitter_px: 1.4,
            overshoot_chance: 0.18,
            step_ms: 9.0,
            keypress_ms: 95.0,
            min_steps: 12,
            max_steps: 140,
        }
    }
}

impl HumanizeConfig {
    fn speed(&self) -> f64 {
        // Guard against zero/negative speeds set via config; never divide by 0.
        if self.speed.is_finite() && self.speed > 0.01 {
            self.speed
        } else {
            1.0
        }
    }
}

/// One waypoint along a movement: where to put the cursor and how long to wait
/// *before* moving to the next one.
#[derive(Clone, Copy, Debug)]
pub struct MoveStep {
    pub point: Point,
    pub delay: Duration,
}

/// Standard-normal sample via Box-Muller. `rand` itself ships no Normal
/// distribution and pulling in `rand_distr` for one transform isn't worth a
/// dependency; this is the textbook two-uniforms version.
fn gauss<R: Rng>(rng: &mut R) -> f64 {
    let u1: f64 = rng.random_range(f64::EPSILON..1.0);
    let u2: f64 = rng.random_range(0.0..1.0);
    (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
}

/// Gaussian positional jitter: independent N(0, sigma²) per axis, each
/// clamped to ±`max_abs` so a rare 4σ outlier can't fling the cursor off a
/// small target.
pub fn gaussian_jitter<R: Rng>(sigma: f64, max_abs: f64, rng: &mut R) -> (f64, f64) {
    if sigma <= 0.0 || max_abs <= 0.0 {
        return (0.0, 0.0);
    }
    (
        (gauss(rng) * sigma).clamp(-max_abs, max_abs),
        (gauss(rng) * sigma).clamp(-max_abs, max_abs),
    )
}

/// Where a click actually lands relative to the aimed-at point. Humans
/// cluster around the centre of a control with roughly normal spread -- they
/// don't hit the exact centre every time (and exact-centre clicks across a
/// whole form are a strong automation tell). Clamped to ±5px so even the
/// small Good/Bad buttons (~24px tall) are never missed from their centre.
pub fn click_landing_jitter<R: Rng>(cfg: &HumanizeConfig, rng: &mut R) -> (f64, f64) {
    gaussian_jitter(cfg.jitter_px * 1.6, 5.0, rng)
}

/// Cubic Bézier interpolation.
fn cubic_bezier(p0: Point, p1: Point, p2: Point, p3: Point, t: f64) -> Point {
    let u = 1.0 - t;
    let w0 = u * u * u;
    let w1 = 3.0 * u * u * t;
    let w2 = 3.0 * u * t * t;
    let w3 = t * t * t;
    Point::new(
        w0 * p0.x + w1 * p1.x + w2 * p2.x + w3 * p3.x,
        w0 * p0.y + w1 * p1.y + w2 * p2.y + w3 * p3.y,
    )
}

/// Ease-in-out (smoothstep-ish) so the cursor accelerates then decelerates.
fn ease(t: f64) -> f64 {
    // cosine ease: 0 at t=0, 1 at t=1, slow at both ends.
    0.5 - 0.5 * (std::f64::consts::PI * t).cos()
}

/// Build a human-like path from `start` to `end`. Always returns at least one
/// step ending exactly on `end`.
pub fn mouse_path<R: Rng>(start: Point, end: Point, cfg: &HumanizeConfig, rng: &mut R) -> Vec<MoveStep> {
    let dist = start.distance(&end);
    let speed = cfg.speed();

    // Degenerate move: already there.
    if dist < 0.5 {
        return vec![MoveStep {
            point: end,
            delay: Duration::from_millis(0),
        }];
    }

    // Step count scales sub-linearly with distance.
    let raw_steps = (dist.sqrt() * 2.2) as usize;
    let steps = raw_steps.clamp(cfg.min_steps, cfg.max_steps);

    // Perpendicular unit vector for bowing the control points.
    let dx = end.x - start.x;
    let dy = end.y - start.y;
    let len = (dx * dx + dy * dy).sqrt().max(1.0);
    let (px, py) = (-dy / len, dx / len);

    let bow = |rng: &mut R| -> f64 {
        let mag = dist * cfg.curve;
        rng.random_range(-mag..mag)
    };
    let along = |frac: f64, off: f64, rng: &mut R| -> Point {
        let base = Point::lerp(start, end, frac);
        let b = bow(rng);
        Point::new(base.x + px * b + off, base.y + py * b + off)
    };
    let c1 = along(0.33, 0.0, rng);
    let c2 = along(0.66, 0.0, rng);

    let mut out: Vec<MoveStep> = Vec::with_capacity(steps + 4);
    for i in 1..=steps {
        let t = ease(i as f64 / steps as f64);
        let mut p = cubic_bezier(start, c1, c2, end, t);
        // Positional jitter on intermediate points only. Gaussian, not
        // uniform: real hand tremor clusters near the ideal path with rare
        // larger wobbles, where uniform noise has a flat, boxy signature.
        if i != steps && cfg.jitter_px > 0.0 {
            let (jx, jy) = gaussian_jitter(cfg.jitter_px, cfg.jitter_px * 2.5, rng);
            p.x += jx;
            p.y += jy;
        }
        // Per-step delay: eased base * jitter, with rare micro-pauses.
        let mut ms = cfg.step_ms * rng.random_range(0.6..1.5);
        if rng.random_bool(0.04) {
            ms += rng.random_range(8.0..40.0);
        }
        out.push(MoveStep {
            point: p,
            delay: Duration::from_secs_f64((ms / speed).max(0.0) / 1000.0),
        });
    }

    // Optional overshoot + correction.
    if rng.random_bool(cfg.overshoot_chance.clamp(0.0, 1.0)) {
        let over = Point::new(
            end.x + rng.random_range(-6.0..6.0) + px * rng.random_range(-3.0..3.0),
            end.y + rng.random_range(-6.0..6.0) + py * rng.random_range(-3.0..3.0),
        );
        if let Some(last) = out.last_mut() {
            last.point = over;
        }
        for k in 1..=3 {
            let t = k as f64 / 3.0;
            out.push(MoveStep {
                point: Point::lerp(over, end, t),
                delay: Duration::from_secs_f64((cfg.step_ms * 1.2 / speed) / 1000.0),
            });
        }
    }

    // Guarantee we finish exactly on target.
    out.push(MoveStep {
        point: end,
        delay: Duration::from_secs_f64((cfg.step_ms / speed) / 1000.0),
    });
    out
}

/// How long a mouse button is held down during a click.
pub fn click_hold<R: Rng>(cfg: &HumanizeConfig, rng: &mut R) -> Duration {
    let ms = rng.random_range(45.0..120.0) / cfg.speed();
    Duration::from_secs_f64(ms / 1000.0)
}

/// A small settle pause after arriving before pressing (humans don't click the
/// instant they stop moving).
pub fn pre_click_settle<R: Rng>(cfg: &HumanizeConfig, rng: &mut R) -> Duration {
    let ms = rng.random_range(30.0..140.0) / cfg.speed();
    Duration::from_secs_f64(ms / 1000.0)
}

/// Per-character delays for typing `text`. Length == number of chars.
pub fn typing_delays<R: Rng>(text: &str, cfg: &HumanizeConfig, rng: &mut R) -> Vec<Duration> {
    let speed = cfg.speed();
    text.chars()
        .map(|c| {
            let mut ms = cfg.keypress_ms * rng.random_range(0.5..1.7);
            // Longer pause after sentence punctuation / spaces, occasionally.
            if matches!(c, '.' | ',' | '!' | '?' | '\n') {
                ms += rng.random_range(80.0..260.0);
            } else if c == ' ' && rng.random_bool(0.15) {
                ms += rng.random_range(40.0..160.0);
            } else if rng.random_bool(0.03) {
                ms += rng.random_range(120.0..420.0);
            }
            Duration::from_secs_f64((ms / speed) / 1000.0)
        })
        .collect()
}

/// A bounded random pause, used by `WorkflowCtx::human_pause`.
pub fn random_pause<R: Rng>(min_ms: u64, max_ms: u64, cfg: &HumanizeConfig, rng: &mut R) -> Duration {
    let (lo, hi) = if max_ms > min_ms {
        (min_ms as f64, max_ms as f64)
    } else {
        (min_ms as f64, min_ms as f64 + 1.0)
    };
    let ms = rng.random_range(lo..hi) / cfg.speed();
    Duration::from_secs_f64(ms / 1000.0)
}
