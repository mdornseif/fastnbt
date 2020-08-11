use byteorder::{BigEndian, ReadBytesExt};
use flate2::read::ZlibDecoder;
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
    // big-endian integers, representing the last modification time of a chunk 
    // in epoch seconds
    timestamps: Vec<u32>,
    // the first three bytes are a (big-endian) offset in 4KiB sectors 
    // from the start of the file, and a remaining byte that gives the 
    // length of the chunk (also in 4KiB sectors, rounded up)
    locations: Vec<Option<ChunkLocation>>,
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub struct ChunkLocation {
    pub begin_sector: usize,
    pub sector_count: usize,
    pub x: usize, 
    pub z: usize,
}

// Encodes how the NBT-Data is compressed
#[derive(Debug)]
pub struct ChunkMeta {
    //  the compressed data is len-1 bytes
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

// find the file offset within a region file
pub fn chunk_offset(x: usize, z: usize) -> usize {
    ((x % 32) + (z % 32) * 32) as usize
}

impl<S: Seek + Read> Region<S> {
    pub fn new(mut data: S) -> Self {
        let mut locations = [None; 1024];
        let mut timestamps = [0u32; 1024];

        // Locations
        let mut locbuf = vec![0u8; 4096];
        data.read_exact(&mut locbuf[..]).unwrap();

        // Timestamps
        let mut tsbuf = vec![0u8; 4096];
        data.read_exact(&mut tsbuf[..]).unwrap();
        let mut rdr = Cursor::new(tsbuf);
        rdr.read_u32_into::<BigEndian>(&mut timestamps).unwrap();

        // Decode Locations
        for xpos in 0..32 {
            for zpos in 0..32 {
                let pos = 4 * chunk_offset(xpos, zpos);
                let mut off = 0usize;
                off = off | ((locbuf[pos + 0] as usize) << 16);
                off = off | ((locbuf[pos + 1] as usize) << 8);
                off = off | ((locbuf[pos + 2] as usize) << 0);
                let count = locbuf[pos+3] as usize;
                if off > 0 && count > 0 {
                    locations[chunk_offset(xpos, zpos)] = Some(ChunkLocation {
                        begin_sector: off,
                        sector_count: count,
                        x: xpos as usize,
                        z: zpos as usize});
                    if timestamps[chunk_offset(xpos, zpos)] == 0 {
                        eprintln!("invalid timestamp for existing chunk: {} {}, fixing", xpos, zpos);
                        timestamps[chunk_offset(xpos, zpos)] = 1
                    }
                } else {
                    if timestamps[chunk_offset(xpos, zpos)] != 0 {
                        eprintln!("invalid timestamp for empty chunk: {} {} {}, fixing", timestamps[chunk_offset(xpos, zpos)],  xpos, zpos);
                        timestamps[chunk_offset(xpos, zpos)] = 0
                    }
                }
            }
        }

        Self {
            data, 
            locations: locations.to_vec(), 
            timestamps: timestamps.to_vec(), 
        }
    }

    /// Return the (region-relative) Chunk location (x, z)
    pub fn chunk_location(&mut self, x: usize, z: usize) -> Result<ChunkLocation> {
        if x >= 32 || z >= 32 {
            return Err(Error::InvalidOffset(x, z));
        }

        match self.locations[chunk_offset(x, z)] {
            Some(offset) => Ok(offset),
            _ => Err(Error::ChunkNotFound)
        }
    }

    /// Return the raw, compressed data for a chunk at ChunkLocation
    pub fn load_chunk(&mut self, offset: &ChunkLocation, dest: &mut Vec<u8>) -> Result<()> {
        dest.resize(offset.sector_count * SECTOR_SIZE, 0u8);
        self.data.seek(SeekFrom::Start(
            offset.begin_sector as u64 * SECTOR_SIZE as u64,
        ))?;

        self.data.read_exact(dest)?;
        Ok(())
    }

    /// Return the raw, compressed data for a chunk at the (region-relative) Chunk location (x, z)
    pub fn load_chunk_at_location(&mut self, x: usize, z: usize) -> Result<Vec<u8>> {
        let location = self.chunk_location(x, z)?;

        // 0,0 chunk location means the chunk isn't present.
        if location.begin_sector != 0 && location.sector_count != 0 {
            let mut buf = Vec::new();
            self.load_chunk(&location, &mut buf)?;
            Ok(buf)
        } else {
            Err(Error::ChunkNotFound)
        }
    }

    /// Return the raw, uncompressed NBT data for a chunk at the (region-relative) Chunk location (x, z)
    ///
    /// Can be further processed with `nbt::Parser::new()` or even with `Blob::from_reader()` of hematite_nbt.
    pub fn load_chunk_nbt_at_location(&mut self, x: usize, z: usize) -> Result<Vec<u8>> {
        let data = self.load_chunk_at_location(x, z)?;
        Ok(decompress_chunk(&data))
    }

/// Call f function with earch uncompressed, non-empty chunk
pub fn for_each_chunk(&mut self, mut f: impl FnMut(usize, usize, &Vec<u8> )) -> Result<()>{
    let mut offsets = Vec::<ChunkLocation>::new();

    // Build list of existing chunks
    for x in 0..32 {
        for z in 0..32 {
            let loc = self.chunk_location(x, z)?;
            // 0,0 chunk location means the chunk isn't present.
            // cannot decide if this means we should return an error from chunk_location() or not.
            if loc.begin_sector != 0 && loc.sector_count != 0 {
                offsets.push(loc);
            }
        }
    }
    // sort for efficient file seeks during processing
    offsets.sort_by(|o1, o2| o2.begin_sector.cmp(&o1.begin_sector));
    offsets.shrink_to_fit();

    let mut buf = Vec::new();
    while !offsets.is_empty() {
        let location: ChunkLocation = offsets.pop().ok_or(0).unwrap();
        // TODO: move outside the loop
        self.load_chunk(&location, &mut buf)?;
        let raw = decompress_chunk(&buf);

        f(location.x, location.z, &raw)
    
    }
    Ok(())
}

}

// Read Information Bytes of Minecraft Chunk and decompress it
fn decompress_chunk(data: &Vec<u8>) -> Vec<u8> {
    // Metadata encodes the length in bytes and the compression type
    let meta = ChunkMeta::new(data.as_slice()).unwrap();

    // compressed data starts at byte 5
    let inbuf = &mut &data[5..];
    let mut decoder = match meta.compression_scheme {
        CompressionScheme::Zlib => ZlibDecoder::new(inbuf),
        _ => panic!("unknown compression scheme (gzip?)"),
    };
    let mut outbuf = Vec::new();
    // read the whole Chunk
    decoder.read_to_end(&mut outbuf).unwrap();
    outbuf
}


#[derive(Debug)]
pub enum Error {
    InsufficientData,
    IO(std::io::Error),
    InvalidOffset(usize, usize),
    InvalidChunkMeta,
    ChunkNotFound,
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
