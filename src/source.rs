//! Sources of immutable content. Object states do not require native snapshots.
use crate::fsutil::Src;
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone, Debug)]
pub enum Source {
    Folder(PathBuf),
    Objects(Arc<crate::object_tree::ObjectTree>),
}
impl Source {
    pub fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Folder(a), Self::Folder(b)) => a == b,
            (Self::Objects(a), Self::Objects(b)) => {
                a.store.root() == b.store.root() && a.refs.tree == b.refs.tree && a.refs.times == b.refs.times
            }
            _ => false,
        }
    }
    pub fn path(&self, rel: &str) -> Result<PathBuf> {
        self.at(rel)
    }
}
impl From<PathBuf> for Source {
    fn from(p: PathBuf) -> Self {
        Self::Folder(p)
    }
}
impl Src for Source {
    fn at(&self, rel: &str) -> Result<PathBuf> {
        match self {
            Self::Folder(p) => p.at(rel),
            Self::Objects(o) => o.path(rel),
        }
    }
    fn folder(&self) -> Option<&Path> {
        match self {
            Self::Folder(p) => Some(p),
            Self::Objects(_) => None,
        }
    }
    fn objects(&self) -> Option<&crate::object_tree::ObjectTree> {
        match self {
            Self::Objects(o) => Some(o),
            _ => None,
        }
    }
}
