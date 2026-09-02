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

/// Joins `root` with a request path, refusing to let `..` segments escape
/// above `root`. Textual traversal (`/../../etc/passwd`) is rejected by
/// construction: a `..` can only cancel out a path segment we already
/// pushed while walking `request_path`, it can never dip into `root`
/// itself. (Actual on-disk symlinks under `root` are still followed by
/// the OS afterwards — that's ordinary filesystem behavior, not something
/// this function is responsible for.)
pub fn safe_join(root: &str, request_path: &str) -> Option<PathBuf> {
    let stack = sanitize_segments(request_path)?;
    let mut result = PathBuf::from(root);
    for s in stack {
        result.push(s);
    }
    Some(result)
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

    #[test]
    fn joins_simple_path() {
        assert_eq!(safe_join("/var/www", "/index.html"), Some(PathBuf::from("/var/www/index.html")));
    }

    #[test]
    fn rejects_traversal_above_root() {
        assert_eq!(safe_join("/var/www", "/../../etc/passwd"), None);
    }

    #[test]
    fn allows_traversal_that_stays_inside_root() {
        assert_eq!(
            safe_join("/var/www", "/a/../b.html"),
            Some(PathBuf::from("/var/www/b.html"))
        );
    }

    #[test]
    fn content_type_defaults_to_octet_stream() {
        assert_eq!(content_type_for(Path::new("file.bin")), "application/octet-stream");
    }

    #[test]
    fn content_type_recognizes_html() {
        assert_eq!(content_type_for(Path::new("index.html")), "text/html; charset=utf-8");
    }
}
