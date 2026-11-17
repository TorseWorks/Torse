# Full-height display window acceptance

The public `apps/server/tests/visual_windows.rs` fixture prepares 102,958 immutable file-backed pages through bounded durable parking batches. Two native sections remain resident throughout. Saved pages contain known air and a deterministic pattern of solid sections below Y=0. A MemoryTransport connection receives the full 65x65x24 window, moves one column, turns back, shrinks to 33x33x24, then revisits the full window. Every final page is compared with its expected content; every stage checks that simulation residency stays at two sections.

Run `cargo test -p torse-server --test visual_windows --release --locked -- --ignored --nocapture`. Preparation is outside the route timers. [Raw output](route.txt), [rows](route.csv) and [environment/source fingerprints](environment.json) preserve the complete final run. All five samples completed; none were discarded as unfinished.

| Stage | Desired pages | Coverage | First available ms | Converged ms | Longest poll ms |
| --- | ---: | ---: | ---: | ---: | ---: |
| Cold full window | 101400 | 100% | 36.185 | 35491.237 | 7.731 |
| Forward one column | 101400 | 100% | 99.420 | 4011.131 | 29.141 |
| Turn back | 101400 | 100% | 97.220 | 241.289 | 29.036 |
| Shrink | 26136 | 100% | 52.053 | 53.267 | 10.612 |
| Revisit full window | 101400 | 100% | 60.220 | 5433.966 | 24.066 |

The candidate queue peaked at 101400 entries. Logical server page storage peaked at 435239512 bytes, and logical receiver storage at 480561940 bytes, including its current desired metadata. The configured server reservation was 739770424 bytes; the receiver's independent page/metadata reservations total 671088640 bytes. These are conservative logical reservations, not RSS. The single process's maximum observed Windows `PeakWorkingSetSize` was 222470144 bytes and `PeakPagefileUsage` was 221032448 bytes, sampled through `GetProcessMemoryInfo` every 50ms over preparation and the route (2469 samples). Uniform storage explains part of the difference between physical and logical figures.

The earlier 16384-page ceiling cannot admit this workload; no comparable baseline throughput or speedup is claimed. This is one fixed route with OS caching enabled, uncontrolled affinity/power and an in-process transport. First availability includes scope admission; longest poll excludes `set_visual_interest` admission and client application. Full control messages still require work proportional to their metadata, and admission/range changes can cause roughly 100ms pauses. Cold convergence remains tens of seconds. The experiment proves complete bounded delivery and consistent content, not sustained gameplay throughput, GPU drawing or a display-present timestamp.

Ordinary regressions separately cover encoded/decoded control byte, item and codec-frame denial before activation; failed movement retaining the old generation; large receiver metadata admission with a tiny page cache; cache denial and malformed/old reset atomicity; permission boundaries; superseded asynchronous reads; corruption, source transitions and retry. Default global budgets and default window sizes are not enlarged. Applications explicitly configure the new `control_bytes` and `metadata_bytes` fields, and sufficient frame/item budgets, before requesting a large window. Protocol 8 is unchanged.
