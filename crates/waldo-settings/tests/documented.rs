//! Every operator setting is named in the configuration reference,
//! `docs/configuration.md`.
//!
//! The reference is the newcomer's only list of what can be set without a
//! source edit, so a setting the runtime or the installer reads but the
//! reference does not name is a defect this test catches: declared settings,
//! and every `WALDO_` name the runtime crates, the plugin manifests, or the
//! installer mention.

use std::{collections::BTreeSet, fs, path::Path};

fn repository_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the settings crate lives at crates/waldo-settings")
}

fn configuration_reference() -> String {
    fs::read_to_string(repository_root().join("docs/configuration.md"))
        .expect("docs/configuration.md is readable")
}

fn names_in(text: &str) -> BTreeSet<String> {
    let bytes = text.as_bytes();
    let mut names = BTreeSet::new();
    let mut index = 0;
    while let Some(offset) = text[index..].find("WALDO_") {
        let start = index + offset;
        let mut end = start;
        while end < bytes.len()
            && (bytes[end].is_ascii_uppercase()
                || bytes[end].is_ascii_digit()
                || bytes[end] == b'_')
        {
            end += 1;
        }
        let name = &text[start..end];
        if name.len() > "WALDO_".len() {
            names.insert(name.to_owned());
        }
        index = end;
    }
    names
}

fn runtime_sources() -> Vec<std::path::PathBuf> {
    let mut files = Vec::new();
    let crates = repository_root().join("crates");
    for entry in fs::read_dir(&crates).expect("crates/ is readable") {
        let path = entry.expect("crate entry").path();
        // The verification crate is a test harness, not the shipped runtime;
        // its prerequisite flags are documented with the conformance suite.
        // This crate's own source is covered by the declared list above, and
        // its unit tests deliberately spell settings wrongly.
        if path
            .file_name()
            .is_some_and(|name| name == "waldo-verification" || name == "waldo-settings")
        {
            continue;
        }
        collect_rust_files(&path.join("src"), &mut files);
    }
    files.push(repository_root().join("install-local"));
    collect_files(&repository_root().join("plugins"), &mut files);
    files
}

/// Every directory the walk is promised is read, so a missing or unreadable
/// one fails the test instead of quietly shrinking what it checks.
fn read_dir(directory: &Path) -> fs::ReadDir {
    fs::read_dir(directory)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", directory.display()))
}

fn collect_rust_files(directory: &Path, files: &mut Vec<std::path::PathBuf>) {
    for entry in read_dir(directory) {
        let path = entry.expect("source entry").path();
        if path.is_dir() {
            collect_rust_files(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

fn collect_files(directory: &Path, files: &mut Vec<std::path::PathBuf>) {
    for entry in read_dir(directory) {
        let path = entry.expect("plugin entry").path();
        if path.is_dir() {
            collect_files(&path, files);
        } else {
            files.push(path);
        }
    }
}

#[test]
fn every_declared_setting_has_a_reference_row() {
    let reference = configuration_reference();
    let missing: Vec<_> = waldo_settings::ALL
        .iter()
        .filter(|spec| !reference.contains(&format!("`{}`", spec.name)))
        .map(|spec| spec.name)
        .collect();
    assert!(
        missing.is_empty(),
        "docs/configuration.md names no row for {missing:?}"
    );
}

#[test]
fn every_waldo_name_the_runtime_or_installer_reads_is_documented() {
    let documented = names_in(&configuration_reference());
    let mut undocumented = BTreeSet::new();
    for file in runtime_sources() {
        let text = fs::read_to_string(&file)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", file.display()));
        for name in names_in(&text) {
            if !documented.contains(&name) {
                undocumented.insert(format!(
                    "{name} ({})",
                    file.strip_prefix(repository_root()).unwrap().display()
                ));
            }
        }
    }
    assert!(
        undocumented.is_empty(),
        "settings read but absent from docs/configuration.md: {undocumented:#?}"
    );
}
