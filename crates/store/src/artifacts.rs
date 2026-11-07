// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
//
// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use artidezi_contract::common::ArtifactRef;
use sha2::{Digest, Sha256};

use crate::{Result, StoreError};

/// Content-addressed, write-once file store. Complete reads re-hash bytes; bounded diagnostic
/// tails remain untrusted hints and never establish result or publication validity.
#[derive(Clone, Debug)]
pub struct ArtifactStore {
    root: PathBuf,
    lifecycle: Arc<RwLock<()>>,
}

impl ArtifactStore {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        fs::create_dir_all(root.join("tmp")).map_err(|source| StoreError::Io {
            path: root.display().to_string(),
            source,
        })?;
        Ok(Self {
            root,
            lifecycle: Arc::new(RwLock::new(())),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Worker uploads hold this guard across authorization, durable pinning and file creation.
    /// Controller mutations are serialized separately. All handles must share this store clone.
    pub fn upload_guard(&self) -> RwLockReadGuard<'_, ()> {
        self.lifecycle.read().unwrap_or_else(|p| p.into_inner())
    }

    pub(crate) fn cleanup_guard(&self) -> RwLockWriteGuard<'_, ()> {
        self.lifecycle.write().unwrap_or_else(|p| p.into_inner())
    }

    fn path_for(&self, sha256: &str) -> PathBuf {
        self.root.join(&sha256[..2]).join(sha256)
    }

    pub fn contains(&self, artifact: &ArtifactRef) -> bool {
        artifact.is_well_formed() && self.path_for(&artifact.sha256).is_file()
    }

    /// Path of a stored artifact, for readers that stream large files. Callers must still treat
    /// the bytes as untrusted.
    pub fn path(&self, artifact: &ArtifactRef) -> Result<PathBuf> {
        if !self.contains(artifact) {
            return Err(StoreError::MissingArtifact(artifact.sha256.clone()));
        }
        Ok(self.path_for(&artifact.sha256))
    }

    pub fn put(&self, bytes: &[u8]) -> Result<ArtifactRef> {
        let sha256 = hex::encode(Sha256::digest(bytes));
        let artifact = ArtifactRef {
            sha256,
            size: bytes.len() as u64,
        };
        let final_path = self.path_for(&artifact.sha256);
        if final_path.is_file() {
            return Ok(artifact);
        }
        let io = |source| StoreError::Io {
            path: final_path.display().to_string(),
            source,
        };
        fs::create_dir_all(final_path.parent().expect("artifact path has a parent")).map_err(io)?;
        let tmp_path =
            self.root
                .join("tmp")
                .join(format!("{}.{}", artifact.sha256, crate::new_id("write")));
        {
            let mut file = fs::File::create(&tmp_path).map_err(io)?;
            file.write_all(bytes).map_err(io)?;
            file.sync_all().map_err(io)?;
        }
        fs::rename(&tmp_path, &final_path).map_err(io)?;
        Ok(artifact)
    }

    /// Where an upload can be written before it is verified, on the store's filesystem.
    pub fn upload_temp_path(&self) -> PathBuf {
        self.root.join("tmp").join(crate::new_id("upload"))
    }

    /// Moves a complete, already hashed temporary file into the store.
    pub fn adopt(&self, temp_path: &Path, artifact: &ArtifactRef) -> Result<ArtifactRef> {
        let final_path = self.path_for(&artifact.sha256);
        if final_path.is_file() {
            return Ok(artifact.clone());
        }
        let io = |source| StoreError::Io {
            path: final_path.display().to_string(),
            source,
        };
        fs::create_dir_all(final_path.parent().expect("artifact path has a parent")).map_err(io)?;
        fs::rename(temp_path, &final_path).map_err(io)?;
        Ok(artifact.clone())
    }

    /// Streams a file into the store without holding it in memory.
    pub fn put_file(&self, source_path: &Path) -> Result<ArtifactRef> {
        let io = |source| StoreError::Io {
            path: source_path.display().to_string(),
            source,
        };
        let mut file = fs::File::open(source_path).map_err(io)?;
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buf).map_err(io)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            size += n as u64;
        }
        let artifact = ArtifactRef {
            sha256: hex::encode(hasher.finalize()),
            size,
        };
        let final_path = self.path_for(&artifact.sha256);
        if final_path.is_file() {
            return Ok(artifact);
        }
        fs::create_dir_all(final_path.parent().expect("artifact path has a parent")).map_err(io)?;
        let tmp_path =
            self.root
                .join("tmp")
                .join(format!("{}.{}", artifact.sha256, crate::new_id("write")));
        fs::copy(source_path, &tmp_path).map_err(io)?;
        fs::rename(&tmp_path, &final_path).map_err(io)?;
        Ok(artifact)
    }

    pub fn get(&self, artifact: &ArtifactRef) -> Result<Vec<u8>> {
        let path = self.path(artifact)?;
        let bytes = fs::read(&path).map_err(|source| StoreError::Io {
            path: path.display().to_string(),
            source,
        })?;
        if bytes.len() as u64 != artifact.size {
            return Err(StoreError::CorruptArtifact {
                sha256: artifact.sha256.clone(),
                reason: format!("size {} does not match {}", bytes.len(), artifact.size),
            });
        }
        if hex::encode(Sha256::digest(&bytes)) != artifact.sha256 {
            return Err(StoreError::CorruptArtifact {
                sha256: artifact.sha256.clone(),
                reason: "digest mismatch".into(),
            });
        }
        Ok(bytes)
    }

    pub fn get_json<T: serde::de::DeserializeOwned>(&self, artifact: &ArtifactRef) -> Result<T> {
        Ok(serde_json::from_slice(&self.get(artifact)?)?)
    }

    /// Bounded diagnostic tail. Bytes are untrusted and do not constitute hash verification.
    pub fn tail(&self, artifact: &ArtifactRef, max_bytes: u64) -> Result<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        let path = self.path(artifact)?;
        let io = |source| StoreError::Io {
            path: path.display().to_string(),
            source,
        };
        let mut file = std::fs::File::open(&path).map_err(io)?;
        if file.metadata().map_err(io)?.len() != artifact.size {
            return Err(StoreError::CorruptArtifact {
                sha256: artifact.sha256.clone(),
                reason: "diagnostic size mismatch".into(),
            });
        }
        file.seek(SeekFrom::Start(artifact.size.saturating_sub(max_bytes)))
            .map_err(io)?;
        let mut bytes = vec![];
        file.take(max_bytes).read_to_end(&mut bytes).map_err(io)?;
        Ok(bytes)
    }

    pub fn put_json<T: serde::Serialize>(&self, value: &T) -> Result<ArtifactRef> {
        self.put(&serde_json::to_vec_pretty(value)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_tamper_detection() {
        let dir = tempfile::tempdir().unwrap();
        let store = ArtifactStore::open(dir.path().join("artifacts")).unwrap();
        let artifact = store.put(b"hello").unwrap();
        assert_eq!(artifact.size, 5);
        assert!(store.contains(&artifact));
        assert_eq!(store.get(&artifact).unwrap(), b"hello");
        assert_eq!(store.put(b"hello").unwrap(), artifact);

        fs::write(store.path(&artifact).unwrap(), b"hellO").unwrap();
        assert!(matches!(
            store.get(&artifact),
            Err(StoreError::CorruptArtifact { .. })
        ));

        let missing = ArtifactRef {
            sha256: "0".repeat(64),
            size: 1,
        };
        assert!(matches!(
            store.get(&missing),
            Err(StoreError::MissingArtifact(_))
        ));
    }

    #[test]
    fn put_file_matches_put() {
        let dir = tempfile::tempdir().unwrap();
        let store = ArtifactStore::open(dir.path().join("artifacts")).unwrap();
        let file = dir.path().join("blob");
        fs::write(&file, b"streamed bytes").unwrap();
        assert_eq!(
            store.put_file(&file).unwrap(),
            store.put(b"streamed bytes").unwrap()
        );
    }

    #[test]
    fn concurrent_identical_uploads_use_distinct_temporary_files() {
        use std::sync::{Arc, Barrier};
        let dir = tempfile::tempdir().unwrap();
        let store = ArtifactStore::open(dir.path().join("artifacts")).unwrap();
        let bytes = vec![42; 512 * 1024];
        let source = dir.path().join("input");
        fs::write(&source, &bytes).unwrap();
        let barrier = Arc::new(Barrier::new(8));
        std::thread::scope(|scope| {
            let threads: Vec<_> = (0..8)
                .map(|i| {
                    let store = store.clone();
                    let barrier = &barrier;
                    let bytes = &bytes;
                    let source = &source;
                    scope.spawn(move || {
                        barrier.wait();
                        let artifact = if i % 2 == 0 {
                            store.put(bytes)
                        } else {
                            store.put_file(source)
                        }
                        .unwrap();
                        assert_eq!(store.get(&artifact).unwrap(), *bytes);
                        artifact
                    })
                })
                .collect();
            let refs: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
            assert!(refs.iter().all(|r| r == &refs[0]));
        });
        assert_eq!(fs::read_dir(store.root().join("tmp")).unwrap().count(), 0);
    }
}
