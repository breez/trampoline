use anyhow::anyhow;
use bytes::{Buf, BufMut};
use serde::{Deserialize, Deserializer};

#[derive(Clone, Debug, PartialEq)]
pub struct TlvEntry {
    pub typ: u64,
    pub value: Vec<u8>,
}

/// A standalone type the represent a binary serialized
/// TlvStream. This is distinct from TlvStream since that expects TLV
/// streams to be encoded as maps in JSON.
#[derive(Clone, Debug, PartialEq)]
pub struct SerializedTlvStream {
    entries: Vec<TlvEntry>,
}

impl SerializedTlvStream {
    pub fn get(&self, typ: u64) -> Option<TlvEntry> {
        self.entries.iter().find(|e| e.typ == typ).cloned()
    }

    pub fn remove(&mut self, typ: u64) {
        if let Some(position) = self.entries.iter().position(|e| e.typ == typ) {
            self.entries.remove(position);
        }
    }

    pub fn insert(&mut self, e: TlvEntry) -> Result<(), anyhow::Error> {
        if let Some(old) = self.get(e.typ) {
            return Err(anyhow::anyhow!(
                "TlvStream contains entry of type={}, old={:?}, new={:?}",
                e.typ,
                old,
                e
            ));
        }

        self.entries.push(e);
        self.entries
            .sort_by(|a, b| a.typ.partial_cmp(&b.typ).unwrap());

        Ok(())
    }
}

pub trait FromBytes: Sized {
    type Error;
    fn from_bytes<T>(s: T) -> Result<Self, Self::Error>
    where
        T: AsRef<[u8]> + 'static;
}

impl FromBytes for SerializedTlvStream {
    type Error = anyhow::Error;
    fn from_bytes<T>(s: T) -> Result<Self, Self::Error>
    where
        T: AsRef<[u8]> + 'static,
    {
        let mut b = s.as_ref();
        //let mut b: bytes::Bytes = r.into();
        let mut entries: Vec<TlvEntry> = vec![];
        while b.remaining() >= 2 {
            let typ = b.get_compact_size();
            let len = b.get_compact_size() as usize;
            if b.remaining() < len {
                return Err(anyhow!(
                    "trying to advance {}, but remaining length is {}",
                    len,
                    b.remaining()
                ));
            }
            let value = b.copy_to_bytes(len).to_vec();
            entries.push(TlvEntry { typ, value });
        }

        Ok(SerializedTlvStream { entries })
    }
}

pub type CompactSize = u64;

/// A variant of CompactSize that works on length-delimited
/// buffers and therefore does not require a length prefix
pub type TU64 = u64;

/// Extensions on top of `Buf` to include LN proto primitives
pub trait ProtoBuf: Buf {
    fn get_compact_size(&mut self) -> CompactSize {
        match self.get_u8() {
            253 => self.get_u16().into(),
            254 => self.get_u32().into(),
            255 => self.get_u64(),
            v => v.into(),
        }
    }

    fn get_tu64(&mut self) -> Result<TU64, anyhow::Error> {
        Ok(match self.remaining() {
            1 => self.get_u8() as u64,
            2 => self.get_u16() as u64,
            4 => self.get_u32() as u64,
            8 => self.get_u64(),
            l => return Err(anyhow::anyhow!("Unexpect TU64 length: {}", l)),
        })
    }
}

impl ProtoBuf for bytes::Bytes {}
impl ProtoBuf for &[u8] {}
impl ProtoBuf for bytes::buf::Take<bytes::Bytes> {}

pub trait ProtoBufMut: bytes::BufMut {
    fn put_compact_size(&mut self, cs: CompactSize) {
        match cs {
            0..=0xFC => self.put_u8(cs as u8),
            0xFD..=0xFFFF => {
                self.put_u8(253);
                self.put_u16(cs as u16);
            }
            0x10000..=0xFFFFFFFF => {
                self.put_u8(254);
                self.put_u32(cs as u32);
            }
            v => {
                self.put_u8(255);
                self.put_u64(v);
            }
        }
    }
}

impl ProtoBufMut for bytes::BytesMut {}

pub trait ToBytes: Sized {
    fn to_bytes(s: Self) -> Vec<u8>;
}

impl ToBytes for SerializedTlvStream {
    fn to_bytes(s: Self) -> Vec<u8> {
        let mut b = bytes::BytesMut::new();

        for e in s.entries.iter() {
            b.put_compact_size(e.typ);
            b.put_compact_size(e.value.len() as u64);
            b.put(&e.value[..]);
        }
        b.to_vec()
    }
}

impl<'de> Deserialize<'de> for SerializedTlvStream {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Start by reading the hex-encoded string
        let s: String = Deserialize::deserialize(deserializer)?;
        let b = hex::decode(s).map_err(|e| serde::de::Error::custom(e.to_string()))?;

        SerializedTlvStream::try_from(b).map_err(|e| serde::de::Error::custom(e.to_string()))
    }
}

impl From<Vec<TlvEntry>> for SerializedTlvStream {
    fn from(value: Vec<TlvEntry>) -> Self {
        SerializedTlvStream { entries: value }
    }
}

impl TryFrom<Vec<u8>> for SerializedTlvStream {
    type Error = anyhow::Error;

    fn try_from(value: Vec<u8>) -> Result<Self, Self::Error> {
        let mut b: bytes::Bytes = value.into();

        if b.is_empty() {
            return Ok(SerializedTlvStream {
                entries: Vec::new(),
            });
        }
        // Skip the length prefix
        let l = b.get_compact_size();
        let b = b.take(l as usize); // Protect against overruns

        Self::from_bytes(b.into_inner())
    }
}
