"""Compare six independent processes with alternating seed/interest order."""
import argparse
import csv
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess

parser = argparse.ArgumentParser()
parser.add_argument("--binary", type=Path, required=True)
parser.add_argument("--output", type=Path, required=True)
parser.add_argument("--work", type=Path, required=True)
parser.add_argument("--build", required=True)
args = parser.parse_args()
args.output.mkdir(parents=True, exist_ok=False)
args.work.mkdir(parents=True, exist_ok=False)
binary = args.binary.resolve()
environment = {
    "build": args.build,
    "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
    "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
    "platform": platform.platform(), "cpu": platform.processor(),
    "logical_cpus": os.cpu_count(), "sections": 256, "overlap": 224,
    "scope": "file-backed fixed world, memory transport; encoded bodies exclude TCP/TLS framing; host call excludes mirror validation",
}
(args.output / "environment.json").write_text(json.dumps(environment, indent=2) + "\n", encoding="utf-8")
expected = None
for run in range(1, 7):
    target = args.work.resolve() / f"run-{run:02}"
    order = ["--interest-first"] if run % 2 == 0 else []
    result = subprocess.run([str(binary), str(target), *order], capture_output=True, text=True, timeout=60)
    if result.returncode:
        raise RuntimeError(f"run {run} failed: {result.stderr[-2000:]}")
    rows = list(csv.DictReader((target / "samples.csv").open(encoding="utf-8")))
    assert len(rows) == 12
    for row in rows:
        assert (int(row["terrain_sections"]), int(row["retained_terrain_sections"])) == (
            (256, 224) if row["mode"] == "seed" else (32, 0))
    hashes = {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in target.glob("*-state.bin")}
    assert len(hashes) == 12
    for step in range(6):
        assert hashes[f"seed-{step}-state.bin"] == hashes[f"interest-{step}-state.bin"]
    if expected is not None:
        assert hashes == expected
    expected = hashes
    (args.output / f"run-{run:02}.csv").write_bytes((target / "samples.csv").read_bytes())
    (args.output / f"run-{run:02}.json").write_text(json.dumps({
        "order": "interest-first" if order else "seed-first", "hashes": hashes,
        "command": [binary.name, f"run-{run:02}", *order], "exit_code": result.returncode,
    }, indent=2) + "\n", encoding="utf-8")
print("6 processes, 72 cuts; full authorized states equal across modes and processes")
