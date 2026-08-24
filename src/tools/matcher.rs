//! A small dependency-free glob matcher supporting `*`, `?`, and `**`.
//!
//! - `*` matches any sequence of characters except `/`
//! - `?` matches exactly one character except `/`
//! - `**` matches any sequence of characters, including `/`

/// Match a glob `pattern` against a `/`-separated `path`.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = path.chars().collect();
    match_here(&p, 0, &s, 0)
}

fn match_here(p: &[char], pi: usize, s: &[char], si: usize) -> bool {
    if pi >= p.len() {
        return si >= s.len();
    }
    match p[pi] {
        '*' => {
            // `**` — can span directory separators.
            if pi + 1 < p.len() && p[pi + 1] == '*' {
                let rest = pi + 2;
                // A trailing `**` matches any suffix, including empty.
                if rest == p.len() {
                    return true;
                }
                // `**/` may match zero directories.
                if p[rest] == '/' && match_here(p, rest + 1, s, si) {
                    return true;
                }
                // Otherwise try matching the rest at every split point.
                let mut k = si;
                loop {
                    if match_here(p, rest, s, k) {
                        return true;
                    }
                    if k >= s.len() {
                        return false;
                    }
                    k += 1;
                }
            }
            // Single `*` — any run of non-separator characters.
            let mut k = si;
            loop {
                if match_here(p, pi + 1, s, k) {
                    return true;
                }
                if k >= s.len() || s[k] == '/' {
                    return false;
                }
                k += 1;
            }
        }
        '?' => {
            if si < s.len() && s[si] != '/' {
                match_here(p, pi + 1, s, si + 1)
            } else {
                false
            }
        }
        c => {
            if si < s.len() && s[si] == c {
                match_here(p, pi + 1, s, si + 1)
            } else {
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::glob_match;

    #[test]
    fn matches_basic_patterns() {
        assert!(glob_match("*.rs", "lib.rs"));
        assert!(!glob_match("*.rs", "lib.rs.bak"));
        assert!(!glob_match("*.rs", "src/lib.rs"), "* must not cross separators");
        assert!(glob_match("**/*.rs", "src/lib.rs"));
        assert!(glob_match("**/*.rs", "lib.rs"));
        assert!(glob_match("src/**", "src/a/b/c"));
        assert!(glob_match("?.txt", "a.txt"));
        assert!(!glob_match("?.txt", "ab.txt"));
        assert!(glob_match("docs/*.md", "docs/readme.md"));
    }
}
