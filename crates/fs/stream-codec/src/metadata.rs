//! Commands observed in a trusted local metadata stream. Unknown commands are ignored.
use crate::{check_body, Attributes, CommandHeader, DecodeError, DecodeLimits, Result, Version};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RangeEnd {
    Offset(u64),
    Eof,
}

mod cmd {
    pub const MKFILE: u16 = 3;
    pub const MKDIR: u16 = 4;
    pub const MKNOD: u16 = 5;
    pub const MKFIFO: u16 = 6;
    pub const MKSOCK: u16 = 7;
    pub const SYMLINK: u16 = 8;
    pub const RENAME: u16 = 9;
    pub const LINK: u16 = 10;
    pub const UNLINK: u16 = 11;
    pub const RMDIR: u16 = 12;
    pub const SET_XATTR: u16 = 13;
    pub const REMOVE_XATTR: u16 = 14;
    pub const WRITE: u16 = 15;
    pub const CLONE: u16 = 16;
    pub const TRUNCATE: u16 = 17;
    pub const CHMOD: u16 = 18;
    pub const UTIMES: u16 = 20;
    pub const UPDATE_EXTENT: u16 = 22;
    pub const FALLOCATE: u16 = 23;
    pub const ENCODED_WRITE: u16 = 25;
}

#[derive(Debug, Clone, PartialEq)]
pub enum MetadataCommand {
    Create { path: String, kind: Kind },
    Symlink { path: String, target: String },
    Rename { from: String, to: String },
    Link { path: String, target: String },
    Unlink { path: String },
    Rmdir { path: String },
    Extent { path: String, offset: u64, end: RangeEnd },
    Truncate { path: String, size: u64 },
    Chmod { path: String, mode: u32 },
    Metadata { path: String },
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Special,
}

impl MetadataCommand {
    /// Does not check receive ordering, paths or CRC and must never authorize receive.
    pub fn decode(header: CommandHeader, body: &[u8], version: Version, limits: DecodeLimits) -> Result<Self> {
        check_body(header, body)?;
        let at = Attributes::decode(body, version, limits)?;
        let path = |tag| -> Result<String> { Ok(String::from_utf8_lossy(at.require(tag)?).into_owned()) };
        let c = match header.code {
            cmd::MKFILE => MetadataCommand::Create { path: path(15)?, kind: Kind::File },
            cmd::MKDIR => MetadataCommand::Create { path: path(15)?, kind: Kind::Dir },
            cmd::MKNOD | cmd::MKFIFO | cmd::MKSOCK => MetadataCommand::Create { path: path(15)?, kind: Kind::Special },
            cmd::SYMLINK => MetadataCommand::Symlink { path: path(15)?, target: path(17)? },
            cmd::RENAME => MetadataCommand::Rename { from: path(15)?, to: path(16)? },
            cmd::LINK => MetadataCommand::Link { path: path(15)?, target: path(17)? },
            cmd::UNLINK => MetadataCommand::Unlink { path: path(15)? },
            cmd::RMDIR => MetadataCommand::Rmdir { path: path(15)? },
            cmd::UPDATE_EXTENT => MetadataCommand::Extent {
                path: path(15)?,
                offset: at.number(18)?,
                end: RangeEnd::Offset(at.number(18)?.checked_add(at.number(4)?).ok_or(DecodeError::Overflow)?),
            },
            cmd::CLONE => MetadataCommand::Extent {
                path: path(15)?,
                offset: at.number(18)?,
                end: RangeEnd::Offset(at.number(18)?.checked_add(at.number(24)?).ok_or(DecodeError::Overflow)?),
            },
            cmd::WRITE | cmd::ENCODED_WRITE | cmd::FALLOCATE => {
                MetadataCommand::Extent { path: path(15)?, offset: at.number(18)?, end: RangeEnd::Eof }
            }
            cmd::TRUNCATE => MetadataCommand::Truncate { path: path(15)?, size: at.number(4)? },
            cmd::CHMOD => {
                MetadataCommand::Chmod { path: path(15)?, mode: u32::try_from(at.number(5)?).map_err(|_| DecodeError::Overflow)? }
            }
            cmd::SET_XATTR | cmd::REMOVE_XATTR | cmd::UTIMES => MetadataCommand::Metadata { path: path(15)? },
            _ => MetadataCommand::Other,
        };
        Ok(c)
    }
}
