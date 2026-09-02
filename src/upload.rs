use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::fs_util::safe_join;
use crate::http::Request;

pub struct UploadedFile {
    pub filename: String,
    pub data: Vec<u8>,
}

/// Saves a POST body into `store_dir`, understanding both a browser's
/// `multipart/form-data` file input and a plain `curl --data-binary`
/// style upload. Returns the list of files actually written to disk, or
/// `Err(message)` if nothing usable could be extracted from the body.
pub fn save(store_dir: &str, remainder: &str, req: &Request) -> Result<Vec<String>, String> {
    std::fs::create_dir_all(store_dir).map_err(|e| format!("could not create upload directory: {}", e))?;

    let content_type = req.header("content-type").unwrap_or("");
    if content_type.to_ascii_lowercase().starts_with("multipart/form-data") {
        let files = parse_multipart(content_type, &req.body).ok_or("malformed multipart/form-data body")?;
        if files.is_empty() {
            return Err("multipart body contained no file part".to_string());
        }
        let mut saved = Vec::new();
        for file in files {
            let name = sanitize_filename(&file.filename);
            let dest = safe_join(store_dir, &name).ok_or("invalid filename")?;
            std::fs::write(&dest, &file.data).map_err(|e| e.to_string())?;
            saved.push(name);
        }
        Ok(saved)
    } else {
        let name = remainder
            .rsplit('/')
            .find(|s| !s.is_empty())
            .map(sanitize_filename)
            .unwrap_or_else(generated_filename);
        let dest = safe_join(store_dir, &name).ok_or("invalid filename")?;
        std::fs::write(&dest, &req.body).map_err(|e| e.to_string())?;
        Ok(vec![name])
    }
}

fn generated_filename() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    format!("upload-{}.bin", nanos)
}

/// Collapses a client-supplied filename down to just its final path
/// component, so `multipart` filenames like `../../etc/passwd` (some
/// browsers send the full client-side path) can never escape the upload
/// directory.
fn sanitize_filename(raw: &str) -> String {
    let base = Path::new(raw).file_name().and_then(|n| n.to_str()).unwrap_or("");
    if base.is_empty() || base == "." || base == ".." {
        generated_filename()
    } else {
        base.to_string()
    }
}

fn parse_multipart(content_type: &str, body: &[u8]) -> Option<Vec<UploadedFile>> {
    let boundary = extract_boundary(content_type)?;
    let delimiter = format!("--{}", boundary).into_bytes();

    let mut files = Vec::new();
    let mut cursor = find_bytes(body, &delimiter)? + delimiter.len();

    loop {
        if body.get(cursor..cursor + 2) == Some(b"--") {
            break;
        }
        if body.get(cursor..cursor + 2) == Some(b"\r\n") {
            cursor += 2;
        }

        let header_end = find_bytes(&body[cursor..], b"\r\n\r\n")? + cursor;
        let header_block = std::str::from_utf8(&body[cursor..header_end]).ok()?;

        let filename = header_block
            .split("\r\n")
            .find(|line| line.to_ascii_lowercase().starts_with("content-disposition:"))
            .and_then(|line| extract_quoted(line, "filename"));

        let data_start = header_end + 4;
        let next_delim = find_bytes(&body[data_start..], &delimiter)? + data_start;
        let data_end = if next_delim >= 2 && &body[next_delim - 2..next_delim] == b"\r\n" {
            next_delim - 2
        } else {
            next_delim
        };

        if let Some(filename) = filename.filter(|f| !f.is_empty()) {
            files.push(UploadedFile { filename, data: body[data_start..data_end].to_vec() });
        }

        cursor = next_delim + delimiter.len();
        if body.get(cursor..cursor + 2) == Some(b"--") {
            break;
        }
    }

    Some(files)
}

fn extract_boundary(content_type: &str) -> Option<String> {
    content_type
        .split(';')
        .map(str::trim)
        .find_map(|p| p.strip_prefix("boundary="))
        .map(|b| b.trim_matches('"').to_string())
}

fn extract_quoted(line: &str, key: &str) -> Option<String> {
    let needle = format!("{}=\"", key);
    let start = line.find(&needle)? + needle.len();
    let end = line[start..].find('"')? + start;
    Some(line[start..end].to_string())
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

pub enum DeleteOutcome {
    Deleted,
    NotFound,
    Forbidden,
    Error(String),
}

pub fn delete(fs_path: &Path) -> DeleteOutcome {
    match std::fs::metadata(fs_path) {
        Err(_) => DeleteOutcome::NotFound,
        Ok(meta) if meta.is_dir() => DeleteOutcome::Forbidden,
        Ok(_) => match std::fs::remove_file(fs_path) {
            Ok(()) => DeleteOutcome::Deleted,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => DeleteOutcome::Forbidden,
            Err(e) => DeleteOutcome::Error(e.to_string()),
        },
    }
}

#[allow(dead_code)]
pub fn store_dir_for(route_store: Option<&str>, base_dir: &str) -> PathBuf {
    PathBuf::from(route_store.unwrap_or(base_dir))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Method;

    fn multipart_body(boundary: &str, filename: &str, contents: &[u8]) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(format!("--{}\r\n", boundary).as_bytes());
        body.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"file\"; filename=\"{}\"\r\nContent-Type: application/octet-stream\r\n\r\n",
                filename
            )
            .as_bytes(),
        );
        body.extend_from_slice(contents);
        body.extend_from_slice(format!("\r\n--{}--\r\n", boundary).as_bytes());
        body
    }

    fn req_with(body: Vec<u8>, content_type: &str) -> Request {
        Request {
            method: Method::Post,
            raw_method: "POST".into(),
            target: "/upload".into(),
            path: "/upload".into(),
            query: None,
            version: (1, 1),
            headers: vec![("content-type".into(), content_type.to_string())],
            body,
        }
    }

    #[test]
    fn saves_multipart_file_uncorrupted() {
        let boundary = "----boundaryXYZ";
        let contents = b"the quick brown fox";
        let req = req_with(multipart_body(boundary, "photo.txt", contents), &format!("multipart/form-data; boundary={}", boundary));

        let dir = std::env::temp_dir().join(format!("localhost-upload-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let saved = save(dir.to_str().unwrap(), "", &req).unwrap();
        assert_eq!(saved, vec!["photo.txt".to_string()]);
        let on_disk = std::fs::read(dir.join("photo.txt")).unwrap();
        assert_eq!(on_disk, contents);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn saves_raw_body_using_url_filename() {
        let req = req_with(b"hello raw upload".to_vec(), "text/plain");
        let dir = std::env::temp_dir().join(format!("localhost-upload-test-raw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let saved = save(dir.to_str().unwrap(), "/note.txt", &req).unwrap();
        assert_eq!(saved, vec!["note.txt".to_string()]);
        assert_eq!(std::fs::read(dir.join("note.txt")).unwrap(), b"hello raw upload");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_path_traversal_in_multipart_filename() {
        let boundary = "----b";
        let req = req_with(
            multipart_body(boundary, "../../etc/passwd", b"pwned"),
            &format!("multipart/form-data; boundary={}", boundary),
        );
        let dir = std::env::temp_dir().join(format!("localhost-upload-test-trav-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let saved = save(dir.to_str().unwrap(), "", &req).unwrap();
        // sanitized down to just "passwd", written *inside* dir, not at /etc/passwd
        assert_eq!(saved, vec!["passwd".to_string()]);
        assert!(dir.join("passwd").exists());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn delete_reports_not_found_for_missing_file() {
        let path = std::env::temp_dir().join("localhost-does-not-exist-xyz");
        assert!(matches!(delete(&path), DeleteOutcome::NotFound));
    }

    #[test]
    fn delete_removes_existing_file() {
        let path = std::env::temp_dir().join(format!("localhost-delete-test-{}", std::process::id()));
        std::fs::write(&path, b"bye").unwrap();
        assert!(matches!(delete(&path), DeleteOutcome::Deleted));
        assert!(!path.exists());
    }
}
