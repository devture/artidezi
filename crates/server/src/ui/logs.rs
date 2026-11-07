// SPDX-FileCopyrightText: 2026 Slavi Pantaleev
// SPDX-License-Identifier: Apache-2.0

use std::collections::BTreeMap;
use std::io::Read;

use artidezi_contract::attempt::{ArtifactEntry, AttemptRecord};
use artidezi_contract::common::ArtifactRef;
use artidezi_store::ArtifactStore;
use sha2::{Digest, Sha256};

pub(super) type LineCache = BTreeMap<String, (u64, u64)>;

pub(super) struct LogView {
    pub entry: ArtifactEntry,
    pub label: String,
    pub kind: &'static str,
    pub lines: Option<u64>,
}

fn count_lines(bytes: &[u8]) -> u64 {
    bytes.iter().filter(|&&b| b == b'\n').count() as u64
        + u64::from(!bytes.is_empty() && !bytes.ends_with(b"\n"))
}

fn lines(
    entry: &ArtifactEntry,
    store: &ArtifactStore,
    cache: &mut LineCache,
    budget: &mut u64,
) -> Option<u64> {
    if let Some(&(size, count)) = cache.get(&entry.sha256) {
        return (size == entry.size).then_some(count);
    }
    // A page refresh must not read arbitrarily large worker artifacts under the controller lock.
    if entry.size > 2 * 1024 * 1024 || entry.size > *budget {
        return None;
    }
    *budget -= entry.size;
    let reference = ArtifactRef {
        sha256: entry.sha256.clone(),
        size: entry.size,
    };
    let file = std::fs::File::open(store.path(&reference).ok()?).ok()?;
    let mut bytes = Vec::new();
    file.take(entry.size + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 != entry.size || hex::encode(Sha256::digest(&bytes)) != entry.sha256 {
        return None;
    }
    let count = count_lines(&bytes);
    if cache.len() >= 4096 {
        cache.clear();
    }
    cache.insert(entry.sha256.clone(), (entry.size, count));
    Some(count)
}

pub(super) fn describe(
    record: &AttemptRecord,
    store: &ArtifactStore,
    cache: &mut LineCache,
    budget: &mut u64,
) -> Vec<LogView> {
    let mut labels = BTreeMap::new();
    for stream in ["stdout", "stderr"] {
        labels.insert(format!("harness-{stream}.log"), format!("Agent {stream}"));
        for repo in &record.repositories {
            for (prefix, steps) in [("setup", &repo.setup), ("verify", &repo.verification)] {
                for (index, step) in steps.iter().enumerate() {
                    labels.insert(
                        format!("{prefix}-{}-{index}-{stream}.log", repo.id),
                        format!("{} · {} · {stream}", repo.id, step.name),
                    );
                }
            }
        }
    }
    record
        .artifacts
        .iter()
        .filter(|entry| {
            entry.media_type.starts_with("text/") || entry.media_type == "application/x-ndjson"
        })
        .map(|entry| {
            let kind = if entry.name.ends_with(".patch") {
                "patch"
            } else if entry.name.ends_with("-stderr.log") {
                "stderr"
            } else if entry.name.ends_with("-stdout.log") {
                "stdout"
            } else {
                "file"
            };
            LogView {
                entry: entry.clone(),
                label: labels
                    .get(&entry.name)
                    .cloned()
                    .unwrap_or_else(|| entry.name.clone()),
                kind,
                lines: lines(entry, store, cache, budget),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_line_counts_handle_empty_unterminated_and_unavailable_logs() {
        let dir = tempfile::tempdir().unwrap();
        let store = ArtifactStore::open(dir.path()).unwrap();
        let mut cache = LineCache::new();
        for (bytes, expected) in [
            (b"".as_slice(), 0),
            (b"a\n", 1),
            (b"a\r\nb", 2),
            (b"\n\n", 2),
        ] {
            let reference = store.put(bytes).unwrap();
            let entry = ArtifactEntry {
                name: "harness-stdout.log".into(),
                sha256: reference.sha256,
                size: reference.size,
                media_type: "text/plain".into(),
            };
            assert_eq!(lines(&entry, &store, &mut cache, &mut 100), Some(expected));
            assert_eq!(lines(&entry, &store, &mut cache, &mut 0), Some(expected));
        }
        let reference = store.put(b"bad").unwrap();
        let mut entry = ArtifactEntry {
            name: "log".into(),
            sha256: reference.sha256,
            size: reference.size,
            media_type: "text/plain".into(),
        };
        assert_eq!(lines(&entry, &store, &mut cache, &mut 0), None);
        entry.size = 0;
        assert_eq!(lines(&entry, &store, &mut cache, &mut 100), None);
        entry.sha256 = "../missing".into();
        assert_eq!(lines(&entry, &store, &mut cache, &mut 100), None);
    }
}
