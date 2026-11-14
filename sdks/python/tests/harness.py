"""What the SDK's tests share: a real supervisor in a scratch directory, and the chaos worker's
request format (conformance/proto). Binaries come from SCZ_BINS (default target/stable/bin),
models from SCZ_MODELS (default target/stable/models)."""

import os
import pathlib
import shutil
import signal
import struct
import subprocess
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parents[3]
BINS = pathlib.Path(os.environ.get("SCZ_BINS", ROOT / "target/stable/bin"))
MODELS = pathlib.Path(os.environ.get("SCZ_MODELS", ROOT / "target/stable/models"))


def _runner_bins():
    """A supervisor with the runner's flags (--worker-arg, --worker-read, --worker-socketpair):
    SCZ_RUNNER_BINS, else the snapshot if it has them, else test.sh's own build."""
    if "SCZ_RUNNER_BINS" in os.environ:
        return pathlib.Path(os.environ["SCZ_RUNNER_BINS"])
    return BINS if has_runner_flags(BINS / "scz-supervisor") else ROOT / "target/python-sup/release"


def has_runner_flags(sup):
    """Whether this supervisor parses --worker-socketpair (an older one calls it unknown)."""
    if not sup.exists():
        return False
    r = subprocess.run([str(sup), "--worker-socketpair"], capture_output=True, text=True, timeout=10)
    return "unknown argument" not in r.stderr


RUNNER_BINS = _runner_bins()

M64 = (1 << 64) - 1
MAGIC = int.from_bytes(b"SCZCHAOS", "little")
NORMAL, DELAY, CRASH, HANG, STATUS, BAD_LEN = range(6)


def splitmix64(x):
    x = (x + 0x9E3779B97F4A7C15) & M64
    x = ((x ^ (x >> 30)) * 0xBF58476D1CE4E5B9) & M64
    x = ((x ^ (x >> 27)) * 0x94D049BB133111EB) & M64
    return x ^ (x >> 31)


def request(op, arg, thread, seq, seed=1, rows=0):
    n = (thread << 32) | seq
    words = [MAGIC, op, arg, n, seed] + [splitmix64(seed ^ n ^ i) for i in range(rows)]
    return struct.pack("<%dQ" % len(words), *words)


def fnv1a64(b):
    h = 0xCBF29CE484222325
    for x in b:
        h = ((h ^ x) * 0x100000001B3) & M64
    return h


def answer(req):
    """What the chaos worker answers: 8 bytes, little-endian."""
    return fnv1a64(req).to_bytes(8, "little")


class Sidecar:
    """A supervisor with `worker`, on a socket in a fresh directory."""

    def __init__(self, worker="scz-worker-chaos", args=(), env=None, bins=None):
        self.bins = bins = pathlib.Path(bins or BINS)
        worker = pathlib.Path(worker) if os.path.isabs(worker) else bins / worker
        for b in (bins / "scz-supervisor", worker):
            if not b.exists():
                raise RuntimeError("%s is not built (SCZ_BINS=%s)" % (b, BINS))
        self.dir = pathlib.Path(tempfile.mkdtemp(prefix="scz-py-"))
        self.sock = str(self.dir / "scz.sock")
        self.admin = str(self.dir / "admin.sock")
        args = list(args)
        if not any(a.startswith("--hb-stale-ms") for a in args):
            args.append("--hb-stale-ms=100")
        self.log = open(self.dir / "supervisor.log", "ab")
        e = dict(os.environ)
        e.update(env or {})
        self.proc = subprocess.Popen(
            [str(bins / "scz-supervisor"), "--socket=" + self.sock, "--worker=" + str(worker), "--admin=" + self.admin] + args,
            stdout=subprocess.DEVNULL,
            stderr=self.log,
            env=e,
        )

    def status(self):
        r = subprocess.run([str(self.bins / "sczctl"), "--admin=" + self.admin, "status"], capture_output=True, text=True)
        return r.stdout

    def wait_healthy(self, timeout=20.0):
        t = time.monotonic()
        while '"state":"HEALTHY"' not in self.status():
            if time.monotonic() - t > timeout:
                raise TimeoutError("the gate did not open: " + self.status())
            time.sleep(0.01)

    def signal(self, sig):
        self.proc.send_signal(sig)

    def stop(self):
        if self.proc.poll() is None:
            self.proc.send_signal(signal.SIGCONT)
            self.proc.kill()
            self.proc.wait()
        self.log.close()
        shutil.rmtree(self.dir, ignore_errors=True)

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.stop()
        return False
