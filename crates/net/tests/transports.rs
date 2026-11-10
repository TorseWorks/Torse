use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;
use torse_net::*;
use torse_types::*;
fn limits() -> TransportLimits {
    TransportLimits {
        frame: 256,
        queued_frames: 2,
        queued_bytes: 1024,
        poll_bytes: 3,
    }
}

fn observed<T>(value: Diagnostic<T>) -> T {
    match value {
        Diagnostic::Available(value) => value,
        Diagnostic::Unavailable(reason) => panic!("{reason:?}"),
    }
}

#[test]
fn queue_diagnostics_observe_memory_backpressure_without_consuming_frames() {
    let config = QueueDiagnostics::enabled(4096).unwrap();
    let (mut a, mut b) = MemoryTransport::pair(limits()).unwrap();
    assert!(a.output_idle());
    assert_eq!(
        a.queue_diagnostics(&QueueDiagnostics::default()),
        Diagnostic::Unavailable(Unavailable::NotEnabled)
    );
    a.send(vec![1; 128]).unwrap();
    assert!(!a.output_idle());
    a.send(vec![2; 128]).unwrap();
    assert!(a.send(vec![3]).is_err());
    for _ in 0..1000 {
        let usage = observed(a.queue_diagnostics(&config));
        assert_eq!(
            (
                usage.outgoing.records,
                usage.outgoing.bytes,
                usage.outgoing.byte_kind
            ),
            (2, 256, QueueByteKind::Payload)
        );
        assert_eq!(observed(b.queue_diagnostics(&config)).incoming.records, 2);
    }
    assert_eq!(b.receive().unwrap().unwrap(), vec![1; 128]);
    assert_eq!(observed(a.queue_diagnostics(&config)).outgoing.bytes, 128);
    a.close();
    assert!(!a.output_idle());
    let closed = observed(b.queue_diagnostics(&config));
    assert!(closed.closed);
    assert_eq!((closed.incoming.records, closed.incoming.bytes), (0, 0));
    let (fresh, _fresh_peer) = MemoryTransport::pair(limits()).unwrap();
    assert!(!observed(fresh.queue_diagnostics(&config)).closed);
}

#[test]
fn tcp_queue_diagnostics_keep_partial_framing_and_remaining_writes_distinct() {
    let config = QueueDiagnostics::enabled(4096).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut raw = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (socket, _) = listener.accept().unwrap();
    let mut tcp = TcpTransport::new(socket, limits()).unwrap();
    assert!(tcp.output_idle());
    tcp.send(vec![7; 10]).unwrap();
    assert!(!tcp.output_idle());
    let before = observed(tcp.queue_diagnostics(&config));
    assert_eq!(
        (
            before.outgoing.records,
            before.outgoing.bytes,
            before.outgoing.byte_kind
        ),
        (1, 14, QueueByteKind::FramedRemaining)
    );
    tcp.poll().unwrap();
    raw.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    let mut written = [0; 3];
    raw.read_exact(&mut written).unwrap();
    assert_eq!(written, 10u32.to_le_bytes()[..3]);
    assert_eq!(observed(tcp.queue_diagnostics(&config)).outgoing.bytes, 11);
    assert!(!tcp.output_idle());
    raw.write_all(&4u32.to_le_bytes()[..2]).unwrap();
    for _ in 0..100 {
        tcp.poll().unwrap();
        if observed(tcp.queue_diagnostics(&config)).incomplete_frame_bytes == 2 {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let partial = observed(tcp.queue_diagnostics(&config));
    assert_eq!(partial.incomplete_frame_bytes, 2);
    assert_eq!(partial.incoming.records, 0);
    assert!(partial.outgoing.bytes < 14);
    raw.write_all(&4u32.to_le_bytes()[2..]).unwrap();
    raw.write_all(&[1, 2, 3, 4]).unwrap();
    for _ in 0..100 {
        tcp.poll().unwrap();
        if observed(tcp.queue_diagnostics(&config)).incoming.records == 1 {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let complete = observed(tcp.queue_diagnostics(&config));
    assert_eq!(
        (
            complete.incoming.records,
            complete.incoming.bytes,
            complete.incomplete_frame_bytes
        ),
        (1, 4, 0)
    );
    assert_eq!(tcp.receive().unwrap().unwrap(), [1, 2, 3, 4]);
    tcp.close();
    let closed = observed(tcp.queue_diagnostics(&config));
    assert!(closed.closed);
    assert_eq!((closed.outgoing.records, closed.outgoing.bytes), (0, 0));
}

#[test]
fn custom_transport_explicitly_reports_unavailable_diagnostics() {
    struct Custom;
    impl Transport for Custom {
        fn send(&mut self, _: Vec<u8>) -> Result<(), Fault> {
            Ok(())
        }
        fn receive(&mut self) -> Result<Option<Vec<u8>>, Fault> {
            Ok(None)
        }
        fn poll(&mut self) -> Result<(), Fault> {
            Ok(())
        }
        fn close(&mut self) {}
    }
    assert_eq!(
        Custom.queue_diagnostics(&QueueDiagnostics::enabled(4096).unwrap()),
        Diagnostic::Unavailable(Unavailable::NotSupported)
    );
}
#[test]
fn memory_backpressure_and_control_close_drop_all_unsent_frames() {
    let (mut a, mut b) = MemoryTransport::pair(limits()).unwrap();
    a.send(vec![1; 128]).unwrap();
    a.send(vec![2; 128]).unwrap();
    assert_eq!(a.send(vec![3]).unwrap_err().code, FaultCode::ResourceLimit);
    assert_eq!(b.receive().unwrap().unwrap(), vec![1; 128]);
    a.send(vec![3]).unwrap();
    a.close();
    assert!(b.receive().is_err());
    assert!(b.send(vec![4]).is_err());
}
#[test]
fn tcp_partial_prefixes_payloads_and_writes_reassemble_exactly() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (peer, _) = listener.accept().unwrap();
    let mut a = TcpTransport::new(stream, limits()).unwrap();
    let mut b = TcpTransport::new(peer, limits()).unwrap();
    let expected: Vec<_> = (0..200).collect();
    a.send(expected.clone()).unwrap();
    let mut received = None;
    for _ in 0..300 {
        a.poll().unwrap();
        b.poll().unwrap();
        if let Some(frame) = b.receive().unwrap() {
            received = Some(frame);
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(received, Some(expected));
    assert!(b.receive().unwrap().is_none());
}
#[test]
fn tcp_oversized_prefix_is_rejected_before_payload_allocation() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut raw = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (peer, _) = listener.accept().unwrap();
    let mut transport = TcpTransport::new(peer, limits()).unwrap();
    raw.write_all(&u32::MAX.to_le_bytes()).unwrap();
    let mut rejected = false;
    for _ in 0..50 {
        if transport.poll().is_err() {
            rejected = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(rejected);
    assert!(transport.receive().is_err());
}
