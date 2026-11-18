//! Small formatting helpers shared by the checks.

/// Seconds with a precision that suits the size: `0.004s`, `1.23s`, `312s`.
/// A value that rounds to zero prints as `0.000s`, never `-0.000s`.
#[must_use]
pub fn secs(s: f64) -> String {
    let s = if s.abs() < 0.0005 { 0.0 } else { s };
    if s >= 100.0 {
        format!("{s:.0}s")
    } else if s >= 10.0 {
        format!("{s:.1}s")
    } else if s >= 0.1 {
        format!("{s:.2}s")
    } else {
        format!("{s:.3}s")
    }
}

/// Bytes in binary units: `512 B`, `1.5 GiB`.
#[must_use]
pub fn bytes(b: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{b} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// A percentage of `part` in `whole`, `0` when `whole` is zero. A result
/// that would print as `-0` (rounding noise in a sum of self times) is 0.
#[must_use]
pub fn pct(part: f64, whole: f64) -> f64 {
    let p = if whole > 0.0 {
        100.0 * part / whole
    } else {
        0.0
    };
    if p < 0.0 && p > -0.5 { 0.0 } else { p }
}

/// Shorten `s` to at most `max` characters, keeping both ends.
#[must_use]
pub fn ellipsize(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max || max < 8 {
        return s.to_owned();
    }
    let head = (max - 3) * 2 / 3;
    let tail = max - 3 - head;
    let start: String = s.chars().take(head).collect();
    let end: String = s.chars().skip(n - tail).collect();
    format!("{start}...{end}")
}

/// The first `n` of `items`, comma-separated, then "and N more": shortens
/// a long list without cutting into the names.
#[must_use]
pub fn capped<S: AsRef<str>>(items: &[S], n: usize) -> String {
    let shown: Vec<&str> = items.iter().take(n).map(AsRef::as_ref).collect();
    match items.len().saturating_sub(n) {
        0 => shown.join(", "),
        more => format!("{} and {more} more", shown.join(", ")),
    }
}

/// Wrap `text` to `width` columns, each line prefixed by `indent`.
#[must_use]
pub fn wrap(text: &str, width: usize, indent: &str) -> String {
    let mut out = String::new();
    for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split(' ').filter(|w| !w.is_empty()) {
            if !line.is_empty() && indent.len() + line.len() + 1 + word.len() > width {
                out.push_str(indent);
                out.push_str(&line);
                out.push('\n');
                line.clear();
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        out.push_str(indent);
        out.push_str(&line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caps_lists() {
        assert_eq!(capped(&["a", "b"], 3), "a, b");
        assert_eq!(capped(&["a", "b", "c", "d"], 2), "a, b and 2 more");
        assert_eq!(capped::<&str>(&[], 2), "");
    }

    #[test]
    fn formats() {
        assert_eq!(secs(312.4), "312s");
        assert_eq!(secs(12.34), "12.3s");
        assert_eq!(secs(1.234), "1.23s");
        assert_eq!(secs(0.0042), "0.004s");
        // Rounding noise in a difference of sums is zero, not -0.
        assert_eq!(secs(-0.0001), "0.000s");
        assert_eq!(format!("{:.0}%", pct(-0.0001, 10.0)), "0%");
        assert_eq!(format!("{:.0}%", pct(-5.0, 10.0)), "-50%");
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(3 * 1024 * 1024 * 1024 / 2), "1.5 GiB");
        assert_eq!(ellipsize("abcdefghijklmnopqrstuvwxyz", 13), "abcdef...wxyz");
        assert_eq!(wrap("aa bb cc", 7, "  "), "  aa bb\n  cc\n");
    }
}
