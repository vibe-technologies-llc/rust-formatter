//! Annotated Rust snippet fixtures for the formatter.
//!
//! Files live under `tests/fixtures/rust/**/*.rs.in`. A leading run of
//! `// @key: value` lines is the harness directive block and is stripped
//! before rustfmt sees the file. Everything after that — including every
//! other comment — is the source under test. Goldens land at
//! `tests/expected/rust/{preset}/{relative/stem}.out`.
//!
//! Known keys: `presets`, `config`, `edition`, `style-edition`, `range`
//! (repeatable), `stdin-filepath`, `assert`. An unknown key fails the suite
//! so a typo cannot silently drop a check.
//!
//! Defaults: every snippet runs under `default`, `comments`, `literals` and
//! `strict`; output is a fixed point; comment *content* is preserved;
//! `#[rustfmt::skip]` regions are byte-identical when that attribute is
//! present. `@assert: no-comments-kept` (and the `no-` forms of the others)
//! opts one out.

use std::path::{Path, PathBuf};

use assert_cmd::Command;

use super::rust_comments;

pub const PRESETS: [&str; 4] = ["default", "comments", "literals", "strict"];

const KNOWN_KEYS: [&str; 7] = [
    "presets",
    "config",
    "edition",
    "style-edition",
    "range",
    "stdin-filepath",
    "assert",
];

#[derive(Debug, Clone)]
pub struct Snippet {
    pub path: PathBuf,
    pub stem: String,
    pub presets: Vec<String>,
    pub config: Option<String>,
    pub edition: Option<String>,
    pub style_edition: Option<String>,
    pub ranges: Vec<String>,
    pub stdin_filepath: Option<String>,
    pub comments_kept: bool,
    pub skip_intact: bool,
    pub fixed_point: bool,
    pub source: String,
}

impl Snippet {
    pub fn parse(path: &Path, fixtures_root: &Path) -> Result<Self, String> {
        let text =
            std::fs::read_to_string(path).map_err(|err| format!("{}: {err}", path.display()))?;
        Self::from_text(path.to_path_buf(), fixtures_root, &text)
    }

    pub fn from_text(path: PathBuf, fixtures_root: &Path, text: &str) -> Result<Self, String> {
        let stem = relative_stem(&path, fixtures_root).ok_or_else(|| {
            format!(
                "{} is not a .rs.in under {}",
                path.display(),
                fixtures_root.display()
            )
        })?;
        parse_text(path, stem, text)
    }

    pub fn stdin_name(&self) -> String {
        self.stdin_filepath
            .clone()
            .unwrap_or_else(|| format!("{}.rs", self.stem))
    }

    pub fn golden(&self, expected_root: &Path, preset: &str) -> PathBuf {
        expected_root
            .join(preset)
            .join(&self.stem)
            .with_extension("out")
    }
}

pub fn discover(root: &Path) -> Result<Vec<Snippet>, String> {
    let mut paths: Vec<PathBuf> = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(walkdir::DirEntry::into_path)
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".rs.in"))
        })
        .collect();
    paths.sort();

    let mut snippets = Vec::with_capacity(paths.len());
    for path in paths {
        snippets.push(Snippet::parse(&path, root)?);
    }
    Ok(snippets)
}

pub fn format(snippet: &Snippet, preset: &str, cache_dir: &Path) -> Result<String, String> {
    let mut cmd = Command::cargo_bin("rust-formatter")
        .map_err(|err| format!("cargo bin rust-formatter: {err}"))?;
    cmd.env("RUST_FORMATTER_CACHE_DIR", cache_dir)
        .args(["--stdin", "--stdin-filepath"])
        .arg(snippet.stdin_name())
        .args(["--preset", preset]);
    if let Some(config) = &snippet.config {
        cmd.args(["--config", config]);
    }
    if let Some(edition) = &snippet.edition {
        cmd.args(["--edition", edition]);
    }
    if let Some(style_edition) = &snippet.style_edition {
        cmd.args(["--style-edition", style_edition]);
    }
    for range in &snippet.ranges {
        cmd.args(["--range", range]);
    }
    cmd.write_stdin(snippet.source.as_str());

    let output = cmd
        .ok()
        .map_err(|err| format!("{} [{preset}] rust-formatter failed\n{err}", snippet.stem))?;
    String::from_utf8(output.stdout)
        .map_err(|err| format!("{} [{preset}] output is not UTF-8: {err}", snippet.stem))
}

pub fn comments_lost(before: &str, after: &str) -> Option<String> {
    let expected = rust_comments::fingerprint(before);
    let actual = rust_comments::fingerprint(after);
    (expected != actual)
        .then(|| format!("comment fingerprint changed\n  before: {expected}\n   after: {actual}"))
}

pub fn skip_lost(before: &str, after: &str) -> Option<String> {
    let regions = rust_comments::skip_regions(before);
    let mut missing = Vec::new();
    for region in &regions {
        if !after.contains(region) {
            missing.push(region.clone());
        }
    }
    (!missing.is_empty()).then(|| {
        format!(
            "{} #[rustfmt::skip] region(s) missing from output: {missing:?}",
            missing.len()
        )
    })
}

fn relative_stem(path: &Path, root: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let text = rel.to_str()?;
    text.strip_suffix(".rs.in").map(str::to_owned)
}

fn parse_text(path: PathBuf, stem: String, text: &str) -> Result<Snippet, String> {
    let (directives, source) = split_directives(text);
    let mut presets = Vec::new();
    let mut config = None;
    let mut edition = None;
    let mut style_edition = None;
    let mut ranges = Vec::new();
    let mut stdin_filepath = None;
    let mut comments_kept = true;
    let mut skip_intact = !rust_comments::skip_regions(&source).is_empty();
    let mut fixed_point = true;

    for (line_no, raw) in &directives {
        let err = |message: String| format!("{}:{line_no}: {message}", path.display());
        let (key, value) = split_directive(raw)
            .ok_or_else(|| err(format!("directive `{raw}` is not `// @key: value`")))?;
        if !KNOWN_KEYS.contains(&key) {
            return Err(err(format!(
                "unknown directive `@{key}`; known: {}",
                KNOWN_KEYS.join(", ")
            )));
        }
        match key {
            "presets" => {
                presets = split_csv(value);
                for name in &presets {
                    if !PRESETS.contains(&name.as_str()) {
                        return Err(err(format!(
                            "unknown preset `{name}`; known: {}",
                            PRESETS.join(", ")
                        )));
                    }
                }
            }
            "config" => config = Some(value.to_owned()),
            "edition" => edition = Some(value.to_owned()),
            "style-edition" => style_edition = Some(value.to_owned()),
            "range" => ranges.extend(split_csv(value)),
            "stdin-filepath" => stdin_filepath = Some(value.to_owned()),
            "assert" => {
                for token in split_csv(value) {
                    match token.as_str() {
                        "comments-kept" => comments_kept = true,
                        "no-comments-kept" => comments_kept = false,
                        "skip-intact" => skip_intact = true,
                        "no-skip-intact" => skip_intact = false,
                        "fixed-point" => fixed_point = true,
                        "no-fixed-point" => fixed_point = false,
                        other => {
                            return Err(err(format!(
                                "unknown @assert `{other}`; known: comments-kept, no-comments-kept, skip-intact, no-skip-intact, fixed-point, no-fixed-point"
                            )));
                        }
                    }
                }
            }
            _ => unreachable!("key filtered by KNOWN_KEYS"),
        }
    }

    if presets.is_empty() {
        presets = PRESETS.iter().map(|name| (*name).to_owned()).collect();
    }

    Ok(Snippet {
        path,
        stem,
        presets,
        config,
        edition,
        style_edition,
        ranges,
        stdin_filepath,
        comments_kept,
        skip_intact,
        fixed_point,
        source,
    })
}

fn split_directives(text: &str) -> (Vec<(usize, String)>, String) {
    let mut directives = Vec::new();
    let mut rest = text;
    let mut line_no = 0usize;

    loop {
        let (line, after) = match rest.find('\n') {
            Some(at) => {
                let line = rest[..at].trim_end_matches('\r');
                (line, Some(&rest[at + 1..]))
            }
            None => (rest.trim_end_matches('\r'), None),
        };
        if !line.trim().starts_with("// @") {
            break;
        }
        line_no += 1;
        directives.push((line_no, line.trim().to_owned()));
        if let Some(after) = after {
            rest = after;
        } else {
            rest = "";
            break;
        }
    }

    (directives, rest.to_owned())
}

fn split_directive(raw: &str) -> Option<(&str, &str)> {
    let rest = raw.trim().strip_prefix("// @")?;
    let (key, value) = rest.split_once(':')?;
    let key = key.trim();
    let value = value.trim();
    if key.is_empty() {
        return None;
    }
    Some((key, value))
}

fn split_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}
