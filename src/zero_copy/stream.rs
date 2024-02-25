use futures_core::Stream;
use rkyv::AlignedVec;
use rkyv::Archive;
use rkyv::Archived;
use tokio::io::{AsyncRead, ReadBuf};

use futures_core::ready;
use pin_project::pin_project;
use std::borrow::Borrow;
use std::borrow::BorrowMut;
use std::fmt;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tracing::trace;

pub struct RkyvVec<T> {
    bytes: AlignedVec,
    _phantom: std::marker::PhantomData<T>,
}

impl<T> RkyvVec<T> {
    pub fn new(bytes: AlignedVec) -> Self {
        Self {
            bytes,
            _phantom: std::marker::PhantomData,
        }
    }

    pub fn archive(&self) -> &Archived<T>
    where
        T: Archive,
    {
        unsafe { rkyv::archived_root::<T>(&self.bytes) }
    }
    
}

enum DecoderState {
    Header,
    Body(usize),
}

struct RkyvDecoder {
    buffer: AlignedVec,
    eof: bool,
    is_readable: bool,
    has_errored: bool,
    decoder_state: DecoderState,
    written_bytes: usize,
}

/// An error when the number of bytes read is more than max frame length.
struct RkyvDecodeLengthError {
    _priv: (),
}

// ===== impl LengthDelimitedCodecError =====

impl fmt::Debug for RkyvDecodeLengthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RkyvDecodeLengthError").finish()
    }
}

impl fmt::Display for RkyvDecodeLengthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("frame size too big")
    }
}

impl std::error::Error for RkyvDecodeLengthError {}

impl RkyvDecoder {
    fn new() -> Self {
        Self {
            buffer: AlignedVec::new(),
            eof: false,
            is_readable: false,
            has_errored: false,
            decoder_state: DecoderState::Header,
            written_bytes: 0,
        }
    }

    fn decodeable(&mut self) -> Result<usize, io::Error> {
        if 16 - self.buffer.len() > 0 {
            self.decoder_state = DecoderState::Header;
            return Ok(16 - self.buffer.len());
        }
    
        let mut len_bytes = [0; 8];
        len_bytes.copy_from_slice(&self.buffer[0..8]);
        let n = u64::from_le_bytes(len_bytes) as usize;
        self.decoder_state = DecoderState::Body(n);

        match self.buffer.len().cmp(&n) {
            std::cmp::Ordering::Less => Ok(n-self.buffer.len()),
            std::cmp::Ordering::Equal => Ok(0),
            std::cmp::Ordering::Greater => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                RkyvDecodeLengthError { _priv: () },
            )),
        }
    }

    fn read<T: AsyncRead>(&mut self, io: Pin<&mut T>, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        match self.decoder_state {
            DecoderState::Header => {
                self.buffer.resize(1024, 0);
                
            }
            DecoderState::Body(n) => {
                self.buffer.resize(n+1, 0);

            }
        }
        let mut buf = ReadBuf::new(&mut self.buffer[self.written_bytes..]);
        ready!(io.poll_read(cx, &mut buf))?;
        self.written_bytes += buf.filled().len();
        self.buffer.resize(self.written_bytes, 0);
        Poll::Ready(Ok(self.written_bytes))
    }

    /// mem swaps the buffer and 
    fn buffer<T>(&mut self) -> RkyvVec<T> {
        let bytes = std::mem::replace(&mut self.buffer, AlignedVec::with_capacity(self.written_bytes));
        self.written_bytes = 0;
        RkyvVec {
            bytes,
            _phantom: std::marker::PhantomData,
        }
    }
}

struct RWFrames {
    read: RkyvDecoder,
    write: RkyvLengthEncoder,
}

impl Borrow<RkyvDecoder> for RWFrames {
    fn borrow(&self) -> &RkyvDecoder {
        &self.read
    }
}
impl BorrowMut<RkyvDecoder> for RWFrames {
    fn borrow_mut(&mut self) -> &mut RkyvDecoder {
        &mut self.read
    }
}
impl Borrow<RkyvLengthEncoder> for RWFrames {
    fn borrow(&self) -> &RkyvLengthEncoder {
        &self.write
    }
}
impl BorrowMut<RkyvLengthEncoder> for RWFrames {
    fn borrow_mut(&mut self) -> &mut RkyvLengthEncoder {
        &mut self.write
    }
}

#[pin_project]
pub struct RkyvStream<T, I> {
    #[pin]
    inner: T,
    state: RkyvDecoder,
    data: std::marker::PhantomData<I>,
}

impl<T, I> RkyvStream<T, I> {
    pub fn new(inner: T) -> Self {
        Self {
            inner,
            state: RkyvDecoder::new(),
            data: std::marker::PhantomData,
        }
    }
}

impl<T, I> Stream for RkyvStream<T, I>
where
    T: AsyncRead,
    I: rkyv::Archive,
{
    type Item = Result<RkyvVec<I>, io::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {

        let mut pinned = self.project();
        let state: &mut RkyvDecoder = pinned.state.borrow_mut();
        // The following loops implements a state machine with each state corresponding
        // to a combination of the `is_readable` and `eof` flags. States persist across
        // loop entries and most state transitions occur with a return.
        //
        // The initial state is `reading`.
        //
        // | state   | eof   | is_readable | has_errored |
        // |---------|-------|-------------|-------------|
        // | reading | false | false       | false       |
        // | framing | false | true        | false       |
        // | pausing | true  | true        | false       |
        // | paused  | true  | false       | false       |
        // | errored | <any> | <any>       | true        |
        //                                                       `decode_eof` returns Err
        //                                          ┌────────────────────────────────────────────────────────┐
        //                   `decode_eof` returns   │                                                        │
        //                             `Ok(Some)`   │                                                        │
        //                                 ┌─────┐  │     `decode_eof` returns               After returning │
        //                Read 0 bytes     ├─────▼──┴┐    `Ok(None)`          ┌────────┐ ◄───┐ `None`    ┌───▼─────┐
        //               ┌────────────────►│ Pausing ├───────────────────────►│ Paused ├─┐   └───────────┤ Errored │
        //               │                 └─────────┘                        └─┬──▲───┘ │               └───▲───▲─┘
        // Pending read  │                                                      │  │     │                   │   │
        //     ┌──────┐  │            `decode` returns `Some`                   │  └─────┘                   │   │
        //     │      │  │                   ┌──────┐                           │  Pending                   │   │
        //     │ ┌────▼──┴─┐ Read n>0 bytes ┌┴──────▼─┐     read n>0 bytes      │  read                      │   │
        //     └─┤ Reading ├───────────────►│ Framing │◄────────────────────────┘                            │   │
        //       └──┬─▲────┘                └─────┬──┬┘                                                      │   │
        //          │ │                           │  │                 `decode` returns Err                  │   │
        //          │ └───decode` returns `None`──┘  └───────────────────────────────────────────────────────┘   │
        //          │                             read returns Err                                               │
        //          └────────────────────────────────────────────────────────────────────────────────────────────┘
        loop {
            // Return `None` if we have encountered an error from the underlying decoder
            // See: https://github.com/tokio-rs/tokio/issues/3976
            if state.has_errored {
                // preparing has_errored -> paused
                trace!("Returning None and setting paused");
                state.is_readable = false;
                state.has_errored = false;
                return Poll::Ready(None);
            }

            // Repeatedly call `decode` or `decode_eof` while the buffer is "readable",
            // i.e. it _might_ contain data consumable as a frame or closing frame.
            // Both signal that there is no such data by returning `None`.
            //
            // If `decode` couldn't read a frame and the upstream source has returned eof,
            // `decode_eof` will attempt to decode the remaining bytes as closing frames.
            //
            // If the underlying AsyncRead is resumable, we may continue after an EOF,
            // but must finish emitting all of it's associated `decode_eof` frames.
            // Furthermore, we don't want to emit any `decode_eof` frames on retried
            // reads after an EOF unless we've actually read more data.
            let remaining = state.decodeable()?;

            if state.is_readable && remaining == 0 {
                // prepare reading -> paused
                state.is_readable = false;
                return Poll::Ready(Some(Ok(state.buffer())));
            }
            
            // reading or paused
            // If we can't build a frame yet, try to read more data and try again.
            // Make sure we've got room for at least one byte to read to ensure
            // that we don't get a spurious 0 that looks like EOF.
        
            #[allow(clippy::blocks_in_conditions)]
            let bytect = match state.read(pinned.inner.as_mut(), cx).map_err(
                |err| {
                    trace!("Got an error, going to errored state");
                    state.has_errored = true;
                    err
                },
            )? {
                Poll::Ready(ct) => ct,
                // implicit reading -> reading or implicit paused -> paused
                Poll::Pending => return Poll::Pending,
            };
            if bytect == 0 {
                if state.eof {
                    // We're already at an EOF, and since we've reached this path
                    // we're also not readable. This implies that we've already finished
                    // our `decode_eof` handling, so we can simply return `None`.
                    // implicit paused -> paused
                    return Poll::Ready(None);
                }
                // prepare reading -> paused
                state.eof = true;
            } else {
                // prepare paused -> framing or noop reading -> framing
                state.eof = false;
            }

            // paused -> framing or reading -> framing or reading -> pausing
            state.is_readable = true;
        }
    }
}
