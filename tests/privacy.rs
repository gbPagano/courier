//! Privacy guard for the observability layer.
//!
//! The docs (`docs/concepts/observability.md`) promise that no logging
//! call references the envelope payload, so customer data cannot leak to
//! log aggregators. This test enforces that promise: it walks every
//! `.rs` file under `src/`, parses each tracing/log macro invocation,
//! and fails the build if any of them touch `.payload`.

use std::path::Path;

/// Macros whose arguments must not reference `.payload`. Both `tracing::*!`
/// and `log::*!` are covered because the runtime bridges `log` events into
/// the `tracing` pipeline via `LogTracer`.
const LOGGING_MACROS: &[&str] = &[
    "tracing::info!",
    "tracing::debug!",
    "tracing::warn!",
    "tracing::error!",
    "tracing::trace!",
    "tracing::info_span!",
    "tracing::debug_span!",
    "tracing::warn_span!",
    "tracing::error_span!",
    "tracing::trace_span!",
    "log::info!",
    "log::debug!",
    "log::warn!",
    "log::error!",
    "log::trace!",
];

#[test]
fn logging_macros_do_not_reference_envelope_payload() {
    let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut violations = Vec::new();
    walk(&src_dir, &mut violations);
    assert!(
        violations.is_empty(),
        "found logging macro invocations referencing `.payload` — payloads must never reach log or trace exports:\n{}",
        violations.join("\n"),
    );
}

fn walk(dir: &Path, violations: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).expect("failed to read src/") {
        let entry = entry.expect("failed to stat dir entry");
        let path = entry.path();
        if path.is_dir() {
            walk(&path, violations);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            scan_file(&path, violations);
        }
    }
}

fn scan_file(path: &Path, violations: &mut Vec<String>) {
    let content =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    for needle in LOGGING_MACROS {
        let mut start = 0;
        while let Some(rel) = content[start..].find(needle) {
            let macro_pos = start + rel;
            let after_macro = macro_pos + needle.len();
            // tracing/log macros always open with `(` — `info!{...}` and
            // `info![...]` are syntactically valid but unused here.
            let Some(rel_open) = content[after_macro..].find('(') else {
                break;
            };
            let open_pos = after_macro + rel_open;
            let Some(close_pos) = find_close_paren(&content, open_pos) else {
                break;
            };
            let body = &content[open_pos..=close_pos];
            if body.contains(".payload") {
                let line_no = content[..macro_pos].bytes().filter(|&b| b == b'\n').count() + 1;
                violations.push(format!("  {}:{} {needle}", path.display(), line_no));
            }
            start = close_pos + 1;
        }
    }
}

/// Mirror of `scan_file` for an in-memory snippet, used by the
/// detector self-tests below.
#[cfg(test)]
fn scan_snippet(source: &str) -> Vec<&'static str> {
    let mut hits = Vec::new();
    for needle in LOGGING_MACROS {
        let mut start = 0;
        while let Some(rel) = source[start..].find(needle) {
            let macro_pos = start + rel;
            let after_macro = macro_pos + needle.len();
            let Some(rel_open) = source[after_macro..].find('(') else {
                break;
            };
            let open_pos = after_macro + rel_open;
            let Some(close_pos) = find_close_paren(source, open_pos) else {
                break;
            };
            if source[open_pos..=close_pos].contains(".payload") {
                hits.push(*needle);
            }
            start = close_pos + 1;
        }
    }
    hits
}

/// Position of the `)` matching the `(` at `open_pos`, skipping content
/// inside string literals and comments so embedded `)` characters don't
/// end the macro call early.
fn find_close_paren(content: &str, open_pos: usize) -> Option<usize> {
    let bytes = content.as_bytes();
    debug_assert_eq!(bytes[open_pos], b'(');
    enum S {
        Code,
        Str,
        LineComment,
        BlockComment,
    }
    let mut state = S::Code;
    let mut depth: i32 = 0;
    let mut i = open_pos;
    while i < bytes.len() {
        let c = bytes[i];
        match state {
            S::Code => match c {
                b'"' => state = S::Str,
                b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => state = S::LineComment,
                b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                    state = S::BlockComment;
                    i += 1;
                }
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i);
                    }
                }
                _ => {}
            },
            S::Str => match c {
                b'\\' => i += 1, // skip the escaped byte
                b'"' => state = S::Code,
                _ => {}
            },
            S::LineComment => {
                if c == b'\n' {
                    state = S::Code;
                }
            }
            S::BlockComment => {
                if c == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                    state = S::Code;
                    i += 1;
                }
            }
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod self_tests {
    use super::scan_snippet;

    #[test]
    fn flags_payload_field_inside_tracing_call() {
        let hits = scan_snippet(r#"tracing::info!(p = ?env.payload, "x");"#);
        assert_eq!(hits, vec!["tracing::info!"]);
    }

    #[test]
    fn flags_payload_inside_multiline_span() {
        let snippet = r#"
            tracing::info_span!(
                "courier.transform",
                payload_dump = ?env.payload,
            );
        "#;
        assert_eq!(scan_snippet(snippet), vec!["tracing::info_span!"]);
    }

    #[test]
    fn ignores_word_payload_in_string_literal() {
        // String contains the substring "payload" but not the `.payload`
        // field access — the existing kafka source does exactly this.
        let snippet = r#"log::error!("message at offset {offset} has no payload");"#;
        assert!(scan_snippet(snippet).is_empty());
    }

    #[test]
    fn ignores_payload_outside_logging_macro() {
        let snippet = r#"
            let body = serde_json::to_string(&env.payload)?;
            tracing::debug!(node_id = %id, "wrote body");
        "#;
        assert!(scan_snippet(snippet).is_empty());
    }

    #[test]
    fn handles_string_with_embedded_close_paren() {
        // The `)` inside the string must not prematurely close the macro
        // call — otherwise the parser would miss the trailing `.payload`.
        let snippet = r#"tracing::warn!("hit (limit)", value = ?out.payload);"#;
        assert_eq!(scan_snippet(snippet), vec!["tracing::warn!"]);
    }
}
