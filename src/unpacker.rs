use std::marker::PhantomData;

use bytes::BytesMut;
use rkyv::{validation::validators::DefaultValidator, Archive, Archived, CheckBytes, Deserialize, Infallible};

use crate::transport::RkyvBytes;



pub trait Unpacker {
    type Item;
    type Input;
    fn unpack(bytes: Self::Input) -> Result<Self::Item, std::io::Error>;
}

pub struct ZeroCopyUnpacker<I> {
    item: PhantomData<I>,
}

impl<I> Unpacker for ZeroCopyUnpacker<I> {
    type Item = RkyvBytes<I>;
    type Input = BytesMut;
    fn unpack(bytes: BytesMut) -> Result<Self::Item, std::io::Error> {
        Ok(RkyvBytes::<I>::new(bytes))
    }
}

pub struct DeserUnpacker<I> {
    item: PhantomData<I>,
}

impl<I: Archive> Unpacker for DeserUnpacker<I>
where
    I: Archive,
    Archived<I>: Deserialize<I, Infallible>,
{
    type Item = I;
    type Input = BytesMut;
    fn unpack(bytes: BytesMut) -> Result<Self::Item, std::io::Error> {
        let archived = unsafe { rkyv::archived_root::<I>(&bytes) };
        let item: I = archived.deserialize(&mut Infallible).unwrap();
        Ok(item)
    }
}

pub struct CheckedZeroCopyUnpacker<I> {
    item: PhantomData<I>,
}

impl<'a, I> Unpacker for CheckedZeroCopyUnpacker<I>
where
    I: Archive,
    Archived<I>: CheckBytes<DefaultValidator<'a>> + 'a,
{
    type Item = RkyvBytes<I>;
    type Input = BytesMut;
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
    item: PhantomData<I>,
}

impl<'a, I> Unpacker for CheckedDeserUnpacker<I>
where
    I: Archive,
    Archived<I>: Deserialize<I, Infallible> + CheckBytes<DefaultValidator<'a>> + 'a,
{
    type Item = I;
    type Input = BytesMut;
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