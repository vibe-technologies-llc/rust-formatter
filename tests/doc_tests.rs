use std::{
    fs,
    path::{Path, PathBuf},
};

use assert_cmd::Command;

/// Every file whose fenced TOML is executed. A `.md` added to `docs/` is picked
/// up on its own, so a new page cannot arrive with unchecked examples.
fn documents() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = vec![root.join("README.md")];
    let mut docs: Vec<PathBuf> = fs::read_dir(root.join("docs"))
        .expect("docs directory")
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "md"))
        .collect();
    docs.sort();
    files.extend(docs);
    files
}

#[derive(Debug)]
struct Block {
    file: PathBuf,
    line: usize,
    info: String,
    body: String,
}

impl Block {
    fn at(&self) -> String {
        format!("{}:{}", self.file.display(), self.line)
    }

    /// The `rf:` annotation as `(kind, arguments)`, or `None` when the fence
    /// carries none.
    fn annotation(&self) -> Option<(&str, &str)> {
        let rest = self.info.get(self.info.find("rf:")? + 3..)?.trim();
        Some(match rest.split_once(char::is_whitespace) {
            Some((kind, args)) => (kind, args.trim()),
            None => (rest, ""),
        })
    }
}

/// Fenced blocks tagged `toml`, with the line the fence opens on.
///
/// A hand-rolled scanner rather than a markdown parser: the only thing needed is
/// the info string and the body, and a dependency that understood more of
/// markdown would not make either more certain.
fn toml_blocks(file: &Path) -> Vec<Block> {
    let text = fs::read_to_string(file).expect("document is UTF-8");
    let mut blocks = Vec::new();
    let mut lines = text.lines().enumerate();

    while let Some((index, line)) = lines.next() {
        let indent = line.len() - line.trim_start().len();
        let Some(info) = line.trim_start().strip_prefix("```") else {
            continue;
        };
        // A fence inside a list is indented, and its body with it. Dropping the
        // indentation here is what keeps such a block executable rather than
        // silently skipped.
        let mut body = String::new();
        for (_, line) in lines.by_ref() {
            if line.trim_start().starts_with("```") {
                break;
            }
            body.push_str(line.get(indent..).unwrap_or(line.trim_start()));
            body.push('\n');
        }
        if info.split_whitespace().next() == Some("toml") {
            blocks.push(Block {
                file: file.to_owned(),
                line: index + 1,
                info: info.to_owned(),
                body,
            });
        }
    }
    blocks
}

fn cache_dir() -> &'static Path {
    static DIR: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    DIR.get_or_init(|| tempfile::tempdir().unwrap()).path()
}

/// Formats `source` through the binary with `flags`.
///
/// The binary and not `format_toml`, so the flag spellings the documents print
/// are themselves under test: a renamed option breaks the example that names it.
fn format(source: &str, flags: &str) -> Result<String, String> {
    let mut cmd = Command::cargo_bin("rust-formatter").unwrap();
    cmd.env("RUST_FORMATTER_CACHE_DIR", cache_dir())
        .args(["--stdin", "--stdin-filepath", "doc.toml"])
        .args(flags.split_whitespace())
        .write_stdin(source.to_owned());
    let output = cmd.output().expect("run the binary");
    if !output.status.success() {
        return Err(format!(
            "exit {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8(output.stdout).expect("output is UTF-8"))
}

fn ends_with_newline(text: &str) -> String {
    if text.is_empty() || text.ends_with('\n') {
        text.to_owned()
    } else {
        format!("{text}\n")
    }
}

fn check_fixed_point(source: &str, flags: &str) -> Result<(), String> {
    let expected = ends_with_newline(source);
    let actual = format(&expected, flags)?;
    if actual == expected {
        return Ok(());
    }
    Err(format!(
        "the block is not what the formatter writes\n--- documented ---\n{expected}\
         --- formatter ---\n{actual}"
    ))
}

/// `# before` / `# after <flags>` in one block, which is how the documents
/// already write a pair.
fn check_before_after(body: &str) -> Result<(), String> {
    let lines: Vec<&str> = body.lines().collect();
    let first = lines.first().ok_or("an empty before/after block")?;
    if !first
        .trim_start_matches('#')
        .trim()
        .eq_ignore_ascii_case("before")
    {
        return Err(format!(
            "a before/after block has to open with `# before`, not {first:?}"
        ));
    }
    let split = lines
        .iter()
        .position(|line| {
            line.strip_prefix('#')
                .is_some_and(|rest| rest.trim_start().starts_with("after"))
        })
        .ok_or("a before/after block needs an `# after` line")?;

    let flags = lines[split]
        .trim_start_matches('#')
        .trim()
        .strip_prefix("after")
        .unwrap_or_default()
        .trim();
    let input = ends_with_newline(lines[1..split].join("\n").trim_end());
    let expected = ends_with_newline(lines[split + 1..].join("\n").trim_end());

    let actual = format(&input, flags)?;
    if actual == expected {
        return Ok(());
    }
    Err(format!(
        "`{flags}` does not turn the before half into the after half\n\
         --- before ---\n{input}--- documented after ---\n{expected}--- formatter ---\n{actual}"
    ))
}

/// Two consecutive blocks: formatting the first with `flags` has to produce the
/// second exactly.
fn check_pair(before: &str, after: &str, flags: &str) -> Result<(), String> {
    let input = ends_with_newline(before.trim_end());
    let expected = ends_with_newline(after.trim_end());
    let actual = format(&input, flags)?;
    if actual == expected {
        return Ok(());
    }
    Err(format!(
        "`{flags}` does not turn the before block into the after block\n\
         --- before ---\n{input}--- documented after ---\n{expected}--- formatter ---\n{actual}"
    ))
}

/// Paragraphs separated by a blank line, each a fixed point on its own. Written
/// for the blocks that show one key at two widths, which as a single document
/// would be a duplicate key.
fn check_fixed_point_parts(body: &str, flags: &str) -> Result<(), String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    for line in body.lines() {
        if line.trim().is_empty() {
            if !current.trim().is_empty() {
                parts.push(std::mem::take(&mut current));
            }
            current.clear();
            continue;
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.trim().is_empty() {
        parts.push(current);
    }
    if parts.len() < 2 {
        return Err("a fixed-point-parts block with one part should be fixed-point".to_owned());
    }
    for part in parts {
        check_fixed_point(&part, flags)?;
    }
    Ok(())
}

/// Every fenced `toml` block in the documentation is either executed or says in
/// so many words why it cannot be.
///
/// The `rf:` annotation goes after the language tag, which renderers ignore, so
/// the fences still highlight as TOML:
///
/// - `rf:fixed-point [flags]` -- the block is what the formatter writes
/// - `rf:fixed-point-parts [flags]` -- each blank-line-separated paragraph is
/// - `rf:before-after` -- `# before` / `# after <flags>` halves in one block
/// - `rf:before [flags]` / `rf:after` -- the same pair written as the two
///   consecutive blocks a `#### Before` / `#### After` heading pair produces
/// - `rf:skip <reason>` -- a layout no formatter run can produce, such as the
///   two-column before/after tables
#[test]
fn every_toml_example_is_annotated() {
    let mut missing = Vec::new();
    for file in documents() {
        for block in toml_blocks(&file) {
            match block.annotation() {
                None => missing.push(format!("{}: no rf: annotation", block.at())),
                Some(("skip", "")) => {
                    missing.push(format!("{}: rf:skip needs a reason", block.at()));
                }
                Some((kind, _))
                    if !matches!(
                        kind,
                        "fixed-point"
                            | "fixed-point-parts"
                            | "before-after"
                            | "before"
                            | "after"
                            | "skip"
                    ) =>
                {
                    missing.push(format!("{}: unknown annotation rf:{kind}", block.at()));
                }
                Some(_) => {}
            }
        }
    }
    assert!(
        missing.is_empty(),
        "{} TOML examples are unaccounted for:\n{}",
        missing.len(),
        missing.join("\n")
    );
}

#[test]
fn every_annotated_toml_example_is_what_the_formatter_writes() {
    let mut failures = Vec::new();
    let mut executed = 0;

    for file in documents() {
        let blocks = toml_blocks(&file);
        for (index, block) in blocks.iter().enumerate() {
            let Some((kind, args)) = block.annotation() else {
                continue;
            };
            let result = match kind {
                "fixed-point" => check_fixed_point(&block.body, args),
                "fixed-point-parts" => check_fixed_point_parts(&block.body, args),
                "before-after" => check_before_after(&block.body),
                "before" => match blocks.get(index + 1) {
                    Some(after) if after.annotation().map(|(kind, _)| kind) == Some("after") => {
                        check_pair(&block.body, &after.body, args)
                    }
                    _ => Err("rf:before must be followed by an rf:after block".to_owned()),
                },
                _ => continue,
            };
            executed += 1;
            if let Err(detail) = result {
                failures.push(format!("{}\n{detail}", block.at()));
            }
        }
    }

    assert!(executed >= 20, "only {executed} examples were executed");
    assert!(
        failures.is_empty(),
        "{} documented examples do not match the formatter:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}
