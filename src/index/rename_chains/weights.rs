//! Phase 14.10 — scoring weights and gates for rename-chain detection.
//!
//! Every constant is justified by either the design doc
//! (`.claude/Task/PHASE14.10-symbol-rename-tracking.md`) or the
//! literature pass summarised therein. Hard-coded as
//! `pub(crate) const` per rust M1 fix — no per-repo tuning in v1.
//! Future-tuning archeology is captured by persisting the active
//! weights into the sidecar header (handled in `src/store/rename_chains.rs`).
//!
//! ## Gate ordering on hot path
//!
//! Cheapest-first: kind → length-ratio → body Jaccard → composite
//! score. `GATE_LEN_RATIO` knocks out extract-method false-positives
//! per RefactoringMiner 3.0's empirical fix (raised RM precision from
//! 63% to 99.3%). `GATE_JACCARD` matches SourcererCC's empirical
//! Type-2/3 optimum (86% precision / 86–100% recall on
//! BigCloneBench). `GATE_SCORE` keeps the composite gate distinct from
//! the body-only gate so the MiniLM tiebreaker can pull marginal
//! pairs above the line without lowering the Jaccard floor.

/// MinHash signature length, in u32 slots. Widened from the
/// literature-default 128 to 240 so the LSH band layout
/// (`NUM_BANDS × ROWS_PER_BAND = 20 × 12`) tiles the signature
/// exactly without overflow. Memory cost per signature: 240 × 4 B =
/// 960 B; at 50k symbols ≈ 48 MB peak in Phase 0, freed before
/// sidecar emit.
pub(crate) const MINHASH_HASHES: usize = 240;

/// LSH band count. b=20, r=12 targets Jaccard ≥ 0.7 per the
/// SourcererCC / MinHash-LSH banding formula; widened from the
/// design's b=10 to reduce per-band collision pressure on monorepo-
/// scale repos (rust H3 fix).
pub(crate) const NUM_BANDS: usize = 20;

/// Rows per LSH band. With NUM_BANDS=20 and ROWS_PER_BAND=12, the
/// product is 240 — exactly MINHASH_HASHES. Compile-time asserts
/// below enforce the invariant.
pub(crate) const ROWS_PER_BAND: usize = 12;

// Compile-time invariant: bands × rows must fit in the signature.
// `<=` rather than `==` so a future signature widening (e.g. 256
// slots, leaving 16 unused) doesn't break the LSH layer.
const _: () = assert!(
    NUM_BANDS * ROWS_PER_BAND <= MINHASH_HASHES,
    "LSH band layout overflows MinHash signature",
);

/// Composite-score weight on body-token Jaccard when MiniLM cosine
/// is available. Body tokens carry the strongest rename signal per
/// CodeShovel + RefactoringMiner empirical results; the 0.70/0.20/0.10
/// split was derived from the design doc's literature pass.
pub(crate) const W_BODY_WITH_COS: f32 = 0.70;

/// Composite-score weight on signature-token Jaccard when MiniLM
/// cosine is available. Captures parameter-list / type-signature
/// continuity across renames.
pub(crate) const W_SIG_WITH_COS: f32 = 0.20;

/// Composite-score weight on MiniLM cosine similarity. Held to 0.10
/// because plain MiniLM (no clone-detect fine-tune) caps near F1 0.70
/// — adequate as a tiebreaker, insufficient as a primary signal.
pub(crate) const W_COS: f32 = 0.10;

/// Body-Jaccard weight when MiniLM is unavailable (e.g.
/// `--no-semantic`). Renormalised from the 0.70 baseline so the
/// no-cosine pair still sums to 1.0 and a perfect body match
/// (j_body = 1.0) still clears `GATE_SCORE` at j_sig = 0 (arch H4 fix).
pub(crate) const W_BODY_NO_COS: f32 = 0.78;

/// Signature-Jaccard weight when MiniLM is unavailable. Pairs with
/// `W_BODY_NO_COS` to sum to 1.0.
pub(crate) const W_SIG_NO_COS: f32 = 0.22;

/// Composite-score gate. Below this the pair is rejected outright.
/// Tuned so j_body = 1.0 alone clears the gate in both with/without
/// cosine modes — i.e. a perfect body match never needs help from
/// the signature or cosine signal.
pub(crate) const GATE_SCORE: f32 = 0.65;

/// Body-Jaccard pre-gate. SourcererCC's empirical Type-2/3 optimum.
/// Pairs below this are skipped before the composite score is even
/// computed, saving the cosine lookup on doomed candidates.
pub(crate) const GATE_JACCARD: f32 = 0.70;

/// Length-ratio gate: `min(len_a, len_b) / max(len_a, len_b)`.
/// RefactoringMiner 3.0's primary fix against extract-method false
/// positives (raised precision from 63% to 99.3%). A method that
/// shrinks to <60% of its prior body is treated as "extracted, not
/// renamed" and rejected.
pub(crate) const GATE_LEN_RATIO: f32 = 0.60;

// Compile-time invariant: with-cosine weights sum to 1.0. f32
// rounding tolerance — true sum is 0.70 + 0.20 + 0.10 = 1.0 exactly
// in f32 (all three are exact dyadic fractions), but the inequality
// guard documents the contract rather than relying on representation.
const _: () = {
    let sum = W_BODY_WITH_COS + W_SIG_WITH_COS + W_COS;
    assert!(
        sum > 0.999 && sum < 1.001,
        "with-cosine weights must sum to 1.0",
    );
};

// Compile-time invariant: no-cosine weights sum to 1.0. 0.78 + 0.22
// = 1.0 exactly in f32 (78/100 and 22/100 round-trip cleanly enough).
const _: () = {
    let sum = W_BODY_NO_COS + W_SIG_NO_COS;
    assert!(
        sum > 0.999 && sum < 1.001,
        "no-cosine weights must sum to 1.0",
    );
};

// Compile-time invariant: a perfect body match (j_body = 1.0) clears
// `GATE_SCORE` in both modes with everything else at zero. Documents
// the design intent: the score gate must never reject a
// byte-identical body just because the signature or cosine signal is
// missing.
const _: () = {
    assert!(
        W_BODY_WITH_COS >= GATE_SCORE,
        "perfect body match must clear GATE_SCORE in with-cosine mode",
    );
    assert!(
        W_BODY_NO_COS >= GATE_SCORE,
        "perfect body match must clear GATE_SCORE in no-cosine mode",
    );
};
