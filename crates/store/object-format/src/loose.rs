//! Loose object headers. Compression and dictionary loading belong to the store.
use crate::{Error, Kind, Result};

pub const MAGIC: &[u8; 4] = b"LOB1";
pub const WHOLE: u8 = b'z';
pub const WHOLE_DICT: u8 = b'y';
pub const DELTA: u8 = b'd';
pub const CHUNKED: u8 = b'c';

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Form {
    Whole,
    Dictionary,
    Delta,
    Chunked,
}
impl Form {
    pub fn from_byte(byte: u8) -> Result<Self> {
        match byte {
            WHOLE => Ok(Self::Whole),
            WHOLE_DICT => Ok(Self::Dictionary),
            DELTA => Ok(Self::Delta),
            CHUNKED => Ok(Self::Chunked),
            _ => Err(Error::LooseForm(byte)),
        }
    }
    pub fn byte(self) -> u8 {
        match self {
            Self::Whole => WHOLE,
            Self::Dictionary => WHOLE_DICT,
            Self::Delta => DELTA,
            Self::Chunked => CHUNKED,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub kind: Kind,
    pub form: Form,
}
impl Header {
    pub fn encode(self) -> [u8; 6] {
        [b'L', b'O', b'B', b'1', self.kind.byte(), self.form.byte()]
    }
    /// Read the fixed prefix; the remaining bytes are the stored payload.
    pub fn decode(data: &[u8]) -> Result<Self> {
        ensure!(data.len() >= 6 && &data[..4] == MAGIC, Error::LooseHeader);
        Ok(Self { kind: Kind::from_byte(data[4])?, form: Form::from_byte(data[5])? })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn headers_are_fixed_and_reject_unknown_tags() -> Result<()> {
        for kind in [Kind::Blob, Kind::Tree, Kind::Chunk, Kind::Opaque, Kind::Times] {
            for form in [Form::Whole, Form::Dictionary, Form::Delta, Form::Chunked] {
                let header = Header { kind, form };
                assert_eq!(Header::decode(&header.encode())?, header);
                for cut in 0..6 {
                    assert!(Header::decode(&header.encode()[..cut]).is_err());
                }
            }
        }
        assert_eq!(Header { kind: Kind::Blob, form: Form::Whole }.encode(), *b"LOB1bz");
        for bytes in [b"LOB2bz", b"LOB1xz", b"LOB1bx"] {
            assert!(Header::decode(bytes).is_err());
        }
        Ok(())
    }
}
