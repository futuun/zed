//! Velocity-based smooth scrolling.
//!
//! Scrolling is modeled as remaining travel with continuous-time exponential
//! decay: wheel input adds travel so the animation covers exactly the input
//! distance, and programmatic jumps retarget the travel so it lands exactly on
//! the target. There are no durations or easing curves, and `advance` is exact
//! for any frame interval, so dropped frames still land near the endpoint.
//!
//! Decay alone lags a continuously *moving* target (e.g. holding `j` nudges the
//! autoscroll target each key repeat), enough to push the cursor off-screen. So
//! when retargets arrive in rapid, small steps the animation also matches the
//! target's observed velocity (feedforward), draining the lag to zero. It only
//! ever drains travel that exists, so it cannot overshoot.

use crate::{App, Axis, Global, Point, point};
use std::time::Instant;

/// Fraction of remaining distance retained per ms. Matches UIKit's
/// `DecelerationRate.fast`: ~69ms half-life, ~95% settled within ~300ms.
const RETENTION_PER_MS: f64 = 0.99;

/// Retargets closer together than this are treated as a moving target (followed)
/// rather than discrete jumps. Covers OS key-repeat rates.
const FOLLOW_MAX_INTERVAL_MS: f64 = 150.;

/// Largest per-retarget step still treated as following. Larger steps are
/// discrete jumps (`G`, `zz`, search) and keep the plain glide.
const FOLLOW_MAX_STEP: f64 = 8.;

/// Runtime configuration for velocity-based smooth scrolling, stored as a global.
/// Defaults to disabled; zed sets it from the `smooth_scrolling` setting.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct SmoothScrollSettings {
    /// Whether wheel input and programmatic jumps animate.
    pub enabled: bool,
}

impl Global for SmoothScrollSettings {}

impl SmoothScrollSettings {
    /// Whether smooth scrolling is enabled in this app.
    pub fn enabled(cx: &App) -> bool {
        cx.try_global::<Self>()
            .map_or_else(|| Self::default().enabled, |settings| settings.enabled)
    }
}

/// Animation state for one scrollable surface: travel remaining before rest, in
/// scroll units (pixels for divs, display lines/columns for the editor).
#[derive(Debug, Clone, Default)]
pub struct ScrollVelocity {
    remaining: Point<f64>,
    /// Matched velocity of a continuously moving target, in units/ms.
    follow_velocity: Point<f64>,
    last_retarget_x: Option<(Instant, f64)>,
    last_retarget_y: Option<(Instant, f64)>,
    last_tick: Option<Instant>,
}

impl ScrollVelocity {
    /// Whether the animation is in motion.
    pub fn is_active(&self) -> bool {
        self.remaining.x != 0. || self.remaining.y != 0.
    }

    /// Extend the remaining travel by exactly `distance`; chained ticks accumulate.
    pub fn impulse(&mut self, distance: Point<f64>) {
        self.remaining.x += distance.x;
        self.remaining.y += distance.y;
        // Relative input has no target to follow; clear any stale follow velocity.
        if distance.x != 0. {
            self.follow_velocity.x = 0.;
            self.last_retarget_x = None;
        }
        if distance.y != 0. {
            self.follow_velocity.y = 0.;
            self.last_retarget_y = None;
        }
        self.on_motion_changed();
    }

    /// Re-aim the animation to come to rest on `target`, replacing in-flight motion.
    pub fn retarget(&mut self, current: Point<f64>, target: Point<f64>) {
        let now = Instant::now();
        self.retarget_axis(Axis::Horizontal, current.x, target.x, now);
        self.retarget_axis(Axis::Vertical, current.y, target.y, now);
    }

    /// Re-aim only one axis, preserving in-flight motion on the other.
    pub fn retarget_along(&mut self, axis: Axis, current: f64, target: f64) {
        self.retarget_axis(axis, current, target, Instant::now());
    }

    fn retarget_axis(&mut self, axis: Axis, current: f64, target: f64, now: Instant) {
        let last_retarget = match axis {
            Axis::Horizontal => &mut self.last_retarget_x,
            Axis::Vertical => &mut self.last_retarget_y,
        };
        let mut follow_velocity = 0.;
        if let Some((last_time, last_target)) = *last_retarget {
            let dt_ms = now.saturating_duration_since(last_time).as_secs_f64() * 1000.;
            let step = target - last_target;
            if dt_ms > 0.
                && dt_ms <= FOLLOW_MAX_INTERVAL_MS
                && step != 0.
                && step.abs() <= FOLLOW_MAX_STEP
            {
                follow_velocity = step / dt_ms;
            }
        }
        *last_retarget = Some((now, target));

        match axis {
            Axis::Horizontal => {
                self.remaining.x = target - current;
                self.follow_velocity.x = follow_velocity;
            }
            Axis::Vertical => {
                self.remaining.y = target - current;
                self.follow_velocity.y = follow_velocity;
            }
        }
        self.on_motion_changed();
    }

    /// Advance to `now`, returning the position delta to apply. Exact for any
    /// interval; each axis snaps to rest once its remaining travel falls below
    /// `snap_epsilon`, expressed in the caller's scroll units.
    pub fn advance(&mut self, now: Instant, snap_epsilon: f64) -> Point<f64> {
        let Some(last_tick) = self.last_tick else {
            return point(0., 0.);
        };
        let dt_ms = now.saturating_duration_since(last_tick).as_secs_f64() * 1000.;
        self.last_tick = Some(now);

        // Fraction of remaining distance retained over the interval.
        let decay = RETENTION_PER_MS.powf(dt_ms);
        let mut delta = point(
            self.remaining.x * (1. - decay),
            self.remaining.y * (1. - decay),
        );
        self.remaining.x *= decay;
        self.remaining.y *= decay;

        // Feedforward: cover the target's own motion, clamped to existing travel
        // so we never pass it (see module docs).
        let feedforward_x = clamp_toward(self.follow_velocity.x * dt_ms, self.remaining.x);
        delta.x += feedforward_x;
        self.remaining.x -= feedforward_x;
        let feedforward_y = clamp_toward(self.follow_velocity.y * dt_ms, self.remaining.y);
        delta.y += feedforward_y;
        self.remaining.y -= feedforward_y;

        if self.remaining.x.abs() < snap_epsilon {
            delta.x += self.remaining.x;
            self.remaining.x = 0.;
        }
        if self.remaining.y.abs() < snap_epsilon {
            delta.y += self.remaining.y;
            self.remaining.y = 0.;
        }

        if !self.is_active() {
            self.last_tick = None;
        }
        delta
    }

    /// Travel remaining before rest, in scroll units; zero when inactive.
    pub fn remaining_travel(&self) -> Point<f64> {
        self.remaining
    }

    /// Cancel the animation.
    pub fn stop(&mut self) {
        *self = Self::default();
    }

    /// Cancel motion along one axis, e.g. when that axis hit a scroll boundary.
    pub fn stop_along(&mut self, axis: Axis) {
        match axis {
            Axis::Horizontal => {
                self.remaining.x = 0.;
                self.follow_velocity.x = 0.;
                self.last_retarget_x = None;
            }
            Axis::Vertical => {
                self.remaining.y = 0.;
                self.follow_velocity.y = 0.;
                self.last_retarget_y = None;
            }
        }
        if !self.is_active() {
            self.last_tick = None;
        }
    }

    fn on_motion_changed(&mut self) {
        if self.is_active() {
            if self.last_tick.is_none() {
                self.last_tick = Some(Instant::now());
            }
        } else {
            self.last_tick = None;
        }
    }
}

/// Clamp `delta` to lie between zero and `limit`, whichever sign `limit` has.
fn clamp_toward(delta: f64, limit: f64) -> f64 {
    if limit >= 0. {
        delta.clamp(0., limit)
    } else {
        delta.clamp(limit, 0.)
    }
}
