# Ready-column lighting comparison

This file-backed synthetic fixture keeps the origin resident and parks 24 mixed-density sections at negative X coordinates. Both versions subscribe through the same strict `HostLightField::subscribe` entry point, with four captures per poll and 1,000,000 solver work units. The baseline is main `bdae92b`; candidate file hashes and machine details are in [environment.json](environment.json). The measurement test is copied unchanged onto the baseline.

| Metric | Baseline | Candidate |
| --- | ---: | ---: |
| Near-column publication, median | 73.930 ms | 2.351 ms |
| All 25 publications, median | 73.932 ms | 66.683 ms |
| Per-sample longest poll, median | 3.072 ms | 3.462 ms |
| Longest observed poll | 6.258 ms | 4.142 ms |
| Distant target inputs verified when near column publishes | 24 | 0 |
| Publication/retained-solver cache peak | 70,444,400 B | 70,444,400 B |
| Native input cache peak | 19,472 B | 19,472 B |

All twelve samples produce the same SHA-256 over the sorted targets and final emitted/sky levels, excluding publication generations. The exact rows are in [baseline.csv](baseline.csv) and [candidate.csv](candidate.csv). Each sample finishes all 25 publications; no incomplete samples were discarded. Native input/cache counters do not include the separately reserved file decoder and result queues, solver workspace or process RSS. The candidate reserves a maximum of `min(pages_per_poll, 4)` outstanding decoded pages, plus decoder overhead, rather than the baseline's single read.

Run `cargo test -p torse-server --test light_column_measurement --release --locked -- --ignored --nocapture`. Six candidate samples were followed by six baseline samples on the same machine. OS file caching was left enabled, with no cache flush or warmup exclusion. The small sample count and non-interleaved order do not establish a general tail-latency improvement: the median longest poll increased, while nearby readiness and publication no longer wait for distant verification. This fixture measures host lighting only, without a network, simulation cadence, client drawing or a full-height display route. Sparse radius-32 admission and terrain-only client calculation are covered separately by behavioral tests.
