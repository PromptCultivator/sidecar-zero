# SidecarZero chaos benchmark report

> **Environment Note:** This run ran in a virtual machine, ran in quick mode (reduced rates, durations, kills, and repetitions), CPU governor `unavailable`, not `performance`, no core isolation or static CPU manager. Virtualized CPU scheduling and shared cores add tail jitter to sub-millisecond measurements.

## 1. Environment

| | |
|---|---|
| kernel | `6.12.76-linuxkit` |
| arch | `aarch64` |
| cpu | `0x000` |
| cpus | `10` |
| governor | `unavailable` |
| cgroup | `v2` |
| vm | `True` |
| onnxruntime | `1.31.1` |
| triton | `nvcr.io/nvidia/tritonserver@sha256:aa5fca20416939f243d587d56557d15f9d2bb81de30c6a4464707225250320fb` |
| commit | `974eda0` |
| dirty | `True` |
| repetitions | 1 |
| kills per kill-loop run | 10 |
| inference time, measured first (bench §2) | fraud 8.6 µs, MLP 14.8 µs |
| started | 2026-11-13T20:44:28Z |
| finished | 2026-11-13T21:12:11Z |

## 2. Claims

| Claim | This run | Verdict |
|---|---|---|
| Model in service through kills — SidecarZero | availability 99.964%; time in fallback per kill: median 739.4 µs, max 915.0 µs; fallbacks per kill median 3 | reported |
| Model in service through kills — Triton gRPC + system SHM | availability 98.240%; time in fallback per kill: median 38.0 ms, max 49.9 ms; fallbacks per kill median 199.5 | reported |
| Model in service through kills — HTTP/JSON | availability 98.753%; time in fallback per kill: median 25.5 ms, max 37.4 ms; fallbacks per kill median 135.0 | reported |
| Model in service through kills — Unix socket, binary | availability 98.590%; time in fallback per kill: median 28.0 ms, max 58.1 ms; fallbacks per kill median 147.5 | reported |
| No request lost to a kill — SidecarZero | errors and timeouts 0 with kills, 0 in the control run | **pass** |
| No request lost to a kill — Triton gRPC + system SHM | errors and timeouts 0 with kills, 0 in the control run | **pass** |
| No request lost to a kill — HTTP/JSON | errors and timeouts 0 with kills, 0 in the control run | **pass** |
| No request lost to a kill — Unix socket, binary | errors and timeouts 0 with kills, 0 in the control run | **pass** |
| Call after the gate flips falls back at once (p99.9 < 1 µs) | 7 admission fallbacks (fewer than 10,000: no p99.9); max 0.5 µs, p50 0.2 µs | reported |
| Kill to HEALTHY with a warm standby (p99 < 5 ms) | 9 kills; median 232.4 µs, max 1136.8 µs (fewer than 1,000 kills: no p99) | reported |
| Calls in flight at a kill are bounded by their deadline | slowest call 8792.7 µs against a 2000.0 µs budget (+ fallback, + timer slack) | reported |
| Transport overhead, spinning, 30,000/s (p50 < 1 µs, p99 < 10 µs) | A less the worker's mean inference (9.2 µs): p50 0.2 µs, p99 130.1 µs; A − E: p50 2.8 µs; send lag p99 30.2 µs | **FAIL** |
| Transport overhead, parked, 2,000/s | A less the worker's mean inference (40.6 µs): p50 49.5 µs, p99 7102.8 µs; A − E: p50 82.0 µs; send lag p99 8060.9 µs | reported |
| An idle pod costs little (< 2% of one core) | sidecar CPU 2.00% of one core | **pass** |
| No allocation per call — java host, A | 48 B over 300 calls; 40 B over 20,000 calls; 0 B over 300,000 calls; 0 B over 301,146 calls | **pass** |
| No allocation per call — rust host, A | 0 B over 300 calls; 0 B over 20,000 calls; 0 B over 300,000 calls; 0 B over 301,146 calls | **pass** |
| Overload fails fast (shed calls < 1 µs at p99) | at 101,023/s: 0 shed calls, 7 deadline misses; time to fallback p99 —; send lag p99 13287.6 ms | invalid here: the load loop was late |
| A reload with an unchanged schema costs nothing (0 fallbacks) | 5 reloads; 1 fallbacks within 1 s of one, where the run's rate elsewhere predicts 1.2 | **pass** |
| Supervisor under 5 MB (stripped, static) | 0.86 MB on aarch64 | **pass** |
| Kubernetes does not notice a kill — SidecarZero (default) | 0 endpoint changes, 0 container restarts | **pass** |
| Kubernetes does not notice a kill — HTTP/JSON (default) | 10 endpoint changes, 4 container restarts | reported |
| Kubernetes does not notice a kill — HTTP/JSON (supervised) | 0 endpoint changes, 0 container restarts | reported |
| Kubernetes does not notice a kill — Unix socket, binary (default) | 9 endpoint changes, 4 container restarts | reported |
| Kubernetes does not notice a kill — Unix socket, binary (supervised) | 0 endpoint changes, 0 container restarts | reported |
| Kubernetes does not notice a kill — In-process (default) | 9 endpoint changes, 4 container restarts | reported |

"Reported" rows have no pass mark in bench §1, or compare systems. A pass needs the sample count bench §5 asks for; with fewer, the row gives the count and the maximum instead.

## 3. Latency by scenario

Latency from the intended send time (open loop), merged across repetitions; in parentheses the range of each repetition's median where there were several.

| Host | System | Variant | Scenario | Rate | Arrival | Calls | Model | p50 | p90 | p99 | p99.9 | max | Send lag p99 | B/call |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| java | A | default | control | 5000 | even | 115000 | 99.99% | 44.5 µs | 53.8 µs | 401.4 µs | 2719.7 µs | 8596.6 µs | 4.3 µs | 0.0 |
| java | A | default | freeze | 5000 | even | 50000 | 99.94% | 44.0 µs | 53.8 µs | 405.5 µs | 5832.7 µs | 8815.8 µs | 4.5 µs | 0.0 |
| java | A | default | kill | 5000 | even | 115000 | 99.88% | 44.5 µs | 60.9 µs | 417.8 µs | 5701.6 µs | 25.3 ms | 7.7 µs | 0.0 |
| java | A | default | overload | 101023 | even | 1010230 | 67.51% | 6308.2 ms | 8455.7 ms | 8724.2 ms | 8724.2 ms | 8751.6 ms | 8724.2 ms ⚠ | 0.0 |
| java | A | default | reload | 5000 | even | 150000 | 99.83% | 44.0 µs | 53.2 µs | 282.6 µs | 6815.7 µs | 22.7 ms | 4.1 µs | 0.0 |
| java | A | default | steady | 10 | even | 300 | 100.00% | 852.0 µs | 3997.7 µs | n<1000 | n<10000 | 4880.1 µs | 3538.9 µs ⚠ | 0.2 |
| java | A | default | steady | 2000 | even | 20000 | 99.98% | 48.6 µs | 103.4 µs | 520.2 µs | 2293.8 µs | 5789.8 µs | 7.5 µs | 0.0 |
| java | A | default | steady | 30000 | even | 300000 | 100.00% | 9.7 µs | 11.1 µs | 153.6 µs | 14.5 ms | 22.2 ms | 36.9 µs | 0.0 |
| java | A | default | steady | 30000 | poisson | 301146 | 100.00% | 20.2 µs | 70.7 µs | 126.0 µs | 10.5 ms | 18.4 ms | 92.2 µs ⚠ | 0.0 |
| java | A | nospin | steady | 30000 | even | 300000 | 100.00% | 42.0 µs | 62.0 µs | 3309.6 µs | 18.9 ms | 21.7 ms | 3178.5 µs ⚠ | 0.0 |
| java | A | nostandby | kill | 5000 | even | 115000 | 49.63% | 15.5 µs | 50.2 µs | 137.2 µs | 6160.4 µs | 14.9 ms | 4.3 µs | 0.0 |
| java | C | default | control | 5000 | even | 115000 | 99.99% | 74.8 µs | 90.1 µs | 118.8 µs | 446.5 µs | 2505.8 µs | 0.9 µs | 256.5 |
| java | C | default | freeze | 5000 | even | 50000 | 99.66% | 86.0 µs | 1258.3 ms | 2063.6 ms | 2130.7 ms | 2147.1 ms | 2046.8 ms ⚠ | 286.3 |
| java | C | default | kill | 5000 | even | 115000 | 98.76% | 71.7 µs | 90.1 µs | 139.3 µs | 3178.5 µs | 10.1 ms | 1.2 µs | 284.0 |
| java | C | default | steady | 2000 | even | 20000 | 99.87% | 72.7 µs | 94.2 µs | 909.3 µs | 9044.0 µs | 16.1 ms | 13.1 µs | 257.0 |
| java | C | default | steady | 30000 | even | 300000 | 99.99% | 65.5 µs | 84.0 µs | 13.2 ms | 22.8 ms | 23.9 ms | 13.1 ms ⚠ | 256.5 |
| java | C | default | steady | 30000 | poisson | 301146 | 99.99% | 85.0 µs | 206.8 µs | 13.9 ms | 25.4 ms | 28.8 ms | 13.6 ms ⚠ | 256.7 |
| java | D | default | control | 5000 | even | 115000 | 100.00% | 79.9 µs | 89.1 µs | 94.2 µs | 409.6 µs | 3851.4 µs | 0.8 µs | 0.1 |
| java | D | default | freeze | 5000 | even | 50000 | 95.33% | 81.9 µs | 880.8 ms | 1543.5 ms | 1610.6 ms | 1623.1 ms | 1543.5 ms ⚠ | 104.7 |
| java | D | default | kill | 5000 | even | 115000 | 98.58% | 71.7 µs | 91.1 µs | 573.4 µs | 9044.0 µs | 26.3 ms | 57.3 µs ⚠ | 17.1 |
| java | D | default | steady | 2000 | even | 20000 | 100.00% | 75.8 µs | 90.1 µs | 110.6 µs | 299.0 µs | 2508.8 µs | 1.9 µs | 0.0 |
| java | D | default | steady | 30000 | even | 300000 | 100.00% | 56.8 µs | 75.8 µs | 126.0 µs | 3833.9 µs | 9240.0 µs | 8.2 µs | 0.0 |
| java | D | default | steady | 30000 | poisson | 301146 | 100.00% | 67.6 µs | 131.1 µs | 274.4 µs | 10.6 ms | 13.4 ms | 215.0 µs ⚠ | 0.0 |
| rust | A | default | control | 5000 | even | 115000 | 98.64% | 46.1 µs | 51.2 µs | 149.5 µs | 20.4 ms | 46.3 ms | 0.1 µs | 0.0 |
| rust | A | default | freeze | 5000 | even | 50000 | 99.85% | 45.6 µs | 50.2 µs | 141.3 µs | 6619.1 µs | 10.9 ms | 0.1 µs | 0.0 |
| rust | A | default | kill | 5000 | even | 115000 | 99.96% | 45.6 µs | 55.3 µs | 340.0 µs | 3735.6 µs | 8792.7 µs | 4.0 µs | 0.0 |
| rust | A | default | overload | 101023 | even | 1010230 | 100.00% | 8187.3 ms | 12348.0 ms | 13287.6 ms | 13287.6 ms | 13407.0 ms | 13287.6 ms ⚠ | 0.0 |
| rust | A | default | reload | 5000 | even | 150000 | 100.00% | 47.6 µs | 59.4 µs | 137.2 µs | 1802.2 µs | 5554.4 µs | 0.1 µs | 0.0 |
| rust | A | default | steady | 10 | even | 300 | 99.67% | 475.1 µs | 4063.2 µs | n<1000 | n<10000 | 12.7 ms | 3866.6 µs ⚠ | 0.0 |
| rust | A | default | steady | 2000 | even | 20000 | 99.72% | 90.1 µs | 121.9 µs | 9830.4 µs | 72.4 ms | 89.4 ms | 8060.9 µs ⚠ | 0.0 |
| rust | A | default | steady | 30000 | even | 300000 | 100.00% | 9.3 µs | 10.9 µs | 139.3 µs | 12.6 ms | 18.4 ms | 30.2 µs | 0.0 |
| rust | A | default | steady | 30000 | poisson | 301146 | 100.00% | 21.0 µs | 57.3 µs | 909.3 µs | 12.8 ms | 20.9 ms | 794.6 µs ⚠ | 0.0 |
| rust | A | nospin | steady | 30000 | even | 300000 | 99.99% | 40.4 µs | 54.8 µs | 2261.0 µs | 27.5 ms | 30.2 ms | 2129.9 µs ⚠ | 0.0 |
| rust | A | nostandby | kill | 5000 | even | 115000 | 49.46% | 3.5 µs | 49.2 µs | 157.7 µs | 5046.3 µs | 22.0 ms | 0.1 µs | 0.0 |
| rust | B | default | control | 5000 | even | 115000 | 99.95% | 117.8 µs | 143.4 µs | 1687.6 µs | 61.3 ms | 76.3 ms | 909.3 µs ⚠ | 28114.8 |
| rust | B | default | freeze | 5000 | even | 50000 | 95.46% | 118.8 µs | 947.9 ms | 1543.5 ms | 1610.6 ms | 1623.6 ms | 1543.5 ms ⚠ | 31262.5 |
| rust | B | default | kill | 5000 | even | 115000 | 98.24% | 118.8 µs | 153.6 µs | 417.8 µs | 3801.1 µs | 10.4 ms | 0.1 µs | 28014.7 |
| rust | B | default | steady | 2000 | even | 20000 | 99.64% | 120.8 µs | 163.8 µs | 2555.9 µs | 19.7 ms | 26.5 ms | 1032.2 µs ⚠ | 32461.3 |
| rust | B | default | steady | 30000 | even | 300000 | 100.00% | 197.1 ms | 364.9 ms | 385.9 ms | 390.1 ms | 393.0 ms | 385.9 ms ⚠ | 32297.5 |
| rust | B | default | steady | 30000 | poisson | 301146 | 100.00% | 369.1 ms | 604.0 ms | 721.4 ms | 771.8 ms | 779.8 ms | 721.4 ms ⚠ | 32232.3 |
| rust | C | default | control | 5000 | even | 115000 | 99.91% | 73.7 µs | 93.2 µs | 4063.2 µs | 24.6 ms | 30.4 ms | 3342.3 µs ⚠ | 136.7 |
| rust | C | default | freeze | 5000 | even | 50000 | 99.69% | 76.8 µs | 1258.3 ms | 2063.6 ms | 2130.7 ms | 2152.9 ms | 2063.6 ms ⚠ | 179.1 |
| rust | C | default | kill | 5000 | even | 115000 | 98.75% | 72.7 µs | 90.1 µs | 219.1 µs | 1900.5 µs | 6589.0 µs | 0.1 µs | 137.9 |
| rust | C | default | steady | 2000 | even | 20000 | 99.95% | 75.8 µs | 102.4 µs | 368.6 µs | 9699.3 µs | 19.3 ms | 2.8 µs | 159.0 |
| rust | C | default | steady | 30000 | even | 300000 | 99.92% | 72.7 µs | 99.3 µs | 671.1 ms | 696.3 ms | 704.1 ms | 671.1 ms ⚠ | 159.6 |
| rust | C | default | steady | 30000 | poisson | 301146 | 99.99% | 111.6 µs | 462.8 µs | 77.6 ms | 101.7 ms | 106.8 ms | 77.6 ms ⚠ | 156.1 |
| rust | D | default | control | 5000 | even | 115000 | 99.97% | 59.4 µs | 75.8 µs | 141.3 µs | 4718.6 µs | 11.1 ms | 0.1 µs | 0.0 |
| rust | D | default | freeze | 5000 | even | 50000 | 94.38% | 60.9 µs | 847.2 ms | 1459.6 ms | 1526.7 ms | 1540.5 ms | 1459.6 ms ⚠ | 0.0 |
| rust | D | default | kill | 5000 | even | 115000 | 98.59% | 60.4 µs | 79.9 µs | 274.4 µs | 3637.2 µs | 10.8 ms | 0.1 µs | 0.0 |
| rust | D | default | steady | 2000 | even | 20000 | 99.92% | 60.9 µs | 78.8 µs | 503.8 µs | 3735.6 µs | 10.1 ms | 0.2 µs | 0.0 |
| rust | D | default | steady | 30000 | even | 300000 | 100.00% | 55.3 µs | 81.9 µs | 161.8 µs | 20.7 ms | 21.8 ms | 31.2 µs | 0.0 |
| rust | D | default | steady | 30000 | poisson | 301146 | 99.99% | 72.7 µs | 157.7 µs | 6684.7 µs | 19.9 ms | 21.7 ms | 6553.6 µs ⚠ | 0.0 |
| rust | E | default | steady | 10 | even | 300 | 100.00% | 1048.6 µs | 3702.8 µs | n<1000 | n<10000 | 3933.4 µs | 3473.4 µs ⚠ | 0.0 |
| rust | E | default | steady | 2000 | even | 20000 | 100.00% | 8.1 µs | 11.6 µs | 64.0 µs | 3964.9 µs | 12.5 ms | 0.2 µs | 0.0 |
| rust | E | default | steady | 30000 | even | 300000 | 100.00% | 6.5 µs | 8.7 µs | 25.3 µs | 3571.7 µs | 8514.8 µs | 0.1 µs | 0.0 |
| rust | E | default | steady | 30000 | poisson | 301146 | 100.00% | 6.7 µs | 9.2 µs | 26.6 µs | 6422.5 µs | 17.9 ms | 8.0 µs | 0.0 |

⚠: the load loop sent its calls more than 50.0 µs late at p99 in that row, so its latencies measure this machine's scheduling, not the system.


## 4. Kill loops

| Host | System | Variant | Kills | Model share | Fallbacks per kill (median, max) | Time in fallback per kill (median, max) | Errors + timeouts | Fallback reasons |
|---|---|---|---|---|---|---|---|---|
| java | A | default | 9 | 99.877% | 2, 122 | 714.4 µs, 873.7 µs | 0 | STALE_HEARTBEAT 119, WORKER_FAILED 13, DEADLINE 6, GATE_DEAD 3 |
| java | A | nostandby | 8 | 49.626% | 3150.5, 25160 | 629.5 ms, 5032.0 ms | 0 | GATE_DEAD 57907, DEADLINE 12, WORKER_FAILED 11 |
| java | C | default | 10 | 98.760% | 144.0, 177 | 27.5 ms, 31.9 ms | 0 | TRANSPORT_ERROR 1410, TRANSPORT_TIMEOUT 16 |
| java | D | default | 10 | 98.580% | 161.5, 234 | 29.8 ms, 42.5 ms | 0 | TRANSPORT_ERROR 1564, TRANSPORT_TIMEOUT 69 |
| rust | A | default | 9 | 99.964% | 3, 11 | 739.4 µs, 915.0 µs | 0 | DEADLINE 20, WORKER_FAILED 14, GATE_DEAD 7 |
| rust | A | nostandby | 8 | 49.460% | 3189.5, 25221 | 630.3 ms, 5044.3 ms | 0 | GATE_DEAD 58036, STALE_HEARTBEAT 74, DEADLINE 6, WORKER_FAILED 5 |
| rust | B | default | 10 | 98.240% | 199.5, 264 | 38.0 ms, 49.9 ms | 0 | TRANSPORT_ERROR 1984, TRANSPORT_TIMEOUT 40 |
| rust | C | default | 10 | 98.753% | 135.0, 193 | 25.5 ms, 37.4 ms | 0 | TRANSPORT_ERROR 1414, TRANSPORT_TIMEOUT 20 |
| rust | D | default | 10 | 98.590% | 147.5, 304 | 28.0 ms, 58.1 ms | 0 | TRANSPORT_ERROR 1582, TRANSPORT_TIMEOUT 39 |

### Recovery intervals, system A (supervisor events against the kill agent's clock)

| Variant | Kills | Kill → first death lock | Lock → gate closed and swept | Gate closed → HEALTHY | Kill → HEALTHY | Kill → exit report |
|---|---|---|---|---|---|---|
| default | 9 | median 122.8 µs, max 1032.2 µs | median 43.5 µs, max 62.2 µs | median 95.8 µs, max 233.7 µs | median 232.4 µs, max 1136.8 µs | median 2005.5 µs, max 3589.0 µs |
| nostandby | 8 | median 102.8 µs, max 142.3 µs | median 47.1 µs, max 86.4 µs | median 630.2 ms, max 5044.0 ms | median 630.4 ms, max 5044.2 ms | median 2838.9 µs, max 4117.3 µs |

## 5. Service mode (Kind)

| System | Variant | Scenario | Rate | Calls | Model share | Errors + timeouts | Endpoint changes | Container restarts | p50 | p99 |
|---|---|---|---|---|---|---|---|---|---|---|
| SidecarZero | default | control | 1000 | 25000 | 100.00% | 0 | 0 | 0 | 78.8 µs | 520.2 µs |
| SidecarZero | default | kill | 1000 | 25000 | 100.00% | 0 | 0 | 0 | 78.8 µs | 446.5 µs |
| HTTP/JSON | default | control | 1000 | 25000 | 99.93% | 0 | 0 | 0 | 114.7 µs | 1097.7 µs |
| HTTP/JSON | default | kill | 1000 | 25000 | 31.32% | 0 | 10 | 4 | 49.7 µs | 884.7 µs |
| HTTP/JSON | supervised | control | 1000 | 25000 | 100.00% | 0 | 0 | 0 | 116.7 µs | 942.1 µs |
| HTTP/JSON | supervised | kill | 1000 | 25000 | 99.72% | 0 | 0 | 0 | 114.7 µs | 1032.2 µs |
| Unix socket, binary | default | control | 1000 | 25000 | 99.95% | 0 | 0 | 0 | 99.3 µs | 1228.8 µs |
| Unix socket, binary | default | kill | 1000 | 25000 | 32.16% | 0 | 9 | 4 | 48.6 µs | 688.1 µs |
| Unix socket, binary | supervised | control | 1000 | 25000 | 100.00% | 0 | 0 | 0 | 96.3 µs | 1228.8 µs |
| Unix socket, binary | supervised | kill | 1000 | 25000 | 99.62% | 0 | 0 | 0 | 97.3 µs | 1294.3 µs |
| In-process | default | control | 1000 | 25000 | 100.00% | 0 | 0 | 0 | 57.3 µs | 2195.5 µs |
| In-process | default | kill | 1000 | 25000 | 87.28% | 3181 | 9 | 4 | 127.0 µs | 9529.5 ms |

## 6. CPU and memory

| Host | System | Variant | Scenario | Rate | Host CPU per million calls | Server CPU per million calls |
|---|---|---|---|---|---|---|
| java | A | default | steady | 10 | 2400.0 s | 3266.7 s |
| java | A | default | steady | 2000 | 2594.5 s | 120.0 s |
| java | A | default | steady | 30000 | 172.9 s | 43.8 s |
| java | A | nospin | steady | 30000 | 125.5 s | 24.6 s |
| java | C | default | steady | 2000 | 2524.5 s | 45.0 s |
| java | C | default | steady | 30000 | 108.1 s | 41.2 s |
| java | D | default | steady | 2000 | 2521.0 s | 43.5 s |
| java | D | default | steady | 30000 | 116.2 s | 39.4 s |
| rust | A | default | steady | 10 | 1061.4 s | 3000.0 s |
| rust | A | default | steady | 2000 | 2536.7 s | 148.5 s |
| rust | A | default | steady | 30000 | 171.8 s | 43.9 s |
| rust | A | nospin | steady | 30000 | 125.5 s | 24.0 s |
| rust | B | default | steady | 2000 | 2440.8 s | 0.0 s |
| rust | B | default | steady | 30000 | 23.6 s | 0.0 s |
| rust | C | default | steady | 2000 | 2499.3 s | 51.0 s |
| rust | C | default | steady | 30000 | 107.9 s | 50.1 s |
| rust | D | default | steady | 2000 | 2536.4 s | 38.5 s |
| rust | D | default | steady | 30000 | 122.3 s | 41.1 s |
| rust | E | default | steady | 10 | 643.1 s | 0.0 s |
| rust | E | default | steady | 2000 | 2599.8 s | 0.0 s |
| rust | E | default | steady | 30000 | 173.4 s | 0.0 s |

Spinning makes CPU follow how closely requests arrive, not how much work is done: A's worker and callers spin through gaps shorter than their spin budget (signaling §4). System A's standby is a second copy of the worker's memory, which the baselines do not have (bench §3, "Variants of A"). Triton's CPU is not counted here: it runs in another container.

## 7. Methodology Notes

- Single-node execution environment; the send-lag column indicates when the open-loop load generator experienced host scheduling delay.
- Synthetic 256-feature fraud model and dataset (models/gen.py) isolating IPC transport and fault recovery from model accuracy.
- Baselines C (HTTP/JSON) and D (Unix socket binary) use dedicated low-overhead Rust servers wrapping ONNX Runtime, and B uses Triton Inference Server 26.04 with registered POSIX shared memory (bench/baselines/triton/README.md).
- In-process mode runs baselines under a supervisor restart loop; service mode evaluates both supervised and default container-restart configurations.
- Transport overhead is computed as SidecarZero's latency minus worker inference time (and compared directly against the in-process ONNX Runtime floor E).
- Percentiles require at least 1,000 samples for p99 and 10,000 samples for p99.9; runs with fewer samples report exact sample counts and maximums.
- Service mode executes inside a single-node Kind cluster.

## 8. Raw data

Every run's JSON is beside this report: [`inproc-java-A-default-control-5000-even-r1.json`](inproc-java-A-default-control-5000-even-r1.json), [`inproc-java-A-default-freeze-5000-even-r1.json`](inproc-java-A-default-freeze-5000-even-r1.json), [`inproc-java-A-default-idle-0-even-r1.json`](inproc-java-A-default-idle-0-even-r1.json), [`inproc-java-A-default-kill-5000-even-r1.json`](inproc-java-A-default-kill-5000-even-r1.json), [`inproc-java-A-default-overload-101023-even-r1.json`](inproc-java-A-default-overload-101023-even-r1.json), [`inproc-java-A-default-reload-5000-even-r1.json`](inproc-java-A-default-reload-5000-even-r1.json), [`inproc-java-A-default-steady-10-even-r1.json`](inproc-java-A-default-steady-10-even-r1.json), [`inproc-java-A-default-steady-2000-even-r1.json`](inproc-java-A-default-steady-2000-even-r1.json), [`inproc-java-A-default-steady-30000-even-r1.json`](inproc-java-A-default-steady-30000-even-r1.json), [`inproc-java-A-default-steady-30000-poisson-r1.json`](inproc-java-A-default-steady-30000-poisson-r1.json), [`inproc-java-A-nospin-steady-30000-even-r1.json`](inproc-java-A-nospin-steady-30000-even-r1.json), [`inproc-java-A-nostandby-kill-5000-even-r1.json`](inproc-java-A-nostandby-kill-5000-even-r1.json), [`inproc-java-C-default-control-5000-even-r1.json`](inproc-java-C-default-control-5000-even-r1.json), [`inproc-java-C-default-freeze-5000-even-r1.json`](inproc-java-C-default-freeze-5000-even-r1.json), [`inproc-java-C-default-kill-5000-even-r1.json`](inproc-java-C-default-kill-5000-even-r1.json), [`inproc-java-C-default-steady-2000-even-r1.json`](inproc-java-C-default-steady-2000-even-r1.json), [`inproc-java-C-default-steady-30000-even-r1.json`](inproc-java-C-default-steady-30000-even-r1.json), [`inproc-java-C-default-steady-30000-poisson-r1.json`](inproc-java-C-default-steady-30000-poisson-r1.json), [`inproc-java-D-default-control-5000-even-r1.json`](inproc-java-D-default-control-5000-even-r1.json), [`inproc-java-D-default-freeze-5000-even-r1.json`](inproc-java-D-default-freeze-5000-even-r1.json), [`inproc-java-D-default-kill-5000-even-r1.json`](inproc-java-D-default-kill-5000-even-r1.json), [`inproc-java-D-default-steady-2000-even-r1.json`](inproc-java-D-default-steady-2000-even-r1.json), [`inproc-java-D-default-steady-30000-even-r1.json`](inproc-java-D-default-steady-30000-even-r1.json), [`inproc-java-D-default-steady-30000-poisson-r1.json`](inproc-java-D-default-steady-30000-poisson-r1.json), [`inproc-rust-A-default-control-5000-even-r1.json`](inproc-rust-A-default-control-5000-even-r1.json), [`inproc-rust-A-default-freeze-5000-even-r1.json`](inproc-rust-A-default-freeze-5000-even-r1.json), [`inproc-rust-A-default-idle-0-even-r1.json`](inproc-rust-A-default-idle-0-even-r1.json), [`inproc-rust-A-default-kill-5000-even-r1.json`](inproc-rust-A-default-kill-5000-even-r1.json), [`inproc-rust-A-default-overload-101023-even-r1.json`](inproc-rust-A-default-overload-101023-even-r1.json), [`inproc-rust-A-default-reload-5000-even-r1.json`](inproc-rust-A-default-reload-5000-even-r1.json), [`inproc-rust-A-default-steady-10-even-r1.json`](inproc-rust-A-default-steady-10-even-r1.json), [`inproc-rust-A-default-steady-2000-even-r1.json`](inproc-rust-A-default-steady-2000-even-r1.json), [`inproc-rust-A-default-steady-30000-even-r1.json`](inproc-rust-A-default-steady-30000-even-r1.json), [`inproc-rust-A-default-steady-30000-poisson-r1.json`](inproc-rust-A-default-steady-30000-poisson-r1.json), [`inproc-rust-A-nospin-steady-30000-even-r1.json`](inproc-rust-A-nospin-steady-30000-even-r1.json), [`inproc-rust-A-nostandby-kill-5000-even-r1.json`](inproc-rust-A-nostandby-kill-5000-even-r1.json), [`inproc-rust-B-default-control-5000-even-r1.json`](inproc-rust-B-default-control-5000-even-r1.json), [`inproc-rust-B-default-freeze-5000-even-r1.json`](inproc-rust-B-default-freeze-5000-even-r1.json), [`inproc-rust-B-default-kill-5000-even-r1.json`](inproc-rust-B-default-kill-5000-even-r1.json), [`inproc-rust-B-default-steady-2000-even-r1.json`](inproc-rust-B-default-steady-2000-even-r1.json), [`inproc-rust-B-default-steady-30000-even-r1.json`](inproc-rust-B-default-steady-30000-even-r1.json), [`inproc-rust-B-default-steady-30000-poisson-r1.json`](inproc-rust-B-default-steady-30000-poisson-r1.json), [`inproc-rust-C-default-control-5000-even-r1.json`](inproc-rust-C-default-control-5000-even-r1.json), [`inproc-rust-C-default-freeze-5000-even-r1.json`](inproc-rust-C-default-freeze-5000-even-r1.json), [`inproc-rust-C-default-kill-5000-even-r1.json`](inproc-rust-C-default-kill-5000-even-r1.json), [`inproc-rust-C-default-steady-2000-even-r1.json`](inproc-rust-C-default-steady-2000-even-r1.json), [`inproc-rust-C-default-steady-30000-even-r1.json`](inproc-rust-C-default-steady-30000-even-r1.json), [`inproc-rust-C-default-steady-30000-poisson-r1.json`](inproc-rust-C-default-steady-30000-poisson-r1.json), [`inproc-rust-D-default-control-5000-even-r1.json`](inproc-rust-D-default-control-5000-even-r1.json), [`inproc-rust-D-default-freeze-5000-even-r1.json`](inproc-rust-D-default-freeze-5000-even-r1.json), [`inproc-rust-D-default-kill-5000-even-r1.json`](inproc-rust-D-default-kill-5000-even-r1.json), [`inproc-rust-D-default-steady-2000-even-r1.json`](inproc-rust-D-default-steady-2000-even-r1.json), [`inproc-rust-D-default-steady-30000-even-r1.json`](inproc-rust-D-default-steady-30000-even-r1.json), [`inproc-rust-D-default-steady-30000-poisson-r1.json`](inproc-rust-D-default-steady-30000-poisson-r1.json), [`inproc-rust-E-default-steady-10-even-r1.json`](inproc-rust-E-default-steady-10-even-r1.json), [`inproc-rust-E-default-steady-2000-even-r1.json`](inproc-rust-E-default-steady-2000-even-r1.json), [`inproc-rust-E-default-steady-30000-even-r1.json`](inproc-rust-E-default-steady-30000-even-r1.json), [`inproc-rust-E-default-steady-30000-poisson-r1.json`](inproc-rust-E-default-steady-30000-poisson-r1.json), [`service-A-default-control-1000-r1.json`](service-A-default-control-1000-r1.json), [`service-A-default-kill-1000-r1.json`](service-A-default-kill-1000-r1.json), [`service-C-default-control-1000-r1.json`](service-C-default-control-1000-r1.json), [`service-C-default-kill-1000-r1.json`](service-C-default-kill-1000-r1.json), [`service-C-supervised-control-1000-r1.json`](service-C-supervised-control-1000-r1.json), [`service-C-supervised-kill-1000-r1.json`](service-C-supervised-kill-1000-r1.json), [`service-D-default-control-1000-r1.json`](service-D-default-control-1000-r1.json), [`service-D-default-kill-1000-r1.json`](service-D-default-kill-1000-r1.json), [`service-D-supervised-control-1000-r1.json`](service-D-supervised-control-1000-r1.json), [`service-D-supervised-kill-1000-r1.json`](service-D-supervised-kill-1000-r1.json), [`service-E-default-control-1000-r1.json`](service-E-default-control-1000-r1.json), [`service-E-default-kill-1000-r1.json`](service-E-default-kill-1000-r1.json).
