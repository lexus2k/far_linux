//! Small helpers shared across the application.

use std::path::{Path, PathBuf};

/// Replaces control characters, for text printed directly to the console. A
/// folder name must not be able to send escape codes to the terminal.
pub fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// Matches `name` against a `*`/`?` wildcard mask, ignoring ASCII case.
/// Far's traditional `*.*` means "everything", including names with no dot.
pub fn wildcard(mask: &str, name: &str) -> bool {
    if mask == "*.*" {
        return true;
    }
    let m: Vec<char> = mask.chars().flat_map(char::to_lowercase).collect();
    let n: Vec<char> = name.chars().flat_map(char::to_lowercase).collect();
    let (mut mi, mut ni) = (0, 0);
    let (mut star, mut retry) = (None, 0);
    while ni < n.len() {
        if mi < m.len() && m[mi] == '*' {
            star = Some(mi);
            retry = ni;
            mi += 1;
        } else if mi < m.len() && (m[mi] == '?' || m[mi] == n[ni]) {
            mi += 1;
            ni += 1;
        } else if let Some(s) = star {
            mi = s + 1;
            retry += 1;
            ni = retry;
        } else {
            return false;
        }
    }
    m[mi..].iter().all(|c| *c == '*')
}

/// Matches against a comma or semicolon separated list of masks.
pub fn wildcard_list(masks: &str, name: &str) -> bool {
    masks
        .split([',', ';'])
        .map(str::trim)
        .filter(|m| !m.is_empty())
        .any(|m| wildcard(m, name))
}

/// Formats a byte count with thousands separators, e.g. `1 234 567`.
pub fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// Formats a size so that it fits in `width` columns, scaling units if needed.
pub fn fit_size(n: u64, width: usize) -> String {
    let plain = n.to_string();
    if plain.len() <= width {
        return plain;
    }
    let mut value = n as f64;
    for unit in ["K", "M", "G", "T", "P", "E"] {
        value /= 1024.0;
        let text = format!("{}{unit}", value.round() as u64);
        if text.len() <= width {
            return text;
        }
    }
    "*".repeat(width)
}

/// Broken-down local time.
pub struct LocalTime {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

/// Converts a Unix timestamp to local time.
pub fn local_time(secs: i64) -> LocalTime {
    // SAFETY: `tm` is plain data and localtime_r only writes through the
    // pointers it is given.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        let t = secs as libc::time_t;
        libc::localtime_r(&t, &mut tm);
        tm
    };
    LocalTime {
        year: tm.tm_year + 1900,
        month: (tm.tm_mon + 1) as u32,
        day: tm.tm_mday as u32,
        hour: tm.tm_hour as u32,
        minute: tm.tm_min as u32,
        second: tm.tm_sec as u32,
    }
}

/// `dd.mm.yy`, as shown in panel columns.
pub fn format_date(secs: i64) -> String {
    let t = local_time(secs);
    format!("{:02}.{:02}.{:02}", t.day, t.month, t.year.rem_euclid(100))
}

/// `hh:mm`, as shown in panel columns.
pub fn format_time(secs: i64) -> String {
    let t = local_time(secs);
    format!("{:02}:{:02}", t.hour, t.minute)
}

/// Full timestamp for dialogs and the info panel.
pub fn format_datetime(secs: i64) -> String {
    let t = local_time(secs);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        t.year, t.month, t.day, t.hour, t.minute, t.second
    )
}

/// The current time as a Unix timestamp.
pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The user's home directory, if known.
pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// Shortens a path by replacing the home directory prefix with `~`.
pub fn tilde(path: &Path) -> String {
    if let Some(home) = home_dir() {
        if let Ok(rest) = path.strip_prefix(&home) {
            if rest.as_os_str().is_empty() {
                return "~".into();
            }
            return format!("~/{}", rest.display());
        }
    }
    path.display().to_string()
}

/// Expands `~` and resolves `input` against `base`.
pub fn resolve_path(base: &Path, input: &str) -> PathBuf {
    let expanded = if input == "~" {
        home_dir().unwrap_or_else(|| PathBuf::from(input))
    } else if let (Some(rest), Some(home)) = (input.strip_prefix("~/"), home_dir()) {
        home.join(rest)
    } else {
        PathBuf::from(input)
    };
    if expanded.is_absolute() {
        expanded
    } else {
        base.join(expanded)
    }
}

/// Removes `.` and `..` components without touching the file system.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Standard base64, used for the OSC 52 clipboard sequence.
pub fn base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        out.push(TABLE[(b[0] >> 2) as usize] as char);
        out.push(TABLE[(((b[0] & 3) << 4) | (b[1] >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(((b[1] & 15) << 2) | (b[2] >> 6)) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(b[2] & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Formats permission bits as `drwxr-xr-x`.
pub fn mode_string(mode: u32, is_dir: bool, is_link: bool) -> String {
    let mut s = String::with_capacity(10);
    s.push(if is_link {
        'l'
    } else if is_dir {
        'd'
    } else {
        '-'
    });
    for shift in [6, 3, 0] {
        let bits = (mode >> shift) & 7;
        s.push(if bits & 4 != 0 { 'r' } else { '-' });
        s.push(if bits & 2 != 0 { 'w' } else { '-' });
        s.push(if bits & 1 != 0 { 'x' } else { '-' });
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printable_removes_escape_codes() {
        assert_eq!(printable("a\x1b]0;x\x07b\nж"), "a?]0;x?b?ж");
    }

    #[test]
    fn wildcard_matching() {
        assert!(wildcard("*.txt", "a.TXT"));
        assert!(wildcard("*.*", "noext"));
        assert!(wildcard("a?c", "abc"));
        assert!(!wildcard("a?c", "ac"));
        assert!(wildcard("*a*b*", "xxaxxbxx"));
        assert!(!wildcard("*.rs", "a.rs.bak"));
        assert!(wildcard_list("*.c, *.h", "x.h"));
        assert!(!wildcard_list("", "x.h"));
    }

    #[test]
    fn number_formatting() {
        assert_eq!(group_digits(1234567), "1 234 567");
        assert_eq!(group_digits(999), "999");
        assert_eq!(fit_size(1023, 6), "1023");
        assert_eq!(fit_size(10 * 1024 * 1024, 6), "10240K");
        assert_eq!(fit_size(10 * 1024 * 1024, 4), "10M");
    }

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
    }

    #[test]
    fn path_helpers() {
        assert_eq!(normalize(Path::new("/a/./b/../c")), Path::new("/a/c"));
        assert_eq!(resolve_path(Path::new("/x"), "y"), Path::new("/x/y"));
        assert_eq!(resolve_path(Path::new("/x"), "/y"), Path::new("/y"));
        assert_eq!(mode_string(0o755, true, false), "drwxr-xr-x");
    }
}
