use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use walkdir::WalkDir;

use crate::error::{Error, Result};

pub fn files_with_suffix(root: &Path, suffix: &str, max_depth: usize) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();

    for entry in WalkDir::new(root).max_depth(max_depth) {
        let entry = entry.map_err(Error::walk(root))?;
        if entry.file_type().is_file() && entry.file_name().to_string_lossy().ends_with(suffix) {
            files.push(entry.into_path());
        }
    }
    files.sort();
    Ok(files)
}

pub fn directories_at_depth(root: &Path, depth: usize) -> Result<Vec<PathBuf>> {
    let mut directories = Vec::new();

    for entry in WalkDir::new(root).min_depth(depth).max_depth(depth) {
        let entry = entry.map_err(Error::walk(root))?;
        if entry.file_type().is_dir() {
            directories.push(entry.into_path());
        }
    }
    directories.sort();
    Ok(directories)
}

pub fn stride<T: Clone>(items: &[T], want: usize) -> Vec<T> {
    let step = if items.len() > want && want > 0 {
        items.len() / want
    } else {
        1
    };
    items.iter().step_by(step).take(want).cloned().collect()
}

pub fn copy_file(from: &Path, to: &Path) -> Result<()> {
    let bytes = fs::read(from).map_err(Error::io(from))?;
    fs::write(to, bytes).map_err(Error::io(to))
}

pub fn copy_tree(from: &Path, to: &Path, skipped_names: &[&str]) -> Result<()> {
    let entries = WalkDir::new(from)
        .into_iter()
        .filter_entry(|entry| !skipped_names.iter().any(|name| entry.file_name() == *name));

    for entry in entries {
        let entry = entry.map_err(Error::walk(from))?;
        let Ok(relative) = entry.path().strip_prefix(from) else {
            continue;
        };
        let target = to.join(relative);
        if entry.file_type().is_dir() {
            fs::create_dir_all(&target).map_err(Error::io(&target))?;
        } else if entry.file_type().is_file() {
            copy_file(entry.path(), &target)?;
        }
    }
    Ok(())
}

pub fn replace_with_copy(from: &Path, to: &Path) -> Result<()> {
    if to.exists() {
        fs::remove_dir_all(to).map_err(Error::io(to))?;
    }
    copy_tree(from, to, &[])
}

pub fn differences(left: &Path, right: &Path) -> Result<Vec<String>> {
    let left_files = listing(left)?;
    let right_files = listing(right)?;

    let mut differences = Vec::new();

    for (relative, left_path) in &left_files {
        match right_files.get(relative) {
            None => differences.push(format!(
                "only in {}: {}",
                left.display(),
                relative.display()
            )),
            Some(right_path) => {
                let left_bytes = fs::read(left_path).map_err(Error::io(left_path))?;
                let right_bytes = fs::read(right_path).map_err(Error::io(right_path))?;
                if left_bytes != right_bytes {
                    differences.push(format!(
                        "{} and {} differ",
                        left_path.display(),
                        right_path.display()
                    ));
                }
            }
        }
    }
    for relative in right_files
        .keys()
        .filter(|relative| !left_files.contains_key(*relative))
    {
        differences.push(format!(
            "only in {}: {}",
            right.display(),
            relative.display()
        ));
    }
    Ok(differences)
}

fn listing(root: &Path) -> Result<BTreeMap<PathBuf, PathBuf>> {
    let mut files = BTreeMap::new();

    for entry in WalkDir::new(root) {
        let entry = entry.map_err(Error::walk(root))?;

        if !entry.file_type().is_file() {
            continue;
        }

        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };

        files.insert(relative.to_owned(), entry.path().to_owned());
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;

    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);

        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn a_stride_spreads_the_sample_over_the_whole_list() {
        let items: Vec<u32> = (0..10).collect();

        assert_eq!(stride(&items, 3), [0, 3, 6]);
        assert_eq!(stride(&items, 20), items);
        assert_eq!(stride(&items, 0), Vec::<u32>::new());
    }

    #[test]
    fn a_copy_is_identical_and_skips_what_it_is_told_to() {
        let dir = TempDir::new().unwrap();
        let from = dir.path().join("from");
        let to = dir.path().join("to");

        write(&from, "a.toml", "a = 1\n");
        write(&from, "nested/b.rs", "fn b() {}\n");
        write(&from, ".cargo-checksum.json", "{}");

        copy_tree(&from, &to, &[".cargo-checksum.json"]).unwrap();

        assert_eq!(
            differences(&from, &to).unwrap(),
            [format!("only in {}: .cargo-checksum.json", from.display())]
        );
    }

    #[test]
    fn a_changed_file_is_a_difference() {
        let dir = TempDir::new().unwrap();
        let left = dir.path().join("left");
        let right = dir.path().join("right");

        write(&left, "a.toml", "a = 1\n");

        replace_with_copy(&left, &right).unwrap();
        write(&right, "a.toml", "a = 2\n");

        assert_eq!(differences(&left, &right).unwrap().len(), 1);
    }
}
