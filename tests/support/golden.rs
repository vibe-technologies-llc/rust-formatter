use std::{
    collections::BTreeSet,
    env, fs,
    path::{Path, PathBuf},
};

/// Whether this run regenerates goldens instead of asserting them.
pub fn updating() -> bool {
    env::var_os("UPDATE_EXPECT").is_some_and(|value| value != "0")
}

/// Compares `actual` with the golden at `path`, or writes it under
/// `UPDATE_EXPECT=1`. Returns the mismatch detail, or `None` when it matches.
///
/// Bytes rather than `&str`: the encoding fixtures carry a BOM, CRLF and lone
/// CRs on purpose, and comparing them as text would normalise away exactly what
/// they exist to pin.
pub fn check(path: &Path, actual: &[u8]) -> Option<String> {
    if updating() {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("golden directory");
        }
        fs::write(path, actual).expect("write golden");
        return None;
    }

    let Ok(expected) = fs::read(path) else {
        return Some(format!(
            "no golden at {}; rerun with UPDATE_EXPECT=1",
            path.display()
        ));
    };
    if expected == actual {
        return None;
    }

    Some(
        if let (Ok(expected), Ok(actual)) = (
            String::from_utf8(expected.clone()),
            std::str::from_utf8(actual),
        ) {
            rust_formatter::unified_diff(
                path,
                &expected,
                actual,
                rust_formatter::DEFAULT_DIFF_CONTEXT,
            )
        } else {
            let at = expected
                .iter()
                .zip(actual)
                .position(|(a, b)| a != b)
                .unwrap_or_else(|| expected.len().min(actual.len()));
            format!(
                "bytes differ at offset {at}: expected {} bytes {:?}, actual {} bytes {:?}",
                expected.len(),
                window(&expected, at),
                actual.len(),
                window(actual, at),
            )
        },
    )
}

fn window(bytes: &[u8], at: usize) -> Vec<u8> {
    let start = at.saturating_sub(16);
    bytes[start..bytes.len().min(at + 16)].to_vec()
}

/// Every golden under `root` that no fixture produced. Without this a deleted
/// fixture leaves its golden behind, and the next `UPDATE_EXPECT=1` run keeps
/// regenerating a file nothing checks.
pub fn orphans(root: &Path, produced: &BTreeSet<PathBuf>) -> Vec<PathBuf> {
    if !root.is_dir() {
        return Vec::new();
    }
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(walkdir::DirEntry::into_path)
        .filter(|path| !produced.contains(path))
        .collect()
}

pub fn assert_no_orphans(root: &Path, produced: &BTreeSet<PathBuf>) {
    let orphans = orphans(root, produced);
    assert!(
        orphans.is_empty(),
        "{} golden files under {} match no fixture: {:?}",
        orphans.len(),
        root.display(),
        orphans,
    );
}
