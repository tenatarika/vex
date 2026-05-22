# Pattern fixtures (Phase 11.4)

Each subdirectory is one regression case for `vex pattern`. The
`tests/pattern_fixture_test.rs` harness loops over every directory,
runs `vex pattern` against `input.<ext>` using the `spec.toml`
metadata, and asserts each expected match line appears in the JSON
output.

## Layout

```
<fixture_name>/
├── input.<ext>   — source code (rs / ts / py)
└── spec.toml     — pattern + lang + expected matches
```

## `spec.toml` schema

```toml
lang = "rust"                            # rust | typescript | python
pattern = "fn $NAME($$$ARGS) -> $R"      # passed verbatim to `vex pattern`
exercises = "block_metavar"              # baseline | block_metavar | arg_ellipsis | and_composition | or_composition

[[expected]]
line = 1
captures = { NAME = "process", R = "Result" }

[[expected]]
line = 7
```

`captures` is optional. When present, the harness asserts each
`$KEY = value` appears in that match's `captures` array.

## RED today

- `baseline_*` fixtures use only today-syntax and **must pass** to
  prove the harness itself works.
- Every other fixture exercises a scope-B feature that hasn't shipped
  yet (`$$$BODY`, `$$ARGS`, `&&`, `||`) and **fails** until the
  matching increment lands. The failure line shows what the matcher
  produced vs. expected, so progress on each feature is visible per
  fixture.
