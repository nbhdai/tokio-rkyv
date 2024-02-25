//! This crate provides the utilities needed to easily implement a Tokio
//! transport using [serde] for serialization and deserialization of frame
//! values.
//!
//! # Introduction
//!
//! This crate provides [transport] combinators that transform a stream of
//! frames encoded as bytes into a stream of frame values. It is expected that
//! the framing happens at another layer. One option is to use a [length
//! delimited] framing transport.
//!
//! The crate provides two traits that must be implemented: [`Serializer`] and
//! [`Deserializer`]. Implementations of these traits are then passed to
//! [`Framed`] along with the upstream [`Stream`] or
//! [`Sink`] that handles the byte encoded frames.
//!
//! By doing this, a transformation pipeline is built. For reading, it looks
//! something like this:
//!
//! * `tokio_serde::Framed`
//! * `tokio_util::codec::FramedRead`
//! * `tokio::net::TcpStream`
//!
//! The write half looks like:
//!
//! * `tokio_serde::Framed`
//! * `tokio_util::codec::FramedWrite`
//! * `tokio::net::TcpStream`
//!
//! # Examples
//!
//! For an example, see how JSON support is implemented:
//!
//! * [server](https://github.com/carllerche/tokio-serde/blob/master/examples/server.rs)
//! * [client](https://github.com/carllerche/tokio-serde/blob/master/examples/client.rs)
//!
//! [serde]: https://serde.rs
//! [serde-json]: https://github.com/serde-rs/json
//! [transport]: https://tokio.rs/docs/going-deeper/transports/
//! [length delimited]: https://docs.rs/tokio-util/0.2/tokio_util/codec/length_delimited/index.html
//! [`Serializer`]: trait.Serializer.html
//! [`Deserializer`]: trait.Deserializer.html
//! [`Framed`]: struct.Framed.html
//! [`Stream`]: https://docs.rs/futures/0.3/futures/stream/trait.Stream.html
//! [`Sink`]: https://docs.rs/futures/0.3/futures/sink/trait.Sink.html

#![cfg_attr(docsrs, feature(doc_cfg))]

use bytes::{Bytes, BytesMut};
use futures_core::{ready, Stream, TryStream};
use futures_sink::Sink;
use pin_project::pin_project;
use rkyv::{ser::serializers::{AllocScratch, AllocScratchError, CompositeSerializer, CompositeSerializerError, FallbackScratch, HeapScratch, SharedSerializeMap, SharedSerializeMapError, WriteSerializer}, validation::validators::DefaultValidator, Archive, Archived, CheckBytes, Deserialize, Fallible, Infallible};
use std::{
    marker::PhantomData, pin::Pin, task::{Context, Poll}
};

type FramedSerializer<const N: usize> = CompositeSerializer<WriteSerializer<Vec<u8>>, FallbackScratch<HeapScratch<N>, AllocScratch>, SharedSerializeMap>;
fn new<const N: usize>() -> FramedSerializer<N> {
    CompositeSerializer::new(WriteSerializer::new(Vec::new()), FallbackScratch::new(HeapScratch::<N>::new(), AllocScratch::new()), SharedSerializeMap::new())
}

type FramedError = CompositeSerializerError<std::io::Error, AllocScratchError, SharedSerializeMapError>;

pub struct RkyvBytes<T> {
    bytes: BytesMut,
    _phantom: PhantomData<T>,
}

impl<T> RkyvBytes<T> {
    pub fn new(bytes: BytesMut) -> Self {
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


/// Adapts a transport to a value sink by serializing the values and to a stream of values by deserializing them.
///
/// It is expected that the buffers yielded by the supplied transport be framed. In
/// other words, each yielded buffer must represent exactly one serialized
/// value.
///
/// The provided transport will receive buffer values containing the
/// serialized value. Each buffer contains exactly one value. This sink will be
/// responsible for writing these buffers to an `AsyncWrite` using some sort of
/// framing strategy.
///
/// The specific framing strategy is left up to the
/// implementor. One option would be to use [length_delimited] provided by
/// [tokio-util].
///
/// [length_delimited]: http://docs.rs/tokio-util/0.2/tokio_util/codec/length_delimited/index.html
/// [tokio-util]: http://crates.io/crates/tokio-util
#[pin_project]
#[derive(Debug)]
pub struct Framed<Transport, Item, SinkItem, UP, const N: usize = 1024> {
    #[pin]
    inner: Transport,
    packer: UP,
    item: PhantomData<(Item, SinkItem)>,
}

pub trait Unpacker {
    type Item;
    fn unpack(bytes: BytesMut) -> Result<Self::Item, std::io::Error>;
}

pub struct ZeroCopyUnpacker<I> {
    item: PhantomData<I>
}

impl<I> Unpacker for ZeroCopyUnpacker<I> {
    type Item = RkyvBytes<I>;
    fn unpack(bytes: BytesMut) -> Result<Self::Item, std::io::Error> {
        Ok(RkyvBytes::<I>::new(bytes))
    }
}

pub struct DeserUnpacker<I> {
    item: PhantomData<I>
}

impl<I: Archive> Unpacker for DeserUnpacker<I>
    where I: Archive,
    Archived<I>: Deserialize<I, Infallible>
{
    type Item = I;
    fn unpack(bytes: BytesMut) -> Result<Self::Item, std::io::Error> {
        let archived = unsafe { rkyv::archived_root::<I>(&bytes) };
        let item: I = archived.deserialize(&mut Infallible).unwrap();
        Ok(item)
    }
}

pub struct CheckedZeroCopyUnpacker<I> {
    item: PhantomData<I>
}

impl<'a, I> Unpacker for CheckedZeroCopyUnpacker<I>
    where I: Archive,
    Archived<I>: CheckBytes<DefaultValidator<'a>> + 'a,
{
    type Item = RkyvBytes<I>;
    fn unpack(bytes: BytesMut) -> Result<Self::Item, std::io::Error> {
        let bbytes = unsafe { std::slice::from_raw_parts(bytes.as_ptr(), bytes.len()) };
        let _archived = rkyv::check_archived_root::<I>(bbytes).map_err(|e| {
            tracing::error!("Invalid rkyv archive: {}", e);
            std::io::Error::new(std::io::ErrorKind::InvalidData, "Invalid rkyv archive")
        })?;

        Ok(RkyvBytes::<I>::new(bytes))
    }
}

pub struct CheckedDeserUnpacker<I> {
    item: PhantomData<I>
}

impl<'a, I> Unpacker for CheckedDeserUnpacker<I>
    where I: Archive,
    Archived<I>: Deserialize<I, Infallible> + CheckBytes<DefaultValidator<'a>> + 'a,
{
    type Item = I;
    fn unpack(bytes: BytesMut) -> Result<Self::Item, std::io::Error> {
        // relifetime this borrow to the lifetime of the bytes, safe as we drop the archive before the bytes
        let bbytes = unsafe { std::slice::from_raw_parts(bytes.as_ptr(), bytes.len()) };
        let archived = rkyv::check_archived_root::<I>(bbytes).map_err(|e| {
            tracing::error!("Invalid rkyv archive: {}", e);
            std::io::Error::new(std::io::ErrorKind::InvalidData, "Invalid rkyv archive")
        })?;
        let item: I = archived.deserialize(&mut Infallible).unwrap();
        Ok(item)
    }
}

impl<Transport, Item, SinkItem> Framed<Transport, Item, SinkItem, ZeroCopyUnpacker<Item>> {
    /// Creates a new `Framed` with the given transport and codec.
    pub fn new(inner: Transport) -> Self
    where {
        Self {
            inner,
            packer: ZeroCopyUnpacker {item : PhantomData},
            item: PhantomData,
        }
    }
}

impl<Transport, Item, SinkItem, const N: usize> Framed<Transport, Item, SinkItem, ZeroCopyUnpacker<Item>, N> {
    /// Creates a new `Framed` with the given transport and codec.
    pub fn new_with_size(inner: Transport) -> Self
    where {
        Self {
            inner,
            packer: ZeroCopyUnpacker {item : PhantomData},
            item: PhantomData,
        }
    }
}

impl<Transport, Item, SinkItem, UP, const N: usize> Framed<Transport, Item, SinkItem, UP, N> {

    /// Returns a reference to the underlying transport wrapped by `Framed`.
    ///
    /// Note that care should be taken to not tamper with the underlying transport as
    /// it may corrupt the sequence of frames otherwise being worked with.
    pub fn get_ref(&self) -> &Transport {
        &self.inner
    }

    /// Returns a mutable reference to the underlying transport wrapped by
    /// `Framed`.
    ///
    /// Note that care should be taken to not tamper with the underlying transport as
    /// it may corrupt the sequence of frames otherwise being worked with.
    pub fn get_mut(&mut self) -> &mut Transport {
        &mut self.inner
    }

    /// Consumes the `Framed`, returning its underlying transport.
    ///
    /// Note that care should be taken to not tamper with the underlying transport as
    /// it may corrupt the sequence of frames otherwise being worked with.
    pub fn into_inner(self) -> Transport {
        self.inner
    }
}

impl<Transport, Item, SinkItem, UP, const N: usize> Stream for Framed<Transport, Item, SinkItem, UP, N>
where
    Transport: TryStream<Ok = BytesMut>,
    BytesMut: From<Transport::Ok>,
    Item: Archive,
    UP: Unpacker<Item = Item>,
    Transport::Error: From<std::io::Error>,
{
    type Item = Result<UP::Item, Transport::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let pinned = self.project();
        match ready!(pinned.inner.try_poll_next(cx)) {
            Some(bytes) => {
                let item = UP::unpack(bytes?)?;
                Poll::Ready(Some(Ok(item)))
            }
            None => Poll::Ready(None),
        }
    }
}

impl<Transport, Item, SinkItem, UP, const N: usize> Sink<SinkItem> for Framed<Transport, Item, SinkItem, UP, N>
where
    Transport: Sink<Bytes>,
    SinkItem: rkyv::Serialize<FramedSerializer<N>>,
{
    type Error = Transport::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.project().inner.poll_ready(cx)
    }

    fn start_send(mut self: Pin<&mut Self>, item: SinkItem) -> Result<(), Self::Error> {
        let mut ser = new::<N>();
        item.serialize(&mut ser).unwrap();
        let vec = ser.into_serializer().into_inner();
        let bytes = Bytes::from(vec);

        self.as_mut().project().inner.start_send(bytes)?;

        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.project().inner.poll_flush(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        ready!(self.as_mut().poll_flush(cx))?;
        self.project().inner.poll_close(cx)
    }
}

pub type SymmetricallyFramed<Transport, Value, UP, const N: usize = 1024> = Framed<Transport, Value, Value, UP, N>;
pub type ZeroCopyFramed<Transport, Value, const N: usize = 1024> = Framed<Transport, Value, Value, ZeroCopyUnpacker<Value>, N>;
pub type DeserFramed<Transport, Value, const N: usize = 1024> = Framed<Transport, Value, Value, DeserUnpacker<Value>, N>;
pub type CheckedZeroCopyFramed<Transport, Value, const N: usize = 1024> = Framed<Transport, Value, Value, CheckedZeroCopyUnpacker<Value>, N>;
pub type CheckedDeserFramed<Transport, Value, const N: usize = 1024> = Framed<Transport, Value, Value, CheckedDeserUnpacker<Value>, N>;

#[cfg(test)]
mod tests {
    
}