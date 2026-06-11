// `batching`, `device`, and `extra` are crate-internal: their signatures carry
// transitive-dep types (`fastembed::TextEmbedding`, ort dispatch values) that
// must not become part of the library surface. External consumers (the
// integration tests) get what they need via the `pub use` facade below.
pub(crate) mod batching;
pub mod cache;
pub(crate) mod device;
pub(crate) mod extra;
pub mod integrity;
pub mod minilm;
pub mod model;
pub mod tokenizer;

use anyhow::{bail, Result};

pub use device::Device;
pub use minilm::{MiniLMEmbedder, MINILM_CHAR_BUDGET, MINILM_DIM, MINILM_ID};
pub use model::build_context;

/// Stable identifier of the default embedder. Persisted in the manifest of
/// pre-9.1 indexes that did not record an `embedder_id` field — readers
/// interpret a missing field as this value for back-compat.
pub const DEFAULT_EMBEDDER: &str = MINILM_ID;

/// Pluggable embedding-model abstraction. Implementations wrap an underlying
/// ONNX (or future) model and produce a single dense `f32` vector per input
/// string. The dimension is fixed per implementation and stored in the index
/// Header (`vector_dim`); the `id` is stored in the manifest so search can
/// detect when the requested embedder differs from the one used to build the
/// index.
pub trait Embedder: Send {
    /// Stable identifier (e.g. `"minilm-l6-v2"`). Used for manifest storage
    /// and mismatch detection — must remain stable across releases.
    fn id(&self) -> &'static str;

    /// Output vector dimension. Must match what the index Header records.
    fn dim(&self) -> u32;

    /// Character budget for the context string passed to [`embed`]. Callers
    /// (e.g. `build_context`) truncate to this size to fit the model's
    /// token-window. Varies per model.
    ///
    /// As of v1.13 E2b the bin no longer calls this through the trait —
    /// `embedder_char_budget(id)` reads the per-embedder const directly
    /// so we can pre-build contexts without instantiating the model.
    /// The trait method stays for downstream embedders that need
    /// per-instance variability.
    #[allow(dead_code)]
    fn char_budget(&self) -> usize;

    /// Embed a single string.
    fn embed(&mut self, text: &str) -> Result<Vec<f32>>;

    /// Embed a batch of strings. Implementations should use the underlying
    /// model's batching APIs to amortise overhead.
    fn embed_batch(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

/// Construct an embedder by its stable identifier.
///
/// Errors when `id` is not a known embedder. The error message lists all
/// known IDs so callers can show them to the user.
pub fn make_embedder(id: &str) -> Result<Box<dyn Embedder>> {
    // CPU-neutral default — see docs/GPU_SUPPORT.md §3 principle 2b. Callers that
    // want GPU (the index path) go through `make_embedder_with_device` with a
    // device resolved by `Device::resolve`; bare callers (e.g. `vex search`
    // query embedding) stay on CPU via `MiniLMEmbedder::new()`.
    match id {
        MINILM_ID => Ok(Box::new(MiniLMEmbedder::new()?)),
        other => match extra::spec_for(other) {
            Some(spec) => Ok(Box::new(extra::FastEmbedModel::new(
                spec,
                Device::Cpu,
                false,
            )?)),
            None => bail!(
                "unknown embedder: `{other}`. Known embedders: {}",
                known_embedders().join(", ")
            ),
        },
    }
}

/// Construct an embedder by ID on a specific compute [`Device`]. Used by the
/// index path, which resolves the device from CLI/config/env via
/// [`Device::resolve`]. On a CPU-only build a non-CPU/Auto device errors (see
/// `device::execution_providers`).
///
/// `strict` makes a failed EP registration a hard error instead of ORT's
/// silent CPU fallback. The index path passes `false` (graceful fallback); the
/// `vex gpu` probe passes `true` so "engaged" vs "fell back" is observable.
pub fn make_embedder_with_device(
    id: &str,
    device: Device,
    strict: bool,
) -> Result<Box<dyn Embedder>> {
    match id {
        MINILM_ID => Ok(Box::new(MiniLMEmbedder::with_device(device, strict)?)),
        other => match extra::spec_for(other) {
            Some(spec) => Ok(Box::new(extra::FastEmbedModel::new(spec, device, strict)?)),
            None => bail!(
                "unknown embedder: `{other}`. Known embedders: {}",
                known_embedders().join(", ")
            ),
        },
    }
}

/// Vector dimension of a known embedder by ID, without instantiating the
/// model. Returns `None` for unknown IDs. Used by the writer to record
/// `Header.vector_dim` even when no symbols are present, and by tests.
pub fn embedder_dim(id: &str) -> Option<u32> {
    match id {
        MINILM_ID => Some(MINILM_DIM),
        _ => extra::spec_for(id).map(|s| s.dim),
    }
}

/// Character budget for [`build_context`] without instantiating the
/// model. v1.13 E2b: lets the embedding-cache pre-build all contexts
/// (and check the cache) before deciding whether the ONNX model is
/// even needed — when every context hits the cache we skip the
/// ~80 MB model load entirely.
pub fn embedder_char_budget(id: &str) -> Option<usize> {
    match id {
        MINILM_ID => Some(MINILM_CHAR_BUDGET),
        _ => extra::spec_for(id).map(|s| s.char_budget),
    }
}

/// Miss-count threshold below which `Device::Auto` stays on CPU for `id` — the
/// GPU warm-up isn't worth it for a tiny `vex update`. Scales inversely with
/// model size (heavier model → GPU pays off at fewer misses). Unknown ids fall
/// back to the MiniLM value. Used by the index pipeline's device gate
/// (`docs/GPU_SUPPORT.md` §3.4); an explicit `--gpu`/`--device` bypasses it.
pub fn embedder_gpu_auto_min_misses(id: &str) -> usize {
    match id {
        MINILM_ID => minilm::MINILM_GPU_AUTO_MIN_MISSES,
        _ => extra::spec_for(id)
            .map(|s| s.gpu_auto_min_misses)
            .unwrap_or(minilm::MINILM_GPU_AUTO_MIN_MISSES),
    }
}

/// List of known embedder IDs in registry order. Used in error messages and
/// for the `--embedder` CLI help.
pub fn known_embedders() -> Vec<&'static str> {
    let mut ids = vec![MINILM_ID];
    ids.extend(extra::SPECS.iter().map(|s| s.id));
    ids
}

/// Resolve the embedder ID to use for an operation.
///
/// Priority: `cli` (e.g. `--embedder` flag) > `config` (`.vex.toml`) >
/// `VEX_EMBEDDER` env > [`DEFAULT_EMBEDDER`]. The env var is a low-precedence
/// fallback (any project can still override it), so it serves as a *global*
/// default embedder across all projects — mirroring `VEX_DEVICE` for the
/// compute device. Returns an owned `String` since the sources supply distinct
/// lifetimes.
///
/// The return is always a *known* embedder id when it came from the env:
/// `VEX_EMBEDDER` is ambient state, not an explicit per-run request, so an
/// unknown value degrades to [`DEFAULT_EMBEDDER`] with a warning instead of
/// failing every command — mirroring `downgrade_uncompiled_env_device` for
/// `VEX_DEVICE`. CLI / config ids pass through verbatim: an explicit request
/// must error loudly in [`make_embedder`] rather than be silently rewritten.
pub fn resolve_embedder(cli: Option<&str>, config: Option<&str>) -> String {
    if let Some(id) = cli {
        return id.to_string();
    }
    if let Some(id) = config {
        return id.to_string();
    }
    if let Ok(env) = std::env::var("VEX_EMBEDDER") {
        let trimmed = env.trim();
        if !trimmed.is_empty() {
            if known_embedders().contains(&trimmed) {
                return trimmed.to_string();
            }
            tracing::warn!(
                embedder = trimmed,
                fallback = DEFAULT_EMBEDDER,
                known = known_embedders().join(", "),
                "VEX_EMBEDDER names an unknown embedder; falling back to the default"
            );
        }
    }
    DEFAULT_EMBEDDER.to_string()
}

/// Verify that the embedder the caller intends to use matches the one
/// recorded in the index manifest.
///
/// `manifest_embedder` is what the index was built with — `None` means the
/// manifest predates Phase 9.1 (no `embedder_id` field), in which case we
/// fall back to [`DEFAULT_EMBEDDER`] for back-compat.
///
/// `requested` is the embedder resolved from CLI/config/default (see
/// [`resolve_embedder`]). When this comes from `.vex.toml` rather than an
/// explicit CLI flag, the user may not have realised they are asking for a
/// different model than the index was built with — the error message says
/// "configured" to make the source clearer.
///
/// **Precondition**: callers should only invoke this when
/// `IndexReader::has_vectors()` is true. A vectorless index has nothing to
/// match against and `None == DEFAULT` would silently pass.
///
/// Returns `Err` with a rebuild hint when stored and requested differ.
pub fn check_embedder_match(manifest_embedder: Option<&str>, requested: &str) -> Result<()> {
    let stored = manifest_embedder.unwrap_or(DEFAULT_EMBEDDER);
    if stored != requested {
        bail!(
            "embedder mismatch: index was built with `{stored}` but \
             this run is configured to use `{requested}`. \
             Either rebuild with `vex index --semantic --embedder {stored}`, \
             or change `embedder` in `.vex.toml` (or pass `--embedder {stored}`)."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serial_test::serial;

    use super::*;

    /// Restores a `VEX_EMBEDDER` env mutation on Drop — including unwind. Mirrors
    /// `VexDeviceGuard` in `embed::device`; without it a panic between any
    /// `set_var` and the trailing `remove_var` left `VEX_EMBEDDER` polluted for
    /// the next `#[serial]` test (the lock orders, doesn't unwind their env).
    struct VexEmbedderGuard {
        prior: Option<std::ffi::OsString>,
    }

    impl VexEmbedderGuard {
        fn capture() -> Self {
            Self {
                prior: std::env::var_os("VEX_EMBEDDER"),
            }
        }
    }

    impl Drop for VexEmbedderGuard {
        fn drop(&mut self) {
            // SAFETY (Rust 1.80+ marks set_var/remove_var unsafe under
            // edition-2024): only called from a `#[serial]` test holding the
            // `serial_test` global lock — no concurrent `getenv` can race.
            match &self.prior {
                Some(v) => std::env::set_var("VEX_EMBEDDER", v),
                None => std::env::remove_var("VEX_EMBEDDER"),
            }
        }
    }

    #[test]
    fn registry_lookups_cover_minilm_and_extras() {
        // MiniLM (default) is always known.
        assert_eq!(embedder_dim(MINILM_ID), Some(MINILM_DIM));
        assert_eq!(embedder_char_budget(MINILM_ID), Some(MINILM_CHAR_BUDGET));
        // An added model resolves through the extra registry.
        assert_eq!(embedder_dim("jina-code"), Some(768));
        assert_eq!(embedder_char_budget("jina-code"), Some(1100));
        // Unknown ids resolve to nothing for dim/budget.
        assert_eq!(embedder_dim("nope"), None);
        assert_eq!(embedder_char_budget("nope"), None);
        // known_embedders lists MiniLM first, then the extras.
        let known = known_embedders();
        assert_eq!(known.first(), Some(&MINILM_ID));
        assert!(known.contains(&"jina-code"));
    }

    #[test]
    fn gpu_gate_threshold_is_model_aware() {
        // MiniLM keeps the high (one-batch) threshold; heavier models break
        // even at fewer misses, so their thresholds are lower.
        let minilm = embedder_gpu_auto_min_misses(MINILM_ID);
        let jina = embedder_gpu_auto_min_misses("jina-code");
        assert_eq!(minilm, minilm::MINILM_GPU_AUTO_MIN_MISSES);
        assert!(
            jina < minilm,
            "jina-code ({jina}) should gate below MiniLM ({minilm})"
        );
        // Unknown ids fall back to the MiniLM threshold (conservative).
        assert_eq!(embedder_gpu_auto_min_misses("nope"), minilm);
    }

    #[test]
    #[serial]
    fn resolve_embedder_precedence() {
        // `#[serial]`: mutating process env from a multi-threaded test runner
        // races every concurrent `getenv` (POSIX UB) — all env-mutating tests
        // share the serial lock. `VexEmbedderGuard`: a panic between any
        // `set_var` and the trailing `remove_var` left `VEX_EMBEDDER` set for
        // the next serial test; Drop restores it now even on unwind.
        let _guard = VexEmbedderGuard::capture();
        std::env::remove_var("VEX_EMBEDDER");
        // CLI flag wins over everything.
        assert_eq!(resolve_embedder(Some("cli-id"), Some("cfg-id")), "cli-id");
        // .vex.toml wins over env + default.
        assert_eq!(resolve_embedder(None, Some("cfg-id")), "cfg-id");
        // No CLI/config + no env → default.
        assert_eq!(resolve_embedder(None, None), DEFAULT_EMBEDDER);
        // Env is the global fallback when CLI/config are absent — accepted
        // only when it names a KNOWN embedder (allowlist at the boundary).
        std::env::set_var("VEX_EMBEDDER", "jina-code");
        assert_eq!(resolve_embedder(None, None), "jina-code");
        // ...but CLI/config still override it.
        assert_eq!(resolve_embedder(Some("cli-id"), None), "cli-id");
        assert_eq!(resolve_embedder(None, Some("cfg-id")), "cfg-id");
        // An unknown env id is ambient state, not an explicit request: it
        // degrades to the default (with a warning) instead of failing every
        // command — and never leaks out as "the embedder id to use".
        std::env::set_var("VEX_EMBEDDER", "not-a-real-embedder");
        assert_eq!(resolve_embedder(None, None), DEFAULT_EMBEDDER);
        // Blank env is ignored (falls through to default).
        std::env::set_var("VEX_EMBEDDER", "   ");
        assert_eq!(resolve_embedder(None, None), DEFAULT_EMBEDDER);
        // `_guard` restores the captured pre-test VEX_EMBEDDER on Drop.
    }
}
