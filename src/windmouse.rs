//! Human-like mouse movement using the WindMouse algorithm.
//!
//! WindMouse (Ben Land, 2021) drives the cursor with a small physical simulation:
//! gravity pulls it toward the destination, a randomly evolving "wind" force bends
//! the path, and a per-step velocity clamp keeps the speed plausible. The result is
//! a curved path that accelerates away from the origin and settles slowly onto the
//! target, instead of the single instantaneous jump a naive `move_mouse` performs.
//!
//! The force constants and the inner loop mirror the DreamBot variant of the
//! algorithm; [`generate_path`] is a pure function so the simulation can be tested
//! without touching the input backend.

use std::time::Duration;

use enigo::{Coordinate, InputError, Mouse};
use rand::Rng;

/// A cursor position in native (physical) pixel space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    pub const fn new(x: i32, y: i32) -> Self {
        Self { x, y }
    }

    /// Euclidean distance to another point.
    fn distance_to(self, other: Self) -> f64 {
        f64::hypot(f64::from(other.x - self.x), f64::from(other.y - self.y))
    }
}

/// Force constants controlling the shape of a simulated move.
#[derive(Debug, Clone, Copy)]
pub struct WindMouseParams {
    /// Gravity strength: how hard the destination pulls the cursor.
    pub gravity: f64,
    /// Wind strength: how much randomness perturbs the path.
    pub wind: f64,
    /// Velocity ceiling, in pixels per simulated step.
    pub max_velocity: f64,
    /// Distance from the destination, in pixels, at which gravity dominates and
    /// the wind is bled off so the cursor settles instead of overshooting.
    pub distance_threshold: f64,
}

impl WindMouseParams {
    /// Randomizes the force constants so no two moves follow the same path.
    ///
    /// Value ranges are taken from the DreamBot WindMouse variant.
    pub fn randomized() -> Self {
        let mut rng = rand::rng();
        Self {
            gravity: rng.random_range(4.0..20.0),
            wind: rng.random_range(1.0..10.0),
            max_velocity: rng.random_range(15.0 / 2.0..15.0),
            distance_threshold: rng.random_range(5.0..25.0),
        }
    }
}

/// Time between two cursor updates.
///
/// Tuned near a real mouse's poll rate: the cursor visibly steps rather than
/// teleporting, but a full-screen move still finishes quickly.
pub const MOUSE_POLL_INTERVAL: Duration = Duration::from_millis(8);

/// Upper bound on simulated steps, so a degenerate force state can never spin
/// forever. Far beyond the step count any real screen distance needs.
const MAX_STEPS: usize = 4096;

/// The simulation ends this close (in pixels) to the destination.
const ARRIVAL_RADIUS: f64 = 1.0;

/// Simulates a human-like cursor path from `start` to `dest`.
///
/// Pure: no sleeps and no input events, so the same parameters and RNG draws
/// produce the same path. The returned points are consecutive cursor positions,
/// and the last one is always `dest` (the simulation stops within
/// [`ARRIVAL_RADIUS`], and the remainder is closed off exactly). An empty result
/// means the cursor is already there.
pub fn generate_path(start: Point, dest: Point, params: WindMouseParams) -> Vec<Point> {
    /// Damping applied to the wind vector every step.
    const SQRT3: f64 = 1.732_050_807_568_877_2;
    /// Divisor scaling each fresh wind impulse.
    const SQRT5: f64 = 2.236_067_977_499_79;

    let mut rng = rand::rng();
    let mut current = start;
    let mut velocity = (0.0_f64, 0.0_f64);
    let mut wind = (0.0_f64, 0.0_f64);
    let mut max_velocity = params.max_velocity;
    let mut path = Vec::new();

    for _ in 0..MAX_STEPS {
        let distance = current.distance_to(dest);
        if distance < ARRIVAL_RADIUS {
            break;
        }

        let wind_magnitude = params.wind.min(distance);
        if distance >= params.distance_threshold {
            // Far away: refresh the wind with a random impulse while damping its
            // previous value, which is what bends the path.
            wind.0 = wind.0 / SQRT3 + (2.0 * rng.random::<f64>() - 1.0) * wind_magnitude / SQRT5;
            wind.1 = wind.1 / SQRT3 + (2.0 * rng.random::<f64>() - 1.0) * wind_magnitude / SQRT5;
        } else {
            // Close in: bleed the wind off and clamp the speed so the cursor
            // decelerates into the target rather than overshooting it.
            wind.0 /= SQRT3;
            wind.1 /= SQRT3;
            max_velocity = if max_velocity < 3.0 {
                rng.random::<f64>() * 3.0 + 3.0
            } else {
                max_velocity / SQRT5
            };
        }

        // Gravity points at the destination with a fixed magnitude.
        let gravity_pull = (
            params.gravity * f64::from(dest.x - current.x) / distance,
            params.gravity * f64::from(dest.y - current.y) / distance,
        );

        velocity.0 += wind.0 + gravity_pull.0;
        velocity.1 += wind.1 + gravity_pull.1;

        let velocity_magnitude = f64::hypot(velocity.0, velocity.1);
        if velocity_magnitude > max_velocity {
            // Clip to a random value in the upper half of the allowed range so the
            // speed itself varies from step to step.
            let clip = max_velocity / 2.0 + rng.random::<f64>() * max_velocity / 2.0;
            let scale = clip / velocity_magnitude;
            velocity.0 *= scale;
            velocity.1 *= scale;
        }

        let next = Point::new(
            current.x + velocity.0.round() as i32,
            current.y + velocity.1.round() as i32,
        );
        if next != current {
            current = next;
            path.push(current);
        }
    }

    // The loop stops within a pixel of the destination; close the remainder so
    // callers always land exactly where they asked.
    if current != dest {
        path.push(dest);
    }

    path
}

/// Moves `input`'s cursor to `dest` along a human-like WindMouse path.
///
/// The current position is read from `input`, so callers need not track it.
/// Returns the number of interpolation steps sent (0 if the cursor was already
/// at `dest`). Blocking: call from a blocking thread.
pub fn move_to<I: Mouse + ?Sized>(input: &mut I, dest: Point) -> Result<usize, InputError> {
    let (x, y) = input.location()?;
    let path = generate_path(Point::new(x, y), dest, WindMouseParams::randomized());

    let mut rng = rand::rng();
    for point in &path {
        // Jitter each interval so the update cadence is not a perfect metronome.
        std::thread::sleep(MOUSE_POLL_INTERVAL + Duration::from_millis(rng.random_range(0..=4)));
        input.move_mouse(point.x, point.y, Coordinate::Abs)?;
    }

    Ok(path.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixed parameters so the assertions do not depend on the RNG's range.
    fn fixed_params() -> WindMouseParams {
        WindMouseParams {
            gravity: 9.0,
            wind: 4.0,
            max_velocity: 15.0,
            distance_threshold: 10.0,
        }
    }

    #[test]
    fn path_ends_exactly_at_the_destination() {
        let dest = Point::new(1234, 567);
        let path = generate_path(Point::new(0, 0), dest, fixed_params());
        assert_eq!(path.last(), Some(&dest));
    }

    #[test]
    fn long_moves_are_interpolated_not_teleported() {
        let path = generate_path(Point::new(0, 0), Point::new(1200, 700), fixed_params());
        assert!(path.len() > 10, "expected many steps, got {}", path.len());
        assert!(
            path.len() <= MAX_STEPS,
            "step budget exceeded: {}",
            path.len()
        );
    }

    #[test]
    fn move_already_at_the_destination_is_empty() {
        let point = Point::new(400, 300);
        assert!(generate_path(point, point, fixed_params()).is_empty());
    }

    #[test]
    fn steps_stay_within_the_velocity_ceiling() {
        let params = fixed_params();
        let start = Point::new(0, 0);
        let dest = Point::new(1500, -900);
        let path = generate_path(start, dest, params);

        // Every emitted position is reachable within one step, so no jump may
        // exceed the clamped velocity (plus up to a pixel of rounding per axis).
        let mut previous = start;
        let limit = params.max_velocity + f64::sqrt(2.0);
        for point in path {
            let step = previous.distance_to(point);
            assert!(
                step <= limit,
                "step {previous:?} -> {point:?} was {step:.1}px, limit {limit:.1}px"
            );
            previous = point;
        }
    }

    #[test]
    fn randomized_params_stay_in_the_documented_ranges() {
        for _ in 0..64 {
            let params = WindMouseParams::randomized();
            assert!((4.0..20.0).contains(&params.gravity));
            assert!((1.0..10.0).contains(&params.wind));
            assert!((7.5..15.0).contains(&params.max_velocity));
            assert!((5.0..25.0).contains(&params.distance_threshold));
        }
    }

    #[test]
    fn distance_to_uses_euclidean_geometry() {
        assert_eq!(Point::new(0, 0).distance_to(Point::new(3, 4)), 5.0);
        assert_eq!(Point::new(10, 10).distance_to(Point::new(10, 10)), 0.0);
    }
}
