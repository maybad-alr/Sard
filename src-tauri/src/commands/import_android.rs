//! Adapt only Android book-picker content URIs, leaving every ordinary path on the old pipeline.

use std::path::Path;
use tauri::{Manager, Runtime};
use tauri_plugin_fs::{FilePath, FsExt};

use super::import_cache::{safe_filename, CachedBook};
use crate::books;

pub(super) fn import_books<R: Runtime>(
    app: &tauri::AppHandle<R>,
    conn: &rusqlite::Connection,
    app_data_dir: &Path,
    paths: &[String],
) -> Vec<books::ImportResult> {
    paths.iter().map(|path| {
        // Do not reinterpret ordinary paths (even on Android) as URLs.
        if !path.get(..10).is_some_and(|prefix| prefix.eq_ignore_ascii_case("content://")) {
            return books::import_books(conn, app_data_dir, std::slice::from_ref(path)).remove(0);
        }
        let mut name = "Imported book".to_string();
        let staged = (|| -> Result<CachedBook, String> {
            let uri = tauri::Url::parse(path).map_err(|e| format!("Invalid document URI: {e}"))?;
            name = uri_filename(&uri);
            let cache = app.path().app_cache_dir().map_err(|e| format!("Cache unavailable: {e}"))?;
            // Fs 2.5.1 exposes read() -> Vec<u8>, not a stream or a metadata/display-name API.
            // It opens content URIs via ContentResolver. Drop this buffer before the importer
            // reads the staged file, so two whole-book buffers are not retained together.
            let bytes = app.fs().read(FilePath::Url(uri))
                .map_err(|e| format!("Couldn't read selected document: {e}"))?;
            CachedBook::copy_from(&cache, Some(&name), bytes.as_slice())
                .map_err(|e| format!("Couldn't cache selected document: {e}"))
        })();
        match staged {
            Ok(staged) => {
                // Drop removes the private temp directory on imported/duplicate/unsupported/error.
                // The existing importer records only its permanent managed path.
                books::import_books(conn, app_data_dir, &[staged.path().to_string_lossy().into_owned()]).remove(0)
            }
            Err(message) => books::ImportResult {
                id: String::new(), title: name, status: "error".into(), message: Some(message),
            },
        }
    }).collect()
}

fn uri_filename(uri: &tauri::Url) -> String {
    // Some providers include a filename in the document ID; others expose only an opaque number.
    // Do not present the number as a title. Embedded EPUB metadata still wins in the importer.
    let segment = uri.path_segments().and_then(|mut parts| parts.next_back()).unwrap_or("");
    let decoded = percent_encoding::percent_decode_str(segment).decode_utf8_lossy();
    let name = safe_filename(Some(&decoded));
    match Path::new(&name).extension().and_then(|ext| ext.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("epub") || ext.eq_ignore_ascii_case("pdf") => name,
        _ => "Imported book".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn document_names_are_decoded_and_opaque_ids_get_a_fallback() {
        let named = tauri::Url::parse("content://provider/document/primary%3ABooks%2FA%20book.pdf").unwrap();
        assert_eq!(uri_filename(&named), "A book.pdf");
        let opaque = tauri::Url::parse("content://provider/document/42").unwrap();
        assert_eq!(uri_filename(&opaque), "Imported book");
    }
}
