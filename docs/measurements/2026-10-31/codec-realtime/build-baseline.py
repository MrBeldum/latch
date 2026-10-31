#!/usr/bin/env python3
"""Rebuild the initial encoder with the final, paced benchmark harness.
Usage: python3 build-baseline.py /tmp/latch-codec-before
Then cargo [zig]build --release --manifest-path /tmp/latch-codec-before/Cargo.toml --example encode_bench ...
"""
import pathlib
import subprocess
import sys

root = pathlib.Path(sys.argv[1])
revision = 'e1b42db'
for name in subprocess.check_output(['git', 'ls-tree', '-r', '--name-only', revision, 'crates/codec'], text=True).splitlines():
    destination = root / pathlib.Path(name).relative_to('crates/codec')
    destination.parent.mkdir(parents=True, exist_ok=True)
    destination.write_bytes(subprocess.check_output(['git', 'show', f'{revision}:{name}']))
(root / 'Cargo.toml').write_text('''[package]
name="latch-codec"
version="5.0.1"
edition="2021"
[profile.release]
lto="thin"
codegen-units=1
opt-level=3
panic="abort"
strip=true
''')
harness = pathlib.Path('crates/codec/examples/encode_bench.rs').read_text()
harness = harness.replace('.with_frame_budget(get("--budget", "65").parse()?),', ',')
start = harness.index('                e.timing().effort,')
end = harness.index('\n            );', start)
harness = harness[:start] + '                0, 0., 0., 0, ""\n' + harness[end:]
(root / 'examples/encode_bench.rs').write_text(harness)
