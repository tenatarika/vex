//! P5 (`docs/V9-FORMAT.md` §4.1, §13 Q7) — `vex modules` (hidden alias
//! `clusters`): CLI surface over the v9 symbol-cluster section. Mirrors
//! `cli_status_clusters_test.rs`'s isolated-cache-dir pattern.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;
use vex::store::format::{
    CallGraphHeader, Header, HierarchyHeader, PatternSkeletonHeader, UnresolvedHierarchyHeader,
    UnresolvedRefsHeader, V5SectionHeader,
};

fn vex_in(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("vex").unwrap();
    cmd.current_dir(dir);
    cmd.env("VEX_CACHE_DIR", dir.join(".vex-test-cache"));
    cmd
}

fn write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, body).unwrap();
}

/// A fully connected call clique: every function calls every other one,
/// so the projection gives it dense intra-cluster weight. The functions
/// are spread over three files of `dir` (no file holds 60% of them) so the
/// cluster label is the directory, not a single file.
fn write_clique(dir: &Path, prefix: &str, names: &[&str], bridge: &str) {
    let mut bodies = [String::new(), String::new(), String::new()];
    for (i, n) in names.iter().enumerate() {
        let calls: Vec<String> = names
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .map(|(_, m)| format!("{prefix}_{m}();"))
            .collect();
        let bridge = if i == 0 { bridge } else { "" };
        bodies[i % 3].push_str(&format!(
            "pub fn {prefix}_{n}() {{ {} {bridge} }}\n",
            calls.join(" ")
        ));
    }
    write(&dir.join("part_a.rs"), &bodies[0]);
    write(&dir.join("part_b.rs"), &bodies[1]);
    write(&dir.join("part_c.rs"), &bodies[2]);
}

/// Three directories, three cliques of different sizes (5 / 4 / 3), one
/// isolated function (UNCLUSTERED) and one markdown heading (NOT_ELIGIBLE).
fn write_fixture(dir: &Path) {
    write_clique(
        &dir.join("src/net"),
        "net",
        &["connect", "send", "recv", "close", "retry"],
        "store_read();",
    );
    write_clique(
        &dir.join("src/store"),
        "store",
        &["read", "write", "flush", "open"],
        "",
    );
    write_clique(&dir.join("src/ui"), "ui", &["draw", "layout", "paint"], "");
    write(&dir.join("src/misc/lone.rs"), "pub fn lonely_helper() {}\n");
    write(&dir.join("src/misc/dup_a.rs"), "pub fn shared_name() {}\n");
    write(&dir.join("src/misc/dup_b.rs"), "pub fn shared_name() {}\n");
    write(&dir.join("docs/guide.md"), "# Quick Start Guide\n\ntext\n");
}

fn index(dir: &Path) {
    write_fixture(dir);
    vex_in(dir).args(["index"]).assert().success();
}

fn modules_json(dir: &Path, args: &[&str]) -> Value {
    let out = vex_in(dir)
        .arg("modules")
        .args(args)
        .args(["--no-stale-check", "--format", "json"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "vex modules {args:?} failed: {:?}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("valid JSON envelope")
}

fn results(env: &Value) -> &Value {
    env.get("results").expect("envelope has results")
}

fn clusters(env: &Value) -> &Vec<Value> {
    results(env)["clusters"].as_array().expect("clusters array")
}

fn find_index_file(dir: &Path) -> Option<PathBuf> {
    for e in std::fs::read_dir(dir).ok()?.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Some(f) = find_index_file(&p) {
                return Some(f);
            }
        } else if p.file_name().and_then(|s| s.to_str()) == Some("index.vex") {
            return Some(p);
        }
    }
    None
}

fn cluster_header_offset() -> usize {
    Header::SIZE
        + CallGraphHeader::SIZE
        + V5SectionHeader::SIZE
        + PatternSkeletonHeader::SIZE
        + UnresolvedRefsHeader::SIZE
        + HierarchyHeader::SIZE
        + UnresolvedHierarchyHeader::SIZE
}

#[test]
fn list_json_has_documented_shape() {
    let tmp = TempDir::new().unwrap();
    index(tmp.path());
    let env = modules_json(tmp.path(), &["--min-size", "1"]);
    let r = results(&env);
    assert_eq!(r["algorithm"], "leiden-cpm/1");
    assert_eq!(r["resolution"], "1/8");
    assert_eq!(r["stale"], false);
    assert_eq!(r["new_since_build"], 0);
    assert!(r["total_clusters"].as_u64().unwrap() >= 3);
    assert!(r["unclustered"].as_u64().unwrap() >= 1);
    assert!(r["not_eligible"].as_u64().unwrap() >= 1);
    assert!(
        r.get("empty_reason").is_none(),
        "omitted when results exist"
    );
    assert!(r.get("symbol").is_none(), "no symbol key in list mode");
    let cs = clusters(&env);
    assert!(cs.len() >= 3);
    for c in cs {
        assert!(c["id"].is_u64());
        assert!(c["label"].is_string());
        assert!(c["size"].is_u64());
        assert!(c["size_at_build"].is_u64());
        let coh = c["cohesion"].as_f64().unwrap();
        assert!((0.0..=1.0).contains(&coh));
        let (i, w) = (
            c["internal_weight"].as_f64().unwrap(),
            c["cut_weight"].as_f64().unwrap(),
        );
        let want = if i + w == 0.0 { 0.0 } else { i / (i + w) };
        assert!(
            (coh - want).abs() <= 5e-5,
            "cohesion {coh} != internal/(internal+cut) = {want}"
        );
        assert!(c["internal_weight"].is_u64());
        assert!(c["cut_weight"].is_u64());
        let hubs = c["hubs"].as_array().unwrap();
        assert!(!hubs.is_empty() && hubs.len() <= 3);
        for h in hubs {
            assert!(h["name"].is_string() && h["path"].is_string() && h["line"].is_u64());
        }
        assert!(
            c.get("members").is_none(),
            "members default to 0 in list mode"
        );
    }
    let labels: Vec<&str> = cs.iter().map(|c| c["label"].as_str().unwrap()).collect();
    for want in ["src/net/", "src/store/", "src/ui/"] {
        assert!(labels.contains(&want), "label {want} missing in {labels:?}");
    }
}

#[test]
fn list_text_has_header_and_cluster_lines_and_no_stale_line() {
    let tmp = TempDir::new().unwrap();
    index(tmp.path());
    let out = vex_in(tmp.path())
        .args(["modules", "--no-stale-check", "--format", "text"])
        .assert()
        .success();
    let s = String::from_utf8_lossy(&out.get_output().stdout).into_owned();
    let first = s.lines().next().unwrap();
    assert!(first.starts_with("Modules"), "header: {first}");
    assert!(first.contains("leiden-cpm/1"), "header: {first}");
    assert!(first.contains("γ=1/8"), "header: {first}");
    assert!(first.contains("unclustered") && first.contains("not eligible"));
    let net = s
        .lines()
        .find(|l| l.contains("src/net/"))
        .expect("net line");
    assert!(net.contains('#') && net.contains("symbols"));
    assert!(net.contains("cohesion") && net.contains("hubs:"));
    assert!(
        !s.lines().any(|l| l.starts_with('!')),
        "no stale line on a fresh index:\n{s}"
    );
}

#[test]
fn min_size_filters_by_live_size() {
    let tmp = TempDir::new().unwrap();
    index(tmp.path());
    let env = modules_json(tmp.path(), &["--min-size", "1"]);
    let sizes: Vec<u64> = clusters(&env)
        .iter()
        .map(|c| c["size"].as_u64().unwrap())
        .collect();
    let max = *sizes.iter().max().unwrap();
    let min = *sizes.iter().min().unwrap();
    assert!(max > min, "fixture should yield distinct sizes: {sizes:?}");
    // Raising the floor to the max drops every smaller cluster.
    let env = modules_json(tmp.path(), &["--min-size", &max.to_string()]);
    assert!(clusters(&env)
        .iter()
        .all(|c| c["size"].as_u64().unwrap() >= max));
    assert!(clusters(&env).len() < sizes.len());
    // Default --min-size is 3.
    let env = modules_json(tmp.path(), &[]);
    assert!(clusters(&env)
        .iter()
        .all(|c| c["size"].as_u64().unwrap() >= 3));
}

#[test]
fn min_size_above_everything_exits_1_filtered_all() {
    let tmp = TempDir::new().unwrap();
    index(tmp.path());
    let out = vex_in(tmp.path())
        .args([
            "modules",
            "--min-size",
            "100000",
            "--no-stale-check",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let env: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(results(&env)["empty_reason"], "filtered_all");
    assert!(clusters(&env).is_empty());
    assert!(!out.stderr.is_empty(), "stderr hint expected");
}

#[test]
fn sort_size_is_descending_and_sort_cohesion_orders_by_cohesion() {
    let tmp = TempDir::new().unwrap();
    index(tmp.path());
    let env = modules_json(tmp.path(), &["--min-size", "1"]);
    let sizes: Vec<u64> = clusters(&env)
        .iter()
        .map(|c| c["size"].as_u64().unwrap())
        .collect();
    assert!(
        sizes.windows(2).all(|w| w[0] >= w[1]),
        "size desc: {sizes:?}"
    );

    let env = modules_json(tmp.path(), &["--min-size", "1", "--sort", "cohesion"]);
    let cohs: Vec<f64> = clusters(&env)
        .iter()
        .map(|c| c["cohesion"].as_f64().unwrap())
        .collect();
    assert!(
        cohs.windows(2).all(|w| w[0] >= w[1]),
        "cohesion desc: {cohs:?}"
    );
}

#[test]
fn limit_truncates_but_total_clusters_reports_all() {
    let tmp = TempDir::new().unwrap();
    index(tmp.path());
    let env = modules_json(tmp.path(), &["--min-size", "1", "--limit", "1"]);
    assert_eq!(clusters(&env).len(), 1);
    assert!(results(&env)["total_clusters"].as_u64().unwrap() >= 3);
}

#[test]
fn members_flag_lists_members_ordered_by_path_then_line() {
    let tmp = TempDir::new().unwrap();
    index(tmp.path());
    let env = modules_json(tmp.path(), &["--members", "100"]);
    for c in clusters(&env) {
        let ms = c["members"].as_array().expect("members present");
        assert_eq!(ms.len() as u64, c["size"].as_u64().unwrap());
        let keys: Vec<(String, u64)> = ms
            .iter()
            .map(|m| {
                assert!(m["name"].is_string() && m["kind"].is_string());
                (
                    m["path"].as_str().unwrap().to_string(),
                    m["line"].as_u64().unwrap(),
                )
            })
            .collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "members ordered by path, then line");
    }
    let env = modules_json(tmp.path(), &["--members", "2"]);
    assert!(clusters(&env)
        .iter()
        .all(|c| c["members"].as_array().unwrap().len() == 2));
}

#[test]
fn scope_exclude_drops_clusters_with_no_member_in_scope() {
    let tmp = TempDir::new().unwrap();
    index(tmp.path());
    let env = modules_json(tmp.path(), &["--min-size", "1", "--exclude", "src/net/**"]);
    assert!(clusters(&env)
        .iter()
        .all(|c| c["label"].as_str().unwrap() != "src/net/"));
    assert!(clusters(&env)
        .iter()
        .any(|c| c["label"].as_str().unwrap() == "src/store/"));
}

#[test]
fn scope_include_restricts_and_displayed_size_is_in_scope_count() {
    let tmp = TempDir::new().unwrap();
    index(tmp.path());
    let env = modules_json(
        tmp.path(),
        &[
            "--min-size",
            "1",
            "--include",
            "src/store/**",
            "--members",
            "100",
        ],
    );
    let cs = clusters(&env);
    assert!(!cs.is_empty());
    for c in cs {
        let ms = c["members"].as_array().unwrap();
        assert!(ms
            .iter()
            .all(|m| m["path"].as_str().unwrap().starts_with("src/store/")));
        assert_eq!(ms.len() as u64, c["size"].as_u64().unwrap());
    }
}

#[test]
fn scope_matching_nothing_exits_1_filtered_all() {
    let tmp = TempDir::new().unwrap();
    index(tmp.path());
    let out = vex_in(tmp.path())
        .args([
            "modules",
            "--include",
            "nowhere/**",
            "--no-stale-check",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let env: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(results(&env)["empty_reason"], "filtered_all");
}

#[test]
fn symbol_mode_reports_status_cluster_and_default_members() {
    let tmp = TempDir::new().unwrap();
    index(tmp.path());
    let env = modules_json(tmp.path(), &["store_read"]);
    let r = results(&env);
    let sym = r["symbol"].as_array().expect("symbol array");
    assert_eq!(sym.len(), 1);
    assert_eq!(sym[0]["name"], "store_read");
    assert_eq!(sym[0]["path"], "src/store/part_a.rs");
    assert_eq!(sym[0]["status"], "clustered");
    let id = sym[0]["cluster_id"].as_u64().unwrap();
    let cs = clusters(&env);
    assert_eq!(cs.len(), 1);
    assert_eq!(cs[0]["id"].as_u64().unwrap(), id);
    let members = cs[0]["members"]
        .as_array()
        .expect("members default 25 in symbol mode");
    assert!(members.iter().any(|m| m["name"] == "store_read"));
}

#[test]
fn symbol_mode_unknown_symbol_exits_1_symbol_not_found() {
    let tmp = TempDir::new().unwrap();
    index(tmp.path());
    let out = vex_in(tmp.path())
        .args([
            "modules",
            "does_not_exist_anywhere",
            "--no-stale-check",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let env: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(results(&env)["empty_reason"], "symbol_not_found");
    assert!(!out.stderr.is_empty());
}

#[test]
fn symbol_mode_unclustered_symbol_exits_1_with_status() {
    let tmp = TempDir::new().unwrap();
    index(tmp.path());
    let out = vex_in(tmp.path())
        .args([
            "modules",
            "lonely_helper",
            "--no-stale-check",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let env: Value = serde_json::from_slice(&out.stdout).unwrap();
    let r = results(&env);
    assert_eq!(r["empty_reason"], "symbol_unclustered");
    assert_eq!(r["symbol"][0]["status"], "unclustered");
    assert!(r["symbol"][0].get("cluster_id").is_none());
}

#[test]
fn symbol_mode_not_eligible_symbol_reports_status() {
    let tmp = TempDir::new().unwrap();
    index(tmp.path());
    let out = vex_in(tmp.path())
        .args([
            "modules",
            "Quick Start Guide",
            "--no-stale-check",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let env: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(results(&env)["symbol"][0]["status"], "not_eligible");
    assert_eq!(results(&env)["empty_reason"], "symbol_unclustered");
}

#[test]
fn symbol_mode_text_shows_status_and_cluster() {
    let tmp = TempDir::new().unwrap();
    index(tmp.path());
    let out = vex_in(tmp.path())
        .args([
            "modules",
            "net_send",
            "--no-stale-check",
            "--format",
            "text",
        ])
        .assert()
        .success();
    let s = String::from_utf8_lossy(&out.get_output().stdout).into_owned();
    assert!(s.contains("net_send") && s.contains("clustered"), "{s}");
    assert!(s.contains("src/net/"), "{s}");
}

#[test]
fn clusters_alias_is_equivalent_and_hidden_from_help() {
    let tmp = TempDir::new().unwrap();
    index(tmp.path());
    let a = vex_in(tmp.path())
        .args(["modules", "--no-stale-check"])
        .output()
        .unwrap();
    let b = vex_in(tmp.path())
        .args(["clusters", "--no-stale-check"])
        .output()
        .unwrap();
    assert!(a.status.success() && b.status.success());
    assert_eq!(a.stdout, b.stdout);

    let help = vex_in(tmp.path()).arg("--help").output().unwrap();
    let h = String::from_utf8_lossy(&help.stdout);
    assert!(h.contains("modules"));
    assert!(
        !h.lines().any(|l| l.trim_start().starts_with("clusters")),
        "alias must stay hidden"
    );
}

#[test]
fn compact_format_is_line_oriented() {
    let tmp = TempDir::new().unwrap();
    index(tmp.path());
    let out = vex_in(tmp.path())
        .args(["modules", "--no-stale-check", "--format", "compact"])
        .assert()
        .success();
    let s = String::from_utf8_lossy(&out.get_output().stdout).into_owned();
    assert!(
        s.lines()
            .any(|l| l.starts_with("cluster ") && l.contains("src/net/")),
        "{s}"
    );
    assert!(
        s.lines()
            .next()
            .unwrap()
            .starts_with("modules leiden-cpm/1"),
        "{s}"
    );
    assert!(!s.contains("Modules —"));
}

#[test]
fn no_clusters_index_exits_1_clusters_not_built() {
    let tmp = TempDir::new().unwrap();
    write_fixture(tmp.path());
    vex_in(tmp.path())
        .args(["index", "--no-clusters"])
        .assert()
        .success();
    let out = vex_in(tmp.path())
        .args(["modules", "--no-stale-check", "--format", "json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let env: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(results(&env)["empty_reason"], "clusters_not_built");
    assert!(clusters(&env).is_empty());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("vex index"),
        "hint should point at `vex index`: {err}"
    );
    // Symbol mode degrades the same way.
    let out = vex_in(tmp.path())
        .args([
            "modules",
            "net_send",
            "--no-stale-check",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let env: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(results(&env)["empty_reason"], "clusters_not_built");
}

#[test]
fn semantically_corrupt_cluster_section_exits_2() {
    let tmp = TempDir::new().unwrap();
    index(tmp.path());
    let idx = find_index_file(&tmp.path().join(".vex-test-cache")).expect("index.vex");
    let mut bytes = std::fs::read(&idx).unwrap();
    // resolution_den lives at byte 36 of the ClusterHeader; 0 is invalid.
    let at = cluster_header_offset() + 36;
    bytes[at..at + 4].copy_from_slice(&0u32.to_le_bytes());
    std::fs::write(&idx, bytes).unwrap();
    let out = vex_in(tmp.path())
        .args(["modules", "--no-stale-check"])
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("vex index"));
}

#[test]
fn structurally_corrupt_cluster_section_exits_2() {
    let tmp = TempDir::new().unwrap();
    index(tmp.path());
    let idx = find_index_file(&tmp.path().join(".vex-test-cache")).expect("index.vex");
    let mut bytes = std::fs::read(&idx).unwrap();
    // assign_len lives at byte 8 of the ClusterHeader; it must be 4*symbol_count.
    let at = cluster_header_offset() + 8;
    bytes[at..at + 8].copy_from_slice(&4u64.to_le_bytes());
    std::fs::write(&idx, bytes).unwrap();
    let out = vex_in(tmp.path())
        .args(["modules", "--no-stale-check"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn stale_line_and_json_after_edit_and_update_then_cleared_by_reindex() {
    let tmp = TempDir::new().unwrap();
    index(tmp.path());
    // Add a brand-new symbol to an existing file, then incremental update.
    let p = tmp.path().join("src/store/part_a.rs");
    let mut body = std::fs::read_to_string(&p).unwrap();
    body.push_str("pub fn store_brand_new() { store_read(); }\n");
    std::fs::write(&p, body).unwrap();
    vex_in(tmp.path()).args(["update"]).assert().success();

    let env = modules_json(tmp.path(), &[]);
    let r = results(&env);
    assert_eq!(r["stale"], true);
    assert!(r["new_since_build"].as_u64().unwrap() >= 1);
    // Cluster staleness must NOT leak into the envelope's index-staleness
    // key. Run with the NORMAL stale check (the index is fresh after the
    // update), so the key is absent only if the handler never sets it.
    let out = vex_in(tmp.path())
        .args(["modules", "--format", "json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let env2: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(results(&env2)["stale"], true);
    let meta = env2.get("_meta").expect("_meta present");
    assert!(meta.get("vex.dev/stale").is_none(), "{meta}");
    // Editing without updating makes the INDEX stale, which the normal
    // check handles; it must not surface as a cluster flag either.
    let mut body = std::fs::read_to_string(&p).unwrap();
    body.push_str("pub fn store_unindexed() {}\n");
    std::fs::write(&p, body).unwrap();
    let out = vex_in(tmp.path())
        .args(["modules", "--format", "json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let env3: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        env3["_meta"].get("vex.dev/stale").is_none(),
        "{}",
        env3["_meta"]
    );

    let out = vex_in(tmp.path())
        .args(["modules", "--no-stale-check", "--format", "text"])
        .assert()
        .success();
    let s = String::from_utf8_lossy(&out.get_output().stdout).into_owned();
    let bang: Vec<&str> = s.lines().filter(|l| l.starts_with('!')).collect();
    assert_eq!(bang.len(), 1, "exactly one stale line:\n{s}");
    assert!(bang[0].contains("vex index"));

    vex_in(tmp.path()).args(["index"]).assert().success();
    let env = modules_json(tmp.path(), &[]);
    assert_eq!(results(&env)["stale"], false);
    assert_eq!(results(&env)["new_since_build"], 0);
}

#[test]
fn symbol_mode_new_since_build_symbol_reports_status() {
    let tmp = TempDir::new().unwrap();
    index(tmp.path());
    let p = tmp.path().join("src/store/part_a.rs");
    let mut body = std::fs::read_to_string(&p).unwrap();
    body.push_str("pub fn store_brand_new() { store_read(); }\n");
    std::fs::write(&p, body).unwrap();
    vex_in(tmp.path()).args(["update"]).assert().success();
    let out = vex_in(tmp.path())
        .args([
            "modules",
            "store_brand_new",
            "--no-stale-check",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let env: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(results(&env)["symbol"][0]["status"], "new_since_build");
}

#[test]
fn workspace_groups_output_per_member() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write_clique(
        &root.join("alpha/src"),
        "alpha",
        &["one", "two", "three", "four"],
        "",
    );
    write_clique(
        &root.join("beta/src"),
        "beta",
        &["uno", "dos", "tres", "cuatro"],
        "",
    );
    write(
        &root.join(".vex-workspace.toml"),
        "[[repo]]\npath = \"alpha\"\n\n[[repo]]\npath = \"beta\"\n",
    );
    vex_in(root)
        .args(["index", "--workspace"])
        .assert()
        .success();

    let out = vex_in(root)
        .args([
            "modules",
            "--workspace",
            "--no-stale-check",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let env: Value = serde_json::from_slice(&out.stdout).unwrap();
    let repos = results(&env)["repos"].as_array().expect("repos array");
    assert_eq!(repos.len(), 2);
    for r in repos {
        assert!(r["repo"].is_string());
        assert!(!r["clusters"].as_array().unwrap().is_empty());
        assert_eq!(r["algorithm"], "leiden-cpm/1");
    }

    // --limit applies per member.
    let out = vex_in(root)
        .args([
            "modules",
            "--workspace",
            "--limit",
            "1",
            "--min-size",
            "1",
            "--no-stale-check",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    let env: Value = serde_json::from_slice(&out.stdout).unwrap();
    for r in results(&env)["repos"].as_array().unwrap() {
        assert_eq!(r["clusters"].as_array().unwrap().len(), 1);
    }

    // Text output is grouped by repo.
    let out = vex_in(root)
        .args([
            "modules",
            "--workspace",
            "--no-stale-check",
            "--format",
            "text",
        ])
        .assert()
        .success();
    let s = String::from_utf8_lossy(&out.get_output().stdout).into_owned();
    assert!(s.contains("── alpha ──") && s.contains("── beta ──"), "{s}");
}

#[test]
fn workspace_with_no_clusters_anywhere_exits_1() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    write_clique(
        &root.join("alpha/src"),
        "alpha",
        &["one", "two", "three"],
        "",
    );
    write(
        &root.join(".vex-workspace.toml"),
        "[[repo]]\npath = \"alpha\"\n",
    );
    vex_in(root)
        .args(["index", "--workspace", "--no-clusters"])
        .assert()
        .success();
    let out = vex_in(root)
        .args([
            "modules",
            "--workspace",
            "--no-stale-check",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let env: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        results(&env)["repos"][0]["empty_reason"],
        "clusters_not_built"
    );
}

fn cohesions(env: &Value) -> Vec<f64> {
    clusters(env)
        .iter()
        .map(|c| c["cohesion"].as_f64().unwrap())
        .collect()
}

#[test]
fn bridge_makes_cohesion_differ_between_clusters() {
    let tmp = TempDir::new().unwrap();
    index(tmp.path());
    let env = modules_json(tmp.path(), &["--min-size", "1"]);
    let by_label = |l: &str| {
        clusters(&env)
            .iter()
            .find(|c| c["label"] == l)
            .unwrap_or_else(|| panic!("no cluster {l}"))
            .clone()
    };
    let net = by_label("src/net/");
    let ui = by_label("src/ui/");
    assert!(
        net["cut_weight"].as_u64().unwrap() > 0,
        "bridge gives net a cut"
    );
    assert!(net["cohesion"].as_f64().unwrap() < 1.0);
    assert_eq!(ui["cut_weight"], 0);
    assert_eq!(ui["cohesion"].as_f64().unwrap(), 1.0);
    let cs = cohesions(&env);
    assert!(
        cs.iter().any(|c| (c - cs[0]).abs() > 1e-9),
        "cohesions differ: {cs:?}"
    );
}

fn write_tie_fixture(dir: &Path) {
    write_clique(&dir.join("src/p"), "p", &["a", "b", "c", "d"], "");
    write_clique(&dir.join("src/q"), "q", &["a", "b", "c", "d"], "");
    // The same name defined once in each clique (two matches, two clusters).
    write(
        &dir.join("src/p/twin.rs"),
        "pub fn twin_fn() { p_a(); p_b(); p_c(); p_d(); }\n",
    );
    write(
        &dir.join("src/q/twin.rs"),
        "pub fn twin_fn() { q_a(); q_b(); q_c(); q_d(); }\n",
    );
}

#[test]
fn equal_clusters_tie_break_by_ascending_id_under_both_sorts() {
    let tmp = TempDir::new().unwrap();
    write_tie_fixture(tmp.path());
    vex_in(tmp.path()).args(["index"]).assert().success();
    for sort in ["size", "cohesion"] {
        let env = modules_json(tmp.path(), &["--min-size", "1", "--sort", sort]);
        let cs = clusters(&env);
        assert_eq!(cs.len(), 2, "{sort}: {cs:?}");
        assert_eq!(cs[0]["size"], cs[1]["size"]);
        assert_eq!(cs[0]["cohesion"], cs[1]["cohesion"], "{sort}: truly tied");
        let ids: Vec<u64> = cs.iter().map(|c| c["id"].as_u64().unwrap()).collect();
        assert!(ids[0] < ids[1], "{sort}: ids ascending, got {ids:?}");
    }
}

#[test]
fn symbol_mode_limit_caps_matches_and_clusters_and_reports_total() {
    let tmp = TempDir::new().unwrap();
    write_tie_fixture(tmp.path());
    vex_in(tmp.path()).args(["index"]).assert().success();
    let env = modules_json(tmp.path(), &["twin_fn"]);
    assert_eq!(results(&env)["symbol"].as_array().unwrap().len(), 2);
    assert_eq!(clusters(&env).len(), 2);
    assert!(results(&env).get("symbols_total").is_none());

    let env = modules_json(tmp.path(), &["twin_fn", "--limit", "1"]);
    assert_eq!(results(&env)["symbol"].as_array().unwrap().len(), 1);
    assert_eq!(clusters(&env).len(), 1);
    assert_eq!(results(&env)["symbols_total"], 2);
}

#[test]
fn limit_zero_is_rejected_by_clap() {
    let tmp = TempDir::new().unwrap();
    index(tmp.path());
    let out = vex_in(tmp.path())
        .args(["modules", "--limit", "0", "--no-stale-check"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("at least 1"));
}

fn write_spanning_fixture(dir: &Path) {
    let names = ["a1", "a2", "a3", "a4", "b1", "b2", "b3", "b4"];
    let mut bodies = [String::new(), String::new()];
    for (i, n) in names.iter().enumerate() {
        let calls: Vec<String> = names
            .iter()
            .filter(|m| *m != n)
            .map(|m| format!("span_{m}();"))
            .collect();
        bodies[i / 4].push_str(&format!("pub fn span_{n}() {{ {} }}\n", calls.join(" ")));
    }
    write(&dir.join("src/span/dir_a/x.rs"), &bodies[0]);
    write(&dir.join("src/span/dir_b/y.rs"), &bodies[1]);
}

#[test]
fn scope_keeps_cluster_spanning_directories_with_in_scope_size_members_and_hubs() {
    let tmp = TempDir::new().unwrap();
    write_spanning_fixture(tmp.path());
    vex_in(tmp.path()).args(["index"]).assert().success();
    let all = modules_json(tmp.path(), &["--min-size", "1", "--members", "100"]);
    assert_eq!(clusters(&all).len(), 1, "one spanning cluster");
    assert_eq!(clusters(&all)[0]["size"], 8);

    for dir in ["dir_a", "dir_b"] {
        let glob = format!("src/span/{dir}/**");
        let env = modules_json(
            tmp.path(),
            &["--min-size", "1", "--members", "100", "--include", &glob],
        );
        let cs = clusters(&env);
        assert_eq!(cs.len(), 1, "{dir}: cluster kept");
        assert_eq!(cs[0]["size"], 4, "{dir}: in-scope count");
        assert_eq!(cs[0]["size_at_build"], 8, "{dir}: full count");
        let members = cs[0]["members"].as_array().unwrap();
        assert_eq!(members.len(), 4);
        assert!(members
            .iter()
            .all(|m| m["path"].as_str().unwrap().contains(dir)));
        // Hubs obey the same scope as members.
        assert!(
            cs[0]["hubs"]
                .as_array()
                .unwrap()
                .iter()
                .all(|h| h["path"].as_str().unwrap().contains(dir)),
            "{dir}: hubs {:?}",
            cs[0]["hubs"]
        );
    }
}

#[test]
fn list_output_with_members_is_deterministic_across_runs() {
    let tmp = TempDir::new().unwrap();
    index(tmp.path());
    let run = || {
        vex_in(tmp.path())
            .args([
                "modules",
                "--members",
                "100",
                "--no-stale-check",
                "--format",
                "text",
            ])
            .output()
            .unwrap()
            .stdout
    };
    let first = run();
    assert!(!first.is_empty());
    assert_eq!(first, run());
}
