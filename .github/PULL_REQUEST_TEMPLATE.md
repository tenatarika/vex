<!--
Thanks for the contribution. Keep this PR description short — the
checklist below is what we actually look at in review.
-->

## Summary

<!-- 1-3 sentences: what changes and why. Link the issue or Phase note. -->

Closes #

## Type of change

- [ ] Bug fix (non-breaking)
- [ ] New feature (non-breaking)
- [ ] Breaking change (JSON envelope, MCP schema, CLI flag rename, index format bump)
- [ ] Refactor / internal cleanup (no behavior change)
- [ ] Docs only
- [ ] CI / build / release tooling

## Checklist

- [ ] `cargo fmt --check` is clean
- [ ] `cargo clippy --all-targets -- -D warnings` is clean
- [ ] `cargo test` passes locally
- [ ] New behavior is covered by a test (unit, integration, or fuzz target)
- [ ] CHANGELOG entry added under `## [Unreleased]` in the right section (`### Added` / `### Changed` / `### Fixed` / `### Refactored`)
- [ ] If breaking: migration note in the CHANGELOG entry **and** README updated
- [ ] If a new flag / command: README command table + `vex --help` text updated
- [ ] If a new index section / format bump: reader keeps reading the previous version, or a "re-run `vex index`" error message is added

## Compatibility notes

<!--
If this changes the JSON envelope, MCP schema, on-disk index format,
or CLI surface, describe the migration path here. If not, write "None".
-->

## How to verify

<!--
Concrete commands a reviewer can paste. Reproduces the old behavior on
main and the new behavior on this branch.
-->

```bash

```
