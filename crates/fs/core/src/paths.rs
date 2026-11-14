use crate::{FsError, Result};

/// Byte-preserving relative entry path, without empty, dot, parent or NUL components.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RelPath(Vec<u8>);
impl RelPath {
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self> {
        let bytes = bytes.into();
        if bytes.contains(&0) || bytes.split(|b| *b == b'/').any(|c| c.is_empty() || c == b"." || c == b"..") {
            return Err(FsError::Invalid("invalid relative path"));
        }
        Ok(Self(bytes))
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
    pub fn join(&self, child: &RelPath) -> Self {
        Self([self.as_bytes(), b"/", child.as_bytes()].concat())
    }
    pub fn is_name(&self) -> bool {
        !self.0.contains(&b'/')
    }
    pub fn utf8(&self) -> Result<&str> {
        std::str::from_utf8(&self.0).map_err(|_| FsError::Invalid("legacy operation requires a UTF-8 path"))
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelDir {
    Root,
    Path(RelPath),
}
impl RelDir {
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() {
            Ok(Self::Root)
        } else {
            Ok(Self::Path(RelPath::new(bytes)?))
        }
    }
    pub fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Root => b"",
            Self::Path(path) => path.as_bytes(),
        }
    }
    pub fn join(&self, child: &RelPath) -> RelPath {
        match self {
            Self::Root => child.clone(),
            Self::Path(path) => path.join(child),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paths_preserve_bytes_and_separate_root() {
        for bad in [b"".as_slice(), b"/a", b"a/", b"a//b", b"a/../b", b"./b", b"a\0b"] {
            assert!(RelPath::new(bad).is_err());
        }
        let path = RelPath::new(vec![b'a', b'/', 255]).unwrap();
        assert_eq!(path.as_bytes(), &[b'a', b'/', 255]);
        assert!(path.utf8().is_err());
        assert_eq!(RelDir::from_bytes(b"").unwrap(), RelDir::Root);
    }
}
