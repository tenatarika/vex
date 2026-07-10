//! Trigram extraction + per-file presence bloom for the `vex grep` skip-index
//! (STORAGE-RESEARCH §2). Two pure pieces, no I/O:
//!
//! - [`required_trigrams`] — the trigrams a file MUST contain for the pattern to
//!   possibly match, or `None` when none can be *safely* derived (caller then
//!   full-walks, reading every file as today).
//! - [`TrigramBloom`] — a fixed-size per-file presence bloom built from file
//!   bytes; [`TrigramBloom::might_contain_all`] is the skip test.
//!
//! **Core invariant — no false negatives.** `required_trigrams` returns `Some`
//! only when EVERY match of the pattern is guaranteed to contain all returned
//! trigrams verbatim; a bloom never reports a set trigram as absent (build sets
//! exactly the bits query tests). So a file that fails `might_contain_all`
//! provably cannot match, and skipping it is safe. Any doubt (non-literal
//! pattern, literal < 3 bytes, invalid regex) → `None` → full walk. False
//! positives (a kept file that doesn't match) are fine — read + regex as today.
//!
//! v1 scope: the required literal is derived only when the pattern's regex HIR
//! is a pure literal (concatenation of literals, transparent to capture groups
//! and zero-width look assertions). Any class — including `(?i)`-folded chars —
//! repetition, or alternation → `None`. Byte-oriented (trigrams are 3-byte
//! windows), so it matches the byte domain grep searches and multibyte literals
//! Just Work. Alternation OR-of-trigram-sets is deferred (csearch RegexpQuery).

use regex_syntax::hir::{Hir, HirKind};

/// Per-file bloom size in bits and hash count. Tunable starting point
/// (STORAGE-RESEARCH §2; P4 may adjust). Single source of truth so the P2
/// sidecar header can persist + validate them.
pub const BLOOM_BITS: usize = 2048;
pub const BLOOM_HASHES: usize = 1;
/// Bloom size in bytes (`BLOOM_BITS / 8`).
pub const BLOOM_BYTES: usize = BLOOM_BITS / 8;

/// Three consecutive bytes.
pub type Trigram = [u8; 3];

/// The trigrams a file must contain for `pattern` to possibly match, or `None`
/// when no safe query is derivable (→ caller reads every file). See the module
/// invariant. Trigrams may repeat; callers treat them as an AND set.
pub fn required_trigrams(pattern: &str) -> Option<Vec<Trigram>> {
    let hir = regex_syntax::parse(pattern).ok()?;
    let literal = required_literal(&hir)?;
    if literal.len() < 3 {
        return None;
    }
    Some(literal.windows(3).map(|w| [w[0], w[1], w[2]]).collect())
}

/// The exact byte string EVERY match of `hir` must contain verbatim, or `None`
/// if the pattern is not a pure literal. Capture groups are transparent (their
/// sub-expression must still match); zero-width look assertions (`^`, `$`,
/// `\b`) contribute no bytes but do not break literalness (the surrounding
/// literals are still required). Anything that can vary or omit bytes — a class
/// (incl. `(?i)` case folding), repetition, or alternation — yields `None`.
fn required_literal(hir: &Hir) -> Option<Vec<u8>> {
    match hir.kind() {
        HirKind::Empty | HirKind::Look(_) => Some(Vec::new()),
        HirKind::Literal(lit) => Some(lit.0.to_vec()),
        HirKind::Capture(cap) => required_literal(&cap.sub),
        HirKind::Concat(parts) => {
            let mut out = Vec::new();
            for part in parts {
                out.extend(required_literal(part)?);
            }
            Some(out)
        }
        // Class / Repetition / Alternation → not a guaranteed literal.
        _ => None,
    }
}

/// A fixed-size per-file trigram presence bloom.
#[derive(Clone, PartialEq, Eq)]
pub struct TrigramBloom {
    bits: [u8; BLOOM_BYTES],
}

impl TrigramBloom {
    /// Build from raw file bytes: set every 3-byte window's bit(s). Files
    /// shorter than 3 bytes produce an empty bloom (they can't contain any
    /// trigram, so any non-empty query correctly excludes them).
    pub fn from_bytes(content: &[u8]) -> Self {
        let mut bloom = TrigramBloom {
            bits: [0; BLOOM_BYTES],
        };
        for w in content.windows(3) {
            bloom.set(&[w[0], w[1], w[2]]);
        }
        bloom
    }

    /// Wrap raw bloom bytes read back from the sidecar (P2).
    pub fn from_raw(bits: [u8; BLOOM_BYTES]) -> Self {
        TrigramBloom { bits }
    }

    /// The raw bloom bytes, for persisting to the sidecar (P2).
    pub fn as_bytes(&self) -> &[u8; BLOOM_BYTES] {
        &self.bits
    }

    /// True iff every trigram's bit(s) are set — i.e. the file *might* contain
    /// all of them. `false` is a definite "cannot contain" → safe to skip. An
    /// empty trigram set is vacuously true (caller must have already decided to
    /// full-walk when `required_trigrams` returned `None`).
    pub fn might_contain_all(&self, trigrams: &[Trigram]) -> bool {
        trigrams.iter().all(|t| self.get(t))
    }

    fn set(&mut self, tri: &Trigram) {
        for pos in Self::positions(tri) {
            self.bits[pos / 8] |= 1 << (pos % 8);
        }
    }

    fn get(&self, tri: &Trigram) -> bool {
        Self::positions(tri).all(|pos| self.bits[pos / 8] & (1 << (pos % 8)) != 0)
    }

    /// The `BLOOM_HASHES` bit positions for a trigram, via double-hashing over
    /// its 24-bit value: `h_i = h1 + i*h2 (mod M)`.
    fn positions(tri: &Trigram) -> impl Iterator<Item = usize> {
        let v = (u32::from(tri[0]) << 16) | (u32::from(tri[1]) << 8) | u32::from(tri[2]);
        let h1 = v.wrapping_mul(0x9E37_79B1);
        let h2 = v.wrapping_mul(0x85EB_CA77) | 1;
        (0..BLOOM_HASHES as u32)
            .map(move |i| (h1.wrapping_add(i.wrapping_mul(h2)) % BLOOM_BITS as u32) as usize)
    }
}

impl std::fmt::Debug for TrigramBloom {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let set = self.bits.iter().map(|b| b.count_ones()).sum::<u32>();
        write!(f, "TrigramBloom({set}/{BLOOM_BITS} bits set)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use regex::Regex;

    fn tris(s: &str) -> Vec<Trigram> {
        required_trigrams(s).unwrap_or_else(|| panic!("expected trigrams for {s:?}"))
    }

    #[test]
    fn plain_literal_yields_all_windows() {
        // "foobar" → foo oob oba bar
        assert_eq!(tris("foobar"), vec![*b"foo", *b"oob", *b"oba", *b"bar"]);
    }

    #[test]
    fn escaped_metachars_are_literal() {
        // `foo\.bar` is the literal "foo.bar".
        assert_eq!(
            required_trigrams(r"foo\.bar"),
            Some(vec![*b"foo", *b"oo.", *b"o.b", *b".ba", *b"bar",])
        );
    }

    #[test]
    fn anchors_and_word_boundaries_keep_inner_literal() {
        // Look assertions contribute no bytes but don't break literalness.
        assert!(required_trigrams("^foobar$").is_some());
        assert!(required_trigrams(r"\bfoobar\b").is_some());
    }

    #[test]
    fn short_literal_is_none() {
        assert!(required_trigrams("ab").is_none());
        assert!(required_trigrams("x").is_none());
    }

    #[test]
    fn non_literal_patterns_are_none() {
        // class, wildcard, repetition, alternation, case-insensitive, invalid.
        for p in [
            "a.c",
            "[abc]def",
            "foo+bar",
            "foo|barbaz",
            "(?i)foobar",
            "fo{2,3}o",
            "foo.*bar",
            "(", // invalid regex
        ] {
            assert!(required_trigrams(p).is_none(), "expected None for {p:?}");
        }
    }

    #[test]
    fn capture_group_of_literals_is_literal() {
        assert!(required_trigrams("(foobar)").is_some());
    }

    #[test]
    fn bloom_build_and_query() {
        let bloom = TrigramBloom::from_bytes(b"the quick brown fox");
        assert!(bloom.might_contain_all(&tris("quick")));
        assert!(bloom.might_contain_all(&tris("brown fox")));
        // A trigram absent from the content is excluded.
        assert!(!bloom.might_contain_all(&[*b"zzz"]));
    }

    #[test]
    fn short_file_excluded_by_nonempty_query() {
        let bloom = TrigramBloom::from_bytes(b"ab");
        assert!(!bloom.might_contain_all(&[*b"abc"]));
    }

    proptest! {
        // THE P1 GATE (architect a9b7d18): the no-false-negative invariant over a
        // pure-literal alphabet. A literal pattern matches iff it's a substring,
        // so when it's present, its trigrams must be in the file → bloom keeps it.
        #[test]
        fn no_false_negative_literals(
            pat in "[a-zA-Z0-9_]{1,10}",
            pre in "[a-zA-Z0-9_ ]{0,30}",
            post in "[a-zA-Z0-9_ ]{0,30}",
        ) {
            let content = format!("{pre}{pat}{post}");
            let re = Regex::new(&pat).unwrap();
            prop_assert!(re.is_match(&content));
            if let Some(t) = required_trigrams(&pat) {
                let bloom = TrigramBloom::from_bytes(content.as_bytes());
                prop_assert!(
                    bloom.might_contain_all(&t),
                    "false negative: pat={pat:?} content={content:?}"
                );
            }
        }

        // Joint pattern×content over ARBITRARY strings, with `regex` as oracle:
        // whenever the pattern matches the content AND we derived trigrams, they
        // must be present. Catches any over-eager literal extraction.
        #[test]
        fn extractor_sound_over_arbitrary(
            pat in "\\PC{1,8}",
            content in "\\PC{0,80}",
        ) {
            let Ok(re) = Regex::new(&pat) else { return Ok(()); };
            if let Some(t) = required_trigrams(&pat) {
                if re.is_match(&content) {
                    let bloom = TrigramBloom::from_bytes(content.as_bytes());
                    prop_assert!(
                        bloom.might_contain_all(&t),
                        "false negative: pat={pat:?} content={content:?}"
                    );
                }
            }
        }
    }
}
