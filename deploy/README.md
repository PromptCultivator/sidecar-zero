# Kubernetes Deployment & Packaging (`deploy/`)

[← 12 — Kubernetes Deployment Spec](../docs/components/12-kubernetes-deploy.md)

Container images, Helm library chart (`deploy/helm-kep753`), Prometheus/Grafana monitoring artifacts, and the 16-scenario Kind end-to-end verification suite for SidecarZero.

| Path | Description |
|---|---|
| [`docker/`](docker/Dockerfile) | Multi-flavor distroless container images (`sidecarzero-ort`, `sidecarzero-gguf`, `sidecarzero-python`, `sidecarzero-chaos`, and test model variants), built via [`docker/build.sh`](docker/build.sh) |
| [`helm-kep753/`](helm-kep753/values.yaml) | Helm library chart providing named templates (`sidecarzero.sidecar`, `sidecarzero.volumes`, `sidecarzero.appVolumeMounts`, `sidecarzero.appEnv`, `sidecarzero.fsGroup`, `sidecarzero.podMonitor`, and `sidecarzero.networkPolicy`) |
| [`helm-kep753/examples/fraud-scorer/`](helm-kep753/examples/fraud-scorer/values.yaml) | Runnable example chart rendering a complete `Deployment`, `Service`, `PodMonitor`, and `NetworkPolicy` |
| [`helm-kep753/examples/monitoring/`](helm-kep753/examples/monitoring/) | `alerts.yaml` (`PrometheusRule` with 15 alert definitions) and `dashboard.json` (Grafana dashboard) |
| [`demo-host/`](demo-host/src/main.rs) | Example host service that calls SidecarZero continuously and exposes outcome counters on `/stats` |
| [`kind/`](kind/e2e.py) | 16-scenario Kubernetes E2E verification suite (`scripts/kind-e2e`), with per-version results in `kind/results/<k8s-version>.json` |

---

## Using the Helm Library Chart

Add `helm-kep753` as a dependency in your application chart's `Chart.yaml` and include its named templates in your pod specification:

```yaml
spec:
  securityContext:
    fsGroup: {{ include "sidecarzero.fsGroup" . }}
  initContainers:
    {{- include "sidecarzero.sidecar" . | nindent 4 }}
  containers:
    - name: app
      env:
        {{- include "sidecarzero.appEnv" . | nindent 8 }}
      volumeMounts:
        {{- include "sidecarzero.appVolumeMounts" . | nindent 8 }}
  volumes:
    {{- include "sidecarzero.volumes" . | nindent 4 }}
```

All configuration options are documented in [`helm-kep753/values.yaml`](helm-kep753/values.yaml) and [12 — Kubernetes Deployment §3](../docs/components/12-kubernetes-deploy.md#3-helm-library-chart-deployhelm-kep753).

---

## Running the Kind E2E Verification Suite

```bash
scripts/kind-e2e             # Build images and run all 16 E2E tests on Kubernetes v1.37
scripts/kind-e2e --matrix    # Run multi-version verification across Kubernetes v1.34–v1.37
scripts/kind-e2e --list      # List individual E2E test names
scripts/kind-e2e kill        # Run a single named test
```
