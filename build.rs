//! Records the versions of the main libraries for `far:about`, as Far lists
//! the libraries it uses.

use std::fs;

const SHOWN: &[&str] = &[
    "crossterm",
    "syntect",
    "rusqlite",
    "zip",
    "tar",
    "flate2",
    "fancy-regex",
    "encoding_rs",
];

fn main() {
    println!("cargo:rerun-if-changed=Cargo.lock");
    let lock = fs::read_to_string("Cargo.lock").unwrap_or_default();
    let mut found = Vec::new();
    let mut name = None;
    for line in lock.lines() {
        if let Some(n) = line.strip_prefix("name = \"") {
            name = n.strip_suffix('"');
        } else if let (Some(v), Some(n)) = (line.strip_prefix("version = \""), name.take()) {
            if SHOWN.contains(&n) {
                // A library can appear twice, as another one may need an older
                // version. The lock lists versions in order, so the last is the
                // newest, which is the one far_linux uses.
                let entry = format!("{n} {}", v.trim_end_matches('"'));
                let prefix = format!("{n} ");
                match found
                    .iter_mut()
                    .find(|f: &&mut String| f.starts_with(&prefix))
                {
                    Some(old) => *old = entry,
                    None => found.push(entry),
                }
            }
        }
    }
    println!("cargo:rustc-env=FAR_LIBRARIES={}", found.join(", "));
}
