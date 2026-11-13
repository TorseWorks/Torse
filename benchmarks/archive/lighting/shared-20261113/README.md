# Shared lighting validation

Measured source: `adb65812491e555e61c5ab84003f628895886978`. Windows 11, Core i9-14900HX, RTX 4060 Laptop GPU, Rust 1.93.1. All archived samples use Release. Commands and measurement boundaries are in [environment.json](environment.json).

## Incremental edits

[Twelve edits](edits.csv) alternate source addition/removal and roof closing/opening in a deterministic mixed-density 16-cell-side, 32-cell-high input. Every published sky and emission cell equals a fresh cold solve after each edit. Two consumers share the retained solver; the queue returns to zero after each edit.

| Wall time, ms | Minimum | Median | Maximum |
|---|---:|---:|---:|
| Incremental solve | 1.5730 | 2.2224 | 4.0786 |
| Matching cold solve | 6.1147 | 6.4840 | 7.7487 |

The initial cold shared publication took 6.4209 ms. A source addition visited 18,236 frontier entries, removal 49,919, and either roof edit 62. Each warm solve also scans its bounded input region to locate changed cells; these measurements do not claim edit cost independent of input size. Retained solver/output accounting peaked at 2,817,776 bytes. This is one process with twelve edits, not a multi-process performance distribution.

## Saved terrain route

The public file-backed fixture uses a 384-cell world height and dense saved pages outside a two-section simulation window. Its fixed x route is `2,3,4,5,6,5,4,3,2`. Each display window contains one section and uses the same input, calculation and transport limits on every step. Samples include forward movement, reversal and revisiting.

| CPU route wall time, ms | Minimum | Median | Maximum |
|---|---:|---:|---:|
| Terrain available | 0.3714 | 0.6108 | 1.7375 |
| Light available | 14.7572 | 15.1398 | 15.8909 |
| Per-sample longest Hub poll | 0.9464 | 1.1747 | 1.3552 |

All nine [CPU samples](cpu-route.csv) are retained, including `complete=false`: missing upper/neighbor sections are unknown rather than air. The results are conservative, version-checked publications, not complete-world light. There were no unfinished timing samples. The CPU fixture's draw column is explicitly `NA`; a separate end-to-end GPU route measures that stage.

## Terrain to offscreen draw

[Nine GPU samples](gpu-route-release.csv) exercise the file-backed host, encoded messages, terrain mirror, derived scene, mesher, upload, Vulkan render and readback. Renderer initialization is excluded. Rendering is 128×128. Each completed draw includes a GPU readback and PNG write, which makes it observable but adds synchronous overhead.

| GPU route wall time, ms | Minimum | Median | Maximum |
|---|---:|---:|---:|
| Terrain receipt to first completed draw/readback | 11.6969 | 12.6167 | 15.9019 |
| Route request to light receipt | 25.4802 | 27.0204 | 31.5934 |
| Light receipt to completed draw/readback | 12.9737 | 13.5064 | 14.5563 |

The in-process measurement pauses host polling during rendering. Therefore its light-delivery time includes that pause and must not be compared directly with the CPU-only route as an independent server cost. It has no monitor presentation timestamp and is not a frame-rate or tail-latency guarantee. All nine conservative boundary results and all timing samples remain in the CSV.

Both routes reached pending peak 1 and returned to zero. Shared output/solver accounting peaked at 14,156,548 bytes, native input at 2,388 bytes, and fixed host reservation was 176,427,008 bytes, including configured cache/work/I/O headroom. The page worker permits one in-flight read. Logical counts exclude allocator overhead, transport buffers, scene/GPU memory and process RSS. See the adjacent metadata files for the raw counters.

## Correctness and failure coverage

The normal regressions cover two consumers receiving the same publication, pending output on one peer while another completes, ready light attached to the first terrain page, cache denial without repeated calculation or connection loss, content-preserving unload/reload, corrupt saved pages and explicit retry, missing neighbors, sky obstruction, cross-section source removal, forty edits compared cell-by-cell with cold solves, and unrelated targets retaining their publication.

Receiver tests atomically reject malformed, out-of-order, wrong-binding and over-budget input, discard mismatched terrain/light identities, and prevent an older light publication from replacing a newer one. A client mesh test verifies actual vertex changes and near-replica priority. The separate Vulkan pixel regression observed 6,722 changed pixels after shared light arrived; it is a correctness observation, not a timing distribution.

Application protocol is explicitly 7; both endpoints must upgrade. Storage formats are unchanged. The display-light feature is opt-in. Immutable saved files are checked on read; explicit refresh rechecks cached backing files. There is no filesystem watcher or operating-system I/O cancellation.
