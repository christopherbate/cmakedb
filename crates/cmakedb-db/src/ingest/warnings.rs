//! Parser for CMake's own configure-time stderr diagnostics (lint roadmap
//! Tier 2, `configure-warnings`).
//!
//! CMake writes diagnostics to stderr as blocks. Shapes verified against
//! real CMake 4.2.1 output:
//!
//! ```text
//! CMake Warning (dev) at CMakeLists.txt:3 (message):
//!   custom author warning
//! This warning is for project developers.  Use -Wno-dev to suppress it.
//!
//! CMake Warning at CMakeLists.txt:4 (message):
//!   plain warning here
//!
//! CMake Deprecation Warning at CMakeLists.txt:6 (cmake_policy):
//!   The OLD behavior for policy CMP0079 will be removed ...
//!
//!   (bodies may contain internal blank lines between indented paragraphs)
//!
//! CMake Error at CMakeLists.txt:4:            (no command — parse errors)
//! CMake Warning (dev) in CMakeLists.txt:      (no line)
//! CMake Warning:                              (no location at all, e.g.
//!   Manually-specified variables were not used ...)
//! Call Stack (most recent call first):        (col-0 terminator after a
//!   CMakeLists.txt:3 (include)                 block raised in an include)
//! ```
//!
//! A block's body is the run of indented-or-blank lines after the header;
//! any column-0 line ends it (the developer-warning trailer, a Call Stack
//! section, the next header, or arbitrary interleaved output). cmake's
//! stderr mixes freely with other output, so every line that isn't part of
//! a recognized block is silently skipped — a garbage line must never
//! panic or derail the scan.

/// How many leading body lines are folded into the stored message.
const MESSAGE_LINES: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// `error` | `warning` | `note`.
    pub severity: &'static str,
    /// The parenthesized tag (`dev`, ...), `deprecation` for
    /// "CMake Deprecation Warning" (which carries no tag), or "".
    pub kind: String,
    /// Path exactly as cmake printed it; "" when the block had no location.
    pub file: String,
    /// 1-based; 0 when the block carried no line ("in file:" / no location).
    pub line: i64,
    /// First few body lines, whitespace-trimmed and joined with a space.
    pub message: String,
}

/// Scan captured configure stderr for diagnostic blocks. Infallible by
/// design: unrecognized content is skipped, never an error.
pub fn parse_stderr(text: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let Some((base, tag, file, line_no)) = parse_header(line) else {
            continue;
        };
        // Body: consume indented/blank lines up to (not including) the next
        // column-0 line, which the outer loop re-examines as a header.
        let mut body: Vec<&str> = Vec::new();
        while let Some(next) = lines.peek() {
            if !next.is_empty() && !next.starts_with(' ') && !next.starts_with('\t') {
                break;
            }
            let consumed = lines.next().unwrap_or_default().trim();
            if !consumed.is_empty() && body.len() < MESSAGE_LINES {
                body.push(consumed);
            }
        }
        let message = body.join(" ");
        // Unset-policy advice: CMake 4.x prints it as "CMake Warning (dev)"
        // with a "Policy CMPxxxx is not set" body — reclassify by content
        // so it maps with the (policy) tag some producers spell out.
        let tag =
            if tag == "dev" && message.starts_with("Policy CMP") && message.contains(" is not set")
            {
                "policy".to_string()
            } else {
                tag
            };
        // CMake's own labels → finding severity, in one place:
        //   "CMake Error ..."                 → error
        //   "CMake Deprecation Warning ..."   → warning
        //   "CMake Warning" / "(dev)" author  → warning
        //   policy advice (tag or body form)  → note (advice-grade)
        let severity = match (base, tag.as_str()) {
            (Base::Error, _) => "error",
            (Base::Deprecation, _) => "warning",
            (_, "policy") => "note",
            (Base::Warning, _) => "warning",
        };
        let kind = if !tag.is_empty() {
            tag
        } else if matches!(base, Base::Deprecation) {
            // No parenthesized tag exists for these; keep them queryable.
            "deprecation".to_string()
        } else {
            String::new()
        };
        out.push(Diagnostic {
            severity,
            kind,
            file,
            line: line_no,
            message,
        });
    }
    out
}

#[derive(Clone, Copy)]
enum Base {
    Error,
    Warning,
    Deprecation,
}

/// Parse one block header line into (label, tag, file, line);
/// None for anything else.
fn parse_header(line: &str) -> Option<(Base, String, String, i64)> {
    let rest = line.strip_prefix("CMake ")?;
    // Order matters: "Deprecation Warning" before "Warning".
    let (base, rest) = if let Some(r) = rest.strip_prefix("Deprecation Warning") {
        (Base::Deprecation, r)
    } else if let Some(r) = rest.strip_prefix("Warning") {
        (Base::Warning, r)
    } else {
        (Base::Error, rest.strip_prefix("Error")?)
    };
    // Optional parenthesized tag: "CMake Warning (dev) at ...".
    let (tag, rest) = match rest.strip_prefix(" (") {
        Some(r) => {
            let (t, r2) = r.split_once(')')?;
            (t, r2)
        }
        None => ("", rest),
    };
    // Block headers always end with ':'; single-line driver errors
    // ("CMake Error: The source directory ... does not exist.") don't and
    // fall out here — those only occur when no recording exists anyway.
    let rest = rest.strip_suffix(':')?;
    let (file, line_no) = if let Some(loc) = rest.strip_prefix(" at ") {
        // "<file>:<line> (<command>)" | "<file>:<line>".
        let loc = match loc.rfind(" (") {
            Some(i) if loc.ends_with(')') => &loc[..i],
            _ => loc,
        };
        // rsplit so Windows drive-letter colons stay in the path.
        match loc.rsplit_once(':') {
            Some((f, l)) => match l.parse::<i64>() {
                Ok(n) => (f.to_string(), n),
                Err(_) => (loc.to_string(), 0),
            },
            None => (loc.to_string(), 0),
        }
    } else if let Some(f) = rest.strip_prefix(" in ") {
        // "CMake Warning (dev) in CMakeLists.txt:" — file, no line.
        (f.to_string(), 0)
    } else if rest.is_empty() {
        // "CMake Warning:" — no location (unused -D variables etc.).
        (String::new(), 0)
    } else {
        return None;
    };
    Some((base, tag.to_string(), file, line_no))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dev_warning_with_location_and_command() {
        let d = parse_stderr(
            "CMake Warning (dev) at CMakeLists.txt:3 (message):\n\
             \x20 custom author warning\n\
             This warning is for project developers.  Use -Wno-dev to suppress it.\n",
        );
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].severity, "warning");
        assert_eq!(d[0].kind, "dev");
        assert_eq!(d[0].file, "CMakeLists.txt");
        assert_eq!(d[0].line, 3);
        assert_eq!(d[0].message, "custom author warning");
    }

    #[test]
    fn policy_tagged_warning_is_note() {
        let d = parse_stderr(
            "CMake Warning (policy) at sub/CMakeLists.txt:7 (add_library):\n\
             \x20 Policy CMP0063 is not set.\n",
        );
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].severity, "note");
        assert_eq!(d[0].kind, "policy");
        assert_eq!(d[0].file, "sub/CMakeLists.txt");
        assert_eq!(d[0].line, 7);
    }

    #[test]
    fn unset_policy_dev_warning_reclassified_by_body() {
        // Real CMake 4.x shape: policy advice arrives as "(dev)" but is
        // advice-grade — recognized by body and downgraded to note.
        let d = parse_stderr(
            "CMake Warning (dev) at CMakeLists.txt:5 (add_executable):\n\
             \x20 Policy CMP0156 is not set: De-duplicate libraries on link lines based on\n\
             \x20 linker capabilities.  Run \"cmake --help-policy CMP0156\" for policy\n\
             This warning is for project developers.  Use -Wno-dev to suppress it.\n",
        );
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].severity, "note");
        assert_eq!(d[0].kind, "policy");
        assert_eq!(d[0].line, 5);
    }

    #[test]
    fn deprecation_multiparagraph_body() {
        // Real CMP0079 shape: internal blank line between paragraphs; only
        // the first MESSAGE_LINES body lines fold into the message.
        let d = parse_stderr(
            "CMake Deprecation Warning at CMakeLists.txt:6 (cmake_policy):\n\
             \x20 The OLD behavior for policy CMP0079 will be removed from a future version\n\
             \x20 of CMake.\n\
             \n\
             \x20 The cmake-policies(7) manual explains that the OLD behaviors of all\n\
             \x20 policies are deprecated.\n\
             \n\
             \n",
        );
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].severity, "warning");
        assert_eq!(d[0].kind, "deprecation");
        assert_eq!(d[0].line, 6);
        assert_eq!(
            d[0].message,
            "The OLD behavior for policy CMP0079 will be removed from a future version \
             of CMake. The cmake-policies(7) manual explains that the OLD behaviors of all"
        );
    }

    #[test]
    fn error_without_command_parse_error_shape() {
        let d = parse_stderr(
            "CMake Error at CMakeLists.txt:4:\n\
             \x20 Parse error.  Function missing ending \")\".\n",
        );
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].severity, "error");
        assert_eq!(d[0].kind, "");
        assert_eq!(d[0].file, "CMakeLists.txt");
        assert_eq!(d[0].line, 4);
    }

    #[test]
    fn in_file_without_line_and_no_location_blocks() {
        let d = parse_stderr(
            "CMake Warning (dev) in CMakeLists.txt:\n\
             \x20 No project() command is present.\n\
             This warning is for project developers.  Use -Wno-dev to suppress it.\n\
             \n\
             CMake Warning:\n\
             \x20 Manually-specified variables were not used by the project:\n\
             \n\
             \x20   FOO\n",
        );
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].file, "CMakeLists.txt");
        assert_eq!(d[0].line, 0);
        assert_eq!(d[0].kind, "dev");
        assert_eq!(d[1].file, "");
        assert_eq!(d[1].line, 0);
        assert_eq!(
            d[1].message,
            "Manually-specified variables were not used by the project: FOO"
        );
    }

    #[test]
    fn interleaved_garbage_and_call_stack() {
        // Non-block output mixes freely; Call Stack sections terminate a
        // block and their indented frames must not attach to anything.
        let d = parse_stderr(
            "-- Configuring incomplete, errors occurred!\n\
             random garbage )():: line\n\
             CMake Warning (dev) at cmake/helper.cmake:1 (message):\n\
             \x20 warning from include\n\
             Call Stack (most recent call first):\n\
             \x20 CMakeLists.txt:3 (include)\n\
             This warning is for project developers.  Use -Wno-dev to suppress it.\n\
             more trailing noise\n\
             CMake Error: driver-style single-line error, no block\n",
        );
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].file, "cmake/helper.cmake");
        assert_eq!(d[0].line, 1);
        assert_eq!(d[0].message, "warning from include");
    }

    #[test]
    fn empty_and_pure_garbage_input() {
        assert!(parse_stderr("").is_empty());
        assert!(parse_stderr("\n\n").is_empty());
        assert!(parse_stderr("no diagnostics here\njust logs\n").is_empty());
        // Truncated/deformed headers must not panic.
        assert!(parse_stderr("CMake Warning (dev\n").is_empty());
        assert!(parse_stderr("CMake Warning at :\n  x\n").len() == 1);
        assert!(parse_stderr("CMake \n").is_empty());
    }
}
