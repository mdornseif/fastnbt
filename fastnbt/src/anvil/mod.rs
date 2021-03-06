use byteorder::{BigEndian, ReadBytesExt};
use num_enum::TryFromPrimitive;
use std::convert::TryFrom;
use std::io::{Cursor, Read, Seek, SeekFrom};

pub const SECTOR_SIZE: usize = 4096;
pub const HEADER_SIZE: usize = 2 * SECTOR_SIZE;

pub mod biome;
pub mod bits;
pub mod draw;

#[derive(Debug, TryFromPrimitive)]
#[repr(u8)]
pub enum CompressionScheme {
    Gzip = 1,
    Zlib = 2,
}

pub struct Region<S: Seek + Read> {
    data: S,
}

#[derive(Debug, PartialEq)]
pub struct ChunkLocation {
    pub begin_sector: usize,
    pub sector_count: usize,
    pub x: usize,
    pub z: usize,
}

#[derive(Debug)]
pub struct ChunkMeta {
    pub len: u32,
    pub compression_scheme: CompressionScheme,
}

impl ChunkMeta {
    pub fn new(data: &[u8]) -> Result<Self> {
        if data.len() < 5 {
            return Err(Error::InsufficientData);
        }

        let mut buf = (&data[..5]).clone();
        let len = buf.read_u32::<BigEndian>()?;
        let scheme = buf.read_u8()?;
        let scheme = CompressionScheme::try_from(scheme).map_err(|_| Error::InvalidChunkMeta)?;

        Ok(Self {
            len,
            compression_scheme: scheme,
        })
    }
}

impl<S: Seek + Read> Region<S> {
    pub fn new(data: S) -> Self {
        Self { data }
    }

    pub fn chunk_location(&mut self, x: usize, z: usize) -> Result<ChunkLocation> {
        if x >= 32 || z >= 32 {
            return Err(Error::InvalidOffset(x, z));
        }

        let pos = 4 * ((x % 32) + (z % 32) * 32);

        self.data.seek(SeekFrom::Start(pos as u64))?;

        let mut buf = [0u8; 4];
        self.data.read_exact(&mut buf[..])?;

        let mut off = 0usize;
        off = off | ((buf[0] as usize) << 16);
        off = off | ((buf[1] as usize) << 8);
        off = off | ((buf[2] as usize) << 0);
        let count = buf[3] as usize;
        Ok(ChunkLocation {
            begin_sector: off,
            sector_count: count,
            x,
            z,
        })
    }

    pub fn load_chunk(&mut self, offset: &ChunkLocation, dest: &mut [u8]) -> Result<()> {
        self.data.seek(SeekFrom::Start(
            offset.begin_sector as u64 * SECTOR_SIZE as u64,
        ))?;

        self.data.read_exact(dest)?;
        Ok(())
    }
}

#[derive(Debug)]
pub enum Error {
    InsufficientData,
    IO(std::io::Error),
    InvalidOffset(usize, usize),
    InvalidChunkMeta,
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Error {
        Error::IO(err)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub struct Builder {
    inner: Vec<u8>,
}

impl Builder {
    pub fn new() -> Self {
        Self { inner: Vec::new() }
    }

    pub fn location(mut self, offset: u32, sectors: u8) -> Self {
        self.inner.extend_from_slice(&offset.to_be_bytes()[1..4]);
        self.inner.push(sectors);
        self
    }

    pub fn build(mut self) -> Cursor<Vec<u8>> {
        let padded_sector_count = (self.inner.len() / SECTOR_SIZE) + 1;
        self.inner.resize(padded_sector_count * SECTOR_SIZE, 0);
        Cursor::new(self.inner)
    }

    pub fn build_unpadded(self) -> Cursor<Vec<u8>> {
        Cursor::new(self.inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_offset() {
        let r = Builder::new().location(2, 1).build();
        let mut r = Region::new(r);
        match r.chunk_location(32, 32) {
            Err(Error::InvalidOffset(32, 32)) => {}
            _ => panic!("should error"),
        }
    }

    #[test]
    fn invalid_offset_only_in_x() {
        let r = Builder::new().location(2, 1).build();
        let mut r = Region::new(r);
        match r.chunk_location(32, 0) {
            Err(Error::InvalidOffset(32, 0)) => {}
            _ => panic!("should error"),
        }
    }

    #[test]
    fn invalid_offset_only_in_z() {
        let r = Builder::new().location(2, 1).build();
        let mut r = Region::new(r);
        match r.chunk_location(0, 32) {
            Err(Error::InvalidOffset(0, 32)) => {}
            _ => panic!("should error"),
        }
    }

    #[test]
    fn offset_beyond_data_given() {
        let r = Builder::new().location(2, 1).build_unpadded();
        let mut r = Region::new(r);
        match r.chunk_location(1, 0) {
            Err(Error::IO(inner)) if inner.kind() == std::io::ErrorKind::UnexpectedEof => {}
            o => panic!("should error {:?}", o),
        }
    }
    #[test]
    fn first_location() -> Result<()> {
        let r = Builder::new().location(2, 1).build();
        let mut r = Region::new(r);

        assert_eq!(
            ChunkLocation {
                begin_sector: 2,
                sector_count: 1,
                x: 0,
                z: 0
            },
            r.chunk_location(0, 0)?
        );
        Ok(())
    }
}
