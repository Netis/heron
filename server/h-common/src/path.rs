//! Path-component utilities shared across crates that build per-source or
//! per-pipeline directories on disk (currently `pcap_dump` and the runtime
//! wiring in the binary). Lives in `h-common` so config validation can
//! apply the exact same rule the dumper uses at runtime — keeping the
//! "is this name safe?" answer single-sourced.

/// Replace any byte outside `[A-Za-z0-9._-]` with `_`. Returns `None` for
/// empty input or for any input that, after substitution, is `.` or `..` —
/// those would create ambiguous or traversal-prone path components.
pub fn sanitize_path_component(s: &str) -> Option<String> {
    if s.is_empty() {
        return None;
    }
    let out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out == "." || out == ".." {
        return None;
    }
    Some(out)
}

/// True iff [`sanitize_path_component`] would accept `s`. Cheaper than the
/// full sanitize when callers only need the predicate (e.g. config
/// validation that just wants to know "would this work at runtime?").
pub fn is_safe_path_component(s: &str) -> bool {
    sanitize_path_component(s).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_paths_pass_through() {
        assert_eq!(sanitize_path_component("local"), Some("local".into()));
        assert_eq!(sanitize_path_component("eth0"), Some("eth0".into()));
        assert_eq!(sanitize_path_component("a-b.c_d"), Some("a-b.c_d".into()));
    }

    #[test]
    fn special_chars_replaced_with_underscore() {
        assert_eq!(sanitize_path_component("a/b"), Some("a_b".into()));
        assert_eq!(sanitize_path_component("a b"), Some("a_b".into()));
        assert_eq!(sanitize_path_component("a:b"), Some("a_b".into()));
    }

    #[test]
    fn empty_dot_dotdot_rejected() {
        assert_eq!(sanitize_path_component(""), None);
        assert_eq!(sanitize_path_component("."), None);
        assert_eq!(sanitize_path_component(".."), None);
    }

    #[test]
    fn predicate_matches_function() {
        for s in ["", ".", "..", "ok", "a/b", "a b"] {
            assert_eq!(
                is_safe_path_component(s),
                sanitize_path_component(s).is_some(),
                "predicate disagreed for {s:?}",
            );
        }
    }
}
