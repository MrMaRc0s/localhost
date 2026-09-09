use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const COOKIE_NAME: &str = "localhost_session";
/// A session nobody has touched in this long is considered abandoned and
/// swept — without this, a long-running server would accumulate one
/// entry per visitor forever.
pub const SESSION_TTL: Duration = Duration::from_secs(30 * 60);
/// A client that never returns its cookie must not grow the store forever.
/// This is intentionally a process-wide ceiling because the server itself
/// is single-process and sessions are not persisted anywhere else.
const MAX_SESSIONS: usize = 4096;
/// Evicting a batch amortizes the oldest-session scan during cookie-less
/// traffic instead of sorting the store for every request beyond the cap.
const SESSION_EVICTION_BATCH: usize = 256;

pub struct Session {
    pub last_seen: Instant,
    /// Small free-form store a route handler could use (not currently
    /// exposed to CGI beyond the visit counter below, but here so the
    /// session concept isn't a bare id with nothing behind it).
    pub data: HashMap<String, String>,
}

impl Session {
    fn new() -> Self {
        Session { last_seen: Instant::now(), data: HashMap::new() }
    }
}

pub struct SessionStore {
    sessions: HashMap<String, Session>,
}

impl SessionStore {
    pub fn new() -> Self {
        SessionStore { sessions: HashMap::new() }
    }

    /// Looks up `existing_id` (from the request's Cookie header, if any)
    /// and refreshes its `last_seen`, or mints a brand new session when
    /// there's no id, or the id is unknown/expired. Returns the session's
    /// id and whether it's new — the connection layer only needs to send
    /// a `Set-Cookie` back on the new-session path.
    pub fn touch(&mut self, existing_id: Option<&str>) -> (String, bool) {
        if let Some(id) = existing_id {
            if let Some(session) = self.sessions.get_mut(id) {
                session.last_seen = Instant::now();
                return (id.to_string(), false);
            }
        }
        if self.sessions.len() >= MAX_SESSIONS {
            self.evict_oldest_batch();
        }
        let id = generate_id();
        self.sessions.insert(id.clone(), Session::new());
        (id, true)
    }

    pub fn increment_visit_count(&mut self, id: &str) -> u64 {
        let session = match self.sessions.get_mut(id) {
            Some(s) => s,
            None => return 0,
        };
        let count = session.data.entry("visits".to_string()).or_insert_with(|| "0".to_string());
        let next = count.parse::<u64>().unwrap_or(0) + 1;
        *count = next.to_string();
        next
    }

    pub fn sweep_expired(&mut self) {
        let now = Instant::now();
        self.sessions.retain(|_, s| now.duration_since(s.last_seen) < SESSION_TTL);
    }

    fn evict_oldest_batch(&mut self) {
        let mut by_age: Vec<(String, Instant)> = self
            .sessions
            .iter()
            .map(|(id, session)| (id.clone(), session.last_seen))
            .collect();
        by_age.sort_unstable_by_key(|(_, last_seen)| *last_seen);

        for (id, _) in by_age.into_iter().take(SESSION_EVICTION_BATCH) {
            self.sessions.remove(&id);
        }
    }
}

/// Extracts one named cookie's value out of a raw `Cookie:` header value
/// like `a=1; localhost_session=abc123; b=2`.
pub fn extract_cookie(cookie_header: &str, name: &str) -> Option<String> {
    cookie_header.split(';').find_map(|part| {
        let (key, value) = part.trim().split_once('=')?;
        if key == name {
            Some(value.trim().to_string())
        } else {
            None
        }
    })
}

fn generate_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();

    let mut half_a = DefaultHasher::new();
    (nanos, counter, std::process::id(), "a").hash(&mut half_a);
    let mut half_b = DefaultHasher::new();
    (nanos, counter, std::process::id(), "b").hash(&mut half_b);

    format!("{:016x}{:016x}", half_a.finish(), half_b.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_touch_creates_a_new_session() {
        let mut store = SessionStore::new();
        let (id, is_new) = store.touch(None);
        assert!(is_new);
        assert_eq!(id.len(), 32);
    }

    #[test]
    fn touching_a_known_id_reuses_it() {
        let mut store = SessionStore::new();
        let (id, _) = store.touch(None);
        let (id2, is_new) = store.touch(Some(&id));
        assert!(!is_new);
        assert_eq!(id, id2);
    }

    #[test]
    fn unknown_id_gets_a_fresh_session_not_an_error() {
        let mut store = SessionStore::new();
        let (id, is_new) = store.touch(Some("this-id-was-never-issued"));
        assert!(is_new);
        assert_ne!(id, "this-id-was-never-issued");
    }

    #[test]
    fn visit_count_increments_per_session() {
        let mut store = SessionStore::new();
        let (id, _) = store.touch(None);
        assert_eq!(store.increment_visit_count(&id), 1);
        assert_eq!(store.increment_visit_count(&id), 2);
        assert_eq!(store.increment_visit_count(&id), 3);
    }

    #[test]
    fn extracts_named_cookie_from_header() {
        let header = "a=1; localhost_session=abc123; b=2";
        assert_eq!(extract_cookie(header, COOKIE_NAME), Some("abc123".to_string()));
        assert_eq!(extract_cookie(header, "missing"), None);
    }

    #[test]
    fn generated_ids_are_unique() {
        let a = generate_id();
        let b = generate_id();
        assert_ne!(a, b);
    }

    #[test]
    fn cookie_less_traffic_cannot_grow_store_past_limit() {
        let mut store = SessionStore::new();
        let mut newest = String::new();
        for _ in 0..=MAX_SESSIONS {
            (newest, _) = store.touch(None);
        }

        assert!(store.sessions.len() <= MAX_SESSIONS);
        assert!(store.sessions.contains_key(&newest));
    }

    #[test]
    fn capacity_eviction_prefers_old_sessions() {
        let mut store = SessionStore::new();
        let old = Instant::now() - Duration::from_secs(60);
        let recent = Instant::now();
        for index in 0..SESSION_EVICTION_BATCH {
            store.sessions.insert(
                format!("old-{index}"),
                Session {
                    last_seen: old,
                    data: HashMap::new(),
                },
            );
        }
        store.sessions.insert(
            "recent".to_string(),
            Session {
                last_seen: recent,
                data: HashMap::new(),
            },
        );

        store.evict_oldest_batch();

        assert_eq!(store.sessions.len(), 1);
        assert!(store.sessions.contains_key("recent"));
    }
}
