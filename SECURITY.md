# Security Policy

## Supported Versions

Only the latest minor release receives security fixes. Older minor versions
are not patched — please upgrade to the latest release before reporting.

| Version | Supported |
|---------|-----------|
| 1.11.x  | yes       |
| < 1.11  | no — upgrade first |

`vex self-update` will fetch the latest GitHub release on Linux, macOS,
and Windows.

## Reporting a Vulnerability

**Do not open a public GitHub issue for security vulnerabilities.** Use
the private channel instead:

**GitHub Security Advisories**:
<https://github.com/tenatarika/vex/security/advisories/new>

This creates a private advisory thread visible only to maintainers.
Please include enough detail to reproduce: vex version, OS, minimal
repro, and the impact you observed.

You can expect:

- Acknowledgement within **72 hours**.
- A first triage assessment (severity + likely fix window) within **7 days**.
- A coordinated-disclosure window of **up to 90 days** before public
  disclosure. We will request an extension only if the fix is non-trivial
  and we keep you informed about progress.
- Credit in the release notes and the CHANGELOG, unless you prefer to
  remain anonymous.

## In-Scope Issues

These are the surfaces we treat as security-relevant:

- **Binary format reader** (`src/store/`): out-of-bounds reads,
  misaligned pointer dereferences, integer overflow on offsets,
  type-confusion across sections. The reader is exercised by three
  libFuzzer targets (`fuzz_index_reader`, `fuzz_refs_fst`,
  `fuzz_symbol_fst`); past fuzz-found bugs are listed in the README.
- **Mmap / unsafe paths**: any code path in `src/store/reader.rs` or
  the FST readers that can be tripped by a hand-crafted `.vex` file.
- **MCP server (`crates/vex-mcp`)**: JSON-RPC parser, stdio handling,
  path-traversal in tool arguments (`VEX_ROOT` containment), or
  resource exhaustion via malformed `tools/call` payloads.
- **`vex self-update`**: signature verification, archive extraction
  (path traversal in tar/zip), or downgrade attacks via the release
  manifest.
- **Tree-sitter grammar / pattern engine**: stack overflow or quadratic
  blowup on adversarial source files, exploitable via `vex index` of
  attacker-supplied code.

## Out of Scope

- Issues that require an attacker to already have write access to your
  source tree or shell. Indexing untrusted code is a supported use case,
  but executing untrusted code is the language toolchain's concern.
- Bugs in upstream tree-sitter grammar crates — please report those
  upstream; we'll bump the grammar version once a fix lands.
- Denial-of-service via legitimately large repositories. `vex index` is
  bounded by your filesystem and `--exclude` is the supported mitigation.
- Findings against pre-release builds, forks, or unreleased branches.

## Hardening Notes

If you're embedding vex into a multi-tenant environment:

- Treat `.vex` index files as **untrusted input** even when you wrote
  them yourself — they're consumed via mmap and parsed without copying.
  The fuzz harness covers this, but defense in depth helps.
- The MCP server reads `VEX_ROOT` from the environment and rejects paths
  that escape it. Don't pass user-controlled values into `VEX_ROOT`.
- `vex self-update` verifies release archives via zipsign signatures —
  do not pipe arbitrary URLs into the updater.
