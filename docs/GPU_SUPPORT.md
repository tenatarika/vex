# Optional GPU Support for Semantic Indexing

> Status: **IMPLEMENTED** (branch `feat/optional-gpu-embedding`). Verified:
> CPU `cargo check`/`clippy -D warnings`/`fmt`, `cargo check --features
> gpu-directml` (Windows EP path), 7 `Device` unit tests + 5 CLI tests green,
> MCP descriptor snapshot updated. GPU *runtime* (does the EP register on real
> hardware?) is still pending the §8 pre-release validation.
> Scope: GPU acceleration of the **ONNX embedding** step only. Parsing, BM25,
> HNSW build, and call-graph extraction are **not** GPU targets (see §9).

## 1. Why (and why not)

`vex index --semantic` generates one 384-dim MiniLM-L6-v2 embedding per symbol
via `fastembed 5.14 → ort 2.0.0-rc.12` (ONNX Runtime). Today the model runs
**CPU-only** — `MiniLMEmbedder::new()` builds `InitOptions` with no execution
provider, so ort's EP list is empty and it uses the default CPU EP
(`src/embed/minilm.rs:43-48`, verified repo-wide: zero `ExecutionProvider`
references in `src/`).

GPU is worth it **only** for cold / large-delta builds:

| Scenario | GPU benefit |
|---|---|
| First `vex index --semantic` (10k+ symbols, all cache-miss) | **Meaningful** — embed stage ~2–5× faster; total cold-build ~1.5–3× (Amdahl: parse/HNSW/IO unaccelerated) |
| Mass refactor / rename touching many symbols | Meaningful |
| Steady-state `vex update` (few/zero misses) | **Negligible** — the content-addressed embed cache means most updates embed almost nothing; an all-hit run skips the model load entirely (`src/index/pipeline/output.rs:811`) |
| Query embedding (`vex search`) | None — one short string; GPU warm-up would make it *slower*. Stays CPU (see §3 principle 2b). |

MiniLM is tiny (~22M params); per-batch host↔device transfer + per-run EP
warm-up (no persistent session) can erode the win on small miss sets. Below a
batch of misses, CPU may be faster — hence the miss-count gate (§3 principle 4).

## 2. Two layers: the binary vs the index

"Compiling with `--features gpu-*`" means building the **`vex` executable**
itself (a Cargo build-time feature) — *not* building an index. The index is
runtime data produced by `vex index`; its format and vectors are identical no
matter which device computed them, and any vex binary can read any index. A
binary becomes GPU-capable only via `cargo install vex --features gpu-cuda`
(or `gpu-directml` / `gpu-coreml`), or via the prebuilt Windows/macOS releases
we ship with the feature on (§6). `--gpu` / `--device` then choose, *at index
time*, whether that compiled-in EP is actually used.

## 3. Design principles

1. **CPU is the floor.** With no `gpu-*` feature compiled in, the EP list is
   always empty regardless of `--device` — exactly today's CPU path. A plain
   `cargo build` / `cargo install` is unchanged. (The shipped Windows/macOS
   release binaries *are* built `--features gpu-*` — see §6.)
2. **Two-layer default — the compile-time feature is the real opt-in.**
   (a) For **indexing**, `Device::resolve()` defaults to `DEFAULT_DEVICE` —
   `Auto` in a GPU-compiled binary, `Cpu` otherwise — so a prebuilt GPU binary
   accelerates cold builds with **no flag**. (b) The low-level embedder
   constructor stays **CPU-neutral** (`new()` / `make_embedder()` default
   `Cpu`), so any caller that bypasses `resolve()` — notably `vex search` query
   embedding (`cmd_search.rs:156`) — stays on CPU. The GPU decision lives only
   in `resolve()` / the index path, never in the library constructor.
3. **`Auto` is safe.** ort's `ExecutionProviderDispatch` defaults
   `error_on_failure = false` (`ort .../ep/mod.rs:160`); a failed/missing EP
   logs a warning and continues on CPU. A GPU-enabled binary on a GPU-less box
   still works. There is **no** public `is_available()` probe in this ort
   version — availability is decided at registration time (`MissingFeature` →
   warn → CPU), so the selector relies on this fallback, not a pre-check.
4. **`Auto` is miss-count-gated** so it never pays GPU warm-up for a tiny
   `vex update`. When the resolved device is `Auto`, the request is **not**
   explicit (`gpu_explicit == false`, see §5.5), and cache-misses
   `< EMBED_BATCH_SIZE` (256), embedding stays on CPU; at/above it (cold builds,
   mass refactors) it dispatches to GPU. An **explicit** `--gpu` / `--device
   <ep>` sets `gpu_explicit` and bypasses the threshold. A 0-miss update never
   loads the model at all (`output.rs:811`), so the common case is free either
   way.
5. **No fastembed fork.** fastembed re-exports `ExecutionProviderDispatch`
   (`lib.rs:69`) and `InitOptions::with_execution_providers(Vec<…>)` flows into
   `Session::builder().with_execution_providers(…)` (`text_embedding/impl.rs:84-85`).
6. **Don't touch the embed loop.** The sequential `chunks(256)` loop over a
   single `&mut` embedder (`output.rs:838-841`) is correct for single-stream GPU.

## 4. Execution-provider matrix (verified against ort 2.0.0-rc.12 / fastembed 5.14)

| EP | Builder | Cargo gate | Sidecar lib? | User SDK? | Shippable prebuilt? |
|---|---|---|---|---|---|
| **CoreML** (macOS arm64) | `ort::ep::CoreML::default().build()` | `ort/coreml` | No — linked as Apple framework | None (OS) | **Yes** (builds on `macos-latest`, no GPU runner) |
| **DirectML** (Windows, any GPU) | `ort::ep::DirectML::default().build()` | `fastembed/directml` (→ `ort/directml`) | **Yes** — the DML *provider* is statically folded into the core, but the redist `DirectML.dll` (~17.7 MB) must ship next to the exe; the in-box `System32\DirectML.dll` is too old (see §11.1) | None (driver only) | **Yes** (builds on `windows-latest`; bundle the DLL — §6) |
| **CUDA** (Linux/Win NVIDIA) | `ort::ep::CUDA::default().build()` | `ort/cuda` | **Yes** — `onnxruntime_providers_cuda.{so,dll}` + pulls cu12/cu13 CDN binary | CUDA 12/13 + cuDNN 9 | **No** — opt-in source build only |

The EP structs (`ort::ep::{CUDA,DirectML,CoreML}`) derive `Default` and expose
`.build() -> ExecutionProviderDispatch` (verified: `ort .../ep/mod.rs:319-332`
`impl_ep!` macro; structs at `cuda.rs:82`, `directml.rs:52`, `coreml.rs:67`).
`ort::ep` and `fastembed::ExecutionProviderDispatch` are the **same type**
(re-export: `fastembed lib.rs:69 → ort lib.rs:60-62 pub use super::ep::*`), so a
`Vec` built from `ort::ep::*` typechecks against fastembed's
`with_execution_providers`.

⚠ **DirectML must route through `fastembed/directml`, not raw `ort/directml`.**
fastembed applies a required DirectML session tweak (`with_memory_pattern(false)`
+ single-threaded exec) only when *its own* `directml` feature is on
(`fastembed .../text_embedding/impl.rs:77-94`). Enabling only `ort/directml`
would register the EP without the tweak and can misbehave.

Note: fastembed's own `cuda`/`coreml` story — fastembed forwards **only**
`directml`; its `cuda` feature targets **candle**, not ort, and it has no
`coreml` passthrough. So CUDA/CoreML are enabled via vex's **own direct `ort`
dependency** at the exact `=2.0.0-rc.12` pin (cargo feature unification turns it
on for fastembed's ort too — verified: `cargo tree -i ort` resolves a single
`ort v2.0.0-rc.12` node shared by fastembed and vex).

## 5. Implementation

> **Provenance note:** the code snippets in this section are the original
> design sketch and are kept for the rationale they carry; **the source is
> authoritative** where they differ. Review-driven deltas shipped after this
> was written: `default_device()` became the cfg-gated const
> `device::DEFAULT_DEVICE`; `execution_providers` / `make_embedder_with_device`
> / `MiniLMEmbedder::with_device` / `FastEmbedModel::new` gained a
> `strict: bool` parameter (strict EP registration is threaded as a parameter,
> never via in-process `set_var`); `Device::resolve` softens a stale
> `VEX_DEVICE` pinning an uncompiled EP instead of erroring;
> `resolve_embedder` allowlists `VEX_EMBEDDER` against the known-embedder
> registry; `batching`, `device`, and `extra` are `pub(crate)` modules behind
> a `pub use` facade; and `vex gpu` / `vex status` emit GPU fields under
> `--format json`.

### 5.1 `Cargo.toml` — optional dep + features (no `[features]` block exists yet)

```toml
[dependencies]
# ... existing ...
# Direct ort dep, pinned EXACTLY to fastembed's pin so cargo unifies to one
# ort/onnxruntime build. Optional: pulled in only by a gpu-* feature, so the
# default CPU build is unchanged. default-features=false mirrors fastembed.
ort = { version = "=2.0.0-rc.12", default-features = false, optional = true }

[features]
default = []
# GPU embedding execution providers. All OFF by default (per-target enabled in
# release.yml, §6). Each turns on the matching ort EP feature; gpu-directml also
# flips fastembed's directml feature so the required session tweak fires.
gpu-coreml   = ["dep:ort", "ort/coreml"]                          # macOS — shippable prebuilt
# ort/directml is implied by fastembed/directml; listed explicitly for intent
# (survives a future fastembed change that stops forwarding it).
gpu-directml = ["dep:ort", "ort/directml", "fastembed/directml"]  # Windows — shippable prebuilt
gpu-cuda     = ["dep:ort", "ort/cuda"]                            # NVIDIA — SOURCE BUILD ONLY
```

`fastembed` stays `fastembed = "5.13"` (resolves 5.14.0). It already pulls a
CPU ONNX Runtime via its default `ort-download-binaries-native-tls`, so the
direct `ort` dep adds no second runtime — only the EP code path. `default = []`
is mandatory (not just conservative): Cargo feature tables can't be
`cfg(target)`-gated, and `ort/directml` only builds on Windows, so the features
are applied **per-target in `release.yml`** (§6), never via `default`.

`crates/vex-mcp` is unaffected — it shells out to the `vex` binary (§5.6) and
does not depend on the `vex` library crate with features.

### 5.2 New file `src/embed/device.rs` — selector + resolution

```rust
//! Embedding compute device selection. CPU is always available and is the
//! library default; GPU execution providers are compiled in only behind
//! `gpu-*` features and selected for the index path via `--device` /
//! `.vex.toml` / VEX_DEVICE (see `resolve`).

use anyhow::{bail, Result};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Device {
    #[default]
    Cpu,           // force CPU (empty EP vec == legacy behavior)
    Auto,          // highest-priority compiled EP, silent CPU fallback
    Cuda,
    DirectMl,
    CoreMl,
}

/// Default device when no CLI flag, `.vex.toml`, or `VEX_DEVICE` is set.
/// A GPU-compiled binary defaults to `Auto` (silent CPU fallback if no GPU is
/// usable); a CPU-only build defaults to `Cpu` — today's behavior. This is the
/// single source of truth shared by `resolve()` and `vex status` (§5.7) so they
/// never drift.
#[cfg(any(feature = "gpu-coreml", feature = "gpu-directml", feature = "gpu-cuda"))]
pub fn default_device() -> Device { Device::Auto }
#[cfg(not(any(feature = "gpu-coreml", feature = "gpu-directml", feature = "gpu-cuda")))]
pub fn default_device() -> Device { Device::Cpu }

impl Device {
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Device::Auto,
            "cpu"       => Device::Cpu,
            "cuda" | "gpu" => Device::Cuda,
            "directml" | "dml" => Device::DirectMl,
            "coreml"    => Device::CoreMl,
            other => bail!("unknown device `{other}` (cpu|auto|cuda|directml|coreml)"),
        })
    }

    /// Resolve the device for the **index path**. Precedence (first present wins):
    ///   1. CLI `--device <x>`            (explicit EP override)
    ///   2. CLI `--gpu` / `--no-gpu`      (boolean: Auto / Cpu)
    ///   3. `.vex.toml` `device = "<x>"`
    ///   4. `.vex.toml` `gpu = true|false`
    ///   5. `VEX_DEVICE` env
    ///   6. default: `default_device()` — **Auto** in a GPU-compiled binary,
    ///      **Cpu** otherwise.
    ///
    /// MCP passes its `gpu` / `device` args straight through as the CLI flags,
    /// so it reuses this exact path — no separate resolution.
    pub fn resolve(
        cli_device: Option<&str>,
        cli_gpu: Option<bool>,
        cfg_device: Option<&str>,
        cfg_gpu: Option<bool>,
    ) -> Result<Self> {
        if let Some(v) = cli_device { return Device::parse(v); }
        if let Some(g) = cli_gpu { return Ok(if g { Device::Auto } else { Device::Cpu }); }
        if let Some(v) = cfg_device { return Device::parse(v); }
        if let Some(g) = cfg_gpu { return Ok(if g { Device::Auto } else { Device::Cpu }); }
        match std::env::var("VEX_DEVICE") {
            Ok(v) => Device::parse(&v),
            Err(_) => Ok(default_device()), // compile-time default
        }
    }
}

/// Build the ort execution-provider list for `device`. Returns an empty Vec
/// (== today's CPU path) on non-gpu builds, for `Device::Cpu`, or for `Auto`
/// when no EP is compiled in. An **explicit** non-auto request for an
/// uncompiled EP errors so the user isn't silently downgraded.
#[cfg(any(feature = "gpu-coreml", feature = "gpu-directml", feature = "gpu-cuda"))]
pub fn execution_providers(
    device: Device,
) -> Result<Vec<fastembed::ExecutionProviderDispatch>> {
    use ort::ep;
    let mut eps = Vec::new();
    match device {
        Device::Cpu => {}
        Device::Auto => {
            // Priority order; each push is a no-op-at-runtime if the EP can't
            // register (ort warns + falls back). cfg-gated to compiled EPs.
            #[cfg(feature = "gpu-cuda")]     eps.push(ep::CUDA::default().build());
            #[cfg(feature = "gpu-directml")] eps.push(ep::DirectML::default().build());
            #[cfg(feature = "gpu-coreml")]   eps.push(ep::CoreML::default().build());
        }
        Device::Cuda => {
            #[cfg(feature = "gpu-cuda")]      eps.push(ep::CUDA::default().build());
            #[cfg(not(feature = "gpu-cuda"))] bail!("vex was not built with CUDA (rebuild: cargo install vex --features gpu-cuda)");
        }
        Device::DirectMl => {
            #[cfg(feature = "gpu-directml")]      eps.push(ep::DirectML::default().build());
            #[cfg(not(feature = "gpu-directml"))] bail!("vex was not built with DirectML (rebuild --features gpu-directml)");
        }
        Device::CoreMl => {
            #[cfg(feature = "gpu-coreml")]        eps.push(ep::CoreML::default().build());
            #[cfg(not(feature = "gpu-coreml"))]   bail!("vex was not built with CoreML (rebuild --features gpu-coreml)");
        }
    }
    Ok(eps)
}

/// CPU-only build: any GPU request that isn't Auto/Cpu errors; otherwise empty
/// (legacy CPU path). This is the impl `cargo test --workspace` compiles in CI.
#[cfg(not(any(feature = "gpu-coreml", feature = "gpu-directml", feature = "gpu-cuda")))]
pub fn execution_providers(
    device: Device,
) -> Result<Vec<fastembed::ExecutionProviderDispatch>> {
    match device {
        Device::Auto | Device::Cpu => Ok(Vec::new()),
        _ => bail!("this vex build has no GPU support compiled in (rebuild with a gpu-* feature)"),
    }
}
```

### 5.3 `src/embed/minilm.rs` — chain the EPs; `new()` stays CPU-neutral

```rust
// new device-aware constructor; new() delegates with Device::Cpu (library
// floor — the Auto/GPU decision lives in the index path via Device::resolve).
pub fn with_device(device: crate::embed::device::Device) -> Result<Self> {
    let cache_dir = crate::util::config::embed_cache_dir();
    std::fs::create_dir_all(&cache_dir).with_context(...)?;

    let eps = crate::embed::device::execution_providers(device)?;
    let model = TextEmbedding::try_new(
        InitOptions::new(EmbeddingModel::AllMiniLML6V2)
            .with_cache_dir(cache_dir.clone())
            .with_show_download_progress(true)
            .with_execution_providers(eps),   // empty == today's CPU path
    )
    .context("failed to load MiniLM-L6-v2 embedding model")?;
    // ... existing integrity check unchanged ...
    Ok(Self { model })
}

pub fn new() -> Result<Self> { Self::with_device(crate::embed::device::Device::Cpu) }
```

The ONNX **bytes** are identical regardless of EP, so the integrity SHA check
(`integrity::verify_with_marker`) is unaffected.

### 5.4 `src/embed/mod.rs` — device-aware factory (default `Cpu`)

```rust
pub mod device;                       // new
pub use device::Device;               // new

// CPU-neutral default — see §3 principle 2b. Query embedding (cmd_search.rs:156)
// calls this and must stay CPU.
pub fn make_embedder(id: &str) -> Result<Box<dyn Embedder>> {
    make_embedder_with_device(id, Device::Cpu)
}
pub fn make_embedder_with_device(id: &str, device: Device) -> Result<Box<dyn Embedder>> {
    match id {
        MINILM_ID => Ok(Box::new(MiniLMEmbedder::with_device(device)?)),
        other => bail!("unknown embedder: `{other}`. Known: {}", known_embedders().join(", ")),
    }
}
```

### 5.5 CLI threading + the miss-count gate

- **`src/cli/args.rs`** — add to `index` and `update` (the embedding-producing
  commands; *not* `search`). Headline boolean + advanced enum override, all
  three mutually exclusive at the clap layer so `Device::resolve` only ever sees
  one CLI source:
  ```rust
  /// Use the GPU for embedding generation (if this build was compiled with a
  /// gpu-* feature). Falls back to CPU if no GPU is usable.
  #[arg(long, conflicts_with = "no_gpu")]
  gpu: bool,
  /// Force CPU embedding even if `gpu = true` is set in .vex.toml.
  #[arg(long)]
  no_gpu: bool,
  /// Advanced: pick a specific execution provider
  /// (cpu | auto | cuda | directml | coreml). Mutually exclusive with --gpu/--no-gpu.
  #[arg(long, conflicts_with_all = ["gpu", "no_gpu"])]
  device: Option<String>,
  ```
  The command body collapses the booleans to
  `cli_gpu = gpu.then_some(true).or(no_gpu.then_some(false))`.
- **`src/index/pipeline/mod.rs`** — add to `IndexOptions` (build-request
  carrier; **not** persisted to the manifest — device is a runtime concern,
  vectors are model-defined):
  ```rust
  pub device: crate::embed::Device,
  /// True only when the GPU was selected by an EXPLICIT CLI `--gpu` / `--device`.
  /// Bypasses the miss-count gate (§3.4). NOT set for `.vex.toml gpu = true` or
  /// `VEX_DEVICE` — config/env Auto stays gated so a tiny `vex update` still
  /// avoids GPU warm-up.
  pub gpu_explicit: bool,
  ```
- **`src/cli/common.rs`** — `build_index_options(...)` gains the resolved
  device + the explicit flag, computed from CLI args only:
  ```rust
  let device = crate::embed::Device::resolve(
      cli_device.as_deref(), cli_gpu, cfg.device.as_deref(), cfg.gpu)?;
  let gpu_explicit = cli_device.is_some() || matches!(cli_gpu, Some(true));
  ```
  `cmd_index.rs` / `cmd_update.rs` pass `cli_gpu` / `cli_device` through.
- **`src/index/pipeline/output.rs`** — `generate_embeddings` currently takes
  `(parsed, embedder_id, root)` (line 729) and is called from `mod.rs:283`
  (full index) and `mod.rs:586` (update). Thread `device: Device` and
  `gpu_explicit: bool` (or `&opts`) into it from **both** call sites, then gate
  after `let misses = miss_indices.len();` (line 801), before the model load at
  line 823:
  ```rust
  let effective_device = if device == Device::Auto && !gpu_explicit
      && misses < EMBED_BATCH_SIZE {
      Device::Cpu                       // tiny update: skip GPU warm-up
  } else {
      device
  };
  // line 823:
  let mut embedder = embed::make_embedder_with_device(embedder_id, effective_device)?;
  ```
  `EMBED_BATCH_SIZE` is already in scope (`output.rs:27`). The 0-miss early
  return (`output.rs:811`) already precedes this, so the gate only matters for
  1..256 misses.
- **`.vex.toml`** (`src/util/config.rs` `VexConfig`) — add `pub gpu:
  Option<bool>` and `pub device: Option<String>`. `Option<T>` deserializes to
  `None` when absent even under the struct's `#[serde(deny_unknown_fields)]`
  (proven by the existing bare `Option` fields like `semantic`), so **no
  backward-compat break** and no `#[serde(default)]` needed. Append commented
  examples to `DEFAULT_CONFIG`:
  ```toml
  # gpu = false        # use the GPU for embedding if this build supports it
  # device = "auto"    # advanced: cpu | auto | cuda | directml | coreml
  ```
- **`VEX_DEVICE`** env — handled in `Device::resolve` (rank 5, before the
  compile-time default).

### 5.6 MCP tool surface (`crates/vex-mcp/src/main.rs`)

The MCP server is a thin shell-out wrapper: `build_command` maps JSON tool args
→ `vex` CLI flags. The existing `opt_bool(args, "gpu", false)` cannot distinguish
**absent** from **explicit `false`**, so it could never forward `--no-gpu` to
override `.vex.toml gpu = true`. Add a tri-state helper mirroring the existing
`opt_u64_some` (`main.rs:97`) / `opt_str` (`main.rs:55`):

```rust
/// Optional bool that distinguishes absent/null from an explicit value.
fn opt_bool_some(args: &Value, field: &str) -> Result<Option<bool>> {
    let v = &args[field];
    if v.is_null() { return Ok(None); }
    Some(v.as_bool().ok_or_else(|| ParamError::wrong_type(field, "a boolean", v)))
        .transpose().map_err(Into::into)
}
```

In the `index` / `update` arms (`main.rs:901-916`), forward tri-state:

```rust
match opt_bool_some(args, "gpu")? {
    Some(true)  => extra.push("--gpu".into()),
    Some(false) => extra.push("--no-gpu".into()),
    None        => {} // absent: let .vex.toml gpu / VEX_DEVICE decide via Device::resolve
}
if let Some(dev) = opt_str(args, "device")? {   // advanced, optional
    extra.extend(["--device".into(), dev.to_string()]);
}
```

Tool descriptors (≈ lines 1364-1387) — add to `index` and `update`
`properties` (note: **no** `"default"`, since absence is meaningful):

```json
"gpu": { "type": "boolean", "description": "Use the GPU for embedding generation if this vex build supports it (CPU fallback otherwise). Omit to let .vex.toml `gpu`/`device` or VEX_DEVICE decide; pass false to force CPU even when config enables GPU. Only affects cold/large semantic builds." }
```

Resolution semantics (correct prose for the doc): absent `gpu` → config /
`VEX_DEVICE` decide; `gpu:false` → forwards `--no-gpu` (overrides config
`gpu=true`); `gpu:true` → forwards `--gpu`. Because MCP just emits CLI flags it
reuses `Device::resolve` — no duplicate logic. Re-accept the
`vex_mcp__tests__tool_descriptors.snap` insta snapshot after the change (§5.8).

### 5.7 `vex status` (`src/cli/cmd_status.rs`)

The status line must reflect the **resolved default**, not a hardcoded `cpu`
(otherwise a GPU prebuilt that runs Auto would print "cpu" and mislead). Print
two compile-time-honest facts after the `Embeddings:` line:

```rust
println!("GPU support: {}", gpu_support_str());      // "yes (DirectML)" | "no (none compiled)"
println!("Default dev: {:?}", crate::embed::device::default_device()); // Cpu | Auto
```

with the suffix helper defined here:

```rust
/// Compile-time feature suffix for `vex status`. Lists the EP(s) baked in.
const fn gpu_support_str() -> &'static str {
    // If multiple EPs can co-compile, prefer listing all; first-match shown here.
    if cfg!(feature = "gpu-directml") { "yes (DirectML)" }
    else if cfg!(feature = "gpu-coreml") { "yes (CoreML)" }
    else if cfg!(feature = "gpu-cuda") { "yes (CUDA)" }
    else { "no (none compiled)" }
}
```

`Default dev` shares `default_device()` with `Device::resolve` so they can't
drift. Note in the help/docs that this is the *baseline* — the device an actual
`vex index` uses still depends on `--gpu` / `--device` / `.vex.toml` /
`VEX_DEVICE` at call time. True runtime-EP introspection ("did DirectML actually
register?") needs ort session-level work — there is no `is_available()` probe in
ort `=2.0.0-rc.12` (§3 principle 3) — so it is deferred to a §7 follow-up.

### 5.8 Tests

The project gates CI on `cargo clippy -D warnings` and `cargo test --workspace`
across three OSes (`ci.yml`), and uses `assert_cmd` + `insta`. Add:

1. **`Device::parse`** unit tests: each variant (`"auto"`/`""`→Auto, `"cpu"`→Cpu,
   `"cuda"`/`"gpu"`→Cuda, `"directml"`/`"dml"`→DirectMl, `"coreml"`→CoreMl) +
   an invalid string → `is_err()` whose message lists the options. Parameterize
   with `rstest`.
2. **`Device::resolve`** precedence table (parameterized): `cli_device` >
   `cli_gpu` > `cfg_device` > `cfg_gpu` > `VEX_DEVICE` > `default_device()`.
   Cover `cli_gpu=Some(true)`→Auto / `Some(false)`→Cpu, cfg fallthrough when CLI
   is None, and env consulted only when all four args are None. Serialize the
   `VEX_DEVICE` cases (process-global env) or save/restore.
3. **`execution_providers`** on the default CPU build (what CI compiles): returns
   an empty `Vec` for `Cpu` and `Auto`, and `bail!`s for explicit `Cuda` /
   `DirectMl` / `CoreMl`. The per-EP `#[cfg(not(...))]` bail arms only compile
   under partial gpu-feature combos — cover those via the `cargo check
   --features gpu-*` compile-guards (§6), not a runtime test.
4. **CLI** (`tests/cli_device_test.rs`, mirroring `cli_status_coverage_test.rs`):
   assert `vex index --gpu --no-gpu` (and `--gpu --device cpu`) is rejected by
   clap (non-zero exit, stderr mentions the conflict), and that `vex status`
   prints the new `GPU support:` / `Default dev:` lines.
5. **MCP snapshot**: after adding the `gpu`/`device` descriptor properties, run
   `cargo insta review` (or `cargo test` then accept) in `crates/vex-mcp` to
   re-pin `vex_mcp__tests__tool_descriptors.snap`.

## 6. Distribution & CI

**Decision (chosen): bake GPU into the standard prebuilt Windows/macOS
binaries.** Those release binaries are built with the EP compiled in, so
`DEFAULT_DEVICE` is `Auto` and a `brew install` / release-download user
gets cold-build acceleration with **no compilation and no SDK**. Linux prebuilt
stays CPU (no vendor-agnostic GPU EP; CUDA needs a host SDK). CUDA remains
source-build-only on every platform.

`release.yml` — `build` job, per-target feature flags:

```yaml
matrix:
  include:
    - target: aarch64-apple-darwin
      os: macos-latest
      features: "gpu-coreml"        # NEW
    - target: x86_64-unknown-linux-gnu
      os: ubuntu-latest
      features: ""                  # CPU
    - target: x86_64-pc-windows-msvc
      os: windows-latest
      features: "gpu-directml"      # NEW
# Build step (current release.yml:60 has no features; extend it). matrix.features
# is "" on the Linux leg; empty string is FALSY in GHA expressions, so the
# `|| ''` branch correctly yields no --features. The `${{ }}` is substituted by
# the runner before any shell runs, so this works on the bash and pwsh legs
# alike with no shell pinning.
- run: cargo build --release --workspace --target ${{ matrix.target }} ${{ matrix.features && format('--features {0}', matrix.features) || '' }}
```

**Windows must also bundle `DirectML.dll`** (§11.1): ORT downloads the modern
redist DLL into its binary cache during `cargo build` but does *not* copy it
beside the exe, and the in-box `System32` copy is too old. `release.yml` adds a
*Stage DirectML.dll* step that selects the cached DLL in
`%LOCALAPPDATA%\ort.pyke.io` **by pinned SHA-256** and copies it into the
release dir, then packs it **into the `vex` tarball only**
(`vex.exe DirectML.dll`) — not `vex-mcp` (it spawns `vex` as a subprocess and
never loads ORT). A smoke-test asserts the DLL is in the archive.
**Self-update:** `vex self-update` (releases after v1.16.0) extracts the
**whole** archive and installs every non-binary file as a sidecar beside the
exe — `DirectML.dll` included (`src/cli/self_update_flow.rs`). The DLL is
SHA-256-compared first and skipped when byte-identical (the common case; it
only changes on an ort bump), and a missing DLL — an install degraded by an
older updater, which extracted only the named binary — is healed on the next
self-update. Sidecars install *before* the binary swap: a failed DLL write
(e.g. unelevated under `C:\Program Files\vex\`) aborts the update with the
old exe + DLL pair intact, never leaving exe↔DLL version skew. One-release
lag: a user *running* a v1.16.0-or-older binary still updates with the old
extract-binary-only code on that hop; the heal kicks in from the next update
onward. The degraded state is also *visible* now: a DirectML build that can't
find the DLL beside the exe prints an unconditional stderr warning at
embedding init (with a `vex self-update` hint) instead of falling back to CPU
silently. macOS CoreML needs no sidecar (system framework); Linux is CPU-only.

**DirectML.dll supply-chain pin.** The staging step never trusts whatever it
finds in the runner's ort cache: every candidate `DirectML.dll` is hashed and
only a file whose SHA-256 equals the pinned constant — the x64 DLL from the
official `Microsoft.AI.DirectML` NuGet package version that ort
(`=2.0.0-rc.12` → DirectML **1.15.4**) stages — is packed into the signed
tarball; otherwise the release fails closed, listing the candidates and their
hashes. A poisoned runner cache, a compromised transitive dependency, or two
ort versions left in the cache by a dirty build therefore cannot substitute a
different DLL silently. On an ort upgrade that bumps DirectML, recompute the
pin from the matching NuGet package on nuget.org (never from the cache alone)
and update the `DIRECTML_DLL_SHA256` constant in `release.yml`.

**DirectML.dll side-loading (install-location hardening).** `vex.exe` loads
`DirectML.dll` from its own directory via the standard Windows DLL search
order. That is the point (it must shadow the stale `System32` copy), but it
also means **any install location writable by a lower-privileged process lets
that process swap the DLL and gain code execution inside vex on the next
run**. This is inherent to redistributing DirectML and cannot be fully closed
without load-time signature verification. Mitigation: install vex in a
directory with restricted write permissions — e.g. `C:\Program Files\vex\`
(admin-writable only) — rather than a user-writable location like
`%LOCALAPPDATA%\Programs\vex\` or a folder on a shared drive. The same advice
applies to any tool that ships sidecar DLLs. Scope of the exposure: vex runs
with the invoking user's privileges (no elevation), so a swap by *that same
user's* malware adds persistence rather than new privilege — but when the
writer is a **different** principal (another user on a shared drive, a
constrained service account), the swap is horizontal privilege escalation
into the invoking user's context. Trade-off to know about: installing under
`C:\Program Files\` means `vex self-update` needs an elevated shell to
replace the binary in place.

`release.yml` Homebrew `install` block — the `--HEAD` (source) branch currently
passes no features (`release.yml:298-304`), so `brew install vex --HEAD` would
be CPU-only while the bottle is GPU. Match them on macOS:

```ruby
def install
  if build.head?
    args = std_cargo_args
    args += ["--features", "gpu-coreml"] if OS.mac?
    system "cargo", "install", *args
  else
    bin.install "vex"
  end
end
```
Linux `--HEAD` deliberately stays CPU (no vendor-agnostic EP), matching the §9
platform table.

`ci.yml` — add a standalone `check-gpu` job (the real `ci.yml` has a single
matrix `test` job with `if:`-guarded steps, **not** per-OS legs, so a separate
job mirroring the `build` matrix is the clean home). `cargo check` only — CI
runners have no GPU, so this verifies the feature **compiles**; it cannot (and
must not) measure GPU perf:

```yaml
check-gpu:
  strategy:
    fail-fast: false
    matrix:
      include:
        - os: macos-latest
          features: gpu-coreml
        - os: windows-latest
          features: gpu-directml
        # gpu-cuda: no CUDA toolkit on stock runners → source-build-only
  runs-on: ${{ matrix.os }}
  steps:
    - uses: actions/checkout@v4
    - uses: dtolnay/rust-toolchain@stable
    - uses: Swatinem/rust-cache@v2
    - run: cargo check --workspace --features ${{ matrix.features }}
```

The `test` job stays CPU (no `--features`) — GPU EPs may be unavailable on CI
runners and silent fallback would mask nothing useful; keep tests deterministic.
**Known CI gap:** the shipped binary's GPU *runtime* path is never exercised in
CI (runners lack GPUs) — only its compilation is. The §8 pre-release validation
covers the runtime side manually.

Binary-size / OOM caution: the repo already caps test debug-info to fit the
~14 GB Linux runner and moved Homebrew to prebuilt tarballs because the source
build OOM-killed clang (`Cargo.toml:24-35`, `release.yml:216-222`). The
DirectML/CoreML provider already ships inside the pyke ORT static lib, so the
increase is the EP-registration code path, not a second runtime — but watch the
Win/macOS legs for size/time regressions. Linux is unaffected.

## 7. Caveats to document for users

- **GPU is on by default in the Windows/macOS release binaries** (`Auto`). It
  helps cold/mass builds; **incremental `vex update` sees ~no change** (cache +
  miss-gate). Document the escape hatch prominently: `--no-gpu`, or
  `gpu = false` in `.vex.toml`, or `VEX_DEVICE=cpu`.
- Default `Auto` reaching every Win/macOS user means an EP/driver bug has wide
  blast radius. Mitigations: ort's silent CPU fallback, the miss-gate (no GPU
  for tiny updates), and the documented off switch. The §8 pre-release
  validation is the real guard.
- Switching CPU↔GPU does **not** invalidate the embed cache (key is
  `embedder_id + context`, not device), so toggling won't re-embed existing
  symbols — it engages only for new/changed ones. CPU vs GPU vectors differ by
  tiny float noise; benign for cosine ranking.
- `ort`/`fastembed` are at RC versions; keep the `=2.0.0-rc.12` pin in lockstep
  with fastembed. **On every fastembed upgrade**, run `cargo update -p fastembed
  && cargo tree -i ort` and confirm a single `ort` node at one version. Two
  versions ⇒ vex's `=` pin drifted from fastembed's internal pin — bump vex's
  `ort` to match so they re-unify to one ONNX Runtime build.
- **"Is GPU actually engaged?"** — `vex gpu` answers this directly: it builds a
  MiniLM session on the compiled EP with strict registration and runs one
  inference, reporting OK/FAILED + targeted remediation (it cannot use ort's
  absent `is_available()`, so it probes by actually loading). `vex status` still
  reports only compiled support + the baseline default.
- **ONNX model downloads are not secondarily SHA-pinned (residual risk).** The
  default MiniLM model has a vex-side pinned-SHA-256 integrity check
  (`src/embed/integrity.rs`). The opt-in heavier embedders (`jina-code`,
  `bge-*`, `mxbai-large`) do **not** carry a second vex-side pin: fastembed
  verifies its own downloads against Hugging Face Hub metadata, and vex trusts
  that. A compromised HF repo or CDN that also controls the metadata could
  therefore serve a tampered model for the opt-in embedders. Accepted residual
  risk for a developer tool (the blast radius is embedding vectors, not code
  execution — ONNX graphs are data to ONNX Runtime); revisit if vex ever runs
  in a trust-sensitive pipeline. Mitigation today: stick to the default
  embedder, or pre-seed the embed cache from a vetted mirror.

## 8. Pre-release validation (manual — CI can't do this)

Before shipping a GPU-enabled release: benchmark a cold ~10k-symbol
`vex index --semantic` (CPU vs DirectML on Windows, CPU vs CoreML on macOS) to
confirm a real speedup, and smoke-test on a clean runner-class machine (no dev
GPU drivers) that the EP actually registers — or cleanly falls back. Because
default `Auto` reaches every Win/macOS user, this gate is the real safety net,
not CI (runners have no GPU; any timing there measures CPU-fallback and is
meaningless).

## 9. Explicitly out of scope (not GPU-amenable)

Parsing (tree-sitter, branchy automaton), BM25 (sort + FST insert), call-graph
(hash-join), content hashing (bandwidth-bound xxh3), and HNSW build (`usearch
2.25`, pure-CPU C++; no CUDA backend). Replacing usearch with a GPU ANN lib
(cuVS/CAGRA) loses to CPU HNSW at vex's vector counts and adds a CUDA runtime
dep. Do not pursue.

## 10. Platform summary

| Platform | Prebuilt release binary | Default device | No-compile GPU? |
|---|---|---|---|
| Windows x86_64 | built `--features gpu-directml` | Auto (DirectML, any GPU vendor) | **Yes** |
| macOS arm64 | built `--features gpu-coreml` | Auto (CoreML) | **Yes** |
| Linux x86_64 | CPU (unchanged) | CPU | No — `cargo install --features gpu-cuda` for NVIDIA |
| Any (NVIDIA/CUDA) | n/a | — | Source build only: `cargo install vex --features gpu-cuda` (host CUDA 12/13 + cuDNN 9) |

---

## 11. Real-world validation (deep-source, RTX 3080) — supersedes earlier estimates

Benchmarked on a Windows **console** session (RTX 3080, current driver) against a
large C++ codebase. This section **corrects two wrong conclusions from earlier
drafts** (flagged inline); the §6 decision to bake DirectML into the Windows
prebuilt is **confirmed**.

### 11.1 DirectML works — the earlier "doesn't work here" was a stale DLL, not RDP

An earlier draft reported the DirectML EP hard-erroring under `VEX_GPU_STRICT=1`:

```
dml_provider_factory.cc 887A0004 (DXGI_ERROR_UNSUPPORTED)
"device interface or feature level not supported on this system"
```

and blamed it on no D3D12 device (RDP/headless). **That was misdiagnosed.** The
session is a local console with the 3080 as the active adapter; the real cause is
a **DirectML version mismatch**. ORT downloads a modern redistributable
`DirectML.dll` (~17.7 MB) into its binary cache, but its `copy-dylibs` step does
**not** place it next to the exe — so vex loaded the stale **in-box
`C:\Windows\System32\DirectML.dll`** (~1.2 MB, DirectML 1.0-era from Win10 1903),
whose feature level is too old for current ORT → `DMLCreateDevice` fails
`887A0004` and ORT silently falls back to CPU.

**Fix:** ship the redist `DirectML.dll` next to `vex.exe`. With it present,
DirectML engages on the 3080 (verified with `VEX_GPU_STRICT=1`, exit 0). The
release pipeline now stages this DLL from the ort cache into the Windows `vex`
tarball (§6). `VEX_GPU_STRICT` remains the way to confirm engagement, and the
new `vex gpu` doctor command productizes it — it probes the compiled EP and
prints exactly this remediation on failure.

### 11.2 Benchmark: CPU vs DirectML vs CUDA (CommonLib, 27,997 symbols)

Embed time (the device-dependent part; GPU engagement strict-verified, fresh
embed cache so every symbol is a real miss):

| Model | CPU | DirectML | CUDA | DirectML/CPU | CUDA/CPU |
|---|---|---|---|---|---|
| `minilm-l6-v2` (384-d, default) | 473 s | 16.2 s | 9.2 s | **29×** | **51×** |
| `jina-code` (768-d code model) | ~30–60 min *(proj.)* | 54.2 s | 30.9 s | ~40–70× | ~60–120× |

Throughput (sym/s): minilm — CPU 59 · DirectML 1,728 · CUDA 3,043 | jina —
DirectML 517 · CUDA 906 · CPU ~8–12. One-time model-load warm-up: minilm ~10 s,
jina ~31–38 s. (`jina/cpu` not run — >30 min on 28k symbols; projected.)

Two findings:

- **GPU helps even the default MiniLM on a full/cold index** — 51× (CUDA) / 29×
  (DirectML) on embed. An earlier claim that "MiniLM is too small to benefit from
  GPU" is true **only for tiny incremental updates**, where the ~10 s warm-up
  dominates — which the §3.4 miss-gate already handles (MiniLM gate is now 512
  misses, ≈ the measured CPU-vs-GPU break-even). For a full index, GPU is a large
  win for every model.
- **DirectML is a consistent ~57% of CUDA throughput** (1,728/3,043 = 0.57;
  517/906 = 0.57) — CUDA is ~1.75× faster, but DirectML is **driver-only** (no
  NVIDIA SDK, any DX12 GPU vendor). This is precisely why the Windows prebuilt
  bakes DirectML (broad, zero-install GPU) and CUDA stays the source-build
  power-user path for the extra 1.75×.

CUDA setup (one-time, source build): the cu12 ORT provider DLLs
(`onnxruntime_providers_{shared,cuda}.dll`) must sit next to the binary, and the
CUDA 12 runtime + **cuDNN 9** must be on PATH — `cudart64_12` / `cublas64_12` /
`cufft64_11` / `cudnn64_9` (`pip install nvidia-cudnn-cu12` is the quickest cuDNN
source). The NVIDIA *driver alone is insufficient*: it ships only `nvcuda.dll`
(the driver API), not the CUDA runtime or cuDNN — which is another reason
DirectML (driver-only) is the better zero-install default.

### 11.3 The full-repo slowness was padding waste — fixed by length-aware batching

Naive fixed-256 batching in symbol order was ~14× slower on the full repo (394s)
because fastembed pads each batch to its longest sequence, so a batch with a few
long C++ symbols processed all 256 at max length. [`src/embed/batching.rs`]
sorts contexts by length and sizes each inference batch from the actual lengths
(`count × max_len² ≤ budget`, `count ≤ 256`). Result: **80,269 syms in 29.3s**,
zero config.

### 11.4 VRAM: ORT's arena reserves ~all free memory; a hard cap OOMs

Measured peak VRAM for the full cold index is **~11 GB regardless of batch
size**. ORT's CUDA BFC arena reserves close to all free VRAM and does not
shrink. A hard `gpu_mem_limit` below that natural peak **fragments and OOMs**
(`BFCArena ... Available memory 0 < requested N`) — observed at 2 GB and 4 GB
caps — so it is **not** a usable bound; batch size doesn't change it either.

Decision: **no memory cap by default** (never OOMs). The high VRAM is
**transient** (released after indexing) and only occurs on a *large cold*
`vex index --semantic`; steady-state `vex update` embeds few symbols and stays
light, and the §3.4 miss-gate keeps tiny updates on CPU. Escape hatches for a
contended shared GPU:
- `--no-gpu` (or `gpu = false`) for the one-off cold build, then GPU for updates.
- `VEX_GPU_MEM_LIMIT=<bytes>` — **advanced opt-in** hard cap; set it generously
  (≥ the working set) or it will OOM on long-context batches.

Truly bounding VRAM low would require bucketed fixed-shape batching
(inference-serving-grade), a larger rework with quality/speed tradeoffs — out of
scope for now.

### 11.5 Diagnostics shipped

- **`vex gpu`** — doctor command: probes the compiled EP (strict, one real
  inference), reports OK/FAILED + EP-specific remediation, and `--enable` pins
  `VEX_DEVICE`. The supported way to confirm GPU engagement.
- `VEX_GPU_STRICT=1` — turn ORT's silent EP-registration fallback into a hard
  error (proves whether the GPU actually engaged). `vex gpu` requests the same
  strict registration internally via a constructor parameter — it does **not**
  mutate the environment (in-process `set_var` races concurrent `getenv` from
  live Rayon workers, which is UB).
- `tracing` `device=…` line at model load shows the selected device.
- `VEX_GPU_MEM_LIMIT`, `VEX_GPU_ATTN_BUDGET`, `VEX_DEVICE`, `VEX_EMBEDDER` env
  overrides.
