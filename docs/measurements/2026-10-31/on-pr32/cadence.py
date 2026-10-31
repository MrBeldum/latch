#!/usr/bin/env python3
"""Isolate both original schedulers with identical synthetic per-frame work."""
import pathlib
import subprocess
import tempfile

root = pathlib.Path(__file__).resolve().parents[4]
old = subprocess.check_output(['git', 'show', 'e49efe8:crates/engine/src/cadence.rs'], cwd=root, text=True)
old = old.split('#[cfg(test)]')[0]
new = (root / 'crates/engine/src/video.rs').read_text()
new = new[new.index('/// When frames are taken:'):new.index('/// What this machine can encode')]
program = '''use std::time::{Instant,Duration};
mod old { OLD }
mod new { use std::time::{Instant,Duration}; NEW }
mod priority { PRIORITY }
fn main() {
 priority::media_thread();
 println!("scheduler,work_ms,fps,interval_sd_ms,interval_p99_ms");
 for cost in [3, 20] {
  for name in ["cadence", "Pacer"] {
   let mut old = old::Cadence::new(60);
   let mut new = new::Pacer::new(60);
   let mut times = Vec::new();
   for _ in 0..120 {
    if name == "cadence" { old.wait(); } else { new.wait(); new.taken(Instant::now()); }
    let work_start = Instant::now();
    while work_start.elapsed() < Duration::from_millis(cost) { std::hint::spin_loop(); }
    times.push(Instant::now());
   }
   let mut gaps: Vec<f64> = times.windows(2).map(|w| (w[1]-w[0]).as_secs_f64()*1000.).collect();
   let mean = gaps.iter().sum::<f64>()/gaps.len() as f64;
   let sd = (gaps.iter().map(|g| (g-mean).powi(2)).sum::<f64>()/gaps.len() as f64).sqrt();
   gaps.sort_by(f64::total_cmp);
   println!("{name},{cost},{:.3},{sd:.3},{:.3}", 1000./mean, gaps[(gaps.len() as f64*0.99) as usize]);
  }
 }
}
'''.replace('OLD', old).replace('NEW', new).replace('PRIORITY', (root / 'crates/engine/src/priority.rs').read_text())
with tempfile.TemporaryDirectory(prefix='latch-cadence-') as tmp:
    src = pathlib.Path(tmp) / 'probe.rs'
    binary = pathlib.Path(tmp) / 'probe'
    src.write_text(program)
    subprocess.run(['rustc', '-O', '-Awarnings', str(src), '-o', str(binary)], check=True)
    subprocess.run([str(binary)], check=True)
