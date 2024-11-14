use std::io::ErrorKind;

use buf_stream::futures::BufStream;
use futures_util::io::{AsyncRead, AsyncWrite};
use thiserror::Error;

use crate::{Interrupt, Io, State};

pub struct Stream<S> {
    stream: BufStream<S>,
}

impl<S> Stream<S> {
    pub fn new(stream: S) -> Self {
        Self {
            stream: BufStream::new(stream),
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> Stream<S> {
    pub async fn next<F: State>(&mut self, mut state: F) -> Result<F::Event, Error<F::Error>> {
        let event = loop {
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

            // Handle the output bytes from the client/server
            if let Io::Output(bytes) = io {
                self.stream.push_bytes(bytes);
            }

            match self.stream.progress().await {
                Err(err) if err.kind() == ErrorKind::UnexpectedEof => {
                    return Err(Error::Closed);
                }
                Err(err) => {
                    return Err(Error::Io(err));
                }
                Ok(bytes) => {
                    state.enqueue_input(bytes);
                }
            }
        };

        Ok(event)
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
    Io(#[from] std::io::Error),
    /// An error occurred while progressing the state.
    #[error(transparent)]
    State(E),
}
