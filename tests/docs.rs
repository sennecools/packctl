//! Consistency checks between the documentation and the code.
//!
//! README.md and docs/*.md quote commands, flags, `--help` output and
//! `.packctlignore` samples. These tests keep them honest: every flag a
//! `packctl <command>` line uses must exist, every help block must match the
//! real `--help` output, every relative link must resolve, and every
//! `.packctlignore` sample must parse to the rules it claims.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use clap::{CommandFactory, Parser};
use packctl::cli::Cli;
use packctl::core::ignore::parse_rules;

/// Flags of other tools that appear in the docs (cargo, gh, sha256sum,
/// systemctl, curl, ...). Anything else spelled `--flag` must be a packctl
/// flag.
const FOREIGN_FLAGS: &[&str] = &[
    "all-features",
    "all-targets",
    "check",
    "git",
    "ignore-missing",
    "locked",
    "quiet",
    "release",
];

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// README.md and every Markdown file in docs/.
fn doc_files() -> Vec<PathBuf> {
    let mut files = vec![repo().join("README.md")];
    let mut docs: Vec<PathBuf> = fs::read_dir(repo().join("docs"))
        .expect("docs/ exists")
        .map(|entry| entry.expect("readable docs entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .collect();
    docs.sort();
    files.extend(docs);
    files
}

fn read(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
        .replace("\r\n", "\n")
}

fn shown(path: &Path) -> String {
    path.strip_prefix(repo())
        .unwrap_or(path)
        .display()
        .to_string()
        .replace('\\', "/")
}

/// A fenced code block: its info string and its lines.
struct Block {
    info: String,
    lines: Vec<String>,
}

/// The fenced code blocks of a Markdown document.
fn code_blocks(text: &str) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut current: Option<Block> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("```") {
            match current.take() {
                Some(block) => blocks.push(block),
                None => {
                    current = Some(Block {
                        info: rest.trim().to_string(),
                        lines: Vec::new(),
                    });
                }
            }
        } else if let Some(block) = current.as_mut() {
            block.lines.push(line.to_string());
        }
    }
    assert!(current.is_none(), "unterminated code block");
    blocks
}

/// Removes HTML comments (`<!-- ... -->`), which GitHub does not render.
fn strip_html_comments(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start..].find("-->") {
            Some(end) => rest = &rest[start + end + 3..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Long and short flags per subcommand, plus the top-level ones under "".
fn cli_flags() -> BTreeMap<String, (BTreeSet<String>, BTreeSet<char>)> {
    let mut command = Cli::command();
    command.build();
    let mut flags = BTreeMap::new();
    let collect = |cmd: &clap::Command| {
        let longs: BTreeSet<String> = cmd
            .get_arguments()
            .filter_map(|arg| arg.get_long().map(str::to_string))
            .collect();
        let shorts: BTreeSet<char> = cmd
            .get_arguments()
            .filter_map(|arg| arg.get_short())
            .collect();
        (longs, shorts)
    };
    flags.insert(String::new(), collect(&command));
    for sub in command.get_subcommands() {
        flags.insert(sub.get_name().to_string(), collect(sub));
    }
    flags
}

/// Joins shell continuation lines (`\` at the end of a line).
fn logical_lines(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut pending = String::new();
    for line in text.lines() {
        match line.strip_suffix('\\') {
            Some(start) => {
                pending.push_str(start);
                pending.push(' ');
            }
            None => {
                pending.push_str(line);
                lines.push(std::mem::take(&mut pending));
            }
        }
    }
    if !pending.is_empty() {
        lines.push(pending);
    }
    lines
}

/// The flag name in a token such as `--to`, `--json,` or `--detect>`.
fn flag_name(token: &str) -> Option<&str> {
    let name = token.strip_prefix("--")?;
    let name = name.split('=').next().unwrap_or_default();
    let name = name.trim_end_matches(|c: char| !c.is_ascii_alphanumeric());
    name.starts_with(|c: char| c.is_ascii_lowercase())
        .then_some(name)
}

#[test]
fn flags_after_packctl_commands_exist() {
    let flags = cli_flags();
    let mut problems = Vec::new();
    for file in doc_files() {
        let text = strip_html_comments(&read(&file));
        for line in logical_lines(&text) {
            let mut rest = line.as_str();
            while let Some(at) = rest.find("packctl ") {
                rest = &rest[at + "packctl ".len()..];
                // The command line ends at a backtick, a comment, a pipe or a
                // command separator.
                let end = rest
                    .find(['`', '#', '|', ';', '&', ')'])
                    .unwrap_or(rest.len());
                let segment = &rest[..end];
                let mut tokens = segment.split_whitespace();
                let Some(first) = tokens.next() else {
                    continue;
                };
                if first.starts_with('-') {
                    continue;
                }
                let sub = first.trim_matches(|c: char| !c.is_ascii_alphanumeric());
                let Some((longs, shorts)) = flags.get(sub) else {
                    continue;
                };
                for token in tokens {
                    if let Some(name) = flag_name(token) {
                        if !longs.contains(name) {
                            problems.push(format!(
                                "{}: `packctl {sub}` has no --{name}: {}",
                                shown(&file),
                                line.trim()
                            ));
                        }
                    } else if let Some(short) = token.strip_prefix('-')
                        && short.len() == 1
                        && let Some(c) = short.chars().next()
                        && c.is_ascii_alphabetic()
                        && !shorts.contains(&c)
                    {
                        problems.push(format!(
                            "{}: `packctl {sub}` has no -{c}: {}",
                            shown(&file),
                            line.trim()
                        ));
                    }
                }
            }
        }
    }
    assert!(
        problems.is_empty(),
        "unknown flags:\n{}",
        problems.join("\n")
    );
}

#[test]
fn every_flag_mentioned_is_a_packctl_flag() {
    let flags = cli_flags();
    let known: BTreeSet<&str> = flags
        .values()
        .flat_map(|(longs, _)| longs.iter().map(String::as_str))
        .chain(FOREIGN_FLAGS.iter().copied())
        .collect();
    let mut problems = BTreeSet::new();
    for file in doc_files() {
        let text = strip_html_comments(&read(&file));
        for token in text.split(|c: char| c.is_whitespace() || "`|(<[\"'".contains(c)) {
            if let Some(name) = flag_name(token)
                && !known.contains(name)
            {
                problems.insert(format!("{}: --{name}", shown(&file)));
            }
        }
    }
    assert!(
        problems.is_empty(),
        "flags that no packctl command has (add other tools' flags to FOREIGN_FLAGS):\n{}",
        problems.into_iter().collect::<Vec<_>>().join("\n")
    );
}

/// The real `--help` output for `command` (`packctl plan --help`), with
/// trailing whitespace and trailing blank lines removed.
fn rendered_help(command: &str) -> Vec<String> {
    let args: Vec<&str> = command.split_whitespace().collect();
    let error = match Cli::try_parse_from(args) {
        Ok(_) => panic!("`{command}` did not print help"),
        Err(error) => error,
    };
    assert_eq!(
        error.kind(),
        clap::error::ErrorKind::DisplayHelp,
        "{command}"
    );
    let mut lines: Vec<String> = error
        .render()
        .to_string()
        .lines()
        .map(|line| line.trim_end().to_string())
        .collect();
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

/// docs/commands.md with every `$ packctl ... --help` block replaced by the
/// real help output, and the number of such blocks.
fn commands_md_with_real_help(text: &str) -> (String, usize) {
    let mut out = Vec::new();
    let mut blocks = 0;
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        out.push(line.to_string());
        if !line.starts_with("```") {
            continue;
        }
        let Some(command) = lines
            .peek()
            .and_then(|next| next.strip_prefix("$ "))
            .filter(|command| command.starts_with("packctl ") && command.ends_with("--help"))
            .map(str::to_string)
        else {
            continue;
        };
        blocks += 1;
        out.push(format!("$ {command}"));
        out.extend(rendered_help(&command));
        // Skip the documented output up to the closing fence.
        for skipped in lines.by_ref() {
            if skipped.starts_with("```") {
                out.push(skipped.to_string());
                break;
            }
        }
    }
    let mut text_out = out.join("\n");
    text_out.push('\n');
    (text_out, blocks)
}

/// The help blocks in docs/commands.md are the real `--help` output. After
/// changing a help text, regenerate them with
/// `PACKCTL_BLESS_DOCS=1 cargo test --test docs`.
#[test]
fn help_blocks_match_the_real_help_output() {
    let file = repo().join("docs").join("commands.md");
    let text = read(&file);
    let (expected, blocks) = commands_md_with_real_help(&text);
    // The top-level help plus one block per command.
    let commands = Cli::command().get_subcommands().count();
    assert_eq!(blocks, commands + 1, "every command has a help block");
    if text == expected {
        return;
    }
    if std::env::var("PACKCTL_BLESS_DOCS").is_ok_and(|value| value == "1") {
        fs::write(&file, expected).expect("write docs/commands.md");
        return;
    }
    let documented: Vec<&str> = text.lines().collect();
    let real: Vec<&str> = expected.lines().collect();
    let first = documented
        .iter()
        .zip(&real)
        .position(|(a, b)| a != b)
        .unwrap_or(documented.len().min(real.len()));
    panic!(
        "docs/commands.md differs from the real --help output at line {}:\n  \
         documented: {:?}\n  real:       {:?}\n\
         regenerate it with: PACKCTL_BLESS_DOCS=1 cargo test --test docs",
        first + 1,
        documented.get(first),
        real.get(first)
    );
}

/// GitHub's anchor for a heading.
fn slug(heading: &str) -> String {
    heading
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-' || *c == '_')
        .map(|c| if c == ' ' { '-' } else { c })
        .collect()
}

/// The anchors of a Markdown document's headings.
fn anchors(text: &str) -> BTreeSet<String> {
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    let mut anchors = BTreeSet::new();
    let mut in_code = false;
    for line in text.lines() {
        if line.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            continue;
        }
        let Some(heading) = line.strip_prefix('#') else {
            continue;
        };
        let heading = heading.trim_start_matches('#');
        if !heading.starts_with(' ') {
            continue;
        }
        let base = slug(heading);
        let count = seen.entry(base.clone()).or_default();
        anchors.insert(if *count == 0 {
            base.clone()
        } else {
            format!("{base}-{count}")
        });
        *count += 1;
    }
    anchors
}

/// Link targets in Markdown links and HTML `href`/`src` attributes, outside
/// code blocks and HTML comments.
fn link_targets(text: &str) -> Vec<String> {
    let mut prose = String::new();
    let mut in_code = false;
    for line in strip_html_comments(text).lines() {
        if line.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if !in_code {
            prose.push_str(line);
            prose.push('\n');
        }
    }
    let mut targets = Vec::new();
    let mut rest = prose.as_str();
    while let Some(at) = rest.find("](") {
        rest = &rest[at + 2..];
        if let Some(end) = rest.find(')') {
            targets.push(rest[..end].trim().to_string());
        }
    }
    for attribute in ["href=\"", "src=\""] {
        let mut rest = prose.as_str();
        while let Some(at) = rest.find(attribute) {
            rest = &rest[at + attribute.len()..];
            if let Some(end) = rest.find('"') {
                targets.push(rest[..end].to_string());
            }
        }
    }
    targets
}

#[test]
fn relative_links_resolve() {
    let mut problems = Vec::new();
    for file in doc_files() {
        let text = read(&file);
        for target in link_targets(&text) {
            if target.starts_with("http://")
                || target.starts_with("https://")
                || target.starts_with("mailto:")
            {
                continue;
            }
            let (path, anchor) = match target.split_once('#') {
                Some((path, anchor)) => (path, Some(anchor)),
                None => (target.as_str(), None),
            };
            let resolved = if path.is_empty() {
                file.clone()
            } else {
                file.parent().expect("doc has a parent").join(path)
            };
            if !resolved.exists() {
                problems.push(format!("{}: {target} does not exist", shown(&file)));
                continue;
            }
            if let Some(anchor) = anchor {
                let is_markdown = resolved.extension().is_some_and(|ext| ext == "md");
                if !is_markdown || !anchors(&read(&resolved)).contains(anchor) {
                    problems.push(format!("{}: no heading for {target}", shown(&file)));
                }
            }
        }
    }
    assert!(
        problems.is_empty(),
        "broken links:\n{}",
        problems.join("\n")
    );
}

/// A path that `pattern` must match: wildcards replaced by plain names, plus
/// a file below it.
fn sample_path(pattern: &str) -> String {
    let path = pattern
        .trim_end_matches('/')
        .replace("**", "deep")
        .replace('*', "x")
        .replace('?', "y");
    format!("{path}/file.txt")
}

#[test]
fn packctlignore_samples_parse_to_the_rules_they_show() {
    let mut samples = 0;
    for file in doc_files() {
        for block in code_blocks(&read(&file)) {
            if block.info != "gitignore" {
                continue;
            }
            samples += 1;
            let content = block.lines.join("\n");
            let rules = parse_rules(&content).unwrap_or_else(|error| {
                panic!(
                    "{}: sample does not parse: {error}\n{content}",
                    shown(&file)
                )
            });
            for line in &block.lines {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                // Each documented rule is one path, optionally followed by a
                // comment; the rule must protect that path.
                let pattern = trimmed.split_whitespace().next().expect("non-empty line");
                let path = sample_path(pattern);
                assert!(
                    rules.is_ignored(Path::new(&path)),
                    "{}: the rule `{trimmed}` does not ignore {path}",
                    shown(&file)
                );
                let single = parse_rules(line).expect("a documented line parses on its own");
                assert!(
                    !single.is_empty(),
                    "{}: `{trimmed}` is no rule",
                    shown(&file)
                );
            }
        }
    }
    assert!(
        samples >= 3,
        "README.md and docs/ show .packctlignore samples"
    );
}

#[test]
fn demo_script_and_tape_exist() {
    let setup = read(&repo().join("scripts").join("demo").join("setup.sh"));
    assert!(
        setup.starts_with("#!/bin/sh\n"),
        "setup.sh is a POSIX sh script"
    );
    let tape = read(&repo().join("docs").join("demo.tape"));
    assert!(tape.contains("Output docs/media/demo.gif"));
    assert!(tape.contains("scripts/demo/setup.sh"));
    assert!(repo().join("docs").join("media").join(".gitkeep").exists());
}
