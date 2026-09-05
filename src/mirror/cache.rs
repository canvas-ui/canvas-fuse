//! Content cache: bytes on disk addressed by their sha256, under a byte
//! budget. `<data_dir>/cache/<aa>/<sha256>` — two-hex-digit fan-out so a
//! big home does not put a hundred thousand files in one directory.
//!
//! Content addressing is what makes the rest simple: a rename never moves
//! bytes, a hub echo of our own push is a no-op, two keys with the same
//! bytes share one file, and eviction can never corrupt anything — a
//! missing file is a cache miss, refetched by digest.
//!
//! Downloads land in `<sha>.part` and are renamed into place only once the
//! digest verifies; a `.part` left behind by a crash is resumed with a
//! `Range` request. The index (`cache_v1`, size + atime + pin refs) lives in
//! the store so LRU decisions do not need a directory walk.

use super::store::{CacheMeta, Store};
use anyhow::{Context as _, Result};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct Cache {
    root: PathBuf,
    store: Arc<Store>,
    budget: u64,
    /// Serializes insert/evict so two threads never race a rename against
    /// a removal of the same digest.
    lock: Mutex<()>,
}

impl Cache {
    pub fn open(root: &Path, store: Arc<Store>, budget: u64) -> Result<Self> {
        std::fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
        Ok(Self {
            root: root.to_path_buf(),
            store,
            budget,
            lock: Mutex::new(()),
        })
    }

    pub fn budget(&self) -> u64 {
        self.budget
    }

    pub fn path_for(&self, sha: &str) -> PathBuf {
        let fan = if sha.len() >= 2 { &sha[..2] } else { "xx" };
        self.root.join(fan).join(sha)
    }

    pub fn part_path_for(&self, sha: &str) -> PathBuf {
        let mut p = self.path_for(sha);
        p.set_extension("part");
        p
    }

    pub fn has(&self, sha: &str) -> bool {
        self.path_for(sha).is_file()
    }

    pub fn used(&self) -> u64 {
        self.store.cache_metas().iter().map(|(_, m)| m.size).sum()
    }

    /// Bump the digest's recency. Cheap enough per read; the store write is
    /// the cost, so callers throttle (the fetch pool touches on fetch, the
    /// read path once per open).
    pub fn touch(&self, sha: &str) {
        if let Some(mut meta) = self.store.cache_meta(sha) {
            meta.atime = super::now_ms();
            let _ = self.store.put_cache_meta(sha, &meta);
        }
    }

    /// A byte window of a cached blob. Short reads at EOF are the caller's
    /// (the kernel's) normal expectation.
    pub fn pread(&self, sha: &str, offset: u64, len: usize) -> std::io::Result<Vec<u8>> {
        use std::os::unix::fs::FileExt;
        let f = std::fs::File::open(self.path_for(sha))?;
        let mut buf = vec![0u8; len];
        let mut done = 0usize;
        while done < len {
            let n = f.read_at(&mut buf[done..], offset + done as u64)?;
            if n == 0 {
                break;
            }
            done += n;
        }
        buf.truncate(done);
        Ok(buf)
    }

    pub fn read_all(&self, sha: &str) -> std::io::Result<Vec<u8>> {
        std::fs::read(self.path_for(sha))
    }

    /// Store bytes we produced locally (a flushed write). Returns the digest.
    pub fn insert_bytes(&self, bytes: &[u8]) -> Result<String> {
        let sha = super::sha256_hex(bytes);
        let _g = self.lock.lock();
        let path = self.path_for(&sha);
        if !path.is_file() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let tmp = self.part_path_for(&sha);
            {
                let mut f = std::fs::File::create(&tmp)?;
                f.write_all(bytes)?;
                f.sync_data()?;
            }
            std::fs::rename(&tmp, &path)?;
        }
        self.record(&sha, bytes.len() as u64)?;
        Ok(sha)
    }

    /// Register a file that just landed (verified) at `path_for(sha)`.
    fn record(&self, sha: &str, size: u64) -> Result<()> {
        let pin_refs = self.store.cache_meta(sha).map(|m| m.pin_refs).unwrap_or(0);
        self.store.put_cache_meta(
            sha,
            &CacheMeta {
                size,
                atime: super::now_ms(),
                pin_refs,
            },
        )
    }

    /// Download `key` from the hub into the cache, verifying `expected_sha`.
    /// Resumes a `.part` if one is there. Returns the digest actually
    /// received (the hub's ETag), which may differ from `expected_sha` when
    /// the key changed under us — the caller then decides what to do with it.
    pub fn fetch(
        &self,
        hub: &super::hub::HubClient,
        key: &str,
        expected_sha: &str,
    ) -> Result<String, super::hub::HubError> {
        use super::hub::HubError;
        if self.has(expected_sha) {
            self.touch(expected_sha);
            return Ok(expected_sha.to_string());
        }
        let part = self.part_path_for(expected_sha);
        if let Some(parent) = part.parent() {
            std::fs::create_dir_all(parent).map_err(|e| HubError::Other(e.to_string()))?;
        }
        let have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        // Hash what is already on disk so the final digest covers the whole
        // file, resumed or not.
        let mut hasher = Sha256::new();
        if have > 0 {
            let mut f = std::fs::File::open(&part).map_err(|e| HubError::Other(e.to_string()))?;
            let mut buf = vec![0u8; 1 << 16];
            loop {
                let n = f
                    .read(&mut buf)
                    .map_err(|e| HubError::Other(e.to_string()))?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
        }
        let (mut resp, etag) = hub.get_object(key, if have > 0 { Some(have) } else { None })?;
        // A hub that ignored the Range (or a full 200 on resume) restarts
        // the file; a 206 continues it.
        let resumed = have > 0 && resp.status() == reqwest::StatusCode::PARTIAL_CONTENT;
        if !resumed {
            hasher = Sha256::new();
        }
        let mut out = std::fs::OpenOptions::new()
            .create(true)
            .append(resumed)
            .write(true)
            .truncate(!resumed)
            .open(&part)
            .map_err(|e| HubError::Other(e.to_string()))?;
        let mut buf = vec![0u8; 1 << 16];
        let mut total = if resumed { have } else { 0 };
        loop {
            let n = resp
                .read(&mut buf)
                .map_err(|e| HubError::Offline(format!("reading {key}: {e}")))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            out.write_all(&buf[..n])
                .map_err(|e| HubError::Other(e.to_string()))?;
            total += n as u64;
        }
        out.sync_data()
            .map_err(|e| HubError::Other(e.to_string()))?;
        drop(out);
        let got = super::hex(&hasher.finalize());
        if let Some(etag) = &etag {
            if etag != &got {
                // Bytes changed mid-download (or a resume straddled a
                // replacement). Start over next time.
                let _ = std::fs::remove_file(&part);
                return Err(HubError::Other(format!(
                    "{key}: digest mismatch (hub says {etag}, got {got})"
                )));
            }
        }
        let _g = self.lock.lock();
        let dest = self.path_for(&got);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| HubError::Other(e.to_string()))?;
        }
        if dest.is_file() {
            let _ = std::fs::remove_file(&part);
        } else {
            std::fs::rename(&part, &dest).map_err(|e| HubError::Other(e.to_string()))?;
        }
        self.record(&got, total)
            .map_err(|e| HubError::Other(e.to_string()))?;
        Ok(got)
    }

    /// Pin bookkeeping: pinned digests are never evicted.
    pub fn set_pinned(&self, sha: &str, pinned: bool) {
        if let Some(mut meta) = self.store.cache_meta(sha) {
            let refs = if pinned { 1 } else { 0 };
            if meta.pin_refs != refs {
                meta.pin_refs = refs;
                let _ = self.store.put_cache_meta(sha, &meta);
            }
        }
    }

    /// Bring the cache under budget, oldest first, skipping `protected`
    /// digests (pinned, dirty, trashed, open). Returns bytes freed.
    pub fn evict(&self, protected: &HashSet<String>) -> u64 {
        let _g = self.lock.lock();
        let mut metas = self.store.cache_metas();
        // Index entries whose file is gone are just noise.
        metas.retain(|(sha, _)| {
            if self.path_for(sha).is_file() {
                true
            } else {
                let _ = self.store.remove_cache_meta(sha);
                false
            }
        });
        let mut used: u64 = metas.iter().map(|(_, m)| m.size).sum();
        if used <= self.budget {
            return 0;
        }
        metas.sort_by_key(|(_, m)| m.atime);
        let mut freed = 0;
        for (sha, meta) in metas {
            if used <= self.budget {
                break;
            }
            if meta.pin_refs > 0 || protected.contains(&sha) {
                continue;
            }
            if std::fs::remove_file(self.path_for(&sha)).is_ok() {
                let _ = self.store.remove_cache_meta(&sha);
                used -= meta.size;
                freed += meta.size;
            }
        }
        freed
    }

    /// Remove one digest outright (its file and index row).
    pub fn remove(&self, sha: &str) {
        let _g = self.lock.lock();
        let _ = std::fs::remove_file(self.path_for(sha));
        let _ = std::fs::remove_file(self.part_path_for(sha));
        let _ = self.store.remove_cache_meta(sha);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache(budget: u64) -> (tempfile::TempDir, Cache, Arc<Store>) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("mirror.redb")).unwrap());
        let cache = Cache::open(&dir.path().join("cache"), store.clone(), budget).unwrap();
        (dir, cache, store)
    }

    #[test]
    fn insert_pread_and_budget_eviction_respect_pins() {
        let (_d, c, store) = cache(25);
        let a = c.insert_bytes(b"aaaaaaaaaa").unwrap(); // 10
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = c.insert_bytes(b"bbbbbbbbbb").unwrap(); // 10
        std::thread::sleep(std::time::Duration::from_millis(2));
        assert_eq!(c.pread(&a, 2, 4).unwrap(), b"aaaa");
        assert_eq!(c.pread(&a, 8, 10).unwrap(), b"aa");
        assert_eq!(c.used(), 20);

        c.set_pinned(&a, true);
        let d = c.insert_bytes(b"dddddddddd").unwrap(); // 30 > 25
        let freed = c.evict(&HashSet::new());
        assert_eq!(freed, 10);
        assert!(c.has(&a), "pinned digest survives");
        assert!(!c.has(&b), "oldest unpinned digest goes first");
        assert!(c.has(&d));
        assert!(store.cache_meta(&b).is_none());

        // Protected digests survive too, even over budget.
        let e = c.insert_bytes(b"eeeeeeeeee").unwrap();
        let protected: HashSet<String> = [d.clone()].into_iter().collect();
        c.evict(&protected);
        assert!(c.has(&a));
        assert!(c.has(&d));
        assert!(!c.has(&e) || c.used() <= 25);
    }

    #[test]
    fn same_bytes_share_one_file() {
        let (_d, c, _s) = cache(1000);
        let a = c.insert_bytes(b"same").unwrap();
        let b = c.insert_bytes(b"same").unwrap();
        assert_eq!(a, b);
        assert_eq!(c.used(), 4);
    }
}
