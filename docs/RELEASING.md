# Releasing vex

This document covers what happens when a `v*` tag is pushed and how the
signing keypair used by `vex self-update` is managed.

## Release flow

A push of `v<X>.<Y>.<Z>` to GitHub triggers `.github/workflows/release.yml`:

1. **`test`** — runs fmt (Linux only), clippy, and `cargo test --workspace`
   on ubuntu-latest, macos-latest, and windows-latest. Fails the release
   if any platform breaks.
2. **`build`** — cross-compiles release binaries for three triples:
   `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`,
   `x86_64-pc-windows-msvc`. The Linux leg runs on **ubuntu-22.04**, not
   ubuntu-latest: a binary needs at least the glibc of the machine it was
   built on, and 24.04's 2.39 locked out Ubuntu 22.04 / Debian 12 (v1.27.2).
   `scripts/check-glibc-floor.sh 2.35` fails the build if a dependency raises
   the floor again; CI's `glibc-floor` job runs the same check on every push. Archives are `.tar.gz` on every platform
   (Windows switched from `.zip` to `.tar.gz` in v1.9.2 — the `self_update`
   crate could not strip zipsign's signed-zip prefix, leaving Windows
   self-update broken since v1.8.2); each contains a single `vex` (or
   `vex.exe`) binary.
3. **`release`** —
   1. downloads all archives,
   2. installs `zipsign`,
   3. signs every archive with the ed25519 private key from the
      `ZIPSIGN_PRIVATE_KEY_B64` GitHub Secret,
   4. generates the release body via `git-cliff` (categorised commit
      log between the previous and current tags), and
   5. publishes the GitHub Release with the signed archives attached.
4. **`update-homebrew`** — bumps the Homebrew formula in
   `tenatarika/homebrew-tap` (source archive only; the binary tarballs
   are not pinned by Homebrew).

Off to the side, the **`mcpb`** job builds the MCP Bundles from the build
artifacts, **`attach-mcpb`** adds them to the release once it exists, and
**`publish-mcp-registry`** publishes them to the MCP Registry — see
[MCP Registry](#mcp-registry-mcpb-bundles). Neither `release` nor
`update-homebrew` depends on these jobs, so an MCPB or registry failure
never blocks the signed tarballs or the formula.

`vex self-update` downloads from this same release, verifies the
embedded zipsign signature against the public key compiled into the
binary (`VEX_RELEASE_PUBKEY` in `src/cli/cmd_self_update.rs`), and
only then extracts and replaces the running binary.

## Signing keypair

The keypair is ed25519. The public key (32 bytes) is embedded in source
as a Rust byte array. The private key (64 bytes) is stored as
`ZIPSIGN_PRIVATE_KEY_B64` in the repository's GitHub Secrets, encoded
as base64.

### One-time setup

If the secret has been lost or never existed:

```bash
# Generate the keypair locally
zipsign gen-key vex.priv vex.pub

# Print the public key bytes for the Rust constant
python3 -c "
with open('vex.pub','rb') as f:
    print(', '.join(f'0x{b:02x}' for b in f.read()))
"

# Print the private key as base64 for the GitHub Secret
base64 -i vex.priv
```

Then:
1. Replace `VEX_RELEASE_PUBKEY` in `src/cli/cmd_self_update.rs` with the
   new public bytes and commit.
2. Add (or rotate) the `ZIPSIGN_PRIVATE_KEY_B64` secret in
   GitHub → Settings → Secrets and variables → Actions.
3. Shred the local key files. They are not needed again until rotation.

### Rotation

Rotating the key is a breaking change for `vex self-update` on every
binary published with the *previous* public key — those binaries can
no longer verify new releases and will refuse the update.

The migration path:
1. Cut a regular release with the **current** key (e.g. v2.0.0).
2. Publish a notice telling users to update.
3. After a reasonable window, generate the new keypair, update the
   constant, rotate the secret, and cut the next release. Users on
   v2.0.0 will fail to self-update past this point and must download
   the new archive manually once.

For this reason, treat the keypair as long-lived. Rotate only if the
private key is suspected compromised.

## Cutting a new release

```bash
# 1. Update CHANGELOG.md — move [Unreleased] entries into a new
#    [X.Y.Z] section dated today.
$EDITOR CHANGELOG.md
git commit -am "docs: prepare vX.Y.Z release notes"

# 2. Bump the version. It lives in ONE place: `[workspace.package] version`
#    in the root Cargo.toml. Both `vex-search` (binary `vex`) and
#    `vex-search-mcp` (binary `vex-mcp`) inherit it via
#    `version.workspace = true`, so they can no longer drift apart (vex-mcp
#    sat at 0.1.0 for several releases before this).
$EDITOR Cargo.toml
cargo build --release  # updates the `vex-search` entry in Cargo.lock to match
# The ROOT Cargo.lock IS tracked (only fuzz/Cargo.lock is gitignored).
# Stage it alongside Cargo.toml or CI's `--locked` build fails with
# "Cargo.lock needs to be updated". `-am` covers it (lock is tracked).
git commit -am "chore: bump version to X.Y.Z"

# 3. Tag and push.
git tag vX.Y.Z
git push origin main
git push origin vX.Y.Z

# 4. (optional) Publish to crates.io. The package names differ from the
#    binary names because `vex` / `vex-mcp` are taken there: package
#    `vex-search` installs `vex`, `vex-search-mcp` installs `vex-mcp`.
#    Release assets (vex-<triple>.tar.gz / vex-mcp-<triple>.tar.gz) are
#    unaffected. Dry-run first; the two crates are independent (no path
#    dep between them), so order does not matter. Publish from a CLEAN
#    checkout of the tag (`git status` empty, HEAD == vX.Y.Z) and never pass
#    --allow-dirty: the uploaded .crate is immutable and must match the tag.
cargo publish --dry-run -p vex-search
cargo publish --dry-run -p vex-search-mcp
cargo publish -p vex-search
cargo publish -p vex-search-mcp
```

`git-cliff` will read commits between the previous tag and `vX.Y.Z` to
build the GitHub release body, so make sure your commit messages
follow the conventional-commit prefixes (`feat`, `fix`, `docs`,
`chore`, etc.). The `chore: bump version` and `docs: prepare vX
release notes` commits are filtered out of the auto-generated body —
see `cliff.toml`.

## MCP Registry (.mcpb bundles)

From v1.27.2 every stable release is published to the official MCP Registry
(<https://registry.modelcontextprotocol.io>) as `io.github.tenatarika/vex`.
The registry stores metadata only; the artifacts are MCP Bundles attached
to the GitHub release.

Files:

| File | Role |
| --- | --- |
| `packaging/mcpb/manifest.json` | MCPB manifest template (`manifest_version` 0.3, `server.type: binary`). Version, `entry_point`, command/`VEX_BIN` paths and `compatibility.platforms` are filled per target. |
| `server.json` (repo root) | Registry `server.json` template. `__VERSION__` / `__SHA256__` placeholders; the template itself fails `mcp-publisher validate` on purpose. |
| `packaging/mcpb/build-bundle.sh` | Renders the manifest for one target, runs `mcpb validate`, `chmod 0755` on the unix binaries, `mcpb pack` → `vex-mcp-<target>.mcpb`. |
| `packaging/mcpb/render-server-json.sh` | Fills the version and each bundle's SHA-256 into `server.json`; fails on a missing bundle, a malformed hash, a leftover placeholder or a description over 100 chars. |
| `packaging/mcpb/dry-run.sh` | Local dry run for the host platform (see below). |
| `packaging/mcpb/package.json` + `package-lock.json` | Pin `@anthropic-ai/mcpb` 2.1.2 and its whole dependency tree (integrity hashes). Installed with `npm ci --ignore-scripts --prefix packaging/mcpb`; `node_modules/` is gitignored. `$MCPB` overrides the CLI for the scripts. To bump: `npm install --package-lock-only --ignore-scripts @anthropic-ai/mcpb@<v>` in that directory. |

Pipeline:

1. **`mcpb`** (after `build`, ubuntu, `contents: read`, checkout without
   persisted credentials) — installs the lockfile-pinned mcpb CLI with
   `npm ci --ignore-scripts`; for each of the three targets extracts
   `vex-<t>.tar.gz` + `vex-mcp-<t>.tar.gz` into `stage/<t>/server` (the
   Windows one brings `DirectML.dll`) and builds the bundle; requires
   `vex --version` to be exactly `vex X.Y.Z` or `vex vX.Y.Z` for the tag;
   renders `server.rendered.json`; unzips each bundle to verify the
   manifest, the exec bits and the DLL; uploads everything as the `mcpb`
   artifact.
2. **`attach-mcpb`** (after `release` and `mcpb`, `contents: write`) —
   `gh release upload`s the three `vex-mcp-*.mcpb`. An asset that is already
   attached is compared by SHA-256: identical → skipped, different → the job
   fails. There is deliberately no `--clobber`: once the registry lists a
   version it pins the old hash, and replacing the bundle would make that
   listing uninstallable. Bundles are not zipsigned (the signing loop in
   `release` matches only `vex-*.tar.gz`; self-update never downloads them,
   and the registry pins them by SHA-256).
3. **`publish-mcp-registry`** (after `attach-mcpb`; skipped for tags containing
   `-`, i.e. prereleases) — checks `server.rendered.json` names this tag,
   downloads each LIVE release URL and compares its SHA-256 with
   `server.json`, asks the registry whether this version already exists,
   then installs `mcp-publisher` v1.8.1 (tarball SHA-256 pinned in the
   workflow), runs `validate`, `login github-oidc` (job permission
   `id-token: write`; no secret) and `publish`.

Bundle naming is load-bearing: `vex-mcp-<target>.mcpb`, never
`vex-<target>.mcpb`. `vex self-update` picks its asset with
`name.contains("vex-<target>")`, which `vex-<target>.mcpb` would also
satisfy. Pinned by `mcpb_bundle_never_matches_self_update_identifier` in
`src/cli/cmd_self_update.rs`. The same file makes `vex self-update` refuse
to run when the exe sits inside an unpacked vex bundle (a `manifest.json`
with `name: "vex"` and `server.type: "binary"` next to the exe or one
directory up): the host app owns that install.

Local dry run (needs a release build, node/npm, jq, unzip; publishes nothing;
runs `npm ci` into `packaging/mcpb/node_modules` on first use):

```bash
cargo build --release --workspace
packaging/mcpb/dry-run.sh            # → target/mcpb-dry-run/vex-mcp-<host>.mcpb
```

It builds the host-platform bundle from `target/release/{vex,vex-mcp}`,
lists its contents, checks exec bits and manifest fields, starts the
bundled `vex-mcp` for an `initialize` + `tools/list` round trip, and prints
the SHA-256.

Failure modes:

- **Publish before the assets are live.** The registry HEAD-checks every
  package URL during `publish` and rejects the version if one 404s. That is
  why the job runs after `attach-mcpb`. If it ran early or the release was
  re-created, re-run the job once the assets are up.
- **`attach-mcpb` fails with "already attached with different bytes".**
  The `mcpb` job was re-run (zip timestamps change, so every rebuild has new
  hashes) after the bundles were attached. If the version is NOT in the
  registry yet, delete the three `vex-mcp-*.mcpb` assets from the release
  (`gh release delete-asset vX.Y.Z <name>`) and re-run `attach-mcpb` +
  `publish-mcp-registry`. If it already is, leave the assets alone.
- **Re-publishing a version.** A version can be published exactly once.
  The job asks `GET /v0/servers/io.github.tenatarika%2Fvex/versions/<v>`
  first: 404 → publish; 200 with the same identifiers and hashes → success
  without publishing, so re-running a green or half-green workflow is safe;
  200 with different hashes → the job fails. Bundles rebuilt after
  publishing (a re-run of `mcpb`, a re-uploaded asset) have new hashes and
  can never be published under the same version. Cut a patch release
  instead. Don't re-run the whole workflow for an already-published tag;
  re-run only the failed jobs.
- **Hash mismatch.** `Verify live release assets` fails when a release
  asset differs from the bundle the `mcpb` job hashed (manual re-upload,
  a release edited by hand). Clients verify `fileSha256` after download, so
  a mismatched listing would be uninstallable. Restore the original asset or
  cut a patch release.
- **OIDC login fails.** The job needs `permissions: id-token: write`, and
  the repo must sit under the `tenatarika` GitHub account the
  `io.github.tenatarika/` namespace belongs to.

Manual fallback (the job failed and re-running it doesn't help). Download
the `mcpb` artifact from the workflow run (it holds
`server.rendered.json`), check it against the live assets, then publish
interactively:

```bash
gh run download <run-id> -n mcpb -D mcpb
jq -r '.packages[] | "\(.fileSha256)  \(.identifier)"' mcpb/server.rendered.json
# for each line: curl -fsSL <url> | sha256sum  → must match
curl -fsSL -o mcp-publisher.tar.gz \
  "https://github.com/modelcontextprotocol/registry/releases/download/v1.8.1/mcp-publisher_$(uname -s | tr A-Z a-z)_$(uname -m | sed 's/x86_64/amd64/;s/aarch64/arm64/').tar.gz"
tar -xzf mcp-publisher.tar.gz mcp-publisher
./mcp-publisher validate mcpb/server.rendered.json
./mcp-publisher login github     # device flow, as the tenatarika account
./mcp-publisher publish mcpb/server.rendered.json
```

If the artifact has expired, rebuild `server.json` from the live release
assets: download the three `vex-mcp-*.mcpb` files into a directory and run
`packaging/mcpb/render-server-json.sh X.Y.Z <dir> > server.rendered.json`.

## DirectML.dll pin (Windows release archive)

`.github/workflows/release.yml` ships `DirectML.dll` next to `vex.exe` in
the Windows tarball and verifies a SHA-256 pin before staging — see the
`EXPECTED_SHA256` env on the staging step. The pinned value MUST match
the DirectML build that the current `ort` crate version (`Cargo.toml`
`ort = "=2.0.0-rc.12"`) statically links against. Updating the pin is
a two-step audit:

```bash
# 1. Find which DirectML redist the installed ort version bundles.
#    ort-sys pulls it via build-time download (path differs by host):
#      Windows: %LOCALAPPDATA%\ort.pyke.io\<version>\runtimes\win-x64\native\DirectML.dll
#      Linux/macOS: ~/.cache/ort.pyke.io/... (no DirectML on these targets;
#                   pin is verified on the Windows runner only)
#    A fresh `cargo build --target x86_64-pc-windows-msvc --features gpu-directml`
#    on the Windows runner populates that path; alternatively, fetch the
#    same blob from nuget.org by the version ort pins in its build.rs.
#
# 2. Compute and verify the SHA-256. Pass the FULL path discovered in
#    step 1 — `sha256sum DirectML.dll` would hash whatever happens to be
#    in $PWD (potentially a stale or unrelated DLL) and silently agree.
#
#    PowerShell (the canonical Windows path — the redist lives under
#    %LOCALAPPDATA% which has no Unix-shell equivalent):
# (Get-FileHash "$env:LOCALAPPDATA\ort.pyke.io\<version>\runtimes\win-x64\native\DirectML.dll" `
#     -Algorithm SHA256).Hash
#
#    Linux/macOS (only if you fetched the NuGet redist manually for audit):
sha256sum "/full/path/to/extracted/DirectML.dll"
```

When bumping `ort` to a version that pulls a different DirectML release
(check the ort changelog for "DirectML version" or grep `ort-sys`
`build.rs` for the pinned URL/version), update `EXPECTED_SHA256` in
`release.yml` to the new SHA. The Windows job will fail closed on
mismatch and surface the candidate list, so a missed bump is loud.

Cross-reference: `Microsoft.AI.DirectML` packages on nuget.org are
Microsoft-signed; the SHA you pin should come from a NuGet-extracted
DLL, not from a redistributable shipped by a third party.

## Internal format versions

Some on-disk format versions live as constants in source rather than in
the v6 binary header — bump them when their backing shape changes,
otherwise stale entries on user machines will deserialize into the wrong
struct.

- `CACHE_FORMAT_VERSION` (`src/index/parse_cache/mod.rs`, u16) —
  bump on any structural change to `ParsedFile` or any of its
  transitively serialized members (`ParsedSymbol`, `ParsedRef`,
  `RawCallEdge`, `BoundRef`, `BindTarget`, `RefKind`, `UsePath`,
  `Skeleton`, `SymbolKind`, `HierarchyCapture`). New variant on a
  serialized enum, field added or removed on a serialized struct,
  changed `repr` on a `#[repr(u8)]` enum — all qualify. The blob
  cache treats a version mismatch as a miss and overwrites lazily,
  so missing a bump only costs cache invalidation work on the next
  user run, not a correctness incident — but the bump is still
  cheap insurance. (Bumped to `5` for the hierarchy-edges P2
  `ParsedFile.hierarchy_captures` field.)

The binary index, by contrast, carries grammar fingerprints inline
and self-invalidates without a manual bump — see
`src/store/pattern_skeletons.rs`.
