use std::path::Path;

/// Strip ASCII control characters (0x00–0x1F) except TAB (0x09) and LF (0x0A),
/// then truncate to `max_len` Unicode characters.
///
/// This prevents prompt-injection via adversarially named symbols extracted from
/// source code (e.g. `IGNORE ALL PREVIOUS INSTRUCTIONS`).
pub fn sanitize_name(s: &str) -> String {
    sanitize_name_with_len(s, 256)
}

/// Like [`sanitize_name`] but with a configurable maximum length.
pub fn sanitize_name_with_len(s: &str, max_len: usize) -> String {
    let cleaned: String = s
        .chars()
        .filter(|&ch| ch == '\t' || ch == '\n' || (ch as u32) >= 0x20)
        .collect();
    cleaned.chars().take(max_len).collect()
}

/// Validate that `root` is a plausible project root.
///
/// Canonicalises the path, checks it exists as a directory, and verifies that
/// at least one VCS/tool marker (`.git`, `.svn`, `.code-review-graph`) is present.
/// Returns `Err` if any check fails (path traversal defence).
pub fn validate_repo_root(root: &Path) -> anyhow::Result<()> {
    let resolved = root
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("Cannot canonicalize repo_root {:?}: {}", root, e))?;

    if !resolved.is_dir() {
        anyhow::bail!("repo_root is not an existing directory: {:?}", resolved);
    }

    let has_vcs = resolved.join(".git").exists()
        || resolved.join(".svn").exists()
        || resolved.join(".code-review-graph").exists();

    if !has_vcs {
        anyhow::bail!(
            "repo_root does not look like a project root \
             (no .git, .svn, or .code-review-graph directory found): {:?}",
            resolved
        );
    }

    Ok(())
}

/// Validate a git ref name against a safe allow-list pattern.
///
/// Accepted characters: `A-Za-z0-9_.~^/@{}\-`
pub fn validate_git_ref(ref_name: &str) -> anyhow::Result<()> {
    use regex::Regex;
    // Compile once via a thread-local to avoid repeated construction.
    thread_local! {
        static RE: Regex = Regex::new(r"^[A-Za-z0-9_.~^/@{}\-]+$").unwrap();
    }
    let ok = RE.with(|re| re.is_match(ref_name));
    if !ok {
        anyhow::bail!("Invalid git ref name: {:?}", ref_name);
    }
    Ok(())
}

/// Escape a string for safe embedding in HTML/JavaScript contexts.
///
/// Escapes `&`, `<`, `>`, `"`, `'`, and `` ` `` — matching the JavaScript
/// `escH()` function in the D3.js visualization.
pub fn esc_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            '`' => out.push_str("&#96;"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_control_chars() {
        let s = "hello\x00world\x01\x1fend";
        assert_eq!(sanitize_name(s), "helloworldend");
    }

    #[test]
    fn sanitize_preserves_tab_and_newline() {
        let s = "foo\tbar\nbaz";
        assert_eq!(sanitize_name(s), "foo\tbar\nbaz");
    }

    #[test]
    fn sanitize_truncates_at_256() {
        let long: String = "a".repeat(300);
        assert_eq!(sanitize_name(&long).len(), 256);
    }

    #[test]
    fn esc_html_escapes_all_special_chars() {
        assert_eq!(esc_html("<script>&\"'`</script>"), "&lt;script&gt;&amp;&quot;&#39;&#96;&lt;/script&gt;");
    }

    #[test]
    fn validate_git_ref_accepts_valid() {
        assert!(validate_git_ref("main").is_ok());
        assert!(validate_git_ref("refs/heads/feature-branch").is_ok());
        assert!(validate_git_ref("v1.2.3").is_ok());
    }

    #[test]
    fn validate_git_ref_rejects_spaces() {
        assert!(validate_git_ref("bad ref").is_err());
    }
}
