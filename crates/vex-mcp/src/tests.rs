use super::*;
use serde_json::json;

fn args_for(tool: &str, args: Value) -> Vec<String> {
    build_command(tool, &args, "/tmp/proj")
        .expect("build_command")
        .extra_args
}

#[test]
fn callers_default_pushes_auto_update_flag() {
    // Most common path: MCP client omits the field. The auto_update()
    // helper defaults to true, so --auto-update must appear in the
    // spawned CLI args.
    let extra = args_for("callers", json!({"name": "Foo"}));
    assert!(
        extra.iter().any(|a| a == "--auto-update"),
        "expected --auto-update flag in callers args, got: {extra:?}"
    );
}

#[test]
fn callers_explicit_false_omits_auto_update_flag() {
    let extra = args_for("callers", json!({"name": "Foo", "auto_update": false}));
    assert!(
        !extra.iter().any(|a| a == "--auto-update"),
        "callers with auto_update=false must not pass --auto-update, got: {extra:?}"
    );
}

#[test]
fn callees_default_pushes_auto_update_flag() {
    let extra = args_for("callees", json!({"name": "Bar"}));
    assert!(
        extra.iter().any(|a| a == "--auto-update"),
        "expected --auto-update flag in callees args, got: {extra:?}"
    );
}

#[test]
fn callees_explicit_false_omits_auto_update_flag() {
    let extra = args_for("callees", json!({"name": "Bar", "auto_update": false}));
    assert!(
        !extra.iter().any(|a| a == "--auto-update"),
        "callees with auto_update=false must not pass --auto-update, got: {extra:?}"
    );
}

#[test]
fn callers_and_callees_schemas_expose_auto_update() {
    // Schema-regression guard: removing the field would silently break
    // MCP clients that pass `auto_update` and expect it to be honored.
    let desc = tool_descriptors();
    let tools = desc.as_array().expect("tool_descriptors returns array");

    for name in ["callers", "callees"] {
        let entry = tools
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("missing tool {name}"));
        let props = &entry["inputSchema"]["properties"];
        assert!(
            props["auto_update"].is_object(),
            "{name} schema is missing the auto_update field: {props}"
        );
    }
}

#[test]
fn search_scope_globs_become_repeated_cli_flags() {
    let extra = args_for(
        "search",
        json!({
            "query": "Foo",
            "include": ["tests/**", "crates/**"],
            "exclude": ["**/generated/**"],
        }),
    );
    // Each glob round-trips as a separate `--include`/`--exclude` pair so
    // clap's `Vec<String>` accumulator on the CLI side sees one value per
    // flag (the standard repeatable-arg shape).
    let pairs: Vec<&str> = extra.iter().map(String::as_str).collect();
    let window = |flag: &str, val: &str| pairs.windows(2).any(|w| w[0] == flag && w[1] == val);
    assert!(
        window("--include", "tests/**"),
        "missing --include tests/**: {pairs:?}"
    );
    assert!(
        window("--include", "crates/**"),
        "missing --include crates/**: {pairs:?}"
    );
    assert!(
        window("--exclude", "**/generated/**"),
        "missing --exclude **/generated/**: {pairs:?}"
    );
}

#[test]
fn canonical_symbol_arg_works_on_renamed_tools() {
    // The v1.7 rename: `name` → `symbol` on the six call-graph /
    // resolution tools. Sending `symbol` is the new canonical form
    // and must produce a non-empty argv with no deprecation flag.
    for tool in [
        "find_symbol",
        "usages",
        "implementations",
        "callers",
        "callees",
        "similar",
    ] {
        let args = json!({"symbol": "Foo"});
        let built =
            build_command(tool, &args, "/tmp/proj").unwrap_or_else(|e| panic!("{tool}: {e}"));
        assert!(
            built.extra_args.iter().any(|a| a == "Foo"),
            "{tool}: expected symbol value in argv, got: {:?}",
            built.extra_args
        );
        assert!(
            built.deprecated_args.is_empty(),
            "{tool}: canonical arg should not flag deprecation, got: {:?}",
            built.deprecated_args
        );
    }
}

#[test]
fn legacy_name_arg_still_accepted_with_deprecation_notice() {
    // Pre-v1.7 clients sending `name` continue to work but get a
    // deprecation marker via _meta.deprecated_args so they can
    // migrate. Same coverage as the canonical test above.
    for tool in [
        "find_symbol",
        "usages",
        "implementations",
        "callers",
        "callees",
        "similar",
    ] {
        let args = json!({"name": "Foo"});
        let built =
            build_command(tool, &args, "/tmp/proj").unwrap_or_else(|e| panic!("{tool}: {e}"));
        assert!(
            built.extra_args.iter().any(|a| a == "Foo"),
            "{tool}: expected legacy `name` arg to surface as argv value"
        );
        assert_eq!(
            built.deprecated_args,
            vec!["name".to_string()],
            "{tool}: legacy `name` arg should emit deprecation marker, got: {:?}",
            built.deprecated_args
        );
    }
}

#[test]
fn outline_legacy_file_arg_is_deprecated() {
    let canon = build_command("outline", &json!({"path": "src/foo.rs"}), "/tmp/proj").unwrap();
    assert!(canon.deprecated_args.is_empty());
    assert!(canon.extra_args.iter().any(|a| a == "src/foo.rs"));

    let legacy = build_command("outline", &json!({"file": "src/foo.rs"}), "/tmp/proj").unwrap();
    assert_eq!(legacy.deprecated_args, vec!["file".to_string()]);
    assert!(legacy.extra_args.iter().any(|a| a == "src/foo.rs"));
}

#[test]
fn check_legacy_names_arg_is_deprecated() {
    let canon = build_command("check", &json!({"symbols": ["Foo", "Bar"]}), "/tmp/proj").unwrap();
    assert!(canon.deprecated_args.is_empty());
    assert!(canon.extra_args.iter().any(|a| a == "Foo"));

    let legacy = build_command("check", &json!({"names": ["Foo", "Bar"]}), "/tmp/proj").unwrap();
    assert_eq!(legacy.deprecated_args, vec!["names".to_string()]);
    assert!(legacy.extra_args.iter().any(|a| a == "Foo"));
}

#[test]
fn show_legacy_singular_symbol_arg_is_deprecated() {
    let canon = build_command("show", &json!({"symbols": ["Foo"]}), "/tmp/proj").unwrap();
    assert!(canon.deprecated_args.is_empty());

    let legacy = build_command("show", &json!({"symbol": "Foo"}), "/tmp/proj").unwrap();
    assert_eq!(legacy.deprecated_args, vec!["symbol".to_string()]);
    assert!(legacy.extra_args.iter().any(|a| a == "Foo"));
}

#[test]
fn filter_path_canonical_and_legacy_filter_alias_both_spawn_filter_flag() {
    // PROTOCOL-EVOLUTION §3.3 — `filter_path` is canonical; `filter` is a
    // back-compat alias. Both resolve to the established `--filter` CLI flag
    // (mixed-version safe); only the legacy name flags a deprecation marker.
    // (tool, required primary field for that tool)
    let cases: [(&str, Value); 4] = [
        ("search", json!({"query": "foo"})),
        ("usages", json!({"symbol": "Foo"})),
        ("grep", json!({"pattern": "foo"})),
        ("duplicates", json!({})),
    ];
    for (tool, base) in cases {
        let mut canon_args = base.clone();
        canon_args["filter_path"] = json!("src/api/");
        let canon = build_command(tool, &canon_args, "/tmp/proj")
            .unwrap_or_else(|e| panic!("{tool}: canonical filter_path must build: {e}"));
        let fi = canon
            .extra_args
            .iter()
            .position(|a| a == "--filter")
            .unwrap_or_else(|| {
                panic!(
                    "{tool}: expected --filter in argv, got: {:?}",
                    canon.extra_args
                )
            });
        assert_eq!(
            canon.extra_args.get(fi + 1).map(String::as_str),
            Some("src/api/"),
            "{tool}: --filter value must follow the flag"
        );
        assert!(
            canon.deprecated_args.is_empty(),
            "{tool}: canonical filter_path must not flag deprecation, got: {:?}",
            canon.deprecated_args
        );

        let mut legacy_args = base.clone();
        legacy_args["filter"] = json!("src/api/");
        let legacy = build_command(tool, &legacy_args, "/tmp/proj")
            .unwrap_or_else(|e| panic!("{tool}: legacy filter must build: {e}"));
        assert!(
            legacy.extra_args.iter().any(|a| a == "src/api/"),
            "{tool}: legacy filter value must reach argv"
        );
        assert_eq!(
            legacy.deprecated_args,
            vec!["filter".to_string()],
            "{tool}: legacy `filter` must emit a deprecation marker, got: {:?}",
            legacy.deprecated_args
        );
    }
}

#[test]
fn search_why_flag_is_pushed() {
    let extra = args_for("search", json!({"query": "Foo", "why": true}));
    assert!(
        extra.iter().any(|a| a == "--why"),
        "expected --why in argv when why:true, got: {extra:?}"
    );

    let extra = args_for("search", json!({"query": "Foo"}));
    assert!(
        !extra.iter().any(|a| a == "--why"),
        "expected no --why without the flag, got: {extra:?}"
    );
}

#[test]
fn search_shaped_tools_expose_scope_in_schema() {
    // Schema-regression guard: every search-shaped tool must surface
    // include/exclude so MCP clients can discover the filter via
    // `tools/list`.
    let desc = tool_descriptors();
    let tools = desc.as_array().expect("tool_descriptors returns array");

    for name in [
        "search",
        "find_symbol",
        "find_similar",
        "show",
        "usages",
        "grep",
        "implementations",
        "callers",
        "callees",
        "similar",
        "duplicates",
    ] {
        let entry = tools
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("missing tool {name}"));
        let props = &entry["inputSchema"]["properties"];
        assert!(
            props["include"].is_object(),
            "{name} schema is missing include: {props}"
        );
        assert!(
            props["exclude"].is_object(),
            "{name} schema is missing exclude: {props}"
        );
    }
}

// ── 11.4 Inc 8: vex_pattern tool surface ──────────────────────────────

#[test]
fn pattern_canonical_args_become_cli_flags() {
    let extra = args_for(
        "pattern",
        json!({
            "pattern": "fn $NAME($$$) -> Result",
            "lang": "rust",
            "limit": 25,
        }),
    );
    assert_eq!(
        extra[0], "fn $NAME($$$) -> Result",
        "pattern is first positional arg"
    );
    assert!(extra.iter().any(|a| a == "--lang"));
    assert!(extra.iter().any(|a| a == "rust"));
    assert!(extra.iter().any(|a| a == "--limit"));
    assert!(extra.iter().any(|a| a == "25"));
}

#[test]
fn pattern_composition_string_passes_through_verbatim() {
    // The CLI parses `&&` / `||` at parse time — MCP just forwards.
    let extra = args_for(
        "pattern",
        json!({
            "pattern": "struct $S && impl $S",
            "lang": "rust",
        }),
    );
    assert_eq!(
        extra[0], "struct $S && impl $S",
        "composition operators must not be mangled by MCP"
    );
}

#[test]
fn pattern_why_true_appends_why_flag() {
    let extra = args_for(
        "pattern",
        json!({
            "pattern": "fn $N()",
            "lang": "rust",
            "why": true,
        }),
    );
    assert!(
        extra.iter().any(|a| a == "--why"),
        "why=true must add --why to the spawned CLI args; got: {extra:?}"
    );
}

#[test]
fn pattern_why_false_omits_why_flag() {
    let extra = args_for(
        "pattern",
        json!({"pattern": "fn $N()", "lang": "rust", "why": false}),
    );
    assert!(
        !extra.iter().any(|a| a == "--why"),
        "why=false must not pass --why; got: {extra:?}"
    );
}

#[test]
fn pattern_why_default_omits_why_flag() {
    let extra = args_for("pattern", json!({"pattern": "fn $N()", "lang": "rust"}));
    assert!(
        !extra.iter().any(|a| a == "--why"),
        "missing why arg must not pass --why; got: {extra:?}"
    );
}

#[test]
fn pattern_schema_exposes_why_and_scope() {
    let desc = tool_descriptors();
    let tools = desc.as_array().expect("tool_descriptors returns array");
    let entry = tools
        .iter()
        .find(|t| t["name"] == "pattern")
        .expect("missing pattern tool descriptor");
    let props = &entry["inputSchema"]["properties"];
    assert!(props["why"].is_object(), "pattern schema must expose `why`");
    assert!(props["include"].is_object());
    assert!(props["exclude"].is_object());
    // Canonical naming (anticipating 11.10): pattern / lang / project_root.
    assert!(props["pattern"].is_object());
    assert!(props["lang"].is_object());
    assert!(props["project_root"].is_object());
}

// ── 11.10: --why on usages / similar / duplicates ─────────────────────

#[test]
fn usages_why_true_appends_why_flag() {
    let extra = args_for("usages", json!({"symbol": "Foo", "why": true}));
    assert!(
        extra.iter().any(|a| a == "--why"),
        "usages why=true must add --why; got: {extra:?}"
    );
}

#[test]
fn usages_why_default_omits_why_flag() {
    let extra = args_for("usages", json!({"symbol": "Foo"}));
    assert!(
        !extra.iter().any(|a| a == "--why"),
        "usages without why must not pass --why; got: {extra:?}"
    );
}

#[test]
fn similar_why_true_appends_why_flag() {
    let extra = args_for("similar", json!({"symbol": "Foo", "why": true}));
    assert!(
        extra.iter().any(|a| a == "--why"),
        "similar why=true must add --why; got: {extra:?}"
    );
}

#[test]
fn similar_why_default_omits_why_flag() {
    let extra = args_for("similar", json!({"symbol": "Foo"}));
    assert!(
        !extra.iter().any(|a| a == "--why"),
        "similar without why must not pass --why; got: {extra:?}"
    );
}

#[test]
fn duplicates_why_true_appends_why_flag() {
    let extra = args_for("duplicates", json!({"why": true}));
    assert!(
        extra.iter().any(|a| a == "--why"),
        "duplicates why=true must add --why; got: {extra:?}"
    );
}

#[test]
fn duplicates_why_default_omits_why_flag() {
    let extra = args_for("duplicates", json!({}));
    assert!(
        !extra.iter().any(|a| a == "--why"),
        "duplicates without why must not pass --why; got: {extra:?}"
    );
}

#[test]
fn usages_similar_duplicates_schemas_expose_why() {
    // Schema regression guard: every tool that supports --why
    // must surface it via tools/list so MCP clients discover the
    // capability without scraping docs.
    let desc = tool_descriptors();
    let tools = desc.as_array().expect("tool_descriptors returns array");
    for name in ["usages", "similar", "duplicates"] {
        let entry = tools
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("missing tool {name}"));
        let props = &entry["inputSchema"]["properties"];
        assert!(
            props["why"].is_object(),
            "{name} schema must expose `why`: {props}"
        );
        assert_eq!(
            props["why"]["type"].as_str(),
            Some("boolean"),
            "{name} `why` must be boolean-typed"
        );
    }
}

// ── Phase 13: capabilities tool + envelope shape ───────────────────────

#[test]
fn capabilities_tool_is_in_tool_descriptors() {
    // Schema regression guard: `capabilities` must appear in tool_descriptors()
    // so MCP clients can discover it via tools/list. Will fail until Stage 3
    // adds the entry to the array.
    let desc = tool_descriptors();
    let tools = desc.as_array().expect("tool_descriptors returns array");
    let found = tools.iter().any(|t| t["name"] == "capabilities");
    assert!(
        found,
        "tool_descriptors() must include a 'capabilities' entry for Phase 13.0; got: {desc}"
    );
}

// ── 13.1 tool description snapshot ────────────────────────────────────
//
// Locks the LLM-facing `description` and per-parameter `description`
// fields exposed via `tools/list`. A future schema-gen refactor that
// silently regresses these strings (back to the pre-13.1 human-prose
// wording) trips this snapshot.
//
// To accept an intentional reword: `cargo insta accept` after running
// `cargo test -p vex-mcp tool_descriptors_snapshot`.

#[test]
fn tool_descriptors_snapshot() {
    let descriptors = tool_descriptors();
    insta::assert_json_snapshot!("tool_descriptors", descriptors);
}

// ---------------------------------------------------------------------
// Phase 13.2 — `bundle` MCP tool (Inc 5)
// ---------------------------------------------------------------------

#[test]
fn args_for_bundle_symbol() {
    let extra = args_for(
        "bundle",
        json!({
            "mode": "symbol",
            "symbol": "MyFunc",
            "callers_max": 7,
        }),
    );
    // Mode flag is always emitted first; symbol-specific args follow.
    assert!(extra
        .windows(2)
        .any(|w| w[0] == "--mode" && w[1] == "symbol"));
    assert!(extra
        .windows(2)
        .any(|w| w[0] == "--symbol" && w[1] == "MyFunc"));
    assert!(extra
        .windows(2)
        .any(|w| w[0] == "--callers-max" && w[1] == "7"));
}

#[test]
fn args_for_bundle_pr_impact_requires_base() {
    // Missing `base` is a server-side validation failure (architect-
    // review A4: per-mode required-field validation lives here, not
    // in JSON-Schema `oneOf`).
    let err = build_command("bundle", &json!({"mode": "pr-impact"}), "/tmp/proj")
        .expect_err("pr-impact without base must error");
    let msg = format!("{err:#}");
    assert!(
        msg.to_lowercase().contains("base"),
        "error should mention the missing `base` field; got: {msg}"
    );
}

#[test]
fn args_for_bundle_pr_impact_passes_base_and_depth() {
    let extra = args_for(
        "bundle",
        json!({
            "mode": "pr-impact",
            "base": "origin/main",
            "depth": 3,
        }),
    );
    assert!(extra
        .windows(2)
        .any(|w| w[0] == "--mode" && w[1] == "pr-impact"));
    assert!(extra
        .windows(2)
        .any(|w| w[0] == "--base" && w[1] == "origin/main"));
    assert!(extra.windows(2).any(|w| w[0] == "--depth" && w[1] == "3"));
}

#[test]
fn args_for_bundle_project_default_top_n() {
    // `top_n` omitted → no `--top-n` flag is appended; the CLI uses
    // its own default (30) via clap. Tests that the MCP layer
    // doesn't force a default and override clap.
    let extra = args_for("bundle", json!({"mode": "project"}));
    assert!(extra
        .windows(2)
        .any(|w| w[0] == "--mode" && w[1] == "project"));
    assert!(
        !extra.iter().any(|a| a == "--top-n"),
        "project without top_n must not push --top-n; got: {extra:?}"
    );
}

#[test]
fn args_for_bundle_project_explicit_top_n_and_glob() {
    let extra = args_for(
        "bundle",
        json!({
            "mode": "project",
            "top_n": 5,
            "path_glob": "src/**",
        }),
    );
    assert!(extra.windows(2).any(|w| w[0] == "--top-n" && w[1] == "5"));
    assert!(extra
        .windows(2)
        .any(|w| w[0] == "--path-glob" && w[1] == "src/**"));
}

#[test]
fn args_for_bundle_symbol_missing_symbol_errors() {
    let err = build_command("bundle", &json!({"mode": "symbol"}), "/tmp/proj")
        .expect_err("symbol mode without `symbol` must error");
    let msg = format!("{err:#}");
    assert!(
        msg.to_lowercase().contains("symbol"),
        "error should mention the missing `symbol` field; got: {msg}"
    );
}

#[test]
fn args_for_bundle_unknown_mode_errors() {
    let err = build_command("bundle", &json!({"mode": "not-a-real-mode"}), "/tmp/proj")
        .expect_err("unknown bundle mode must error");
    let msg = format!("{err:#}");
    assert!(
        msg.to_lowercase().contains("not-a-real-mode"),
        "error should mention the offending mode value; got: {msg}"
    );
}

#[test]
fn args_for_bundle_missing_mode_errors() {
    let err = build_command("bundle", &json!({"symbol": "Foo"}), "/tmp/proj")
        .expect_err("bundle without `mode` must error");
    let msg = format!("{err:#}");
    assert!(
        msg.to_lowercase().contains("mode"),
        "error should mention the missing `mode` field; got: {msg}"
    );
}

// ── HIGH-tier CLI ↔ MCP parity gap closure ────────────────────────────

// search: filter / kind / context_path / no_bm25

#[test]
fn search_filter_arg_pushes_filter_flag() {
    let extra = args_for("search", json!({"query": "Foo", "filter": "src/api/"}));
    assert!(
        extra
            .windows(2)
            .any(|w| w[0] == "--filter" && w[1] == "src/api/"),
        "search filter must surface as --filter; got: {extra:?}"
    );
}

#[test]
fn search_kind_array_becomes_repeated_kind_flags() {
    let extra = args_for(
        "search",
        json!({"query": "Foo", "kind": ["fn", "method", "struct"]}),
    );
    let kinds: Vec<&str> = extra
        .windows(2)
        .filter_map(|w| (w[0] == "--kind").then_some(w[1].as_str()))
        .collect();
    assert_eq!(
        kinds,
        vec!["fn", "method", "struct"],
        "search kind must emit one --kind pair per element; got: {extra:?}"
    );
}

#[test]
fn search_context_path_pushes_context_path_flag() {
    let extra = args_for(
        "search",
        json!({"query": "Foo", "context_path": "src/main.rs"}),
    );
    assert!(
        extra
            .windows(2)
            .any(|w| w[0] == "--context-path" && w[1] == "src/main.rs"),
        "search context_path must surface as --context-path; got: {extra:?}"
    );
}

#[test]
fn search_no_bm25_true_pushes_flag() {
    let extra = args_for("search", json!({"query": "Foo", "no_bm25": true}));
    assert!(
        extra.iter().any(|a| a == "--no-bm25"),
        "search no_bm25:true must add --no-bm25; got: {extra:?}"
    );
}

#[test]
fn search_no_bm25_default_omits_flag() {
    let extra = args_for("search", json!({"query": "Foo"}));
    assert!(
        !extra.iter().any(|a| a == "--no-bm25"),
        "search without no_bm25 must not pass --no-bm25; got: {extra:?}"
    );
}

// show: filter / kind / context_path / signature_only / head / no_body / collapsed

#[test]
fn show_filter_arg_pushes_filter_flag() {
    let extra = args_for("show", json!({"symbols": ["Foo"], "filter": "tests/"}));
    assert!(
        extra
            .windows(2)
            .any(|w| w[0] == "--filter" && w[1] == "tests/"),
        "show filter must surface as --filter; got: {extra:?}"
    );
}

#[test]
fn show_kind_array_becomes_repeated_kind_flags() {
    let extra = args_for(
        "show",
        json!({"symbols": ["Foo"], "kind": ["fn", "method"]}),
    );
    let kinds: Vec<&str> = extra
        .windows(2)
        .filter_map(|w| (w[0] == "--kind").then_some(w[1].as_str()))
        .collect();
    assert_eq!(
        kinds,
        vec!["fn", "method"],
        "show kind must emit one --kind pair per element; got: {extra:?}"
    );
}

#[test]
fn show_context_path_pushes_context_path_flag() {
    let extra = args_for(
        "show",
        json!({"symbols": ["Foo"], "context_path": "src/main.rs"}),
    );
    assert!(
        extra
            .windows(2)
            .any(|w| w[0] == "--context-path" && w[1] == "src/main.rs"),
        "show context_path must surface as --context-path; got: {extra:?}"
    );
}

#[test]
fn show_signature_only_pushes_flag() {
    let extra = args_for("show", json!({"symbols": ["Foo"], "signature_only": true}));
    assert!(
        extra.iter().any(|a| a == "--signature-only"),
        "show signature_only must add --signature-only; got: {extra:?}"
    );
}

#[test]
fn show_head_pushes_head_with_value() {
    let extra = args_for("show", json!({"symbols": ["Foo"], "head": 5}));
    assert!(
        extra.windows(2).any(|w| w[0] == "--head" && w[1] == "5"),
        "show head must surface as --head <N>; got: {extra:?}"
    );
}

#[test]
fn show_no_body_pushes_flag() {
    let extra = args_for("show", json!({"symbols": ["Foo"], "no_body": true}));
    assert!(
        extra.iter().any(|a| a == "--no-body"),
        "show no_body must add --no-body; got: {extra:?}"
    );
}

#[test]
fn show_collapsed_pushes_flag() {
    let extra = args_for("show", json!({"symbols": ["Foo"], "collapsed": true}));
    assert!(
        extra.iter().any(|a| a == "--collapsed"),
        "show collapsed must add --collapsed; got: {extra:?}"
    );
}

#[test]
fn show_truncation_flags_are_mutually_exclusive() {
    let err = build_command(
        "show",
        &json!({"symbols": ["Foo"], "signature_only": true, "no_body": true}),
        "/tmp/proj",
    )
    .expect_err("mutually exclusive show truncation flags must error");
    let msg = format!("{err:#}").to_lowercase();
    assert!(
        msg.contains("mutually exclusive"),
        "error should call out the mutual exclusion; got: {msg}"
    );
}

#[test]
fn show_no_truncation_flags_yields_clean_argv() {
    let extra = args_for("show", json!({"symbols": ["Foo"]}));
    for flag in ["--signature-only", "--head", "--no-body", "--collapsed"] {
        assert!(
            !extra.iter().any(|a| a == flag),
            "no truncation flag requested but {flag} present; got: {extra:?}"
        );
    }
}

// usages: filter

#[test]
fn usages_filter_arg_pushes_filter_flag() {
    let extra = args_for("usages", json!({"symbol": "Foo", "filter": "src/"}));
    assert!(
        extra
            .windows(2)
            .any(|w| w[0] == "--filter" && w[1] == "src/"),
        "usages filter must surface as --filter; got: {extra:?}"
    );
}

// v1.20.0 D5: tests_for + history MCP tools

#[test]
fn tests_for_target_pushes_positional_argv() {
    let extra = args_for("tests_for", json!({"target": "Foo"}));
    assert_eq!(
        extra.first().map(String::as_str),
        Some("Foo"),
        "tests_for must surface target as the first positional argv; got: {extra:?}"
    );
}

#[test]
fn tests_for_legacy_symbol_alias_accepted() {
    // Pre-D5 there was no MCP tool, but client code that
    // hallucinated `symbol` (consistent with every other tool)
    // should still resolve.
    let extra = args_for("tests_for", json!({"symbol": "Foo"}));
    assert_eq!(
        extra.first().map(String::as_str),
        Some("Foo"),
        "tests_for must accept legacy `symbol` alias; got: {extra:?}"
    );
}

#[test]
fn tests_for_include_fixtures_pushes_flag() {
    let extra = args_for(
        "tests_for",
        json!({"target": "Foo", "include_fixtures": true}),
    );
    assert!(
        extra.iter().any(|a| a == "--include-fixtures"),
        "tests_for include_fixtures=true must add --include-fixtures; got: {extra:?}"
    );
}

#[test]
fn tests_for_test_pattern_repeats() {
    let extra = args_for(
        "tests_for",
        json!({"target": "Foo", "test_pattern": ["tests/**", "spec/**"]}),
    );
    let pattern_pairs: Vec<_> = extra
        .windows(2)
        .filter(|w| w[0] == "--test-pattern")
        .collect();
    assert_eq!(
        pattern_pairs.len(),
        2,
        "two test_pattern entries must yield two --test-pattern flags; got: {extra:?}"
    );
}

#[test]
fn tests_for_descriptor_present_in_tools_list() {
    let desc = tool_descriptors();
    let tools = desc.as_array().expect("tool_descriptors must be array");
    assert!(
        tools.iter().any(|t| t["name"] == "tests_for"),
        "tool_descriptors() must include a 'tests_for' entry for v1.20.0 D5; got: {desc}"
    );
}

#[test]
fn history_symbol_pushes_positional_argv() {
    let extra = args_for("history", json!({"symbol": "Foo"}));
    assert_eq!(
        extra.first().map(String::as_str),
        Some("Foo"),
        "history must surface symbol as the first positional argv; got: {extra:?}"
    );
}

#[test]
fn history_diff_flag_propagates() {
    let extra = args_for("history", json!({"symbol": "Foo", "diff": true}));
    assert!(
        extra.iter().any(|a| a == "--diff"),
        "history diff=true must add --diff; got: {extra:?}"
    );
}

#[test]
fn history_since_until_author_propagate() {
    let extra = args_for(
        "history",
        json!({"symbol": "Foo", "since": "2026-01-01", "until": "2026-06-22", "author": "alice"}),
    );
    for (canonical, value) in [
        ("--since", "2026-01-01"),
        ("--until", "2026-06-22"),
        ("--author", "alice"),
    ] {
        assert!(
            extra.windows(2).any(|w| w[0] == canonical && w[1] == value),
            "history must surface {canonical} {value}; got: {extra:?}"
        );
    }
}

#[test]
fn history_diff_and_exact_presence_together_is_invalid_params() {
    // The CLI rejects this combination at the clap layer (the diff
    // path groups by `(symbol, kind)` which breaks per-row presence
    // mapping). The MCP wrapper rejects it earlier so clients see
    // `-32602 Invalid params` with the canonical recovery shape,
    // not an opaque downstream exit code.
    let err = build_command(
        "history",
        &json!({"symbol": "Foo", "diff": true, "exact_presence": true}),
        "/tmp/proj",
    )
    .expect_err("diff + exact_presence must surface as invalid params");
    let msg = format!("{err}");
    assert!(
        msg.contains("mutually exclusive"),
        "error must mention mutual exclusion; got: {msg}"
    );
}

#[test]
fn history_no_index_pushes_flag() {
    let extra = args_for("history", json!({"symbol": "Foo", "no_index": true}));
    assert!(
        extra.iter().any(|a| a == "--no-index"),
        "history no_index=true must add --no-index; got: {extra:?}"
    );
}

#[test]
fn history_descriptor_present_in_tools_list() {
    let desc = tool_descriptors();
    let tools = desc.as_array().expect("tool_descriptors must be array");
    assert!(
        tools.iter().any(|t| t["name"] == "history"),
        "tool_descriptors() must include a 'history' entry for v1.20.0 D5; got: {desc}"
    );
}

// v1.20.0 D4: search code_only opt-in

#[test]
fn search_code_only_pushes_flag() {
    let extra = args_for("search", json!({"query": "foo", "code_only": true}));
    assert!(
        extra.iter().any(|a| a == "--code-only"),
        "search code_only=true must add --code-only; got: {extra:?}"
    );
}

#[test]
fn search_exclude_generated_pushes_flag() {
    let extra = args_for("search", json!({"query": "foo", "exclude_generated": true}));
    assert!(
        extra.contains(&"--exclude-generated".to_string()),
        "search exclude_generated=true must add --exclude-generated; got: {extra:?}"
    );
}

#[test]
fn search_exclude_generated_default_omits_flag() {
    let extra = args_for("search", json!({"query": "foo"}));
    assert!(
        !extra.contains(&"--exclude-generated".to_string()),
        "search without exclude_generated must NOT add --exclude-generated; got: {extra:?}"
    );
}

#[test]
fn search_code_only_default_omits_flag() {
    let extra = args_for("search", json!({"query": "foo"}));
    assert!(
        !extra.iter().any(|a| a == "--code-only"),
        "search without code_only must NOT add --code-only; got: {extra:?}"
    );
}

// v1.20.0 F1: impact (delete-safety blast radius)

#[test]
fn impact_uses_symbol_as_positional_argv() {
    let extra = args_for("impact", json!({"symbol": "Foo"}));
    assert_eq!(
        extra.first().map(String::as_str),
        Some("Foo"),
        "impact must surface symbol as the first positional argv; got: {extra:?}"
    );
}

#[test]
fn impact_legacy_name_alias_accepted_as_symbol() {
    // Pre-v1.7 callers used `name`; the canonical reader treats
    // it as a deprecated alias for `symbol` (consistent with
    // every other symbol-keyed tool).
    let extra = args_for("impact", json!({"name": "Foo"}));
    assert_eq!(
        extra.first().map(String::as_str),
        Some("Foo"),
        "impact must accept legacy `name` alias; got: {extra:?}"
    );
}

#[test]
fn impact_passes_scope_flags_through() {
    let extra = args_for(
        "impact",
        json!({"symbol": "Foo", "include": ["src/**"], "exclude": ["tests/**"]}),
    );
    assert!(
        extra
            .windows(2)
            .any(|w| w[0] == "--include" && w[1] == "src/**"),
        "impact include must surface as --include <glob>; got: {extra:?}"
    );
    assert!(
        extra
            .windows(2)
            .any(|w| w[0] == "--exclude" && w[1] == "tests/**"),
        "impact exclude must surface as --exclude <glob>; got: {extra:?}"
    );
}

#[test]
fn impact_descriptor_present_in_tools_list() {
    let desc = tool_descriptors();
    let tools = desc.as_array().expect("tool_descriptors must be array");
    let found = tools.iter().any(|t| t["name"] == "impact");
    assert!(
        found,
        "tool_descriptors() must include an 'impact' entry for v1.20.0 F1; got: {desc}"
    );
}

// v1.20.0 D2: include_self / include_docs opt-in flags

#[test]
fn usages_include_self_pushes_flag() {
    let extra = args_for("usages", json!({"symbol": "Foo", "include_self": true}));
    assert!(
        extra.iter().any(|a| a == "--include-self"),
        "usages include_self=true must add --include-self; got: {extra:?}"
    );
}

#[test]
fn usages_include_self_default_omits_flag() {
    let extra = args_for("usages", json!({"symbol": "Foo"}));
    assert!(
        !extra.iter().any(|a| a == "--include-self"),
        "usages without include_self must NOT add --include-self; got: {extra:?}"
    );
}

#[test]
fn usages_include_docs_pushes_flag() {
    let extra = args_for("usages", json!({"symbol": "Foo", "include_docs": true}));
    assert!(
        extra.iter().any(|a| a == "--include-docs"),
        "usages include_docs=true must add --include-docs; got: {extra:?}"
    );
}

#[test]
fn usages_include_docs_default_omits_flag() {
    let extra = args_for("usages", json!({"symbol": "Foo"}));
    assert!(
        !extra.iter().any(|a| a == "--include-docs"),
        "usages without include_docs must NOT add --include-docs; got: {extra:?}"
    );
}

// pattern / similar / duplicates: diff scope

#[test]
fn pattern_since_pushes_since_with_value() {
    let extra = args_for(
        "pattern",
        json!({"pattern": "fn $N()", "lang": "rust", "since": "main"}),
    );
    assert!(
        extra
            .windows(2)
            .any(|w| w[0] == "--since" && w[1] == "main"),
        "pattern since must surface as --since <rev>; got: {extra:?}"
    );
}

#[test]
fn pattern_since_branched_pushes_flag() {
    let extra = args_for(
        "pattern",
        json!({"pattern": "fn $N()", "lang": "rust", "since_branched": true}),
    );
    assert!(
        extra.iter().any(|a| a == "--since-branched"),
        "pattern since_branched must add --since-branched; got: {extra:?}"
    );
}

#[test]
fn pattern_changed_only_pushes_flag() {
    let extra = args_for(
        "pattern",
        json!({"pattern": "fn $N()", "lang": "rust", "changed_only": true}),
    );
    assert!(
        extra.iter().any(|a| a == "--changed-only"),
        "pattern changed_only must add --changed-only; got: {extra:?}"
    );
}

#[test]
fn pattern_diff_scope_flags_mutually_exclusive() {
    let err = build_command(
        "pattern",
        &json!({
            "pattern": "fn $N()",
            "lang": "rust",
            "since": "main",
            "since_branched": true,
        }),
        "/tmp/proj",
    )
    .expect_err("mutually exclusive diff-scope flags must error");
    let msg = format!("{err:#}").to_lowercase();
    assert!(
        msg.contains("mutually exclusive"),
        "error should call out the mutual exclusion; got: {msg}"
    );
}

#[test]
fn similar_since_pushes_since_with_value() {
    let extra = args_for("similar", json!({"symbol": "Foo", "since": "HEAD~3"}));
    assert!(
        extra
            .windows(2)
            .any(|w| w[0] == "--since" && w[1] == "HEAD~3"),
        "similar since must surface as --since <rev>; got: {extra:?}"
    );
}

#[test]
fn similar_changed_only_pushes_flag() {
    let extra = args_for("similar", json!({"symbol": "Foo", "changed_only": true}));
    assert!(
        extra.iter().any(|a| a == "--changed-only"),
        "similar changed_only must add --changed-only; got: {extra:?}"
    );
}

#[test]
fn duplicates_since_branched_pushes_flag() {
    let extra = args_for("duplicates", json!({"since_branched": true}));
    assert!(
        extra.iter().any(|a| a == "--since-branched"),
        "duplicates since_branched must add --since-branched; got: {extra:?}"
    );
}

// no_stale_check — applied to every tool that already accepted auto_update.

#[test]
fn no_stale_check_default_omits_flag_across_tools() {
    // Default-off — no MCP client should see surprise behaviour.
    let cases: Vec<(&str, Value)> = vec![
        ("search", json!({"query": "Foo"})),
        ("find_symbol", json!({"symbol": "Foo"})),
        ("find_similar", json!({"query": "Foo"})),
        ("show", json!({"symbols": ["Foo"]})),
        ("usages", json!({"symbol": "Foo"})),
        ("implementations", json!({"symbol": "Foo"})),
        ("callers", json!({"symbol": "Foo"})),
        ("callees", json!({"symbol": "Foo"})),
        ("paths", json!({"from": "A", "to": "B"})),
        ("reachable", json!({"target": "Foo"})),
        ("check", json!({"symbols": ["Foo"]})),
        ("similar", json!({"symbol": "Foo"})),
        ("duplicates", json!({})),
        ("bundle", json!({"mode": "project"})),
    ];
    for (tool, args) in cases {
        let extra = args_for(tool, args);
        assert!(
            !extra.iter().any(|a| a == "--no-stale-check"),
            "{tool} default must not pass --no-stale-check; got: {extra:?}"
        );
    }
}

#[test]
fn no_stale_check_true_pushes_flag_across_tools() {
    let cases: Vec<(&str, Value)> = vec![
        ("search", json!({"query": "Foo", "no_stale_check": true})),
        (
            "find_symbol",
            json!({"symbol": "Foo", "no_stale_check": true}),
        ),
        (
            "find_similar",
            json!({"query": "Foo", "no_stale_check": true}),
        ),
        ("show", json!({"symbols": ["Foo"], "no_stale_check": true})),
        ("usages", json!({"symbol": "Foo", "no_stale_check": true})),
        (
            "implementations",
            json!({"symbol": "Foo", "no_stale_check": true}),
        ),
        ("callers", json!({"symbol": "Foo", "no_stale_check": true})),
        ("callees", json!({"symbol": "Foo", "no_stale_check": true})),
        (
            "paths",
            json!({"from": "A", "to": "B", "no_stale_check": true}),
        ),
        (
            "reachable",
            json!({"target": "Foo", "no_stale_check": true}),
        ),
        ("check", json!({"symbols": ["Foo"], "no_stale_check": true})),
        ("similar", json!({"symbol": "Foo", "no_stale_check": true})),
        ("duplicates", json!({"no_stale_check": true})),
        ("bundle", json!({"mode": "project", "no_stale_check": true})),
    ];
    for (tool, args) in cases {
        let extra = args_for(tool, args);
        assert!(
            extra.iter().any(|a| a == "--no-stale-check"),
            "{tool} no_stale_check:true must add --no-stale-check; got: {extra:?}"
        );
    }
}

#[test]
fn no_stale_check_appears_in_every_relevant_schema() {
    // Schema-regression guard: every tool that accepts `auto_update`
    // must also expose `no_stale_check` so MCP clients discover the
    // companion flag via tools/list.
    let desc = tool_descriptors();
    let tools = desc.as_array().expect("tool_descriptors returns array");
    for name in [
        "search",
        "find_symbol",
        "find_similar",
        "show",
        "usages",
        "implementations",
        "callers",
        "callees",
        "paths",
        "reachable",
        "check",
        "similar",
        "duplicates",
        "bundle",
    ] {
        let entry = tools
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("missing tool {name}"));
        let props = &entry["inputSchema"]["properties"];
        assert!(
            props["no_stale_check"].is_object(),
            "{name} schema must expose no_stale_check: {props}"
        );
        assert_eq!(
            props["no_stale_check"]["type"].as_str(),
            Some("boolean"),
            "{name} no_stale_check must be boolean"
        );
    }
}

#[test]
fn bundle_schema_uses_flat_structure_no_oneof() {
    // Locks architect-review A4 — the bundle inputSchema MUST be
    // flat (no `oneOf` discriminated union). If a future revision
    // re-introduces `oneOf`, this test catches it.
    let desc = tool_descriptors();
    let tools = desc.as_array().expect("tool_descriptors returns array");
    let bundle = tools
        .iter()
        .find(|t| t["name"] == "bundle")
        .expect("bundle tool descriptor missing");
    let schema = &bundle["inputSchema"];
    assert!(
        schema.get("oneOf").is_none(),
        "bundle inputSchema must NOT use `oneOf` (A4 — flat schema only); got: {schema}"
    );
    // And `mode` must be a top-level enum field.
    let mode_field = &schema["properties"]["mode"];
    let modes = mode_field["enum"]
        .as_array()
        .expect("bundle.mode must be an enum");
    let mode_names: Vec<&str> = modes.iter().filter_map(|v| v.as_str()).collect();
    assert_eq!(
        mode_names,
        vec!["symbol", "pr-impact", "project"],
        "bundle.mode enum must list the three phase-13.2 modes"
    );
    // Only `mode` is required.
    let required = schema["required"]
        .as_array()
        .expect("bundle.required must be present");
    assert_eq!(required.len(), 1);
    assert_eq!(required[0], "mode");
}

// ── HIGH-tier reviewer follow-up: absence-of-flag guards ──────────────
// `search_filter_arg_pushes_filter_flag` and siblings assert that the
// CLI flag appears when the MCP arg is set; the omitted-by-default
// path was untested. A future change accidentally always-pushing
// `--filter` (or `--kind`, `--context-path`) would slip past the
// present-when-set tests — these absence guards close that gap.

#[test]
fn search_filter_default_omits_flag() {
    let extra = args_for("search", json!({"query": "Foo"}));
    assert!(
        !extra.iter().any(|a| a == "--filter"),
        "search without filter must not pass --filter; got: {extra:?}"
    );
}

#[test]
fn search_kind_default_omits_flag() {
    let extra = args_for("search", json!({"query": "Foo"}));
    assert!(
        !extra.iter().any(|a| a == "--kind"),
        "search without kind must not pass --kind; got: {extra:?}"
    );
}

#[test]
fn search_context_path_default_omits_flag() {
    let extra = args_for("search", json!({"query": "Foo"}));
    assert!(
        !extra.iter().any(|a| a == "--context-path"),
        "search without context_path must not pass --context-path; got: {extra:?}"
    );
}

#[test]
fn show_filter_default_omits_flag() {
    let extra = args_for("show", json!({"symbols": ["Foo"]}));
    assert!(
        !extra.iter().any(|a| a == "--filter"),
        "show without filter must not pass --filter; got: {extra:?}"
    );
}

#[test]
fn show_kind_default_omits_flag() {
    let extra = args_for("show", json!({"symbols": ["Foo"]}));
    assert!(
        !extra.iter().any(|a| a == "--kind"),
        "show without kind must not pass --kind; got: {extra:?}"
    );
}

#[test]
fn show_context_path_default_omits_flag() {
    let extra = args_for("show", json!({"symbols": ["Foo"]}));
    assert!(
        !extra.iter().any(|a| a == "--context-path"),
        "show without context_path must not pass --context-path; got: {extra:?}"
    );
}

// ── HIGH-tier reviewer follow-up: head input validation ───────────────
// `head` is a positive integer. `serde_json::Value::as_u64()` returns
// `None` for negatives and floats — silently dropping the bad value
// hides bugs. Surface them as JSON-RPC errors instead. `head: 0` is
// also rejected (CLI would otherwise reject it too).

#[test]
fn show_head_zero_returns_error() {
    let err = build_command("show", &json!({"symbols": ["Foo"], "head": 0}), "/tmp/proj")
        .expect_err("head: 0 must error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("integer") && msg.contains("head"),
        "error should mention `head` and integer-typed expectation; got: {msg}"
    );
}

#[test]
fn show_head_negative_returns_error() {
    let err = build_command(
        "show",
        &json!({"symbols": ["Foo"], "head": -1}),
        "/tmp/proj",
    )
    .expect_err("head: -1 must error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("integer") && msg.contains("head"),
        "error should mention `head` and integer-typed expectation; got: {msg}"
    );
}

#[test]
fn show_head_float_returns_error() {
    let err = build_command(
        "show",
        &json!({"symbols": ["Foo"], "head": 5.5}),
        "/tmp/proj",
    )
    .expect_err("head: 5.5 must error");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("integer") && msg.contains("head"),
        "error should mention `head` and integer-typed expectation; got: {msg}"
    );
}

// ── HIGH-tier reviewer follow-up: diff-scope parity on search/usages ──
// The diff added diff-scope wiring to pattern/similar/duplicates but
// overlooked search and usages even though both already flatten
// `DiffFilterArgs` in `src/cli/args.rs`. Close the audit hole.

#[test]
fn search_since_pushes_since_flag() {
    let extra = args_for("search", json!({"query": "Foo", "since": "main"}));
    assert!(
        extra
            .windows(2)
            .any(|w| w[0] == "--since" && w[1] == "main"),
        "search since must surface as --since <rev>; got: {extra:?}"
    );
}

#[test]
fn search_since_branched_pushes_flag() {
    let extra = args_for("search", json!({"query": "Foo", "since_branched": true}));
    assert!(
        extra.iter().any(|a| a == "--since-branched"),
        "search since_branched must add --since-branched; got: {extra:?}"
    );
}

#[test]
fn search_changed_only_pushes_flag() {
    let extra = args_for("search", json!({"query": "Foo", "changed_only": true}));
    assert!(
        extra.iter().any(|a| a == "--changed-only"),
        "search changed_only must add --changed-only; got: {extra:?}"
    );
}

#[test]
fn search_diff_scope_flags_mutually_exclusive() {
    let err = build_command(
        "search",
        &json!({"query": "Foo", "since": "main", "since_branched": true}),
        "/tmp/proj",
    )
    .expect_err("mutually exclusive diff-scope flags must error");
    let msg = format!("{err:#}").to_lowercase();
    assert!(
        msg.contains("mutually exclusive"),
        "error should call out the mutual exclusion; got: {msg}"
    );
}

#[test]
fn usages_since_pushes_since_flag() {
    let extra = args_for("usages", json!({"symbol": "Foo", "since": "HEAD~3"}));
    assert!(
        extra
            .windows(2)
            .any(|w| w[0] == "--since" && w[1] == "HEAD~3"),
        "usages since must surface as --since <rev>; got: {extra:?}"
    );
}

#[test]
fn usages_since_branched_pushes_flag() {
    let extra = args_for("usages", json!({"symbol": "Foo", "since_branched": true}));
    assert!(
        extra.iter().any(|a| a == "--since-branched"),
        "usages since_branched must add --since-branched; got: {extra:?}"
    );
}

#[test]
fn usages_changed_only_pushes_flag() {
    let extra = args_for("usages", json!({"symbol": "Foo", "changed_only": true}));
    assert!(
        extra.iter().any(|a| a == "--changed-only"),
        "usages changed_only must add --changed-only; got: {extra:?}"
    );
}

#[test]
fn usages_diff_scope_flags_mutually_exclusive() {
    let err = build_command(
        "usages",
        &json!({"symbol": "Foo", "since": "main", "changed_only": true}),
        "/tmp/proj",
    )
    .expect_err("mutually exclusive diff-scope flags must error");
    let msg = format!("{err:#}").to_lowercase();
    assert!(
        msg.contains("mutually exclusive"),
        "error should call out the mutual exclusion; got: {msg}"
    );
}

// ── HIGH-tier reviewer follow-up: exhaustive diff-scope pair coverage ─
// `pattern_diff_scope_flags_mutually_exclusive` only covered the
// `since + since_branched` pair. The shared `push_diff_scope` helper
// enforces all three pairs — round out the matrix for pattern, plus
// one pair per other tool (similar, duplicates) since the helper is
// shared.

#[test]
fn pattern_diff_scope_since_plus_changed_only_errors() {
    let err = build_command(
        "pattern",
        &json!({
            "pattern": "fn $N()",
            "lang": "rust",
            "since": "main",
            "changed_only": true,
        }),
        "/tmp/proj",
    )
    .expect_err("since + changed_only must error");
    let msg = format!("{err:#}").to_lowercase();
    assert!(
        msg.contains("mutually exclusive"),
        "error should call out the mutual exclusion; got: {msg}"
    );
}

#[test]
fn pattern_diff_scope_since_branched_plus_changed_only_errors() {
    let err = build_command(
        "pattern",
        &json!({
            "pattern": "fn $N()",
            "lang": "rust",
            "since_branched": true,
            "changed_only": true,
        }),
        "/tmp/proj",
    )
    .expect_err("since_branched + changed_only must error");
    let msg = format!("{err:#}").to_lowercase();
    assert!(
        msg.contains("mutually exclusive"),
        "error should call out the mutual exclusion; got: {msg}"
    );
}

#[test]
fn similar_diff_scope_flags_mutually_exclusive() {
    let err = build_command(
        "similar",
        &json!({"symbol": "Foo", "since": "main", "since_branched": true}),
        "/tmp/proj",
    )
    .expect_err("similar mutually exclusive diff-scope flags must error");
    let msg = format!("{err:#}").to_lowercase();
    assert!(
        msg.contains("mutually exclusive"),
        "error should call out the mutual exclusion; got: {msg}"
    );
}

#[test]
fn duplicates_diff_scope_flags_mutually_exclusive() {
    let err = build_command(
        "duplicates",
        &json!({"since": "main", "changed_only": true}),
        "/tmp/proj",
    )
    .expect_err("duplicates mutually exclusive diff-scope flags must error");
    let msg = format!("{err:#}").to_lowercase();
    assert!(
        msg.contains("mutually exclusive"),
        "error should call out the mutual exclusion; got: {msg}"
    );
}

// ── FU-2b: diff-scope on call-graph tools ────────────────────────────
// CLI's `Commands::Callers` / `Callees` / `Implementations` already
// flatten `DiffFilterArgs` (see src/cli/args.rs:556-627). MCP forwards
// through the shared `push_diff_scope` helper. One presence + one
// mutual-exclusion test per tool covers the wiring; the helper itself
// is exhaustively tested for pair combinations above on `pattern`.

#[test]
fn callers_since_pushes_since_flag() {
    let extra = args_for("callers", json!({"symbol": "Foo", "since": "main"}));
    assert!(
        extra
            .windows(2)
            .any(|w| w[0] == "--since" && w[1] == "main"),
        "callers since must surface as --since <rev>; got: {extra:?}"
    );
}

#[test]
fn callers_diff_scope_flags_mutually_exclusive() {
    let err = build_command(
        "callers",
        &json!({"symbol": "Foo", "since": "main", "since_branched": true}),
        "/tmp/proj",
    )
    .expect_err("callers mutually exclusive diff-scope flags must error");
    let msg = format!("{err:#}").to_lowercase();
    assert!(
        msg.contains("mutually exclusive"),
        "error should call out the mutual exclusion; got: {msg}"
    );
}

#[test]
fn callees_since_pushes_since_flag() {
    let extra = args_for("callees", json!({"symbol": "Bar", "since": "HEAD~2"}));
    assert!(
        extra
            .windows(2)
            .any(|w| w[0] == "--since" && w[1] == "HEAD~2"),
        "callees since must surface as --since <rev>; got: {extra:?}"
    );
}

#[test]
fn callees_diff_scope_flags_mutually_exclusive() {
    let err = build_command(
        "callees",
        &json!({"symbol": "Bar", "since_branched": true, "changed_only": true}),
        "/tmp/proj",
    )
    .expect_err("callees mutually exclusive diff-scope flags must error");
    let msg = format!("{err:#}").to_lowercase();
    assert!(
        msg.contains("mutually exclusive"),
        "error should call out the mutual exclusion; got: {msg}"
    );
}

#[test]
fn implementations_since_pushes_since_flag() {
    let extra = args_for(
        "implementations",
        json!({"symbol": "Trait", "since": "origin/main"}),
    );
    assert!(
        extra
            .windows(2)
            .any(|w| w[0] == "--since" && w[1] == "origin/main"),
        "implementations since must surface as --since <rev>; got: {extra:?}"
    );
}

#[test]
fn implementations_diff_scope_flags_mutually_exclusive() {
    let err = build_command(
        "implementations",
        &json!({"symbol": "Trait", "since": "main", "changed_only": true}),
        "/tmp/proj",
    )
    .expect_err("implementations mutually exclusive diff-scope flags must error");
    let msg = format!("{err:#}").to_lowercase();
    assert!(
        msg.contains("mutually exclusive"),
        "error should call out the mutual exclusion; got: {msg}"
    );
}

#[test]
fn diff_scope_flags_default_omitted_across_call_graph_tools() {
    // Belt-and-suspenders absence guard: the empty-args case must NOT
    // accidentally push diff-scope flags. Mirrors the existing
    // `no_stale_check_default_omits_flag_across_tools` table test, but
    // narrowed to the FU-2b-affected tools where diff-scope is new.
    let cases: Vec<(&str, Value)> = vec![
        ("callers", json!({"symbol": "Foo"})),
        ("callees", json!({"symbol": "Foo"})),
        ("implementations", json!({"symbol": "Foo"})),
    ];
    for (tool, args) in cases {
        let extra = args_for(tool, args);
        for flag in ["--since", "--since-branched", "--changed-only"] {
            assert!(
                !extra.iter().any(|a| a == flag),
                "{tool} default must not pass {flag}; got: {extra:?}"
            );
        }
    }
}

// ── FU-1: `eval` MCP wrapper ─────────────────────────────────────────
// Thin forwarder for `vex eval --path <ROOT> [--bench <PATH>]
// [--min-ndcg <F64>] [--json]`. Note: MCP defaults `json` to true
// (CLI default is false) so agents always get structured output.

#[test]
fn eval_default_args_pushes_json_flag() {
    let extra = args_for("eval", json!({}));
    // --path <root> always present.
    assert!(
        extra
            .windows(2)
            .any(|w| w[0] == "--path" && w[1] == "/tmp/proj"),
        "eval must always forward --path; got: {extra:?}"
    );
    // MCP defaults to JSON.
    assert!(
        extra.iter().any(|a| a == "--json"),
        "eval defaults json=true in MCP — --json must be present; got: {extra:?}"
    );
    // Neither optional flag should leak in when its key is omitted.
    assert!(
        !extra.iter().any(|a| a == "--bench"),
        "eval without bench must not pass --bench; got: {extra:?}"
    );
    assert!(
        !extra.iter().any(|a| a == "--min-ndcg"),
        "eval without min_ndcg must not pass --min-ndcg; got: {extra:?}"
    );
}

#[test]
fn eval_bench_path_arg_pushes_bench_flag() {
    let extra = args_for("eval", json!({"bench": "/tmp/golden.toml"}));
    assert!(
        extra
            .windows(2)
            .any(|w| w[0] == "--bench" && w[1] == "/tmp/golden.toml"),
        "eval bench must surface as --bench; got: {extra:?}"
    );
}

#[test]
fn eval_min_ndcg_arg_pushes_min_ndcg_flag() {
    let extra = args_for("eval", json!({"min_ndcg": 0.75}));
    let value = extra
        .windows(2)
        .find_map(|w| (w[0] == "--min-ndcg").then(|| w[1].clone()))
        .expect("eval min_ndcg must surface as --min-ndcg");
    // f64::to_string can render 0.75 with platform-specific precision;
    // assert via numeric parse to stay robust to formatting.
    let parsed: f64 = value
        .parse()
        .unwrap_or_else(|_| panic!("--min-ndcg value should parse as f64; got: {value}"));
    assert!(
        (parsed - 0.75).abs() < 1e-9,
        "eval --min-ndcg value mismatch: {value}"
    );
}

#[test]
fn eval_json_false_omits_json_flag() {
    // MCP default flip is overridable — passing json:false must opt
    // back into the CLI's human-readable summary.
    let extra = args_for("eval", json!({"json": false}));
    assert!(
        !extra.iter().any(|a| a == "--json"),
        "eval json=false must NOT pass --json; got: {extra:?}"
    );
}

#[test]
fn eval_schema_exposes_expected_properties() {
    // Schema regression guard: the `eval` tool must exist in
    // tool_descriptors() and expose exactly the 4 documented
    // properties (bench, min_ndcg, json, project_root). Catches
    // accidental schema drift or copy-paste leakage from neighbouring
    // tools.
    let desc = tool_descriptors();
    let tools = desc.as_array().expect("tool_descriptors returns array");
    let entry = tools
        .iter()
        .find(|t| t["name"] == "eval")
        .expect("eval tool descriptor missing");
    let props = entry["inputSchema"]["properties"]
        .as_object()
        .expect("eval inputSchema.properties must be an object");
    let mut keys: Vec<&str> = props.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec!["bench", "json", "min_ndcg", "project_root"],
        "eval schema must expose exactly bench/min_ndcg/json/project_root"
    );
    // Type sanity.
    assert_eq!(props["bench"]["type"].as_str(), Some("string"));
    assert_eq!(props["min_ndcg"]["type"].as_str(), Some("number"));
    assert_eq!(props["json"]["type"].as_str(), Some("boolean"));
    assert_eq!(props["project_root"]["type"].as_str(), Some("string"));
    // MCP default flip lives in the schema too — surface for clients.
    assert_eq!(
        props["json"]["default"].as_bool(),
        Some(true),
        "eval schema must advertise json default=true (MCP override of CLI default)"
    );
    // No required fields — eval should run with `{}`.
    assert!(
        entry["inputSchema"].get("required").is_none(),
        "eval schema must NOT mark any field required"
    );
}

// ── FU-4: include / exclude glob parity across all path-aware tools ──
// Every MCP tool whose CLI variant flattens `ScopeArgs` in
// `src/cli/args.rs` must (a) forward the `include` / `exclude` arrays
// verbatim through `push_scope`, and (b) advertise both fields in its
// inputSchema so MCP clients can discover them via `tools/list`.
//
// The three tests below are dispatch-presence + dispatch-presence +
// schema-regression guards. The shared `push_scope` helper means one
// tool's presence test already covers the wiring; the table form here
// is belt-and-suspenders against an arm forgetting the helper call
// after a copy-paste of an unrelated dispatch path.

/// Single source of truth for the 16 tools that the FU-4 audit
/// confirmed accept `--include` / `--exclude` on the CLI side. Used
/// by both the presence tests and the schema regression guard so
/// drift in one place is caught by the others.
fn fu4_scope_aware_tool_cases() -> Vec<(&'static str, Value)> {
    vec![
        ("search", json!({"query": "Foo"})),
        ("find_symbol", json!({"symbol": "Foo"})),
        ("find_similar", json!({"query": "Foo"})),
        ("show", json!({"symbols": ["Foo"]})),
        ("usages", json!({"symbol": "Foo"})),
        ("grep", json!({"pattern": "Foo"})),
        ("implementations", json!({"symbol": "Foo"})),
        ("callers", json!({"symbol": "Foo"})),
        ("callees", json!({"symbol": "Foo"})),
        ("pattern", json!({"pattern": "fn $N()", "lang": "rust"})),
        ("diff", json!({"base": "main"})),
        ("paths", json!({"from": "A", "to": "B"})),
        ("reachable", json!({"target": "Foo"})),
        ("similar", json!({"symbol": "Foo"})),
        ("duplicates", json!({})),
        ("bundle", json!({"mode": "project"})),
    ]
}

#[test]
fn include_glob_pushes_include_flag_across_tools() {
    // Each glob round-trips as a separate `--include` flag so clap's
    // `Vec<String>` accumulator on the CLI side sees one value per
    // flag (the standard repeatable-arg shape).
    for (tool, base_args) in fu4_scope_aware_tool_cases() {
        let mut args = base_args.clone();
        args["include"] = json!(["foo/**", "bar/**"]);
        let extra = args_for(tool, args);
        let pairs: Vec<&str> = extra.iter().map(String::as_str).collect();
        let window = |flag: &str, val: &str| pairs.windows(2).any(|w| w[0] == flag && w[1] == val);
        assert!(
            window("--include", "foo/**"),
            "{tool} include[0] must surface as --include foo/**; got: {pairs:?}"
        );
        assert!(
            window("--include", "bar/**"),
            "{tool} include[1] must surface as --include bar/**; got: {pairs:?}"
        );
        let occurrences = pairs.iter().filter(|a| **a == "--include").count();
        assert_eq!(
            occurrences, 2,
            "{tool} should push exactly two --include flags; got: {pairs:?}"
        );
    }
}

#[test]
fn exclude_glob_pushes_exclude_flag_across_tools() {
    for (tool, base_args) in fu4_scope_aware_tool_cases() {
        let mut args = base_args.clone();
        args["exclude"] = json!(["**/generated/**", "vendor/**"]);
        let extra = args_for(tool, args);
        let pairs: Vec<&str> = extra.iter().map(String::as_str).collect();
        let window = |flag: &str, val: &str| pairs.windows(2).any(|w| w[0] == flag && w[1] == val);
        assert!(
            window("--exclude", "**/generated/**"),
            "{tool} exclude[0] must surface as --exclude **/generated/**; got: {pairs:?}"
        );
        assert!(
            window("--exclude", "vendor/**"),
            "{tool} exclude[1] must surface as --exclude vendor/**; got: {pairs:?}"
        );
        let occurrences = pairs.iter().filter(|a| **a == "--exclude").count();
        assert_eq!(
            occurrences, 2,
            "{tool} should push exactly two --exclude flags; got: {pairs:?}"
        );
    }
}

#[test]
fn include_exclude_schemas_complete_across_tools() {
    // Schema-regression guard: every FU-4 scope-aware tool must
    // expose both `include` and `exclude` as `array<string>` so the
    // MCP client's `tools/list` view advertises the filter shape.
    let desc = tool_descriptors();
    let tools = desc.as_array().expect("tool_descriptors returns array");
    for (name, _) in fu4_scope_aware_tool_cases() {
        let entry = tools
            .iter()
            .find(|t| t["name"] == name)
            .unwrap_or_else(|| panic!("missing tool {name}"));
        let props = &entry["inputSchema"]["properties"];
        for field in ["include", "exclude"] {
            let prop = &props[field];
            assert!(
                prop.is_object(),
                "{name} schema is missing {field}: {props}"
            );
            assert_eq!(
                prop["type"].as_str(),
                Some("array"),
                "{name}.{field} must be type=array"
            );
            assert_eq!(
                prop["items"]["type"].as_str(),
                Some("string"),
                "{name}.{field}.items must be type=string"
            );
        }
    }
}

// -----------------------------------------------------------------
// C4 — JSON-RPC parse-error response + stdin-error resilience.
// Regression guard: a malformed input line must produce a
// spec-compliant `-32700` frame with `id: null`, and the loop must
// continue serving subsequent valid requests.
// -----------------------------------------------------------------

#[test]
fn malformed_line_produces_parse_error_then_keeps_serving() {
    // Two lines: garbage (must elicit -32700) + a valid `ping`
    // request (must get its normal response). The loop must NOT
    // terminate on the parse error.
    let input = b"{not valid json\n\
            {\"jsonrpc\":\"2.0\",\"id\":42,\"method\":\"ping\"}\n"
        .to_vec();
    let reader = std::io::BufReader::new(&input[..]);
    let mut output: Vec<u8> = Vec::new();
    run_loop(reader, &mut output).expect("loop exits cleanly");

    let stdout = String::from_utf8(output).expect("stdout is utf8");
    let mut lines = stdout.lines();

    // Frame 1 — parse error.
    let parse_err: Value = serde_json::from_str(lines.next().expect("parse-error frame present"))
        .expect("frame 1 is JSON");
    assert_eq!(parse_err["jsonrpc"].as_str(), Some("2.0"));
    // §5.1: id is explicitly null (not omitted).
    assert!(
        parse_err["id"].is_null(),
        "parse-error id must be JSON null, got: {}",
        parse_err["id"]
    );
    assert_eq!(parse_err["error"]["code"].as_i64(), Some(-32700));
    assert_eq!(parse_err["error"]["message"].as_str(), Some("Parse error"));
    // The truncated echo is best-effort but must be present so a
    // future regression that drops it surfaces here.
    assert!(
        parse_err["error"]["data"].is_string(),
        "parse-error data must echo the raw input fragment, got: {}",
        parse_err["error"]["data"]
    );

    // Frame 2 — valid `ping` request still gets answered.
    let ping_resp: Value = serde_json::from_str(lines.next().expect("ping response present"))
        .expect("frame 2 is JSON");
    assert_eq!(ping_resp["jsonrpc"].as_str(), Some("2.0"));
    assert_eq!(ping_resp["id"].as_i64(), Some(42));
    assert!(
        ping_resp["error"].is_null(),
        "ping must return result, not error: {ping_resp}"
    );
    assert!(
        ping_resp["result"].is_object(),
        "ping result must be a JSON object: {ping_resp}"
    );

    assert!(
        lines.next().is_none(),
        "exactly two response frames expected"
    );
}

// ---- H8: typed-params contract ----
//
// Every `build_command` path that consumes a typed argument must
// emit a `ParamError` when the JSON value carries the wrong type.
// `handle_request` downcasts to `ParamError` and maps it to the
// JSON-RPC spec's `-32602 Invalid params`.

/// Convenience: run `build_command` and require that the error
/// downcasts to `ParamError` containing `needle` in its message.
#[track_caller]
fn assert_param_error(tool: &str, args: Value, needle: &str) {
    let err = build_command(tool, &args, "/tmp/proj")
        .err()
        .unwrap_or_else(|| panic!("expected ParamError for `{tool}` with args {args}"));
    let pe = err.downcast_ref::<ParamError>().unwrap_or_else(|| {
        panic!("expected ParamError for `{tool}`; got non-param error: {err:#}")
    });
    assert!(
        pe.0.contains(needle),
        "ParamError for `{tool}` must mention `{needle}`; got: {msg}",
        msg = pe.0
    );
}

#[test]
fn search_missing_query_is_invalid_params() {
    assert_param_error("search", json!({}), "query");
}

#[test]
fn search_string_limit_is_invalid_params() {
    assert_param_error("search", json!({"query": "foo", "limit": "20"}), "limit");
}

#[test]
fn search_string_auto_update_is_invalid_params() {
    assert_param_error(
        "search",
        json!({"query": "foo", "auto_update": "true"}),
        "auto_update",
    );
}

#[test]
fn search_kind_string_instead_of_array_is_invalid_params() {
    assert_param_error("search", json!({"query": "foo", "kind": "fn"}), "kind");
}

#[test]
fn callers_missing_symbol_is_invalid_params() {
    // Canonical field is `symbol` (legacy alias `name` falls back to
    // it inside `read_canonical_str`). When both are absent we report
    // the canonical name in the error so callers see the
    // up-to-date schema vocabulary.
    assert_param_error("callers", json!({}), "symbol");
}

#[test]
fn callees_missing_symbol_is_invalid_params() {
    assert_param_error("callees", json!({}), "symbol");
}

// ---- per-tool required-field coverage (H8 review follow-up) ----

#[test]
fn paths_missing_from_is_invalid_params() {
    assert_param_error("paths", json!({"to": "bar"}), "from");
}

#[test]
fn paths_missing_to_is_invalid_params() {
    assert_param_error("paths", json!({"from": "foo"}), "to");
}

#[test]
fn reachable_missing_target_is_invalid_params() {
    assert_param_error("reachable", json!({}), "target");
}

#[test]
fn diff_missing_base_is_invalid_params() {
    assert_param_error("diff", json!({}), "base");
}

#[test]
fn show_missing_symbols_is_invalid_params() {
    assert_param_error("show", json!({}), "symbols");
}

#[test]
fn show_missing_field_error_mentions_legacy_alias_symbol() {
    // An LLM agent that hallucinated `symbol` (singular) or sent no
    // matching field at all should see the legacy alias in the
    // error so the recovery is one prompt-of-context away.
    let err =
        build_command("show", &json!({}), "/tmp/proj").expect_err("missing field should error");
    let pe = err.downcast_ref::<ParamError>().expect("ParamError");
    assert!(
        pe.0.contains("symbols") && pe.0.contains("symbol") && pe.0.contains("legacy"),
        "show missing-symbols error must mention canonical + legacy + the word \
             `legacy`; got: {}",
        pe.0
    );
}

#[test]
fn check_missing_field_error_mentions_legacy_alias_names() {
    let err =
        build_command("check", &json!({}), "/tmp/proj").expect_err("missing field should error");
    let pe = err.downcast_ref::<ParamError>().expect("ParamError");
    assert!(
        pe.0.contains("symbols") && pe.0.contains("names") && pe.0.contains("legacy"),
        "check missing-symbols error must mention canonical + legacy alias `names`; \
             got: {}",
        pe.0
    );
}

#[test]
fn check_non_string_element_is_invalid_params() {
    assert_param_error("check", json!({"symbols": ["Foo", 42]}), "symbols[1]");
}

#[test]
fn search_kind_array_with_non_string_element_is_invalid_params() {
    assert_param_error(
        "search",
        json!({"query": "foo", "kind": ["fn", 42]}),
        "kind[1]",
    );
}

#[test]
fn bundle_missing_mode_is_invalid_params() {
    assert_param_error("bundle", json!({}), "mode");
}

#[test]
fn bundle_symbol_mode_missing_symbol_is_invalid_params() {
    assert_param_error("bundle", json!({"mode": "symbol"}), "symbol");
}

#[test]
fn bundle_pr_impact_missing_base_is_invalid_params() {
    assert_param_error("bundle", json!({"mode": "pr-impact"}), "base");
}

#[test]
fn bundle_unknown_mode_is_invalid_params() {
    assert_param_error("bundle", json!({"mode": "nope"}), "unknown bundle mode");
}

/// Pin the cleanup of the bundle arm: presence of an integer field
/// forwards it to the CLI; absence omits the flag entirely (no
/// `--depth 0` leakage). Exercises `opt_u64_some`.
#[test]
fn bundle_pr_impact_omits_depth_when_absent() {
    let built = build_command(
        "bundle",
        &json!({"mode": "pr-impact", "base": "main"}),
        "/tmp/proj",
    )
    .expect("build_command");
    assert!(
        !built.extra_args.iter().any(|a| a == "--depth"),
        "absent `depth` must NOT forward --depth: {:?}",
        built.extra_args
    );
}

#[test]
fn bundle_pr_impact_forwards_depth_when_present() {
    let built = build_command(
        "bundle",
        &json!({"mode": "pr-impact", "base": "main", "depth": 3}),
        "/tmp/proj",
    )
    .expect("build_command");
    let depth_idx = built
        .extra_args
        .iter()
        .position(|a| a == "--depth")
        .expect("expected --depth flag");
    assert_eq!(built.extra_args.get(depth_idx + 1), Some(&"3".to_string()));
}

/// Mutually-exclusive flag pairs that previously fell into `-32000`
/// now ride the same `-32602` channel as the other type errors.
#[test]
fn search_diff_scope_conflict_is_invalid_params() {
    assert_param_error(
        "search",
        json!({"query": "x", "since": "main", "changed_only": true}),
        "mutually exclusive",
    );
}

#[test]
fn show_truncate_conflict_is_invalid_params() {
    assert_param_error(
        "show",
        json!({"symbols": ["Foo"], "signature_only": true, "no_body": true}),
        "mutually exclusive",
    );
}

#[test]
fn search_async_only_no_async_conflict_is_invalid_params() {
    assert_param_error(
        "search",
        json!({"query": "x", "async_only": true, "no_async": true}),
        "mutually exclusive",
    );
}

#[test]
fn search_float_limit_is_invalid_params() {
    assert_param_error("search", json!({"query": "foo", "limit": 5.5}), "limit");
}

#[test]
fn search_negative_limit_is_invalid_params() {
    assert_param_error("search", json!({"query": "foo", "limit": -3}), "limit");
}

/// Sanity: a correctly-typed call still builds.
#[test]
fn search_correctly_typed_args_succeed() {
    let built = build_command(
        "search",
        &json!({"query": "foo", "limit": 5, "auto_update": false}),
        "/tmp/proj",
    )
    .expect("typed-correct search must build");
    assert_eq!(built.subcommand, "search");
    assert!(built.extra_args.iter().any(|a| a == "foo"));
}

/// handle_request must surface ParamError as JSON-RPC `-32602`.
#[test]
fn handle_request_maps_param_error_to_minus_32602() {
    let req = JsonRpcRequest {
        jsonrpc: "2.0".into(),
        id: Some(json!(7)),
        method: "tools/call".into(),
        params: Some(json!({
            "name": "search",
            "arguments": { "query": "foo", "limit": "twenty" }
        })),
    };
    let resp = handle_request(&req);
    let err = resp.error.expect("expected error response");
    assert_eq!(err.code, -32602, "expected -32602 Invalid params");
    assert!(err.message.contains("limit"));
}

// ── Multi-repo `--workspace` surface (Phase 8) ──────────────────────────

/// `(tool, required-args)` for each workspace-capable tool. Adding
/// `"workspace": true` to these must push `--workspace`.
fn workspace_tool_cases() -> Vec<(&'static str, Value)> {
    vec![
        ("search", json!({ "query": "Foo" })),
        ("grep", json!({ "pattern": "Foo" })),
        ("check", json!({ "symbols": ["Foo"] })),
        ("usages", json!({ "symbol": "Foo" })),
        ("impact", json!({ "symbol": "Foo" })),
        ("callers", json!({ "symbol": "Foo" })),
        ("callees", json!({ "symbol": "Foo" })),
        ("reachable", json!({ "target": "Foo" })),
        ("index", json!({})),
        ("update", json!({})),
    ]
}

#[test]
fn workspace_true_pushes_flag_for_every_covered_tool() {
    for (tool, base) in workspace_tool_cases() {
        let mut args = base.clone();
        args["workspace"] = json!(true);
        let extra = args_for(tool, args);
        assert!(
            extra.iter().any(|a| a == "--workspace"),
            "{tool} with workspace=true must push --workspace, got: {extra:?}"
        );
    }
}

#[test]
fn workspace_omitted_does_not_push_flag() {
    for (tool, base) in workspace_tool_cases() {
        let extra = args_for(tool, base.clone());
        assert!(
            !extra.iter().any(|a| a == "--workspace"),
            "{tool} without workspace must not push --workspace, got: {extra:?}"
        );
    }
}

#[test]
fn grep_text_true_pushes_flag() {
    let extra = args_for("grep", json!({ "pattern": "Foo", "text": true }));
    assert!(
        extra.iter().any(|a| a == "--text"),
        "grep with text=true must push --text, got: {extra:?}"
    );
}

#[test]
fn grep_text_omitted_does_not_push_flag() {
    let extra = args_for("grep", json!({ "pattern": "Foo" }));
    assert!(
        !extra.iter().any(|a| a == "--text"),
        "grep without text must not push --text, got: {extra:?}"
    );
}

#[test]
fn search_workspace_drops_why_clap_conflict() {
    // `--workspace` conflicts_with `--why` on the CLI; workspace wins.
    let extra = args_for(
        "search",
        json!({ "query": "Foo", "workspace": true, "why": true }),
    );
    assert!(extra.iter().any(|a| a == "--workspace"), "got: {extra:?}");
    assert!(
        !extra.iter().any(|a| a == "--why"),
        "search must drop --why in workspace mode, got: {extra:?}"
    );
    // Without workspace, --why is still honoured.
    let extra2 = args_for("search", json!({ "query": "Foo", "why": true }));
    assert!(extra2.iter().any(|a| a == "--why"), "got: {extra2:?}");
}

#[test]
fn usages_workspace_drops_why_clap_conflict() {
    let extra = args_for(
        "usages",
        json!({ "symbol": "Foo", "workspace": true, "why": true }),
    );
    assert!(extra.iter().any(|a| a == "--workspace"), "got: {extra:?}");
    assert!(
        !extra.iter().any(|a| a == "--why"),
        "usages must drop --why in workspace mode, got: {extra:?}"
    );
    let extra2 = args_for("usages", json!({ "symbol": "Foo", "why": true }));
    assert!(extra2.iter().any(|a| a == "--why"), "got: {extra2:?}");
}

#[test]
fn workspace_param_exposed_on_covered_tools_only() {
    let desc = tool_descriptors();
    let tools = desc.as_array().expect("array");
    let has_ws = |name: &str| -> bool {
        tools
            .iter()
            .find(|t| t["name"] == name)
            .map(|t| t["inputSchema"]["properties"]["workspace"].is_object())
            .unwrap_or(false)
    };
    for (tool, _) in workspace_tool_cases() {
        assert!(has_ws(tool), "{tool} schema must expose `workspace`");
    }
    // find_symbol is intentionally excluded (use check/search for cross-repo).
    assert!(
        !has_ws("find_symbol"),
        "find_symbol must NOT expose `workspace` (excluded by design)"
    );
}
