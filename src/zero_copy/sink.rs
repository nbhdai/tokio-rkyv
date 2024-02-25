use rkyv::ser::Serializer;
use rkyv::{AlignedVec, Fallible};
use tokio::io::AsyncWrite;

use futures_core::ready;
use futures_sink::Sink;
use pin_project::pin_project;
use std::borrow::BorrowMut;
use std::io::{self, IoSlice, Write};
use std::pin::Pin;
use std::task::{Context, Poll};
use tracing::trace;

const INITIAL_CAPACITY: usize = 8 * 1024;

#[derive(Debug)]
struct RkyvLengthEncoder {
    buffer: AlignedVec,
    written: usize,
    backpressure_boundary: usize,
}

impl  Fallible for RkyvLengthEncoder  {
    type Error = io::Error;
}

impl Serializer for RkyvLengthEncoder {
    /// Returns the current position of the serializer.
    fn pos(&self) -> usize {
        0
    }

    /// Attempts to write the given bytes to the serializer.
    fn write(&mut self, bytes: &[u8]) -> Result<(), Self::Error> {
        self.buffer.write(bytes)?;
        Ok(())
    }
}

impl RkyvLengthEncoder {
    pub fn new() -> Self {
        Self {
            buffer: AlignedVec::with_capacity(INITIAL_CAPACITY),
            written: 0,
            backpressure_boundary: INITIAL_CAPACITY,
        }
    }

    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    pub fn has_remaining(&self) -> bool {
        self.written < self.buffer.len()
    }

    pub fn data(&self) -> &[u8] {
        &self.buffer[self.written..]
    }

    pub fn advance(&mut self, n: usize) {
        self.written += n;
    }

    pub fn encode<'a, T: rkyv::Serialize<Self>>(&mut self, data: T) -> Result<(), io::Error> {
        // Clear and write the header to the data.
        self.buffer.clear();
        self.buffer.extend_from_slice(&[0;16]);
        self.written = 0;
        // Serialize the data to the buffer.
        data.serialize(self).unwrap();
        // Update the header with the total written
        let len_bytes = (self.buffer.len() as u64).to_ne_bytes();
        self.buffer[0..8].copy_from_slice(&len_bytes);

        Ok(())
    }
}

#[pin_project]
#[derive(Debug)]
pub struct RkyvSink<T> {
    #[pin]
    inner: T,
    state: RkyvLengthEncoder,
}

impl <T> RkyvSink<T> {
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            state: RkyvLengthEncoder::new(),
        }
    }
}

fn poll_write_buf<T: AsyncWrite>(
    io: Pin<&mut T>,
    cx: &mut Context<'_>,
    buf: &mut RkyvLengthEncoder,
) -> Poll<io::Result<usize>> {
    const MAX_BUFS: usize = 64;

    if !buf.has_remaining() {
        return Poll::Ready(Ok(0));
    }

    let n = if io.is_write_vectored() {
        let slices = [IoSlice::new(buf.data()); MAX_BUFS];

        ready!(io.poll_write_vectored(cx, &slices))?
    } else {
        ready!(io.poll_write(cx, buf.data()))?
    };

    buf.advance(n);

    Poll::Ready(Ok(n))
}

impl<T, I> Sink<I> for RkyvSink<T>
where
    T: AsyncWrite,
    I: rkyv::Serialize<RkyvLengthEncoder>,
{
    type Error = io::Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        if self.state.len() >= self.state.backpressure_boundary {
            Sink::<I>::poll_flush(self.as_mut(), cx)
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn start_send(self: Pin<&mut Self>, item: I) -> Result<(), Self::Error> {
        let pinned = self.project();
        pinned
            .state
            .encode(item)?;
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        trace!("flushing framed transport");
        let mut pinned = self.project();

        while !pinned.state.borrow_mut().is_empty() {
            let buffer = pinned.state.borrow_mut();
            trace!(remaining = buffer.len(), "writing;");

            let n = ready!(poll_write_buf(pinned.inner.as_mut(), cx, buffer))?;

            if n == 0 {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to \
                     write frame to transport",
                )
                .into()));
            }
        }

        // Try flushing the underlying IO
        ready!(pinned.inner.poll_flush(cx))?;

        trace!("framed transport flushed");
        Poll::Ready(Ok(()))
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        ready!(Sink::<I>::poll_flush(self.as_mut(), cx))?;
        ready!(self.project().inner.poll_shutdown(cx))?;

        Poll::Ready(Ok(()))
    }
}