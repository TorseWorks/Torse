//! Nonblocking framed transports. Socket polling stays outside world publication.
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::sync::{Arc, Mutex};
use torse_types::{
    Diagnostic, FrameQueue, QueueByteKind, QueueDiagnostics, TransportQueues, Unavailable,
};
use torse_types::{EvictionRank, Fault, FaultCode, MemoryClass, MemoryLease, MemoryPool};
#[derive(Clone, Copy, Debug)]
pub struct TransportLimits {
    pub frame: usize,
    pub queued_frames: usize,
    pub queued_bytes: usize,
    pub poll_bytes: usize,
}
impl TransportLimits {
    fn reserve(self, pool: &MemoryPool) -> Result<MemoryLease, Fault> {
        let bytes = (self.queued_bytes as u64)
            .checked_mul(6)
            .and_then(|bytes| bytes.checked_add((self.queued_frames as u64).checked_mul(128)?))
            .ok_or_else(|| Fault::resource_limit("transport memory reservation overflow"))?;
        pool.reserve(MemoryClass::Replica, bytes, EvictionRank::Pinned)
    }
    fn validate(self) -> Result<(), Fault> {
        if self.frame == 0
            || self.frame > u32::MAX as usize
            || self.queued_frames == 0
            || self.queued_bytes < self.frame.saturating_add(4)
            || self.poll_bytes == 0
        {
            return Err(Fault::invalid_data("transport limits"));
        }
        Ok(())
    }
}
pub trait Transport {
    /// No retained application/TLS output remains. Bytes already accepted by
    /// the socket or peer are outside this cut. Unknown adapters return false.
    fn output_idle(&self) -> bool {
        false
    }
    /// Observe existing buffers only. Implementations must not poll, encode or perform I/O.
    /// Custom adapters remain source-compatible and explicitly unsupported by default.
    /// Read existing queues without I/O, allocation, consumption or blocking waits.
    fn queue_diagnostics(&self, config: &QueueDiagnostics) -> Diagnostic<TransportQueues> {
        if let Err(reason) = config.check::<TransportQueues>() {
            return Diagnostic::Unavailable(reason);
        }
        Diagnostic::Unavailable(Unavailable::NotSupported)
    }
    fn security(&self) -> crate::tls::TransportSecurity {
        crate::tls::TransportSecurity::Unprotected
    }
    fn attach_memory(&mut self, _pool: MemoryPool) -> Result<(), Fault> {
        Err(Fault::incompatible(
            "transport does not support shared memory reservations",
        ))
    }
    fn send(&mut self, frame: Vec<u8>) -> Result<(), Fault>;
    fn receive(&mut self) -> Result<Option<Vec<u8>>, Fault>;
    fn poll(&mut self) -> Result<(), Fault>;
    /// Control path: drops unsent data and closes, independent of queue capacity.
    fn close(&mut self);
}
struct Queue {
    frames: VecDeque<Vec<u8>>,
    bytes: usize,
    closed: bool,
}
pub struct MemoryTransport {
    input: Arc<Mutex<Queue>>,
    output: Arc<Mutex<Queue>>,
    limits: TransportLimits,
    memory: Option<MemoryLease>,
}
fn closed() -> Fault {
    Fault::new(FaultCode::InvalidState, "transport closed")
}
impl MemoryTransport {
    pub fn pair(limits: TransportLimits) -> Result<(Self, Self), Fault> {
        limits.validate()?;
        let queue = || {
            Arc::new(Mutex::new(Queue {
                frames: VecDeque::new(),
                bytes: 0,
                closed: false,
            }))
        };
        let a = queue();
        let b = queue();
        Ok((
            Self {
                input: a.clone(),
                output: b.clone(),
                limits,
                memory: None,
            },
            Self {
                input: b,
                output: a,
                limits,
                memory: None,
            },
        ))
    }
}
impl Transport for MemoryTransport {
    fn output_idle(&self) -> bool {
        self.output
            .try_lock()
            .is_ok_and(|q| !q.closed && q.frames.is_empty())
    }
    fn queue_diagnostics(&self, config: &QueueDiagnostics) -> Diagnostic<TransportQueues> {
        if let Err(reason) = config.check::<TransportQueues>() {
            return Diagnostic::Unavailable(reason);
        }
        // Never wait while observing a shared queue; locks are taken separately.
        let (input, input_closed) = match self.input.try_lock() {
            Ok(q) => (
                FrameQueue {
                    records: q.frames.len() as u64,
                    bytes: q.bytes as u64,
                    byte_kind: QueueByteKind::Payload,
                },
                q.closed,
            ),
            Err(_) => return Diagnostic::Unavailable(Unavailable::Busy),
        };
        let (output, output_closed) = match self.output.try_lock() {
            Ok(q) => (
                FrameQueue {
                    records: q.frames.len() as u64,
                    bytes: q.bytes as u64,
                    byte_kind: QueueByteKind::Payload,
                },
                q.closed,
            ),
            Err(_) => return Diagnostic::Unavailable(Unavailable::Busy),
        };
        Diagnostic::Available(TransportQueues {
            incoming: input,
            incomplete_frame_bytes: 0,
            outgoing: output,
            closed: input_closed || output_closed,
        })
    }
    fn attach_memory(&mut self, pool: MemoryPool) -> Result<(), Fault> {
        if self.memory.is_some() {
            return Err(Fault::invalid_data("transport memory already attached"));
        }
        self.memory = Some(self.limits.reserve(&pool)?);
        Ok(())
    }
    fn send(&mut self, frame: Vec<u8>) -> Result<(), Fault> {
        let mut out = self.output.lock().map_err(|_| closed())?;
        if out.closed {
            return Err(closed());
        }
        if frame.is_empty()
            || frame.len() > self.limits.frame
            || out.frames.len() >= self.limits.queued_frames
            || frame.len() > self.limits.queued_bytes.saturating_sub(out.bytes)
        {
            return Err(Fault::resource_limit("transport output capacity"));
        }
        out.bytes += frame.len();
        out.frames.push_back(frame);
        Ok(())
    }
    fn receive(&mut self) -> Result<Option<Vec<u8>>, Fault> {
        let mut input = self.input.lock().map_err(|_| closed())?;
        if input.closed {
            return Err(closed());
        }
        let value = input.frames.pop_front();
        if let Some(v) = &value {
            input.bytes -= v.len();
        }
        Ok(value)
    }
    fn poll(&mut self) -> Result<(), Fault> {
        if self.input.lock().map_err(|_| closed())?.closed {
            Err(closed())
        } else {
            Ok(())
        }
    }
    fn close(&mut self) {
        for queue in [&self.input, &self.output] {
            if let Ok(mut q) = queue.lock() {
                q.closed = true;
                q.frames = VecDeque::new();
                q.bytes = 0;
            }
        }
        self.memory = None;
    }
}
impl Drop for MemoryTransport {
    fn drop(&mut self) {
        self.close();
    }
}
pub struct TcpTransport {
    stream: TcpStream,
    tls: Option<crate::tls::Tls>,
    limits: TransportLimits,
    input: Vec<u8>,
    ready: VecDeque<Vec<u8>>,
    output: VecDeque<Vec<u8>>,
    offset: usize,
    queued: usize,
    ready_bytes: usize,
    closed: bool,
    memory: Option<MemoryLease>,
    tls_memory: Option<MemoryLease>,
}
impl TcpTransport {
    pub fn new(stream: TcpStream, limits: TransportLimits) -> Result<Self, Fault> {
        limits.validate()?;
        stream.set_nonblocking(true).map_err(io_fault)?;
        stream.set_nodelay(true).map_err(io_fault)?;
        Ok(Self {
            stream,
            tls: None,
            limits,
            input: Vec::new(),
            ready: VecDeque::new(),
            output: VecDeque::new(),
            offset: 0,
            queued: 0,
            ready_bytes: 0,
            closed: false,
            memory: None,
            tls_memory: None,
        })
    }
    pub fn tls_server(
        stream: TcpStream,
        limits: TransportLimits,
        config: Arc<rustls::ServerConfig>,
    ) -> Result<Self, Fault> {
        let mut transport = Self::new(stream, limits)?;
        transport.tls = Some(crate::tls::Tls::server(config)?);
        Ok(transport)
    }
    pub fn tls_client(
        stream: TcpStream,
        limits: TransportLimits,
        config: Arc<rustls::ClientConfig>,
        name: &str,
    ) -> Result<Self, Fault> {
        let mut transport = Self::new(stream, limits)?;
        transport.tls = Some(crate::tls::Tls::client(config, name)?);
        Ok(transport)
    }
    fn pump(&mut self) -> Result<(), Fault> {
        if let Some(tls) = &mut self.tls {
            tls.pump(&mut self.stream, self.limits.poll_bytes.div_ceil(2))?;
        }
        let mut remaining = self.limits.poll_bytes;
        let mut buffer = [0u8; 8192];
        let mut operations = 64;
        while remaining > 0 && operations > 0 {
            operations -= 1;
            let n = buffer.len().min(remaining);
            let read = match &mut self.tls {
                Some(tls) => tls.connection.reader().read(&mut buffer[..n]),
                None => self.stream.read(&mut buffer[..n]),
            };
            match read {
                Ok(0) => return Err(closed()),
                Ok(n) => {
                    if n > self
                        .limits
                        .queued_bytes
                        .saturating_sub(self.input.len() + self.ready_bytes)
                    {
                        return Err(Fault::resource_limit("TCP incoming capacity"));
                    }
                    self.input.extend_from_slice(&buffer[..n]);
                    remaining -= n;
                    loop {
                        if self.input.len() < 4 {
                            break;
                        }
                        let len =
                            u32::from_le_bytes(self.input[..4].try_into().expect("four bytes"))
                                as usize;
                        if len == 0 || len > self.limits.frame {
                            return Err(Fault::invalid_data("TCP frame length"));
                        }
                        if self.input.len() < len + 4 {
                            break;
                        }
                        if self.ready.len() >= self.limits.queued_frames {
                            return Err(Fault::resource_limit("TCP incoming frame count"));
                        }
                        self.ready.push_back(self.input[4..4 + len].to_vec());
                        self.ready_bytes += len;
                        self.input.drain(..4 + len);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(io_fault(e)),
            }
        }
        let mut remaining = self.limits.poll_bytes;
        let mut operations = 64;
        while remaining > 0 && operations > 0 {
            operations -= 1;
            let Some(front) = self.output.front() else {
                break;
            };
            let end = front.len().min(self.offset + remaining);
            let written = match &mut self.tls {
                Some(tls) => tls.connection.writer().write(&front[self.offset..end]),
                None => self.stream.write(&front[self.offset..end]),
            };
            match written {
                Ok(0) if self.tls.is_some() => break,
                Ok(0) => return Err(closed()),
                Ok(n) => {
                    remaining -= n;
                    self.offset += n;
                    self.queued -= n;
                    if self.offset == front.len() {
                        self.output.pop_front();
                        self.offset = 0;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(io_fault(e)),
            }
        }
        if let Some(tls) = &mut self.tls {
            tls.pump(&mut self.stream, self.limits.poll_bytes.div_ceil(2))?;
        }
        Ok(())
    }
}
fn io_fault(e: std::io::Error) -> Fault {
    Fault::new(FaultCode::InvalidState, format!("transport I/O: {e}"))
}
impl Transport for TcpTransport {
    fn output_idle(&self) -> bool {
        !self.closed
            && self.output.is_empty()
            && self.offset == 0
            && self
                .tls
                .as_ref()
                .is_none_or(|tls| !tls.connection.wants_write())
    }
    fn queue_diagnostics(&self, config: &QueueDiagnostics) -> Diagnostic<TransportQueues> {
        if let Err(reason) = config.check::<TransportQueues>() {
            return Diagnostic::Unavailable(reason);
        }
        Diagnostic::Available(TransportQueues {
            incoming: FrameQueue {
                records: self.ready.len() as u64,
                bytes: self.ready_bytes as u64,
                byte_kind: QueueByteKind::Payload,
            },
            incomplete_frame_bytes: self.input.len() as u64,
            outgoing: FrameQueue {
                records: self.output.len() as u64,
                bytes: self.queued as u64,
                byte_kind: QueueByteKind::FramedRemaining,
            },
            closed: self.closed,
        })
    }
    fn security(&self) -> crate::tls::TransportSecurity {
        self.tls.as_ref().map_or(
            crate::tls::TransportSecurity::Unprotected,
            crate::tls::Tls::security,
        )
    }
    fn attach_memory(&mut self, pool: MemoryPool) -> Result<(), Fault> {
        if self.memory.is_some() {
            return Err(Fault::invalid_data("transport memory already attached"));
        }
        let memory = self.limits.reserve(&pool)?;
        self.tls_memory = self
            .tls
            .as_ref()
            .map(|_| {
                pool.reserve(
                    MemoryClass::Replica,
                    crate::tls::RESERVATION_BYTES,
                    EvictionRank::Pinned,
                )
            })
            .transpose()?;
        self.memory = Some(memory);
        Ok(())
    }
    fn send(&mut self, frame: Vec<u8>) -> Result<(), Fault> {
        if self.closed {
            return Err(closed());
        }
        if frame.is_empty()
            || frame.len() > self.limits.frame
            || self.output.len() >= self.limits.queued_frames
            || frame.len() + 4 > self.limits.queued_bytes.saturating_sub(self.queued)
        {
            return Err(Fault::resource_limit("TCP output capacity"));
        }
        let mut packet = Vec::with_capacity(frame.len() + 4);
        packet.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        packet.extend(frame);
        self.queued += packet.len();
        self.output.push_back(packet);
        Ok(())
    }
    fn receive(&mut self) -> Result<Option<Vec<u8>>, Fault> {
        if self.closed {
            return Err(closed());
        }
        let frame = self.ready.pop_front();
        if let Some(v) = &frame {
            self.ready_bytes -= v.len();
        }
        Ok(frame)
    }
    fn poll(&mut self) -> Result<(), Fault> {
        if self.closed {
            return Err(closed());
        }
        let result = self.pump();
        if result.is_err() {
            self.close();
        }
        result
    }
    fn close(&mut self) {
        self.closed = true;
        self.input = Vec::new();
        self.ready = VecDeque::new();
        self.output = VecDeque::new();
        self.queued = 0;
        self.ready_bytes = 0;
        let _ = self.stream.shutdown(Shutdown::Both);
        self.tls = None;
        self.tls_memory = None;
        self.memory = None;
    }
}
impl Drop for TcpTransport {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod memory_tests {
    use super::*;
    use torse_types::MemoryBudget;
    #[test]
    fn diagnostics_do_not_wait_for_locked_shared_queues() {
        let limits = TransportLimits {
            frame: 64,
            queued_frames: 4,
            queued_bytes: 256,
            poll_bytes: 64,
        };
        let (transport, _) = MemoryTransport::pair(limits).unwrap();
        let _guard = transport.input.lock().unwrap();
        assert_eq!(
            transport.queue_diagnostics(&QueueDiagnostics::default()),
            Diagnostic::Unavailable(Unavailable::NotEnabled)
        );
        assert_eq!(
            transport.queue_diagnostics(&QueueDiagnostics::enabled(4096).unwrap()),
            Diagnostic::Unavailable(Unavailable::Busy)
        );
    }
    #[test]
    fn transport_reservations_cover_queue_lifetimes_and_close() {
        let limits = TransportLimits {
            frame: 64,
            queued_frames: 4,
            queued_bytes: 256,
            poll_bytes: 64,
        };
        let pool = MemoryPool::new(MemoryBudget::new(4096));
        let (mut a, mut b) = MemoryTransport::pair(limits).unwrap();
        a.attach_memory(pool.clone()).unwrap();
        b.attach_memory(pool.clone()).unwrap();
        assert_eq!(pool.usage().total, 4096);
        a.send(vec![1; 64]).unwrap();
        assert_eq!(b.receive().unwrap().unwrap(), [1; 64]);
        a.close();
        assert_eq!(pool.usage().total, 2048);
        assert_eq!(a.input.lock().unwrap().frames.capacity(), 0);
        drop(b);
        assert_eq!(pool.usage().total, 0);
    }
}
