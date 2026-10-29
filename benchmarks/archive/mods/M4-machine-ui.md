# Minimal machine UI and authorized actions

日期：[20261024]
类型：验收
状态：待复验
环境：未记录
范围：未记录

## 目标

Date: 2026-10-24. Environment: Windows, Rust 1.93.1, server `mods` feature.

The B-side delivery provides a shared presentation model, authorized machine state,

an existing inventory action binding, and durable block notifications. Graphical

client integration remains pending; these results do not certify a rendered UI.

## 方法

未记录

## 结果

### Implementation

- `crates/mod_api/src/ui.rs` defines a fixed four-node description and rebuilds
  progress and inventory summaries from an authorized projection. Missing data
  clears the corresponding view. Summaries reject malformed or oversized slots
  and arithmetic overflow. No Wasm or server dependency is required by clients.
- The example machine component uses Server + Authorized. Machine, input and
  output each require independent object grants. A reader can see both summaries
  while still being denied `inventory:action/transfer` version 1.
- `DurableRule::action_events` derives notifications without calling guests.
  They freeze with the action and settle with its world transaction, receipt and
  event identities. A losing transaction registers no notification.
- Package block `behavior = true` binds only its own behavior rule. Notifications
  retain stable definition, finite values and position after removal. The existing
  WIT block events use reserved entity zero for block-only objects. Block traps
  settle terminally; wakeup retry persistence remains unchanged.
- `--mod DIRECTORY` loads the same packages on create and reopen. The explicit
  `--machine-operators` demo policy grants machine and port access to every peer.
  It is a local scenario policy, without an authentication claim.

### Reproduction

Run at the repository root:

```powershell
cargo test -p torse-mod-api
cargo test -p torse-server --features mods --test mod_durability
cargo test --workspace --features torse-server/mods
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --features torse-server/mods -- -D warnings
$env:RUSTDOCFLAGS = '-D warnings'
cargo doc --workspace --no-deps --features torse-server/mods
```

The complete workspace run passed 518 tests, with 1 existing ignored test.
Mod durability has 16 passing test entries, including two child-process helpers.
The new UI unit suite has 2 passing tests. Formatting, Clippy and Rustdoc passed.

| Scenario | Observed result |
|---|---|
| Two encoded clients | Identical visible machine progress and inventory summaries |
| Visitor | No machine or container component in seed or updates |
| Machine-only reader | Progress visible; both inventory summaries unavailable |
| Object reader without transfer permission | Same view, transfer rejected |
| Repeated action request | One transfer and one durable deduction; duplicate returns prior result |
| Revocation and reconnect | Old transport closed; new subscription obtains a complete new baseline |
| Process restart | New epoch reconstructs the same machine view without guest replay |
| Conflicting placements | Winning action produces one event; losing action produces none |
| Break notification | Original block data persists even after the cell becomes empty |
| Unbound/denied block action | No Mod notification |
| Four abrupt process exits | Action freeze, committed action, frozen notification and settled notification recover without duplicate dispatch |

The process tests call `process::exit` before destructors. In the action-frozen
case the recovered cell is still empty; the frozen plan later commits once. In
the committed case the cell and pending event both recover. Frozen and settled
notifications do not invoke the guest during replay or task completion.

For a real TCP baseline, use separate terminals and a fresh world directory:

```powershell
cargo run -p torse-server --features mods -- .work/machine-ui-world --listen 127.0.0.1:17831 --ticks 100 --mod examples/mods/machine --machine-operators
cargo run -p torse-server --features mods --example headless_client -- 127.0.0.1:17831 --mod examples/mods/machine
```

The observed run returned epoch 1, 2 sections, 3 entities and 3 visible component
sets. Machine 3 showed progress 3/4, an empty input and one `base:stone` in output.
After 100 ticks the server closed at durable sequence 1209. Reopening the same
directory started epoch 2 at tick 100 and closed after two ticks at sequence 1233.

### Limits

The description is compiled for the example machine, rather than downloaded as
an arbitrary package UI tree. Clients must load matching content and clear the
view and pending intents on disconnect/reset. Existing graphical clients still
need the model/action adapter and matching content setup; dual graphical client
acceptance and screenshots remain pending. The notification hook covers native
action place/break mutations, not arbitrary trusted or guest writes. Event count,
phase work and runtime budgets remain finite. Protocol, WIT and execution envelope
versions remain unchanged; changed content compatibility digests reject old demo
worlds without migration.

## 复现

未记录

## 边界

未记录
