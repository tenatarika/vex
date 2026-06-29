//! Phase 1 of the multi-repo design (docs/MULTIREPO.md): workspace
//! manifest parsing + member resolution. No indexing/fanout yet.

use std::fs;
use std::path::{Path, PathBuf};

use tempfile::TempDir;
use vex::util::config;
use vex::workspace::{find_workspace_file, Workspace, WORKSPACE_FILE};

// Every test pre-canonicalizes `tmp.path()`: on macOS the temp dir is
// reached via the `/tmp -> /private/tmp` symlink, so a raw TempDir path
// won't equal the canonicalized member roots `Workspace::load` produces.

/// Create a member repo dir under `base`, optionally with a `.vex.toml`.
fn mk_repo(base: &Path, name: &str, vex_toml: Option<&str>) -> PathBuf {
    let dir = base.join(name);
    fs::create_dir_all(&dir).unwrap();
    if let Some(body) = vex_toml {
        fs::write(dir.join(".vex.toml"), body).unwrap();
    }
    dir
}

fn write_manifest(base: &Path, body: &str) -> PathBuf {
    let file = base.join(WORKSPACE_FILE);
    fs::write(&file, body).unwrap();
    file
}

#[test]
fn resolves_members_with_default_and_explicit_names() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    mk_repo(&root, "service-a", None);
    mk_repo(&root, "service-b", None);
    let file = write_manifest(
        &root,
        "[[repo]]\npath = \"service-a\"\n\n[[repo]]\npath = \"service-b\"\nname = \"beta\"\n",
    );

    let ws = Workspace::load(&file).unwrap();
    assert_eq!(ws.members.len(), 2);
    assert_eq!(ws.members[0].display_name, "service-a"); // default = dir name
    assert_eq!(ws.members[1].display_name, "beta"); // explicit override
                                                    // Roots are canonical and point at the right dirs.
    assert_eq!(ws.members[0].root, root.join("service-a"));
    assert_eq!(ws.members[1].root, root.join("service-b"));
}

#[test]
fn relative_paths_resolve_against_manifest_dir() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    mk_repo(&root, "libs/shared", None);
    let file = write_manifest(&root, "[[repo]]\npath = \"libs/shared\"\n");

    let ws = Workspace::load(&file).unwrap();
    assert_eq!(ws.members[0].root, root.join("libs").join("shared"));
    assert_eq!(ws.members[0].display_name, "shared");
}

#[test]
fn member_index_dir_matches_single_repo_mode() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    mk_repo(&root, "service-a", None);
    let file = write_manifest(&root, "[[repo]]\npath = \"service-a\"\n");

    let ws = Workspace::load(&file).unwrap();
    // The whole point: a member's index dir is exactly what single-repo
    // mode computes for that canonical root.
    assert_eq!(
        ws.members[0].index_dir(),
        config::index_dir(&root.join("service-a"))
    );
}

#[test]
fn rejects_nonexistent_member() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let file = write_manifest(&root, "[[repo]]\npath = \"ghost\"\n");

    let err = Workspace::load(&file).unwrap_err().to_string();
    assert!(err.contains("does not resolve"), "got: {err}");
}

#[test]
fn rejects_nested_members() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    mk_repo(&root, "outer", None);
    mk_repo(&root, "outer/inner", None);
    let file = write_manifest(
        &root,
        "[[repo]]\npath = \"outer\"\n\n[[repo]]\npath = \"outer/inner\"\n",
    );

    let err = Workspace::load(&file).unwrap_err().to_string();
    assert!(err.contains("nested"), "got: {err}");
}

#[test]
fn rejects_duplicate_members() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    mk_repo(&root, "service-a", None);
    let file = write_manifest(
        &root,
        "[[repo]]\npath = \"service-a\"\n\n[[repo]]\npath = \"./service-a\"\n",
    );

    let err = Workspace::load(&file).unwrap_err().to_string();
    assert!(err.contains("same path"), "got: {err}");
}

#[test]
fn accepts_member_with_local_cache_override() {
    // Phase 2: a member's OWN local_cache is honoured (resolved per-member by
    // the CacheResolver), no longer rejected at load.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    mk_repo(&root, "service-a", Some("local_cache = true\n"));
    let file = write_manifest(&root, "[[repo]]\npath = \"service-a\"\n");

    let ws = Workspace::load(&file).expect("per-member local_cache must load");
    assert_eq!(ws.members.len(), 1);
}

#[test]
fn accepts_member_with_cache_dir_override() {
    // Phase 2: a member's OWN cache_dir is honoured, no longer rejected.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    mk_repo(&root, "service-a", Some("cache_dir = \"./.vex/cache\"\n"));
    let file = write_manifest(&root, "[[repo]]\npath = \"service-a\"\n");

    let ws = Workspace::load(&file).expect("per-member cache_dir must load");
    assert_eq!(ws.members.len(), 1);
}

#[test]
fn ancestor_vex_toml_does_not_reject_members() {
    // A `.vex.toml` at the workspace ROOT (shared/global config) that sets
    // a cache override must NOT reject members — only a member's OWN
    // .vex.toml triggers the MVP rejection. Regression guard for the
    // load_config walk-up over-rejection (review H1).
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    fs::write(root.join(".vex.toml"), "local_cache = true\n").unwrap();
    mk_repo(&root, "service-a", None); // no own .vex.toml
    let file = write_manifest(&root, "[[repo]]\npath = \"service-a\"\n");

    let ws = Workspace::load(&file).expect("ancestor config must not reject members");
    assert_eq!(ws.members.len(), 1);
    assert_eq!(ws.members[0].display_name, "service-a");
}

#[test]
fn rejects_empty_manifest() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let file = write_manifest(&root, "# no members\n");

    let err = Workspace::load(&file).unwrap_err().to_string();
    assert!(err.contains("no [[repo]] members"), "got: {err}");
}

#[test]
fn find_workspace_file_walks_up_from_member() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let member = mk_repo(&root, "service-a", None);
    let nested = member.join("src");
    fs::create_dir_all(&nested).unwrap();
    let file = write_manifest(&root, "[[repo]]\npath = \"service-a\"\n");

    let found = find_workspace_file(&nested).expect("should walk up to workspace file");
    assert_eq!(found, file);
}
