pub mod cache;
pub mod integrity;
pub mod minilm;
pub mod model;
pub mod tokenizer;

use anyhow::{bail, Result};

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
    match id {
        MINILM_ID => Ok(Box::new(MiniLMEmbedder::new()?)),
        other => bail!(
            "unknown embedder: `{other}`. Known embedders: {}",
            known_embedders().join(", ")
        ),
    }
}

/// Vector dimension of a known embedder by ID, without instantiating the
/// model. Returns `None` for unknown IDs. Used by the writer to record
/// `Header.vector_dim` even when no symbols are present, and by tests.
pub fn embedder_dim(id: &str) -> Option<u32> {
    match id {
        MINILM_ID => Some(MINILM_DIM),
        _ => None,
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
        _ => None,
    }
}

/// List of known embedder IDs in registry order. Used in error messages and
/// for the `--embedder` CLI help.
pub fn known_embedders() -> Vec<&'static str> {
    vec![MINILM_ID]
}

/// Resolve the embedder ID to use for an operation.
///
/// Priority: `cli` (e.g. `--embedder` flag) > `config` (`.vex.toml`) >
/// [`DEFAULT_EMBEDDER`]. Returns an owned `String` since CLI/config sources
/// supply distinct lifetimes.
pub fn resolve_embedder(cli: Option<&str>, config: Option<&str>) -> String {
    cli.or(config)
        .map(|s| s.to_string())
        .unwrap_or_else(|| DEFAULT_EMBEDDER.to_string())
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
