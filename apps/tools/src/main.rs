//! Reproducible developer probes and headless scenarios.
//!
//! The default build exposes dependency-free content validation and the M1
//! world scenario. Expensive ECS, Wasmtime, and graphics integrations are
//! feature-gated so ordinary code checks and CI do not download or initialize
//! those runtimes by accident.

use std::process::ExitCode;

use torse_content::{compile, m0_fixture_pack};
use torse_modules::resolve_modules;
use torse_types::Digest;

mod commit_scenario;
mod cooked_runtime;
mod cooked_tables;
mod exchange_scenario;
mod movement_scenario;
mod persistence_scenario;
mod recovery_scenario;
mod scenario;
mod tick_scenario;

fn main() -> ExitCode {
    let command = std::env::args().nth(1).unwrap_or_else(|| "help".to_owned());
    match command.as_str() {
        "content-fixture" => content_fixture(),
        "cook-base" => cooked_tables::command(std::env::args().skip(2)),
        "run-cooked" => cooked_runtime::command(std::env::args().skip(2)),
        "m1-scenario" => m1_scenario(),
        "commit-scenario" => commit_scenario(),
        "tick-scenario" => tick_scenario(),
        "exchange-scenario" => exchange_scenario(),
        "recovery-scenario" => recovery_scenario(),
        "movement-scenario" => movement_scenario(),
        "persistence-scenario" => persistence_scenario(),
        "ecs-probe" => ecs_probe(),
        "wasm-probe" => wasm_probe(),
        "gpu-probe" => gpu_probe(),
        "help" | "--help" | "-h" => {
            println!(
                "usage: torse-tools <content-fixture|cook-base|run-cooked|m1-scenario|commit-scenario|tick-scenario|\
                 exchange-scenario|recovery-scenario|movement-scenario|persistence-scenario|\
                 ecs-probe|wasm-probe|gpu-probe>\n\
                 experimental probes require their matching cargo feature"
            );
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("unknown command: {other}");
            ExitCode::FAILURE
        }
    }
}

fn movement_scenario() -> ExitCode {
    match movement_scenario::run() {
        Ok(report) => {
            println!("moved_entities={}", report.moved_entities);
            println!("discarded_preparations={}", report.discarded_preparations);
            println!("negative_x={:.3}", report.negative_x);
            println!("wall_x={:.3}", report.wall_x);
            println!("unloaded_x={:.3}", report.unloaded_x);
            println!("recovered_seq={}", report.recovered_seq);
            println!("continued_seq={}", report.continued_seq);
            println!("indexed_entities={}", report.indexed_entities);
            println!("replayed_equal={}", report.replayed_equal);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("movement scenario failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn persistence_scenario() -> ExitCode {
    match persistence_scenario::run() {
        Ok(report) => {
            println!("snapshot_bytes={}", report.snapshot_bytes);
            println!("sections={}", report.sections);
            println!("states={}", report.states);
            println!("entities={}", report.entities);
            println!("journal_records={}", report.journal_records);
            println!("journal_bytes={}", report.journal_bytes);
            println!("recovered_seq={}", report.recovered_seq);
            println!("continued_seq={}", report.continued_seq);
            println!("loaded_equal={}", report.loaded_equal);
            println!("recovered_equal={}", report.recovered_equal);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("persistence scenario failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn recovery_scenario() -> ExitCode {
    match recovery_scenario::run() {
        Ok(report) => {
            println!("snapshot_seq={}", report.snapshot_seq);
            println!("live_seq={}", report.live_seq);
            println!("recovered_seq={}", report.recovered_seq);
            println!("cells_checked={}", report.cells_checked);
            println!("restored_equal={}", report.restored_equal);
            println!("continued_seq={}", report.continued_seq);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("recovery scenario failed: {error}");
            ExitCode::FAILURE
        }
    }
}
fn exchange_scenario() -> ExitCode {
    match exchange_scenario::run() {
        Ok(report) => {
            println!("sections={}", report.sections);
            println!("states={}", report.states);
            println!("entities={}", report.entities);
            println!("cells_checked={}", report.cells_checked);
            println!("changes={}", report.changes);
            println!("commit_seq={}", report.commit_seq);
            println!("restored_equal={}", report.restored_equal);
            println!("replayed_equal={}", report.replayed_equal);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("exchange scenario failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn tick_scenario() -> ExitCode {
    match tick_scenario::run() {
        Ok(report) => {
            println!("ticks={}", report.ticks);
            println!("committed={}", report.committed);
            println!("rejected={}", report.rejected);
            println!("commit_seq={}", report.commit_seq);
            println!("overruns={}", report.overruns);
            println!("entities={}", report.entities);
            println!("moved_section_entities={}", report.moved_section_entities);
            println!("contested_state={}", report.contested_state);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("tick scenario failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn commit_scenario() -> ExitCode {
    match commit_scenario::run() {
        Ok(report) => {
            println!("committed={}", report.committed);
            println!("conflicts={}", report.conflicts);
            println!("commit_seq={}", report.commit_seq);
            println!("winner_seq={}", report.winner_seq);
            println!("contested_state={}", report.contested_state);
            println!("unrelated_committed={}", report.unrelated_committed);
            println!("dropped={}", report.dropped);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("commit scenario failed: {error}");
            ExitCode::FAILURE
        }
    }
}

fn m1_scenario() -> ExitCode {
    // The scenario asserts every M1 acceptance property it exercises, so a
    // nonzero exit means a property regressed rather than that a measurement
    // moved.
    match scenario::run() {
        Ok(report) => {
            println!("sections_loaded={}", report.sections_loaded);
            println!("dense_cells={}", report.dense_cells);
            println!("cell_bytes={}", report.cell_bytes);
            println!("blocks_edited={}", report.blocks_edited);
            println!("steps={}", report.steps);
            println!("blocked_steps={}", report.blocked_steps);
            println!("final_x={:.3}", report.final_x);
            println!("seam_x={:.3}", report.seam_x);
            println!("mesh_faces={}", report.mesh_faces);
            println!("stale_rejected={}", report.stale_rejected);
            println!("cache_bytes={}", report.cache_bytes);
            println!("unloaded_blocked={}", report.unloaded_blocked);
            println!("unloaded_edit_refused={}", report.unloaded_edit_refused);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("m1 scenario failed: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(feature = "ecs-probe")]
fn ecs_probe() -> ExitCode {
    // This workload is large enough to exercise query traversal and scheduling,
    // but it is a correctness/smoke probe rather than a benchmark.
    for executor in [
        torse_runtime::EcsProbeExecutor::SingleThreaded,
        torse_runtime::EcsProbeExecutor::MultiThreaded,
    ] {
        let report = torse_runtime::run_ecs_probe_with_executor(10_000, 100, executor);
        println!("executor={:?}", report.executor);
        println!("entities={}", report.entities);
        println!("ticks={}", report.ticks);
        println!("pool_threads={}", report.pool_threads);
        println!("observed_threads={}", report.observed_threads);
        println!(
            "observed_non_caller_threads={}",
            report.observed_non_caller_threads
        );
        println!("peak_concurrent_systems={}", report.peak_concurrent_systems);
        println!("instrumented_elapsed={:?}", report.elapsed);
        println!("checksum={}", report.checksum);
        println!("mismatched_entities={}", report.mismatched_entities);
        if report.mismatched_entities != 0 {
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}

#[cfg(not(feature = "ecs-probe"))]
fn ecs_probe() -> ExitCode {
    // A missing feature is an explicit configuration error. Returning success
    // would make an automated experiment think the probe actually ran.
    missing_feature("ecs-probe")
}

#[cfg(feature = "wasm-probe")]
fn wasm_probe() -> ExitCode {
    // Time compilation separately from calls so the recorded duration reflects
    // guest execution plus host-call overhead.
    match torse_mod_host::run_wasm_probe(10_000, 100) {
        Ok(report) => {
            println!("calls={}", report.calls);
            println!("iterations_per_call={}", report.iterations_per_call);
            println!("elapsed={:?}", report.elapsed);
            println!("checksum={}", report.checksum);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("wasm probe failed: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(feature = "wasm-probe"))]
fn wasm_probe() -> ExitCode {
    missing_feature("wasm-probe")
}

#[cfg(feature = "gpu-probe")]
fn gpu_probe() -> ExitCode {
    // The graphics probe is fully feature-gated; a headless/default build must
    // not request an adapter or create a native window.
    match torse_render::run_gpu_probe() {
        Ok(report) => {
            println!("adapter={}", report.adapter_name);
            println!("backend={}", report.backend);
            println!("device_type={}", report.device_type);
            println!("surface_format={}", report.surface_format);
            println!("present_mode={}", report.present_mode);
            println!("elapsed={:?}", report.elapsed);
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("gpu probe failed: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(feature = "gpu-probe"))]
fn gpu_probe() -> ExitCode {
    missing_feature("gpu-probe")
}

#[cfg(any(
    not(feature = "ecs-probe"),
    not(feature = "wasm-probe"),
    not(feature = "gpu-probe")
))]
fn missing_feature(feature: &str) -> ExitCode {
    // Keep the message directly actionable for a developer using the tool.
    eprintln!("experiment disabled; rebuild torse-tools with --features {feature}");
    ExitCode::FAILURE
}

fn content_fixture() -> ExitCode {
    // Compile the same fixture used by tests and print deterministic summaries.
    // The tool exits non-zero on diagnostics so it can be used in CI.
    match compile(m0_fixture_pack()) {
        Ok(content) => {
            println!("blocks={}", content.blocks.len());
            println!("state_schemas={}", content.state_schemas.len());
            println!("capabilities={}", content.capabilities.len());
            println!("actions={}", content.actions.len());
            println!("modules={}", content.modules.len());
            let modules = match resolve_modules(content.modules.values().cloned().collect()) {
                Ok(modules) => modules,
                Err(errors) => {
                    for error in errors {
                        eprintln!("{error}");
                    }
                    return ExitCode::FAILURE;
                }
            };
            println!(
                "module_order={}",
                modules
                    .order()
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            );
            println!("authority_digest={}", hex(&content.authority_digest));
            println!("presentation_digest={}", hex(&content.presentation_digest));
            ExitCode::SUCCESS
        }
        Err(errors) => {
            for error in errors {
                eprintln!("{error}");
            }
            ExitCode::FAILURE
        }
    }
}

fn hex(digest: &Digest) -> String {
    // Digest is intentionally a byte array at the shared boundary; this helper
    // only converts it for human-readable probe output.
    let mut output = String::with_capacity(64);
    for byte in digest.0 {
        use std::fmt::Write as _;
        write!(output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    output
}
