#!/usr/bin/env python3
"""Spike S5 - generate the benchmark models. Throwaway.

Three models, each taking `features: float32[rows, 256]` and producing `score: float32[rows, 1]`:

  tree.onnx   gradient-boosted-style ensemble: 300 complete binary trees of depth 6, expressed with
              the ONNX-ML TreeEnsembleRegressor operator, SUM aggregation, no post-transform (the
              score is the raw margin). Structure is synthetic (random feature ids, thresholds
              ~ N(0,1), small leaf weights). It is not a trained model; it has the shape and
              branchiness of one.
  tree1.onnx  the same operator with ONE tree of depth 1. Its arithmetic is negligible, so the time
              of a Run on it is the runtime's fixed cost per call for this operator.
  mlp.onnx    256 -> 4096 -> 2048 -> 1 with ReLU and a final Sigmoid. About 38 MB of float32 weights,
              enough to make "are the weights shared file pages?" visible in /proc/<pid>/smaps.

Each .onnx is then converted to ORT format with the official converter that ships in the
onnxruntime wheel (python -m onnxruntime.tools.convert_onnx_models_to_ort).
"""
import os
import subprocess
import sys

import numpy as np
import onnx
from onnx import TensorProto, helper, numpy_helper

N_FEATURES = 256
OPSETS = [helper.make_opsetid("", 17), helper.make_opsetid("ai.onnx.ml", 3)]
IR_VERSION = 8


def io_infos():
    x = helper.make_tensor_value_info("features", TensorProto.FLOAT, ["rows", N_FEATURES])
    y = helper.make_tensor_value_info("score", TensorProto.FLOAT, ["rows", 1])
    return x, y


def make_tree(path, n_trees, depth, seed, post_transform="NONE"):
    rng = np.random.default_rng(seed)
    n_internal = 2**depth - 1
    n_nodes = 2 ** (depth + 1) - 1
    treeids, nodeids, featureids, values, modes, true_ids, false_ids = [], [], [], [], [], [], []
    t_treeids, t_nodeids, t_ids, t_weights = [], [], [], []
    for t in range(n_trees):
        for n in range(n_nodes):
            treeids.append(t)
            nodeids.append(n)
            if n < n_internal:
                featureids.append(int(rng.integers(0, N_FEATURES)))
                values.append(float(rng.normal()))
                modes.append("BRANCH_LEQ")
                true_ids.append(2 * n + 1)
                false_ids.append(2 * n + 2)
            else:
                featureids.append(0)
                values.append(0.0)
                modes.append("LEAF")
                true_ids.append(0)
                false_ids.append(0)
                t_treeids.append(t)
                t_nodeids.append(n)
                t_ids.append(0)
                t_weights.append(float(rng.normal(scale=0.05)))
    node = helper.make_node(
        "TreeEnsembleRegressor",
        ["features"],
        ["score"],
        domain="ai.onnx.ml",
        n_targets=1,
        aggregate_function="SUM",
        post_transform=post_transform,
        base_values=[-1.0],
        nodes_treeids=treeids,
        nodes_nodeids=nodeids,
        nodes_featureids=featureids,
        nodes_values=values,
        nodes_modes=modes,
        nodes_truenodeids=true_ids,
        nodes_falsenodeids=false_ids,
        nodes_missing_value_tracks_true=[0] * len(nodeids),
        target_treeids=t_treeids,
        target_nodeids=t_nodeids,
        target_ids=t_ids,
        target_weights=t_weights,
    )
    x, y = io_infos()
    graph = helper.make_graph([node], f"tree_{n_trees}x{depth}", [x], [y])
    model = helper.make_model(graph, opset_imports=OPSETS, ir_version=IR_VERSION, producer_name="scz-spike-s5")
    onnx.checker.check_model(model)
    onnx.save(model, path)
    return n_trees * n_nodes


def make_mlp(path, hidden, seed):
    rng = np.random.default_rng(seed)
    dims = [N_FEATURES] + list(hidden) + [1]
    nodes, inits = [], []
    prev = "features"
    n_params = 0
    for i in range(len(dims) - 1):
        fan_in, fan_out = dims[i], dims[i + 1]
        # PyTorch-style Linear export: weight is [out, in], Gemm with transB=1.
        w = rng.normal(scale=1.0 / np.sqrt(fan_in), size=(fan_out, fan_in)).astype(np.float32)
        b = rng.normal(scale=0.01, size=(fan_out,)).astype(np.float32)
        n_params += w.size + b.size
        inits.append(numpy_helper.from_array(w, f"w{i}"))
        inits.append(numpy_helper.from_array(b, f"b{i}"))
        last = i == len(dims) - 2
        out = "logit" if last else f"h{i}"
        nodes.append(helper.make_node("Gemm", [prev, f"w{i}", f"b{i}"], [out], transB=1, name=f"gemm{i}"))
        if last:
            nodes.append(helper.make_node("Sigmoid", [out], ["score"], name="sigmoid"))
        else:
            nodes.append(helper.make_node("Relu", [out], [f"a{i}"], name=f"relu{i}"))
            prev = f"a{i}"
    x, y = io_infos()
    graph = helper.make_graph(nodes, "mlp", [x], [y], initializer=inits)
    model = helper.make_model(graph, opset_imports=OPSETS, ir_version=IR_VERSION, producer_name="scz-spike-s5")
    onnx.checker.check_model(model)
    onnx.save(model, path)
    return n_params


def check_logistic(out):
    """Noticed while building the model: does a single-target regressor apply post_transform=LOGISTIC?"""
    import onnxruntime as ort

    a, b = os.path.join(out, "_none.onnx"), os.path.join(out, "_logistic.onnx")
    make_tree(a, 300, 6, 7, "NONE")
    make_tree(b, 300, 6, 7, "LOGISTIC")
    x = np.random.default_rng(1).normal(size=(4, N_FEATURES)).astype(np.float32)
    ya = ort.InferenceSession(a, providers=["CPUExecutionProvider"]).run(None, {"features": x})[0].ravel()
    yb = ort.InferenceSession(b, providers=["CPUExecutionProvider"]).run(None, {"features": x})[0].ravel()
    print("side check, TreeEnsembleRegressor with n_targets=1 in onnxruntime", ort.__version__)
    print("  post_transform=NONE    :", ya)
    print("  post_transform=LOGISTIC:", yb)
    print("  identical:", bool(np.array_equal(ya, yb)), "(identical means LOGISTIC was NOT applied)")
    os.unlink(a)
    os.unlink(b)


def main():
    out = sys.argv[1]
    os.makedirs(out, exist_ok=True)
    check_logistic(out)
    n = make_tree(os.path.join(out, "tree.onnx"), n_trees=300, depth=6, seed=7)
    print(f"tree.onnx : 300 trees, depth 6, {n} nodes")
    n = make_tree(os.path.join(out, "tree1.onnx"), n_trees=1, depth=1, seed=8)
    print(f"tree1.onnx: 1 tree, depth 1, {n} nodes (fixed-cost floor)")
    n = make_mlp(os.path.join(out, "mlp.onnx"), hidden=(4096, 2048), seed=9)
    print(f"mlp.onnx  : 256-4096-2048-1, {n} parameters, {n * 4 / 1e6:.1f} MB of float32")

    # Official converter. 'Fixed' = optimizations are applied now and baked into the .ort file.
    cmd = [sys.executable, "-m", "onnxruntime.tools.convert_onnx_models_to_ort", out,
           "--optimization_style", "Fixed"]
    print("+", " ".join(cmd), flush=True)
    subprocess.run(cmd, check=True)
    for f in sorted(os.listdir(out)):
        print(f"{os.path.getsize(os.path.join(out, f)):>12}  {f}")
    for name in ("tree", "tree1", "mlp"):
        assert os.path.exists(os.path.join(out, name + ".ort")), name


if __name__ == "__main__":
    main()
