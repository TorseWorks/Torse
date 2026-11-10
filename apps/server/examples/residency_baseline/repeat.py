"""Run six fresh file-fixture processes, alternating single/batch order."""
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
    "logical_cpus": os.cpu_count(),
    "fixture_pages": 24, "journal_pending_records": 4,
    "io_reservation_bytes": 24 * 32 * 1024 * 1024,
    "scope": "production file server; fixed terrain-only fixture; backend sync not instrumented",
}
(args.output / "environment.json").write_text(json.dumps(environment, indent=2) + "\n", encoding="utf-8")
expected = {}
for run in range(1, 7):
    target = args.work.resolve() / f"run-{run:02}"
    order = ["--batch-first"] if run % 2 == 0 else []
    result = subprocess.run([str(binary), str(target), *order], text=True, capture_output=True, timeout=60)
    if result.returncode:
        raise RuntimeError(f"run {run} failed ({result.returncode}): {result.stderr[-2000:]}")
    rows = list(csv.DictReader((target / "samples.csv").open(encoding="utf-8")))
    assert len(rows) == 4
    assert sorted((r["mode"], r["case"], r["flush_requests"]) for r in rows) == [
        ("1", "install", "24"), ("1", "reload", "24"),
        ("24", "install", "6"), ("24", "reload", "6"),
    ]
    assert all(not row["backend_sync_calls"] for row in rows)
    hashes = {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in target.iterdir() if p.suffix in (".bin", ".log")}
    if expected:
        assert hashes == expected, "state/log hashes differ across independent processes"
    expected = hashes
    (args.output / f"run-{run:02}.csv").write_bytes((target / "samples.csv").read_bytes())
    (args.output / f"run-{run:02}.json").write_text(json.dumps({
        "run": run, "pid_scope": "independent child process", "exit_code": result.returncode,
        "order": "batch-first" if order else "single-first", "hashes": hashes,
        "command": [binary.name, f"run-{run:02}", *order],
    }, indent=2) + "\n", encoding="utf-8")
print("6 independent processes passed; 24 timing rows; identical complete states and journal bytes")
