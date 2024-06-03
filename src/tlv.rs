use bytes::Buf;
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
            let value = b.copy_to_bytes(len).to_vec();
            entries.push(TlvEntry { typ, value });
        }

        Ok(SerializedTlvStream { entries })
    }
}

pub type CompactSize = u64;

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
}

impl ProtoBuf for bytes::Bytes {}
impl ProtoBuf for &[u8] {}
impl ProtoBuf for bytes::buf::Take<bytes::Bytes> {}

impl<'de> Deserialize<'de> for SerializedTlvStream {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Start by reading the hex-encoded string
        let s: String = Deserialize::deserialize(deserializer)?;
        let mut b: bytes::Bytes = hex::decode(s)
            .map_err(|e| serde::de::Error::custom(e.to_string()))?
            .into();

        if b.is_empty() {
            return Ok(SerializedTlvStream {
                entries: Vec::new(),
            });
        }
        // Skip the length prefix
        let l = b.get_compact_size();
        let b = b.take(l as usize); // Protect against overruns

        Self::from_bytes(b.into_inner()).map_err(|e| serde::de::Error::custom(e.to_string()))
    }
}

impl From<Vec<TlvEntry>> for SerializedTlvStream {
    fn from(value: Vec<TlvEntry>) -> Self {
        SerializedTlvStream { entries: value }
    }
}
