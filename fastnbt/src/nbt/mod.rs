use byteorder::{BigEndian, ReadBytesExt};
use num_enum::TryFromPrimitive;
use std::convert::TryFrom;
use std::io::Read;

type Name = Option<String>;

#[cfg(test)]
mod test;

/// The NBT tag. This does not carry the value or the name.
#[derive(Debug, TryFromPrimitive, PartialEq, Clone)]
#[repr(u8)]
pub enum Tag {
    End = 0,
    Byte = 1,
    Short = 2,
    Int = 3,
    Long = 4,
    Float = 5,
    Double = 6,
    ByteArray = 7,
    String = 8,
    List = 9,
    Compound = 10,
    IntArray = 11,
    LongArray = 12,
}

/// An NBT value.
///
/// For every value except compounds and lists, this contains the value of the tag. For example, a `Value::Byte` will
/// contain the name and the byte of that NBT tag.
///
/// The name part of each variant is optional, since elements in an NBT list do not have names. The end of lists do not
/// have a name in the binary format, so it isn't included here either.
///
/// See `nbt::Parser` for more information.
#[derive(Debug, PartialEq)]
pub enum Value {
    CompoundEnd,
    Byte(Name, i8),
    Short(Name, i16),
    Int(Name, i32),
    Long(Name, i64),
    Float(Name, f32),
    Double(Name, f64),
    ByteArray(Name, Vec<i8>),
    String(Name, String),
    List(Name, Tag, i32),
    ListEnd,
    Compound(Name),
    IntArray(Name, Vec<i32>),
    LongArray(Name, Vec<i64>),
}

#[derive(Debug)]
pub enum Error {
    IO(std::io::Error),
    ShortRead,
    InvalidTag(u8),
    InvalidName,
    EOF,
}

pub type Result<T> = std::result::Result<T, Error>;

/// Parser can take any reader and parse it as NBT data. Does not do decompression.
///
/// # Examples
///
/// ## Dump NBT
/// The following takes a stream of GZip compressed data from stdin and dumps it out in Rust's `Debug` format, with
/// some indentation to help see the structure.
///
/// ```man
/// let stdin = std::io::stdin();
/// let decoder = GzDecoder::new(stdin);
///
/// let mut parser = nbt::Parser::new(decoder);
/// let mut indent = 0;
///
/// loop {
///     match parser.next() {
///         Err(e) => {
///             println!("{:?}", e);
///             break;
///         }
///         Ok(value) => {
///             match value {
///                 nbt::Value::CompoundEnd => indent -= 4,
///                 nbt::Value::ListEnd => indent -= 4,
///                 _ => {}
///             }
///
///             println!("{:indent$}{:?}", "", value, indent = indent);
///
///             match value {
///                 nbt::Value::Compound(_) => indent += 4,
///                 nbt::Value::List(_, _, _) => indent += 4,
///                 _ => {}
///             }
///         }
///     }
/// }
/// ```
/// ## Finding a heightmap
/// Here we assume we've parsed up until we have entered the `Heightmaps` compound of the
/// [Minecraft Anvil chunk format](https://minecraft.gamepedia.com/Chunk_format). We keep parsing until we find the
/// `WORLD_SURFACE` long array. We avoid entering nested compounds by skipping them if we enter one. We know we have
/// finished with the current compound when we see the `CompoundEnd` value.
///
/// ```
/// use fastnbt::nbt::{self, Value};
/// use fastnbt::anvil::bits;
///
/// # fn f() -> nbt::Result<Option<Vec<u16>>> {
/// let mut parser = /* ... */
/// # nbt::Parser::new(&[1u8,2,3][..]);
///
/// loop {
///     match parser.next()? {
///         Value::LongArray(Some(ref name), data) if name == "WORLD_SURFACE" => {
///             nbt::skip_compound(&mut parser)?;
///             return Ok(Some(bits::expand_heightmap(data.as_slice())));
///         }
///         Value::Compound(_) => {
///             // don't enter another compound.
///             nbt::skip_compound(&mut parser)?;
///         }
///         Value::CompoundEnd => {
///             // No heightmap found, it happens.
///             return Ok(None);
///         }
///         _ => {}
///     }
/// }
/// # }
/// ```
pub struct Parser<R: Read> {
    reader: R,
    layers: Vec<Layer>,
}

impl<R: Read> Parser<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            layers: Vec::new(),
        }
    }

    pub fn next(&mut self) -> Result<Value> {
        let v = self.next_inner();
        //println!("{:?}", v);
        v
    }

    /// Get the next value from the reader. Returns EOF if the stream ended sucessfully, and
    /// IO(err) for any other IO error.
    fn next_inner(&mut self) -> Result<Value> {
        let last_layer = self.layers.last().map(|l| (*l).clone());
        match last_layer {
            Some(Layer::List(_, 0)) => {
                self.layers.pop();
                return Ok(Value::ListEnd);
            }
            Some(_) => {}
            None => {}
        }

        if let Some(layer) = self.layers.last_mut() {
            match layer {
                Layer::List(_, remainder) => {
                    *remainder -= 1;
                }
                Layer::Compound => {}
            };
        }

        let last_layer = self.layers.last().map(|l| (*l).clone());
        if let Some(layer) = last_layer {
            match layer {
                Layer::List(tag, _) => return self.read_payload(tag.clone(), None),
                Layer::Compound => {}
            };
        }

        // If we get EOF reading a tag, it means we completed a tag to get here, so this is a
        // natural end of stream.
        let tag = match self.reader.read_u8() {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(Error::EOF)?,
            e => e?,
        };

        let tag = u8_to_tag(tag)?;

        if tag == Tag::End {
            // End tags have no name or value.
            let last_layer = self.layers.last().map(|l| (*l).clone());
            return match last_layer {
                Some(Layer::Compound) => {
                    self.layers.pop();
                    Ok(Value::CompoundEnd)
                }
                Some(_) => Err(Error::InvalidTag(0)),
                None => Err(Error::InvalidTag(0)), // special case for no root compound?
            };
        }

        let name = Some(self.read_size_prefixed_string()?);

        self.read_payload(tag, name)
    }

    fn read_size_prefixed_string(&mut self) -> Result<String> {
        let name_len = self.reader.read_u16::<BigEndian>()? as usize;

        let mut buf = vec![0; name_len];
        self.reader.read_exact(&mut buf[..])?;

        Ok(std::str::from_utf8(&buf[..])
            .map_err(|_| Error::InvalidName)?
            .to_owned())
    }

    fn read_payload(&mut self, tag: Tag, name: Name) -> Result<Value> {
        match tag {
            Tag::Byte => Ok(Value::Byte(name, self.reader.read_i8()?)),
            Tag::Short => Ok(Value::Short(name, self.reader.read_i16::<BigEndian>()?)),
            Tag::Int => Ok(Value::Int(name, self.reader.read_i32::<BigEndian>()?)),
            Tag::Long => Ok(Value::Long(name, self.reader.read_i64::<BigEndian>()?)),
            Tag::Float => Ok(Value::Float(name, self.reader.read_f32::<BigEndian>()?)),
            Tag::Double => Ok(Value::Double(name, self.reader.read_f64::<BigEndian>()?)),
            Tag::Compound => {
                self.layers.push(Layer::Compound);
                Ok(Value::Compound(name))
            }
            Tag::End => panic!("end tag should have returned early"),
            Tag::List => {
                let element_tag = self.reader.read_u8()?;
                let element_tag = u8_to_tag(element_tag)?;
                let size = self.reader.read_i32::<BigEndian>()?;
                self.layers.push(Layer::List(element_tag.clone(), size));
                Ok(Value::List(name, element_tag, size))
            }
            Tag::String => Ok(Value::String(name, self.read_size_prefixed_string()?)),
            Tag::ByteArray => {
                let size = self.reader.read_i32::<BigEndian>()?;
                let mut buf = vec![0u8; size as usize];
                self.reader.read_exact(&mut buf[..])?;
                Ok(Value::ByteArray(name, vec_u8_into_i8(buf)))
            }
            Tag::IntArray => {
                let size = self.reader.read_i32::<BigEndian>()?;
                let mut buf = vec![0i32; size as usize];
                for i in 0..size {
                    buf[i as usize] = self.reader.read_i32::<BigEndian>()?;
                }

                Ok(Value::IntArray(name, buf))
            }
            Tag::LongArray => {
                let size = self.reader.read_i32::<BigEndian>()?;
                let mut buf = vec![0i64; size as usize];
                for i in 0..size {
                    buf[i as usize] = self.reader.read_i64::<BigEndian>()?;
                }

                Ok(Value::LongArray(name, buf))
            }
        }
    }
}

pub fn skip_compound<R: Read>(parser: &mut Parser<R>) -> Result<()> {
    let mut depth = 1;

    while depth != 0 {
        let value = parser.next()?;
        match value {
            Value::CompoundEnd => depth -= 1,
            Value::Compound(_) => depth += 1,
            _ => {}
        }
    }
    Ok(())
}

pub fn find_compound<R: Read>(parser: &mut Parser<R>, name: Option<&str>) -> Result<()> {
    loop {
        match parser.next()? {
            //Value::Compound(n) if n == name.map(|s| s.to_owned()) => break,
            Value::Compound(n) if n.as_deref() == name => break,
            _ => {}
        }
    }
    Ok(())
}

pub fn find_list<R: Read>(parser: &mut Parser<R>, name: Option<&str>) -> Result<usize> {
    loop {
        match parser.next()? {
            Value::List(n, _, size) if n.as_deref() == name => return Ok(size as usize),
            _ => {}
        }
    }
}

// Thanks to https://stackoverflow.com/a/59707887
fn vec_u8_into_i8(v: Vec<u8>) -> Vec<i8> {
    // ideally we'd use Vec::into_raw_parts, but it's unstable,
    // so we have to do it manually:

    // first, make sure v's destructor doesn't free the data
    // it thinks it owns when it goes out of scope
    let mut v = std::mem::ManuallyDrop::new(v);

    // then, pick apart the existing Vec
    let p = v.as_mut_ptr();
    let len = v.len();
    let cap = v.capacity();

    // finally, adopt the data into a new Vec
    unsafe { Vec::from_raw_parts(p as *mut i8, len, cap) }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Error {
        Error::IO(err)
    }
}

fn u8_to_tag(tag: u8) -> Result<Tag> {
    Tag::try_from(tag).or_else(|_| Err(Error::InvalidTag(tag)))
}

#[derive(Clone)]
enum Layer {
    List(Tag, i32),
    Compound,
}
