"""``python -m sidecarzero.worker MODULE``: run MODULE's model as a SidecarZero worker. The
supervisor appends its own arguments (--ctl-fd=3 ... --beat-slot=N); this never returns."""

import sys

from .._core import _worker_run

_worker_run(sys.argv[1:])
