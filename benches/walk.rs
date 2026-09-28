//! What finding the files costs, which is the part of a run no cache removes.

use std::{fs, hint::black_box};

use criterion::{Criterion, criterion_group, criterion_main};
use rust_formatter::{SelectionOptions, Selector, detector::collect, selection::Languages};
use tempfile::TempDir;

/// A tree shaped like a workspace: packages of source files, plus the two
/// things the walk has to prune -- a build directory and a vendored crate.
fn tree(packages: usize, per_package: usize) -> TempDir {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = [\"p0\"]\n").unwrap();

    for package in 0..packages {
        let dir = root.join(format!("p{package}")).join("src");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.parent().unwrap().join("Cargo.toml"),
            "[package]\nname = \"p\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        for file in 0..per_package {
            fs::write(dir.join(format!("m{file}.rs")), "pub fn f() {}\n").unwrap();
        }
    }

    let target = root.join("target").join("debug").join("deps");
    fs::create_dir_all(&target).unwrap();
    fs::write(
        root.join("target").join("CACHEDIR.TAG"),
        "Signature: 8a477f597d28d172789f06886806bc55",
    )
    .unwrap();
    for file in 0..500 {
        fs::write(target.join(format!("dep{file}.rs")), "pub fn f() {}\n").unwrap();
    }

    let vendored = root.join("vendor").join("some-crate");
    fs::create_dir_all(&vendored).unwrap();
    fs::write(vendored.join(".cargo-checksum.json"), "{}").unwrap();
    fs::write(vendored.join("lib.rs"), "pub fn f() {}\n").unwrap();

    temp
}

fn walking(c: &mut Criterion) {
    let temp = tree(8, 60);
    let selector = Selector::new(&SelectionOptions::default()).unwrap();
    c.bench_function("collect/workspace", |b| {
        b.iter(|| {
            let walk = collect(
                black_box(temp.path()),
                &selector,
                Languages::Both,
                &[],
                false,
            )
            .unwrap();
            black_box(walk.rust_files.len() + walk.toml_files.len())
        });
    });
}

/// The pruning half on its own: a tree that is mostly build output is the case
/// the directory tests were reordered for.
fn pruning(c: &mut Criterion) {
    let temp = tree(1, 10);
    let selector = Selector::new(&SelectionOptions::default()).unwrap();
    c.bench_function("collect/mostly-pruned", |b| {
        b.iter(|| {
            let walk = collect(
                black_box(temp.path()),
                &selector,
                Languages::Both,
                &[],
                false,
            )
            .unwrap();
            black_box(walk.rust_files.len())
        });
    });
}

criterion_group!(benches, walking, pruning);
criterion_main!(benches);
