//! Small shared helpers: identifiers, wildcard matching, path display and truncation.

use rand::Rng;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

/// An identifier in the upstream format `<prefix>_<12 hex digits of time and counter><14 base62>`.
pub fn id(prefix: &str) -> String {
    make_id(prefix, false)
}

/// Like [`id`], but with the time part inverted so that newer ids sort first (upstream session ids).
pub fn id_desc(prefix: &str) -> String {
    make_id(prefix, true)
}

fn make_id(prefix: &str, descending: bool) -> String {
    static STATE: Mutex<(i64, u64)> = Mutex::new((0, 0));
    let now = now_ms();
    let r = {
        let mut st = STATE.lock().unwrap();
        if st.0 != now {
            *st = (now, 0);
        }
        st.1 += 1;
        (now as u64).wrapping_mul(0x1000).wrapping_add(st.1)
    };
    let r = if descending { !r } else { r };
    let time: String = (0..6).map(|i| format!("{:02x}", (r >> (40 - 8 * i)) & 0xff)).collect();
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut rng = rand::thread_rng();
    let rand: String = (0..14).map(|_| ALPHABET[(rng.gen::<u8>() % 62) as usize] as char).collect();
    format!("{prefix}_{time}{rand}")
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Glob-style matching for permission rules: `*` matches any sequence, `/` included, and `?` matches
/// a single character.
pub fn wildcard(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    // "git *" should also match a bare "git".
    pi == p.len() || (p.len() >= 2 && pi == p.len() - 2 && p[pi] == ' ' && p[pi + 1] == '*')
}

/// Normalizes a path lexically (resolving `.` and `..`) without touching the filesystem.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

pub fn resolve(base: &Path, p: &str) -> PathBuf {
    let p = expand_home(p);
    let pb = PathBuf::from(&p);
    if pb.is_absolute() {
        normalize(&pb)
    } else {
        normalize(&base.join(pb))
    }
}

pub fn expand_home(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/") {
        return crate::paths::get().home.join(rest).to_string_lossy().into_owned();
    }
    if p == "~" {
        return crate::paths::get().home.to_string_lossy().into_owned();
    }
    p.to_string()
}

pub fn contains(dir: &Path, path: &Path) -> bool {
    normalize(path).starts_with(normalize(dir))
}

/// Displays `path` relative to `base` when it lies inside it.
pub fn relative(base: &Path, path: &Path) -> String {
    if contains(base, path) {
        if let Ok(r) = normalize(path).strip_prefix(normalize(base)) {
            let s = r.to_string_lossy().into_owned();
            return if s.is_empty() { ".".into() } else { s };
        }
    }
    path.to_string_lossy().into_owned()
}

pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Strips a UTF-8 BOM, reporting whether one was present.
pub fn strip_bom(s: &str) -> (&str, bool) {
    match s.strip_prefix('\u{feff}') {
        Some(rest) => (rest, true),
        None => (s, false),
    }
}

pub fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

/// Unified diff of two texts, for display and tool metadata.
pub fn unified_diff(path: &str, before: &str, after: &str) -> String {
    let diff = similar::TextDiff::from_lines(before, after);
    diff.unified_diff().context_radius(3).header(path, path).to_string()
}

pub fn diff_stats(before: &str, after: &str) -> (usize, usize) {
    let diff = similar::TextDiff::from_lines(before, after);
    let mut add = 0;
    let mut del = 0;
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => add += 1,
            similar::ChangeTag::Delete => del += 1,
            _ => {}
        }
    }
    (add, del)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wildcard_basics() {
        assert!(wildcard("*", "anything"));
        assert!(wildcard("git *", "git status"));
        assert!(wildcard("git *", "git"));
        assert!(!wildcard("git *", "gitk"));
        assert!(wildcard("*.env", "/a/b/.env"));
        assert!(!wildcard("*.env", "/a/b/.env.example"));
        assert!(wildcard("*.env.*", "/a/.env.local"));
    }
}
