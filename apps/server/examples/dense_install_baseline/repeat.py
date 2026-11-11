"""Alternate independent, paced baseline and optimized processes; retain every sample."""
import argparse
import csv
import json
import math
import platform
import statistics
import subprocess
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("baseline", type=Path)
parser.add_argument("optimized", type=Path)
parser.add_argument("output", type=Path)
parser.add_argument("--baseline-source", required=True)
parser.add_argument("--optimized-source", required=True)
parser.add_argument("--runs", type=int, default=6)
args = parser.parse_args()
if not 1 <= args.runs <= 32:
    parser.error("runs must be 1..32")
args.output.mkdir()
samples = {"baseline": [], "optimized": [], "uniform-baseline": [], "uniform-optimized": []}
executables = {"baseline": args.baseline.resolve(), "optimized": args.optimized.resolve()}
for repeat in range(args.runs + 1):
    uniform = repeat == args.runs
    order = ["baseline", "optimized"] if repeat % 2 == 0 else ["optimized", "baseline"]
    for mode in order:
        label = ("uniform-" if uniform else "") + mode
        folder = args.output / f"{label}-{repeat + 1:02}"
        command = [str(executables[mode]), str(folder.resolve())]
        if uniform:
            command.append("--uniform")
        subprocess.run(command, check=True)
        rows = list(csv.DictReader((folder / "samples.csv").open()))
        assert len(rows) == 30 and all(int(r["flush_requests"]) == 1 for r in rows)
        samples[label].extend(rows)
        print(f"{label} run {repeat + 1}: 30 columns, exact recovery and replica verified", flush=True)
summary = {}
for mode, rows in samples.items():
    group = {"samples": len(rows), "kernel_ge_5ms": sum(int(r["kernel_total_ns"]) >= 5_000_000 for r in rows)}
    for column in rows[0]:
        if column.endswith("_ns"):
            values = sorted(int(row[column]) for row in rows)
            group[column] = {
                "mean": statistics.mean(values), "median": statistics.median(values),
                "p95": values[math.ceil(.95 * len(values)) - 1],
                "p99": values[math.ceil(.99 * len(values)) - 1], "max": values[-1],
            }
    cpu = sorted(int(r["kernel_total_ns"]) - int(r["durable_wait_ns"]) for r in rows)
    group["kernel_excluding_durable_wait_ns"] = {
        "mean": statistics.mean(cpu), "median": statistics.median(cpu),
        "p95": cpu[math.ceil(.95 * len(cpu)) - 1], "p99": cpu[math.ceil(.99 * len(cpu)) - 1], "max": cpu[-1],
    }
    summary[mode] = group
(args.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")
environment = {
    "baseline_source": args.baseline_source, "optimized_source": args.optimized_source,
    "os": platform.platform(), "cpu": platform.processor(), "build": "Release",
    "rust": subprocess.check_output(["rustc", "--version"], text=True).strip(),
    "runs_per_mode": args.runs, "order": "alternating fresh processes, then one uniform run per mode",
    "timing": "50ms paced kernel ticks over 10s at 10.9 cells/s; initial 25 columns and recovery excluded; host input construction reported separately; no concurrent builds requested",
    "fixture": "fixed seed 8327473519530246828; 24 pages, 16 dense, three attached entities per column; 30 measured columns; steady 600 resident sections, maximum 720",
    "semantics": "one atomic record/publication/durable barrier; one flush request asserted for every measured column; exact replica and reopened state verified every process",
    "scope": "representative kernel fixture; original external host seed/entry point not provided; no Socket or external gameplay systems; logical memory reported, RSS not sampled",
    "capacities": "runtime max_items=200000; replica/checkpoint snapshot max_items=2000000; original byte and journal synchronization policies; identical in both modes",
    "quantiles": "nearest-rank ceil(p*n)-1; all final samples retained, including outliers",
}
(args.output / "environment.json").write_text(json.dumps(environment, indent=2) + "\n", encoding="utf-8")
