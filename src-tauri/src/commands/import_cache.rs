//! Private cache custody for Android document imports. The importer still owns format detection,
//! deduplication and the permanent copy; this directory exists only for the duration of one import.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_IMPORT: AtomicU64 = AtomicU64::new(0);

pub(super) fn safe_filename(name: Option<&str>) -> String {
    let name = name.unwrap_or("").rsplit(['/', '\\']).next().unwrap_or("");
    if name.trim().is_empty()
        || name == "."
        || name == ".."
        || name.len() > 240
        || name.chars().any(|c| c.is_control() || matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|'))
    {
        "Imported book".into()
    } else {
        name.to_string()
    }
}

pub(super) struct CachedBook {
    dir: PathBuf,
    path: PathBuf,
}

impl CachedBook {
    pub(super) fn copy_from(cache: &Path, name: Option<&str>, mut source: impl Read) -> io::Result<Self> {
        std::fs::create_dir_all(cache)?;
        let dir = loop {
            let sequence = NEXT_IMPORT.fetch_add(1, Ordering::Relaxed);
            let candidate = cache.join(format!("sard-import-{}-{sequence}", std::process::id()));
            match std::fs::create_dir(&candidate) {
                Ok(()) => break candidate,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        };
        // Establish cleanup before opening/writing, so partial copies are removed on errors too.
        let staged = Self { path: dir.join(safe_filename(name)), dir };
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&staged.path)?;
        io::copy(&mut source, &mut file)?;
        Ok(staged)
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for CachedBook {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.dir) {
            eprintln!("[Sard] could not remove temporary book import: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "sard-import-cache-test-{}-{}",
            std::process::id(),
            NEXT_IMPORT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn names_preserve_unicode_but_cannot_escape_cache() {
        assert_eq!(safe_filename(Some("كتاب جديد.pdf")), "كتاب جديد.pdf");
        assert_eq!(safe_filename(Some("../../book.epub")), "book.epub");
        assert_eq!(safe_filename(Some("C:\\books\\book.epub")), "book.epub");
        for name in [None, Some(""), Some(".."), Some("."), Some("bad\0.pdf"), Some("primary:42")] {
            assert_eq!(safe_filename(name), "Imported book");
        }
    }

    #[test]
    fn preserves_bytes_and_name_then_cleans_up() {
        let cache = cache();
        let path;
        {
            let staged = CachedBook::copy_from(&cache, Some("كتاب.pdf"), &b"%PDF-test"[..]).unwrap();
            path = staged.path().to_path_buf();
            assert_eq!(path.file_name().unwrap(), "كتاب.pdf");
            assert_eq!(std::fs::read(&path).unwrap(), b"%PDF-test");
        }
        assert!(!path.exists());
        assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 0);
        std::fs::remove_dir(cache).unwrap();
    }

    #[test]
    fn failed_copy_removes_partial_file() {
        struct Broken(bool);
        impl Read for Broken {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                if !self.0 && !buf.is_empty() {
                    self.0 = true;
                    buf[0] = b'x';
                    return Ok(1);
                }
                Err(io::Error::new(io::ErrorKind::Other, "provider disconnected"))
            }
        }
        let cache = cache();
        assert!(CachedBook::copy_from(&cache, Some("book.epub"), Broken(false)).is_err());
        assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 0);
        std::fs::remove_dir(cache).unwrap();
    }

    #[test]
    fn failed_import_cleans_up_and_identical_names_do_not_collide() {
        let cache = cache();
        let first = CachedBook::copy_from(&cache, Some("book.pdf"), &b"first"[..]).unwrap();
        let result = (|| -> Result<(), &'static str> {
            let second = CachedBook::copy_from(&cache, Some("book.pdf"), &b"second"[..]).unwrap();
            assert_ne!(first.path(), second.path());
            Err("importer rejected document")
        })();
        assert!(result.is_err());
        assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 1);
        assert_eq!(std::fs::read(first.path()).unwrap(), b"first");
        drop(first);
        assert_eq!(std::fs::read_dir(&cache).unwrap().count(), 0);
        std::fs::remove_dir(cache).unwrap();
    }
}
