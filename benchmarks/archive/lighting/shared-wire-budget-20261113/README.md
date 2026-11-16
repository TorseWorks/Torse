# Shared light delivery with full wire-budget checks

Source `22876ba6d8a4592aa9b3a4725d307aa33de7e015`. This reruns both routes after adding full message cost checks for inline and later light batches. A combined terrain/light frame exceeding the wire item or byte budget now sends terrain first and defers light without disconnecting; a dedicated 9,000-item-budget regression verifies this path. Later batches shrink before advancing delivery progress.

Environment, commands, route and timing limitations are in [environment.json](environment.json). The workload and all limits match the [initial record](../shared-20261113/README.md); that record also contains the unchanged solver's twelve-edit cold/incremental comparison. Each route below contains nine samples from one fresh Release process. All boundary-incomplete results remain `complete=false`, and no timing sample was discarded.

| Wall time, ms | Minimum | Median | Maximum |
|---|---:|---:|---:|
| CPU route: terrain receipt | 0.3896 | 1.5644 | 2.0860 |
| CPU route: light receipt | 13.5853 | 13.9290 | 14.5098 |
| CPU route: per-sample longest Hub poll | 1.1156 | 1.1784 | 1.6055 |
| GPU route: terrain receipt to first draw/readback | 12.0652 | 12.4859 | 16.4821 |
| GPU route: request to light receipt | 26.3007 | 26.9612 | 32.1824 |
| GPU route: light receipt to draw/readback | 13.6870 | 14.1346 | 16.9024 |

GPU timings include scene application, meshing, upload, 128x128 Vulkan rendering, readback and PNG writing; initialization is excluded. The in-process fixture stops host polling during rendering, so its delivery time includes that pause. These are offscreen observations, not monitor presentation timestamps, independent server processing time or a frame-rate guarantee.

Both routes reached queue peak 1 and returned to zero. Retained solver/output bytes peaked at 14,156,548, native input bytes at 2,388, and the fixed host logical reservation was 176,427,008 bytes. I/O finished with no pending read. These exclude allocator overhead, transport buffers, scene/GPU memory and RSS. Raw counters are preserved beside the CSV files. CPU-only draw entries stay explicitly NA; the GPU route supplies draw samples separately.
