#[path = "../src/congestion.rs"]
mod congestion;
use latch_proto::control::Report;
use std::time::{Duration, Instant};
#[test]
fn underfilled_policed_stream() {
    let start = Instant::now();
    let mut c = congestion::Controller::new(100000, start);
    for tick in 0..600 {
        let sent = c.budget().min(10000);
        let received = sent.min(5000);
        let loss = 100.0 * (sent - received) as f32 / sent as f32;
        c.observe_sent(sent);
        c.update(
            start + Duration::from_millis(tick * 50),
            &Report {
                rtt_ms: 10.0,
                received_kbps: received,
                loss_pct: loss,
                unrecovered_loss_pct: Some(loss),
                frames_lost: u64::from(loss > 0.0),
                window_ms: 50,
                ..Default::default()
            },
            10.0,
            0.0,
        );
        if tick % 100 == 99 {
            println!(
                "POLICER ms={} sent={} recv={} loss={} budget={}",
                tick * 50,
                sent,
                received,
                loss,
                c.budget()
            );
        }
    }
    assert!(
        (4500..=5250).contains(&c.budget()),
        "underfilled policer: {}",
        c.budget()
    );
}
#[test]
fn mixed_random_policer() {
    let start = Instant::now();
    let mut c = congestion::Controller::new(100000, start);
    for tick in 0..1200 {
        let sent = c.budget();
        let delivered = sent.min(90000) * 9 / 10;
        let raw = (sent - delivered) as f32 * 100.0 / sent as f32;
        c.observe_sent(sent);
        c.update(
            start + Duration::from_millis(tick * 50),
            &Report {
                rtt_ms: 10.0,
                received_kbps: delivered,
                loss_pct: raw,
                unrecovered_loss_pct: Some(4.0),
                frames_lost: 1,
                window_ms: 50,
                ..Default::default()
            },
            10.0,
            0.0,
        );
        if tick % 100 == 99 {
            println!(
                "MIXED ms={} sent={} raw={} budget={}",
                tick * 50,
                sent,
                raw,
                c.budget()
            );
        }
    }
    assert!(
        (85000..=91000).contains(&c.budget()),
        "mixed policer: {}",
        c.budget()
    );
}

#[test]
fn underfilled_moderate_loss_probe_reduces_actual_offer() {
    let start = Instant::now();
    let mut c = congestion::Controller::new(100_000, start);
    let mut probed = false;
    for tick in 0..160 {
        let sent = c.budget().min(10_000);
        probed |= sent <= 8000;
        let received = sent.min(9200);
        let loss = (sent - received) as f32 * 100.0 / sent as f32;
        c.observe_sent(sent);
        c.update(
            start + Duration::from_millis(tick * 50),
            &Report {
                rtt_ms: 10.0,
                received_kbps: received,
                loss_pct: loss,
                unrecovered_loss_pct: Some(loss),
                frames_lost: u64::from(loss > 0.0),
                window_ms: 50,
                ..Default::default()
            },
            10.0,
            0.0,
        );
    }
    assert!(probed, "a budget-only probe never changes the load");
    assert!((8500..=9400).contains(&c.budget()), "{}", c.budget());
}
