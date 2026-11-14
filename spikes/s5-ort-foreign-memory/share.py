#!/usr/bin/env python3
"""Spike S5 - two processes loading the same model, measured while both are alive. Throwaway.

Starts `s5 load <model> <variant> --hold` twice. Each child loads the model, runs a few inferences so
every weight page has been touched, prints READY and then answers "report <label>" on stdin with its
/proc/self/smaps numbers. The first child reports alone, then again once the second is up.
"""
import re
import subprocess
import sys


def start(model, variant):
    p = subprocess.Popen(["/work/s5", "load", model, variant, "--hold"], stdin=subprocess.PIPE,
                         stdout=subprocess.PIPE, text=True, bufsize=1)
    for line in p.stdout:
        if line.strip() == "READY":
            return p
        print("   ", line.rstrip())
    raise SystemExit("child exited before READY")


def report(p, label):
    p.stdin.write(f"report {label}\n")
    p.stdin.flush()
    out = []
    for line in p.stdout:
        if line.strip() == "DONE":
            break
        out.append(line.rstrip())
        print("  ", line.rstrip())
    return "\n".join(out)


def field(text, scope, key):
    for line in text.splitlines():
        if scope in line:
            m = re.search(rf"\b{key}=(-?\d+)", line)
            if m:
                return int(m.group(1))
    return None


def main():
    model, variant = sys.argv[1], sys.argv[2]
    print(f"== share model={model} variant={variant}", flush=True)
    p1 = start(model, variant)
    alone = report(p1, "P1-alone")
    p2 = start(model, variant)
    both1 = report(p1, "P1-with-P2")
    both2 = report(p2, "P2-with-P1")
    for p in (p1, p2):
        p.stdin.write("quit\n")
        p.stdin.flush()
        p.wait()
    a_map, b_map = field(alone, "model-file mapping", "Pss"), field(both1, "model-file mapping", "Pss")
    a_all, b_all = field(alone, "whole process", "Pss"), field(both1, "whole process", "Pss")
    a_anon, b_anon = field(alone, "whole process", "Pss_Anon"), field(both1, "whole process", "Pss_Anon")
    b2_all = field(both2, "whole process", "Pss")
    print(f"SUMMARY {model.split('/')[-1]} {variant}: model-file mapping Pss {a_map} kB alone -> {b_map} kB with a "
          f"second process; whole-process Pss {a_all} -> {b_all} kB; Pss_Anon {a_anon} -> {b_anon} kB; "
          f"both processes together {b_all + b2_all} kB Pss (one alone: {a_all} kB)")
    print(flush=True)


if __name__ == "__main__":
    main()
