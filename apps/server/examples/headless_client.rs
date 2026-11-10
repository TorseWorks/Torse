//! Receive an authorized baseline from the standalone server, without a GPU.
use std::time::{Duration, Instant};
use torse_codec::wire;
use torse_net::{ReplicaMirror, TcpTransport, Transport, TransportLimits};
use torse_protocol::PROTOCOL_VERSION;
use torse_server::bootstrap as base;
use torse_server::network::{ClientMessage, ServerMessage};
use torse_types::Fault;

fn run() -> Result<(), Fault> {
    let mut args = std::env::args().skip(1);
    let address = args.next().unwrap_or_else(|| "127.0.0.1:7777".into());
    let mut mod_dirs = Vec::new();
    while let Some(arg) = args.next() {
        if arg != "--mod" {
            return Err(Fault::invalid_data("unknown client argument"));
        }
        mod_dirs
            .push(std::path::PathBuf::from(args.next().ok_or_else(|| {
                Fault::invalid_data("missing mod directory")
            })?));
    }
    let stream =
        std::net::TcpStream::connect(address).map_err(|e| Fault::invalid_data(e.to_string()))?;
    let limits = TransportLimits {
        frame: 8_388_608,
        queued_frames: 32,
        queued_bytes: 16_777_216,
        poll_bytes: 262_144,
    };
    let mut transport = TcpTransport::new(stream, limits)?;
    let compatibility = if mod_dirs.is_empty() {
        base::world()?.0.coordinator().compatibility()
    } else {
        #[cfg(feature = "mods")]
        {
            torse_server::mods::mod_world(&mod_dirs, true, &[])?
                .world
                .coordinator()
                .compatibility()
        }
        #[cfg(not(feature = "mods"))]
        return Err(Fault::invalid_data("--mod requires the mods feature"));
    };
    for message in [
        ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            compatibility,
        },
        ClientMessage::Subscribe {
            sections: base::scope().sections.into_iter().collect(),
        },
    ] {
        let mut bytes = Vec::new();
        wire::encode(&message, &mut bytes, base::budget(), base::codec_limits())?;
        transport.send(bytes)?;
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut welcomed = None;
    while Instant::now() < deadline {
        transport.poll()?;
        while let Some(bytes) = transport.receive()? {
            let message: ServerMessage =
                wire::decode(&bytes, base::budget(), base::codec_limits())?;
            match message {
                ServerMessage::Welcome { binding, session } => {
                    println!("session={} epoch={}", session.0, binding.epoch.0);
                    welcomed = Some(binding);
                }
                ServerMessage::Seed(seed) => {
                    if welcomed != Some(seed.binding) {
                        return Err(Fault::invalid_data("baseline without matching welcome"));
                    }
                    let replica = ReplicaMirror::from_seed(seed)?;
                    println!(
                        "baseline={} sections={} entities={} component_sets={}",
                        replica.through.seq.0,
                        replica.state().sections.len(),
                        replica.state().entities.len(),
                        replica
                            .state()
                            .entities
                            .iter()
                            .map(|e| e.components.len())
                            .sum::<usize>()
                    );
                    for entity in &replica.state().entities {
                        if let Some(view) = torse_mod_api::ui::MachineView::rebuild(
                            &torse_mod_api::ui::machine(),
                            entity.id,
                            |id, schema| {
                                replica
                                    .state()
                                    .entities
                                    .iter()
                                    .find(|e| e.id == id)?
                                    .components
                                    .get(schema)
                            },
                        ) {
                            println!(
                                "machine={} progress={}/{} input={:?} output={:?}",
                                view.entity.0, view.progress, view.steps, view.input, view.output
                            );
                        }
                    }
                    return Ok(());
                }
                ServerMessage::Rejected(reason) => {
                    return Err(Fault::invalid_data(format!("request rejected: {reason:?}")));
                }
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Err(Fault::invalid_data("baseline timeout"))
}
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("client failed: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
