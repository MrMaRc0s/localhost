use std::fmt;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::fs_util::safe_join;
use crate::http::Request;

pub struct UploadedFile {
    pub filename: String,
    pub data: Vec<u8>,
}

#[derive(Debug)]
pub enum UploadError {
    BadRequest(String),
    Forbidden(String),
    Io(String),
}

impl fmt::Display for UploadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::BadRequest(message) | Self::Forbidden(message) | Self::Io(message) => message,
        };
        f.write_str(message)
    }
}

/// Saves a POST body into `store_dir`, understanding both a browser's
/// `multipart/form-data` file input and a plain `curl --data-binary`
/// style upload. Returns the list of files actually written to disk and
/// classifies failures so the router can choose the correct HTTP status.
pub fn save(store_dir: &str, remainder: &str, req: &Request) -> Result<Vec<String>, UploadError> {
    std::fs::create_dir_all(store_dir)
        .map_err(|error| classify_io_error("could not create upload directory", error))?;

    let content_type = req.header("content-type").unwrap_or("");
    let media_type = content_type.split(';').next().unwrap_or("").trim();
    if media_type.eq_ignore_ascii_case("multipart/form-data") {
        let files = parse_multipart(content_type, &req.body).ok_or_else(|| {
            UploadError::BadRequest("malformed multipart/form-data body".to_string())
        })?;
        if files.is_empty() {
            return Err(UploadError::BadRequest(
                "multipart body contained no file part".to_string(),
            ));
        }
        let mut saved = Vec::new();
        for file in files {
            let name = sanitize_filename(&file.filename);
            let dest = safe_join(store_dir, &name).ok_or_else(|| {
                UploadError::Forbidden(
                    "upload destination escapes its configured directory".to_string(),
                )
            })?;
            write_upload(&dest, &file.data)?;
            saved.push(name);
        }
        Ok(saved)
    } else {
        let name = remainder
            .rsplit('/')
            .find(|s| !s.is_empty())
            .map(sanitize_filename)
            .unwrap_or_else(generated_filename);
        let dest = safe_join(store_dir, &name).ok_or_else(|| {
            UploadError::Forbidden(
                "upload destination escapes its configured directory".to_string(),
            )
        })?;
        write_upload(&dest, &req.body)?;
        Ok(vec![name])
    }
}

fn write_upload(path: &Path, contents: &[u8]) -> Result<(), UploadError> {
    // safe_join checks the path as it exists now. O_NOFOLLOW closes the
    // final-component symlink race between that check and opening the file.
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| classify_io_error("could not open upload destination", error))?;

    file.write_all(contents)
        .map_err(|error| classify_io_error("could not write upload", error))
}

fn classify_io_error(context: &str, error: io::Error) -> UploadError {
    let message = format!("{}: {}", context, error);
    let raw_error = error.raw_os_error();
    if error.kind() == io::ErrorKind::PermissionDenied
        || raw_error == Some(libc::ELOOP)
        || raw_error == Some(libc::EISDIR)
    {
        UploadError::Forbidden(message)
    } else {
        UploadError::Io(message)
    }
}

fn generated_filename() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("upload-{}.bin", nanos)
}

/// Collapses a client-supplied filename down to just its final path
/// component, so `multipart` filenames like `../../etc/passwd` (some
/// browsers send the full client-side path) can never escape the upload
/// directory.
fn sanitize_filename(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("");
    if base.is_empty() || base == "." || base == ".." || base.chars().any(char::is_control) {
        generated_filename()
    } else {
        base.to_string()
    }
}

fn parse_multipart(content_type: &str, body: &[u8]) -> Option<Vec<UploadedFile>> {
    let boundary = extract_boundary(content_type)?;
    let delimiter = format!("--{}", boundary).into_bytes();

    let mut files = Vec::new();
    let mut cursor = find_delimiter(body, &delimiter, 0)?;

    loop {
        cursor += delimiter.len();
        if body.get(cursor..cursor + 2) == Some(b"--") {
            let after_close = cursor + 2;
            if after_close == body.len() || body.get(after_close..after_close + 2) == Some(b"\r\n")
            {
                return Some(files);
            }
            return None;
        }
        if body.get(cursor..cursor + 2) != Some(b"\r\n") {
            return None;
        }
        cursor += 2;

        let header_end = find_bytes(&body[cursor..], b"\r\n\r\n")? + cursor;
        if header_end - cursor > 16 * 1024 {
            return None;
        }
        let header_block = std::str::from_utf8(&body[cursor..header_end]).ok()?;

        let filename = header_block
            .split("\r\n")
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                if name.trim().eq_ignore_ascii_case("content-disposition") {
                    extract_parameter(value, "filename")
                } else {
                    None
                }
            })
            .filter(|name| !name.is_empty());

        let data_start = header_end + 4;
        let next_delim = find_delimiter(body, &delimiter, data_start)?;
        if next_delim < 2 || body.get(next_delim - 2..next_delim) != Some(b"\r\n") {
            return None;
        }
        let data_end = next_delim - 2;

        if let Some(filename) = filename {
            files.push(UploadedFile {
                filename,
                data: body[data_start..data_end].to_vec(),
            });
        }

        cursor = next_delim;
    }
}

fn extract_boundary(content_type: &str) -> Option<String> {
    let boundary = extract_parameter(content_type, "boundary")?;
    let bytes = boundary.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.len() <= 70
        && bytes.last() != Some(&b' ')
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"'()+_,-./:=? ".contains(byte));
    valid.then_some(boundary)
}

fn extract_parameter(value: &str, wanted: &str) -> Option<String> {
    for field in split_parameter_fields(value)? {
        let Some((name, raw_value)) = field.trim().split_once('=') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case(wanted) {
            return decode_parameter_value(raw_value.trim());
        }
    }
    None
}

/// Splits a MIME-style parameter list without treating semicolons inside
/// quoted values (for example `filename="quarter;final.txt"`) as separators.
fn split_parameter_fields(value: &str) -> Option<Vec<&str>> {
    let mut fields = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;

    for (index, character) in value.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match character {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ';' if !quoted => {
                fields.push(&value[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    if quoted || escaped {
        return None;
    }
    fields.push(&value[start..]);
    Some(fields)
}

fn decode_parameter_value(raw: &str) -> Option<String> {
    if !raw.starts_with('"') {
        return (!raw.is_empty() && !raw.contains('"')).then(|| raw.to_string());
    }
    let inner = raw.strip_prefix('"')?.strip_suffix('"')?;
    let mut decoded = String::new();
    let mut characters = inner.chars();
    while let Some(character) = characters.next() {
        if character == '\\' {
            let escaped = characters.next()?;
            if escaped == '\\' || escaped == '"' {
                decoded.push(escaped);
            } else {
                // Browsers have historically sent Windows paths using
                // literal backslashes rather than MIME quoted-pairs.
                decoded.push('\\');
                decoded.push(escaped);
            }
        } else {
            decoded.push(character);
        }
    }
    Some(decoded)
}

fn find_delimiter(body: &[u8], delimiter: &[u8], start: usize) -> Option<usize> {
    let mut search_from = start;
    while search_from <= body.len() {
        let relative = find_bytes(body.get(search_from..)?, delimiter)?;
        let position = search_from + relative;
        let line_start =
            position == 0 || (position >= 2 && body.get(position - 2..position) == Some(b"\r\n"));
        let after = position.checked_add(delimiter.len())?;
        let regular = body.get(after..after + 2) == Some(b"\r\n");
        let closing = body.get(after..after + 2) == Some(b"--")
            && (after + 2 == body.len() || body.get(after + 2..after + 4) == Some(b"\r\n"));
        let valid_end = regular || closing;
        if line_start && valid_end {
            return Some(position);
        }
        search_from = position.checked_add(1)?;
    }
    None
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
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => DeleteOutcome::NotFound,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => DeleteOutcome::Forbidden,
        Err(e) => DeleteOutcome::Error(e.to_string()),
        Ok(meta) if meta.is_dir() => DeleteOutcome::Forbidden,
        Ok(_) => match std::fs::remove_file(fs_path) {
            Ok(()) => DeleteOutcome::Deleted,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => DeleteOutcome::NotFound,
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
    use std::os::unix::fs::symlink;

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
        let req = req_with(
            multipart_body(boundary, "photo.txt", contents),
            &format!("multipart/form-data; boundary={}", boundary),
        );

        let dir =
            std::env::temp_dir().join(format!("localhost-upload-test-{}", std::process::id()));
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
        let dir =
            std::env::temp_dir().join(format!("localhost-upload-test-raw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let saved = save(dir.to_str().unwrap(), "/note.txt", &req).unwrap();
        assert_eq!(saved, vec!["note.txt".to_string()]);
        assert_eq!(
            std::fs::read(dir.join("note.txt")).unwrap(),
            b"hello raw upload"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_path_traversal_in_multipart_filename() {
        let boundary = "----b";
        let req = req_with(
            multipart_body(boundary, "../../etc/passwd", b"pwned"),
            &format!("multipart/form-data; boundary={}", boundary),
        );
        let dir =
            std::env::temp_dir().join(format!("localhost-upload-test-trav-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        let saved = save(dir.to_str().unwrap(), "", &req).unwrap();
        // sanitized down to just "passwd", written *inside* dir, not at /etc/passwd
        assert_eq!(saved, vec!["passwd".to_string()]);
        assert!(dir.join("passwd").exists());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn preserves_boundary_like_bytes_inside_file() {
        let boundary = "----inside-check";
        let contents = b"before--inside-check-after\r\n------inside-check-not-a-delimiter";
        let body = multipart_body(boundary, "payload.bin", contents);

        let files = parse_multipart(
            &format!("multipart/form-data; boundary={}", boundary),
            &body,
        )
        .unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].data, contents);
    }

    #[test]
    fn handles_quoted_case_insensitive_boundary_and_windows_filename() {
        let boundary = "BrowserBoundary";
        let req = req_with(
            multipart_body(boundary, r"C:\fakepath\quarter;final.txt", b"report"),
            &format!("Multipart/Form-Data; BOUNDARY=\"{}\"", boundary),
        );
        let dir = std::env::temp_dir().join(format!(
            "localhost-upload-test-windows-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        let saved = save(dir.to_str().unwrap(), "", &req).unwrap();

        assert_eq!(saved, vec!["quarter;final.txt".to_string()]);
        assert_eq!(
            std::fs::read(dir.join("quarter;final.txt")).unwrap(),
            b"report"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_missing_closing_delimiter() {
        let body = b"--unfinished\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n\r\ndata";
        assert!(parse_multipart("multipart/form-data; boundary=unfinished", body).is_none());
    }

    #[test]
    fn rejects_invalid_boundaries() {
        assert!(extract_boundary("multipart/form-data; boundary=").is_none());
        assert!(extract_boundary("multipart/form-data; boundary=has@invalid").is_none());
        let too_long = "a".repeat(71);
        assert!(extract_boundary(&format!("multipart/form-data; boundary={too_long}")).is_none());
        assert!(parse_multipart("multipart/form-data; boundary=b", b"x--b--").is_none());
    }

    #[test]
    fn refuses_to_overwrite_file_through_symlink() {
        let req = req_with(b"replacement".to_vec(), "text/plain");
        let dir =
            std::env::temp_dir().join(format!("localhost-upload-test-link-{}", std::process::id()));
        let outside = std::env::temp_dir().join(format!(
            "localhost-upload-test-outside-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&outside);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&outside, b"keep me").unwrap();
        symlink(&outside, dir.join("note.txt")).unwrap();

        let result = save(dir.to_str().unwrap(), "/note.txt", &req);
        assert!(matches!(result, Err(UploadError::Forbidden(_))));
        assert_eq!(std::fs::read(&outside).unwrap(), b"keep me");

        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_file(outside).unwrap();
    }

    #[test]
    fn delete_reports_not_found_for_missing_file() {
        let path = std::env::temp_dir().join("localhost-does-not-exist-xyz");
        assert!(matches!(delete(&path), DeleteOutcome::NotFound));
    }

    #[test]
    fn delete_removes_existing_file() {
        let path =
            std::env::temp_dir().join(format!("localhost-delete-test-{}", std::process::id()));
        std::fs::write(&path, b"bye").unwrap();
        assert!(matches!(delete(&path), DeleteOutcome::Deleted));
        assert!(!path.exists());
    }
}
