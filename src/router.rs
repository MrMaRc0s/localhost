use std::path::{Path, PathBuf};

use crate::config::{RouteConfig, ServerConfig};
use crate::fs_util::{content_type_for, safe_join, sanitize_segments};
use crate::http::{Method, Request, Response};

/// What the connection layer should do with a request once routing has
/// decided. Everything filesystem-only (static files, directory listing,
/// redirects, method errors) resolves synchronously to a `Response` right
/// here — local disk I/O doesn't need epoll. A CGI match is the one case
/// that needs a child process, so it's handed back unexecuted for the
/// event loop to run through its own epoll-registered pipes.
pub enum Dispatch {
    Response(Response),
    Cgi(CgiRequest),
}

pub struct CgiRequest {
    pub interpreter: String,
    pub script_path: PathBuf,
    /// Directory the CGI process should be spawned in, so relative paths
    /// it opens resolve the way the script author expects.
    pub script_dir: PathBuf,
    /// Extra path after the script name (PATH_INFO per the CGI/1.1 spec),
    /// e.g. `/cgi-bin/app.py/foo/bar` -> script `app.py`, PATH_INFO `/foo/bar`.
    pub path_info: String,
}

pub fn dispatch(server: &ServerConfig, req: &Request) -> Dispatch {
    let route = match server.find_route(&req.path) {
        Some(r) => r,
        None => return Dispatch::Response(super::error_page::build(Some(server), 404)),
    };

    if let Some((code, target)) = &route.redirect {
        return Dispatch::Response(Response::redirect(*code, target));
    }

    if !route.allows(req.method) {
        let allow = route.methods.iter().map(Method::as_str).collect::<Vec<_>>().join(", ");
        let resp = super::error_page::build(Some(server), 405).header("Allow", allow);
        return Dispatch::Response(resp);
    }

    let base_dir = route.root.as_deref().unwrap_or(&server.root);
    let remainder = strip_route_prefix(&route.path, &req.path);

    // DELETE never falls through to directory-index / autoindex handling —
    // it either removes the exact resource named or it doesn't.
    if req.method == Method::Delete {
        let fs_path = match safe_join(base_dir, &remainder) {
            Some(p) => p,
            None => return Dispatch::Response(super::error_page::build(Some(server), 403)),
        };
        return resolve_delete(server, &fs_path);
    }

    // A CGI match can be the exact resource (`/cgi-bin/app.py`) or carry
    // a PATH_INFO tail (`/cgi-bin/app.py/foo/bar`) that would never stat()
    // as a real file, so this walks path segments looking for the first
    // one that's an actual file with a configured CGI extension — same
    // traversal guard as safe_join, just applied one prefix at a time.
    if let Some((interpreter, script_path, path_info)) = find_cgi_script(route, base_dir, &remainder) {
        if matches!(req.method, Method::Get | Method::Head | Method::Post) {
            return Dispatch::Cgi(CgiRequest {
                interpreter,
                script_dir: script_path.parent().unwrap_or(Path::new(".")).to_path_buf(),
                path_info,
                script_path,
            });
        }
    }

    if req.method == Method::Post {
        return resolve_upload(server, route, base_dir, &remainder, req);
    }

    let fs_path = match safe_join(base_dir, &remainder) {
        Some(p) => p,
        None => return Dispatch::Response(super::error_page::build(Some(server), 403)),
    };
    resolve_path(server, route, &fs_path, &remainder, req)
}

/// Walks `remainder`'s path segments one at a time looking for the
/// shortest prefix that names a real file whose extension is configured
/// for CGI on this route. Everything past that point becomes PATH_INFO.
/// Returns `None` (not a traversal-invalid path, just "no CGI script
/// found here") the moment a segment either doesn't exist or exists but
/// isn't a directory to keep descending into.
fn find_cgi_script(route: &RouteConfig, base_dir: &str, remainder: &str) -> Option<(String, PathBuf, String)> {
    if route.cgi.is_empty() {
        return None;
    }
    let segments = sanitize_segments(remainder)?;

    let mut candidate = PathBuf::from(base_dir);
    for (i, segment) in segments.iter().enumerate() {
        candidate.push(segment);

        let meta = std::fs::metadata(&candidate).ok()?;
        if meta.is_file() {
            let ext = candidate.extension().and_then(|e| e.to_str())?;
            let interpreter = route.cgi.get(&format!(".{}", ext))?;
            let path_info = if i + 1 < segments.len() { format!("/{}", segments[i + 1..].join("/")) } else { String::new() };
            return Some((interpreter.clone(), candidate, path_info));
        }
        // A file that isn't a recognized CGI script ends the search
        // (can't descend "into" a file); a directory just keeps going.
        if !meta.is_dir() {
            return None;
        }
    }
    None
}

fn resolve_delete(server: &ServerConfig, fs_path: &Path) -> Dispatch {
    use crate::upload::DeleteOutcome;
    let resp = match crate::upload::delete(fs_path) {
        DeleteOutcome::Deleted => Response::no_content(204),
        DeleteOutcome::NotFound => super::error_page::build(Some(server), 404),
        DeleteOutcome::Forbidden => super::error_page::build(Some(server), 403),
        DeleteOutcome::Error(reason) => {
            eprintln!("localhost: DELETE {}: {}", fs_path.display(), reason);
            super::error_page::build(Some(server), 500)
        }
    };
    Dispatch::Response(resp)
}

fn resolve_upload(server: &ServerConfig, route: &RouteConfig, base_dir: &str, remainder: &str, req: &Request) -> Dispatch {
    let store_dir = route.upload_store.as_deref().unwrap_or(base_dir);
    let resp = match crate::upload::save(store_dir, remainder, req) {
        Ok(saved) => {
            let list = saved.join(", ");
            Response::html(201, format!("<p>Uploaded: {}</p>", html_escape(&list)))
                .header("Location", req.path.clone())
        }
        Err(reason) => {
            eprintln!("localhost: upload to {} failed: {}", store_dir, reason);
            super::error_page::build(Some(server), 400)
        }
    };
    Dispatch::Response(resp)
}

/// The portion of the request path beyond the matched route's prefix,
/// e.g. route `/uploads`, request `/uploads/a/b.txt` -> `/a/b.txt`.
fn strip_route_prefix(route_path: &str, request_path: &str) -> String {
    if route_path == "/" {
        return request_path.to_string();
    }
    request_path.strip_prefix(route_path).unwrap_or("").to_string()
}

fn resolve_path(server: &ServerConfig, route: &RouteConfig, fs_path: &Path, remainder: &str, req: &Request) -> Dispatch {
    let meta = match std::fs::metadata(fs_path) {
        Ok(m) => m,
        Err(_) => return Dispatch::Response(super::error_page::build(Some(server), 404)),
    };

    if meta.is_dir() {
        let index_name = route.index.as_deref().or(server.index.as_deref());
        if let Some(index_name) = index_name {
            let index_path = fs_path.join(index_name);
            if index_path.is_file() {
                return serve_file_or_cgi(server, route, &index_path, remainder, req);
            }
        }
        if route.autoindex {
            return Dispatch::Response(autoindex(fs_path, &req.path));
        }
        return Dispatch::Response(super::error_page::build(Some(server), 403));
    }

    serve_file_or_cgi(server, route, fs_path, remainder, req)
}

fn serve_file_or_cgi(server: &ServerConfig, route: &RouteConfig, file_path: &Path, remainder: &str, req: &Request) -> Dispatch {
    if let Some(ext) = file_path.extension().and_then(|e| e.to_str()) {
        let dotted = format!(".{}", ext);
        if let Some(interpreter) = route.cgi.get(&dotted) {
            return Dispatch::Cgi(CgiRequest {
                interpreter: interpreter.clone(),
                script_dir: file_path.parent().unwrap_or(Path::new(".")).to_path_buf(),
                path_info: path_info_after_script(file_path, remainder),
                script_path: file_path.to_path_buf(),
            });
        }
    }

    match req.method {
        Method::Get | Method::Head => Dispatch::Response(serve_static_file(server, file_path)),
        _ => Dispatch::Response(super::error_page::build(Some(server), 501)),
    }
}

/// If the matched filesystem entry is the CGI script itself, PATH_INFO is
/// empty; if the route mapped e.g. `/cgi-bin/app.py/x/y`, PATH_INFO is
/// `/x/y`. We don't have to walk the filesystem to find this split since
/// `resolve_path` already found a real file at `file_path` — anything in
/// `remainder` past that file's own name is the PATH_INFO tail.
fn path_info_after_script(file_path: &Path, remainder: &str) -> String {
    let script_name = file_path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    match remainder.find(script_name) {
        Some(pos) => remainder[pos + script_name.len()..].to_string(),
        None => String::new(),
    }
}

pub fn serve_static_file(server: &ServerConfig, path: &Path) -> Response {
    match std::fs::read(path) {
        Ok(contents) => Response::new(200).header("Content-Type", content_type_for(path)).body(contents),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => super::error_page::build(Some(server), 403),
        Err(_) => super::error_page::build(Some(server), 404),
    }
}

fn autoindex(dir: &Path, request_path: &str) -> Response {
    let mut entries: Vec<String> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).filter_map(|e| e.file_name().into_string().ok()).collect(),
        Err(_) => Vec::new(),
    };
    entries.sort();

    let base = if request_path.ends_with('/') { request_path.to_string() } else { format!("{}/", request_path) };

    let mut rows = String::new();
    if request_path != "/" {
        rows.push_str("<tr><td><a href=\"../\">../</a></td></tr>\n");
    }
    for name in &entries {
        let is_dir = dir.join(name).is_dir();
        let suffix = if is_dir { "/" } else { "" };
        rows.push_str(&format!(
            "<tr><td><a href=\"{name}{suffix}\">{name}{suffix}</a></td></tr>\n",
            name = html_escape(name),
            suffix = suffix
        ));
    }

    let body = format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head><meta charset="utf-8"><title>Index of {path}</title>
<style>
  body {{ font-family: -apple-system, sans-serif; background:#14161a; color:#e8e8e8; padding:2rem; }}
  h1 {{ font-weight: 500; font-size: 1.3rem; }}
  a {{ color: #6cb2ff; text-decoration: none; }}
  a:hover {{ text-decoration: underline; }}
  table {{ border-collapse: collapse; }}
  td {{ padding: 0.2rem 0.6rem 0.2rem 0; }}
</style></head>
<body>
<h1>Index of {path}</h1>
<table>
{rows}</table>
</body>
</html>
"#,
        path = html_escape(&base),
        rows = rows
    );

    Response::html(200, body)
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RouteConfig;
    use std::collections::HashMap;

    fn server_with_root(root: &str) -> ServerConfig {
        ServerConfig {
            listens: Vec::new(),
            server_names: Vec::new(),
            root: root.to_string(),
            index: Some("index.html".to_string()),
            client_max_body_size: 1024,
            error_pages: HashMap::new(),
            routes: vec![RouteConfig::new("/")],
        }
    }

    fn req(method: Method, path: &str) -> Request {
        Request {
            method,
            raw_method: method.as_str().to_string(),
            target: path.to_string(),
            path: path.to_string(),
            query: None,
            version: (1, 1),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    #[test]
    fn serves_index_for_directory_root() {
        let dir = tempdir();
        std::fs::write(dir.join("index.html"), b"<h1>hi</h1>").unwrap();
        let server = server_with_root(dir.to_str().unwrap());

        match dispatch(&server, &req(Method::Get, "/")) {
            Dispatch::Response(resp) => {
                assert_eq!(resp.status, 200);
                assert_eq!(resp.body, b"<h1>hi</h1>");
            }
            _ => panic!("expected a static response"),
        }
        cleanup(&dir);
    }

    #[test]
    fn missing_file_is_404() {
        let dir = tempdir();
        let server = server_with_root(dir.to_str().unwrap());
        match dispatch(&server, &req(Method::Get, "/nope.html")) {
            Dispatch::Response(resp) => assert_eq!(resp.status, 404),
            _ => panic!("expected a static response"),
        }
        cleanup(&dir);
    }

    #[test]
    fn disallowed_method_is_405() {
        let dir = tempdir();
        std::fs::write(dir.join("index.html"), b"hi").unwrap();
        let mut server = server_with_root(dir.to_str().unwrap());
        server.routes = vec![RouteConfig::new("/")]; // default methods: GET, HEAD
        match dispatch(&server, &req(Method::Delete, "/")) {
            Dispatch::Response(resp) => assert_eq!(resp.status, 405),
            _ => panic!("expected a static response"),
        }
        cleanup(&dir);
    }

    #[test]
    fn traversal_outside_root_is_rejected() {
        let dir = tempdir();
        let server = server_with_root(dir.to_str().unwrap());
        match dispatch(&server, &req(Method::Get, "/../../etc/passwd")) {
            Dispatch::Response(resp) => assert_eq!(resp.status, 403),
            _ => panic!("expected a static response"),
        }
        cleanup(&dir);
    }

    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("localhost-router-test-{}-{}", std::process::id(), id));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cleanup(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
    }
}
