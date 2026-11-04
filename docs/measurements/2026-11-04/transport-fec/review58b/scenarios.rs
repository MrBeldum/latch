pub fn all() -> Vec<Scenario> {
    let mut v = Vec::new();
    for rtt in [10u64, 50, 150, 250] {
        v.push(Scenario { name: leak(format!("clean_resume_rtt{rtt}")), fps: 60, ceiling: 20000, caps: &[(0, 100000)], fifo_ms: 40, rtt, interval: 50, loss: 0.0, burst_enter: 0.0, jitter_ms: 0, legacy: false, content: Content::IdleThenMotion { at: 5000 }, spike: None, idr_at: &[], secs: 12, seed: 17 });
    }
    for rtt in [10u64, 50] {
        v.push(Scenario { name: leak(format!("desktop_2s_idle_1s_motion_rtt{rtt}")), fps: 60, ceiling: 20000, caps: &[(0, 100000)], fifo_ms: 40, rtt, interval: 50, loss: 0.0, burst_enter: 0.0, jitter_ms: 0, legacy: false, content: Content::Desktop { idle: 2000, motion: 1000 }, spike: None, idr_at: &[], secs: 35, seed: 17 });
    }
    v.push(Scenario { name: "sparse_idr_rtt20", fps: 60, ceiling: 20000, caps: &[(0, 100000)], fifo_ms: 40, rtt: 20, interval: 50, loss: 0.0, burst_enter: 0.0, jitter_ms: 0, legacy: false, content: Content::Sparse { kbps: 500, idle_until: 3000 }, spike: None, idr_at: &[(8000, 120_000), (15000, 120_000)], secs: 20, seed: 17 });
    v.push(Scenario { name: "sparse_spike_rtt20", fps: 60, ceiling: 20000, caps: &[(0, 100000)], fifo_ms: 40, rtt: 20, interval: 50, loss: 0.0, burst_enter: 0.0, jitter_ms: 0, legacy: false, content: Content::Sparse { kbps: 500, idle_until: 3000 }, spike: Some((6000, 6150, 8.0)), idr_at: &[(8000, 120_000)], secs: 12, seed: 17 });
    v.push(Scenario { name: "resume_spike_rtt20", fps: 60, ceiling: 20000, caps: &[(0, 100000)], fifo_ms: 40, rtt: 20, interval: 50, loss: 0.0, burst_enter: 0.0, jitter_ms: 0, legacy: false, content: Content::IdleThenMotion { at: 5000 }, spike: Some((5100, 5250, 8.0)), idr_at: &[], secs: 12, seed: 17 });
    for (rtt, fifo) in [(100u64, 40u64), (250, 40), (250, 200)] {
        for x in [8000u32, 20000, 40000, 70000] {
            let caps: &'static [(u64, u32)] = Box::leak(vec![(0, 100000), (2500, x)].into_boxed_slice());
            v.push(Scenario { name: leak(format!("overshoot_rtt{rtt}_fifo{fifo}_to{x}")), fps: 120, ceiling: 100000, caps, fifo_ms: fifo, rtt, interval: 50, loss: 0.0, burst_enter: 0.0, jitter_ms: 0, legacy: false, content: Content::IdleThenMotion { at: 5000 }, spike: None, idr_at: &[], secs: 15, seed: 17 });
        }
    }
    for (name, legacy, interval, loss, jitter) in [("legacy1000_wifi3", true, 1000u64, 0.03, 4u64), ("legacy1000_wifi1", true, 1000, 0.01, 4), ("legacy1000_clean_jitter", true, 1000, 0.0, 4), ("v51_250_wifi3", false, 250, 0.03, 4), ("v50_wifi3", true, 1000, 0.03, 4), ("v50_wifi8", true, 1000, 0.08, 4), ("v50_clean", true, 1000, 0.0, 4)] {
        v.push(Scenario { name, fps: 120, ceiling: 50000, caps: &[(0, 50000)], fifo_ms: 40, rtt: 10, interval, loss, burst_enter: if loss > 0.0 { 0.002 } else { 0.0 }, jitter_ms: jitter, legacy, content: Content::Motion, spike: None, idr_at: &[], secs: 35, seed: 17 });
    }
    for (rtt, fifo) in [(250u64, 200u64), (100, 40)] {
        v.push(Scenario { name: leak(format!("desktop_cap20_ceiling100_rtt{rtt}_fifo{fifo}")), fps: 120, ceiling: 100000, caps: &[(0, 20000)], fifo_ms: fifo, rtt, interval: 50, loss: 0.0, burst_enter: 0.0, jitter_ms: 0, legacy: false, content: Content::Desktop { idle: 2000, motion: 1000 }, spike: None, idr_at: &[], secs: 35, seed: 17 });
    }
    v
}
fn leak(s: String) -> &'static str { Box::leak(s.into_boxed_str()) }

#[test]
fn review58b() {
    let only = std::env::var("R58B").ok();
    let verbose = std::env::var("R58B_V").is_ok();
    for s in all() {
        if only.as_deref().is_some_and(|o| !s.name.contains(o)) { continue; }
        let o = run(&s);
        report(&s, &o, verbose);
    }
}
