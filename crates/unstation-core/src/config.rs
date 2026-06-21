//! Engine configuration and picker tuning knobs (TECH_SPEC §8).

use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Live,
    Vod,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Role {
    Publisher,
    Viewer,
    Seed,
}

/// Picker weights — the knobs the simulator sweeps and CI pins.
#[derive(Clone, Copy, Debug)]
pub struct PickerWeights {
    /// Deadline/urgency weight `w_d` (dominates as deadlines approach).
    pub w_d: f64,
    /// Rarity weight `w_r`.
    pub w_r: f64,
    /// Probabilistic-spreading exponent `β` (greedy-ish; default 4).
    pub beta: f64,
    /// Panic-zone horizon: segments due within this many ms are earliest-deadline-first.
    pub panic_horizon_ms: u64,
    /// Extra slack added to a panic deadline before escalating to seed/Bulletin.
    pub fallback_slack_ms: u64,
}

impl Default for PickerWeights {
    fn default() -> Self {
        Self {
            w_d: 1.0,
            w_r: 0.3,
            beta: 4.0,
            panic_horizon_ms: 3_000,
            fallback_slack_ms: 500,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct MeshConfig {
    pub mode: Mode,
    pub role: Role,
    /// Buffer window `W` in segments (default 16 ≈ 32 s at 2 s segments).
    pub window: u32,
    /// Scheduler tick interval (default 100 ms).
    pub tick: Duration,
    /// Nominal segment duration in ms (default 2000).
    pub seg_ms: u64,
    /// Upload budget in bits/sec (publisher/seed high; viewer mid; 0 = download-only).
    pub upload_budget_bps: u64,
    pub weights: PickerWeights,
}

impl Default for MeshConfig {
    fn default() -> Self {
        Self {
            mode: Mode::Live,
            role: Role::Viewer,
            window: 16,
            tick: Duration::from_millis(100),
            seg_ms: 2_000,
            upload_budget_bps: 0,
            weights: PickerWeights::default(),
        }
    }
}
