//! Unit tests for the ABR rules.

use dash_rs::abr::{
    abandon_requests::AbandonRequestsRule,
    buffer::BufferOccupancyRule,
    decision::AbrDecisionReason,
    dropped_frames::DroppedFramesRule,
    insufficient_buffer::InsufficientBufferRule,
    rule::{AbrRule, RepresentationInfo},
    switch_history::SwitchHistoryRule,
    throughput::ThroughputRule,
    AbrController, AbrMetrics,
};

// ── Helpers ───────────────────────────────────────────────────────────────────

fn reps() -> Vec<RepresentationInfo> {
    vec![
        RepresentationInfo {
            id: "low".into(),
            bandwidth: 500_000,
            width: Some(640),
            height: Some(360),
        },
        RepresentationInfo {
            id: "mid".into(),
            bandwidth: 1_500_000,
            width: Some(1280),
            height: Some(720),
        },
        RepresentationInfo {
            id: "high".into(),
            bandwidth: 4_000_000,
            width: Some(1920),
            height: Some(1080),
        },
    ]
}

// ── ThroughputRule ────────────────────────────────────────────────────────────

#[test]
fn throughput_rule_selects_highest_fitting_quality() {
    let rule = ThroughputRule::default();
    let reps = reps();

    // 6 Mbps: should select "high" (4 Mbps * 1/0.9 ≈ 4.4 Mbps — fits).
    let metrics = AbrMetrics::new(6_000_000.0, 15_000.0);
    assert_eq!(rule.max_index(&reps, &metrics), Some(2));

    // 1.8 Mbps: usable = 1.62 Mbps. "mid" at 1.5 Mbps fits; "high" at 4 Mbps does not.
    let metrics = AbrMetrics::new(1_800_000.0, 15_000.0);
    assert_eq!(rule.max_index(&reps, &metrics), Some(1));

    // 400 kbps: only "low" (500 kbps) would exceed usable (360 kbps), falls back to lowest.
    let metrics = AbrMetrics::new(400_000.0, 15_000.0);
    assert_eq!(rule.max_index(&reps, &metrics), Some(0));
}

#[test]
fn throughput_rule_empty_representations() {
    let rule = ThroughputRule::default();
    let metrics = AbrMetrics::new(10_000_000.0, 10_000.0);
    assert_eq!(rule.max_index(&[], &metrics), None);
}

// ── BufferOccupancyRule ───────────────────────────────────────────────────────

#[test]
fn buffer_rule_abstains_when_no_buffer() {
    let rule = BufferOccupancyRule::default();
    let reps = reps();
    // Buffer == 0 → abstain so ThroughputRule drives the first few segments.
    let metrics = AbrMetrics::new(10_000_000.0, 0.0);
    assert_eq!(rule.max_index(&reps, &metrics), None);
}

#[test]
fn buffer_rule_returns_some_when_buffer_present() {
    let rule = BufferOccupancyRule::default();
    let reps = reps();
    let metrics = AbrMetrics::new(5_000_000.0, 10_000.0);
    // Just verify it returns Some — the exact index depends on BOLA parameters.
    assert!(rule.max_index(&reps, &metrics).is_some());
}

// ── InsufficientBufferRule ────────────────────────────────────────────────────

#[test]
fn insufficient_buffer_fires_below_threshold() {
    let rule = InsufficientBufferRule::default();
    let reps = reps();

    // Exactly at threshold (2 000 ms): forces lowest quality.
    let metrics = AbrMetrics::new(10_000_000.0, 2_000.0);
    assert_eq!(rule.max_index(&reps, &metrics), Some(0));

    // Just below threshold.
    let metrics = AbrMetrics::new(10_000_000.0, 500.0);
    assert_eq!(rule.max_index(&reps, &metrics), Some(0));
}

#[test]
fn insufficient_buffer_abstains_above_threshold() {
    let rule = InsufficientBufferRule::default();
    let reps = reps();

    // Well above the 2 000 ms threshold.
    let metrics = AbrMetrics::new(10_000_000.0, 8_000.0);
    assert_eq!(rule.max_index(&reps, &metrics), None);
}

// ── SwitchHistoryRule ─────────────────────────────────────────────────────────

#[test]
fn switch_history_clamps_after_recent_switch() {
    let mut rule = SwitchHistoryRule::default();
    let reps = reps();
    let mut metrics = AbrMetrics::new(10_000_000.0, 15_000.0);
    metrics.current_index = Some(1); // currently at "mid"

    // Record a switch right now.
    rule.record_switch();

    // Should clamp to current index because the switch was too recent.
    assert_eq!(rule.max_index(&reps, &metrics), Some(1));
}

#[test]
fn switch_history_abstains_with_no_history() {
    let rule = SwitchHistoryRule::default();
    let reps = reps();
    let metrics = AbrMetrics::new(10_000_000.0, 15_000.0);
    // No switches recorded → no opinion.
    assert_eq!(rule.max_index(&reps, &metrics), None);
}

// ── AbrController ─────────────────────────────────────────────────────────────

#[test]
fn controller_picks_conservative_result() {
    let controller = AbrController::default();
    let reps = reps();

    // Insufficient buffer fires (≤ 2 s) → should override throughput and pick 0.
    let metrics = AbrMetrics::new(10_000_000.0, 1_500.0);
    let decision = controller.select(&reps, &metrics);
    assert_eq!(decision.index, 0);
}

#[test]
fn controller_selects_high_with_good_conditions() {
    let controller = AbrController::default();
    let reps = reps();

    // 10 Mbps throughput, 20 s buffer — should comfortably pick "high".
    let metrics = AbrMetrics::new(10_000_000.0, 20_000.0);
    let decision = controller.select(&reps, &metrics);
    assert_eq!(decision.index, 2);
}

#[test]
fn controller_empty_representations() {
    let controller = AbrController::default();
    let metrics = AbrMetrics::new(10_000_000.0, 10_000.0);
    let decision = controller.select(&[], &metrics);
    assert_eq!(decision.index, 0);
}

// ── DroppedFramesRule ─────────────────────────────────────────────────────────

#[test]
fn dropped_frames_rule_fires_above_threshold() {
    let rule = DroppedFramesRule::default();
    let reps = reps();

    // 15 % drop rate — above the 10 % threshold.
    let mut metrics = AbrMetrics::new(10_000_000.0, 15_000.0);
    metrics.dropped_frames = Some(150);
    metrics.total_decoded_frames = Some(1_000);

    assert_eq!(rule.max_index(&reps, &metrics), Some(0));
}

#[test]
fn dropped_frames_rule_abstains_below_threshold() {
    let rule = DroppedFramesRule::default();
    let reps = reps();

    // 5 % drop rate — below the 10 % threshold.
    let mut metrics = AbrMetrics::new(10_000_000.0, 15_000.0);
    metrics.dropped_frames = Some(50);
    metrics.total_decoded_frames = Some(1_000);

    assert_eq!(rule.max_index(&reps, &metrics), None);
}

#[test]
fn dropped_frames_rule_abstains_when_no_data() {
    let rule = DroppedFramesRule::default();
    let reps = reps();
    // Neither field set — rule must abstain.
    let metrics = AbrMetrics::new(10_000_000.0, 15_000.0);
    assert_eq!(rule.max_index(&reps, &metrics), None);
}

#[test]
fn dropped_frames_rule_abstains_when_zero_total_frames() {
    let rule = DroppedFramesRule::default();
    let reps = reps();
    let mut metrics = AbrMetrics::new(10_000_000.0, 15_000.0);
    metrics.dropped_frames = Some(10);
    metrics.total_decoded_frames = Some(0); // guard against divide-by-zero
    assert_eq!(rule.max_index(&reps, &metrics), None);
}

// ── AbandonRequestsRule ───────────────────────────────────────────────────────

#[test]
fn abandon_rule_fires_below_multiplier() {
    let rule = AbandonRequestsRule::default();
    let reps = reps();

    // Effective ratio 0.3 — below the 0.5 multiplier.
    let mut metrics = AbrMetrics::new(1_000_000.0, 10_000.0);
    metrics.effective_download_ratio = Some(0.3);

    assert_eq!(rule.max_index(&reps, &metrics), Some(0));
}

#[test]
fn abandon_rule_abstains_above_multiplier() {
    let rule = AbandonRequestsRule::default();
    let reps = reps();

    // Effective ratio 0.8 — above the threshold.
    let mut metrics = AbrMetrics::new(1_000_000.0, 10_000.0);
    metrics.effective_download_ratio = Some(0.8);

    assert_eq!(rule.max_index(&reps, &metrics), None);
}

#[test]
fn abandon_rule_abstains_when_no_ratio() {
    let rule = AbandonRequestsRule::default();
    let reps = reps();
    // Field not set — rule must abstain.
    let metrics = AbrMetrics::new(1_000_000.0, 10_000.0);
    assert_eq!(rule.max_index(&reps, &metrics), None);
}

#[test]
fn controller_drops_frames_overrides_throughput() {
    let controller = AbrController::default();
    let reps = reps();

    // Plenty of bandwidth but heavy frame drops → should force index 0.
    let mut metrics = AbrMetrics::new(10_000_000.0, 20_000.0);
    metrics.dropped_frames = Some(200);
    metrics.total_decoded_frames = Some(1_000); // 20 % — above threshold

    let decision = controller.select(&reps, &metrics);
    assert_eq!(decision.index, 0);
}

// ── SwitchHistoryRule — cooldown expiry ───────────────────────────────────────

#[test]
fn switch_history_abstains_after_cooldown_elapsed() {
    let mut rule = SwitchHistoryRule::default();
    // Set a very short cooldown so the test doesn't take seconds.
    rule.min_switch_interval = std::time::Duration::from_millis(5);
    let reps = reps();
    let metrics = AbrMetrics::new(10_000_000.0, 15_000.0);

    rule.record_switch();

    // Let the cooldown expire.
    std::thread::sleep(std::time::Duration::from_millis(10));

    // After the interval has elapsed the rule should abstain (return None).
    assert_eq!(rule.max_index(&reps, &metrics), None);
}

// ── AbrController — record_switch clamps subsequent select ───────────────────

#[test]
fn controller_record_switch_then_select_respects_history() {
    let mut controller = AbrController::default();
    let reps = reps();

    // Record a switch right now — switch history should clamp to current_index.
    controller.record_switch();

    // With 10 Mbps + 20 s buffer, ThroughputRule alone would return index 2.
    // But the switch-history cooldown (8 s) should clamp the result to current_index.
    let mut metrics = AbrMetrics::new(10_000_000.0, 20_000.0);
    metrics.current_index = Some(1); // currently at "mid"

    let decision = controller.select(&reps, &metrics);
    // Must not exceed the current quality level during the cooldown window.
    assert!(
        decision.index <= 1,
        "expected index ≤ 1 during switch cooldown, got {}",
        decision.index
    );
}

// ── AbandonRequestsRule — exact-boundary abstains ────────────────────────────

#[test]
fn abandon_rule_at_threshold_boundary_abstains() {
    let rule = AbandonRequestsRule::default();
    let reps = reps();

    // ABANDON_MULTIPLIER = 0.5; the check is `ratio < 0.5` (strict less-than),
    // so at exactly 0.5 the rule must abstain.
    let mut metrics = AbrMetrics::new(1_000_000.0, 10_000.0);
    metrics.effective_download_ratio = Some(0.5);

    assert_eq!(rule.max_index(&reps, &metrics), None);
}

// ── AbrController — empty representations return AbrDecisionReason::Initial ───

#[test]
fn abr_controller_empty_representations_returns_initial_reason() {
    let controller = AbrController::default();
    let metrics = AbrMetrics::new(10_000_000.0, 15_000.0);

    let decision = controller.select(&[], &metrics);
    assert_eq!(decision.index, 0);
    assert!(
        matches!(decision.reason, AbrDecisionReason::Initial),
        "expected Initial reason for empty representations, got {:?}",
        decision.reason
    );
}

// ── SwitchHistoryRule — ring buffer capacity cap ───────────────────────────────

#[test]
fn switch_history_capacity_capped_at_4() {
    let mut rule = SwitchHistoryRule::default();
    let reps = reps();
    let mut metrics = AbrMetrics::new(10_000_000.0, 15_000.0);
    metrics.current_index = Some(1);

    // Record 6 switches — only the last 4 should be retained (HISTORY_CAPACITY=4).
    for _ in 0..6 {
        rule.record_switch();
    }

    // Should not panic, and the most-recent switch is fresh → clamp active.
    let result = rule.max_index(&reps, &metrics);
    assert_eq!(
        result,
        Some(1),
        "should clamp to current_index after recent switch"
    );
}

// ── InsufficientBufferRule — exact threshold boundary ────────────────────────

#[test]
fn insufficient_buffer_at_exact_threshold_fires() {
    let rule = InsufficientBufferRule::default();
    let reps = reps();

    // The threshold is ≤ 2 000 ms → exactly 2 000 must trigger the rule.
    let metrics = AbrMetrics::new(10_000_000.0, 2_000.0);
    assert_eq!(
        rule.max_index(&reps, &metrics),
        Some(0),
        "buffer exactly at threshold should force lowest quality"
    );
}

// ── DroppedFramesRule — exact 10% boundary abstains ──────────────────────────

#[test]
fn dropped_frames_rule_at_exact_threshold_abstains() {
    let rule = DroppedFramesRule::default();
    let reps = reps();

    // The threshold is > 10 % (strict greater-than), so exactly 10 % abstains.
    let mut metrics = AbrMetrics::new(10_000_000.0, 15_000.0);
    metrics.dropped_frames = Some(100);
    metrics.total_decoded_frames = Some(1_000); // exactly 10 %

    assert_eq!(
        rule.max_index(&reps, &metrics),
        None,
        "exactly 10% drop rate should abstain (strict > threshold)"
    );
}
