//! Bounded framing and project-source validation for untrusted receive streams.
//! Pure validation lives in `layr-stream-codec`; project and filesystem checks stay here.

use anyhow::{anyhow, bail, Result};
use layr_stream_codec::{CommandHeader, DecodeLimits, ReceiveDecoder, Version};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

#[cfg(test)]
const MAGIC: &[u8] = layr_stream_codec::MAGIC;

const SUBVOL: u16 = 1;
const SNAPSHOT: u16 = 2;
const CLONE: u16 = 16;
const END: u16 = 21;

const A_UUID: u16 = 1;
const A_CTRANSID: u16 = 2;
const A_PATH: u16 = 15;
const A_CLONE_UUID: u16 = 20;
const A_CLONE_CTRANSID: u16 = 21;
const A_CLONE_PATH: u16 = 22;

/// What a stream must be.
pub struct Expect<'a> {
    /// The name of the new subvolume.
    pub name: &'a str,
    /// The UUID the signed record names for it (the received UUID after the receive).
    pub uuid: &'a str,
    /// The local subvolume of a state of this project, by the UUID its signed record names.
    pub parent: &'a dyn Fn(&str) -> Option<PathBuf>,
    /// Extra immutable sources verified against signed states of this project.
    /// A local original UUID has no transaction constraint; received UUIDs require the send transaction.
    pub clones: &'a [(String, Option<u64>, PathBuf)],
}

/// Extra clone sources are immutable and remain under the project lock during receive.
/// Check each component because btrfs receive follows links when it opens a clone source.
fn check_clone_file(root: &Path, path: &[u8]) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let mut full = root.to_path_buf();
    for component in path.split(|b| *b == b'/') {
        full.push(std::ffi::OsStr::from_bytes(component));
        if std::fs::symlink_metadata(&full)?.file_type().is_symlink() {
            bail!("clone source path contains a symbolic link");
        }
    }
    if !std::fs::symlink_metadata(full)?.is_file() {
        bail!("clone source is not a regular file");
    }
    Ok(())
}

fn uuid_text(b: &[u8]) -> Result<String> {
    let a: [u8; 16] = b.try_into().map_err(|_| anyhow!("bad UUID in the stream"))?;
    Ok(uuid::Uuid::from_bytes(a).hyphenated().to_string())
}

/// Check a stream while copying it to `out`. Returns the local parent of an incremental stream.
/// Stops at the first command that fails a check, before that command reaches `out`.
pub fn check(src: &mut dyn Read, out: &mut dyn Write, e: &Expect) -> Result<Option<PathBuf>> {
    let mut head = [0u8; 17];
    src.read_exact(&mut head)?;
    let limits = DecodeLimits::default();
    let version = Version::decode(&head)?;
    let mut decoder = ReceiveDecoder::new(version, limits);
    out.write_all(&head)?;
    // (uuid, ctransid) of the new subvolume and of its parent: the only clone sources.
    let mut own: Option<(Vec<u8>, Vec<u8>)> = None;
    let mut parent: Option<(Vec<u8>, Vec<u8>)> = None;
    let mut parent_path = None;
    let mut hdr = [0u8; 10];
    let mut body = Vec::new();
    loop {
        src.read_exact(&mut hdr)?;
        let frame = CommandHeader::decode(&hdr, limits)?;
        body.resize(frame.body_len(), 0);
        src.read_exact(&mut body)?;
        let command = decoder.decode(frame, &body)?;
        let code = command.code();
        match code {
            SUBVOL | SNAPSHOT => {
                if command.value(A_PATH)? != e.name.as_bytes() {
                    bail!("the stream is not {}", e.name);
                }
                let uuid = command.value(A_UUID)?;
                if uuid_text(uuid)? != e.uuid {
                    bail!("the stream of {} is not the subvolume its record names", e.name);
                }
                own = Some((uuid.to_vec(), command.value(A_CTRANSID)?.to_vec()));
                if code == SNAPSHOT {
                    let pu = command.value(A_CLONE_UUID)?;
                    let pu_text = uuid_text(pu)?;
                    let local =
                        (e.parent)(&pu_text).ok_or_else(|| anyhow!("the parent of {} is not a state of this project", e.name))?;
                    parent = Some((pu.to_vec(), command.value(A_CLONE_CTRANSID)?.to_vec()));
                    parent_path = Some(local);
                }
            }
            CLONE => {
                let source = (command.value(A_CLONE_UUID)?.to_vec(), command.value(A_CLONE_CTRANSID)?.to_vec());
                if Some(&source) != own.as_ref() && Some(&source) != parent.as_ref() {
                    let uuid = uuid_text(&source.0)?;
                    let transid =
                        u64::from_le_bytes(source.1.as_slice().try_into().map_err(|_| anyhow!("bad clone transaction"))?);
                    let (_, _, root) =
                        e.clones.iter().find(|(u, t, _)| *u == uuid && t.is_none_or(|t| t == transid)).ok_or_else(|| {
                            anyhow!("the stream clones from a subvolume outside this state and its allowed sources")
                        })?;
                    check_clone_file(root, command.value(A_CLONE_PATH)?)?;
                }
            }
            _ => {}
        }
        out.write_all(&hdr)?;
        out.write_all(&body)?;
        if code == END {
            break;
        }
    }
    let mut extra = [0u8; 1];
    if src.read(&mut extra)? != 0 {
        bail!("data after the end of the stream");
    }
    decoder.finish()?;
    Ok(parent_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(code: u16, attrs: &[(u16, &[u8])]) -> Vec<u8> {
        let zero = 0u64.to_le_bytes();
        let mut attrs = attrs.to_vec();
        if (3..=8).contains(&code) {
            attrs.push((3, &zero));
        }
        if code == CLONE {
            attrs.extend([(18, zero.as_slice()), (23, zero.as_slice()), (24, zero.as_slice())]);
        }
        layr_stream_codec::encode_command(code, &attrs, DecodeLimits::default()).unwrap()
    }

    fn stream(cmds: &[Vec<u8>]) -> Vec<u8> {
        let mut s = MAGIC.to_vec();
        s.extend_from_slice(&1u32.to_le_bytes());
        for c in cmds {
            s.extend_from_slice(c);
        }
        s
    }

    const U: [u8; 16] = [1; 16];
    const P: [u8; 16] = [2; 16];
    const X: [u8; 16] = [3; 16];

    fn run(s: &[u8]) -> Result<Option<PathBuf>> {
        let uuid = uuid_text(&U).unwrap();
        let p_text = uuid_text(&P).unwrap();
        let parent = move |u: &str| if u == p_text { Some(PathBuf::from("/parent")) } else { None };
        let e = Expect { name: "s1", uuid: &uuid, parent: &parent, clones: &[] };
        check(&mut &s[..], &mut Vec::new(), &e)
    }

    #[test]
    fn checks_streams() {
        let t = 7u64.to_le_bytes();
        let snap = cmd(SNAPSHOT, &[(A_PATH, b"s1"), (A_UUID, &U), (A_CTRANSID, &t), (A_CLONE_UUID, &P), (A_CLONE_CTRANSID, &t)]);
        let end = cmd(END, &[]);
        let ok = stream(&[snap.clone(), cmd(3, &[(A_PATH, b"a/b")]), end.clone()]);
        assert_eq!(run(&ok).unwrap(), Some(PathBuf::from("/parent")));
        let foreign =
            cmd(SNAPSHOT, &[(A_PATH, b"s1"), (A_UUID, &U), (A_CTRANSID, &t), (A_CLONE_UUID, &X), (A_CLONE_CTRANSID, &t)]);
        assert_eq!(
            run(&stream(&[foreign, end.clone()])).unwrap_err().to_string(),
            "the parent of s1 is not a state of this project"
        );
        let up = cmd(3, &[(A_PATH, b"../escape")]);
        assert!(run(&stream(&[snap.clone(), up, end.clone()])).is_err(), "a path with ..");
        let clone = cmd(CLONE, &[(A_PATH, b"f"), (A_CLONE_UUID, &X), (A_CLONE_CTRANSID, &t), (A_CLONE_PATH, b"g")]);
        assert!(run(&stream(&[snap.clone(), clone, end.clone()])).is_err(), "a clone from another subvolume");
        let clone_path = cmd(CLONE, &[(A_PATH, b"f"), (A_CLONE_UUID, &P), (A_CLONE_CTRANSID, &t), (A_CLONE_PATH, b"../../x")]);
        assert!(run(&stream(&[snap.clone(), clone_path, end.clone()])).is_err(), "a clone path with ..");
        let other_name = cmd(SUBVOL, &[(A_PATH, b"s2"), (A_UUID, &U), (A_CTRANSID, &t)]);
        assert_eq!(run(&stream(&[other_name, end.clone()])).unwrap_err().to_string(), "the stream is not s1");
        let mut trailing = ok.clone();
        trailing.push(0);
        assert!(run(&trailing).is_err(), "data after the end");
        let two = stream(&[snap.clone(), cmd(SUBVOL, &[(A_PATH, b"s1"), (A_UUID, &U), (A_CTRANSID, &t)]), end]);
        assert_eq!(
            run(&two).unwrap_err().downcast_ref::<layr_stream_codec::DecodeError>(),
            Some(&layr_stream_codec::DecodeError::Sequence)
        );
    }
    #[test]
    fn invalid_command_is_never_forwarded() {
        let uuid = uuid_text(&U).unwrap();
        let e = Expect { name: "s1", uuid: &uuid, parent: &|_| None, clones: &[] };
        let first = cmd(SUBVOL, &[(A_PATH, b"s1"), (A_UUID, &U), (A_CTRANSID, &7u64.to_le_bytes())]);
        for bad in [
            cmd(3, &[(A_PATH, b"../escape")]),
            cmd(17, &[(A_PATH, b"file"), (4, &[0; 7])]),
            cmd(17, &[(A_PATH, b"file")]),
            cmd(3, &[(A_PATH, b"file"), (A_PATH, b"other")]),
            {
                let mut damaged = cmd(3, &[(A_PATH, b"file")]);
                damaged[6] ^= 1;
                damaged
            },
        ] {
            let data = stream(&[first.clone(), bad, cmd(END, &[])]);
            let mut out = Vec::new();
            assert!(check(&mut data.as_slice(), &mut out, &e).is_err());
            assert_eq!(out, stream(std::slice::from_ref(&first)));
        }
    }

    #[test]
    fn extra_clone_sources_require_exact_identity() {
        let t = 7u64.to_le_bytes();
        let uuid = uuid_text(&U).unwrap();
        let extra = uuid_text(&X).unwrap();
        let parent = |_: &str| None;
        let root = std::env::temp_dir().join(format!("layr-clone-test-{}", crate::ids::new_id()));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("g"), b"data").unwrap();
        std::os::unix::fs::symlink(root.join("g"), root.join("link")).unwrap();
        std::os::unix::fs::symlink(&root, root.join("dir-link")).unwrap();
        let allowed = [(extra.clone(), Some(7), root.clone())];
        let e = Expect { name: "s1", uuid: &uuid, parent: &parent, clones: &allowed };
        let subvol = cmd(SUBVOL, &[(A_PATH, b"s1"), (A_UUID, &U), (A_CTRANSID, &t)]);
        let clone = |transid: &[u8], path: &[u8]| {
            cmd(CLONE, &[(A_PATH, b"f"), (A_CLONE_UUID, &X), (A_CLONE_CTRANSID, transid), (A_CLONE_PATH, path)])
        };
        let valid = stream(&[subvol.clone(), clone(&t, b"g"), cmd(END, &[])]);
        assert!(check(&mut valid.as_slice(), &mut Vec::new(), &e).is_ok());
        assert!(run(&valid).is_err(), "the old policy rejects the extra source");
        let local = [(extra, None, root.clone())];
        let local_expect = Expect { name: "s1", uuid: &uuid, parent: &parent, clones: &local };
        let returned = stream(&[subvol.clone(), clone(&8u64.to_le_bytes(), b"g"), cmd(END, &[])]);
        assert!(check(&mut returned.as_slice(), &mut Vec::new(), &local_expect).is_ok());
        for bad in [
            clone(&8u64.to_le_bytes(), b"g"),
            clone(&t, b"../secret"),
            clone(&[], b"g"),
            clone(&t, b"link"),
            clone(&t, b"dir-link/g"),
            clone(&t, b"missing"),
        ] {
            let data = stream(&[subvol.clone(), bad, cmd(END, &[])]);
            assert!(check(&mut data.as_slice(), &mut Vec::new(), &e).is_err());
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
