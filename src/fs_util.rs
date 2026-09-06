use std::path::{Path, PathBuf};

pub fn content_type_for(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();

    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "json" => "application/json",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "txt" => "text/plain; charset=utf-8",
        "pdf" => "application/pdf",
        "xml" => "application/xml",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "zip" => "application/zip",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

/// Joins `root` with a request path and confines the result to that root.
/// The segment pass rejects textual traversal, while the canonical-path
/// check also catches symlinks that point outside the configured tree.
///
/// The final path does not have to exist: uploads and missing-resource
/// lookups are checked through their nearest existing ancestor instead.
pub fn safe_join(root: &str, request_path: &str) -> Option<PathBuf> {
    let stack = sanitize_segments(request_path)?;
    let mut result = PathBuf::from(root);
    for s in stack {
        result.push(s);
    }
    path_is_confined(Path::new(root), &result).then_some(result)
}

/// Confirms that `candidate`, after resolving symlinks, is below `root`.
/// For a path that does not exist yet, walk upward to its nearest existing
/// ancestor. This makes the same guard usable for reads and new uploads.
fn path_is_confined(root: &Path, candidate: &Path) -> bool {
    let canonical_root = match std::fs::canonicalize(root) {
        Ok(path) => path,
        Err(_) => return false,
    };

    let mut existing = candidate;
    loop {
        match std::fs::symlink_metadata(existing) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                existing = match existing.parent() {
                    Some(parent) => parent,
                    None => return false,
                };
            }
            Err(_) => return false,
        }
    }

    std::fs::canonicalize(existing)
        .map(|path| path.starts_with(canonical_root))
        .unwrap_or(false)
}

/// Walks `request_path` into the segment stack `safe_join` builds a path
/// from, without actually joining it to a root yet — exposed so callers
/// that need to inspect each intermediate prefix (e.g. hunting for a CGI
/// script somewhere along a PATH_INFO-bearing URL) can reuse the same
/// traversal guard instead of re-implementing it.
pub fn sanitize_segments(request_path: &str) -> Option<Vec<&str>> {
    let mut stack: Vec<&str> = Vec::new();
    for segment in request_path.split('/') {
        match segment {
            "" | "." => continue,
            ".." => {
                stack.pop()?;
            }
            s => stack.push(s),
        }
    }
    Some(stack)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn joins_simple_path() {
        let root = tempdir("simple");
        assert_eq!(
            safe_join(root.to_str().unwrap(), "/index.html"),
            Some(root.join("index.html"))
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_traversal_above_root() {
        assert_eq!(safe_join("/var/www", "/../../etc/passwd"), None);
    }

    #[test]
    fn allows_traversal_that_stays_inside_root() {
        let root = tempdir("inside-traversal");
        assert_eq!(
            safe_join(root.to_str().unwrap(), "/a/../b.html"),
            Some(root.join("b.html"))
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn content_type_defaults_to_octet_stream() {
        assert_eq!(
            content_type_for(Path::new("file.bin")),
            "application/octet-stream"
        );
    }

    #[test]
    fn content_type_recognizes_html() {
        assert_eq!(
            content_type_for(Path::new("index.html")),
            "text/html; charset=utf-8"
        );
    }

    #[test]
    fn accepts_missing_path_below_existing_root() {
        let root = tempdir("missing");
        assert_eq!(
            safe_join(root.to_str().unwrap(), "/new/file.txt"),
            Some(root.join("new/file.txt"))
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rejects_symlink_to_file_outside_root() {
        let root = tempdir("root");
        let outside = tempdir("outside");
        let secret = outside.join("secret.txt");
        std::fs::write(&secret, b"outside").unwrap();
        symlink(&secret, root.join("escape.txt")).unwrap();

        assert_eq!(safe_join(root.to_str().unwrap(), "/escape.txt"), None);

        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    #[test]
    fn rejects_missing_path_below_symlinked_directory() {
        let root = tempdir("root-dir-link");
        let outside = tempdir("outside-dir-link");
        symlink(&outside, root.join("escape")).unwrap();

        assert_eq!(safe_join(root.to_str().unwrap(), "/escape/new.txt"), None);

        std::fs::remove_dir_all(root).unwrap();
        std::fs::remove_dir_all(outside).unwrap();
    }

    fn tempdir(label: &str) -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "localhost-fs-test-{}-{}-{}",
            std::process::id(),
            label,
            id
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }
}
