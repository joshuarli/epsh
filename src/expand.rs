use crate::arith;
use crate::ast::*;
use crate::glob;
use crate::var::Variables;

/// An expanded word fragment with metadata for field splitting.
#[derive(Debug, Clone)]
pub struct ExpandedWord {
    pub value: String,
    /// True if this fragment came from an unquoted expansion ($var, $(cmd))
    /// and should be subject to field splitting.
    pub split_fields: bool,
    /// True if this starts a new word boundary (for "$@" expansion).
    pub word_break: bool,
}

/// Expand a Word AST node into a list of strings (after field splitting and globbing).
/// This is the full expansion pipeline for command arguments:
///   tilde → parameter → command subst → arithmetic → field split → glob → quote removal
pub fn expand_word_to_fields(
    word: &Word,
    vars: &mut Variables,
    exit_status: i32,
    shell_pid: u32,
    cmd_subst: &mut Option<&mut dyn FnMut(&Command) -> String>,
) -> Vec<String> {
    // Step 1: Expand to intermediate fragments with split_fields metadata
    let fragments = expand_word_parts(&word.parts, vars, exit_status, shell_pid, false, cmd_subst);

    // Step 2: Field splitting on fragments marked split_fields=true
    let ifs = vars.ifs().to_string();
    let split = field_split(&fragments, &ifs);

    // Step 3: Pathname expansion (globbing) on results
    let mut result = Vec::new();
    for field in split {
        if glob::has_glob_chars(&field) {
            let matches = glob::glob(&field);
            if matches.is_empty() {
                // No matches: keep the pattern literal (POSIX behavior)
                result.push(field);
            } else {
                result.extend(matches);
            }
        } else {
            result.push(field);
        }
    }

    result
}

/// Expand a Word to a single string (no field splitting or globbing).
/// Used for: here-doc bodies, assignment values, case words.
pub fn expand_word_to_string(
    word: &Word,
    vars: &mut Variables,
    exit_status: i32,
    shell_pid: u32,
    cmd_subst: &mut Option<&mut dyn FnMut(&Command) -> String>,
) -> String {
    let fragments = expand_word_parts(&word.parts, vars, exit_status, shell_pid, true, cmd_subst);
    fragments.into_iter().map(|f| f.value).collect()
}

/// Expand a list of WordParts into ExpandedWord fragments.
fn expand_word_parts(
    parts: &[WordPart],
    vars: &mut Variables,
    exit_status: i32,
    shell_pid: u32,
    quoted_context: bool,
    cmd_subst: &mut Option<&mut dyn FnMut(&Command) -> String>,
) -> Vec<ExpandedWord> {
    let mut result = Vec::new();

    for part in parts {
        match part {
            WordPart::Literal(s) => {
                result.push(ExpandedWord {
                    value: s.clone(),
                    split_fields: false,
                    word_break: false,
                });
            }
            WordPart::SingleQuoted(s) => {
                result.push(ExpandedWord {
                    value: s.clone(),
                    split_fields: false,
                    word_break: false,
                });
            }
            WordPart::DoubleQuoted(inner) => {
                // Check if inner contains a bare $@ — needs special multi-field expansion
                let has_at = inner.iter().any(|p| matches!(p, WordPart::Param(pe) if pe.name == "@" && matches!(pe.op, ParamOp::Normal)));

                if has_at && inner.len() == 1 {
                    // Simple case: "$@" alone — expand to separate fields
                    for arg in vars.positional.iter() {
                        result.push(ExpandedWord {
                            value: arg.clone(),
                            split_fields: false,
                            word_break: true,
                        });
                    }
                } else if has_at {
                    // Mixed case: "prefix$@suffix" — expand $@ into multiple fields
                    // with prefix on first and suffix on last
                    let mut prefix = String::new();
                    let mut suffix_parts: Vec<WordPart> = Vec::new();
                    let mut found_at = false;
                    for p in inner {
                        if !found_at {
                            if matches!(p, WordPart::Param(pe) if pe.name == "@" && matches!(pe.op, ParamOp::Normal))
                            {
                                found_at = true;
                            } else {
                                let expanded = expand_word_parts(
                                    std::slice::from_ref(p),
                                    vars,
                                    exit_status,
                                    shell_pid,
                                    true,
                                    cmd_subst,
                                );
                                for f in expanded {
                                    prefix.push_str(&f.value);
                                }
                            }
                        } else {
                            suffix_parts.push(p.clone());
                        }
                    }
                    let suffix_frags = expand_word_parts(
                        &suffix_parts,
                        vars,
                        exit_status,
                        shell_pid,
                        true,
                        cmd_subst,
                    );
                    let suffix: String = suffix_frags.into_iter().map(|f| f.value).collect();

                    if vars.positional.is_empty() {
                        // "$@" with no positional params produces nothing (not even empty string)
                    } else {
                        for (i, arg) in vars.positional.iter().enumerate() {
                            let mut val = String::new();
                            if i == 0 {
                                val.push_str(&prefix);
                            }
                            val.push_str(arg);
                            if i == vars.positional.len() - 1 {
                                val.push_str(&suffix);
                            }
                            result.push(ExpandedWord {
                                value: val,
                                split_fields: false,
                                word_break: i > 0,
                            });
                        }
                    }
                } else {
                    // No $@ — normal double-quote handling
                    let expanded =
                        expand_word_parts(inner, vars, exit_status, shell_pid, true, cmd_subst);
                    let value: String = expanded.into_iter().map(|f| f.value).collect();
                    result.push(ExpandedWord {
                        value,
                        split_fields: false,
                        word_break: false,
                    });
                }
            }
            WordPart::Param(param) => {
                // $@ unquoted: expand to separate fields
                if param.name == "@" && matches!(param.op, ParamOp::Normal) && !quoted_context {
                    for arg in &vars.positional {
                        result.push(ExpandedWord {
                            value: arg.clone(),
                            split_fields: false,
                            word_break: true, // each arg is a separate field
                        });
                    }
                }
                // $* in quoted context: join with first char of IFS
                else if param.name == "*" && matches!(param.op, ParamOp::Normal) && quoted_context
                {
                    let sep = vars.ifs().chars().next().map_or(String::new(), |c| c.to_string());
                    let value = vars.positional.join(&sep);
                    result.push(ExpandedWord {
                        value,
                        split_fields: false,
                        word_break: false,
                    });
                }
                // $* unquoted: each positional param is a separate field
                // (same as $@ when unquoted — POSIX specifies both produce separate fields)
                else if param.name == "*"
                    && matches!(param.op, ParamOp::Normal)
                    && !quoted_context
                {
                    for arg in &vars.positional {
                        result.push(ExpandedWord {
                            value: arg.clone(),
                            split_fields: true, // subject to further IFS splitting
                            word_break: true,
                        });
                    }
                } else {
                    let frags = expand_param_to_fragments(
                        param,
                        vars,
                        exit_status,
                        shell_pid,
                        cmd_subst,
                        quoted_context,
                    );
                    result.extend(frags);
                }
            }
            WordPart::Tilde(user) => {
                let expanded = expand_tilde(user);
                result.push(ExpandedWord {
                    value: expanded,
                    split_fields: false,
                    word_break: false,
                });
            }
            WordPart::CmdSubst(cmd) | WordPart::Backtick(cmd) => {
                let value = if let Some(f) = cmd_subst.as_mut() {
                    f(cmd)
                } else {
                    String::new()
                };
                result.push(ExpandedWord {
                    value,
                    split_fields: !quoted_context,
                    word_break: false,
                });
            }
            WordPart::Arith(inner) => {
                // First expand any variables in the arithmetic expression
                let text: String =
                    expand_word_parts(inner, vars, exit_status, shell_pid, true, cmd_subst)
                        .into_iter()
                        .map(|f| f.value)
                        .collect();
                // Then evaluate the arithmetic
                let value = match arith::eval_arith(&text, vars, exit_status, shell_pid) {
                    Ok(n) => n.to_string(),
                    Err(e) => {
                        eprintln!("arithmetic error: {e}");
                        "0".to_string()
                    }
                };
                result.push(ExpandedWord {
                    value,
                    split_fields: !quoted_context,
                    word_break: false,
                });
            }
        }
    }

    result
}

/// Expand a parameter expression to fragments, preserving quoting info.
fn expand_param_to_fragments(
    param: &ParamExpr,
    vars: &mut Variables,
    exit_status: i32,
    shell_pid: u32,
    cmd_subst: &mut Option<&mut dyn FnMut(&Command) -> String>,
    quoted_context: bool,
) -> Vec<ExpandedWord> {
    let name = &param.name;

    let raw_value = if is_special_param(name) {
        vars.get_special(name, exit_status, shell_pid)
    } else {
        vars.get(name).map(String::from)
    };

    // For operators that use a word (default, assign, alternative, error),
    // expand the word to fragments to preserve quoting
    match &param.op {
        ParamOp::BadSubst => {
            eprintln!("{}: bad substitution", param.name);
            vec![ExpandedWord {
                value: String::new(),
                split_fields: false,
                word_break: false,
            }]
        }
        ParamOp::Normal => {
            let value = raw_value.unwrap_or_default();
            vec![ExpandedWord {
                value,
                split_fields: !quoted_context,
                word_break: false,
            }]
        }
        ParamOp::Length => {
            let value = raw_value
                .as_ref()
                .map(|v| v.len().to_string())
                .unwrap_or_else(|| "0".to_string());
            vec![ExpandedWord {
                value,
                split_fields: !quoted_context,
                word_break: false,
            }]
        }
        ParamOp::Default { colon, word } | ParamOp::Assign { colon, word } => {
            let is_unset = if *colon {
                raw_value.as_ref().is_none_or(|v| v.is_empty())
            } else {
                raw_value.is_none()
            };
            if is_unset {
                let frags = expand_word_parts(
                    word,
                    vars,
                    exit_status,
                    shell_pid,
                    quoted_context,
                    cmd_subst,
                );
                if matches!(param.op, ParamOp::Assign { .. }) {
                    let val: String = frags.iter().map(|f| f.value.clone()).collect();
                    let _ = vars.set(name, &val);
                }
                frags
            } else {
                let value = raw_value.unwrap_or_default();
                vec![ExpandedWord {
                    value,
                    split_fields: !quoted_context,
                    word_break: false,
                }]
            }
        }
        ParamOp::Error { colon, word } => {
            let is_unset = if *colon {
                raw_value.as_ref().is_none_or(|v| v.is_empty())
            } else {
                raw_value.is_none()
            };
            if is_unset {
                let msg = expand_param_word(word, vars, exit_status, shell_pid, cmd_subst);
                eprintln!(
                    "{name}: {}",
                    if msg.is_empty() {
                        "parameter not set"
                    } else {
                        &msg
                    }
                );
                vec![ExpandedWord {
                    value: String::new(),
                    split_fields: false,
                    word_break: false,
                }]
            } else {
                let value = raw_value.unwrap_or_default();
                vec![ExpandedWord {
                    value,
                    split_fields: !quoted_context,
                    word_break: false,
                }]
            }
        }
        ParamOp::Alternative { colon, word } => {
            let is_unset = if *colon {
                raw_value.as_ref().is_none_or(|v| v.is_empty())
            } else {
                raw_value.is_none()
            };
            if is_unset {
                vec![]
            } else {
                expand_word_parts(
                    word,
                    vars,
                    exit_status,
                    shell_pid,
                    quoted_context,
                    cmd_subst,
                )
            }
        }
        _ => {
            // Trim operations — fall back to string-based expand_param
            let value = expand_param(param, vars, exit_status, shell_pid, cmd_subst);
            vec![ExpandedWord {
                value,
                split_fields: !quoted_context,
                word_break: false,
            }]
        }
    }
}

/// Expand a parameter expression (returns flat string).
fn expand_param(
    param: &ParamExpr,
    vars: &mut Variables,
    exit_status: i32,
    shell_pid: u32,
    cmd_subst: &mut Option<&mut dyn FnMut(&Command) -> String>,
) -> String {
    let name = &param.name;

    // Get the raw value
    let raw_value = if is_special_param(name) {
        vars.get_special(name, exit_status, shell_pid)
    } else {
        vars.get(name).map(String::from)
    };

    match &param.op {
        ParamOp::BadSubst => {
            eprintln!("{}: bad substitution", param.name);
            String::new()
        }
        ParamOp::Normal => raw_value.unwrap_or_default(),
        ParamOp::Length => raw_value
            .as_ref()
            .map(|v| v.len().to_string())
            .unwrap_or_else(|| "0".to_string()),
        ParamOp::Default { colon, word } => {
            let is_unset = if *colon {
                raw_value.as_ref().is_none_or(|v| v.is_empty())
            } else {
                raw_value.is_none()
            };
            if is_unset {
                expand_param_word(word, vars, exit_status, shell_pid, cmd_subst)
            } else {
                raw_value.unwrap_or_default()
            }
        }
        ParamOp::Assign { colon, word } => {
            let is_unset = if *colon {
                raw_value.as_ref().is_none_or(|v| v.is_empty())
            } else {
                raw_value.is_none()
            };
            if is_unset {
                let default = expand_param_word(word, vars, exit_status, shell_pid, cmd_subst);
                let _ = vars.set(name, &default);
                default
            } else {
                raw_value.unwrap_or_default()
            }
        }
        ParamOp::Error { colon, word } => {
            let is_unset = if *colon {
                raw_value.as_ref().is_none_or(|v| v.is_empty())
            } else {
                raw_value.is_none()
            };
            if is_unset {
                let msg = expand_param_word(word, vars, exit_status, shell_pid, cmd_subst);
                // TODO: this should be a proper error, not just stderr
                eprintln!(
                    "{name}: {}",
                    if msg.is_empty() {
                        "parameter not set"
                    } else {
                        &msg
                    }
                );
                String::new()
            } else {
                raw_value.unwrap_or_default()
            }
        }
        ParamOp::Alternative { colon, word } => {
            let is_unset = if *colon {
                raw_value.as_ref().is_none_or(|v| v.is_empty())
            } else {
                raw_value.is_none()
            };
            if is_unset {
                String::new()
            } else {
                expand_param_word(word, vars, exit_status, shell_pid, cmd_subst)
            }
        }
        ParamOp::TrimSuffixSmall(pattern) => {
            let val = raw_value.unwrap_or_default();
            let pat = expand_param_word(pattern, vars, exit_status, shell_pid, cmd_subst);
            trim_suffix(&val, &pat, false)
        }
        ParamOp::TrimSuffixLarge(pattern) => {
            let val = raw_value.unwrap_or_default();
            let pat = expand_param_word(pattern, vars, exit_status, shell_pid, cmd_subst);
            trim_suffix(&val, &pat, true)
        }
        ParamOp::TrimPrefixSmall(pattern) => {
            let val = raw_value.unwrap_or_default();
            let pat = expand_param_word(pattern, vars, exit_status, shell_pid, cmd_subst);
            trim_prefix(&val, &pat, false)
        }
        ParamOp::TrimPrefixLarge(pattern) => {
            let val = raw_value.unwrap_or_default();
            let pat = expand_param_word(pattern, vars, exit_status, shell_pid, cmd_subst);
            trim_prefix(&val, &pat, true)
        }
    }
}

/// Expand a parameter operation's word (the part after :-, :=, etc.)
fn expand_param_word(
    parts: &[WordPart],
    vars: &mut Variables,
    exit_status: i32,
    shell_pid: u32,
    cmd_subst: &mut Option<&mut dyn FnMut(&Command) -> String>,
) -> String {
    let fragments = expand_word_parts(parts, vars, exit_status, shell_pid, true, cmd_subst);
    fragments.into_iter().map(|f| f.value).collect()
}

/// Tilde expansion: ~ → $HOME, ~user → user's home dir
fn expand_tilde(user: &str) -> String {
    if user.is_empty() {
        // ~ → $HOME
        std::env::var("HOME").unwrap_or_else(|_| "~".into())
    } else {
        // ~user → look up user's home directory
        // For stdlib-only, we can't easily do this without libc getpwnam.
        // Fall back to leaving it unexpanded.
        format!("~{user}")
    }
}

/// Check if a parameter name is a special parameter.
fn is_special_param(name: &str) -> bool {
    matches!(name, "@" | "*" | "#" | "?" | "-" | "$" | "!" | "0")
        || (name.len() == 1 && name.chars().next().unwrap().is_ascii_digit())
}

/// Field splitting based on IFS.
/// Uses posh-style state machine: Init → Word / IfsWs / IfsNws
fn field_split(fragments: &[ExpandedWord], ifs: &str) -> Vec<String> {
    if fragments.is_empty() {
        return Vec::new();
    }

    // Concatenate non-splitting fragments and split where marked
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut have_field = false;

    let ifs_ws: Vec<char> = ifs.chars().filter(|c| c.is_whitespace()).collect();
    let ifs_nws: Vec<char> = ifs.chars().filter(|c| !c.is_whitespace()).collect();

    for frag in fragments {
        // word_break starts a new field (used for "$@" expansion)
        if frag.word_break && have_field {
            fields.push(std::mem::take(&mut current));
            have_field = false;
        }

        if !frag.split_fields || ifs.is_empty() {
            // No splitting: append directly
            current.push_str(&frag.value);
            have_field = true;
            continue;
        }

        // Split this fragment on IFS
        let chars: Vec<char> = frag.value.chars().collect();
        let mut i = 0;

        while i < chars.len() {
            let ch = chars[i];

            if ifs_ws.contains(&ch) {
                // IFS whitespace: skip consecutive whitespace, then emit field
                if have_field {
                    fields.push(std::mem::take(&mut current));
                    have_field = false;
                }
                while i < chars.len() && ifs_ws.contains(&chars[i]) {
                    i += 1;
                }
                // Check for non-ws IFS delimiter after whitespace
                if i < chars.len() && ifs_nws.contains(&chars[i]) {
                    i += 1;
                    // Skip trailing ws after nws delimiter
                    while i < chars.len() && ifs_ws.contains(&chars[i]) {
                        i += 1;
                    }
                }
            } else if ifs_nws.contains(&ch) {
                // IFS non-whitespace: always creates a field boundary
                if have_field {
                    fields.push(std::mem::take(&mut current));
                } else {
                    fields.push(String::new()); // empty field
                }
                have_field = false;
                i += 1;
                // Skip trailing ws after nws delimiter
                while i < chars.len() && ifs_ws.contains(&chars[i]) {
                    i += 1;
                }
            } else {
                current.push(ch);
                have_field = true;
                i += 1;
            }
        }
    }

    if have_field {
        fields.push(current);
    }

    fields
}

/// Remove smallest/largest suffix matching pattern.
fn trim_suffix(value: &str, pattern: &str, greedy: bool) -> String {
    let chars: Vec<char> = value.chars().collect();
    if greedy {
        // Largest suffix: try from the beginning
        for i in 0..chars.len() {
            let suffix: String = chars[i..].iter().collect();
            if glob::fnmatch(pattern, &suffix) {
                return chars[..i].iter().collect();
            }
        }
    } else {
        // Smallest suffix: try from the end
        for i in (0..chars.len()).rev() {
            let suffix: String = chars[i..].iter().collect();
            if glob::fnmatch(pattern, &suffix) {
                return chars[..i].iter().collect();
            }
        }
    }
    value.to_string()
}

/// Remove smallest/largest prefix matching pattern.
fn trim_prefix(value: &str, pattern: &str, greedy: bool) -> String {
    let chars: Vec<char> = value.chars().collect();
    if greedy {
        // Largest prefix: try from the end
        for i in (1..=chars.len()).rev() {
            let prefix: String = chars[..i].iter().collect();
            if glob::fnmatch(pattern, &prefix) {
                return chars[i..].iter().collect();
            }
        }
    } else {
        // Smallest prefix: try from the beginning
        for i in 1..=chars.len() {
            let prefix: String = chars[..i].iter().collect();
            if glob::fnmatch(pattern, &prefix) {
                return chars[i..].iter().collect();
            }
        }
    }
    value.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Span;

    fn make_vars() -> Variables {
        let mut vars = Variables::new();
        vars.set("FOO", "hello").unwrap();
        vars.set("EMPTY", "").unwrap();
        vars.set("PATH_VAR", "/usr/local/bin:/usr/bin:/bin")
            .unwrap();
        vars.set("FILE", "archive.tar.gz").unwrap();
        vars
    }

    fn make_word(parts: Vec<WordPart>) -> Word {
        Word {
            parts,
            span: Span::default(),
        }
    }

    #[test]
    fn expand_literal() {
        let mut vars = make_vars();
        let word = make_word(vec![WordPart::Literal("hello".into())]);
        assert_eq!(
            expand_word_to_fields(&word, &mut vars, 0, 1, &mut None),
            vec!["hello"]
        );
    }

    #[test]
    fn expand_simple_variable() {
        let mut vars = make_vars();
        let word = make_word(vec![WordPart::Param(ParamExpr {
            name: "FOO".into(),
            op: ParamOp::Normal,
            span: Span::default(),
        })]);
        assert_eq!(
            expand_word_to_fields(&word, &mut vars, 0, 1, &mut None),
            vec!["hello"]
        );
    }

    #[test]
    fn expand_default_unset() {
        let mut vars = make_vars();
        let word = make_word(vec![WordPart::Param(ParamExpr {
            name: "UNSET".into(),
            op: ParamOp::Default {
                colon: false,
                word: vec![WordPart::Literal("fallback".into())],
            },
            span: Span::default(),
        })]);
        assert_eq!(
            expand_word_to_fields(&word, &mut vars, 0, 1, &mut None),
            vec!["fallback"]
        );
    }

    #[test]
    fn expand_default_colon_empty() {
        let mut vars = make_vars();
        let word = make_word(vec![WordPart::Param(ParamExpr {
            name: "EMPTY".into(),
            op: ParamOp::Default {
                colon: true,
                word: vec![WordPart::Literal("fallback".into())],
            },
            span: Span::default(),
        })]);
        assert_eq!(
            expand_word_to_fields(&word, &mut vars, 0, 1, &mut None),
            vec!["fallback"]
        );
    }

    #[test]
    fn expand_default_set() {
        let mut vars = make_vars();
        let word = make_word(vec![WordPart::Param(ParamExpr {
            name: "FOO".into(),
            op: ParamOp::Default {
                colon: false,
                word: vec![WordPart::Literal("fallback".into())],
            },
            span: Span::default(),
        })]);
        assert_eq!(
            expand_word_to_fields(&word, &mut vars, 0, 1, &mut None),
            vec!["hello"]
        );
    }

    #[test]
    fn expand_assign_default() {
        let mut vars = make_vars();
        let word = make_word(vec![WordPart::Param(ParamExpr {
            name: "NEW_VAR".into(),
            op: ParamOp::Assign {
                colon: false,
                word: vec![WordPart::Literal("assigned".into())],
            },
            span: Span::default(),
        })]);
        assert_eq!(
            expand_word_to_fields(&word, &mut vars, 0, 1, &mut None),
            vec!["assigned"]
        );
        assert_eq!(vars.get("NEW_VAR"), Some("assigned"));
    }

    #[test]
    fn expand_alternative_set() {
        let mut vars = make_vars();
        let word = make_word(vec![WordPart::Param(ParamExpr {
            name: "FOO".into(),
            op: ParamOp::Alternative {
                colon: false,
                word: vec![WordPart::Literal("alt".into())],
            },
            span: Span::default(),
        })]);
        assert_eq!(
            expand_word_to_fields(&word, &mut vars, 0, 1, &mut None),
            vec!["alt"]
        );
    }

    #[test]
    fn expand_alternative_unset() {
        let mut vars = make_vars();
        let word = make_word(vec![WordPart::Param(ParamExpr {
            name: "UNSET".into(),
            op: ParamOp::Alternative {
                colon: false,
                word: vec![WordPart::Literal("alt".into())],
            },
            span: Span::default(),
        })]);
        let result = expand_word_to_fields(&word, &mut vars, 0, 1, &mut None);
        assert!(result.is_empty() || result == vec![""]);
    }

    #[test]
    fn expand_length() {
        let mut vars = make_vars();
        let word = make_word(vec![WordPart::Param(ParamExpr {
            name: "FOO".into(),
            op: ParamOp::Length,
            span: Span::default(),
        })]);
        assert_eq!(
            expand_word_to_fields(&word, &mut vars, 0, 1, &mut None),
            vec!["5"]
        );
    }

    #[test]
    fn expand_trim_suffix_small() {
        let mut vars = make_vars();
        let word = make_word(vec![WordPart::Param(ParamExpr {
            name: "FILE".into(),
            op: ParamOp::TrimSuffixSmall(vec![WordPart::Literal(".*".into())]),
            span: Span::default(),
        })]);
        assert_eq!(
            expand_word_to_fields(&word, &mut vars, 0, 1, &mut None),
            vec!["archive.tar"]
        );
    }

    #[test]
    fn expand_trim_suffix_large() {
        let mut vars = make_vars();
        let word = make_word(vec![WordPart::Param(ParamExpr {
            name: "FILE".into(),
            op: ParamOp::TrimSuffixLarge(vec![WordPart::Literal(".*".into())]),
            span: Span::default(),
        })]);
        assert_eq!(
            expand_word_to_fields(&word, &mut vars, 0, 1, &mut None),
            vec!["archive"]
        );
    }

    #[test]
    fn expand_trim_prefix_small() {
        let mut vars = make_vars();
        let word = make_word(vec![WordPart::Param(ParamExpr {
            name: "PATH_VAR".into(),
            op: ParamOp::TrimPrefixSmall(vec![WordPart::Literal("*/".into())]),
            span: Span::default(),
        })]);
        // Smallest prefix matching */: removes "/" (since */ matches "/")
        let result = expand_word_to_fields(&word, &mut vars, 0, 1, &mut None);
        assert_eq!(result, vec!["usr/local/bin:/usr/bin:/bin"]);
    }

    #[test]
    fn expand_trim_prefix_large() {
        let mut vars = make_vars();
        let word = make_word(vec![WordPart::Param(ParamExpr {
            name: "PATH_VAR".into(),
            op: ParamOp::TrimPrefixLarge(vec![WordPart::Literal("*/".into())]),
            span: Span::default(),
        })]);
        // Largest prefix matching */: removes everything up to last /
        let result = expand_word_to_fields(&word, &mut vars, 0, 1, &mut None);
        assert_eq!(result, vec!["bin"]);
    }

    #[test]
    fn expand_tilde_home() {
        let mut vars = make_vars();
        let word = make_word(vec![
            WordPart::Tilde(String::new()),
            WordPart::Literal("/bin".into()),
        ]);
        let result = expand_word_to_string(&word, &mut vars, 0, 1, &mut None);
        let home = std::env::var("HOME").unwrap();
        assert_eq!(result, format!("{home}/bin"));
    }

    #[test]
    fn expand_single_quoted() {
        let mut vars = make_vars();
        let word = make_word(vec![WordPart::SingleQuoted("$FOO".into())]);
        assert_eq!(
            expand_word_to_fields(&word, &mut vars, 0, 1, &mut None),
            vec!["$FOO"]
        );
    }

    #[test]
    fn expand_double_quoted_no_split() {
        let mut vars = make_vars();
        vars.set("X", "a  b  c").unwrap();
        let word = make_word(vec![WordPart::DoubleQuoted(vec![WordPart::Param(
            ParamExpr {
                name: "X".into(),
                op: ParamOp::Normal,
                span: Span::default(),
            },
        )])]);
        // Inside double quotes, no field splitting
        assert_eq!(
            expand_word_to_fields(&word, &mut vars, 0, 1, &mut None),
            vec!["a  b  c"]
        );
    }

    #[test]
    fn field_split_basic() {
        let fragments = vec![ExpandedWord {
            value: "a b c".into(),
            split_fields: true,
            word_break: false,
        }];
        let result = field_split(&fragments, " \t\n");
        assert_eq!(result, vec!["a", "b", "c"]);
    }

    #[test]
    fn field_split_custom_ifs() {
        let fragments = vec![ExpandedWord {
            value: "a:b:c".into(),
            split_fields: true,
            word_break: false,
        }];
        let result = field_split(&fragments, ":");
        assert_eq!(result, vec!["a", "b", "c"]);
    }

    #[test]
    fn field_split_mixed() {
        let fragments = vec![
            ExpandedWord {
                value: "prefix-".into(),
                split_fields: false,
                word_break: false,
            },
            ExpandedWord {
                value: "a b".into(),
                split_fields: true,
                word_break: false,
            },
        ];
        let result = field_split(&fragments, " \t\n");
        assert_eq!(result, vec!["prefix-a", "b"]);
    }

    #[test]
    fn expand_exit_status() {
        let mut vars = make_vars();
        let word = make_word(vec![WordPart::Param(ParamExpr {
            name: "?".into(),
            op: ParamOp::Normal,
            span: Span::default(),
        })]);
        assert_eq!(
            expand_word_to_fields(&word, &mut vars, 42, 1, &mut None),
            vec!["42"]
        );
    }
}
