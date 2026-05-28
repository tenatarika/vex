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
   `x86_64-pc-windows-msvc`. Archives are `.tar.gz` on every platform
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

`vex self-update` downloads from this same release, verifies the
embedded zipsign signature against the public key compiled into the
binary (`VEX_RELEASE_PUBKEY` in `src/cli/mod.rs`), and only then
extracts and replaces the running binary.

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
1. Replace `VEX_RELEASE_PUBKEY` in `src/cli/mod.rs` with the new public
   bytes and commit.
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

# 2. Bump the version in Cargo.toml.
$EDITOR Cargo.toml
cargo build --release  # so Cargo.lock matches even though it's gitignored
git commit -am "chore: bump version to X.Y.Z"

# 3. Tag and push.
git tag vX.Y.Z
git push origin main
git push origin vX.Y.Z
```

`git-cliff` will read commits between the previous tag and `vX.Y.Z` to
build the GitHub release body, so make sure your commit messages
follow the conventional-commit prefixes (`feat`, `fix`, `docs`,
`chore`, etc.). The `chore: bump version` and `docs: prepare vX
release notes` commits are filtered out of the auto-generated body —
see `cliff.toml`.

## Internal format versions

Some on-disk format versions live as constants in source rather than in
the v6 binary header — bump them when their backing shape changes,
otherwise stale entries on user machines will deserialize into the wrong
struct.

- `CACHE_FORMAT_VERSION` (`src/index/parse_cache/mod.rs`, u16) —
  bump on any structural change to `ParsedFile` or any of its
  transitively serialized members (`ParsedSymbol`, `ParsedRef`,
  `RawCallEdge`, `BoundRef`, `BindTarget`, `RefKind`, `UsePath`,
  `Skeleton`, `SymbolKind`). New variant on a serialized enum,
  field added or removed on a serialized struct, changed `repr` on
  a `#[repr(u8)]` enum — all qualify. The blob cache treats a
  version mismatch as a miss and overwrites lazily, so missing a
  bump only costs cache invalidation work on the next user run,
  not a correctness incident — but the bump is still cheap
  insurance.

The v6 binary index, by contrast, carries grammar fingerprints inline
and self-invalidates without a manual bump — see
`src/store/pattern_skeletons.rs`.
