#!/usr/bin/env python3
"""Generate the test and benchmark models, deterministically, into a directory.

    python3 models/gen.py OUT_DIR

Each model takes `features: float32[rows, N]` and returns `score: float32[rows, 1]`, with a
variable batch dimension unless noted. Each .onnx file is converted to ORT format with the
converter that ships in the onnxruntime wheel; the .ort files are what the worker loads.

  fraud.ort        The benchmark model (overview §1.3): 300 trees of depth 6 in an ONNX-ML
                   TreeEnsembleRegressor, 256 features. Its structure is synthetic (random
                   feature ids, thresholds ~ N(0,1), small leaf weights): the shape and
                   branchiness of a trained fraud model, not a trained one. Spike S5's `tree`.
  fraud-b.ort      The same schema with other trees: a same-schema reload to a new digest.
  fraud128.ort     128 features: a reload that changes the schema.
  fraud-fixed1.ort The fraud model with its batch dimension fixed at 1.
  mlp.ort          256 -> 512 -> 256 -> 1 with ReLU and a Sigmoid, about 0.7 MB of weights:
                   activations that grow with the row count, for the memory-profile tests.
  reference.json   64 seeded input rows for fraud.ort and the scores onnxruntime gives them.
  reference-features.f32, reference-scores.f32
                   The same rows and scores as raw little-endian float32 (64 x 256, and 64),
                   for the conformance agents' prebuilt-codec calls in every language.

Nothing is regenerated if OUT_DIR/stamp holds this script's hash.
"""
import hashlib
import json
import os
import subprocess
import sys

import numpy as np
import onnx
from onnx import TensorProto, helper, numpy_helper

OPSETS = [helper.make_opsetid("", 17), helper.make_opsetid("ai.onnx.ml", 3)]
IR_VERSION = 8


def io_infos(n_features, batch="rows"):
    x = helper.make_tensor_value_info("features", TensorProto.FLOAT, [batch, n_features])
    y = helper.make_tensor_value_info("score", TensorProto.FLOAT, [batch, 1])
    return x, y


def save(model, path):
    onnx.checker.check_model(model)
    onnx.save(model, path)


def make_tree(path, n_features, n_trees, depth, seed, batch="rows"):
    rng = np.random.default_rng(seed)
    n_internal = 2**depth - 1
    n_nodes = 2 ** (depth + 1) - 1
    a = {k: [] for k in ("treeids", "nodeids", "featureids", "values", "modes", "true", "false")}
    t = {k: [] for k in ("treeids", "nodeids", "ids", "weights")}
    for tree in range(n_trees):
        for n in range(n_nodes):
            a["treeids"].append(tree)
            a["nodeids"].append(n)
            if n < n_internal:
                a["featureids"].append(int(rng.integers(0, n_features)))
                a["values"].append(float(rng.normal()))
                a["modes"].append("BRANCH_LEQ")
                a["true"].append(2 * n + 1)
                a["false"].append(2 * n + 2)
            else:
                a["featureids"].append(0)
                a["values"].append(0.0)
                a["modes"].append("LEAF")
                a["true"].append(0)
                a["false"].append(0)
                t["treeids"].append(tree)
                t["nodeids"].append(n)
                t["ids"].append(0)
                t["weights"].append(float(rng.normal(scale=0.05)))
    node = helper.make_node(
        "TreeEnsembleRegressor", ["features"], ["score"], domain="ai.onnx.ml",
        n_targets=1, aggregate_function="SUM", post_transform="NONE", base_values=[-1.0],
        nodes_treeids=a["treeids"], nodes_nodeids=a["nodeids"], nodes_featureids=a["featureids"],
        nodes_values=a["values"], nodes_modes=a["modes"], nodes_truenodeids=a["true"],
        nodes_falsenodeids=a["false"], nodes_missing_value_tracks_true=[0] * len(a["nodeids"]),
        target_treeids=t["treeids"], target_nodeids=t["nodeids"], target_ids=t["ids"], target_weights=t["weights"],
    )
    x, y = io_infos(n_features, batch)
    graph = helper.make_graph([node], f"fraud_{n_trees}x{depth}", [x], [y])
    save(helper.make_model(graph, opset_imports=OPSETS, ir_version=IR_VERSION, producer_name="sidecarzero-models"), path)


def make_mlp(path, n_features, hidden, seed):
    rng = np.random.default_rng(seed)
    dims = [n_features] + list(hidden) + [1]
    nodes, inits, prev = [], [], "features"
    for i in range(len(dims) - 1):
        w = rng.normal(scale=1.0 / np.sqrt(dims[i]), size=(dims[i + 1], dims[i])).astype(np.float32)
        b = rng.normal(scale=0.01, size=(dims[i + 1],)).astype(np.float32)
        inits += [numpy_helper.from_array(w, f"w{i}"), numpy_helper.from_array(b, f"b{i}")]
        last = i == len(dims) - 2
        out = "logit" if last else f"h{i}"
        nodes.append(helper.make_node("Gemm", [prev, f"w{i}", f"b{i}"], [out], transB=1, name=f"gemm{i}"))
        if last:
            nodes.append(helper.make_node("Sigmoid", [out], ["score"], name="sigmoid"))
        else:
            nodes.append(helper.make_node("Relu", [out], [f"a{i}"], name=f"relu{i}"))
            prev = f"a{i}"
    x, y = io_infos(n_features)
    graph = helper.make_graph(nodes, "mlp", [x], [y], initializer=inits)
    save(helper.make_model(graph, opset_imports=OPSETS, ir_version=IR_VERSION, producer_name="sidecarzero-models"), path)


def main():
    out = sys.argv[1]
    me = hashlib.sha256(open(__file__, "rb").read()).hexdigest()
    stamp = os.path.join(out, "stamp")
    if os.path.exists(stamp) and open(stamp).read().strip() == me:
        return
    os.makedirs(out, exist_ok=True)
    src = os.path.join(out, "onnx")
    os.makedirs(src, exist_ok=True)
    make_tree(os.path.join(src, "fraud.onnx"), 256, 300, 6, seed=7)
    make_tree(os.path.join(src, "fraud-b.onnx"), 256, 300, 6, seed=11)
    make_tree(os.path.join(src, "fraud128.onnx"), 128, 300, 6, seed=13)
    make_tree(os.path.join(src, "fraud-fixed1.onnx"), 256, 300, 6, seed=7, batch=1)
    make_mlp(os.path.join(src, "mlp.onnx"), 256, (512, 256), seed=9)
    # 'Fixed': optimizations are applied now and baked into each .ort file.
    subprocess.run([sys.executable, "-m", "onnxruntime.tools.convert_onnx_models_to_ort", src, "--output_dir", out,
                    "--optimization_style", "Fixed"], check=True, stdout=subprocess.DEVNULL)
    for name in ("fraud", "fraud-b", "fraud128", "fraud-fixed1", "mlp"):
        assert os.path.exists(os.path.join(out, name + ".ort")), name

    import onnxruntime as ort

    x = np.random.default_rng(1).normal(size=(64, 256)).astype(np.float32)
    sess = ort.InferenceSession(os.path.join(out, "fraud.ort"), providers=["CPUExecutionProvider"])
    y = sess.run(None, {"features": x})[0].ravel()
    ref = {"model": "fraud.ort", "onnxruntime": ort.__version__, "features": x.tolist(), "score": y.tolist()}
    with open(os.path.join(out, "reference.json"), "w") as f:
        json.dump(ref, f)
    x.astype("<f4").tofile(os.path.join(out, "reference-features.f32"))
    y.astype("<f4").tofile(os.path.join(out, "reference-scores.f32"))
    with open(stamp, "w") as f:
        f.write(me + "\n")


if __name__ == "__main__":
    main()
