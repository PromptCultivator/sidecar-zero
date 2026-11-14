# AWS Infrastructure Guide

[← High-level design](../high-level-design.md) · Google Cloud setup in [gcp.md](gcp.md)

This guide documents how to provision Amazon Elastic Kubernetes Service (EKS) clusters and EC2 bare-metal instances on AWS for running SidecarZero's managed-cluster end-to-end verification tests ([12 — Kubernetes Deployment §10](../components/12-kubernetes-deploy.md#10-kind-end-to-end-verification-suite-deploykinde2epy)) and latency benchmarks ([13 — Chaos Benchmark](../components/13-chaos-benchmark.md)).

| Resource | Purpose | Section |
|---|---|---|
| Amazon EKS cluster (`x86-64` and `aarch64` managed node groups) | Managed-cluster verification on Amazon Linux 2023 (cgroup v2) | [§1](#1-eks-cluster-for-managed-kubernetes-verification) |
| EC2 bare-metal instances (`c7i.metal-24xl` and `c7g.metal`) | Dedicated bare-metal benchmarking on Intel Sapphire Rapids and AWS Graviton3 | [§2](#2-ec2-bare-metal-performance-instances) |

---

## 1. EKS Cluster for Managed Kubernetes Verification

### 1.1 Cluster Topology

| Setting | Value | Rationale |
|---|---|---|
| Kubernetes version | `1.34`–`1.37` | Supports KEP-753 GA native sidecars and kubelet `singleProcessOOMKill` |
| Node operating system | Amazon Linux 2023 (`AL2023_x86_64_STANDARD` / `AL2023_ARM_64_STANDARD`) | Ships with cgroup v2 and Linux kernel 6.1+ by default |
| Node groups | One `x86-64` (`m7i.2xlarge`), one `aarch64` (`m7g.2xlarge`) | Verifies both target architectures |

### 1.2 Kubelet Configuration (`NodeConfig`)

On Amazon Linux 2023, kubelet flags for the static CPU manager ([12 — Kubernetes Deployment §4](../components/12-kubernetes-deploy.md#4-resource-sizing)) and `singleProcessOOMKill` ([12 — Kubernetes Deployment §5](../components/12-kubernetes-deploy.md#5-oom-shielding)) are configured via an `application/node.eks.aws` `NodeConfig` document in the node group's launch template MIME user data:

```
MIME-Version: 1.0
Content-Type: multipart/mixed; boundary="BOUNDARY"

--BOUNDARY
Content-Type: application/node.eks.aws

---
apiVersion: node.eks.aws/v1alpha1
kind: NodeConfig
spec:
  kubelet:
    config:
      cpuManagerPolicy: static
      singleProcessOOMKill: true

--BOUNDARY--
```

When using a managed node group without a custom AMI, EKS merges this `NodeConfig` with its bootstrap configuration automatically.

### 1.3 Creating and Deleting the Cluster

```bash
eksctl create cluster --name scz-test --region us-east-1
```

After running the E2E verification tests, delete the cluster and all associated node groups and volumes:

```bash
eksctl delete cluster --name scz-test --region us-east-1
```

---

## 2. EC2 Bare-Metal Performance Instances

EC2 bare-metal instances provide direct access to hardware performance counters, C-state/P-state controls on Intel/AMD processors, and fixed-frequency execution on AWS Graviton processors:

| Architecture | Instance Type | Hardware Threads | Processor |
|---|---|---|---|
| `x86-64` | `c7i.metal-24xl` | 96 | Intel Sapphire Rapids |
| `x86-64` (larger) | `c7i.metal-48xl`, `c7a.metal-48xl` | 192 | Intel Sapphire Rapids, AMD EPYC Genoa |
| `aarch64` | `c7g.metal` | 64 | AWS Graviton3 (Arm Neoverse V1, fixed frequency) |
| `aarch64` (newer) | `c8g.metal-24xl` | 96 | AWS Graviton4 (Arm Neoverse V2) |

Apply the same kernel command-line isolation parameters (`isolcpus`, `nohz_full`, `rcu_nocbs`, and `max_cstate=1` on `x86-64`) and `performance` frequency governor described in [gcp.md §1.2](gcp.md#12-provisioning-and-kernel-tuning), then run:

```bash
sudo ./spikes/run-all.sh --install
./spikes/run-all.sh
make -C bench chaos-bench
```

---

## 3. Teardown Checklist

- [ ] The `scz-test` EKS cluster, managed node groups, and launch templates are deleted.
- [ ] All bare-metal EC2 instances (`c7i.metal-*`, `c7g.metal`, `c8g.metal-*`) are terminated.
- [ ] No unattached EBS volumes or Elastic Load Balancers remain in the region.
