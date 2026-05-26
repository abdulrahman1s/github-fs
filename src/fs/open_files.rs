use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// Tracks `std::fs::File` handles handed out by `open()`. Keyed by the `fh`
/// value we return to FUSE.
///
/// `File` is wrapped in `Arc` so concurrent reads can grab their own clone
/// of the handle without holding the map's read lock for the duration of the
/// I/O. We use `FileExt::read_at` (pread) for reads, which is thread-safe
/// without mutating file position — so no per-file Mutex is needed.
pub struct OpenFiles {
    next_fh: AtomicU64,
    map: RwLock<HashMap<u64, Arc<std::fs::File>>>,
}

impl Default for OpenFiles {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenFiles {
    pub fn new() -> Self {
        Self {
            // 0 is a legal fh, but starting at 1 makes "no fh" sentinels safer.
            next_fh: AtomicU64::new(1),
            map: RwLock::new(HashMap::new()),
        }
    }

    pub fn insert(&self, file: std::fs::File) -> u64 {
        let fh = self.next_fh.fetch_add(1, Ordering::SeqCst);
        self.map
            .write()
            .expect("OpenFiles map poisoned")
            .insert(fh, Arc::new(file));
        fh
    }

    pub fn get(&self, fh: u64) -> Option<Arc<std::fs::File>> {
        self.map
            .read()
            .expect("OpenFiles map poisoned")
            .get(&fh)
            .cloned()
    }

    pub fn remove(&self, fh: u64) {
        self.map
            .write()
            .expect("OpenFiles map poisoned")
            .remove(&fh);
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.map.read().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::FileExt;
    use tempfile::tempdir;

    fn write_temp_file(content: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempdir().unwrap();
        let p = dir.path().join("blob");
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(content).unwrap();
        (dir, p)
    }

    #[test]
    fn insert_returns_distinct_fh_values() {
        let of = OpenFiles::new();
        let (_d, p) = write_temp_file(b"a");
        let f1 = std::fs::File::open(&p).unwrap();
        let f2 = std::fs::File::open(&p).unwrap();
        let h1 = of.insert(f1);
        let h2 = of.insert(f2);
        assert_ne!(h1, h2);
        assert_eq!(of.len(), 2);
    }

    #[test]
    fn get_and_remove_round_trip() {
        let of = OpenFiles::new();
        let (_d, p) = write_temp_file(b"hello-world");
        let f = std::fs::File::open(&p).unwrap();
        let fh = of.insert(f);

        let got = of.get(fh).expect("should be present");
        let mut buf = [0u8; 5];
        let n = got.read_at(&mut buf, 0).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&buf, b"hello");

        of.remove(fh);
        assert!(of.get(fh).is_none());
        assert_eq!(of.len(), 0);
    }

    #[test]
    fn concurrent_reads_get_independent_views() {
        // Verify that two clones of the Arc<File> can pread concurrently
        // without affecting each other's file positions.
        let of = OpenFiles::new();
        let (_d, p) = write_temp_file(b"0123456789");
        let fh = of.insert(std::fs::File::open(&p).unwrap());

        let a = of.get(fh).unwrap();
        let b = of.get(fh).unwrap();

        let mut buf_a = [0u8; 3];
        let mut buf_b = [0u8; 3];
        a.read_at(&mut buf_a, 0).unwrap();
        b.read_at(&mut buf_b, 7).unwrap();
        assert_eq!(&buf_a, b"012");
        assert_eq!(&buf_b, b"789");
    }
}
