//! Extraction of variable references from raw (unexpanded) argument text.
//!
//! Handles nesting: `${${prefix}_LIBS}` yields the statically-known inner
//! name `prefix`; the outer reference is dynamic and cannot be named
//! statically, so it is skipped (design §1.2 — resolving it requires the
//! trace, which is exactly what the events table provides).

/// Statically-resolvable variable names referenced via `${NAME}` or
/// `$CACHE{NAME}`. `$ENV{...}` refs are excluded (they read the environment,
/// not CMake variables). Bracket arguments must not be passed here — they
/// suppress expansion entirely.
pub fn static_var_refs(text: &str) -> Vec<String> {
    // Bracket arguments suppress expansion; cheap guard for callers that
    // pass raw text.
    if text.starts_with("[[") || text.starts_with("[=") {
        return Vec::new();
    }
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    // Stack entries: (start-of-name index, is_dynamic, kind); kind: '$' for
    // ${}, 'C' for $CACHE{}, 'E' for $ENV{}.
    let mut stack: Vec<(usize, bool, u8)> = Vec::new();
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1, // skip escaped char
            b'$' => {
                if bytes.get(i + 1) == Some(&b'{') {
                    stack.push((i + 2, false, b'$'));
                    i += 1;
                } else if text[i + 1..].starts_with("ENV{") {
                    stack.push((i + 5, false, b'E'));
                    i += 4;
                } else if text[i + 1..].starts_with("CACHE{") {
                    stack.push((i + 7, false, b'C'));
                    i += 6;
                }
            }
            b'}' => {
                if let Some((start, dynamic, kind)) = stack.pop() {
                    if !dynamic && kind != b'E' {
                        let name = &text[start..i];
                        if !name.is_empty() && !name.contains('$') {
                            out.push(name.to_string());
                        }
                    }
                    // The enclosing reference (if any) has a dynamic name.
                    if let Some(top) = stack.last_mut() {
                        top.1 = true;
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// Statically-named environment references: the `NAME` in `$ENV{NAME}`.
///
/// Deliberately a *separate* extractor from [`static_var_refs`], whose
/// exact semantics (env excluded) several passes depend on: `$ENV{X}`
/// reads the process environment, not a CMake variable. Dynamic outers
/// (`$ENV{${v}}`) cannot be named statically and are skipped — their
/// inner `${v}` is already reported by [`static_var_refs`]. Bracket
/// arguments must not be passed here (expansion suppressed).
pub fn env_var_refs(text: &str) -> Vec<String> {
    if text.starts_with("[[") || text.starts_with("[=") {
        return Vec::new();
    }
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    // Same stack machine as static_var_refs; here we harvest kind 'E'.
    let mut stack: Vec<(usize, bool, u8)> = Vec::new();
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 1, // skip escaped char
            b'$' => {
                if bytes.get(i + 1) == Some(&b'{') {
                    stack.push((i + 2, false, b'$'));
                    i += 1;
                } else if text[i + 1..].starts_with("ENV{") {
                    stack.push((i + 5, false, b'E'));
                    i += 4;
                } else if text[i + 1..].starts_with("CACHE{") {
                    stack.push((i + 7, false, b'C'));
                    i += 6;
                }
            }
            b'}' => {
                if let Some((start, dynamic, kind)) = stack.pop() {
                    if !dynamic && kind == b'E' {
                        let name = &text[start..i];
                        if !name.is_empty() && !name.contains('$') {
                            out.push(name.to_string());
                        }
                    }
                    // The enclosing reference (if any) has a dynamic name.
                    if let Some(top) = stack.last_mut() {
                        top.1 = true;
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple() {
        assert_eq!(static_var_refs("${FOO}"), vec!["FOO"]);
    }

    #[test]
    fn nested_only_inner() {
        assert_eq!(static_var_refs("${${prefix}_LIBRARIES}"), vec!["prefix"]);
    }

    #[test]
    fn env_excluded_cache_included() {
        assert_eq!(static_var_refs("$ENV{PATH}"), Vec::<String>::new());
        assert_eq!(static_var_refs("$CACHE{OPT}"), vec!["OPT"]);
    }

    #[test]
    fn multiple_and_embedded() {
        assert_eq!(static_var_refs("prefix-${A}/mid/${B}.txt"), vec!["A", "B"]);
    }

    #[test]
    fn escaped_dollar() {
        assert_eq!(static_var_refs("\\${NOT}"), Vec::<String>::new());
    }

    #[test]
    fn env_nested_inner_still_counts() {
        // $ENV{${v}} -> inner ${v} is a real variable read
        assert_eq!(static_var_refs("$ENV{${v}}"), vec!["v"]);
    }

    #[test]
    fn env_refs_simple_and_embedded() {
        assert_eq!(env_var_refs("$ENV{PATH}"), vec!["PATH"]);
        assert_eq!(env_var_refs("a-$ENV{A}/x/$ENV{B}.txt"), vec!["A", "B"]);
    }

    #[test]
    fn env_refs_exclude_plain_and_cache() {
        assert_eq!(env_var_refs("${FOO}"), Vec::<String>::new());
        assert_eq!(env_var_refs("$CACHE{OPT}"), Vec::<String>::new());
        assert_eq!(env_var_refs("${FOO}$ENV{BAR}"), vec!["BAR"]);
    }

    #[test]
    fn env_refs_dynamic_outer_skipped() {
        // $ENV{${v}}: the env name is dynamic — cannot be named statically.
        assert_eq!(env_var_refs("$ENV{${v}}"), Vec::<String>::new());
        // But a static env ref next to it still counts.
        assert_eq!(env_var_refs("$ENV{${v}}$ENV{HOME}"), vec!["HOME"]);
    }

    #[test]
    fn env_refs_escaped_and_bracket_suppressed() {
        assert_eq!(env_var_refs("\\$ENV{NOT}"), Vec::<String>::new());
        assert_eq!(env_var_refs("[[$ENV{NOT}]]"), Vec::<String>::new());
    }
}
