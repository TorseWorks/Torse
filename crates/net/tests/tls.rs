#[path = "support/tls.rs"]
mod fixture;
use fixture::Fixture;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use torse_net::{TcpTransport, Transport, TransportLimits, tls::*};
use torse_types::{MemoryBudget, MemoryPool};

#[test]
fn queue_diagnostics_report_application_frames_before_tls_acceptance() {
    use torse_types::{Diagnostic, QueueByteKind, QueueDiagnostics};
    let fixture = Fixture::temporary();
    let (mut server, mut client) = pair(&fixture, "alice", "localhost");
    assert!(ready(&mut server, &mut client));
    let config = QueueDiagnostics::enabled(4096).unwrap();
    client.send(vec![7; 16]).unwrap();
    for _ in 0..1000 {
        let Diagnostic::Available(usage) = client.queue_diagnostics(&config) else {
            panic!("application queue unavailable")
        };
        assert_eq!(usage.outgoing.records, 1);
        assert_eq!(usage.outgoing.bytes, 20);
        assert_eq!(usage.outgoing.byte_kind, QueueByteKind::FramedRemaining);
    }
    let mut received = None;
    for _ in 0..10_000 {
        client.poll().unwrap();
        server.poll().unwrap();
        received = server.receive().unwrap();
        if received.is_some() {
            break;
        }
        std::thread::yield_now();
    }
    assert_eq!(received, Some(vec![7; 16]));
    let Diagnostic::Available(usage) = client.queue_diagnostics(&config) else {
        panic!("application queue unavailable")
    };
    assert_eq!(usage.outgoing.bytes, 0);
}

fn configs(
    fixture: &Fixture,
    user: &str,
) -> (Arc<rustls::ServerConfig>, Arc<rustls::ClientConfig>) {
    (
        server_config(
            &fixture.path("server.pem"),
            &fixture.path("server.key"),
            &fixture.path("ca.pem"),
        )
        .unwrap(),
        client_config(
            &fixture.path("ca.pem"),
            &fixture.path(&format!("{user}.pem")),
            &fixture.path(&format!("{user}.key")),
        )
        .unwrap(),
    )
}
fn pair(fixture: &Fixture, user: &str, name: &str) -> (TcpTransport, TcpTransport) {
    let (server, client) = configs(fixture, user);
    pair_configs(server, client, name)
}
fn pair_configs(
    server: Arc<rustls::ServerConfig>,
    client: Arc<rustls::ClientConfig>,
    name: &str,
) -> (TcpTransport, TcpTransport) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let client_socket = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server_socket, _) = listener.accept().unwrap();
    let limits = TransportLimits {
        frame: 262_144,
        queued_frames: 4,
        queued_bytes: 524_288,
        poll_bytes: 4096,
    };
    (
        TcpTransport::tls_server(server_socket, limits, server).unwrap(),
        TcpTransport::tls_client(client_socket, limits, client, name).unwrap(),
    )
}
fn ready(server: &mut TcpTransport, client: &mut TcpTransport) -> bool {
    for _ in 0..10_000 {
        if server.poll().and_then(|_| client.poll()).is_err() {
            return false;
        }
        if matches!(server.security(), TransportSecurity::MutualTls(_))
            && matches!(client.security(), TransportSecurity::MutualTls(_))
        {
            return true;
        }
        std::thread::yield_now();
    }
    panic!("TLS did not settle")
}

#[test]
fn mutual_tls_frames_fragment_and_release_shared_capacity() {
    let fixture = Fixture::temporary();
    let (mut server, mut client) = pair(&fixture, "alice", "localhost");
    let pool = MemoryPool::new(MemoryBudget::new(16 * 1024 * 1024));
    server.attach_memory(pool.clone()).unwrap();
    client.attach_memory(pool.clone()).unwrap();
    assert_eq!(server.security(), TransportSecurity::Authenticating);
    let payload = vec![7; 200_000];
    client.send(payload.clone()).unwrap();
    assert!(ready(&mut server, &mut client));
    let certificate = certificates(&fixture.path("alice.pem")).unwrap();
    assert_eq!(
        server.security(),
        TransportSecurity::MutualTls(certificate_fingerprint(certificate[0].as_ref()))
    );
    let mut received = None;
    for _ in 0..10_000 {
        client.poll().unwrap();
        server.poll().unwrap();
        received = server.receive().unwrap();
        if received.is_some() {
            break;
        }
        std::thread::yield_now();
    }
    assert_eq!(received.unwrap(), payload);
    server.close();
    client.close();
    assert_eq!(pool.usage().total, 0);
}

#[test]
fn mismatched_name_expired_credentials_and_plaintext_fail_closed() {
    let fixture = Fixture::temporary();
    for (user, name) in [("alice", "wrong.invalid"), ("expired", "localhost")] {
        let (mut server, mut client) = pair(&fixture, user, name);
        assert!(!ready(&mut server, &mut client));
        assert!(!matches!(
            server.security(),
            TransportSecurity::MutualTls(_)
        ));
    }
    let (server_configured, client) = configs(&fixture, "alice");
    let expired_server = server_config(
        &fixture.path("expired-server.pem"),
        &fixture.path("expired-server.key"),
        &fixture.path("ca.pem"),
    )
    .unwrap();
    let (mut server, mut peer) = pair_configs(expired_server, client, "localhost");
    assert!(!ready(&mut server, &mut peer));
    let other = Fixture::temporary();
    let unknown = client_config(
        &fixture.path("ca.pem"),
        &other.path("alice.pem"),
        &other.path("alice.key"),
    )
    .unwrap();
    let (mut server, mut peer) = pair_configs(server_configured.clone(), unknown, "localhost");
    assert!(!ready(&mut server, &mut peer));
    let mut roots = rustls::RootCertStore::empty();
    for cert in certificates(&fixture.path("ca.pem")).unwrap() {
        roots.add(cert).unwrap();
    }
    let mut anonymous = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    anonymous.alpn_protocols = vec![b"torse/3".to_vec()];
    let (mut server, mut peer) = pair_configs(server_configured, Arc::new(anonymous), "localhost");
    assert!(!ready(&mut server, &mut peer));
    assert!(
        server_config(
            &fixture.path("server.pem"),
            &fixture.path("alice.key"),
            &fixture.path("ca.pem")
        )
        .is_err()
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut plain = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (socket, _) = listener.accept().unwrap();
    let (config, _) = configs(&fixture, "alice");
    let limits = TransportLimits {
        frame: 1024,
        queued_frames: 2,
        queued_bytes: 4096,
        poll_bytes: 4096,
    };
    let mut server = TcpTransport::tls_server(socket, limits, config).unwrap();
    plain
        .write_all(b"plaintext is not a TLS handshake")
        .unwrap();
    assert!(server.poll().is_err());
    assert!(server.receive().is_err());
}

#[test]
fn encrypted_record_replay_is_rejected_and_no_early_data_is_enabled() {
    let fixture = Fixture::temporary();
    let (server, client) = configs(&fixture, "alice");
    assert_eq!(server.max_early_data_size, 0);
    assert_eq!(server.send_tls13_tickets, 0);
    assert!(!client.enable_early_data);
    let mut server = rustls::ServerConnection::new(server).unwrap();
    let mut client =
        rustls::ClientConnection::new(client, "localhost".try_into().unwrap()).unwrap();
    for _ in 0..100 {
        let mut packet = Vec::new();
        client.write_tls(&mut packet).unwrap();
        if !packet.is_empty() {
            server.read_tls(&mut packet.as_slice()).unwrap();
            server.process_new_packets().unwrap();
        }
        packet.clear();
        server.write_tls(&mut packet).unwrap();
        if !packet.is_empty() {
            client.read_tls(&mut packet.as_slice()).unwrap();
            client.process_new_packets().unwrap();
        }
        if !client.is_handshaking() && !server.is_handshaking() {
            break;
        }
    }
    assert!(!server.is_handshaking());
    client.writer().write_all(b"authenticated message").unwrap();
    let mut encrypted = Vec::new();
    client.write_tls(&mut encrypted).unwrap();
    server.read_tls(&mut encrypted.as_slice()).unwrap();
    server.process_new_packets().unwrap();
    let mut decoded = Vec::new();
    let _ = server.reader().read_to_end(&mut decoded);
    assert_eq!(decoded, b"authenticated message");
    server.read_tls(&mut encrypted.as_slice()).unwrap();
    assert!(server.process_new_packets().is_err());
}
