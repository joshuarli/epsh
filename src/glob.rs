use std::fs;
use std::path::{Path, PathBuf};

/// Expand a glob pattern into matching filenames.
/// Returns sorted matches, or empty vec if no matches.
pub fn glob(pattern: &str) -> Vec<String> {
    let mut results = Vec::new();

    if pattern.is_empty() {
        return results;
    }

    // Split pattern into directory prefix and the rest
    let path = Path::new(pattern);
    let mut components: Vec<&str> = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::RootDir => components.push("/"),
            std::path::Component::Normal(s) => components.push(s.to_str().unwrap_or("")),
            std::path::Component::CurDir => components.push("."),
            std::path::Component::ParentDir => components.push(".."),
            _ => {}
        }
    }

    if components.is_empty() {
        return results;
    }

    // Start matching from the first component
    let start = if components[0] == "/" {
        glob_recursive(&PathBuf::from("/"), &components[1..], &mut results);
        return sort_results(results);
    } else {
        PathBuf::from(".")
    };

    glob_recursive(&start, &components, &mut results);
    sort_results(results)
}

fn glob_recursive(dir: &Path, components: &[&str], results: &mut Vec<String>) {
    if components.is_empty() {
        return;
    }

    let pattern = components[0];
    let remaining = &components[1..];

    // If pattern has no glob chars, just check if path exists
    if !has_glob_chars(pattern) {
        let candidate = dir.join(pattern);
        if remaining.is_empty() {
            if candidate.exists() {
                results.push(path_to_string(&candidate));
            }
        } else {
            if candidate.is_dir() {
                glob_recursive(&candidate, remaining, results);
            }
        }
        return;
    }

    // Read directory and match each entry
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        let name = entry.file_name();
        let name_str = match name.to_str() {
            Some(s) => s,
            None => continue,
        };

        // Dotfiles are only matched if the pattern starts with '.'
        if name_str.starts_with('.') && !pattern.starts_with('.') {
            continue;
        }

        if fnmatch(pattern, name_str) {
            let full = dir.join(name_str);
            if remaining.is_empty() {
                results.push(path_to_string(&full));
            } else if full.is_dir() {
                glob_recursive(&full, remaining, results);
            }
        }
    }
}

/// Check if a string contains glob metacharacters.
pub fn has_glob_chars(s: &str) -> bool {
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '*' | '?' | '[' => return true,
            '\\' => {
                chars.next(); // skip escaped char
            }
            _ => {}
        }
    }
    false
}

/// POSIX fnmatch-style pattern matching.
/// Supports: * (any string), ? (any char), [...] (char class), \ (escape)
pub fn fnmatch(pattern: &str, string: &str) -> bool {
    let pat: Vec<char> = pattern.chars().collect();
    let s: Vec<char> = string.chars().collect();
    fnmatch_inner(&pat, 0, &s, 0)
}

fn fnmatch_inner(pat: &[char], mut pi: usize, s: &[char], mut si: usize) -> bool {
    while pi < pat.len() {
        match pat[pi] {
            '?' => {
                if si >= s.len() {
                    return false;
                }
                pi += 1;
                si += 1;
            }
            '*' => {
                // Skip consecutive *'s
                while pi < pat.len() && pat[pi] == '*' {
                    pi += 1;
                }
                // * at end matches everything
                if pi >= pat.len() {
                    return true;
                }
                // Try matching the rest from each position
                for i in si..=s.len() {
                    if fnmatch_inner(pat, pi, s, i) {
                        return true;
                    }
                }
                return false;
            }
            '[' => {
                if si >= s.len() {
                    return false;
                }
                pi += 1;
                let negate = pi < pat.len() && (pat[pi] == '!' || pat[pi] == '^');
                if negate {
                    pi += 1;
                }

                let mut matched = false;
                let mut first = true;
                while pi < pat.len() && (first || pat[pi] != ']') {
                    first = false;
                    let c1 = pat[pi];
                    pi += 1;

                    // Range: a-z
                    if pi + 1 < pat.len() && pat[pi] == '-' && pat[pi + 1] != ']' {
                        let c2 = pat[pi + 1];
                        pi += 2;
                        if s[si] >= c1 && s[si] <= c2 {
                            matched = true;
                        }
                    } else if s[si] == c1 {
                        matched = true;
                    }
                }
                if pi < pat.len() {
                    pi += 1; // skip ]
                }

                if matched == negate {
                    return false;
                }
                si += 1;
            }
            '\\' => {
                pi += 1;
                if pi >= pat.len() {
                    return false;
                }
                if si >= s.len() || s[si] != pat[pi] {
                    return false;
                }
                pi += 1;
                si += 1;
            }
            c => {
                if si >= s.len() || s[si] != c {
                    return false;
                }
                pi += 1;
                si += 1;
            }
        }
    }

    si >= s.len()
}

fn path_to_string(path: &Path) -> String {
    let s = path.to_string_lossy().to_string();
    // Strip leading "./" for relative paths
    s.strip_prefix("./").unwrap_or(&s).to_string()
}

fn sort_results(mut results: Vec<String>) -> Vec<String> {
    results.sort();
    results
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnmatch_literal() {
        assert!(fnmatch("hello", "hello"));
        assert!(!fnmatch("hello", "world"));
    }

    #[test]
    fn fnmatch_star() {
        assert!(fnmatch("*", "anything"));
        assert!(fnmatch("*.txt", "file.txt"));
        assert!(!fnmatch("*.txt", "file.rs"));
        assert!(fnmatch("h*o", "hello"));
        assert!(fnmatch("h*o", "ho"));
    }

    #[test]
    fn fnmatch_question() {
        assert!(fnmatch("?", "a"));
        assert!(!fnmatch("?", ""));
        assert!(!fnmatch("?", "ab"));
        assert!(fnmatch("h?llo", "hello"));
    }

    #[test]
    fn fnmatch_bracket() {
        assert!(fnmatch("[abc]", "a"));
        assert!(fnmatch("[abc]", "c"));
        assert!(!fnmatch("[abc]", "d"));
    }

    #[test]
    fn fnmatch_bracket_range() {
        assert!(fnmatch("[a-z]", "m"));
        assert!(!fnmatch("[a-z]", "M"));
        assert!(fnmatch("[0-9]", "5"));
    }

    #[test]
    fn fnmatch_bracket_negate() {
        assert!(!fnmatch("[!abc]", "a"));
        assert!(fnmatch("[!abc]", "d"));
    }

    #[test]
    fn fnmatch_escape() {
        assert!(fnmatch("\\*", "*"));
        assert!(!fnmatch("\\*", "a"));
    }

    #[test]
    fn fnmatch_complex() {
        assert!(fnmatch("*.tar.gz", "archive.tar.gz"));
        assert!(fnmatch("[Mm]ake*", "Makefile"));
        assert!(fnmatch("[Mm]ake*", "makefile.in"));
    }

    #[test]
    fn has_glob_chars_test() {
        assert!(has_glob_chars("*.txt"));
        assert!(has_glob_chars("file?"));
        assert!(has_glob_chars("[abc]"));
        assert!(!has_glob_chars("plain"));
        assert!(!has_glob_chars("escaped\\*"));
    }
}
