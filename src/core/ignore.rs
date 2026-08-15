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
//! segment, `**` matches zero or more segments. Absolute paths, `..`
//! components, and empty segments are rejected. Negation (`!`) is not
//! supported in V1.

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

/// Parses the contents of a `.packctlignore` file.
///
/// Blank lines and `#` comments are skipped. Leading and trailing slashes are
/// ignored. Rejects absolute paths, `.`/`..` components, empty segments, and
/// negation patterns with a contextual error.
pub fn parse_rules(content: &str) -> Result<IgnoreRules> {
    let mut patterns = Vec::new();
    for (index, raw) in content.lines().enumerate() {
        let line = raw.trim();
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
    fn question_mark_matches_single_char() {
        let rules = rules("config/cache?.db\n");
        assert!(rules.is_ignored(Path::new("config/cache1.db")));
        assert!(!rules.is_ignored(Path::new("config/cache12.db")));
    }
}
