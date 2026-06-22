# Exit codes (v1.12.0)

`vex` follows a strict three-state exit-code contract so shell scripts,
CI gates, and agent harnesses can distinguish "no results" from real
errors without parsing stdout.

| Code | Meaning              | When                                                                                                                    |
|------|----------------------|-------------------------------------------------------------------------------------------------------------------------|
| `0`  | success with results | A query returned ≥1 match, or an action (`vex index`, `vex update`, `vex self-update`) completed.                       |
| `1`  | empty result         | A query ran without error but the answer is "nothing" — zero matches, no callers, symbol not found, no near-duplicates. |
| `2`  | error                | Bad regex, corrupted index, I/O failure, invalid args past clap's own validation, or any other `Err(_)` from a handler. |

`2` is also what clap itself emits on argument-parsing errors, so
script-side error handling can collapse "vex didn't run" and "vex ran
and failed" into a single non-zero, non-one bucket.

## Coverage by subcommand

**Distinguishes 0 / 1:** `search`, `usages`, `callers`, `callees`,
`pattern`, `grep`, `show`, `similar`, `duplicates`, `implementations`,
`paths`, `reachable`, `tests-for`, `history`, `diff`, `bundle`.

These commands query the index for results. Empty result sets are a
normal outcome, not an error.

**`vex history` caveat — `--branch <unknown-ref>` exits `2`.** Phase
14.11 routes non-HEAD `--branch` queries to the walker, which shells
out to `git log` / `git grep <revision>`. Git surfaces an error for
revisions it can't resolve; vex propagates that as exit `2` (real
error from a handler), not `1` (empty result). Pre-14.11 the indexed
path silently swallowed unknown refs and returned HEAD-time data with
exit `0` — the new exit code is more honest. Scripts that pass
user-supplied refs should treat `2` as "bad ref" and recover, not
abort.

**`vex bundle` caveat — soft-degrades exit `1`.** `--mode project`
without a call graph (or with `--directory-tree-only` filtering to
nothing) returns an empty `items[]` plus a populated
`mode_hints.empty_reason` (`"no_call_graph"`,
`"directory_tree_top_zero"`, `"path_glob_filtered_all"`). The exit
code is `1` for both "genuinely unreachable" and "soft-degrade" cases
— scripts that need to distinguish them must read
`results.mode_hints.empty_reason` from the JSON envelope.

**Always `0` on success:** `index`, `update`, `watch`, `self-update`,
`status`, `outline`, `check`, `init`, `completions`, `eval`, `impact`.

These are actions or configuration queries. There is no
"empty-but-successful" state worth distinguishing from regular success.

**`vex impact` caveat — verdict is in the envelope, not the exit code.**
`impact` always exits `0` on success (any `safe` / `unsafe` / `uncertain`
verdict is a successful execution); a real handler error still exits `2`.
Scripts that gate on impact must read `results.verdict` from the JSON
envelope, NOT compare against exit `1` the way they would for `search` /
`usages`. Rationale: the verdict is a structured outcome with three
states, not a binary "did we find anything?" — collapsing it into the
0/1 exit code would either lose information or surprise users.

## Why a side-channel instead of typed handler returns

`vex` has ~25 CLI subcommands. Threading `Result<ExitCode>` through every
handler would be a wide-blast-radius refactor for what is inherently a
process-global property ("what code does this process exit with?"). A
single static `AtomicBool` (`src/cli/exit_code.rs`) captures the
0-vs-1 distinction with near-zero call-site cost: handlers that find no
results call `signal_no_results()` once before returning their normal
`Ok(())`; the binary maps that into `ExitCode::from(1)` at the dispatch
boundary. Errors stay on `Err(_)` and are mapped to `2` by `main`.

The CLI binary runs exactly one subcommand per process, so the global
state has no concurrency footgun.

## Examples

```sh
# Gate a CI step on "results found", treating empty as a non-fatal skip.
#
# NB: capture `$?` immediately — putting the call inside `if` would
# overwrite `$?` with the exit status of `[` before the elif sees it.
vex callers MyHandler > /dev/null
code=$?
case $code in
    0) run_dependent_analysis ;;
    1) echo "MyHandler is dead code; skipping downstream analysis." ;;
    *) echo "vex callers failed; aborting." >&2; exit 1 ;;
esac

# Distinguish "no PR-impact items" from "diff failed":
vex bundle --mode pr-impact --base origin/main > impact.json
case $? in
    0) jq '.results' impact.json | post_to_slack ;;
    1) echo "PR touches nothing reachable; no review fan-out needed." ;;
    *) echo "vex bundle failed; rerun with VEX_TRACE=1." >&2; exit 1 ;;
esac
```

## History

Landed as **S8.2** in v1.12.0. Before v1.12.0 every query subcommand
exited `0` on both "found results" and "no results", forcing scripts to
parse stdout (or `--format json` `results.length`) to tell them apart.
The initial S8.2 pass wired the 0/1 split into `search`, `usages`,
`callers`, `callees`, `pattern`, `grep`, and `show`; the final review
pass before tag extended coverage to `similar`, `duplicates`,
`implementations`, `paths`, `reachable`, `diff`, and `bundle` — all
fourteen query subcommands now honour the contract.
