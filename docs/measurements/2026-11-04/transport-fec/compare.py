#!/usr/bin/env python3
"""Run this branch's transport simulations against an older revision.

usage: compare.py main|e2fad22 <extracted-old-tree> <this-checkout>

Appends a shim to the old tree's congestion.rs exposing that revision's own
production policy behind the harness API, copies this branch's
steady_transport.rs and transport_scenarios.rs into it, and turns their
assertions into printed SOFTFAIL lines so failing cases still report.
Neither old revision sends probe trains (`probe_train` is always None).
"main" is ee3176a (engine as 3d5d69e: no burst FEC, pacer at the budget
when pressured, its own video_budget, FEC percent capped at 30%).
"e2fad22" is the first review-fix round: its encoder share
(`encoder_wire_budget`, the aged idle trust) sizes frames, and its own
pacer rate, FecPolicy and reservation are used as they are.
"""
import re, sys

rev, root, src = sys.argv[1:4]
cong = f"{root}/crates/engine/src/congestion.rs"
s = open(cong).read()
shim = """
// ---- comparison shim: this revision's production policy behind the new harness API ----
#[allow(dead_code)]
impl Controller {
    pub(crate) fn probe_train(&mut self, _now: Instant) -> Option<u8> { None }
    pub(crate) fn frame_budget(&self) -> u32 { SIZE }
}
"""
if rev == "main":
    s += shim.replace("SIZE", "self.budget()") + """
#[allow(dead_code)]
impl Controller {
    // This revision's pacer multiplies by 2.5 itself.
    pub(crate) fn pace_kbps(&self) -> u32 { if self.pacing() { self.budget() } else { 0 } }
}
#[allow(dead_code)]
pub(crate) fn video_budget6(w: u32, f: u8, _b: u8, _fps: u32, a: bool, _p: bool) -> u32 { video_budget(w, f, a) }
#[allow(dead_code)]
#[derive(Default)]
pub(crate) struct FecPolicy { loss: f32 }
#[allow(dead_code)]
impl FecPolicy {
    pub(crate) fn update(&mut self, report: &Report, c: &Controller, elapsed: Duration) -> (u8, u8) {
        self.loss = latch_proto::video::fec_loss_sample(self.loss, report.loss_pct, elapsed);
        (latch_proto::video::fec_percent_for(self.loss, c.base_rtt()).min(30), 0)
    }
}
"""
else:
    s += shim.replace("SIZE", "self.encoder_wire_budget()") + """
#[allow(dead_code)]
pub(crate) fn video_budget6(w: u32, f: u8, b: u8, fps: u32, a: bool, p: bool) -> u32 { video_budget(w, f, b, fps, a, p) }
"""
open(cong, "w").write(s)
SOFT = """macro_rules! soft { ($c:expr $(,)?) => { if !$c { println!("SOFTFAIL {}", stringify!($c)); } }; ($c:expr, $($a:tt)+) => { if !$c { println!("SOFTFAIL {}", format!($($a)+)); } }; }
macro_rules! soft_eq { ($a:expr, $b:expr $(,)?) => { if $a != $b { println!("SOFTFAIL {} != {}", stringify!($a), stringify!($b)); } }; ($a:expr, $b:expr, $($m:tt)+) => { if $a != $b { println!("SOFTFAIL {}", format!($($m)+)); } }; }
"""
for t in ["steady_transport.rs", "transport_scenarios.rs"]:
    h = open(f"{src}/crates/engine/tests/{t}").read()
    # Frames are sized by the revision's encoder share.
    h = re.sub(r"congestion::video_budget\(\s*controller\.budget\(\)",
               "congestion::video_budget6(controller.frame_budget()", h)
    h = h.replace("congestion::video_budget(", "congestion::video_budget6(")
    # The old protocol crates have no probe trains: drop sender and meter.
    h = re.sub(r"\n\s*let mut (meter|train|probe) = [^;]*;", "", h)
    h = re.sub(r"\n            if let Some\(count\) = controller\.probe_train\(now\) \{.*?\n            \}",
               "", h, flags=re.S)
    h = re.sub(r"\n\s*Ok\(Datagram::Probe \{.*?\}\) => meter\.push\([^)]*\),", "", h, flags=re.S)
    h = re.sub(r"\n\s*probe_kbps: meter\.take_kbps\(now\),", "", h)
    if rev == "main":
        h = h.replace("tx.fec_burst()", "0")
        h = re.sub(r"\n\s*tx\.set_fec_burst\(burst\);", "\n                let _ = burst;", h)
        h = re.sub(r"\n\s*loss_burst_packets: rx\.take_loss_burst\(\),", "", h)
    h = re.sub(r"\bassert_eq!\(", "soft_eq!(", h)
    h = re.sub(r"\bassert!\(", "soft!(", h)
    h = SOFT + h.replace("//!", "//")
    open(f"{root}/crates/engine/tests/{t}", "w").write(h)
