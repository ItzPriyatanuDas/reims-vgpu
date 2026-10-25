//! `features.md`'s guards: every `test: <module>::<name>` it names is a test function in
//! `src/<module>.rs` or under `src/<module>/`, so deleting a feature's test fails here.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::{Path, PathBuf};

/// The crate's directory, which cargo and nextest give a test process: read at run time, so the
/// test is the same binary in every worktree (docs/development.md, "Building through kache").
fn crate_dir() -> PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .expect("CARGO_MANIFEST_DIR: run the tests with cargo or nextest")
}

/// Every backquoted span of `text` (a span does not cross a line).
fn spans(text: &str) -> Vec<&str> {
    text.lines()
        .flat_map(|line| line.split('`').skip(1).step_by(2))
        .collect()
}

/// The guards `text` names: `(module, test)` for each `test: <module>::<name>` span, other
/// than the form itself, which the file's preamble quotes.
fn guards(text: &str) -> Vec<(String, String)> {
    spans(text)
        .into_iter()
        .filter_map(|s| s.strip_prefix("test:"))
        .filter(|s| !s.contains('<'))
        .map(|s| {
            let s = s.trim();
            let (module, name) = s
                .rsplit_once("::")
                .unwrap_or_else(|| panic!("`test: {s}` names no module"));
            (module.to_owned(), name.to_owned())
        })
        .collect()
}

/// The Rust sources of `module`: `src/<module>.rs` and every file under `src/<module>/`.
fn sources(src: &Path, module: &str) -> Vec<PathBuf> {
    let rel = module.replace("::", "/");
    let mut files = Vec::new();
    let file = src.join(format!("{rel}.rs"));
    if file.is_file() {
        files.push(file);
    }
    let mut dirs = vec![src.join(&rel)];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                files.push(path);
            }
        }
    }
    files
}

/// Every `test: <module>::<name>` guard of `features.md` names a function of that module
#[test]
fn features_md_guards_exist() {
    let dir = crate_dir();
    let text = std::fs::read_to_string(dir.join("features.md")).unwrap();
    let src = dir.join("src");
    let mut missing = Vec::new();
    for (module, name) in guards(&text) {
        let wanted = format!("fn {name}(");
        let found = sources(&src, &module).iter().any(|file| {
            std::fs::read_to_string(file)
                .unwrap()
                .lines()
                .any(|l| l.trim_start().contains(&wanted))
        });
        if !found {
            missing.push(format!("{module}::{name}"));
        }
    }
    assert!(
        missing.is_empty(),
        "features.md names tests that do not exist: {missing:?}"
    );
}

/// The parser: spans, and guards with nested modules
#[test]
fn guards_parse() {
    let text = "`test: <module>::<name>`\n- a thing. `test: geometry::keeps_ratio`, `manual`\n\
                - `test: a::b::c` `test`";
    assert_eq!(
        guards(text),
        vec![
            ("geometry".to_owned(), "keeps_ratio".to_owned()),
            ("a::b".to_owned(), "c".to_owned()),
        ]
    );
}
