use std::{fmt::Write as _, fs, hint::black_box, path::PathBuf};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use rust_formatter::{
    toml_fmt::format_toml,
    toml_style::{ArrayStyle, IndentSpec, InlineTableStyle, TomlStyle},
};

/// Every fixture, kept as the separate documents they are: each one exists
/// because it broke something, so together they are the widest sample of the
/// formatter's edge cases the repository has.
fn fixtures() -> Vec<String> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut paths: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("fixtures")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "in"))
        .collect();
    paths.sort();
    // A few fixtures are deliberately not valid TOML -- they exist to pin how
    // a parse failure is reported -- and timing the error path would be timing
    // the wrong thing.
    paths
        .iter()
        .filter_map(|path| fs::read_to_string(path).ok())
        .filter(|text| format_toml(text, &TomlStyle::default()).is_ok())
        .collect()
}

/// One large manifest, which is the document a real run spends most of its time
/// on and the one the sort and width passes have the most work in.
fn manifest(packages: usize) -> String {
    let mut out = String::with_capacity(packages * 128);
    out.push_str("[package]\nname = \"bench\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n");
    out.push_str("[dependencies]\n");
    for index in 0..packages {
        let _ = writeln!(
            out,
            "dep-{index:04} = {{ version = \"1.{index}.0\", features = [\"one\", \"two\"], \
             optional = true }}"
        );
    }
    out.push_str("\n[features]\n");
    for index in 0..packages / 4 {
        let _ = writeln!(
            out,
            "feature-{index:04} = [\"dep:dep-{index:04}\", \"dep:dep-{:04}\"]",
            index + 1
        );
    }
    out
}

/// The knobs that change how much work there is, rather than the shipped
/// presets: a preset is a bundle of settings whose cost is only ever the sum of
/// these.
fn styles() -> Vec<(&'static str, TomlStyle)> {
    let narrow = TomlStyle {
        max_width: 40,
        ..TomlStyle::default()
    };
    let expanded = TomlStyle {
        inline_tables: InlineTableStyle::Expand,
        arrays: ArrayStyle::Expand,
        ..TomlStyle::default()
    };
    let tabs = TomlStyle {
        indent: IndentSpec::Tab.resolve(4),
        ..TomlStyle::default()
    };
    vec![
        ("default", TomlStyle::default()),
        ("narrow", narrow),
        ("expanded", expanded),
        ("tabs", tabs),
    ]
}

fn documents(c: &mut Criterion) {
    let documents = fixtures();
    let bytes: usize = documents.iter().map(String::len).sum();
    let mut group = c.benchmark_group("format_toml/fixtures");
    group.throughput(Throughput::Bytes(bytes as u64));
    for (name, style) in styles() {
        group.bench_with_input(BenchmarkId::from_parameter(name), &style, |b, style| {
            b.iter(|| {
                for text in &documents {
                    black_box(format_toml(black_box(text), style).unwrap());
                }
            });
        });
    }
    group.finish();
}

fn large(c: &mut Criterion) {
    let source = manifest(400);
    let style = TomlStyle::default();
    let mut group = c.benchmark_group("format_toml/manifest");
    group.throughput(Throughput::Bytes(source.len() as u64));
    group.bench_function("dirty", |b| {
        b.iter(|| black_box(format_toml(black_box(&source), &style).unwrap()));
    });

    // A converged document goes through the same parse and render as a dirty
    // one, which is why a no-op `--check` was never free.
    let once = format_toml(&source, &style).unwrap();
    group.bench_function("converged", |b| {
        b.iter(|| black_box(format_toml(black_box(&once), &style).unwrap()));
    });
    group.finish();
}

criterion_group!(benches, documents, large);
criterion_main!(benches);
