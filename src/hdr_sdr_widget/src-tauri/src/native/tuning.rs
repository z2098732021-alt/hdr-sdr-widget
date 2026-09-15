//! Production motion/material parameters. These are calibrated defaults, not Apple private constants.
// Response is the undamped period (omega = 2pi / response).
// Apple documents 0.5s / 0.825 as the default response/damping spring.
// These are reference-based app defaults, not a claim of private system timing.
pub const POSITION: (f64, f64) = (1.0 / 0.50, 0.825);
pub const REVEAL: (f64, f64) = (1.0 / 0.52, 0.825);
pub const SHAPE: (f64, f64) = (5.0, 0.82);
pub const PRESS: (f64, f64) = (9.0, 0.82);
pub const LIQUID: (f64, f64) = (5.0, 0.90);
pub const HOVER_WIDTH: f64 = 0.04;
pub const HOVER_HEIGHT: f64 = 0.012;
pub const PRESS_WIDTH: f64 = 0.015;
pub const PRESS_HEIGHT: f64 = -0.025;
pub const OPTICS: [f32; 4] = [10.0, 22.0, 0.25, 0.0];
