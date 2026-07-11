//! Pure supervisor policy: who gets admitted, who gets evicted, who sleeps, and how
//! the uplink budget splits. No I/O and no clocks — the supervisor feeds elapsed
//! seconds in via [`WorkerView`], so every branch is table-testable.

use std::time::Duration;

/// A released (publisher said stop) worker with no peers left lingers only this long
/// — much shorter than the full `idle_evict`, since nobody is coming back for it.
const RELEASED_IDLE_EVICT: Duration = Duration::from_secs(60);

/// The knobs. Built from env by [`PolicyCfg::from_env`]; `Default` is the same
/// without the env overrides.
pub struct PolicyCfg {
    /// Most streams served at once (env `UNSTATION_NODE_MAX_STREAMS`).
    pub max_streams: usize,
    /// Total uplink donated across all streams (env `UNSTATION_NODE_BUDGET_MBPS`).
    pub total_budget_bps: u64,
    /// Floor per stream — below this a swarm gets more churn than help from us.
    pub min_stream_budget_bps: u64,
    /// Evict a non-pinned worker after this long at zero peers.
    pub idle_evict: Duration,
    /// Evict a non-pinned worker whose cached head stopped advancing for this long.
    pub stall_evict: Duration,
    /// A fresh worker whose head NEVER advanced gets this long to find the swarm
    /// before the stall rule applies.
    pub join_grace: Duration,
    /// Park a zero-peer worker's chain polling after this long.
    pub dormant_after: Duration,
    /// Most non-pinned streams one publisher may occupy (recruit-spam containment).
    pub per_publisher_cap: usize,
}

impl Default for PolicyCfg {
    fn default() -> Self {
        Self {
            max_streams: 8,
            total_budget_bps: 50 * 1_000_000,
            min_stream_budget_bps: 4_000_000,
            idle_evict: Duration::from_secs(600),
            stall_evict: Duration::from_secs(60),
            join_grace: Duration::from_secs(120),
            dormant_after: Duration::from_secs(60),
            per_publisher_cap: 2,
        }
    }
}

impl PolicyCfg {
    /// Defaults with the env overrides applied (`UNSTATION_NODE_MAX_STREAMS`,
    /// `UNSTATION_NODE_BUDGET_MBPS`).
    pub fn from_env() -> Self {
        Self {
            max_streams: env_u64("UNSTATION_NODE_MAX_STREAMS", 8) as usize,
            total_budget_bps: env_u64("UNSTATION_NODE_BUDGET_MBPS", 50) * 1_000_000,
            ..Self::default()
        }
    }

    /// How many streams the budget can carry at the per-stream floor. Admission caps
    /// at `min(max_streams, this)` — beyond it a new stream would starve the rest.
    fn budget_capacity(&self) -> usize {
        (self.total_budget_bps / self.min_stream_budget_bps.max(1)) as usize
    }
}

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// What the supervisor knows about one worker at sweep time (durations pre-elapsed
/// so this module stays clock-free).
pub struct WorkerView {
    pub stream: [u8; 32],
    /// The recruiting publisher (None for pins).
    pub publisher: Option<[u8; 32]>,
    pub pinned: bool,
    pub peers: usize,
    pub head_seq: u64,
    pub secs_since_head_advance: u64,
    pub secs_since_spawn: u64,
    /// 0 while any peer is connected.
    pub secs_at_zero_peers: u64,
    /// The publisher posted a Release for this stream (eligible for the short idle
    /// eviction, but never hard-killed — viewers may still be on us).
    pub released: bool,
}

/// Why a worker is being torn down (heartbeat/log label).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvictReason {
    /// Cached head stopped advancing — the publisher is gone or unreachable.
    Stalled,
    /// Zero peers for the full idle window.
    Idle,
    /// Released by its publisher and drained of peers.
    Released,
}

impl std::fmt::Display for EvictReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EvictReason::Stalled => write!(f, "stalled"),
            EvictReason::Idle => write!(f, "idle"),
            EvictReason::Released => write!(f, "released"),
        }
    }
}

/// Per-sweep fate of one worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Serve,
    Dormant,
    Evict(EvictReason),
}

/// Decide one worker's fate. Pinned streams are the operator's explicit choice —
/// they never evict, only park (Dormant) while nobody watches.
pub fn evaluate(v: &WorkerView, cfg: &PolicyCfg) -> Verdict {
    if !v.pinned {
        // Stall: the head stopped advancing. A worker that NEVER saw the head move is
        // still hunting for the swarm — give it `join_grace` before calling it dead.
        let stalled = v.secs_since_head_advance >= cfg.stall_evict.as_secs()
            && (v.head_seq > 0 || v.secs_since_spawn >= cfg.join_grace.as_secs());
        if stalled {
            return Verdict::Evict(EvictReason::Stalled);
        }
        if v.released && v.secs_at_zero_peers >= RELEASED_IDLE_EVICT.as_secs() {
            return Verdict::Evict(EvictReason::Released);
        }
        if v.secs_at_zero_peers >= cfg.idle_evict.as_secs() {
            return Verdict::Evict(EvictReason::Idle);
        }
    }
    if v.secs_at_zero_peers >= cfg.dormant_after.as_secs() {
        Verdict::Dormant
    } else {
        Verdict::Serve
    }
}

/// Admission decision for a verified recruitment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admit {
    Accept,
    /// Room can be made: shut down this (LRU, zero-peer, non-pinned) stream first.
    AcceptEvicting([u8; 32]),
    Reject(&'static str),
}

/// Should we take on `stream` for `publisher`? `views` is every live worker.
pub fn admit(publisher: [u8; 32], stream: [u8; 32], views: &[WorkerView], cfg: &PolicyCfg) -> Admit {
    if views.iter().any(|v| v.stream == stream) {
        // The supervisor treats this as an LRU refresh, not an error.
        return Admit::Reject("already serving");
    }
    let publisher_load =
        views.iter().filter(|v| !v.pinned && v.publisher == Some(publisher)).count();
    if publisher_load >= cfg.per_publisher_cap {
        return Admit::Reject("per-publisher cap");
    }
    // Capacity is the tighter of the operator's stream cap and what the budget can
    // carry at the per-stream floor.
    let capacity = cfg.max_streams.min(cfg.budget_capacity());
    if views.len() < capacity {
        return Admit::Accept;
    }
    // Full: bump the most-idle zero-peer non-pinned worker, if there is one. A worker
    // with peers is doing real work — never displace it for a newcomer.
    let victim = views
        .iter()
        .filter(|v| !v.pinned && v.peers == 0)
        .max_by_key(|v| (v.secs_at_zero_peers, v.secs_since_spawn));
    match victim {
        Some(v) => Admit::AcceptEvicting(v.stream),
        None => Admit::Reject("at capacity, every stream has peers"),
    }
}

/// Split the uplink: streams actually serving peers share the total evenly (never
/// below the floor); zero-peer streams idle at the floor. Returned for every view.
pub fn budgets(views: &[WorkerView], cfg: &PolicyCfg) -> Vec<([u8; 32], u64)> {
    let n_serving = views.iter().filter(|v| v.peers > 0).count() as u64;
    let serving_share = cfg
        .total_budget_bps
        .checked_div(n_serving)
        .map_or(cfg.min_stream_budget_bps, |share| share.max(cfg.min_stream_budget_bps));
    views
        .iter()
        .map(|v| {
            let bps =
                if v.peers > 0 { serving_share } else { cfg.min_stream_budget_bps };
            (v.stream, bps)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> PolicyCfg {
        PolicyCfg::default()
    }

    /// A healthy serving worker: peers connected, head advancing.
    fn view(stream: u8) -> WorkerView {
        WorkerView {
            stream: [stream; 32],
            publisher: Some([0xAA; 32]),
            pinned: false,
            peers: 3,
            head_seq: 100,
            secs_since_head_advance: 1,
            secs_since_spawn: 300,
            secs_at_zero_peers: 0,
            released: false,
        }
    }

    #[test]
    fn evaluate_verdict_table() {
        let c = cfg();
        // (label, view mutation, expected)
        let table: Vec<(&str, WorkerView, Verdict)> = vec![
            ("healthy serving", view(1), Verdict::Serve),
            ("zero peers, under dormant_after", {
                let mut v = view(1);
                v.peers = 0;
                v.secs_at_zero_peers = c.dormant_after.as_secs() - 1;
                v
            }, Verdict::Serve),
            ("zero peers past dormant_after parks", {
                let mut v = view(1);
                v.peers = 0;
                v.secs_at_zero_peers = c.dormant_after.as_secs();
                v
            }, Verdict::Dormant),
            ("stall after the head ever advanced", {
                let mut v = view(1);
                v.secs_since_head_advance = c.stall_evict.as_secs();
                v
            }, Verdict::Evict(EvictReason::Stalled)),
            ("no head yet, inside join_grace: still hunting", {
                let mut v = view(1);
                v.head_seq = 0;
                v.secs_since_head_advance = c.stall_evict.as_secs() + 10;
                v.secs_since_spawn = c.join_grace.as_secs() - 1;
                v
            }, Verdict::Serve),
            ("no head yet, past join_grace: dead on arrival", {
                let mut v = view(1);
                v.head_seq = 0;
                v.secs_since_head_advance = c.join_grace.as_secs() + 1;
                v.secs_since_spawn = c.join_grace.as_secs() + 1;
                v
            }, Verdict::Evict(EvictReason::Stalled)),
            ("idle-evict at zero peers for idle_evict", {
                let mut v = view(1);
                v.peers = 0;
                v.secs_at_zero_peers = c.idle_evict.as_secs();
                v
            }, Verdict::Evict(EvictReason::Idle)),
            ("released + drained goes on the short clock", {
                let mut v = view(1);
                v.peers = 0;
                v.released = true;
                v.secs_at_zero_peers = RELEASED_IDLE_EVICT.as_secs();
                v
            }, Verdict::Evict(EvictReason::Released)),
            ("released but under the short clock survives", {
                let mut v = view(1);
                v.peers = 0;
                v.released = true;
                v.secs_at_zero_peers = RELEASED_IDLE_EVICT.as_secs() - 1;
                v
            }, Verdict::Serve),
            ("released with viewers still on us keeps serving", {
                let mut v = view(1);
                v.released = true;
                v
            }, Verdict::Serve),
            ("pinned never evicts: stalled", {
                let mut v = view(1);
                v.pinned = true;
                v.secs_since_head_advance = c.stall_evict.as_secs() * 10;
                v
            }, Verdict::Serve),
            ("pinned never evicts: long idle parks instead", {
                let mut v = view(1);
                v.pinned = true;
                v.peers = 0;
                v.secs_at_zero_peers = c.idle_evict.as_secs() * 2;
                v
            }, Verdict::Dormant),
        ];
        for (label, v, expected) in table {
            assert_eq!(evaluate(&v, &c), expected, "{label}");
        }
    }

    #[test]
    fn admit_rejects_a_stream_already_served() {
        let views = vec![view(1)];
        assert_eq!(admit([0xBB; 32], [1; 32], &views, &cfg()), Admit::Reject("already serving"));
    }

    #[test]
    fn admit_enforces_the_per_publisher_cap_over_non_pinned_only() {
        let c = cfg();
        let publisher = [0xAA; 32];
        // Two non-pinned streams from the same publisher: at the cap.
        let views = vec![view(1), view(2)];
        assert_eq!(admit(publisher, [3; 32], &views, &c), Admit::Reject("per-publisher cap"));
        // A different publisher is unaffected.
        assert_eq!(admit([0xBB; 32], [3; 32], &views, &c), Admit::Accept);
        // A pinned stream doesn't count against its publisher.
        let mut pinned = view(1);
        pinned.pinned = true;
        let views = vec![pinned, view(2)];
        assert_eq!(admit(publisher, [3; 32], &views, &c), Admit::Accept);
    }

    #[test]
    fn admit_accepts_below_capacity() {
        assert_eq!(admit([0xBB; 32], [9; 32], &[], &cfg()), Admit::Accept);
        let views = vec![view(1)];
        assert_eq!(admit([0xBB; 32], [9; 32], &views, &cfg()), Admit::Accept);
    }

    #[test]
    fn admit_at_max_streams_bumps_the_most_idle_zero_peer_worker() {
        let mut c = cfg();
        c.max_streams = 3;
        let mut idle_a = view(1);
        idle_a.peers = 0;
        idle_a.secs_at_zero_peers = 100;
        let mut idle_b = view(2);
        idle_b.peers = 0;
        idle_b.secs_at_zero_peers = 400; // most idle → the LRU victim
        let views = vec![idle_a, idle_b, view(3)];
        assert_eq!(admit([0xBB; 32], [9; 32], &views, &c), Admit::AcceptEvicting([2; 32]));
    }

    #[test]
    fn admit_victim_tie_breaks_on_age_and_skips_pinned_and_busy() {
        let mut c = cfg();
        c.max_streams = 3;
        // Equal idle time → the older spawn loses.
        let mut a = view(1);
        a.peers = 0;
        a.secs_at_zero_peers = 100;
        a.secs_since_spawn = 500;
        let mut b = view(2);
        b.peers = 0;
        b.secs_at_zero_peers = 100;
        b.secs_since_spawn = 200;
        let views = vec![a, b, view(3)];
        assert_eq!(admit([0xBB; 32], [9; 32], &views, &c), Admit::AcceptEvicting([1; 32]));

        // A pinned idle stream is never the victim, and busy streams are safe: full
        // house of (pinned idle, busy, busy) → reject.
        let mut pinned_idle = view(1);
        pinned_idle.pinned = true;
        pinned_idle.peers = 0;
        pinned_idle.secs_at_zero_peers = 9999;
        let views = vec![pinned_idle, view(2), view(3)];
        assert_eq!(
            admit([0xBB; 32], [9; 32], &views, &c),
            Admit::Reject("at capacity, every stream has peers")
        );
    }

    #[test]
    fn admit_caps_at_what_the_budget_can_carry() {
        // 8 Mbps total at a 4 Mbps floor carries two streams, whatever max_streams says.
        let mut c = cfg();
        c.max_streams = 8;
        c.total_budget_bps = 8_000_000;
        let views = vec![view(1), view(2)];
        assert_eq!(
            admit([0xBB; 32], [9; 32], &views, &c),
            Admit::Reject("at capacity, every stream has peers")
        );
    }

    #[test]
    fn budgets_split_evenly_among_serving_with_a_floor_for_idlers() {
        let c = cfg(); // 50 Mbps total, 4 Mbps floor
        let mut idle = view(3);
        idle.peers = 0;
        let out = budgets(&[view(1), view(2), idle], &c);
        let get = |s: u8| out.iter().find(|(id, _)| id == &[s; 32]).unwrap().1;
        assert_eq!(get(1), 25_000_000, "two serving streams split the total");
        assert_eq!(get(2), 25_000_000);
        assert_eq!(get(3), 4_000_000, "an idle stream sits at the floor");
    }

    #[test]
    fn budgets_never_drop_a_serving_stream_below_the_floor() {
        let mut c = cfg();
        c.total_budget_bps = 6_000_000; // ÷ 3 serving = 2 Mbps < 4 Mbps floor
        let out = budgets(&[view(1), view(2), view(3)], &c);
        assert!(out.iter().all(|(_, bps)| *bps == c.min_stream_budget_bps));
    }

    #[test]
    fn budgets_with_nobody_serving_puts_everyone_at_the_floor() {
        let c = cfg();
        let mut idle = view(1);
        idle.peers = 0;
        let out = budgets(&[idle], &c);
        assert_eq!(out, vec![([1; 32], c.min_stream_budget_bps)]);
        assert!(budgets(&[], &c).is_empty());
    }

    #[test]
    fn single_serving_stream_gets_the_whole_budget() {
        let c = cfg();
        let out = budgets(&[view(1)], &c);
        assert_eq!(out, vec![([1; 32], c.total_budget_bps)]);
    }
}
