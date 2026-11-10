"""Collect six independent alternating-order mirror comparisons."""
import argparse
import csv
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess

p = argparse.ArgumentParser()
p.add_argument("--binary", type=Path, required=True)
p.add_argument("--output", type=Path, required=True)
p.add_argument("--work", type=Path, required=True)
p.add_argument("--build", required=True)
a = p.parse_args()
a.output.mkdir(parents=True, exist_ok=False)
a.work.mkdir(parents=True, exist_ok=False)
binary = a.binary.resolve()
(a.output / "environment.json").write_text(json.dumps({
    "build": a.build, "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
    "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
    "platform": platform.platform(), "cpu": platform.processor(), "logical_cpus": os.cpu_count(),
    "scope": "dense 16^3 sections, 10 warmups and 100 samples per case; logical retained cell copy bytes only, not allocator traffic or validation reads; clone_validate is an explicit reference, not a historical binary",
}, indent=2) + "\n", encoding="utf-8")
for run in range(1, 7):
    target = a.work.resolve() / f"run-{run:02}"
    order = ["--in-place-first"] if run % 2 == 0 else []
    subprocess.run([str(binary), str(target), *order], check=True, timeout=90)
    rows = list(csv.DictReader((target / "samples.csv").open(encoding="utf-8")))
    assert len(rows) == 400
    assert all(int(r["retained_terrain_copy_bytes"]) == (0 if r["mode"] == "in_place" else int(r["sections"]) * 4096 * 2) for r in rows)
    (a.output / f"run-{run:02}.csv").write_bytes((target / "samples.csv").read_bytes())
    (a.output / f"run-{run:02}.json").write_text(json.dumps({"order": "in-place-first" if order else "clone-first", "exit_code": 0, "command": [binary.name, f"run-{run:02}", *order]}, indent=2) + "\n", encoding="utf-8")
print("6 independent processes, 2400 samples; full states and retained buffer identities validated")
