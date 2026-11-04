#!/usr/bin/env python3
"""Prepare a standalone baseline crate; no checkout/worktree changes.
Run from the repo root, then cargo build --release --example transport using
--manifest-path /tmp/latch-gfn/b-baseline/Cargo.toml (or cargo zigbuild).
"""
import pathlib
import subprocess

out = pathlib.Path('/tmp/latch-gfn/b-baseline')
(out / 'src').mkdir(parents=True, exist_ok=True)
(out / 'examples').mkdir(exist_ok=True)
for p in pathlib.Path('crates/proto/src').glob('*.rs'):
    (out / 'src' / p.name).write_bytes(subprocess.check_output(['git', 'show', '9fe0ae9:' + str(p)]))
(out / 'Cargo.toml').write_text('''[package]
name = "latch-proto"
version = "0.0.0"
edition = "2021"
[dependencies]
socket2 = "=0.6.5"
serde = { version = "=1.0.229", features = ["derive"] }
serde_json = "=1.0.151"
[target.'cfg(unix)'.dependencies]
libc = "=0.2.189"
''')
s = pathlib.Path('crates/proto/examples/transport.rs').read_text()
a = s.index('            tx.packetize_batches(')
b = s.index('            frames +=', a)
s = s[:a] + '''            tx.packetize(&frame, |dg| match udp.send_to(dg, peer) {
                Ok(_) => packets += 1,
                Err(_) => errors += 1,
            });
''' + s[b:]
(out / 'examples' / 'transport.rs').write_text(s)
p = out / 'src' / 'transport.rs'
p.write_text(p.read_text() + '''
pub struct ReceiveBatch { buffer: [u8; crate::MAX_DATAGRAM + 1] }
impl Default for ReceiveBatch { fn default() -> Self { Self { buffer: [0; crate::MAX_DATAGRAM + 1] } } }
impl ReceiveBatch {
    pub fn receive(&mut self, udp: &UdpSocket, mut emit: impl FnMut(&[u8], std::net::SocketAddr)) -> io::Result<usize> {
        let (n, from) = udp.recv_from(&mut self.buffer)?;
        emit(&self.buffer[..n], from); Ok(1)
    }
}
''')
print(out / 'Cargo.toml')
# Compare the original controller with the current deterministic harness, on
# the same 50-ms feedback schedule. Keep the original parity policy as well.
(out / 'congestion.rs').write_bytes(subprocess.check_output(['git', 'show', '9fe0ae9:crates/engine/src/congestion.rs']))
s = pathlib.Path('crates/engine/tests/transport_scenarios.rs').read_text()
s = s.replace('#[path = "../src/congestion.rs"]', '#[allow(dead_code)]\n#[path = "../congestion.rs"]')
s = s.replace('#[test]\nfn transport_scenarios()', 'fn main()')
s = s[:s.index('        assert!(p95')] + '    }\n}\n'
a = s.index('                fec_loss =')
b = s.index('                tx.set_fec_percent', a)
s = s[:a] + s[b:]
s = s.replace('fec_percent_for(fec_loss, controller.base_rtt())', 'fec_percent_for(report.loss_pct, controller.base_rtt())')
s = s.replace('        let mut fec_loss = 0.0;', '').replace('        let mut minimum = ceiling;', '')
s = s.replace('            minimum = minimum.min(controller.budget());', '')
(out / 'examples' / 'scenarios.rs').write_text(s)

# A read-only backlog accessor for the harness; no baseline link behavior changes.
p = out / 'src' / 'sim.rs'
s = p.read_text().replace('    fn random(&mut self)', '    pub fn queue_delay(&self, now: Instant) -> Duration {\n        self.serialized_until.map_or(Duration::ZERO, |at| at.saturating_duration_since(now))\n    }\n    fn random(&mut self)')
p.write_text(s)
