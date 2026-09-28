// Compiled into every test binary that drives Rust formatting, so the half
// a given binary does not use looks unused from there.
#![allow(dead_code, unused_imports, unused_macros)]

use std::{process::Command, sync::OnceLock};

/// Whether rustup can produce a rustfmt for `toolchain`.
///
/// Memoised per binary: the gate is consulted once per test, and a `rustup
/// which` spawn per test is the single largest fixed cost in the suites that
/// drive Rust formatting.
pub fn toolchain_available(toolchain: &str) -> bool {
    Command::new("rustup")
        .args(["which", "rustfmt", "--toolchain", toolchain])
        .output()
        .is_ok_and(|out| out.status.success())
}

pub fn nightly_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| toolchain_available("nightly"))
}

pub fn stable_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| toolchain_available("stable"))
}

/// Rust work needs a nightly rustfmt; TOML-only work does not. Skipping keeps
/// the suite meaningful on a machine without the toolchain -- and failing is
/// not the alternative, because a missing toolchain says nothing about the code
/// under test.
///
/// The expansion names `nightly_available` unqualified, so a caller has to
/// `use toolchain::{needs_nightly, nightly_available};` -- both, not just the
/// macro.
macro_rules! needs_nightly {
    () => {
        if !nightly_available() {
            eprintln!("SKIP: no nightly rustfmt");
            return;
        }
    };
}

macro_rules! needs_stable {
    () => {
        if !stable_available() {
            eprintln!("SKIP: no stable rustfmt");
            return;
        }
    };
}

pub(crate) use needs_nightly;
pub(crate) use needs_stable;
