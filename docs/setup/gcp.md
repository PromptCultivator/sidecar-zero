# Google Cloud Infrastructure Guide

[← High-level design](../high-level-design.md) · AWS setup in [aws.md](aws.md)

This guide documents how to provision bare-metal performance benchmarking instances and Google Kubernetes Engine (GKE) clusters on Google Cloud for running SidecarZero's latency benchmarks ([13 — Chaos Benchmark](../components/13-chaos-benchmark.md)) and managed-cluster end-to-end verification tests ([12 — Kubernetes Deployment §10](../components/12-kubernetes-deploy.md#10-kind-end-to-end-verification-suite-deploykinde2epy)).

| Resource | Purpose | Section |
|---|---|---|
| Bare-metal Compute Engine instances (`x86-64` and `aarch64`) | Microsecond-level latency benchmarking and performance gate verification | [§1](#1-bare-metal-performance-instances) |
| GKE Standard cluster (`x86-64` and `aarch64` node pools) | Managed-cluster verification of KEP-753 native sidecars and `singleProcess` / `nestedCgroup` OOM shields | [§2](#2-gke-cluster-for-managed-kubernetes-verification) |
| OpenShift Local (`crc`) | Verification under CRI-O, SELinux enforcing mode, and arbitrary OpenShift UIDs | [§3](#3-openshift-local-verification) |

---

## 1. Bare-Metal Performance Instances

### 1.1 Machine Types

Bare-metal instances (`*-metal`) run directly on physical hardware without a hypervisor and expose hardware performance counters (`perf`) and CPU frequency/idle controls.

| Architecture | Machine Type | Hardware Threads | Processor |
|---|---|---|---|
| `x86-64` | `c3-standard-192-metal` | 192 | Intel Sapphire Rapids |
| `x86-64` (alternatives) | `c4-standard-288-metal`, `c4d-standard-384-metal` | 288, 384 | Intel Granite Rapids, AMD Turin |
| `aarch64` | `c4a-highmem-96-metal` | 96 | Google Axion (Arm Neoverse V2) |

Key platform characteristics on Compute Engine bare metal:

| Characteristic | Operational Requirement |
|---|---|
| Network driver | Uses the Intel IDPF driver; select an OS image with IDPF support (Debian 12 / Ubuntu 24.04 LTS or newer) |
| Storage | Uses Hyperdisk (`hyperdisk-balanced`) rather than standard Persistent Disk |
| Maintenance policy | Must be set to `--maintenance-policy=TERMINATE` (live migration is not applicable to bare metal) |
| Hardware counters | Full PMU counter access for `perf stat` and `perf record` |

### 1.2 Provisioning and Kernel Tuning

1. **Verify machine availability in the target zone:**

```bash
gcloud compute machine-types list --zones=us-central1-a --filter="name~metal"
```

2. **Configure deterministic kernel and CPU isolation settings** on the benchmark host image so that worker consume threads, caller threads, and load generator threads run on isolated physical cores without scheduler ticks or deep C-state exit latency:

| Setting | Location | Configuration |
|---|---|---|
| Core isolation | Kernel command line (`/etc/default/grub`) | `isolcpus=2-17 nohz_full=2-17 rcu_nocbs=2-17` |
| C-state cap (`x86-64`) | Kernel command line | `intel_idle.max_cstate=1 processor.max_cstate=1` |
| CPU frequency governor | Boot service | `cpupower frequency-set -g performance` |
| IRQ affinity | Boot service | Disable `irqbalance` and steer NIC/NVMe interrupts away from isolated cores |

3. **Verify host tuning after boot:**

```bash
cat /proc/cmdline
cat /sys/devices/system/cpu/isolated
lscpu
cpupower frequency-info
cpupower idle-info
```

4. **Launch an instance for a benchmark campaign:**

```bash
gcloud compute instances create scz-perf-x86 \
  --zone=us-central1-a \
  --machine-type=c3-standard-192-metal \
  --maintenance-policy=TERMINATE \
  --image-family=debian-12 --image-project=debian-cloud \
  --boot-disk-type=hyperdisk-balanced --boot-disk-size=100GB \
  --labels=purpose=scz-perf
```

5. **Run the kernel validation suite and chaos benchmark:**

```bash
sudo ./spikes/run-all.sh --install
./spikes/run-all.sh
make -C bench chaos-bench
```

Always terminate bare-metal instances immediately after uploading benchmark artifacts (`gcloud compute instances delete scz-perf-x86 --zone=us-central1-a`).

---

## 2. GKE Cluster for Managed Kubernetes Verification

### 2.1 Cluster Topology

| Setting | Value | Rationale |
|---|---|---|
| Cluster mode | GKE Standard | Allows custom kubelet and containerd node configuration (`singleProcessOOMKill`, `cpuManagerPolicy`, and `writableCgroups`) |
| Kubernetes version | `1.34.1-gke.2541000` or newer | Supports KEP-753 GA native sidecars, `singleProcessOOMKill`, and writable cgroups |
| Node pools | `x86-64` (`c3-standard-8`) and `aarch64` (`c4a-standard-8`) | Verifies both target architectures |

### 2.2 Node System and `containerd` Configuration

GKE supports both non-privileged Layer 1 OOM shield modes ([12 — Kubernetes Deployment §5](../components/12-kubernetes-deploy.md#5-oom-shielding)):

**`node-config.yaml` (enables `singleProcess` OOM shield and static CPU manager):**

```yaml
kubeletConfig:
  cpuManagerPolicy: static
  singleProcessOOMKill: true
linuxConfig:
  cgroupMode: 'CGROUP_MODE_V2'
```

**`containerd-config.yaml` (enables `nestedCgroup` OOM shield):**

```yaml
writableCgroups:
  enabled: true
```

Note that on GKE, pods using `writableCgroups` must run in the `Guaranteed` QoS class (`requests == limits`, matching SidecarZero's dedicated CPU profile) and include the label and annotation `node.gke.io/enable-writable-cgroups: "true"`.

### 2.3 Creating and Testing the Cluster

```bash
gcloud container clusters create scz-test \
  --location=us-central1-a \
  --machine-type=c3-standard-8 --num-nodes=2 \
  --system-config-from-file=node-config.yaml \
  --containerd-config-from-file=containerd-config.yaml

gcloud container node-pools create arm \
  --cluster=scz-test --location=us-central1-a \
  --machine-type=c4a-standard-8 --num-nodes=2 \
  --system-config-from-file=node-config.yaml \
  --containerd-config-from-file=containerd-config.yaml
```

After running the E2E verification suite against the cluster, delete the cluster and its associated disks:

```bash
gcloud container clusters delete scz-test --location=us-central1-a
```

---

## 3. OpenShift Local Verification

To verify CRI-O, SELinux enforcing mode (`svirt_lxc_net_t` MCS category sharing across containers in a pod), and OpenShift's arbitrary UID assignment on a single-node OpenShift cluster using Red Hat OpenShift Local (`crc`):

```bash
crc setup
crc start
```

Deploy the test pod from [`spikes/s1-memfd-across-containers`](../../spikes/s1-memfd-across-containers/FINDING.md) or the Helm example (`deploy/helm-kep753/examples/fraud-scorer`), verify shared-memory attachment and `SCM_RIGHTS` descriptor passing, and clean up the local VM with `crc delete`.

---

## 4. Teardown Checklist

- [ ] No Compute Engine instances labeled `purpose=scz-perf` remain running.
- [ ] The `scz-test` GKE cluster and its node pools are deleted.
- [ ] No unattached Hyperdisk or Persistent Disk volumes remain in the project.
