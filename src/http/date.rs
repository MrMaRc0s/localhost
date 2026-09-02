/// Formats the current time as an HTTP-date (IMF-fixdate, RFC 9110 §5.6.7),
/// e.g. `Tue, 15 Nov 1994 08:12:31 GMT`.
///
/// `std` has no civil-calendar conversion, so this reaches for libc's
/// `gmtime_r` — exactly the kind of syscall-adjacent helper the spec
/// allows `libc` for. The `unsafe` is confined to this one FFI call.
pub fn http_date_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format_http_date(now.as_secs() as libc::time_t)
}

pub fn format_http_date(secs: libc::time_t) -> String {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        libc::gmtime_r(&secs, &mut tm);
    }

    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        WEEKDAYS[(tm.tm_wday.max(0) as usize) % 7],
        tm.tm_mday,
        MONTHS[(tm.tm_mon.max(0) as usize) % 12],
        tm.tm_year + 1900,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
    )
}
