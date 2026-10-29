"""Create host preferences once; never overwrite a damaged or edited file."""
import os
from pathlib import Path
import tempfile
import tomllib

def atomic_write(path, content):
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    fd, tmp = tempfile.mkstemp(dir=path.parent, prefix=path.name + ".")
    try:
        with os.fdopen(fd, "w") as out:
            out.write(content)
            out.flush()
            os.fsync(out.fileno())
        os.replace(tmp, path)
    finally:
        if os.path.exists(tmp):
            os.unlink(tmp)


def configure(host):
    if host.exists():
        tomllib.loads(host.read_text())
        return
    atomic_write(host, "power_allowed = true\nstart_with_windows = true\nstay_awake = true\naccept_updates = true\n")


if __name__ == "__main__":
    configure(Path("/root/.local/share/latch/host.toml"))
