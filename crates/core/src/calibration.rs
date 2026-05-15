//! Piecewise-linear calibration. Mirrors ESPHome's `calibrate_linear` filter,
//! including its semantics: with ≥2 points, points are sorted by input and
//! interpolation is linear between adjacent points, with the leftmost and
//! rightmost segments extrapolated.
//!
//! Two calibrations can be `chain`ed — the output of the first is fed into
//! the second. The pressure sensor in the original ESPHome config uses a
//! 2-stage chain.

use alloc::vec::Vec;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Calibration {
    /// (raw, calibrated) pairs. Sorted by `raw` on construction.
    points: Vec<[f32; 2]>,
}

impl Calibration {
    /// Build from at least two points. Returns `None` if fewer than 2 points
    /// are supplied or any two points share the same `raw` value.
    pub fn new<I>(points: I) -> Option<Self>
    where
        I: IntoIterator<Item = (f32, f32)>,
    {
        let mut pts: Vec<[f32; 2]> = points.into_iter().map(|(r, c)| [r, c]).collect();
        if pts.len() < 2 {
            return None;
        }
        pts.sort_by(|a, b| a[0].partial_cmp(&b[0]).unwrap_or(core::cmp::Ordering::Equal));
        for w in pts.windows(2) {
            if (w[0][0] - w[1][0]).abs() < f32::EPSILON {
                return None;
            }
        }
        Some(Self { points: pts })
    }

    /// Apply the calibration. Inputs outside the known range are extrapolated
    /// linearly using the nearest segment.
    pub fn apply(&self, raw: f32) -> f32 {
        let pts = &self.points;
        // Find the segment whose left x ≤ raw < right x. If raw < first or
        // raw ≥ last, extrapolate using the edge segment.
        if raw < pts[0][0] {
            return interp(pts[0], pts[1], raw);
        }
        for w in pts.windows(2) {
            if raw >= w[0][0] && raw <= w[1][0] {
                return interp(w[0], w[1], raw);
            }
        }
        let n = pts.len();
        interp(pts[n - 2], pts[n - 1], raw)
    }

    /// Compose two calibrations: `(self.then(other))(x) == other.apply(self.apply(x))`.
    pub fn then(&self, other: &Calibration) -> Calibration {
        // Collapse to a new piecewise-linear function on the union of breakpoints.
        // Self's outputs become inputs to `other`, so the chained breakpoints
        // are: self's input points, plus any of other's input points that fall
        // within self's output range mapped back through self's inverse.
        let mut xs: Vec<f32> = self.points.iter().map(|p| p[0]).collect();
        // pull other's breakpoints back through self if they fall within self's
        // output range
        let self_outs: Vec<f32> = self.points.iter().map(|p| p[1]).collect();
        let (lo, hi) = (
            self_outs.iter().cloned().fold(f32::INFINITY, f32::min),
            self_outs.iter().cloned().fold(f32::NEG_INFINITY, f32::max),
        );
        for op in &other.points {
            let y = op[0];
            if y >= lo && y <= hi {
                if let Some(x) = invert_self(self, y) {
                    xs.push(x);
                }
            }
        }
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
        xs.dedup_by(|a, b| (*a - *b).abs() < f32::EPSILON);
        let pairs: Vec<(f32, f32)> = xs
            .into_iter()
            .map(|x| (x, other.apply(self.apply(x))))
            .collect();
        Calibration::new(pairs).expect("chained calibration should have ≥2 unique points")
    }
}

fn interp(a: [f32; 2], b: [f32; 2], x: f32) -> f32 {
    let (x0, y0) = (a[0], a[1]);
    let (x1, y1) = (b[0], b[1]);
    let t = (x - x0) / (x1 - x0);
    y0 + t * (y1 - y0)
}

/// Solve `cal.apply(x) = y` for x by walking the (raw, calibrated) segments.
/// Used internally by `then`. Returns None if y is outside cal's output range
/// (no extrapolation here — extrapolation is handled by `apply`).
fn invert_self(cal: &Calibration, y: f32) -> Option<f32> {
    let pts = &cal.points;
    for w in pts.windows(2) {
        let (lo, hi) = (w[0][1].min(w[1][1]), w[0][1].max(w[1][1]));
        if y >= lo && y <= hi {
            let (x0, y0) = (w[0][0], w[0][1]);
            let (x1, y1) = (w[1][0], w[1][1]);
            if (y1 - y0).abs() < f32::EPSILON {
                return Some(x0);
            }
            let t = (y - y0) / (y1 - y0);
            return Some(x0 + t * (x1 - x0));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32, eps: f32) {
        assert!(
            (a - b).abs() < eps,
            "expected {a} ≈ {b} (diff {})",
            (a - b).abs()
        );
    }

    #[test]
    fn battery_calibration_matches_yaml_anchors() {
        // From ref/watercontroller_esphome.yaml: 1130 -> 5.00V, 2931 -> 12.2V
        let cal = Calibration::new([(1130.0, 5.00), (2931.0, 12.2)]).unwrap();
        approx(cal.apply(1130.0), 5.00, 1e-4);
        approx(cal.apply(2931.0), 12.2, 1e-4);
        // Midpoint roughly halfway in volts.
        approx(cal.apply(2030.5), 8.6, 1e-3);
    }

    #[test]
    fn pressure_two_stage_chain_matches_yaml() {
        // Stage 1: 0.37 -> 0.54, 2.62 -> 3.98
        // Stage 2: 0.54 -> 0.0, 4.50 -> 10.34214
        let s1 = Calibration::new([(0.37, 0.54), (2.62, 3.98)]).unwrap();
        let s2 = Calibration::new([(0.54, 0.0), (4.50, 10.34214)]).unwrap();
        let chained = s1.then(&s2);

        // Check chain anchors
        approx(chained.apply(0.37), 0.0, 1e-3);
        // 2.62V -> 3.98 (stage1) -> 9.0867 bar (stage2)
        let stage2_at_398 = s2.apply(3.98);
        approx(chained.apply(2.62), stage2_at_398, 1e-3);
        // Linearity of the chain (composition of two linear functions): equal
        // to the direct composition at any input.
        approx(chained.apply(1.5), s2.apply(s1.apply(1.5)), 1e-3);
        approx(chained.apply(2.0), s2.apply(s1.apply(2.0)), 1e-3);
    }

    #[test]
    fn extrapolation_uses_nearest_segment() {
        let cal = Calibration::new([(0.0, 0.0), (10.0, 100.0)]).unwrap();
        approx(cal.apply(-5.0), -50.0, 1e-4);
        approx(cal.apply(15.0), 150.0, 1e-4);
    }

    #[test]
    fn rejects_fewer_than_two_points() {
        assert!(Calibration::new([] as [(f32, f32); 0]).is_none());
        assert!(Calibration::new([(1.0, 2.0)]).is_none());
    }

    #[test]
    fn rejects_duplicate_x_inputs() {
        assert!(Calibration::new([(1.0, 2.0), (1.0, 3.0)]).is_none());
    }

    #[test]
    fn three_point_piecewise() {
        let cal = Calibration::new([(0.0, 0.0), (1.0, 10.0), (2.0, 12.0)]).unwrap();
        approx(cal.apply(0.5), 5.0, 1e-4); // first segment
        approx(cal.apply(1.5), 11.0, 1e-4); // second segment, slope 2
    }
}
