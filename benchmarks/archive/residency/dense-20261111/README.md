# Dense generated section installations

The [follow-up Ryzen/NVMe comparison](../dense-install-tail-20261111.md) measures a later restore optimization on a separate machine.

Six alternating pairs of fresh Release processes on Windows 11 / Intel Core i9-14900HX. Each process initializes 25 columns (600 sections), then drives a 50 ms kernel tick for ten seconds at a virtual 10.9 cells/s. It installs 30 columns, each with 24 pages (16 dense) and three attached entities, while encoding and applying authorized replica updates and unloading the rear window. Steady residency is 600 sections, temporarily at most 720. Final replicas match a fresh projection and reopening reproduces the exact world and entity allocator.

The baseline is PR #70 behavior plus identical phase instrumentation and fixture at `03f8681`. The optimized source is `f3d2238`. Every measured column asserts one flush request; record/publication/barrier semantics and physical backend call counts are covered by separate atomic recovery tests. No durability policy was weakened. The paced fixture does not separately intercept backend sync calls.

## Measured wall time

180 mixed-density samples per mode. Values are milliseconds; quantiles use nearest rank. All final samples, including outliers, are retained. Clock observations include scheduling; the non-durable remainder is not a hardware CPU-cycle measurement.

| Stage | Baseline mean | Optimized mean | Baseline p95 | Optimized p95 | Baseline max | Optimized max |
|---|---:|---:|---:|---:|---:|---:|
| Host total | 5.6367 | 4.2293 | 10.0351 | 7.7154 | 12.2153 | 14.4445 |
| Kernel total | 4.7110 | 3.1825 | 8.1034 | 5.3087 | 11.5014 | 13.3282 |
| Page preparation | 0.2578 | 0.2671 | 0.5220 | 0.4829 | 0.8495 | 0.5552 |
| Validation/preparation | 3.3667 | 1.7386 | 5.7806 | 2.8599 | 10.2271 | 8.6194 |
| Final journal encoding | 0.1215 | 0.1401 | 0.2193 | 0.2888 | 0.4621 | 0.7150 |
| Publication/remainder | 0.0823 | 0.0854 | 0.1289 | 0.1332 | 0.2655 | 0.3881 |
| Durable wait | 0.8815 | 0.9500 | 1.4717 | 1.5899 | 4.3911 | 8.3926 |
| Kernel excluding durable wait | 3.8295 | 2.2325 | 6.6272 | 3.7188 | 10.5753 | 9.4310 |

Kernel mean improved by 32.4%; validation/preparation mean by 48.4%; kernel p95 by 34.5%. Columns at or above 5 ms fell from 61/180 to 15/180. Kernel p99 is 8.9816 ms before and 11.1123 ms after. The optimized maximum is higher than the baseline maximum; this result does not establish a maximum-latency improvement.

**The all-samples-below-5-ms target is not met.** The optimized p95 is 5.3087 ms, maximum 13.3282 ms, with durable-wait maximum 8.3926 ms. CPU-side wall-time outliers also remain; these samples cannot all be attributed to I/O. Issue #72 stays open. The original external host seed, generation parameters and entry point were not supplied, so this representative kernel fixture does not certify that host workload.

One uniform run per mode adds 30 samples each as a contrast, not as mixed-density acceptance. Its raw rows and all stage summaries are retained. Earlier exploratory runs and intermediate implementations are excluded from this final paired comparison; this is not a selection of fast rows from the final runs.

## Reproduction

Build the baseline and optimized example at the archived source commits into separate target directories. Preserve the executables, then run the versioned helper without concurrent local builds:

```sh
python apps/server/examples/dense_install_baseline/repeat.py BASELINE_EXE OPTIMIZED_EXE NEW_OUTPUT_DIRECTORY --baseline-source 03f8681d1d606bec7f2a79e4d5297d7698154ce2 --optimized-source f3d2238143024052f84453eb09142f5337a71726
```

The fixed seed and capacity changes are in `environment.json` and the shared workload. Runtime item capacity is 200000 and replica/checkpoint snapshot capacity 2000000 to accommodate dense pages and 600 sections; both modes use identical byte and synchronization policies. This is a kernel tick/replica fixture, not external gameplay systems or Socket traffic. Logical ledger peaks are recorded per process; process RSS is not measured.
