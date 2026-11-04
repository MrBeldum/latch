#!/usr/bin/env python3
"""Run this branch's transport simulations against an older revision.

usage: compare.py main|head <extracted-old-tree> <this-checkout>

Appends a shim to the old tree's congestion.rs exposing that revision's own
production policy behind the harness API (encoder share = budget, pacing at
the budget when pressured, no keyframe pacing, its own video_budget, FEC
percent capped at 30% plus, for 42adc9e, its burst floor), copies this
branch's steady_transport.rs and transport_scenarios.rs into it, and turns
their assertions into printed SOFTFAIL lines so failing cases still report.
"main" is 3d5d69e (no burst FEC); "head" is 42adc9e.
"""
import re, sys

rev, root, src = sys.argv[1:4]
cong = f"{root}/crates/engine/src/congestion.rs"
s = open(cong).read()
if rev == "main":
    vb, burst = "video_budget(w, f, a)", "0"
else:
    vb = "video_budget(w, f, b, fps, a)"
    burst = """{
            self.burst = latch_proto::video::fec_loss_sample(
                self.burst,
                if c.pacing() || report.unrecovered_loss_pct.is_some_and(|l| l > 10.0) { 0.0 } else { f32::from(report.loss_burst_packets) },
                elapsed,
            );
            self.burst.round() as u8
        }"""
s += f"""
// ---- comparison shim: this revision's production policy behind the new harness API ----
#[allow(dead_code)]
impl Controller {{
    pub(crate) fn encoder_wire_budget(&self) -> u32 {{ self.budget() }}
    // This revision's pacer multiplies by 2.5 itself.
    pub(crate) fn pace_kbps(&self) -> u32 {{ if self.pacing() {{ self.budget() }} else {{ 0 }} }}
    // No special keyframe pacing in this revision.
    pub(crate) fn protected_pace_kbps(&self) -> u32 {{ self.pace_kbps() }}
}}
#[allow(dead_code)]
pub(crate) fn video_budget6(w: u32, f: u8, b: u8, fps: u32, a: bool, _p: bool) -> u32 {{ let _ = (b, fps); {vb} }}
#[allow(dead_code)]
#[derive(Default)]
pub(crate) struct FecPolicy {{ loss: f32, burst: f32 }}
#[allow(dead_code)]
impl FecPolicy {{
    pub(crate) fn update(&mut self, report: &Report, c: &Controller, elapsed: Duration) -> (u8, u8) {{
        let _ = self.burst;
        self.loss = latch_proto::video::fec_loss_sample(self.loss, report.loss_pct, elapsed);
        let burst = {burst};
        (latch_proto::video::fec_percent_for(self.loss, c.base_rtt()).min(30), burst)
    }}
}}
"""
open(cong, "w").write(s)
SOFT = """macro_rules! soft { ($c:expr $(,)?) => { if !$c { println!("SOFTFAIL {}", stringify!($c)); } }; ($c:expr, $($a:tt)+) => { if !$c { println!("SOFTFAIL {}", format!($($a)+)); } }; }
macro_rules! soft_eq { ($a:expr, $b:expr $(,)?) => { if $a != $b { println!("SOFTFAIL {} != {}", stringify!($a), stringify!($b)); } }; ($a:expr, $b:expr, $($m:tt)+) => { if $a != $b { println!("SOFTFAIL {}", format!($($m)+)); } }; }
"""
for t in ["steady_transport.rs", "transport_scenarios.rs"]:
    h = open(f"{src}/crates/engine/tests/{t}").read()
    h = h.replace("congestion::video_budget(", "congestion::video_budget6(")
    if rev == "main":
        h = h.replace("tx.fec_burst()", "0")
        h = re.sub(r"\n\s*tx\.set_fec_burst\(burst\);", "\n                let _ = burst;", h)
        h = re.sub(r"\n\s*loss_burst_packets: rx\.take_loss_burst\(\),", "", h)
    else:
        h = h.replace("tx.fec_burst()", "burst_now")
        h = h.replace("tx.set_fec_burst(burst);", "tx.set_fec_burst(burst);\n                burst_now = burst;")
        h = h.replace("let mut fec = congestion::FecPolicy::default();",
                      "let mut fec = congestion::FecPolicy::default();\n        let mut burst_now = 0u8;")
    h = re.sub(r"\bassert_eq!\(", "soft_eq!(", h)
    h = re.sub(r"\bassert!\(", "soft!(", h)
    h = SOFT + h.replace("//!", "//")
    open(f"{root}/crates/engine/tests/{t}", "w").write(h)
