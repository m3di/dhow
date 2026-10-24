use std::cell::RefCell;

use sha2::Digest as _;

use super::*;

struct Dir {
    dir: tempfile::TempDir,
}

impl Dir {
    /// A finished download: `report.pdf.part` with `content` and its sidecar.
    fn with_part(content: &[u8]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("report.pdf.part"), content).unwrap();
        std::fs::write(dir.path().join("report.pdf.dhow"), b"journal").unwrap();
        Self { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn finalize(&self, digest: Option<&Digest>) -> Result<PathBuf, CompleteError> {
        let (part, sidecar) = (self.path("report.pdf.part"), self.path("report.pdf.dhow"));
        let target = Target {
            part: &part,
            sidecar: &sidecar,
            directory: self.dir.path(),
            name: "report.pdf",
        };
        finalize(&target, digest, |_| Ok(()))
    }
}

fn digest_of(algorithm: DigestAlgorithm, data: &[u8]) -> Digest {
    let value = match algorithm {
        DigestAlgorithm::Md5 => Md5::digest(data).to_vec(),
        DigestAlgorithm::Sha1 => Sha1::digest(data).to_vec(),
        DigestAlgorithm::Sha256 => Sha256::digest(data).to_vec(),
        DigestAlgorithm::Sha512 => Sha512::digest(data).to_vec(),
    };
    Digest { algorithm, value }
}

#[test]
fn renames_and_removes_the_sidecar() {
    let d = Dir::with_part(b"hello");
    let final_path = d.finalize(None).unwrap();
    assert_eq!(final_path, d.path("report.pdf"));
    assert_eq!(d.names(), ["report.pdf"]);
    assert_eq!(std::fs::read(final_path).unwrap(), b"hello");
}

#[test]
fn own_part_and_sidecar_are_not_collisions() {
    // "report.pdf.part" exists, which deduplicate alone would treat as taken.
    let d = Dir::with_part(b"x");
    assert_eq!(d.finalize(None).unwrap(), d.path("report.pdf"));
}

#[test]
fn name_taken_since_add_gets_the_collision_rule() {
    let d = Dir::with_part(b"new");
    std::fs::write(d.path("report.pdf"), b"user's file").unwrap();
    let final_path = d.finalize(None).unwrap();
    assert_eq!(final_path, d.path("report (2).pdf"));
    assert_eq!(std::fs::read(d.path("report.pdf")).unwrap(), b"user's file");
    assert_eq!(std::fs::read(final_path).unwrap(), b"new");
}

#[test]
fn a_file_appearing_at_the_last_moment_is_never_overwritten() {
    let d = Dir::with_part(b"ours");
    let (part, sidecar) = (d.path("report.pdf.part"), d.path("report.pdf.dhow"));
    let target = Target {
        part: &part,
        sidecar: &sidecar,
        directory: d.dir.path(),
        name: "report.pdf",
    };
    let tried = RefCell::new(Vec::new());
    let final_path = finalize(&target, None, |p| {
        // Another program creates the chosen name between our listing and
        // the rename, once.
        if tried.borrow().is_empty() {
            std::fs::write(p, b"theirs").unwrap();
        }
        tried.borrow_mut().push(p.to_owned());
        Ok(())
    })
    .unwrap();
    assert_eq!(
        *tried.borrow(),
        [d.path("report.pdf"), d.path("report (2).pdf")]
    );
    assert_eq!(std::fs::read(d.path("report.pdf")).unwrap(), b"theirs");
    assert_eq!(std::fs::read(&final_path).unwrap(), b"ours");
}

#[test]
fn before_rename_failure_aborts_with_nothing_moved() {
    let d = Dir::with_part(b"x");
    let (part, sidecar) = (d.path("report.pdf.part"), d.path("report.pdf.dhow"));
    let target = Target {
        part: &part,
        sidecar: &sidecar,
        directory: d.dir.path(),
        name: "report.pdf",
    };
    let err = finalize(&target, None, |_| Err(io::Error::other("db down"))).unwrap_err();
    assert!(matches!(err, CompleteError::Io(_)));
    assert_eq!(d.names(), ["report.pdf.dhow", "report.pdf.part"]);
}

#[test]
fn every_supported_digest_is_checked() {
    let data = b"the quick brown fox".repeat(100_000);
    for algorithm in [
        DigestAlgorithm::Md5,
        DigestAlgorithm::Sha1,
        DigestAlgorithm::Sha256,
        DigestAlgorithm::Sha512,
    ] {
        let d = Dir::with_part(&data);
        d.finalize(Some(&digest_of(algorithm, &data))).unwrap();
        assert_eq!(d.names(), ["report.pdf"], "{algorithm:?}");
    }
}

/// T-I8: mismatch fails, keeps the part and sidecar, renames nothing.
#[test]
fn digest_mismatch_keeps_everything_for_inspection() {
    let d = Dir::with_part(b"what we got");
    let expected = digest_of(DigestAlgorithm::Sha256, b"what the server meant");
    let err = d.finalize(Some(&expected)).unwrap_err();
    assert!(matches!(
        err,
        CompleteError::DigestMismatch { algorithm: DigestAlgorithm::Sha256, ref expected, ref actual }
            if expected != actual && actual.len() == 32
    ));
    assert_eq!(d.names(), ["report.pdf.dhow", "report.pdf.part"]);
}

#[test]
fn recover_after_rename_before_sidecar_delete() {
    let d = Dir::with_part(b"x");
    // The crash window: rename done, sidecar still there.
    std::fs::rename(d.path("report.pdf.part"), d.path("report.pdf")).unwrap();
    let (part, sidecar, fin) = (
        d.path("report.pdf.part"),
        d.path("report.pdf.dhow"),
        d.path("report.pdf"),
    );
    assert!(recover(&part, &sidecar, &fin).unwrap());
    assert_eq!(d.names(), ["report.pdf"]);
    // Idempotent.
    assert!(recover(&part, &sidecar, &fin).unwrap());
}

#[test]
fn recover_does_nothing_while_the_part_exists() {
    let d = Dir::with_part(b"x");
    std::fs::write(d.path("report.pdf"), b"unrelated").unwrap();
    let (part, sidecar, fin) = (
        d.path("report.pdf.part"),
        d.path("report.pdf.dhow"),
        d.path("report.pdf"),
    );
    assert!(!recover(&part, &sidecar, &fin).unwrap());
    assert_eq!(
        d.names(),
        ["report.pdf", "report.pdf.dhow", "report.pdf.part"]
    );
}

#[test]
fn rename_noreplace_refuses_existing_targets() {
    let d = Dir::with_part(b"a");
    std::fs::write(d.path("taken"), b"b").unwrap();
    let err = rename_noreplace(&d.path("report.pdf.part"), &d.path("taken")).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(std::fs::read(d.path("taken")).unwrap(), b"b");
    assert!(d.path("report.pdf.part").exists());
}
