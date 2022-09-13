use blake2::{Blake2s256, Digest};
use lazy_static::lazy_static;
use rustls::internal::msgs::deframer::MessageDeframer;
use rustls::internal::msgs::enums::AlertLevel;
use rustls::internal::msgs::message::Message;
use snow::params::NoiseParams;
use snow::TransportState;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

use futures::ready;
use std::cmp;
use std::fmt;
use std::io::{self};
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::utils::SyncReadAdapter;

lazy_static! {
    pub static ref NOISE_PARAMS: NoiseParams =
        "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s".parse().unwrap();
}

pub const TLS_RECORD_HEADER_LENGTH: usize = 5; // 1 type + 2 proto ver + 2 data len
pub const MAXIMUM_CIPHERTEXT_LENGTH: usize = 2usize.pow(14); // 16KiB < show::constants::MAXMSGLEN
pub const AEAD_TAG_LENGTH: usize = 16; // show::constants::TAGLEN
pub const MAXIMUM_PLAINTEXT_LENGTH: usize = MAXIMUM_CIPHERTEXT_LENGTH - AEAD_TAG_LENGTH;
pub const PSKLEN: usize = 32; // snow::constants::PSKLEN;
const CONTEXT: &[u8] = b"the secure tunnel under snow";

// #[derive(Debug)]
pub struct SnowyStream {
    pub(crate) socket: TcpStream,
    pub(crate) noise: TransportState,
    pub(crate) state: SnowyState,
    pub(crate) tls_deframer: MessageDeframer,
    pub(crate) read_buffer: Vec<u8>,
    pub(crate) read_offset: usize,
    pub(crate) write_buffer: Vec<u8>,
    pub(crate) write_offset: usize,
}

impl SnowyStream {
    pub fn new(io: TcpStream, noise: TransportState) -> Self {
        SnowyStream {
            socket: io,
            noise,
            state: SnowyState::Stream,
            tls_deframer: Default::default(),
            read_buffer: Default::default(),
            read_offset: 0,
            write_buffer: Default::default(),
            write_offset: 0,
        }
    }

    pub fn as_inner(&self) -> &TcpStream {
        &self.socket
    }

    pub fn as_inner_mut(&mut self) -> &mut TcpStream {
        &mut self.socket
    }
}

impl fmt::Debug for SnowyStream {
    fn fmt(&self, _fmt: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        unimplemented!();
    }
}

impl AsyncRead for SnowyStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if !self.state.readable() {
            return Poll::Ready(Ok(()));
        }
        // Ref: https://github.com/tokio-rs/tls/blob/bcf4f8e3f96983dbb7a61808b0f1fcd04fb678ae/tokio-rustls/src/common/mod.rs#L91
        let this = self.get_mut();
        let mut has_read = false;
        'read_more: loop {
            'read_ready: loop {
                // first, clean pending read_buffer
                if this.read_offset < this.read_buffer.len() {
                    let len = cmp::min(this.read_buffer.len() - this.read_offset, buf.remaining());
                    buf.put_slice(&this.read_buffer[this.read_offset..this.read_offset + len]);
                    this.read_offset += len;
                    has_read |= len > 0;
                    if this.read_offset < this.read_buffer.len() {
                        // buf is full
                        break 'read_more;
                    }
                }
                this.read_offset = 0;
                this.read_buffer.clear();

                // then, try to pop a ready TLS frame
                if let Some(message) = this.tls_deframer.frames.pop_front() {
                    if message.payload.0.is_empty() {
                        continue;
                    }
                    this.read_buffer
                        .reserve_exact(MAXIMUM_PLAINTEXT_LENGTH - this.read_buffer.capacity());
                    unsafe { this.read_buffer.set_len(MAXIMUM_PLAINTEXT_LENGTH) };
                    let len = this
                        .noise
                        .read_message(&message.payload.0, &mut this.read_buffer)
                        .expect("TODO");
                    this.read_buffer.truncate(len);
                } else {
                    // no ready tls frame, proceed to read the inner socket
                    break 'read_ready;
                }
            }
            // the best practice is to be conservative on making syscalls
            // so here prefer to return progress if any over proceeding to read the inner socket
            if has_read {
                break 'read_more;
            }

            // otherwse, read the underlying socket
            match this.tls_deframer.read(&mut SyncReadAdapter {
                io: &mut this.socket,
                cx,
            }) {
                Ok(n) => {
                    if n == 0 {
                        this.state.shutdown_read(); // TODO: ?
                        return Poll::Ready(Ok(()));
                    }
                }
                Err(ref err) if err.kind() == io::ErrorKind::WouldBlock => {
                    return Poll::Pending;
                }
                Err(err) if err.kind() == io::ErrorKind::ConnectionAborted => {
                    // other error?
                    this.state.shutdown_read();
                    return dbg!(Poll::Ready(Err(err)));
                }
                Err(err) => {
                    return dbg!(Poll::Ready(Err(err)));
                }
            }
        }
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for SnowyStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if !self.state.writeable() {
            return Poll::Ready(Ok(0));
        }
        let mut this = self.get_mut();
        let mut offset = 0;
        loop {
            // first, clean pending write_buffer (an encoded TLS frame)
            while this.write_offset != this.write_buffer.len() {
                match Pin::new(&mut this.socket)
                    .poll_write(cx, &this.write_buffer[this.write_offset..])
                {
                    Poll::Ready(r) => {
                        let n = r?;
                        this.write_offset += n;
                        if n == 0 {
                            // TODO: clean write buffer?
                            // this.state.shutdown_write();
                            return Poll::Ready(Ok(0));
                        }
                    }
                    Poll::Pending => {
                        return if offset == 0 {
                            Poll::Pending
                        } else {
                            Poll::Ready(Ok(offset))
                        };
                    }
                }
            }
            this.write_offset = 0;
            this.write_buffer.clear();
            if offset == buf.len() {
                return Poll::Ready(Ok(offset));
            }
            // then, encode buf as TLS frame in write_buffer ready to be written to socket
            // TODO: should we store more than one TLS frame in write_buffer?
            this.write_buffer
                .reserve_exact(TLS_RECORD_HEADER_LENGTH + MAXIMUM_CIPHERTEXT_LENGTH);
            unsafe {
                this.write_buffer
                    .set_len(TLS_RECORD_HEADER_LENGTH + MAXIMUM_CIPHERTEXT_LENGTH);
            }
            this.write_buffer[0..3].copy_from_slice(&[0x17, 0x03, 0x03]); // 3,3 is for TLS 1.2/1.3
            let len = cmp::min(buf.len() - offset, MAXIMUM_PLAINTEXT_LENGTH);
            let n = this
                .noise
                .write_message(
                    &buf[offset..offset + len],
                    &mut this.write_buffer[TLS_RECORD_HEADER_LENGTH..],
                )
                .unwrap();
            // Note: plaintext.len < ciphertext.n and typically len + AEAD_TAG_LENGTH = n
            debug_assert_eq!(len + AEAD_TAG_LENGTH, n);
            offset += len;
            debug_assert!(offset <= buf.len());
            this.write_buffer[3..5].copy_from_slice(&(n as u16).to_be_bytes());
            this.write_buffer.truncate(TLS_RECORD_HEADER_LENGTH + n);
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut this = self.get_mut();
        while this.write_offset != this.write_buffer.len() {
            // should we try to poll_write the underlying more than once at all?
            match Pin::new(&mut this.socket).poll_write(cx, &this.write_buffer[this.write_offset..])
            {
                Poll::Ready(r) => {
                    this.write_offset += r?;
                }
                Poll::Pending => {
                    return Poll::Pending;
                }
            }
        }
        this.write_offset = 0;
        this.write_buffer.clear();
        Pin::new(&mut this.socket).poll_flush(cx) // actually, tcp flush is a no-op
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // self.state.shutdown_write();
        // proceed even if state has already been not writable
        // otherwise latter steps would be ignored in second poll calls
        if self.state.writeable() {
            self.state.shutdown_write();
        }
        // TODO: https://www.openssl.org/docs/man1.0.2/man3/SSL_shutdown.html
        // https://github.com/tokio-rs/tls/blob/56855b71661a9bf848c1a3c3f03ead6ac3f1b49f/tokio-rustls/src/client.rs#L235
        // self.send_warning_alert_no_log(AlertDescription::CloseNotify);
        // let alert = Message::build_alert(AlertLevel::Warning, rustls::AlertDescription::CloseNotify);

        // per trait's doc, flush should be polled till ready before shutdown returns ready
        ready!(self.as_mut().poll_flush(cx))?;
        Pin::new(&mut self.socket).poll_shutdown(cx)
    }
}

#[derive(Debug)]
pub enum SnowyState {
    Stream,
    ReadShutdown,
    WriteShutdown,
    FullyShutdown,
}

impl SnowyState {
    #[inline]
    pub fn shutdown_read(&mut self) {
        match *self {
            SnowyState::WriteShutdown | SnowyState::FullyShutdown => {
                *self = SnowyState::FullyShutdown
            }
            _ => *self = SnowyState::ReadShutdown,
        }
    }

    #[inline]
    pub fn shutdown_write(&mut self) {
        match *self {
            SnowyState::ReadShutdown | SnowyState::FullyShutdown => {
                *self = SnowyState::FullyShutdown
            }
            _ => *self = SnowyState::WriteShutdown,
        }
    }

    #[inline]
    pub fn writeable(&self) -> bool {
        !matches!(*self, SnowyState::WriteShutdown | SnowyState::FullyShutdown)
    }

    #[inline]
    pub fn readable(&self) -> bool {
        !matches!(*self, SnowyState::ReadShutdown | SnowyState::FullyShutdown)
    }
}

pub fn derive_psk(key: &[u8]) -> [u8; PSKLEN] {
    // Blake3 defines a key derive function, but blake2 does not.
    let mut h = Blake2s256::new();
    h.update(CONTEXT);
    h.update(key);
    h.finalize().into()
}