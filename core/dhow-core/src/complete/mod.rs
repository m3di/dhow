//! The completion gate (ENGINE-SPEC §8, I1, I7, I8): nothing appears at a
//! final name until the bytes are proven, and the rename is the commit
//! point. Blocking file I/O; call from `spawn_blocking`.

#[cfg(test)]
mod tests;

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use md5::Md5;
use sha1::Sha1;
use sha2::{Sha256, Sha512};

use crate::journal::sync_dir;
use crate::naming;
use crate::probe::{Digest, DigestAlgorithm};

#[derive(Debug, thiserror::Error)]
pub enum CompleteError {
    /// I8: the bytes are not what the server said they are. The part file
    /// and sidecar are kept for inspection; nothing was renamed.
    #[error("{algorithm:?} digest mismatch")]
    DigestMismatch {
        algorithm: DigestAlgorithm,
        expected: Vec<u8>,
        actual: Vec<u8>,
    },
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Where a finished download goes.
pub struct Target<'a> {
    pub part: &'a Path,
    pub sidecar: &'a Path,
    pub directory: &'a Path,
    /// The name decided at `Add`; the collision rule is applied again now.
    pub name: &'a str,
}

/// I8: hashes the whole part file, in order, and compares.
pub fn verify_digest(part: &Path, digest: &Digest) -> Result<(), CompleteError> {
    let actual = hash_file(part, digest.algorithm)?;
    if actual == digest.value {
        Ok(())
    } else {
        Err(CompleteError::DigestMismatch {
            algorithm: digest.algorithm,
            expected: digest.value.clone(),
            actual,
        })
    }
}

fn hash_file(path: &Path, algorithm: DigestAlgorithm) -> io::Result<Vec<u8>> {
    fn run<H: sha2::Digest>(mut file: File) -> io::Result<Vec<u8>> {
        let mut hasher = H::new();
        let mut buf = vec![0; 1024 * 1024];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                return Ok(hasher.finalize().to_vec());
            }
            hasher.update(&buf[..n]);
        }
    }
    let file = File::open(path)?;
    match algorithm {
        DigestAlgorithm::Md5 => run::<Md5>(file),
        DigestAlgorithm::Sha1 => run::<Sha1>(file),
        DigestAlgorithm::Sha256 => run::<Sha256>(file),
        DigestAlgorithm::Sha512 => run::<Sha512>(file),
    }
}

/// §8 steps 3 and 5–7: digest check, collision rule, no-replace rename,
/// directory sync, sidecar removal. `before_rename` gets each final path
/// before it is tried, so the caller can record it first: if the process
/// dies right after the rename, recovery must know which name was used.
/// Returns the final path.
pub fn finalize(
    target: &Target,
    digest: Option<&Digest>,
    mut before_rename: impl FnMut(&Path) -> io::Result<()>,
) -> Result<PathBuf, CompleteError> {
    if let Some(digest) = digest {
        verify_digest(target.part, digest)?;
    }
    let own: Vec<_> = [target.part, target.sidecar]
        .iter()
        .filter_map(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .collect();
    let final_path = loop {
        let existing: Vec<String> = std::fs::read_dir(target.directory)?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !own.contains(n))
            .collect();
        let candidate = target
            .directory
            .join(naming::deduplicate(target.name, &existing));
        before_rename(&candidate)?;
        match rename_noreplace(target.part, &candidate) {
            Ok(()) => break candidate,
            // Someone took the name since we listed: pick the next one.
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    };
    sync_dir(&final_path)?;
    remove_if_exists(target.sidecar)?;
    Ok(final_path)
}

/// §8: the process died after the rename but before the sidecar was
/// deleted. The rename is the commit point, so this download is complete;
/// the stale sidecar goes. Returns whether that was the situation.
pub fn recover(part: &Path, sidecar: &Path, final_path: &Path) -> io::Result<bool> {
    if part.try_exists()? || !final_path.try_exists()? {
        return Ok(false);
    }
    remove_if_exists(sidecar)?;
    Ok(true)
}

/// Renames `from` to `to`, failing with `AlreadyExists` instead of ever
/// replacing a file that is there (a plain `rename` would destroy one
/// that appeared since we chose the name).
fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android", target_vendor = "apple"))]
    {
        use rustix::fs::{CWD, RenameFlags, renameat_with};
        use rustix::io::Errno;
        match renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE) {
            Ok(()) => return Ok(()),
            // The filesystem doesn't support it; fall back below.
            Err(Errno::INVAL | Errno::NOSYS | Errno::NOTSUP) => {}
            Err(e) => return Err(e.into()),
        }
    }
    // A hard link also refuses to replace; then drop the old name.
    match std::fs::hard_link(from, to) {
        Ok(()) => std::fs::remove_file(from),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(e),
        // No hard links here (FAT, some network shares): best effort.
        Err(_) if to.try_exists()? => Err(io::ErrorKind::AlreadyExists.into()),
        Err(_) => std::fs::rename(from, to),
    }
}

fn remove_if_exists(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}
