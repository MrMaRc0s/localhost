use crate::config::ServerConfig;
use crate::fs_util::{content_type_for, safe_join};
use crate::http::status::reason_phrase;
use crate::http::Response;

/// Builds an error response for `code`. If `server` configured a custom
/// `error_page` for this status and the file is readable, that's served
/// verbatim; otherwise falls back to a minimal built-in page so every
/// status code we ever emit is human-readable, not just the ones a config
/// author remembered to override.
pub fn build(server: Option<&ServerConfig>, code: u16) -> Response {
    if let Some(server) = server {
        if let Some(configured_path) = server.error_page(code) {
            if let Some(full_path) = safe_join(&server.root, configured_path) {
                if let Ok(contents) = std::fs::read(&full_path) {
                    return Response::new(code)
                        .header("Content-Type", content_type_for(&full_path))
                        .body(contents);
                }
            }
        }
    }
    Response::html(code, default_page_html(code))
}

fn default_page_html(code: u16) -> String {
    let reason = reason_phrase(code);
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>{code} {reason}</title>
<style>
  html, body {{ height: 100%; margin: 0; }}
  body {{
    display: flex; align-items: center; justify-content: center;
    font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
    background: #14161a; color: #e8e8e8;
  }}
  .box {{ text-align: center; }}
  h1 {{ font-size: 5.5rem; margin: 0; color: #ff6b6b; letter-spacing: -2px; }}
  p {{ font-size: 1.1rem; color: #9aa0a6; margin-top: 0.25rem; }}
  hr {{ border: none; border-top: 1px solid #2a2d33; width: 8rem; margin: 1.5rem auto; }}
  small {{ color: #5f6368; }}
</style>
</head>
<body>
  <div class="box">
    <h1>{code}</h1>
    <p>{reason}</p>
    <hr>
    <small>localhost</small>
  </div>
</body>
</html>
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn falls_back_to_builtin_page_without_server() {
        let resp = build(None, 404);
        assert_eq!(resp.status, 404);
        let body = String::from_utf8(resp.body).unwrap();
        assert!(body.contains("404"));
        assert!(body.contains("Not Found"));
    }

    #[test]
    fn falls_back_when_custom_page_missing_on_disk() {
        let server = ServerConfig {
            listens: Vec::new(),
            server_names: Vec::new(),
            root: "/nonexistent-root-for-test".into(),
            index: None,
            client_max_body_size: 1024,
            error_pages: [(404u16, "/missing.html".to_string())].into_iter().collect(),
            routes: Vec::new(),
        };
        let resp = build(Some(&server), 404);
        assert_eq!(resp.status, 404);
        assert!(String::from_utf8(resp.body).unwrap().contains("404"));
    }
}
