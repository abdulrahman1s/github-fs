use std::io::Write;
use std::path::{Path, PathBuf};

use crate::cache::CacheError;

pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    /// Open (creating if necessary) a blob store rooted at `root`. The store
    /// uses a `<root>/<aa>/<sha>` layout to keep any single directory below a
    /// few thousand entries.
    pub fn open<P: AsRef<Path>>(root: P) -> Result<Self, CacheError> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn path_for(&self, sha: &str) -> PathBuf {
        let prefix = if sha.len() >= 2 { &sha[..2] } else { sha };
        self.root.join(prefix).join(sha)
    }

    pub fn contains(&self, sha: &str) -> bool {
        self.path_for(sha).exists()
    }

    pub fn read(&self, sha: &str) -> Result<Vec<u8>, CacheError> {
        std::fs::read(self.path_for(sha)).map_err(Into::into)
    }

    /// Write `bytes` to the path for `sha`, atomically. We write to a
    /// `NamedTempFile` in the same directory as the final path (so the rename
    /// is atomic on the same filesystem), fsync the data, and only then
    /// rename it into place. This guarantees readers either see the complete
    /// bytes or no file at all, even across crashes.
    pub fn put(&self, sha: &str, bytes: &[u8]) -> Result<(), CacheError> {
        let final_path = self.path_for(sha);
        let parent = final_path
            .parent()
            .expect("path_for always returns a path with a parent");
        std::fs::create_dir_all(parent)?;

        let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
        tmp.write_all(bytes)?;
        tmp.as_file().sync_all()?;
        tmp.persist(&final_path)
            .map_err(|e| CacheError::Persist(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn put_then_read_round_trip() {
        let dir = tempdir().unwrap();
        let bs = BlobStore::open(dir.path()).unwrap();
        bs.put("abcd1234", b"hello world").unwrap();
        assert!(bs.contains("abcd1234"));
        assert_eq!(bs.read("abcd1234").unwrap(), b"hello world");
    }

    #[test]
    fn path_for_shards_by_first_two_chars() {
        let dir = tempdir().unwrap();
        let bs = BlobStore::open(dir.path()).unwrap();
        let p = bs.path_for("abcd1234");
        assert_eq!(p.parent().unwrap().file_name().unwrap(), "ab");
        assert_eq!(p.file_name().unwrap(), "abcd1234");
    }

    #[test]
    fn path_for_tolerates_short_sha() {
        let dir = tempdir().unwrap();
        let bs = BlobStore::open(dir.path()).unwrap();
        let p = bs.path_for("x");
        // Degenerate but defined: shard prefix == the whole sha.
        assert_eq!(p.parent().unwrap().file_name().unwrap(), "x");
        assert_eq!(p.file_name().unwrap(), "x");
    }

    #[test]
    fn second_put_overwrites_atomically() {
        let dir = tempdir().unwrap();
        let bs = BlobStore::open(dir.path()).unwrap();
        bs.put("sha1234", b"first").unwrap();
        bs.put("sha1234", b"second-longer-content").unwrap();
        assert_eq!(bs.read("sha1234").unwrap(), b"second-longer-content");
    }

    #[test]
    fn no_temp_files_left_behind_after_put() {
        let dir = tempdir().unwrap();
        let bs = BlobStore::open(dir.path()).unwrap();
        bs.put("ababxyz", b"x").unwrap();
        let prefix_dir = dir.path().join("ab");
        let entries: Vec<_> = std::fs::read_dir(&prefix_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), 1, "found leftovers: {entries:?}");
        assert_eq!(entries[0], "ababxyz");
    }

    #[test]
    fn handles_full_byte_range_payload() {
        let dir = tempdir().unwrap();
        let bs = BlobStore::open(dir.path()).unwrap();
        let bytes: Vec<u8> = (0u8..=255).collect();
        bs.put("binsha", &bytes).unwrap();
        assert_eq!(bs.read("binsha").unwrap(), bytes);
    }

    #[test]
    fn read_missing_returns_error() {
        let dir = tempdir().unwrap();
        let bs = BlobStore::open(dir.path()).unwrap();
        assert!(bs.read("nope").is_err());
    }

    #[test]
    fn contains_returns_false_for_missing() {
        let dir = tempdir().unwrap();
        let bs = BlobStore::open(dir.path()).unwrap();
        assert!(!bs.contains("nope"));
    }

    #[test]
    fn empty_blob_round_trips() {
        let dir = tempdir().unwrap();
        let bs = BlobStore::open(dir.path()).unwrap();
        bs.put("empty1", b"").unwrap();
        assert!(bs.contains("empty1"));
        assert_eq!(bs.read("empty1").unwrap(), b"");
    }

    #[test]
    fn open_creates_root_dir() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join("a").join("b").join("c");
        assert!(!nested.exists());
        let _bs = BlobStore::open(&nested).unwrap();
        assert!(nested.exists());
    }
}
