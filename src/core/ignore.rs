//! `.packctlignore` rules.
//!
//! A `.packctlignore` file in the server root lists paths that packctl must
//! never touch: they are excluded from the pack-owned sweep (never removed)
//! and never updated from the upstream pack. This is the escape hatch for
//! runtime data that mods write inside pack-owned folders — for example a
//! permission plugin's storage — when the full-sweep model would otherwise
//! reset it on every update. The overlay is still applied to ignored paths,
//! since putting a file in the overlay is the user's explicit choice.
//!
//! Format (gitignore-flavoured, deliberately simpler):
//!
//! ```text
//! # comments and blank lines are ignored
//! config/luckperms        # this directory and everything under it
//! defaultconfigs/ftbranks
//! config/*.bak            # * matches within one segment
//! **/tmp                  # ** matches across segments
//! ```
//!
//! A pattern matches the named relative path and, because it is a directory
//! prefix, everything beneath it. `*` and `?` match within a single path
//! segment, `**` matches zero or more segments. Patterns are always relative
//! to the server root: leading and trailing `/` are dropped, so `/mods` is
//! `mods`. `.` and `..` components and empty segments (`a//b`) are
//! rejected. Negation (`!`) is not supported in V1.

use std::path::Path;

use crate::error::{PackError, Result};

/// Compiled `.packctlignore` rules.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IgnoreRules {
    patterns: Vec<Pattern>,
}

impl IgnoreRules {
    /// True when there are no rules.
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// True when `rel` matches any rule and must not be touched by the pack
    /// side of an update.
    pub fn is_ignored(&self, rel: &Path) -> bool {
        if self.patterns.is_empty() {
            return false;
        }
        let key = rel.to_string_lossy().replace('\\', "/");
        let segments: Vec<&str> = key.split('/').collect();
        self.patterns
            .iter()
            .any(|pattern| pattern.matches_prefix(&segments))
    }
}

/// One compiled rule.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pattern {
    /// Path segments; `None` for a `**` segment.
    segments: Vec<Option<String>>,
}

impl Pattern {
    /// True when the pattern matches a prefix of `path` (a pattern naming a
    /// directory therefore covers everything beneath it).
    fn matches_prefix(&self, path: &[&str]) -> bool {
        let plen = self.segments.len();
        let dlen = path.len();
        let mut dp = vec![vec![false; dlen + 1]; plen + 1];
        dp[0][0] = true;
        for i in 0..=plen {
            for j in 0..=dlen {
                if !dp[i][j] || i == plen {
                    continue;
                }
                match &self.segments[i] {
                    // `**` matches zero or more path segments.
                    None => {
                        dp[i + 1][j] = true;
                        if j < dlen {
                            dp[i][j + 1] = true;
                        }
                    }
                    Some(segment) => {
                        if j < dlen && segment_matches(segment, path[j]) {
                            dp[i + 1][j + 1] = true;
                        }
                    }
                }
            }
        }
        dp[plen].iter().any(|matched| *matched)
    }
}

/// Matches one path segment against a segment pattern supporting `*` and `?`.
fn segment_matches(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (m, n) = (p.len(), t.len());
    let mut dp = vec![vec![false; n + 1]; m + 1];
    dp[0][0] = true;
    for i in 1..=m {
        if p[i - 1] == '*' {
            dp[i][0] = dp[i - 1][0];
        }
    }
    for i in 1..=m {
        for j in 1..=n {
            dp[i][j] = match p[i - 1] {
                '*' => dp[i - 1][j] || dp[i][j - 1],
                '?' => dp[i - 1][j - 1],
                ch => dp[i - 1][j - 1] && ch == t[j - 1],
            };
        }
    }
    dp[m][n]
}

/// Cuts an inline comment: a `#` that follows whitespace starts a comment,
/// so `config/luckperms   # the whole tree` is the rule `config/luckperms`.
/// A `#` inside a segment (`config/a#b`) is part of the path.
fn strip_inline_comment(line: &str) -> &str {
    let mut previous_is_space = false;
    for (index, c) in line.char_indices() {
        if c == '#' && previous_is_space {
            return &line[..index];
        }
        previous_is_space = c.is_whitespace();
    }
    line
}

/// First line of the block `packctl adopt` maintains in `.packctlignore`.
pub const ADOPT_BLOCK_BEGIN: &str = "# >>> packctl adopt";
/// Last line of the block `packctl adopt` maintains in `.packctlignore`.
pub const ADOPT_BLOCK_END: &str = "# <<< packctl adopt";

/// Longest block comment kept, in characters.
const MAX_BLOCK_COMMENT: usize = 120;

/// Replaces packctl's managed block in a `.packctlignore` file, or appends it.
///
/// The block is `{BEGIN}\n# {comment}\n{rules, one per line}\n{END}\n`. When
/// `existing` already has a block (from its BEGIN line to its END line,
/// inclusive) that block is replaced in place; otherwise the block is
/// appended after one blank line. Everything outside the block is kept byte
/// for byte, so rules the admin wrote are never touched.
///
/// `comment` comes partly from the provider (a version name), so control
/// characters are replaced and its length is capped; a comment or rule that
/// would contain one of the block markers, or a rule spanning lines, is
/// refused rather than written.
pub fn upsert_managed_block(existing: &str, comment: &str, rules: &[String]) -> Result<String> {
    let comment: String = comment
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_BLOCK_COMMENT)
        .collect();
    let comment = comment.trim();
    let has_marker =
        |text: &str| text.contains(ADOPT_BLOCK_BEGIN) || text.contains(ADOPT_BLOCK_END);
    if has_marker(comment) {
        return Err(PackError::Config(
            "refusing to write a .packctlignore block whose comment contains the block markers"
                .to_string(),
        ));
    }
    for rule in rules {
        if rule.contains(['\n', '\r']) || has_marker(rule) || rule.trim().starts_with('#') {
            return Err(PackError::Config(format!(
                "refusing to write the .packctlignore rule '{}'",
                rule.escape_debug()
            )));
        }
    }

    let mut block = format!("{ADOPT_BLOCK_BEGIN}\n");
    if !comment.is_empty() {
        block.push_str(&format!("# {comment}\n"));
    }
    for rule in rules {
        block.push_str(rule);
        block.push('\n');
    }
    block.push_str(ADOPT_BLOCK_END);
    block.push('\n');

    if let Some((start, end)) = managed_block_span(existing) {
        let mut out = String::with_capacity(existing.len() + block.len());
        out.push_str(&existing[..start]);
        out.push_str(&block);
        out.push_str(&existing[end..]);
        return Ok(out);
    }
    let mut out = existing.to_string();
    if !out.is_empty() {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        if !out.ends_with("\n\n") {
            out.push('\n');
        }
    }
    out.push_str(&block);
    Ok(out)
}

/// True when `content` holds packctl's managed block.
pub fn has_managed_block(content: &str) -> bool {
    managed_block_span(content).is_some()
}

/// `content` without packctl's managed block: only the rules the admin wrote.
///
/// `packctl adopt` classifies a server against these, not the whole file, so
/// re-adopting re-derives every path the previous block covered (and the
/// new block, which replaces the old one, protects them again) instead of
/// skipping them and dropping their rules.
pub fn without_managed_block(content: &str) -> std::borrow::Cow<'_, str> {
    match managed_block_span(content) {
        Some((start, end)) => {
            let mut out = String::with_capacity(content.len() - (end - start));
            out.push_str(&content[..start]);
            out.push_str(&content[end..]);
            std::borrow::Cow::Owned(out)
        }
        None => std::borrow::Cow::Borrowed(content),
    }
}

/// Byte range of the managed block: from the start of its BEGIN line to just
/// after its END line (including that line's newline, when there is one).
fn managed_block_span(content: &str) -> Option<(usize, usize)> {
    let mut offset = 0;
    let mut start = None;
    for line in content.split_inclusive('\n') {
        let text = line.trim_end_matches(['\n', '\r']);
        match start {
            None if text == ADOPT_BLOCK_BEGIN => start = Some(offset),
            Some(begin) if text == ADOPT_BLOCK_END => {
                return Some((begin, offset + line.len()));
            }
            _ => {}
        }
        offset += line.len();
    }
    None
}

/// The rule that ignores exactly `rel` (a forward-slash relative path) and
/// everything beneath it, or `None` when no rule can express it safely.
///
/// A path containing `#`, `*` or `?`, or a segment with leading or trailing
/// whitespace, cannot be written as a literal rule. The rule must also
/// round-trip: parsed back, it must ignore `rel` and must not ignore a
/// sibling whose last segment differs.
pub fn rule_for_exact_path(rel: &str) -> Option<String> {
    let rel = rel.replace('\\', "/");
    if rel.is_empty()
        || rel.contains(['#', '*', '?', '\n', '\r'])
        || rel.starts_with('!')
        || rel.chars().any(char::is_control)
    {
        return None;
    }
    if rel
        .split('/')
        .any(|segment| segment.is_empty() || segment.trim() != segment)
    {
        return None;
    }
    let rules = parse_rules(&rel).ok()?;
    if !rules.is_ignored(Path::new(&rel)) {
        return None;
    }
    let sibling = format!("{rel}~sibling");
    if rules.is_ignored(Path::new(&sibling)) {
        return None;
    }
    Some(rel)
}

/// Parses the contents of a `.packctlignore` file.
///
/// Blank lines and `#` comments are skipped, including a comment that
/// follows a rule after whitespace. Leading and trailing slashes are
/// dropped, so every rule is relative to the server root. Rejects `.`/`..`
/// components, empty segments, and negation patterns with a contextual
/// error.
pub fn parse_rules(content: &str) -> Result<IgnoreRules> {
    let mut patterns = Vec::new();
    for (index, raw) in content.lines().enumerate() {
        let line = strip_inline_comment(raw).trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('!') {
            return Err(PackError::Config(format!(
                "invalid .packctlignore line {}: negation (!) is not supported",
                index + 1
            )));
        }
        let trimmed = line.trim_matches('/');
        let mut segments = Vec::new();
        for part in trimmed.split('/') {
            if part.is_empty() {
                return Err(PackError::Config(format!(
                    "invalid .packctlignore line {}: empty path segment",
                    index + 1
                )));
            }
            if part == "." || part == ".." {
                return Err(PackError::Config(format!(
                    "invalid .packctlignore line {}: unsafe path component '{part}'",
                    index + 1
                )));
            }
            segments.push(if part == "**" {
                None
            } else {
                Some(part.to_string())
            });
        }
        patterns.push(Pattern { segments });
    }
    Ok(IgnoreRules { patterns })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(contents: &str) -> IgnoreRules {
        parse_rules(contents).unwrap()
    }

    #[test]
    fn matches_directories_and_their_contents() {
        let rules = rules("config/luckperms\ndefaultconfigs/ftbranks\n");
        assert!(rules.is_ignored(Path::new("config/luckperms")));
        assert!(rules.is_ignored(Path::new("config/luckperms/luckperms.conf")));
        assert!(rules.is_ignored(Path::new("config/luckperms/luckperms-h2-v2.mv.db")));
        assert!(rules.is_ignored(Path::new("defaultconfigs/ftbranks/ranks.snbt")));
        assert!(!rules.is_ignored(Path::new("config/jei-server.toml")));
        assert!(!rules.is_ignored(Path::new("mods/foo.jar")));
    }

    #[test]
    fn matches_exact_files() {
        let rules = rules("config/jei-server.toml\n");
        assert!(rules.is_ignored(Path::new("config/jei-server.toml")));
        assert!(!rules.is_ignored(Path::new("config/jei-client.toml")));
        assert!(!rules.is_ignored(Path::new("jei-server.toml")));
    }

    #[test]
    fn star_matches_within_a_segment() {
        let rules = rules("config/*.bak\n");
        assert!(rules.is_ignored(Path::new("config/create_shimmer-server-1.toml.bak")));
        // Like gitignore, a matched prefix is treated as a directory, so
        // children of a matched path are ignored too.
        assert!(rules.is_ignored(Path::new("config/x.bak/jar")));
        // Patterns with a slash are anchored to the server root.
        assert!(!rules.is_ignored(Path::new("sub/config/x.bak")));
    }

    #[test]
    fn double_star_matches_across_segments() {
        let rules = rules("**/spark\n");
        assert!(rules.is_ignored(Path::new("spark")));
        assert!(rules.is_ignored(Path::new("config/spark")));
        assert!(rules.is_ignored(Path::new("config/spark/tmp/x.jfr")));
        assert!(!rules.is_ignored(Path::new("sparky")));
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let rules = rules("# comment\n\n   \nmods/cache\n");
        assert_eq!(rules.patterns.len(), 1);
        assert!(rules.is_ignored(Path::new("mods/cache/x")));
    }

    #[test]
    fn trailing_and_leading_slashes_are_ignored() {
        let rules = rules("/config/luckperms/\n");
        assert!(rules.is_ignored(Path::new("config/luckperms/x")));
    }

    #[test]
    fn empty_rules_ignore_nothing() {
        let rules = rules("");
        assert!(rules.is_empty());
        assert!(!rules.is_ignored(Path::new("anything")));
    }

    #[test]
    fn backslash_paths_are_normalized() {
        let rules = rules("config/luckperms\n");
        assert!(rules.is_ignored(Path::new("config\\luckperms\\x")));
    }

    #[test]
    fn invalid_lines_are_rejected() {
        assert!(parse_rules("!negate\n").is_err());
        assert!(parse_rules("../escape\n").is_err());
        assert!(parse_rules("a//b\n").is_err());
        assert!(parse_rules("a/./b\n").is_err());
        assert!(
            parse_rules("/abs/path\n").is_ok(),
            "leading slash is allowed"
        );
    }

    #[test]
    fn inline_comments_after_whitespace_are_stripped() {
        let parsed = rules("config/luckperms/   # the whole directory tree\nmods/a#b.jar\n");
        assert!(parsed.is_ignored(Path::new("config/luckperms/x.db")));
        assert!(parsed.is_ignored(Path::new("config/luckperms")));
        // A '#' inside a segment is part of the path.
        assert!(parsed.is_ignored(Path::new("mods/a#b.jar")));
        assert!(rules("config/a\t# tab comment\n").is_ignored(Path::new("config/a/b")));
    }

    #[test]
    fn upsert_block_appends_then_replaces_idempotently() {
        let user = "# my rules\nconfig/luckperms\n";
        let once =
            upsert_managed_block(user, "2026-11-18 8.2 (id 1)", &["mods/x.jar".into()]).unwrap();
        assert_eq!(
            once,
            "# my rules\nconfig/luckperms\n\n# >>> packctl adopt\n# 2026-11-18 8.2 (id 1)\nmods/x.jar\n# <<< packctl adopt\n"
        );
        let twice =
            upsert_managed_block(&once, "2026-11-18 8.2 (id 1)", &["mods/x.jar".into()]).unwrap();
        assert_eq!(twice, once, "same input is a no-op");

        let replaced = upsert_managed_block(&once, "later", &["a/b".into(), "c/d".into()]).unwrap();
        assert_eq!(
            replaced,
            "# my rules\nconfig/luckperms\n\n# >>> packctl adopt\n# later\na/b\nc/d\n# <<< packctl adopt\n"
        );
        let parsed = parse_rules(&replaced).unwrap();
        assert!(parsed.is_ignored(Path::new("a/b")));
        assert!(!parsed.is_ignored(Path::new("mods/x.jar")));

        assert_eq!(
            upsert_managed_block("", "c", &["x/y".into()]).unwrap(),
            "# >>> packctl adopt\n# c\nx/y\n# <<< packctl adopt\n"
        );
    }

    #[test]
    fn preserves_user_rules_outside_block() {
        let existing = "before/a\n# >>> packctl adopt\n# old\nold/rule\n# <<< packctl adopt\nafter/b   # keep me\r\nno-newline";
        let out = upsert_managed_block(existing, "new", &["new/rule".into()]).unwrap();
        assert_eq!(
            out,
            "before/a\n# >>> packctl adopt\n# new\nnew/rule\n# <<< packctl adopt\nafter/b   # keep me\r\nno-newline"
        );
        // A file without a trailing newline gets one before the block.
        assert_eq!(
            upsert_managed_block("a/b", "c", &[]).unwrap(),
            "a/b\n\n# >>> packctl adopt\n# c\n# <<< packctl adopt\n"
        );
    }

    #[test]
    fn without_managed_block_keeps_only_the_admins_rules() {
        let existing =
            "before/a\n# >>> packctl adopt\n# old\nold/rule\n# <<< packctl adopt\nafter/b\n";
        assert_eq!(without_managed_block(existing), "before/a\nafter/b\n");
        let rules = parse_rules(&without_managed_block(existing)).unwrap();
        assert!(rules.is_ignored(Path::new("after/b")));
        assert!(!rules.is_ignored(Path::new("old/rule")));
        // No block (or an unterminated one): the file as it is.
        assert_eq!(without_managed_block("a/b\n"), "a/b\n");
        let open = "a/b\n# >>> packctl adopt\nc/d\n";
        assert_eq!(without_managed_block(open), open);
    }

    #[test]
    fn block_comment_is_sanitized_and_markers_refused() {
        let out = upsert_managed_block("", "8.2\nmods\n**", &["a/b".into()]).unwrap();
        let parsed = parse_rules(&out).unwrap();
        assert!(!parsed.is_ignored(Path::new("mods/x.jar")), "{out}");
        assert!(!parsed.is_ignored(Path::new("config/x")), "{out}");
        assert!(upsert_managed_block("", "x # <<< packctl adopt", &[]).is_err());
        assert!(upsert_managed_block("", "c", &["a\nb".into()]).is_err());
        let long = "v".repeat(500);
        let out = upsert_managed_block("", &long, &[]).unwrap();
        assert!(out.lines().nth(1).unwrap().len() <= MAX_BLOCK_COMMENT + 2);
    }

    #[test]
    fn rule_for_exact_path_rejects_hash_and_glob_chars() {
        assert_eq!(
            rule_for_exact_path("config/luckperms").as_deref(),
            Some("config/luckperms")
        );
        assert_eq!(
            rule_for_exact_path("mods/my mod.jar").as_deref(),
            Some("mods/my mod.jar")
        );
        for bad in [
            "config/a#b",
            "config/a #b",
            "config/*.db",
            "config/a?.db",
            "config/ lead",
            "config/trail ",
            "!mods/x",
            "a//b",
            "../x",
            "",
        ] {
            assert_eq!(rule_for_exact_path(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn question_mark_matches_single_char() {
        let rules = rules("config/cache?.db\n");
        assert!(rules.is_ignored(Path::new("config/cache1.db")));
        assert!(!rules.is_ignored(Path::new("config/cache12.db")));
    }
}
