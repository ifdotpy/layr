//! A portable layered namespace. Removal records mask lower layers, never current entries.
use layr_fs_core::{evidence::FType, *};
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct ReadLayer {
    pub reader: Arc<dyn TreeRead>,
    pub removed: BTreeSet<RelPath>,
}
#[derive(Clone)]
pub struct LayeredTree {
    layers: Vec<Layer>,
    domain: LinkDomain,
    links: Arc<Mutex<HashMap<(LinkDomain, u64, u64), u64>>>,
}
#[derive(Clone)]
struct Layer {
    reader: Arc<dyn TreeRead>,
    removed: BTreeSet<RelPath>,
    domain: LinkDomain,
}
impl LayeredTree {
    pub fn new(layers: Vec<ReadLayer>) -> Result<Self> {
        if layers.is_empty() {
            return Err(FsError::Invalid("layered reader needs a base"));
        }
        let layers = layers
            .into_iter()
            .map(|layer| {
                let domain = layer.reader.link_domain().cloned().unwrap_or_default();
                Layer { reader: layer.reader, removed: layer.removed, domain }
            })
            .collect();
        Ok(Self { layers, domain: LinkDomain::default(), links: Arc::new(Mutex::new(HashMap::new())) })
    }
    fn ancestor_blocks(layer: &Layer, path: &RelDir) -> Result<bool> {
        for (index, _) in path.as_bytes().iter().enumerate().filter(|(_, byte)| **byte == b'/') {
            let parent = RelDir::from_bytes(&path.as_bytes()[..index])?;
            if layer.reader.kind(&parent)?.is_some_and(|kind| kind != FType::Dir) {
                return Ok(true);
            }
        }
        Ok(false)
    }
    fn blocked(layer: &Layer, path: &RelDir) -> bool {
        let bytes = path.as_bytes();
        layer.removed.iter().any(|removed| {
            bytes == removed.as_bytes() || bytes.strip_prefix(removed.as_bytes()).is_some_and(|suffix| suffix.starts_with(b"/"))
        })
    }
    fn at(&self, path: &RelDir) -> Result<Option<(&Layer, FType)>> {
        for layer in &self.layers {
            if Self::ancestor_blocks(layer, path)? {
                return Ok(None);
            }
            if let Some(kind) = layer.reader.kind(path)? {
                return Ok(Some((layer, kind)));
            }
            if Self::blocked(layer, path) {
                return Ok(None);
            }
        }
        Ok(None)
    }
}
impl TreeRead for LayeredTree {
    fn link_domain(&self) -> Option<&LinkDomain> {
        Some(&self.domain)
    }
    fn kind(&self, path: &RelDir) -> Result<Option<FType>> {
        Ok(self.at(path)?.map(|(_, kind)| kind))
    }
    fn metadata(&self, path: &RelDir) -> Result<Option<EntryMetadata>> {
        let Some((layer, _)) = self.at(path)? else { return Ok(None) };
        let mut metadata =
            layer.reader.metadata(path)?.ok_or(FsError::Invalid("layered entry disappeared during observation"))?;
        if let Some(link) = &mut metadata.link {
            let mut links = self.links.lock().map_err(|_| FsError::Invalid("layered link registry lock poisoned"))?;
            let next = u64::try_from(links.len())
                .ok()
                .and_then(|count| count.checked_add(1))
                .ok_or(FsError::Invalid("too many layered link identities"))?;
            link.inode = *links.entry((layer.domain.clone(), link.device, link.inode)).or_insert(next);
            link.device = 0;
        }
        Ok(Some(metadata))
    }
    fn entries(&self, dir: &RelDir, visit: &mut dyn FnMut(&RelPath) -> Result<()>) -> Result<()> {
        if self.kind(dir)? != Some(FType::Dir) {
            return Err(FsError::Invalid("layered entry is not a directory"));
        }
        if self.layers.len() == 1 && self.layers[0].removed.is_empty() {
            return self.layers[0].reader.entries(dir, &mut |name| {
                if !name.is_name() {
                    return Err(FsError::Invalid("reader returned compound child name"));
                }
                visit(name)
            });
        }
        let mut names = BTreeSet::new();
        for layer in &self.layers {
            if Self::ancestor_blocks(layer, dir)? {
                break;
            }
            if let Some(kind) = layer.reader.kind(dir)? {
                if kind != FType::Dir {
                    break;
                }
                layer.reader.entries(dir, &mut |name| {
                    if !name.is_name() {
                        return Err(FsError::Invalid("reader returned compound child name"));
                    }
                    names.insert(name.clone());
                    Ok(())
                })?;
            }
            if Self::blocked(layer, dir) {
                break;
            }
        }
        for name in names {
            if self.kind(&RelDir::Path(dir.join(&name)))?.is_some() {
                visit(&name)?
            }
        }
        Ok(())
    }
    fn open_file(&self, path: &RelPath) -> Result<Box<dyn ReadSeek + '_>> {
        let (reader, kind) = self.at(&RelDir::Path(path.clone()))?.ok_or(FsError::Invalid("layered file absent"))?;
        if kind != FType::File {
            return Err(FsError::Invalid("layered entry is not a file"));
        }
        reader.reader.open_file(path)
    }
    fn read_link(&self, path: &RelPath) -> Result<Vec<u8>> {
        let (reader, kind) = self.at(&RelDir::Path(path.clone()))?.ok_or(FsError::Invalid("layered link absent"))?;
        if kind != FType::Symlink {
            return Err(FsError::Invalid("layered entry is not a link"));
        }
        reader.reader.read_link(path)
    }
}
