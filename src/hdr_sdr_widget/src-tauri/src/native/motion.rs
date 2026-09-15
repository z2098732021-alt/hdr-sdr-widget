//! Analytic damped springs. Retargeting never resets position or velocity.
#[derive(Clone, Copy, Debug)]
pub struct Spring {
    pub position: f64,
    pub velocity: f64,
    pub target: f64,
}

impl Spring {
    pub fn new(value: f64) -> Self {
        Self {
            position: value,
            velocity: 0.0,
            target: value,
        }
    }
    pub fn snap(&mut self, value: f64) {
        *self = Self::new(value);
    }
    pub fn moving(&self) -> bool {
        (self.position - self.target).abs() > 0.0001 || self.velocity.abs() > 0.001
    }
    pub fn step(&mut self, dt: f64, frequency: f64, damping: f64) {
        if !dt.is_finite() || dt <= 0.0 {
            return;
        }
        let w = frequency * std::f64::consts::TAU;
        let y = self.position - self.target;
        let v = self.velocity;
        let dt = dt.min(0.1);
        if damping < 1.0 {
            let a = damping * w;
            let b = w * (1.0 - damping * damping).sqrt();
            let e = (-a * dt).exp();
            let (s, c) = (b * dt).sin_cos();
            self.position = self.target + e * (y * c + (v + a * y) / b * s);
            self.velocity = e * (v * c - (a * v + w * w * y) / b * s);
        } else {
            let e = (-w * dt).exp();
            let j = v + w * y;
            self.position = self.target + (y + j * dt) * e;
            self.velocity = (v - w * j * dt) * e;
        }
        if !self.moving() {
            self.snap(self.target);
        }
    }
}

/// Bounded resistance; the hardware value is clamped separately.
pub fn rubber(distance: f64, limit: f64) -> f64 {
    distance.signum() * limit * (1.0 - (-distance.abs() / limit).exp())
}

/// Velocity energy couples travel to shape; smooth at reversal and bounded at high speed.
pub fn travel_stretch(velocity_dip: f64) -> f64 {
    let energy = (velocity_dip / 420.0).powi(2);
    0.04 * energy / (1.0 + energy)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn travel_shape_is_bounded_symmetric_and_restores_at_rest() {
        assert_eq!(travel_stretch(0.0), 0.0);
        for v in [0.01, 20.0, 420.0, 2500.0, 100000.0] {
            assert!(travel_stretch(v) > 0.0 && travel_stretch(v) < 0.04);
            assert_eq!(travel_stretch(v), travel_stretch(-v));
        }
        assert!(travel_stretch(0.01) < 1e-9);
    }
    #[test]
    fn reveal_reversal_is_continuous_at_all_refresh_rates() {
        for hz in [60, 120, 160] {
            let mut reveal = Spring::new(0.65);
            reveal.target = 1.0;
            for _ in 0..hz / 10 {
                reveal.step(
                    1.0 / hz as f64,
                    super::super::tuning::REVEAL.0,
                    super::super::tuning::REVEAL.1,
                );
            }
            let before = (reveal.position, reveal.velocity);
            reveal.target = 0.65;
            assert_eq!(before, (reveal.position, reveal.velocity));
            for _ in 0..hz * 2 {
                reveal.step(
                    1.0 / hz as f64,
                    super::super::tuning::REVEAL.0,
                    super::super::tuning::REVEAL.1,
                );
            }
            assert!((reveal.position - 0.65).abs() < 0.001);
        }
    }
    #[test]
    fn refresh_rates_have_identical_trajectories() {
        let simulate = |hz: u32| {
            let mut s = Spring::new(0.0);
            s.target = 1.0;
            for _ in 0..hz / 2 {
                s.step(1.0 / hz as f64, 3.5, 0.78);
            }
            s
        };
        let a = simulate(60);
        let b = simulate(160);
        assert!((a.position - b.position).abs() < 1e-9);
        assert!((a.velocity - b.velocity).abs() < 1e-9);
    }
    #[test]
    fn reversal_preserves_momentum_and_settles() {
        let mut s = Spring::new(0.0);
        s.target = 1.0;
        s.step(0.04, 4.0, 0.8);
        let before = (s.position, s.velocity);
        s.target = -1.0;
        assert_eq!((s.position, s.velocity), before);
        for _ in 0..240 {
            s.step(1.0 / 120.0, 4.0, 0.8);
        }
        assert!((s.position + 1.0).abs() < 0.001);
        assert!(!s.moving());
    }
    #[test]
    fn resistance_is_continuous_bounded_and_symmetric() {
        assert_eq!(rubber(0.0, 12.0), 0.0);
        for i in 1..1000 {
            let x = i as f64;
            assert!(rubber(x, 12.0) <= 12.0);
            assert_eq!(rubber(-x, 12.0), -rubber(x, 12.0));
            assert!(rubber(x, 12.0) >= rubber(x - 1.0, 12.0));
        }
    }
}
