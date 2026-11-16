//! Pure btrfs send framing. Metadata observations never grant receive authority.
#![forbid(unsafe_code)]

use std::fmt;

mod metadata;
pub use metadata::{Kind, MetadataCommand, RangeEnd};

pub const MAGIC: &[u8; 13] = b"btrfs-stream\0";
pub const STREAM_HEADER_LEN: usize = 17;
pub const COMMAND_HEADER_LEN: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    Truncated,
    Magic,
    Version,
    Limit,
    AttributeSize(u16),
    DuplicateAttribute(u16),
    MissingAttribute(u16),
    UnknownCommand(u16),
    Path,
    Overflow,
    Checksum,
    Sequence,
}
impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid send stream: {self:?}")
    }
}
impl std::error::Error for DecodeError {}
type Result<T> = std::result::Result<T, DecodeError>;

/// Bounds apply before allocating a command body or collecting its attributes.
#[derive(Debug, Clone, Copy)]
pub struct DecodeLimits {
    pub max_command_bytes: usize,
    pub max_attributes: usize,
    pub max_path_bytes: usize,
}
impl Default for DecodeLimits {
    fn default() -> Self {
        Self { max_command_bytes: 16 << 20, max_attributes: 64, max_path_bytes: 4096 }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Version(u32);
impl Version {
    pub fn decode(header: &[u8]) -> Result<Self> {
        if header.len() != STREAM_HEADER_LEN {
            return Err(DecodeError::Truncated);
        }
        if &header[..13] != MAGIC {
            return Err(DecodeError::Magic);
        }
        let version = u32::from_le_bytes(header[13..].try_into().map_err(|_| DecodeError::Truncated)?);
        if !(1..=3).contains(&version) {
            return Err(DecodeError::Version);
        }
        Ok(Self(version))
    }
    #[must_use]
    pub fn number(self) -> u32 {
        self.0
    }
}

/// A bounded length token; adapters must parse it before resizing their IO buffer.
#[derive(Debug, Clone, Copy)]
pub struct CommandHeader {
    len: usize,
    code: u16,
    checksum: u32,
}
impl CommandHeader {
    pub fn decode(header: &[u8], limits: DecodeLimits) -> Result<Self> {
        if header.len() != COMMAND_HEADER_LEN {
            return Err(DecodeError::Truncated);
        }
        let len = u32::from_le_bytes(header[..4].try_into().map_err(|_| DecodeError::Truncated)?);
        let len = usize::try_from(len).map_err(|_| DecodeError::Limit)?;
        if len > limits.max_command_bytes {
            return Err(DecodeError::Limit);
        }
        Ok(Self {
            len,
            code: u16::from_le_bytes([header[4], header[5]]),
            checksum: u32::from_le_bytes(header[6..].try_into().map_err(|_| DecodeError::Truncated)?),
        })
    }
    #[must_use]
    pub fn body_len(self) -> usize {
        self.len
    }
}

struct Attributes<'a>(Vec<(u16, &'a [u8])>);
impl<'a> Attributes<'a> {
    fn decode(body: &'a [u8], version: Version, limits: DecodeLimits) -> Result<Self> {
        if body.len() > limits.max_command_bytes {
            return Err(DecodeError::Limit);
        }
        let mut attrs = Self(Vec::new());
        let mut rest = body;
        while !rest.is_empty() {
            if attrs.0.len() >= limits.max_attributes {
                return Err(DecodeError::Limit);
            }
            let tag = rest.get(..2).ok_or(DecodeError::Truncated)?;
            let tag = u16::from_le_bytes([tag[0], tag[1]]);
            if attrs.get(tag).is_some() {
                return Err(DecodeError::DuplicateAttribute(tag));
            }
            let (value, tail) = if tag == 19 && version.0 >= 2 {
                (&rest[2..], &[][..])
            } else {
                let len = rest.get(2..4).ok_or(DecodeError::Truncated)?;
                let len = usize::from(u16::from_le_bytes([len[0], len[1]]));
                (rest.get(4..4 + len).ok_or(DecodeError::Truncated)?, &rest[4 + len..])
            };
            let size = match tag {
                1 | 20 => Some(16),
                2..=8 | 18 | 21 | 23 | 24 | 26..=29 => Some(8),
                9..=12 => Some(12),
                25 | 30 | 31 | 33 => Some(4),
                32 => Some(1),
                _ => None,
            };
            if size.is_some_and(|size| value.len() != size) {
                return Err(DecodeError::AttributeSize(tag));
            }
            if matches!(tag, 15..=17 | 22) && value.len() > limits.max_path_bytes {
                return Err(DecodeError::Limit);
            }
            attrs.0.push((tag, value));
            rest = tail;
        }
        Ok(attrs)
    }
    fn get(&self, tag: u16) -> Option<&'a [u8]> {
        self.0.iter().find(|(k, _)| *k == tag).map(|(_, v)| *v)
    }
    fn require(&self, tag: u16) -> Result<&'a [u8]> {
        self.get(tag).ok_or(DecodeError::MissingAttribute(tag))
    }
    fn number(&self, tag: u16) -> Result<u64> {
        Ok(u64::from_le_bytes(self.require(tag)?.try_into().map_err(|_| DecodeError::AttributeSize(tag))?))
    }
}

fn check_body(header: CommandHeader, body: &[u8]) -> Result<()> {
    if header.len != body.len() {
        return Err(DecodeError::Truncated);
    }
    Ok(())
}

fn check_path(path: &[u8]) -> Result<()> {
    if path.contains(&0) || (!path.is_empty() && path.split(|b| *b == b'/').any(|c| c.is_empty() || c == b"." || c == b"..")) {
        return Err(DecodeError::Path);
    }
    Ok(())
}

/// A command with checked CRC, attributes, paths and receive ordering. Project identity and
/// filesystem source authorization remain the receiving adapter's responsibility.
/// There is deliberately no conversion from `MetadataCommand` to this type.
/// ```compile_fail
/// use layr_stream_codec::{MetadataCommand, VerifiedReceiveCommand};
/// fn receive(_: VerifiedReceiveCommand<'_>) {}
/// receive(MetadataCommand::Other);
/// ```
pub struct VerifiedReceiveCommand<'a> {
    code: u16,
    attrs: Attributes<'a>,
}
impl<'a> VerifiedReceiveCommand<'a> {
    #[must_use]
    pub fn code(&self) -> u16 {
        self.code
    }
    pub fn value(&self, tag: u16) -> Result<&'a [u8]> {
        self.attrs.require(tag)
    }
    pub fn number(&self, tag: u16) -> Result<u64> {
        self.attrs.number(tag)
    }
}

#[derive(Clone, Copy)]
enum ReceiveState {
    Start,
    Commands,
    End,
}
/// Stateful strict decoder for a single untrusted subvolume stream.
pub struct ReceiveDecoder {
    version: Version,
    limits: DecodeLimits,
    state: ReceiveState,
}
impl ReceiveDecoder {
    #[must_use]
    pub fn new(version: Version, limits: DecodeLimits) -> Self {
        Self { version, limits, state: ReceiveState::Start }
    }
    pub fn decode<'a>(&mut self, header: CommandHeader, body: &'a [u8]) -> Result<VerifiedReceiveCommand<'a>> {
        check_body(header, body)?;
        if header.len > self.limits.max_command_bytes {
            return Err(DecodeError::Limit);
        }
        let code = header.code;
        if !(1..=26).contains(&code) {
            return Err(DecodeError::UnknownCommand(code));
        }
        if (self.version.0 == 1 && code >= 23) || (self.version.0 < 3 && code == 26) {
            return Err(DecodeError::Version);
        }
        let mut bytes = [0; 10];
        bytes[..4].copy_from_slice(&u32::try_from(header.len).map_err(|_| DecodeError::Limit)?.to_le_bytes());
        bytes[4..6].copy_from_slice(&code.to_le_bytes());
        if crc_parts(&bytes, body) != header.checksum {
            return Err(DecodeError::Checksum);
        }
        let attrs = Attributes::decode(body, self.version, self.limits)?;
        for tag in [15, 16, 22] {
            if let Some(path) = attrs.get(tag) {
                check_path(path)?;
            }
        }
        let required: &[u16] = match code {
            1 => &[15, 1, 2],
            2 => &[15, 1, 2, 20, 21],
            3..=7 => &[15, 3],
            8 => &[15, 3, 17],
            9 => &[15, 16],
            10 => &[15, 17],
            11 | 12 => &[15],
            13 => &[15, 13, 14],
            14 => &[15, 13],
            15 => &[15, 18, 19],
            16 => &[15, 18, 20, 21, 22, 23, 24],
            17 => &[15, 4],
            18 => &[15, 5],
            19 => &[15, 6, 7],
            20 => &[15, 9, 10, 11],
            21 => &[],
            22 => &[15, 18, 4],
            23 => &[15, 25, 18, 4],
            24 => &[15, 26],
            // Compression and encryption are optional, with protocol-defined defaults.
            25 => &[15, 18, 19, 27, 28, 29],
            26 => &[15, 32, 33, 34, 35],
            _ => return Err(DecodeError::UnknownCommand(code)),
        };
        for &tag in required {
            attrs.require(tag)?;
        }
        if code == 5 {
            attrs.require(5)?;
            attrs.require(8)?;
        }
        if code == 10 {
            check_path(attrs.require(17)?)?;
        }
        if code == 8 && attrs.require(17)?.contains(&0) {
            return Err(DecodeError::Path);
        }
        if code == 21 && !body.is_empty() {
            return Err(DecodeError::Sequence);
        }
        for tag in 9..=12 {
            if let Some(time) = attrs.get(tag) {
                let ns = u32::from_le_bytes(time[8..].try_into().map_err(|_| DecodeError::AttributeSize(tag))?);
                if ns >= 1_000_000_000 {
                    return Err(DecodeError::AttributeSize(tag));
                }
            }
        }
        let next = match (self.state, code) {
            (ReceiveState::Start, 1 | 2) => ReceiveState::Commands,
            (ReceiveState::Commands, 21) => ReceiveState::End,
            (ReceiveState::Commands, 3..=26) => ReceiveState::Commands,
            _ => return Err(DecodeError::Sequence),
        };
        self.state = next;
        Ok(VerifiedReceiveCommand { code, attrs })
    }
    /// Must be called at EOF; rejects streams that did not reach END.
    pub fn finish(self) -> Result<()> {
        if matches!(self.state, ReceiveState::End) {
            Ok(())
        } else {
            Err(DecodeError::Sequence)
        }
    }
}

// btrfs CRC32C uses a zero seed and no final inversion.
const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    let mut seed = 0u32;
    while i < 256 {
        let mut c = seed;
        let mut bit = 0;
        while bit < 8 {
            c = (c >> 1) ^ (0x82f63b78u32 & 0u32.wrapping_sub(c & 1));
            bit += 1;
        }
        table[i] = c;
        i += 1;
        seed += 1;
    }
    table
};
fn crc_parts(head: &[u8], body: &[u8]) -> u32 {
    head.iter().chain(body).fold(0u32, |c, b| (c >> 8) ^ CRC_TABLE[usize::from(c.to_le_bytes()[0] ^ b)])
}

/// Encode one v1 command; callers own IO. Rejects lengths before allocating.
pub fn encode_command(code: u16, attrs: &[(u16, &[u8])], limits: DecodeLimits) -> Result<Vec<u8>> {
    if attrs.len() > limits.max_attributes {
        return Err(DecodeError::Limit);
    }
    let mut len = 0usize;
    for &(_, value) in attrs {
        u16::try_from(value.len()).map_err(|_| DecodeError::Limit)?;
        len = len.checked_add(4).and_then(|n| n.checked_add(value.len())).ok_or(DecodeError::Limit)?;
    }
    if len > limits.max_command_bytes {
        return Err(DecodeError::Limit);
    }
    let wire_len = u32::try_from(len).map_err(|_| DecodeError::Limit)?;
    let mut out = vec![0; 10];
    out.reserve(len);
    out[..4].copy_from_slice(&wire_len.to_le_bytes());
    out[4..6].copy_from_slice(&code.to_le_bytes());
    for &(tag, value) in attrs {
        out.extend(tag.to_le_bytes());
        out.extend(u16::try_from(value.len()).map_err(|_| DecodeError::Limit)?.to_le_bytes());
        out.extend(value);
    }
    let crc = crc_parts(&out, &[]);
    out[6..10].copy_from_slice(&crc.to_le_bytes());
    Ok(out)
}

#[cfg(test)]
mod tests;
