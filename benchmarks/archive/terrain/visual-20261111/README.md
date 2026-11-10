# Independent display terrain

Six fresh Release processes each traverse 35 windows over 24 saved, edited dense sections. The simulation retains two sections; each display window covers eight. Every result is compared with its saved terrain projection, with no entities or authority cursor advancement. Setup includes real file pages and parked entities; timed navigation is read-only.

All 210 windows reached 8/8 CPU mirror coverage. Median/p95/maximum first-ready latency: 0.22385/0.4451/0.6569 ms. Full-window latency: 0.22395/1.3932/2.0906 ms. Maximum single network poll: 0.1747 ms. These are host-loop observations including decoding/application and scheduler variability, with immediate availability possible for cache hits. They are not monitor presentation or network round-trip times.

Peak observed server cache charge was 100424 bytes; its fixed logical reservation was 34221056 bytes, including bounded I/O and output work. Client cache charge peaked at 101448 bytes. Transport, derived scene, GPU and process RSS are separate and excluded. Samples and source fingerprint are in the adjacent CSV and JSON files; p95 uses the sorted floor-index sample.

The separate Vulkan regression `cargo test -p torse-client --features gpu-tests --locked display_page_mesh_reaches -- --nocapture` compares blank and display-terrain offscreen readbacks and requires over 100 changed pixels. On the local NVIDIA GeForce RTX 4060 Laptop GPU it passed; one debug observation for display application, mesh, upload, readback and PNG write was 10.8066 ms. Renderer setup is excluded. This is a correctness check with file I/O, not a rendering performance distribution or physical presentation measurement.

Server regressions additionally cover loading saved terrain into the near authority, editing it, leaving and saving, and revisiting; overlapping forward/reverse windows; process reopen; superseded reads; missing/corrupt/repair; output backlog; cache and shared-pool denial; principal revocation. Mirror regressions reject illegal and discontinuous messages atomically and prevent delayed display data from undoing newer near terrain.
