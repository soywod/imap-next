use std::io::{ErrorKind, Read, Write};

use bytes::{Buf, BufMut, BytesMut};
#[cfg(debug_assertions)]
use imap_codec::imap_types::utils::escape_byte_string;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf},
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpStream,
    },
    select,
};
use tokio_rustls::rustls;
use tokio_rustls::TlsStream;
#[cfg(debug_assertions)]
use tracing::trace;

use crate::{Interrupt, Io, State};

pub struct Stream {
    kind: StreamKind,
    read_buffer: BytesMut,
    write_buffer: BytesMut,
}

pub enum StreamKind {
    Tcp(OwnedReadHalf, OwnedWriteHalf),
    Tls(
        ReadHalf<TlsStream<TcpStream>>,
        WriteHalf<TlsStream<TcpStream>>,
    ),
}

impl Stream {
    pub fn insecure(stream: TcpStream) -> Self {
        let (r, w) = stream.into_split();

        Self {
            kind: StreamKind::Tcp(r, w),
            read_buffer: BytesMut::default(),
            write_buffer: BytesMut::default(),
        }
    }

    pub fn tls(stream: TlsStream<TcpStream>) -> Self {
        let (r, w) = tokio::io::split(stream);

        Self {
            kind: StreamKind::Tls(r, w),
            read_buffer: BytesMut::default(),
            write_buffer: BytesMut::default(),
        }
    }

    // pub async fn flush(&mut self) -> Result<(), Error<Infallible>> {
    //     // Flush TLS
    //     if let Some(tls) = &mut self.tls {
    //         tls.writer().flush()?;
    //         encrypt(tls, &mut self.write_buffer, Vec::new())?;
    //     }

    //     // Flush TCP
    //     write(&mut self.stream, &mut self.write_buffer).await?;
    //     self.stream.flush().await?;

    //     Ok(())
    // }

    pub async fn next<F: State>(&mut self, mut state: F) -> Result<F::Event, Error<F::Error>> {
        let event = loop {
            if !self.read_buffer.is_empty() {
                state.enqueue_input(&self.read_buffer);
                self.read_buffer.clear();
            }

            // Progress the client/server
            let result = state.next();

            // Return events immediately without doing IO
            let interrupt = match result {
                Err(interrupt) => interrupt,
                Ok(event) => break event,
            };

            // Return errors immediately without doing IO
            let io = match interrupt {
                Interrupt::Io(io) => io,
                Interrupt::Error(err) => return Err(Error::State(err)),
            };

            match io {
                Io::Output(bytes) if !bytes.is_empty() => {
                    self.write_buffer.extend(bytes);
                }
                _ => (),
            };

            // Progress the stream
            if self.write_buffer.is_empty() {
                match &mut self.kind {
                    StreamKind::Tcp(r, _) => {
                        read(r, &mut self.read_buffer).await?;
                    }
                    StreamKind::Tls(r, _) => {
                        read(r, &mut self.read_buffer).await?;
                    }
                }
            } else {
                match &mut self.kind {
                    StreamKind::Tcp(r, w) => {
                        select! {
                            result = read(r, &mut self.read_buffer) => result,
                            result = write(w, &mut self.write_buffer) => result,
                        }?;
                    }
                    StreamKind::Tls(r, w) => {
                        select! {
                            result = read(r, &mut self.read_buffer) => result,
                            result = write(w, &mut self.write_buffer) => result,
                        }?;
                    }
                }
            };
        };

        Ok(event)
    }

    #[cfg(feature = "expose_stream")]
    /// Return the underlying stream for debug purposes (or experiments).
    ///
    /// Note: Writing to or reading from the stream may introduce
    /// conflicts with `imap-next`.
    pub fn stream_mut(&mut self) -> &mut TcpStream {
        panic!()
    }
}

/// Take the [`TcpStream`] out of a [`Stream`].
///
/// Useful when a TCP stream needs to be upgraded to a TLS one.
#[cfg(feature = "expose_stream")]
impl From<Stream> for TcpStream {
    fn from(stream: Stream) -> Self {
        panic!()
    }
}

/// Error during reading into or writing from a stream.
#[derive(Debug, Error)]
pub enum Error<E> {
    /// Operation failed because stream is closed.
    ///
    /// We detect this by checking if the read or written byte count is 0. Whether the stream is
    /// closed indefinitely or temporarily depends on the actual stream implementation.
    #[error("Stream was closed")]
    Closed,
    /// An I/O error occurred in the underlying stream.
    #[error(transparent)]
    Io(#[from] tokio::io::Error),
    /// An error occurred in the underlying TLS connection.
    #[error(transparent)]
    Tls(#[from] rustls::Error),
    /// An error occurred while progressing the state.
    #[error(transparent)]
    State(E),
}

async fn read<S: AsyncRead + Unpin>(
    mut stream: S,
    read_buffer: &mut BytesMut,
) -> Result<(), ReadWriteError> {
    #[cfg(debug_assertions)]
    let old_len = read_buffer.len();
    let byte_count = stream.read_buf(read_buffer).await?;
    #[cfg(debug_assertions)]
    trace!(
        data = escape_byte_string(&read_buffer[old_len..]),
        "io/read/raw"
    );

    if byte_count == 0 {
        // The result is 0 if the stream reached "end of file" or the read buffer was
        // already full before calling `read_buf`. Because we use an unlimited buffer we
        // know that the first case occurred.
        return Err(ReadWriteError::Closed);
    }

    Ok(())
}

async fn write<S: AsyncWrite + Unpin>(
    mut stream: S,
    write_buffer: &mut BytesMut,
) -> Result<(), ReadWriteError> {
    while !write_buffer.is_empty() {
        let byte_count = stream.write(write_buffer).await?;
        #[cfg(debug_assertions)]
        trace!(
            data = escape_byte_string(&write_buffer[..byte_count]),
            "io/write/raw"
        );
        write_buffer.advance(byte_count);

        if byte_count == 0 {
            // The result is 0 if the stream doesn't accept bytes anymore or the write buffer
            // was already empty before calling `write_buf`. Because we checked the buffer
            // we know that the first case occurred.
            return Err(ReadWriteError::Closed);
        }
    }

    Ok(())
}

#[derive(Debug, Error)]
enum ReadWriteError {
    #[error("Stream was closed")]
    Closed,
    #[error(transparent)]
    Io(#[from] tokio::io::Error),
}

impl<E> From<ReadWriteError> for Error<E> {
    fn from(value: ReadWriteError) -> Self {
        match value {
            ReadWriteError::Closed => Error::Closed,
            ReadWriteError::Io(err) => Error::Io(err),
        }
    }
}

fn decrypt(
    tls: &mut rustls::Connection,
    read_buffer: &mut BytesMut,
) -> Result<Vec<u8>, DecryptEncryptError> {
    let mut plain_bytes = Vec::new();

    while tls.wants_read() && !read_buffer.is_empty() {
        let mut encrypted_bytes = read_buffer.reader();
        tls.read_tls(&mut encrypted_bytes)?;
        tls.process_new_packets()?;

        loop {
            let mut plain_bytes_chunk = [0; 128];
            // We need to handle different cases according to:
            // https://docs.rs/rustls/latest/rustls/struct.Reader.html#method.read
            match tls.reader().read(&mut plain_bytes_chunk) {
                // There are no more bytes to read
                Err(err) if err.kind() == ErrorKind::WouldBlock => break,
                // The TLS session was closed uncleanly
                Err(err) if err.kind() == ErrorKind::UnexpectedEof => {
                    return Err(DecryptEncryptError::Closed)
                }
                // We got an unexpected error
                Err(err) => return Err(DecryptEncryptError::Io(err)),
                // The TLS session was closed cleanly
                Ok(0) => return Err(DecryptEncryptError::Closed),
                // We read some plaintext bytes
                Ok(n) => plain_bytes.extend(&plain_bytes_chunk[0..n]),
            };
        }
    }

    Ok(plain_bytes)
}

fn encrypt(
    tls: &mut rustls::Connection,
    write_buffer: &mut BytesMut,
    plain_bytes: Vec<u8>,
) -> Result<(), DecryptEncryptError> {
    if !plain_bytes.is_empty() {
        tls.writer().write_all(&plain_bytes)?;
    }

    while tls.wants_write() {
        let mut encrypted_bytes = write_buffer.writer();
        tls.write_tls(&mut encrypted_bytes)?;
    }

    Ok(())
}

#[derive(Debug, Error)]
enum DecryptEncryptError {
    #[error("Session was closed")]
    Closed,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Tls(#[from] rustls::Error),
}

impl<E> From<DecryptEncryptError> for Error<E> {
    fn from(value: DecryptEncryptError) -> Self {
        match value {
            DecryptEncryptError::Closed => Error::Closed,
            DecryptEncryptError::Io(err) => Error::Io(err),
            DecryptEncryptError::Tls(err) => Error::Tls(err),
        }
    }
}
