//! Cosmetic comparison of text config files.
//!
//! `packctl adopt` uses this to tell a config the admin only reformatted
//! (comments, blank lines, trailing whitespace) from one they really edited.
//! A reformatted file is accepted as-is instead of being frozen in the
//! overlay. The comparison is deliberately conservative: line order is
//! significant (a value moved between two TOML sections is a real edit),
//! comment lines are only dropped in formats where the marker starts a
//! comment, and a file that may hold multi-line strings (where a `#` line, a
//! blank line or trailing whitespace can be data) is never cosmetic.

/// Largest file compared, in bytes. Bigger files are never cosmetic.
const MAX_COSMETIC_BYTES: usize = 256 * 1024;

/// How full-line comments look in a format.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CommentStyle {
    /// No comment syntax is assumed (`.json`, `.snbt`, `.txt`).
    None,
    /// `#` starts a full-line comment.
    Hash,
    /// `//` starts a full-line comment (JSON5).
    Slash,
}

fn comment_style(ext: &str) -> Option<CommentStyle> {
    match ext.to_ascii_lowercase().as_str() {
        "toml" | "cfg" | "properties" | "conf" | "yml" | "yaml" => Some(CommentStyle::Hash),
        "json5" => Some(CommentStyle::Slash),
        "json" | "snbt" | "txt" => Some(CommentStyle::None),
        _ => None,
    }
}

/// True when [`cosmetic_equal`] can compare files with extension `ext`.
pub fn supports(ext: &str) -> bool {
    comment_style(ext).is_some()
}

/// True when `a` and `b` differ only cosmetically.
///
/// Only text formats packs commonly ship as configs qualify (`toml`, `cfg`,
/// `properties`, `json`, `json5`, `snbt`, `txt`, `conf`, `yml`, `yaml`),
/// and only when both inputs are valid UTF-8 and at most 256 KiB. Each line
/// loses its trailing whitespace; blank lines are dropped, and so are
/// full-line comments where the format has them (`#` for toml, cfg,
/// properties, conf and yaml; `//` for json5). The remaining lines must be
/// equal in order.
///
/// Dropping lines is only sound outside string values, so a file that may
/// hold a multi-line string (see [`may_span_lines`]) is never cosmetic: the
/// caller then treats it as a real edit and keeps it.
pub fn cosmetic_equal(a: &[u8], b: &[u8], ext: &str) -> bool {
    let Some(style) = comment_style(ext) else {
        return false;
    };
    if a.len() > MAX_COSMETIC_BYTES || b.len() > MAX_COSMETIC_BYTES {
        return false;
    }
    let (Ok(a), Ok(b)) = (std::str::from_utf8(a), std::str::from_utf8(b)) else {
        return false;
    };
    if style != CommentStyle::None && (may_span_lines(a, ext) || may_span_lines(b, ext)) {
        return false;
    }
    significant_lines(a, style).eq(significant_lines(b, style))
}

/// True when `text` may contain a string value that spans lines, where a
/// `#`/`//` line, a blank line or trailing whitespace is content.
///
/// Deliberately over-approximates: a false positive only means a reformatted
/// file is kept in the overlay instead of being accepted.
/// - TOML / HOCON triple-quoted strings (`"""`, `'''`).
/// - A line ending in `\` (properties, cfg and json5 continuations).
/// - A Forge `.cfg` list opened with a trailing `<`.
/// - YAML block scalars (`key: |`, `- >-`, ...) and quoted scalars left open
///   at the end of a line.
///
/// `.json`, `.snbt` and `.txt` never reach this: only blank lines and
/// trailing whitespace are dropped there, and JSON strings cannot hold a raw
/// newline.
fn may_span_lines(text: &str, ext: &str) -> bool {
    if text.contains("\"\"\"") || text.contains("'''") {
        return true;
    }
    let yaml = matches!(ext.to_ascii_lowercase().as_str(), "yml" | "yaml");
    text.lines().map(str::trim_end).any(|line| {
        if line.ends_with('\\') || line.ends_with('<') {
            return true;
        }
        yaml && (yaml_block_scalar(line) || yaml_open_quote(line))
    })
}

/// True when a YAML line ends with a block scalar header such as `key: |`,
/// `key: >-`, `- |2` or a bare `|`.
fn yaml_block_scalar(line: &str) -> bool {
    let header = line.trim_end_matches(|c: char| c.is_ascii_digit() || c == '-' || c == '+');
    let Some(before) = header
        .strip_suffix('|')
        .or_else(|| header.strip_suffix('>'))
    else {
        return false;
    };
    let before = before.trim_end();
    before.is_empty() || before.ends_with(':') || before.ends_with('-')
}

/// True when a YAML line leaves a quoted scalar open, so the value continues
/// on the next line.
fn yaml_open_quote(line: &str) -> bool {
    let mut in_double = false;
    let mut in_single = false;
    // Set after a backslash in "..." or the first quote of '' in '...'.
    let mut skip_next = false;
    for (index, c) in line.char_indices() {
        if skip_next {
            skip_next = false;
            continue;
        }
        if in_double {
            match c {
                '\\' => skip_next = true,
                '"' => in_double = false,
                _ => {}
            }
        } else if in_single {
            if c == '\'' {
                // '' is an escaped quote inside a single-quoted scalar.
                if line[index + 1..].starts_with('\'') {
                    skip_next = true;
                } else {
                    in_single = false;
                }
            }
        } else {
            match c {
                '#' if index == 0 || line[..index].ends_with(char::is_whitespace) => return false,
                '"' => in_double = true,
                // A quote only opens a scalar at the start of a value, not
                // in the middle of plain text like `don't`.
                '\'' if line[..index]
                    .trim_end()
                    .ends_with([':', '-', '[', '{', ','])
                    || line[..index].trim().is_empty() =>
                {
                    in_single = true
                }
                _ => {}
            }
        }
    }
    in_double || in_single
}

fn significant_lines(text: &str, style: CommentStyle) -> impl Iterator<Item = &str> {
    // A UTF-8 byte order mark is formatting, not content.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    text.lines().map(str::trim_end).filter(move |line| {
        let start = line.trim_start();
        if start.is_empty() {
            return false;
        }
        match style {
            CommentStyle::None => true,
            CommentStyle::Hash => !start.starts_with('#'),
            CommentStyle::Slash => !start.starts_with("//"),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_whitespace_and_blank_lines_are_cosmetic() {
        let pack = b"# Generated by the mod\n[general]\n\tenabled = true\nlimit = 4\n";
        let live = b"[general]\r\n\tenabled = true   \r\n\r\n# admin note\r\nlimit = 4\r\n\r\n";
        assert!(cosmetic_equal(pack, live, "toml"));
        assert!(cosmetic_equal(
            b"{\n  // note\n  a: 1\n}\n",
            b"{\n  a: 1\n\n}",
            "json5"
        ));
    }

    #[test]
    fn line_order_is_not_cosmetic() {
        assert!(cosmetic_equal(
            b"a=1\n# c\nb=2\n",
            b"a=1   \n\nb=2",
            "properties"
        ));
        assert!(!cosmetic_equal(b"a=1\nb=2\n", b"b=2\na=1\n", "properties"));
    }

    #[test]
    fn swapped_values_across_sections_are_not_cosmetic() {
        let pack = b"[a]\nx = 1\n[b]\nx = 2\n";
        let live = b"[a]\nx = 2\n[b]\nx = 1\n";
        assert!(!cosmetic_equal(pack, live, "toml"));
    }

    #[test]
    fn value_change_is_not_cosmetic() {
        assert!(!cosmetic_equal(b"limit = 4\n", b"limit = 5\n", "toml"));
        assert!(!cosmetic_equal(b"a\n", b"a\nb\n", "txt"));
        // Leading indentation matters (YAML nesting).
        assert!(!cosmetic_equal(b"a:\n  b: 1\n", b"a:\nb: 1\n", "yaml"));
    }

    #[test]
    fn comment_markers_only_strip_in_formats_that_have_them() {
        // In .txt, .json and .snbt a '#' or '//' line is content.
        assert!(!cosmetic_equal(b"a\n", b"a\n# b\n", "txt"));
        assert!(!cosmetic_equal(b"{}\n", b"{}\n// x\n", "json"));
        assert!(!cosmetic_equal(b"{a:1}\n", b"{a:1}\n#x\n", "snbt"));
        // '//' is not a comment in TOML, '#' is not one in JSON5.
        assert!(!cosmetic_equal(b"a=1\n", b"a=1\n// x\n", "toml"));
        assert!(!cosmetic_equal(b"{}\n", b"{}\n# x\n", "json5"));
    }

    #[test]
    fn toml_multiline_string_content_is_not_cosmetic() {
        // Inside """...""" a '#' line is data, not a comment.
        let pack = b"motd = \"\"\"\n# original message\n\"\"\"\n";
        let live = b"motd = \"\"\"\n# custom message\n\"\"\"\n";
        assert!(!cosmetic_equal(pack, live, "toml"));
        // Blank lines and trailing whitespace inside the string are data too.
        assert!(!cosmetic_equal(
            b"a = '''\nx\n'''\n",
            b"a = '''\nx  \n\n'''\n",
            "toml"
        ));
        // A file with a multi-line string is never cosmetic, even when the
        // only difference is a real comment outside it.
        assert!(!cosmetic_equal(
            b"a = \"\"\"x\"\"\"\n",
            b"# note\na = \"\"\"x\"\"\"\n",
            "toml"
        ));
    }

    #[test]
    fn continuations_and_forge_lists_are_not_cosmetic() {
        // properties continuation: the '# b' line belongs to the value.
        assert!(!cosmetic_equal(
            b"k = a \\\n# b\n",
            b"k = a \\\n# c\n",
            "properties"
        ));
        // Forge .cfg string list.
        assert!(!cosmetic_equal(
            b"S:list <\n    # one\n >\n",
            b"S:list <\n    # two\n >\n",
            "cfg"
        ));
        // json5 string continuation.
        assert!(!cosmetic_equal(
            b"{ a: 'x \\\n// y' }\n",
            b"{ a: 'x \\\n// z' }\n",
            "json5"
        ));
    }

    #[test]
    fn yaml_block_and_open_quoted_scalars_are_not_cosmetic() {
        for (pack, live) in [
            (&b"motd: |\n  # hello\n"[..], &b"motd: |\n  # bye\n"[..]),
            (b"motd: >-\n  # hello\n", b"motd: >-\n  # bye\n"),
            (b"- |2\n  # hello\n", b"- |2\n  # bye\n"),
            (b"motd: \"a\n  # hello\"\n", b"motd: \"a\n  # bye\"\n"),
            (b"motd: 'a\n  # hello'\n", b"motd: 'a\n  # bye'\n"),
        ] {
            assert!(
                !cosmetic_equal(pack, live, "yaml"),
                "{}",
                String::from_utf8_lossy(pack)
            );
        }
    }

    #[test]
    fn closed_quotes_and_plain_text_stay_cosmetic() {
        // Balanced quotes, an apostrophe in plain text, '' escapes and a
        // '|' inside a value do not disable the comparison.
        let pack = b"a: \"x # y\"\nb: don't\nc: 'it''s'\nd: x | y\n";
        let live = b"# note\na: \"x # y\"   \n\nb: don't\nc: 'it''s'\nd: x | y\n";
        assert!(cosmetic_equal(pack, live, "yaml"));
        assert!(cosmetic_equal(b"a = \"x\"\n", b"# c\na = \"x\"\n", "toml"));
    }

    #[test]
    fn yaml_scalar_detection() {
        assert!(yaml_block_scalar("key: |"));
        assert!(yaml_block_scalar("key: >+"));
        assert!(yaml_block_scalar("  - |-"));
        assert!(yaml_block_scalar("|"));
        assert!(!yaml_block_scalar("a: x | y"));
        assert!(!yaml_block_scalar("a: x>"));
        assert!(yaml_open_quote("a: \"open"));
        assert!(yaml_open_quote("a: 'it''s open"));
        assert!(!yaml_open_quote("a: \"closed \\\" quote\""));
        assert!(!yaml_open_quote("a: 'it''s'"));
        assert!(!yaml_open_quote("a: plain # \"comment"));
        assert!(!yaml_open_quote("b: don't"));
    }

    #[test]
    fn binary_or_large_not_cosmetic() {
        assert!(!cosmetic_equal(b"\xff\xfe", b"\xff\xfe ", "toml"));
        let big = vec![b'a'; MAX_COSMETIC_BYTES + 1];
        assert!(!cosmetic_equal(&big, &big, "txt"));
        assert!(!cosmetic_equal(b"a", b"a ", "jar"));
        assert!(!cosmetic_equal(b"a", b"a ", ""));
    }
}
