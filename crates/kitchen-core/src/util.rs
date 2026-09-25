use std::time::{SystemTime, UNIX_EPOCH};

use rand::Rng;

/// Seconds since the unix epoch.
pub fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Short random id (8 lowercase hex chars) for workspaces and tabs.
pub fn new_id() -> String {
    let n: u32 = rand::rng().random();
    format!("{n:08x}")
}

/// Quote one word for a POSIX shell (bash, zsh and sh all accept this form).
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_./=:@%+,".contains(&b)) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

pub fn shell_join<S: AsRef<str>>(argv: &[S]) -> String {
    argv.iter().map(|a| shell_quote(a.as_ref())).collect::<Vec<_>>().join(" ")
}

/// Turn free text (a ticket title, a PR title) into a branch/dir slug.
pub fn slugify(s: &str, max_len: usize) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash && !out.is_empty() {
            out.push('-');
            dash = true;
        }
        if out.len() >= max_len {
            break;
        }
    }
    out.trim_matches('-').to_string()
}

pub fn content_hash(s: &str) -> String {
    blake3::hash(s.as_bytes()).to_hex()[..24].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("abc/def"), "abc/def");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn slugs() {
        assert_eq!(slugify("AI-123: Fix the Thing!", 40), "ai-123-fix-the-thing");
        assert_eq!(slugify("  --x--  ", 40), "x");
        assert_eq!(slugify("abcdefghij", 4), "abcd");
    }
}
