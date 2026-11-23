//! Complete item-body ticks with file durability and authorized replicas.
#[path = "drop_baseline/workload.rs"]
mod workload;

use std::net::{TcpListener, TcpStream};
use std::time::Duration;
use std::{error::Error, fs, io::Write, path::PathBuf, time::Instant};
use torse_codec::wire;
use torse_diagnostics::{SerialProfiler, Stage};
use torse_net::{ReplicaMirror, TcpTransport, Transport, TransportLimits};
use torse_protocol::PROTOCOL_VERSION;
use torse_server::network::{ClientMessage, NetworkHub, PeerPolicy, ServerMessage};
use torse_server::{bootstrap as base, execution::*, *};
use torse_sim::action::{ActionTarget, ActorContext};
use torse_types::*;

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let output = PathBuf::from(args.next().ok_or("missing output directory")?);
    let bodies: usize = args.next().ok_or("missing body count")?.parse()?;
    let workers: usize = args.next().ok_or("missing worker count")?.parse()?;
    let ticks: u64 = args.next().ok_or("missing measured ticks")?.parse()?;
    if !(1..=64).contains(&bodies) || ![0, 1, 4].contains(&workers) || !(1..=300).contains(&ticks) {
        return Err("expected 1..64 bodies, 0/1/4 workers and 1..300 ticks".into());
    }
    let peers: usize = args.next().ok_or("missing peer count")?.parse()?;
    let pause = match args.next().as_deref() {
        None => false,
        Some("--pause-last") if peers > 1 => true,
        _ => return Err("invalid pause option".into()),
    };
    if ![1, 4, 8].contains(&peers) {
        return Err("expected 1, 4 or 8 peers".into());
    }
    fs::create_dir(&output)?;
    let path = output.join("world");
    fs::create_dir(&path)?;
    let (world, actions, catalog) = workload::world(bodies, false);
    let compatibility = world.coordinator().compatibility();
    let mut policy = base::file_policy();
    policy.journal.max_pending_records = 16;
    let codec = base::Codec::new(base::codec_limits())?;
    let mut server = base::Files::create_file(
        WorldDirectory::acquire(&path, base::WORLD, 65_536)?,
        compatibility,
        codec,
        policy,
        |binding| {
            ExecutionHost::new(
                binding,
                world,
                actions,
                workload::rules(catalog),
                base::execution_limits(),
            )
        },
    )?;
    if workers != 0 {
        server.set_parallel_planning(Some(PlanningConfig {
            workers,
            max_in_flight: 4,
        }))?;
    }
    let mut hub = NetworkHub::new(server.binding(), base::network_limits())?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let limits = TransportLimits {
        frame: 8_388_608,
        queued_frames: 32,
        queued_bytes: 16_777_216,
        poll_bytes: 262_144,
    };
    let mut clients = Vec::new();
    for _ in 0..peers {
        let stream = TcpStream::connect(listener.local_addr()?)?;
        let (peer, _) = listener.accept()?;
        let actor = server.authority()?.world().entities().iter().fold(
            ActorContext::new(base::key("benchmark:viewer")),
            |actor, (_, entity)| actor.target(ActionTarget::Entity(entity.id)),
        );
        let id = hub.add(
            TcpTransport::new(peer, limits)?,
            PeerPolicy {
                actor,
                scope: base::scope(),
            },
            Tick(0),
        )?;
        let mut transport = TcpTransport::new(stream, limits)?;
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
        clients.push(Client {
            id,
            transport,
            mirror: None,
            session: None,
        });
    }
    let diagnostics = QueueDiagnostics::enabled(4096)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while clients.iter().any(|client| client.mirror.is_none()) {
        if Instant::now() >= deadline {
            return Err("TCP subscription setup timed out".into());
        }
        hub.poll(&mut server)?;
        for client in &mut clients {
            client.receive()?;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let profiler = SerialProfiler::new(0);
    server.set_serial_profiler(profiler.clone());
    let mut csv = fs::File::create(output.join("ticks.csv"))?;
    write!(
        csv,
        "tick,tick_ns,replica_batches,payload_bytes,native_jobs,server_outgoing_bytes,paused"
    )?;
    for stage in Stage::ALL {
        write!(csv, ",{}_ns", stage.name())?;
    }
    writeln!(csv)?;
    let mut prior_jobs = 0;
    let mut replica_batches = 0;
    for tick in 0..5 + ticks {
        profiler.clear();
        let started = Instant::now();
        let mut batches = 0;
        let mut bytes = 0;
        let mut outgoing = 0;
        let paused = pause && (15..45).contains(&tick);
        let mut complete = false;
        for _ in 0..256 {
            let batch = server.drive_batch(&mut hub, 16)?;
            assert!(batch.committed != 0 && batch.blocked.is_none());
            hub.poll(&mut server)?;
            for (index, client) in clients.iter_mut().enumerate() {
                if let Diagnostic::Available(Some(peer)) =
                    hub.peer_queue_diagnostics(client.id, &diagnostics)
                    && let Diagnostic::Available(transport) = peer.transport
                {
                    outgoing = outgoing.max(transport.outgoing.bytes);
                }
                if !paused || index + 1 != peers {
                    let (received, payload) = client.receive()?;
                    batches += received;
                    bytes += payload;
                }
            }
            if server.authority()?.at_tick_boundary() {
                assert_eq!(server.authority()?.resume_point().tick, Tick(tick + 1));
                complete = true;
                break;
            }
        }
        assert!(complete);
        replica_batches += batches;
        let elapsed = started.elapsed().as_nanos();
        let jobs = server.authority()?.native_planning_stats().jobs;
        if tick >= 5 {
            write!(
                csv,
                "{tick},{elapsed},{batches},{bytes},{},{outgoing},{paused}",
                jobs - prior_jobs
            )?;
            for stage in profiler.totals() {
                write!(csv, ",{}", stage.total_ns)?;
            }
            writeln!(csv)?;
        }
        prior_jobs = jobs;
    }
    assert!(replica_batches > 0);
    let state = workload::snapshot(server.authority()?);
    fs::write(output.join("state.bin"), &state)?;
    let fresh = server.open_replica(
        clients[0].session.ok_or("welcome missing")?,
        base::scope(),
        base::budget(),
    )?;
    let reference = ReplicaMirror::from_seed(fresh)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while clients
        .iter()
        .any(|client| client.mirror.as_ref().unwrap().state() != reference.state())
    {
        if Instant::now() >= deadline {
            return Err("TCP replicas did not converge".into());
        }
        hub.poll(&mut server)?;
        for client in &mut clients {
            client.receive()?;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    for client in &clients {
        let mirror = client.mirror.as_ref().unwrap();
        assert_eq!(mirror.state(), reference.state());
    }
    let mut bytes = Vec::new();
    wire::encode(
        reference.state(),
        &mut bytes,
        base::budget(),
        base::codec_limits(),
    )?;
    fs::write(output.join("replica.bin"), bytes)?;
    drop(server);
    let (world, actions, catalog) = workload::world(bodies, false);
    let recovered = base::Files::open_file(
        WorldDirectory::acquire(&path, base::WORLD, 65_536)?,
        compatibility,
        codec,
        policy,
        |binding| {
            Ok(ExecutionRecovery::new(
                binding,
                world.registry(),
                world.schemas(),
                actions,
                workload::rules(catalog),
                base::execution_limits(),
            ))
        },
    )?;
    assert_eq!(workload::snapshot(recovered.authority()?), state);
    Ok(())
}

struct Client {
    id: u128,
    transport: TcpTransport,
    mirror: Option<ReplicaMirror>,
    session: Option<SessionId>,
}

impl Client {
    fn receive(&mut self) -> Result<(u64, u64), Fault> {
        self.transport.poll()?;
        let (mut batches, mut payload) = (0, 0);
        while let Some(bytes) = self.transport.receive()? {
            payload += bytes.len() as u64;
            match wire::decode::<ServerMessage>(&bytes, base::budget(), base::codec_limits())? {
                ServerMessage::Seed(seed) if self.mirror.is_none() => {
                    self.mirror = Some(ReplicaMirror::from_seed(seed)?);
                }
                ServerMessage::Batch(batch) => {
                    self.mirror
                        .as_mut()
                        .ok_or_else(|| Fault::invalid_data("batch before seed"))?
                        .apply(batch)?;
                    batches += 1;
                }
                ServerMessage::Welcome { session, .. } => self.session = Some(session),
                other => {
                    return Err(Fault::invalid_data(format!(
                        "unexpected TCP message: {other:?}"
                    )));
                }
            }
        }
        Ok((batches, payload))
    }
}
