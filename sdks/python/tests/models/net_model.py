"""A model module that opens a network socket when it is imported: the runner's sandbox must
make that fail loudly, at load (seccomp refuses socket(2))."""

import socket

_s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)

from numpy_model import load, predict, signature  # noqa: E402,F401
