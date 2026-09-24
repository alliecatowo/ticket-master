//! Owns text embedding: the [`Embedder`] trait, the no-network [`LocalHashEmbedder`], a
//! content-hash keyed cache so re-embedding unchanged text is free, and cosine similarity.
//! [`crate::potion::PotionEmbedder`] (real static-embedding semantic search over
//! `model2vec-rs`, see D-025) lives in [`crate::potion`] rather than here, since it pulls in
//! its own dependency tree; [`build_default_embedder`] is what picks between the two. An
//! API-backed embedder (e.g. `Fabric::embed` on role `embedder`) is pluggable behind the same
//! trait but is not implemented in this crate; higher layers supply it.
//!
//! [`LocalHashEmbedder`] must be deterministic (same input text always yields the same
//! vector, on any machine, any run) and every output vector must be L2-normalized (norm 1,
//! within floating-point tolerance) so cosine similarity reduces to a dot product. Those two
//! properties are what this module's tests check.

use std::collections::HashMap;
use std::env;
use std::sync::Arc;

use parking_lot::Mutex;
use tm_types::Result;

/// Fixed output dimensionality of [`LocalHashEmbedder`].
pub const LOCAL_HASH_DIMS: usize = 512;

/// Something that turns text into fixed-length vectors.
pub trait Embedder: Send + Sync {
    /// The dimensionality of every vector this embedder produces.
    fn dims(&self) -> usize;

    /// Embed a batch of texts, one vector per input, in the same order. Every returned vector
    /// has length [`Embedder::dims`] and is L2-normalized.
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;

    /// A stable identifier for the embedder implementation and configuration, stored
    /// alongside vectors so a corpus can detect a mismatched embedder (e.g. after switching
    /// from local-hash to an API embedder) and know to re-embed.
    fn identifier(&self) -> &str;
}

/// Deterministic, no-network embedder: hashed character 3-5-grams plus token unigrams and
/// bigrams, sublinear term frequency, corpus IDF, hashed into [`LOCAL_HASH_DIMS`] buckets and
/// L2-normalized.
///
/// IDF statistics are corpus-dependent and must be fit (via [`LocalHashEmbedder::fit_idf`])
/// before `embed` reflects them; before fitting, IDF defaults to 1.0 for every bucket (pure
/// TF weighting), so the embedder is always usable, just less discriminative until fit.
pub struct LocalHashEmbedder {
    idf: Vec<f32>,
    cache: Mutex<HashMap<blake3::Hash, Vec<f32>>>,
}

impl LocalHashEmbedder {
    /// A fresh embedder with uniform (unfit) IDF weights.
    pub fn new() -> Self {
        LocalHashEmbedder {
            idf: vec![1.0; LOCAL_HASH_DIMS],
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Recompute per-bucket IDF weights from a representative corpus of documents. Call this
    /// once per full index build (or incrementally, at the caller's discretion) before relying
    /// on embeddings for ranking; it does not affect determinism of a given (text, idf-state)
    /// pair, only the weights used.
    ///
    /// Feature extraction (shared with [`Embedder::embed`]): lowercase the text via plain
    /// `char::to_lowercase` (no unicode-segmentation crate available — flat_map, since it can
    /// yield >1 char per input char), take char 3-, 4- and 5-gram windows over the lowercased
    /// `Vec<char>` (each window joined back to a `String`), and separately tokenize by
    /// splitting on `!c.is_alphanumeric()` to get unigrams, filtering empty tokens, plus each
    /// adjacent token pair joined by a single space as bigrams. Each feature string hashes to
    /// a bucket via `blake3::hash(feature.as_bytes())`, folding the first 8 bytes to a `u64`
    /// (`from_le_bytes`) modulo [`LOCAL_HASH_DIMS`] — deterministic and seedless by
    /// construction, so identical across runs/machines.
    pub fn fit_idf(&mut self, corpus: &[String]) {
        if corpus.is_empty() {
            self.idf = vec![1.0; LOCAL_HASH_DIMS];
            self.cache.lock().clear();
            return;
        }

        let mut df = vec![0u64; LOCAL_HASH_DIMS];
        for doc in corpus {
            let mut seen = vec![false; LOCAL_HASH_DIMS];
            for bucket in feature_buckets(doc) {
                seen[bucket] = true;
            }
            for (bucket, was_seen) in seen.into_iter().enumerate() {
                if was_seen {
                    df[bucket] += 1;
                }
            }
        }

        let n_docs = corpus.len() as f32;
        self.idf = df
            .iter()
            .map(|&count| ((1.0 + n_docs) / (1.0 + count as f32)).ln() + 1.0)
            .collect();
        self.cache.lock().clear();
    }
}

/// Lowercase and hash a text into its raw (bucket, weighted-features-not-yet-applied) feature
/// list: char 3-5-grams plus token unigrams/bigrams, each mapped to a bucket in
/// `0..LOCAL_HASH_DIMS`. Shared by `fit_idf` (document-frequency pass) and `embed_one`
/// (term-frequency pass).
fn feature_buckets(text: &str) -> Vec<usize> {
    let lower: Vec<char> = text.to_lowercase().chars().collect();
    let mut features: Vec<String> = Vec::new();

    for n in 3..=5 {
        if lower.len() >= n {
            for window in lower.windows(n) {
                features.push(window.iter().collect());
            }
        }
    }

    let tokens: Vec<String> = text
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect();
    for token in &tokens {
        features.push(token.clone());
    }
    for pair in tokens.windows(2) {
        features.push(format!("{} {}", pair[0], pair[1]));
    }

    features.into_iter().map(bucket_of).collect()
}

/// Hash a single feature string into its bucket via blake3, folding the first 8 bytes to a
/// `u64` modulo [`LOCAL_HASH_DIMS`].
fn bucket_of(feature: String) -> usize {
    let hash = blake3::hash(feature.as_bytes());
    let bytes: [u8; 8] = hash.as_bytes()[..8]
        .try_into()
        .expect("blake3 hash is at least 8 bytes");
    (u64::from_le_bytes(bytes) % LOCAL_HASH_DIMS as u64) as usize
}

impl Default for LocalHashEmbedder {
    fn default() -> Self {
        LocalHashEmbedder::new()
    }
}

impl Embedder for LocalHashEmbedder {
    fn dims(&self) -> usize {
        LOCAL_HASH_DIMS
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let mut out = Vec::with_capacity(texts.len());
        for text in texts {
            let key = blake3::hash(text.as_bytes());
            if let Some(cached) = self.cache.lock().get(&key) {
                out.push(cached.clone());
                continue;
            }

            let mut counts = vec![0u32; LOCAL_HASH_DIMS];
            for bucket in feature_buckets(text) {
                counts[bucket] += 1;
            }

            let mut vector = vec![0.0f32; LOCAL_HASH_DIMS];
            for (bucket, &count) in counts.iter().enumerate() {
                if count > 0 {
                    vector[bucket] = (1.0 + (count as f32).ln()) * self.idf[bucket];
                }
            }

            let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
            if norm > 0.0 {
                for v in &mut vector {
                    *v /= norm;
                }
            }

            self.cache.lock().insert(key, vector.clone());
            out.push(vector);
        }
        Ok(out)
    }

    fn identifier(&self) -> &str {
        "local-hash-512"
    }
}

impl<T: Embedder + ?Sized> Embedder for Arc<T> {
    fn dims(&self) -> usize {
        (**self).dims()
    }
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        (**self).embed(texts)
    }
    fn identifier(&self) -> &str {
        (**self).identifier()
    }
}

/// Env var choosing which embedder [`build_default_embedder`] picks: `"potion"` requires
/// [`crate::potion::PotionEmbedder`] (falling back to [`LocalHashEmbedder`] with a logged
/// warning if it can't load, never erroring); `"hash"` forces [`LocalHashEmbedder`] and skips
/// even trying to load Potion; unset or any other value tries Potion from the local
/// HuggingFace cache first and falls back to `LocalHashEmbedder` if it isn't cached.
/// `config_override` (typically a project's `index.embedder` config key, once a caller wires
/// it up — see D-025) takes precedence over this env var when both are given.
pub const TM_EMBEDDER_ENV: &str = "TM_EMBEDDER";

/// Build the embedder [`crate::api::CodeIntel`] should use by default, per [`TM_EMBEDDER_ENV`]
/// / `config_override`.
///
/// This never downloads the Potion model: it only picks Potion when it is already present in
/// the local HuggingFace cache (`crate::potion::PotionEmbedder::try_load_offline`), even when
/// explicitly requested via `"potion"`. Deliberately conservative, not just cautious: this
/// function backs `CodeIntel::open_auto`/`open_at_auto`, which real command paths use, but it
/// must stay safe to call from anywhere, including a future test that forgets this — a caller
/// that wants to *download* the model when missing should call
/// `crate::potion::PotionEmbedder::try_load()` directly (e.g. from `tm doctor` or an explicit
/// `tm index --embedder potion` verb) and pass the result to `CodeIntel::open_with_embedder`.
pub fn build_default_embedder(config_override: Option<&str>) -> Arc<dyn Embedder> {
    let choice = config_override
        .map(str::to_string)
        .or_else(|| env::var(TM_EMBEDDER_ENV).ok());

    select_embedder(choice.as_deref(), || {
        crate::potion::PotionEmbedder::try_load_offline()
            .map(|embedder| Arc::new(embedder) as Arc<dyn Embedder>)
    })
}

/// Pure core of [`build_default_embedder`]: `choice` is the already-resolved
/// `config_override`/[`TM_EMBEDDER_ENV`] string (trimmed and lowercased before matching, so
/// `"HASH"`/`" hash "` behave like `"hash"`), and `load_potion` is called at most once, only
/// when `choice` isn't `"hash"`. Split out specifically so tests can exercise every branch
/// with a stub loader instead of depending on this machine's HuggingFace cache state (which
/// would otherwise make the "Potion available" and "falls back to hash" branches
/// indistinguishable, and non-deterministic across machines, in a test that calls the real
/// [`crate::potion::PotionEmbedder::try_load_offline`]).
fn select_embedder(
    choice: Option<&str>,
    load_potion: impl FnOnce() -> Option<Arc<dyn Embedder>>,
) -> Arc<dyn Embedder> {
    match choice.map(|s| s.trim().to_lowercase()).as_deref() {
        Some("hash") => Arc::new(LocalHashEmbedder::new()),
        _ => load_potion().unwrap_or_else(|| Arc::new(LocalHashEmbedder::new())),
    }
}

/// Cosine similarity between two vectors of equal length. Returns 0.0 for length mismatch or
/// either vector having zero norm, rather than panicking or returning NaN.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let na = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embed_produces_the_configured_dimensionality() {
        let embedder = LocalHashEmbedder::new();
        let vectors = embedder.embed(&["hello world".to_string()]).unwrap();
        assert_eq!(vectors[0].len(), LOCAL_HASH_DIMS);
    }

    #[test]
    fn embed_is_deterministic_across_calls() {
        let embedder = LocalHashEmbedder::new();
        let texts = vec!["the quick brown fox".to_string()];
        let first = embedder.embed(&texts).unwrap();
        let second = embedder.embed(&texts).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn embed_is_deterministic_across_fresh_instances() {
        let a = LocalHashEmbedder::new();
        let b = LocalHashEmbedder::new();
        let texts = vec!["ticketmaster codeintel embed".to_string()];
        assert_eq!(a.embed(&texts).unwrap(), b.embed(&texts).unwrap());
    }

    #[test]
    fn embed_output_is_l2_normalized() {
        let embedder = LocalHashEmbedder::new();
        let vectors = embedder
            .embed(&["some non-empty text to embed".to_string()])
            .unwrap();
        let norm = vectors[0].iter().map(|v| v * v).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-4, "norm was {norm}");
    }

    #[test]
    fn empty_text_embeds_to_the_zero_vector_without_panicking() {
        let embedder = LocalHashEmbedder::new();
        let vectors = embedder.embed(&[String::new()]).unwrap();
        assert!(vectors[0].iter().all(|&v| v == 0.0));
    }

    #[test]
    fn different_texts_generally_produce_different_vectors() {
        let embedder = LocalHashEmbedder::new();
        let vectors = embedder
            .embed(&[
                "completely different content here".to_string(),
                "another unrelated string".to_string(),
            ])
            .unwrap();
        assert_ne!(vectors[0], vectors[1]);
    }

    #[test]
    fn cache_returns_identical_vector_for_repeated_content() {
        let embedder = LocalHashEmbedder::new();
        let texts = vec![
            "repeated content".to_string(),
            "repeated content".to_string(),
        ];
        let vectors = embedder.embed(&texts).unwrap();
        assert_eq!(vectors[0], vectors[1]);
        assert_eq!(embedder.cache.lock().len(), 1);
    }

    #[test]
    fn fit_idf_changes_subsequent_embeddings_and_clears_cache() {
        let mut embedder = LocalHashEmbedder::new();
        let text = "distinctive rare phrase".to_string();
        let before = embedder.embed(std::slice::from_ref(&text)).unwrap();

        let corpus = vec![
            "distinctive rare phrase".to_string(),
            "totally different unrelated document".to_string(),
            "another filler document with words".to_string(),
        ];
        embedder.fit_idf(&corpus);
        assert!(embedder.cache.lock().is_empty());

        let after = embedder.embed(&[text]).unwrap();
        assert_ne!(before, after);
    }

    #[test]
    fn fit_idf_on_empty_corpus_resets_to_uniform_weights() {
        let mut embedder = LocalHashEmbedder::new();
        embedder.fit_idf(&["some document".to_string()]);
        embedder.fit_idf(&[]);
        assert!(embedder.idf.iter().all(|&w| w == 1.0));
    }

    #[test]
    fn cosine_similarity_of_identical_normalized_vectors_is_one() {
        let embedder = LocalHashEmbedder::new();
        let vectors = embedder.embed(&["some text".to_string()]).unwrap();
        let sim = cosine_similarity(&vectors[0], &vectors[0]);
        assert!((sim - 1.0).abs() < 1e-4);
    }

    #[test]
    fn cosine_similarity_returns_zero_for_mismatched_lengths() {
        assert_eq!(cosine_similarity(&[1.0, 0.0], &[1.0, 0.0, 0.0]), 0.0);
    }

    #[test]
    fn cosine_similarity_returns_zero_for_zero_norm_vectors() {
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 0.0]), 0.0);
    }

    #[test]
    fn cosine_similarity_of_orthogonal_vectors_is_zero() {
        assert_eq!(cosine_similarity(&[1.0, 0.0], &[0.0, 1.0]), 0.0);
    }

    #[test]
    fn identifier_is_stable() {
        let embedder = LocalHashEmbedder::new();
        assert_eq!(embedder.identifier(), "local-hash-512");
    }

    #[test]
    fn build_default_embedder_honors_hash_override_without_touching_potion() {
        // "hash" must short-circuit before even trying to load Potion (offline-only or not),
        // so this is deterministic on every machine regardless of what's cached locally.
        let embedder = build_default_embedder(Some("hash"));
        assert_eq!(embedder.identifier(), "local-hash-512");
    }

    /// A stub embedder purely for asserting [`select_embedder`] passes a loader's result
    /// through unchanged, without depending on Potion or the real HuggingFace cache.
    struct StubEmbedder;
    impl Embedder for StubEmbedder {
        fn dims(&self) -> usize {
            1
        }
        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| vec![0.0]).collect())
        }
        fn identifier(&self) -> &str {
            "stub-embedder"
        }
    }

    #[test]
    fn select_embedder_hash_choice_never_calls_the_loader() {
        let embedder = select_embedder(Some("hash"), || {
            panic!("the loader must not be called when \"hash\" is explicitly chosen")
        });
        assert_eq!(embedder.identifier(), "local-hash-512");
    }

    #[test]
    fn select_embedder_hash_choice_is_case_and_whitespace_insensitive() {
        let embedder = select_embedder(Some(" HASH \n"), || {
            panic!("the loader must not be called when \"hash\" is explicitly chosen")
        });
        assert_eq!(embedder.identifier(), "local-hash-512");
    }

    #[test]
    fn select_embedder_falls_back_to_hash_when_the_loader_returns_none() {
        let embedder = select_embedder(None, || None);
        assert_eq!(embedder.identifier(), "local-hash-512");
    }

    #[test]
    fn select_embedder_uses_whatever_the_loader_returns_when_not_hash() {
        let embedder: Arc<dyn Embedder> =
            select_embedder(None, || Some(Arc::new(StubEmbedder) as Arc<dyn Embedder>));
        assert_eq!(embedder.identifier(), "stub-embedder");
    }

    #[test]
    fn select_embedder_treats_potion_choice_the_same_as_unset() {
        let embedder: Arc<dyn Embedder> = select_embedder(Some("potion"), || {
            Some(Arc::new(StubEmbedder) as Arc<dyn Embedder>)
        });
        assert_eq!(embedder.identifier(), "stub-embedder");
    }
}
