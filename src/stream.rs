use std::{
    convert::Infallible,
    io::{ErrorKind, Read, Write},
};

use bytes::{Buf, BufMut, BytesMut};
#[cfg(debug_assertions)]
use imap_types::utils::escape_byte_string;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    select,
};
use tokio_rustls::TlsStream;
#[cfg(debug_assertions)]
use tracing::trace;

use crate::{Flow, FlowInterrupt, FlowIo};

pub struct Stream {
    stream: TcpStream,
    tls: Option<rustls::Connection>,
    read_buffer: BytesMut,
    write_buffer: BytesMut,
}

impl Stream {
    pub fn insecure(stream: TcpStream) -> Self {
        Self {
            stream,
            tls: None,
            read_buffer: BytesMut::default(),
            write_buffer: BytesMut::default(),
        }
    }

    pub fn tls(stream: TlsStream<TcpStream>) -> Self {
        // We want to use `TcpStream::split` for handling reading and writing separately,
        // but `TlsStream` does not expose this functionality. Therefore, we destruct `TlsStream`
        // into `TcpStream` and `rustls::Connection` and handling them ourselves.
        //
        // Some notes:
        //
        // - There is also `tokio::io::split` which works for all kind of streams. But this
        //   involves too much scary magic because its use-case is reading and writing from
        //   different threads. We prefer to use the more low-level `TcpStream::split`.
        //
        // - We could get rid of `TlsStream` and construct `rustls::Connection` directly.
        //   But `TlsStream` is still useful because it gives us the guarantee that the handshake
        //   was already handled properly.
        //
        // - In the long run it would be nice if `TlsStream::split` would exist and we would use
        //   it because `TlsStream` is better at handling the edge cases of `rustls`.
        let (stream, tls) = match stream {
            TlsStream::Client(stream) => {
                let (stream, tls) = stream.into_inner();
                (stream, rustls::Connection::Client(tls))
            }
            TlsStream::Server(stream) => {
                let (stream, tls) = stream.into_inner();
                (stream, rustls::Connection::Server(tls))
            }
        };

        Self {
            stream,
            tls: Some(tls),
            read_buffer: BytesMut::default(),
            write_buffer: BytesMut::default(),
        }
    }

    pub async fn flush(&mut self) -> Result<(), StreamError<Infallible>> {
        // Flush TLS
        if let Some(tls) = &mut self.tls {
            tls.writer().flush()?;
            encrypt(tls, &mut self.write_buffer, Vec::new())?;
        }

        // Flush TCP
        write(&mut self.stream, &mut self.write_buffer).await?;
        self.stream.flush().await?;

        Ok(())
    }

    pub async fn progress<F: Flow>(
        &mut self,
        mut flow: F,
    ) -> Result<F::Event, StreamError<F::Error>> {
        let event = loop {
            match &mut self.tls {
                None => {
                    // Provide input bytes to the client/server
                    if !self.read_buffer.is_empty() {
                        flow.enqueue_input(&self.read_buffer);
                        self.read_buffer.clear();
                    }
                }
                Some(tls) => {
                    // Decrypt input bytes
                    let plain_bytes = decrypt(tls, &mut self.read_buffer)?;

                    // Provide input bytes to the client/server
                    if !plain_bytes.is_empty() {
                        flow.enqueue_input(&plain_bytes);
                    }
                }
            }

            // Progress the client/server
            let result = flow.progress();

            // Return events immediately without doing IO
            let interrupt = match result {
                Err(interrupt) => interrupt,
                Ok(event) => break event,
            };

            // Return errors immediately without doing IO
            let io = match interrupt {
                FlowInterrupt::Io(io) => io,
                FlowInterrupt::Error(err) => return Err(StreamError::Flow(err)),
            };

            match &mut self.tls {
                None => {
                    // Handle the output bytes from the client/server
                    if let FlowIo::Output(bytes) = io {
                        self.write_buffer.extend(bytes);
                    }
                }
                Some(tls) => {
                    // Handle the output bytes from the client/server
                    let plain_bytes = if let FlowIo::Output(bytes) = io {
                        bytes
                    } else {
                        Vec::new()
                    };

                    // Encrypt output bytes
                    encrypt(tls, &mut self.write_buffer, plain_bytes)?;
                }
            }

            // Progress the stream
            if self.write_buffer.is_empty() {
                read(&mut self.stream, &mut self.read_buffer).await?;
            } else {
                // We read and write the stream simultaneously because otherwise
                // a deadlock between client and server might occur if both sides
                // would only read or only write.
                let (read_stream, write_stream) = self.stream.split();
                select! {
                    result = read(read_stream, &mut self.read_buffer) => result,
                    result = write(write_stream, &mut self.write_buffer) => result,
                }?;
            };
        };

        Ok(event)
    }

    #[cfg(feature = "expose_stream")]
    /// Return the underlying stream for debug purposes (or experiments).
    ///
    /// Note: Writing to or reading from the stream may introduce
    /// conflicts with `imap-flow`.
    pub fn stream_mut(&mut self) -> &mut TcpStream {
        &mut self.stream
    }
}

/// Error during reading into or writing from a stream.
#[derive(Debug, Error)]
pub enum StreamError<E> {
    /// The operation failed because the stream is closed.
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
    /// An error occurred in the IMAP flow.
    #[error(transparent)]
    Flow(E),
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

impl<E> From<ReadWriteError> for StreamError<E> {
    fn from(value: ReadWriteError) -> Self {
        match value {
            ReadWriteError::Closed => StreamError::Closed,
            ReadWriteError::Io(err) => StreamError::Io(err),
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
            match tls.reader().read(&mut plain_bytes_chunk) {
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    // `rustls` doesn't have more data to yield, but it believes the
                    // connection is open. Not sure why an error needs to be handled for
                    // that case, but `tokio_rustls` does the same, see
                    // https://github.com/rustls/tokio-rustls/blob/7448a86b7b5eb69b438dae0782c179e55a2d3e4f/src/common/mod.rs#L226
                    break;
                }
                Err(err) => return Err(DecryptEncryptError::Io(err)),
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
    #[error(transparent)]
    Io(#[from] tokio::io::Error),
    #[error(transparent)]
    Tls(#[from] rustls::Error),
}

impl<E> From<DecryptEncryptError> for StreamError<E> {
    fn from(value: DecryptEncryptError) -> Self {
        match value {
            DecryptEncryptError::Io(err) => StreamError::Io(err),
            DecryptEncryptError::Tls(err) => StreamError::Tls(err),
        }
    }
}
