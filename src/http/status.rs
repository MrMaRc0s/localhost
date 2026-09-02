/// Maps an HTTP status code to its standard reason phrase (RFC 9110 §15).
///
/// We only need the codes this server actually emits; anything else falls
/// back to a generic phrase so a response is still well-formed.
pub fn reason_phrase(code: u16) -> &'static str {
    match code {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Content Too Large",
        414 => "URI Too Long",
        415 => "Unsupported Media Type",
        417 => "Expectation Failed",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        505 => "HTTP Version Not Supported",
        _ if code < 200 => "Continue",
        _ if code < 300 => "OK",
        _ if code < 400 => "Multiple Choices",
        _ if code < 500 => "Bad Request",
        _ => "Internal Server Error",
    }
}

pub fn is_client_error(code: u16) -> bool {
    (400..500).contains(&code)
}

pub fn is_server_error(code: u16) -> bool {
    (500..600).contains(&code)
}
