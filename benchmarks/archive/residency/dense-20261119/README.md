# Current dense column install latency

Measured [20261119] at `a9071182b322c90c1d4241ff1ea55d21e3282339` on Windows 11 / Intel Core i9-14900HX, with an NVMe WD PC SN560, NTFS and the Balanced power plan. The implementation is unchanged. This is a fresh observation of the existing mixed-density kernel fixture, not a paired optimization comparison or the original external host workload.

**Issue #72 remains open.** Of 240 final mixed-density columns, nine took at least 5 ms. Kernel p95 was 4.4280 ms, p99 6.5195 ms and maximum 8.5499 ms. All nine over-target samples spent most of their time in the journal synchronization call. That call includes OS scheduling and cannot identify device latency separately.

## Method

Eight independent Release processes each initialized a 5-by-5 window of 600 sections, then ran 200 paced 50 ms ticks at a virtual 10.9 cells/s. Each process installed 30 columns of 24 pages, including 16 dense pages and three attached entities, while encoding/applying authorized replica updates and unloading the rear window. One final uniform process adds 30 contrast samples. No concurrent local builds or tests were requested during measurement.

Every measured column asserted one journal append, one journal sync and one flush request. Every process verified its authorized mirror against a fresh projection, then reopened the directory and checked the complete world snapshot and entity allocator. These checks passed for all nine final processes and the pilot. No durability settings, wire format, API or runtime code changed.

An initial 30-column pilot preceded the fixed final series. Its raw rows remain in [pilot](pilot/samples.csv), outside the final aggregates. All 270 final samples, including all outliers, are retained. [Environment](environment.json), [per-process results](runs.json) and [full stage summaries](summary.json) accompany the CSV files.

## Results

Final mixed-density samples; milliseconds, nearest-rank quantiles. Timings are wall-clock observations, not hardware CPU-cycle measurements. Stage maxima can belong to different samples.

| Stage | Mean | P95 | P99 | Maximum |
|---|---:|---:|---:|---:|
| Host total | 3.1203 | 4.9004 | 6.9509 | 9.0433 |
| Kernel total | 2.6632 | 4.4280 | 6.5195 | 8.5499 |
| Page preparation | 0.1268 | 0.1776 | 0.2545 | 0.2643 |
| Validation/preparation | 0.4418 | 0.6579 | 0.9230 | 1.1154 |
| Journal encoding | 0.0723 | 0.1186 | 0.1562 | 0.2931 |
| Publication/remainder | 0.0549 | 0.1157 | 0.2033 | 0.2866 |
| Durable wait | 1.9664 | 3.5279 | 5.8921 | 7.8740 |
| Journal append | 0.0351 | 0.0972 | 0.1422 | 0.5845 |
| Journal sync | 1.9297 | 3.5120 | 5.7811 | 7.8564 |
| Kernel minus durable wait | 0.6968 | 1.0448 | 1.3893 | 1.5754 |

The [slowest column](mixed-05/samples.csv#L18) took 8.5499 ms, including 7.8564 ms in journal sync, 0.4253 ms in validation/preparation and 0.0723 ms in journal encoding. The largest [non-durable remainder](mixed-02/samples.csv#L30) was 1.5754 ms. Mean journal payload was 14,875 bytes per mixed column, unchanged across the final mixed samples.

Uniform contrast: kernel mean 0.7567 ms, p95 1.2218 ms and maximum 1.4703 ms, with zero of 30 columns reaching 5 ms. These rows do not establish mixed-density acceptance.

## Reproduction

Check out the measured source and build with Rust 1.93.1:

```sh
cargo build --locked --release -p torse-server --example dense_install_baseline
```

Set `CARGO_PROFILE_RELEASE_DEBUG=0` and `CARGO_INCREMENTAL=0` before building. Invoke the resulting executable in nine fresh processes, using a new output directory each time. Run eight mixed processes without extra flags, followed by one with `--uniform`:

```sh
target/release/examples/dense_install_baseline NEW_OUTPUT_DIRECTORY
target/release/examples/dense_install_baseline NEW_UNIFORM_OUTPUT_DIRECTORY --uniform
```

The existing fixture writes `samples.csv` and `environment.txt` and asserts the correctness conditions above. Combine the eight mixed CSV files separately from the uniform CSV. For each timing column, sort nanoseconds and use index `ceil(p * n) - 1` for quantiles. The derived non-durable remainder is computed per row as `kernel_total_ns - durable_wait_ns` before sorting. The existing [paired-run helper](../../../../apps/server/examples/dense_install_baseline/repeat.py) uses the same aggregation formula; this archive did not run an old-source comparison.

## Remaining scope

This uses the same CPU model as the original report, but the original external host seed, generation parameters and entry point remain unavailable. It cannot certify the original workload or a speedup against an older source. The fixture includes kernel ticks and authorized replica work, but no external gameplay workload or Socket traffic. Logical memory peaks are in each process's `environment.txt`; process RSS was not sampled.

The observed non-durable stages leave headroom below 5 ms, while some sync calls alone exceed 5 ms. Further CPU-copy changes cannot guarantee the requested end-to-end bound. No device or scheduler trace was captured, so the underlying cause of sync variability remains unassigned. Any future asynchronous completion API would need to preserve the distinction between accepted and durable results, failure handling and exact recovery; that change is not part of this measurement.
