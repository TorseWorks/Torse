"""Run six independent generated-entity installation comparisons."""
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
    "pages": 24, "generated_entities_per_page": 1, "journal_pending_records": 4,
    "logical_batch_budget_bytes": 8388608,
    "scope": "file-backed trusted installs; total includes batch handle validation; max call excludes handle construction; flush requests only, backend sync calls uninstrumented; no shared MemoryPool attached",
}, indent=2) + "\n", encoding="utf-8")
expected = None
for run in range(1, 7):
    target = a.work.resolve() / f"run-{run:02}"
    order = ["--batch-first"] if run % 2 == 0 else []
    subprocess.run([str(binary), str(target), *order], check=True, timeout=60)
    rows = list(csv.DictReader((target / "samples.csv").open(encoding="utf-8")))
    assert sorted((r["mode"], r["pages"], r["flush_requests"]) for r in rows) == [("batch", "24", "6"), ("single", "24", "24")]
    assert all(not r["backend_sync_calls"] for r in rows)
    hashes = {f.name: hashlib.sha256(f.read_bytes()).hexdigest() for f in target.iterdir() if f.suffix in (".bin", ".log")}
    assert len(hashes) == 4
    assert hashes["single-state.bin"] == hashes["batch-state.bin"]
    assert hashes["single-journal.log"] == hashes["batch-journal.log"]
    if expected is not None: assert hashes == expected
    expected = hashes
    (a.output / f"run-{run:02}.csv").write_bytes((target / "samples.csv").read_bytes())
    (a.output / f"run-{run:02}.json").write_text(json.dumps({"order": "batch-first" if order else "single-first", "hashes": hashes, "exit_code": 0, "command": [binary.name, f"run-{run:02}", *order]}, indent=2) + "\n", encoding="utf-8")
print("6 processes, 12 samples; complete states, entity identities, recovery and journal hashes agree")
