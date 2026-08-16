//! Byte-exact rendering fixtures.
//!
//! Each directory under `tests/golden/` holds a `config.boot` and the files
//! Nightshade produces from it. The configs are written in the format an
//! operator writes, so a fixture reads as a configuration rather than as test
//! data, and a reviewer can see what is being claimed.
//!
//! Byte-exact is the point. It makes a rendering change visible in the diff of
//! the commit that caused it -- which is the only reliable way to notice that
//! a tweak to one interface type quietly moved a line in another.
//!
//! Regenerate after a deliberate change:
//!
//! ```sh
//! UPDATE_GOLDEN=1 cargo test -p nightshade-render --test golden
//! ```
//!
//! and read the resulting diff before committing it. A fixture updated without
//! being read is a fixture that tests nothing.

use std::collections::BTreeMap;
use std::path::{Path as FsPath, PathBuf};
use std::sync::Arc;

use nightshade_common::paths::Paths;
use nightshade_render::{Artifacts, Host, MockHost};
use nightshade_schema::config::ConfigTree;
use nightshade_schema::curly;
use nightshade_schema::model::Schema;

fn golden_dir(name: &str) -> PathBuf {
    FsPath::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name)
}

fn updating() -> bool {
    std::env::var_os("UPDATE_GOLDEN").is_some()
}

/// Parse a fixture and hold it to the same standard as a committed config.
///
/// A fixture that would not survive `commit` is a fixture that proves the
/// renderer handles something the renderer can never be given.
fn fixture(name: &str) -> ConfigTree {
    let path = golden_dir(name).join("config.boot");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
    let config = curly::parse(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));

    let schema = Schema::compiled();
    assert_eq!(
        schema.validate_tree(&config),
        [],
        "{} is not a valid config",
        path.display()
    );
    assert_eq!(
        schema.check_constraints(&config),
        [],
        "{} breaks a cross-node constraint",
        path.display()
    );
    config
}

/// Everything both renderers produce, keyed by file name, plus a listing of
/// the actions.
fn produce(config: &ConfigTree) -> BTreeMap<String, String> {
    let host: Arc<dyn Host> = Arc::new(MockHost::new());
    let mut out = BTreeMap::new();
    let mut actions = String::new();

    for renderer in nightshade_render::all(Paths::system(), Arc::clone(&host)) {
        let artifacts = renderer
            .render(config)
            .unwrap_or_else(|e| panic!("{} render: {e}", renderer.name()));
        renderer
            .check(&artifacts)
            .unwrap_or_else(|e| panic!("{} check: {e}", renderer.name()));

        for (path, contents) in artifacts.all_files() {
            let name = path
                .file_name()
                .expect("a file name")
                .to_string_lossy()
                .into_owned();
            if let Some(clash) = out.insert(name.clone(), contents) {
                assert_eq!(clash, out[&name], "two renderers produced a different {name}");
            }
        }

        // Actions are part of the output and have to be pinned too: a lost
        // `networkctl reload` renders perfectly and applies nothing.
        for action in &artifacts.actions {
            match action.argv() {
                Some(argv) => actions.push_str(&argv.join(" ")),
                None => actions.push_str("(no command)"),
            }
            actions.push('\n');
        }

        assert_eq!(
            artifacts,
            renderer.render(config).unwrap(),
            "{} rendered differently the second time",
            renderer.name()
        );
        assert_no_stale_state(&artifacts);
    }

    out.insert("actions".to_string(), actions);
    out
}

/// Nothing in the output may depend on when or where it was produced.
fn assert_no_stale_state(artifacts: &Artifacts) {
    for (path, contents) in artifacts.all_files() {
        assert!(
            contents.starts_with("# Managed by Nightshade."),
            "{} has no managed-by header",
            path.display()
        );
    }
}

fn check(name: &str) {
    let produced = produce(&fixture(name));
    let dir = golden_dir(name);
    let expected_dir = dir.join("expected");

    if updating() {
        let _ = std::fs::remove_dir_all(&expected_dir);
        std::fs::create_dir_all(&expected_dir).expect("creating the golden directory");
        for (file, contents) in &produced {
            std::fs::write(expected_dir.join(file), contents).expect("writing a golden file");
        }
        return;
    }

    let mut expected = BTreeMap::new();
    let entries = std::fs::read_dir(&expected_dir).unwrap_or_else(|e| {
        panic!(
            "{} is missing: {e}\nrun UPDATE_GOLDEN=1 cargo test -p nightshade-render --test golden",
            expected_dir.display()
        )
    });
    for entry in entries {
        let entry = entry.expect("reading the golden directory");
        expected.insert(
            entry.file_name().to_string_lossy().into_owned(),
            std::fs::read_to_string(entry.path()).expect("reading a golden file"),
        );
    }

    let produced_names: Vec<&String> = produced.keys().collect();
    let expected_names: Vec<&String> = expected.keys().collect();
    assert_eq!(
        produced_names, expected_names,
        "{name}: the set of rendered files changed"
    );

    for (file, contents) in &produced {
        assert_eq!(
            contents.as_str(),
            expected[file].as_str(),
            "{name}/{file} changed"
        );
    }
}

#[test]
fn system() {
    check("system");
}

#[test]
fn ethernet() {
    check("ethernet");
}

#[test]
fn loopback() {
    check("loopback");
}

#[test]
fn vlan() {
    check("vlan");
}

#[test]
fn bonding() {
    check("bonding");
}

#[test]
fn bridge() {
    check("bridge");
}

#[test]
fn vxlan() {
    check("vxlan");
}

/// Everything at once, which is where interactions between the types show up:
/// a bond enslaving ports, a VLAN on the bond, a bridge over the VLAN and a
/// VXLAN sourced from the bond.
#[test]
fn combined() {
    check("combined");
}
