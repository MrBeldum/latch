#!/usr/bin/env python3
"""Run the preserved reviewer scenarios in this checkout, then remove scratch targets."""
import os
from pathlib import Path
import subprocess

here = Path(__file__).resolve().parent
root = here.parents[4]
created = []
try:
    command = ["cargo", "test", "--release", "-p", "latch-engine"]
    for name in ["review58c", "review_controller", "review_overhead"]:
        target = root / "crates/engine/tests" / (name + ".rs")
        with target.open("x") as output:
            output.write((here / (name + ".rs")).read_text())
        created.append(target)
        command += ["--test", name]
    subprocess.run(command + ["--", "--nocapture"], cwd=root, check=True,
                   env=dict(os.environ, TMPDIR="/tmp"))
finally:
    for target in created:
        target.unlink()
