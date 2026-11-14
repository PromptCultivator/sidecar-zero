"""The prebuilt codecs (sdk-contract §2).

- ``Bytes(response_len=None)``: any buffer-protocol object in, the response's ``bytes`` out.
  It carries opaque bytes, so it binds only to a sidecar whose model registry is empty (the
  echo and chaos workers). ``response_len`` makes any other response length a decode failure.
- ``DenseF32ToScore(n, input="features", output="score", strict=False)``: a model with input
  ``features: float32[rows, n]`` and output ``score: float32[rows, 1]``. A one-dimensional
  input of ``n`` numbers is answered with a ``float``; a ``(rows, n)`` input with a float32
  array (NumPy's when NumPy is installed, else ``array.array('f')``). Raw little-endian
  float32 ``bytes`` are taken as rows. Other numeric types are converted during the copy
  unless ``strict``; lists work, slowly. A wrong shape falls back with ``CODEC_ERROR``.
"""

from ._core import Bytes, DenseF32ToScore

__all__ = ["Bytes", "DenseF32ToScore"]
