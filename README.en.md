<p align="center">
  <img src="figures/torse-logo.png" alt="Torse logo" width="420">
</p>

# Torse

[简体中文](README.md) | English

Torse is a Rust engine core and development framework for open-world voxel games. It provides world storage, fixed-step simulation, persistence, network synchronization, and mod support. The repository includes a graphical client, a standalone server, and a sample game so you can run the engine and see how these systems work together.

Torse is in early development. Public APIs, save formats, and the network protocol are still evolving. The multiplayer demo uses a small, bounded world. See the [validation and benchmark records](benchmarks/README.md) for the workloads and scales tested so far.

## Quick start

Install [Rustup](https://rustup.rs/), then clone the repository. Run the commands below from the repository root. Rustup selects Rust 1.93.1 from [rust-toolchain.toml](rust-toolchain.toml). Native Windows builds require the MSVC build tools. The graphical client requires a GPU and drivers that support Vulkan, DirectX 12, or Metal.

```sh
git clone https://github.com/TorseWorks/Torse.git
cd Torse
```

### Run the local demo

```sh
cargo run -p torse-client -- --window
```

| Action | Default input |
|---|---|
| Move and look around | WASD and mouse |
| Ascend and descend while flying | Space and Left Shift |
| Switch between flying and walking | F5. Press Space to jump while walking. |
| Break and place blocks | Left and right mouse buttons |
| Select a hotbar slot and open the inventory | 1 through 9 and Tab |
| Pick up and drop items | F and Q |
| Adjust movement speed | Mouse wheel |
| Open the menu and preview the machine panel | Esc and E |
| Show debug info and take a screenshot | F8 and F12 |

You can also render a frame to a PNG. Offscreen rendering still requires a GPU.

```sh
cargo run -p torse-client -- --capture out.png --width 960 --height 720 --radius 2
```

See the [client guide](apps/client/README.md) for more options and key bindings.

### Run the multiplayer demo

Start the server in one terminal. This creates a local demo world with two players.

```sh
cargo run -p torse-server -- .work/demo-world --listen 127.0.0.1:7777 --fill --editors --players 2
```

Leave the server running and start the client in another terminal. Run the client command a second time to see edits to the same world in both windows.

```sh
cargo run -p torse-client -- --window --connect 127.0.0.1:7777 --walk
```

Use `--fill` when you create a world and omit it when reopening the same directory. `--editors` grants edit access within the demo area. Without it, clients connect as read-only guests. The plaintext demo listens only on the loopback interface. Remote connections require mutual TLS and configured user permissions. See the [server guide](apps/server/README.md).

On a machine without a GPU, use the headless example to connect to the running server.

```sh
cargo run -p torse-server --example headless_client -- 127.0.0.1:7777
```

## Current capabilities

| Area | What's implemented |
|---|---|
| World and rendering | Section-based terrain storage and streaming, deterministic generation, collision detection, meshing, and textured rendering. Hosts can stream terrain and lighting updates for a separate, read-only view of distant terrain. |
| Simulation and persistence | Entity and component updates run on fixed ticks. The engine authorizes, prepares, and commits world changes atomically. Journals and checkpoints restore world state, execution progress, and request deduplication state. |
| Multiplayer | The authoritative server replicates world changes according to session permissions. It supports multiplayer editing and server-authoritative walking with client prediction and reconciliation. |
| Content and gameplay | Content definitions and optional modules make up the sample game. With finite inventory, placing blocks consumes items and breaking blocks spawns drops that fall, collide with terrain, and can be picked up. |
| Mods | WebAssembly components submit typed behavior plans. The host checks permissions and commits world, component, and inventory updates. The machine mod includes a runnable headless scenario. The graphical client's machine panel is a preview. |
| Resource limits | The client and server support shared logical memory budgets, backpressure, and eviction of cached data. Diagnostic APIs report queue activity and execution progress. |

## Repository layout

| Path | Contents |
|---|---|
| [apps](apps/README.md) | Graphical client, standalone server, and developer tools |
| [crates](crates/README.md) | Libraries for the world, simulation, rendering, storage, protocol, and networking |
| [crates/mod_sdk](crates/mod_sdk/src/lib.rs) | SDK for mod authors |
| [modules](modules/README.md) | Optional modules, including inventory, items, and a skinnable UI |
| [games/base](games/base/README.md) | Sample game content, rules, and assets |
| [examples/mods](examples/mods/README.md) | Runnable WebAssembly mod examples |
| [tests](tests/README.md) | Cross-module scenarios, fault injection, and fixtures |
| [benchmarks](benchmarks/README.md) | Reproduction steps, screenshots, and measurements |

Most linked guides are in Simplified Chinese.

## Development and contributions

For code changes, run the formatter check, Clippy, and workspace tests before submitting.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
```

The developer tools include standalone scenarios for content, transactions, and recovery.

```sh
cargo run -p torse-tools -- content-fixture
cargo run -p torse-tools -- commit-scenario
cargo run -p torse-tools -- recovery-scenario
```

To write a mod, start with the [machine example](examples/mods/README.md#运行机器示例). Install the Wasm target, then check the example package.

```sh
rustup target add wasm32-unknown-unknown
cargo run -p torse-mod-host --features package --bin torse-mod -- check examples/mods/machine
```

Bug reports, documentation fixes, tests, and code improvements are welcome. Open an issue first to discuss new features or changes to public APIs, save formats, or the network protocol. See the [contribution guide](CONTRIBUTING.md) for setup, checks for optional features, and the review process.

## License

Torse is licensed under the GNU General Public License, version 3 only (`GPL-3.0-only`). Unless otherwise stated, this license applies to the original code, documentation, examples, and assets in the repository. Third-party materials retain their own license notices. See [LICENSE](LICENSE) for the full terms.
