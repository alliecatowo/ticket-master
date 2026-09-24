//! [`PotionEmbedder`]: a real semantic embedder over MinishLab's `model2vec-rs`, using the
//! same static-embedding model (`minishlab/potion-code-16M-v2`) zvec-grep's own semantic
//! search runs on. Pure Rust, CPU-only inference, no ONNX runtime and no per-query network
//! call — only *loading* the model can touch the network, and only when it is not already in
//! the local HuggingFace cache. See `docs/decisions/D-025-potion-semantic-embedder.md`.
//!
//! [`PotionEmbedder::try_load`] never panics and never returns an `Err`: any failure (missing
//! cache, corrupt snapshot, disabled/failed download) is logged once via `tracing` and
//! reported as `None`, so callers ([`crate::embed::build_default_embedder`]) can fall back to
//! [`crate::embed::LocalHashEmbedder`] instead of breaking indexing. Tests must never reach
//! the network (hygiene forbids it): [`PotionEmbedder::try_load_offline`] and the
//! `allow_download` parameter on the internal loader exist specifically so tests can exercise
//! the fallback path deterministically instead of depending on this machine's cache state.

use std::env;
use std::path::{Path, PathBuf};
use std::sync::Once;

use model2vec_rs::model::StaticModel;
use tm_types::Result;

use crate::embed::Embedder;

/// HuggingFace repo id of the default Potion model — the same one zvec-grep uses.
pub const POTION_MODEL_REPO: &str = "minishlab/potion-code-16M-v2";

/// Stable identifier stored in the `vectors.embedder` column and returned by
/// [`Embedder::identifier`]. Bump this (or change [`POTION_MODEL_REPO`]) whenever the model
/// changes, so [`crate::api::CodeIntel::update_incremental`]'s embedder-mismatch check
/// re-embeds instead of silently mixing vector spaces.
pub const POTION_IDENTIFIER: &str = "potion-code-16m-v2";

/// Env var that opts out of downloading the model when it isn't already cached locally.
/// Accepts the same truthy/falsy spellings as `TM_NOTIFY` elsewhere in this repo: `"0"`,
/// `"false"`, `"off"` and `"no"` (case-insensitive) disable the download; anything else
/// (including unset) allows it.
pub const TM_EMBEDDER_DOWNLOAD_ENV: &str = "TM_EMBEDDER_DOWNLOAD";

static WARN_ONCE: Once = Once::new();

/// Static-embedding `model2vec-rs` wrapper implementing [`Embedder`].
pub struct PotionEmbedder {
    model: StaticModel,
    dims: usize,
}

impl PotionEmbedder {
    /// Load [`POTION_MODEL_REPO`] from the local HuggingFace cache (`$HF_HOME/hub` or
    /// `~/.cache/huggingface/hub`), downloading it first only if it is missing and
    /// downloading hasn't been disabled (see [`TM_EMBEDDER_DOWNLOAD_ENV`]). Returns `None`
    /// (after logging once) on any failure rather than propagating an error, since a missing
    /// or broken Potion install must never stop indexing — [`crate::embed::LocalHashEmbedder`]
    /// is always a usable fallback.
    pub fn try_load() -> Option<PotionEmbedder> {
        Self::try_load_with(
            cache_snapshot_dir(POTION_MODEL_REPO).as_deref(),
            download_allowed(),
        )
    }

    /// Load only from the local cache; never attempts a download regardless of
    /// [`TM_EMBEDDER_DOWNLOAD_ENV`]. Used by callers that must guarantee no network access
    /// (e.g. tests, and [`crate::embed::build_default_embedder`]).
    pub fn try_load_offline() -> Option<PotionEmbedder> {
        Self::try_load_with(cache_snapshot_dir(POTION_MODEL_REPO).as_deref(), false)
    }

    /// Core loader: try `local_snapshot` first if given, then fall back to
    /// `StaticModel::from_pretrained(POTION_MODEL_REPO, ..)` (which downloads) only when
    /// `allow_download` is true. Split out from [`PotionEmbedder::try_load`] so tests can
    /// exercise every branch (missing snapshot, corrupt snapshot, download disabled) without
    /// touching real cache paths or the network.
    fn try_load_with(
        local_snapshot: Option<&Path>,
        allow_download: bool,
    ) -> Option<PotionEmbedder> {
        if let Some(dir) = local_snapshot {
            // `model2vec-rs`'s `StaticModel::from_pretrained` decides local-vs-hub purely by
            // `Path::exists()`: a path that does *not* exist on disk is treated as a HuggingFace
            // repo id and, with the `hf-hub` feature enabled, silently falls through to a real
            // network download regardless of this function's own `allow_download` gate. Check
            // existence ourselves first so a missing/deleted snapshot directory can only ever
            // reach the explicit, `allow_download`-gated branch below, never `from_pretrained`'s
            // own hub fallback.
            if dir.is_dir() {
                match Self::load_from_path(dir) {
                    Ok(embedder) => return Some(embedder),
                    Err(e) => warn_once(&format!(
                        "local Potion model at {} failed to load ({e}); falling back to the hash embedder",
                        dir.display()
                    )),
                }
            } else {
                warn_once(&format!(
                    "expected a local Potion snapshot at {} but it does not exist; falling back \
                     to the hash embedder",
                    dir.display()
                ));
            }
        }

        if !allow_download {
            if local_snapshot.is_none() {
                warn_once(
                    "no local Potion model cached and downloading is disabled; \
                     falling back to the hash embedder",
                );
            }
            return None;
        }

        match StaticModel::from_pretrained(POTION_MODEL_REPO, None, Some(true), None) {
            Ok(model) => Some(Self::from_model(model)),
            Err(e) => {
                warn_once(&format!(
                    "downloading the Potion model failed ({e}); falling back to the hash embedder"
                ));
                None
            }
        }
    }

    fn load_from_path(dir: &Path) -> Result<PotionEmbedder> {
        let model = StaticModel::from_pretrained(dir, None, Some(true), None)
            .map_err(|e| tm_types::TmError::storage(format!("failed to load Potion model: {e}")))?;
        Ok(Self::from_model(model))
    }

    fn from_model(model: StaticModel) -> PotionEmbedder {
        // model2vec-rs exposes no direct dimensionality accessor; probe it once with a
        // non-empty string so an empty-token edge case can't report a spurious zero.
        let dims = model.encode(&["potion".to_string()])[0].len();
        PotionEmbedder { model, dims }
    }
}

impl Embedder for PotionEmbedder {
    fn dims(&self) -> usize {
        self.dims
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(self.model.encode(texts))
    }

    fn identifier(&self) -> &str {
        POTION_IDENTIFIER
    }
}

fn warn_once(msg: &str) {
    // Only the first failure in a process's lifetime is worth a log line: a hot
    // re-embed/retry loop must not spam stderr with the same "falling back to the hash
    // embedder" warning on every call.
    WARN_ONCE.call_once(|| {
        tracing::warn!("{msg}");
    });
}

/// Resolve the `$HF_HOME/hub` (or `~/.cache/huggingface/hub`) directory, matching the
/// `huggingface_hub` Python client's own resolution order.
fn hf_hub_dir() -> Option<PathBuf> {
    hf_hub_dir_from(env::var("HF_HOME").ok().as_deref())
}

/// Pure core of [`hf_hub_dir`], taking `$HF_HOME`'s value (or `None` if unset) directly so
/// tests can exercise both branches without mutating process-global env state (this crate
/// forbids `unsafe_code`, and `std::env::set_var`/`remove_var` are `unsafe fn`).
fn hf_hub_dir_from(hf_home: Option<&str>) -> Option<PathBuf> {
    if let Some(home) = hf_home {
        if !home.trim().is_empty() {
            return Some(PathBuf::from(home).join("hub"));
        }
    }
    dirs::home_dir().map(|h| h.join(".cache").join("huggingface").join("hub"))
}

/// Find the most recent snapshot directory for `repo` under the local HuggingFace cache, if
/// present: `<hub>/models--<org>--<name>/snapshots/<hash>/`.
fn cache_snapshot_dir(repo: &str) -> Option<PathBuf> {
    cache_snapshot_dir_in(&hf_hub_dir()?, repo)
}

/// Pure core of [`cache_snapshot_dir`], taking the hub directory directly so tests can point
/// it at a fixture without touching the real HuggingFace cache or `$HF_HOME`.
fn cache_snapshot_dir_in(hub: &Path, repo: &str) -> Option<PathBuf> {
    let dir_name = format!("models--{}", repo.replace('/', "--"));
    let snapshots = hub.join(dir_name).join("snapshots");
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&snapshots)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    entries.sort();
    entries.pop()
}

/// Whether [`PotionEmbedder::try_load`] may download a missing model, per
/// [`TM_EMBEDDER_DOWNLOAD_ENV`].
fn download_allowed() -> bool {
    download_allowed_from(env::var(TM_EMBEDDER_DOWNLOAD_ENV).ok().as_deref())
}

/// Pure core of [`download_allowed`], taking the env var's value (or `None` if unset)
/// directly so tests don't need to mutate process-global env state.
fn download_allowed_from(value: Option<&str>) -> bool {
    match value {
        Some(v) => !matches!(
            v.trim().to_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        ),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_load_with_no_snapshot_and_no_download_returns_none_without_network() {
        // No local snapshot path given, download disabled: must not touch the network and
        // must return None rather than panicking or erroring.
        assert!(PotionEmbedder::try_load_with(None, false).is_none());
    }

    #[test]
    fn try_load_with_missing_directory_and_no_download_returns_none() {
        let missing = std::env::temp_dir().join("tm-codeintel-test-nonexistent-potion-snapshot");
        assert!(PotionEmbedder::try_load_with(Some(&missing), false).is_none());
    }

    #[test]
    fn try_load_with_corrupt_snapshot_falls_back_without_downloading() {
        let dir = tempfile::tempdir().expect("tempdir");
        // An empty directory is not a valid model2vec snapshot (no tokenizer.json /
        // model.safetensors), so loading it must fail cleanly and, with downloading
        // disabled, fall back to None rather than reaching the network.
        assert!(PotionEmbedder::try_load_with(Some(dir.path()), false).is_none());
    }

    #[test]
    fn download_allowed_from_reads_the_falsy_spellings() {
        assert!(!download_allowed_from(Some("0")));
        assert!(!download_allowed_from(Some("false")));
        assert!(!download_allowed_from(Some("FALSE")));
        assert!(!download_allowed_from(Some("off")));
        assert!(!download_allowed_from(Some("no")));
    }

    #[test]
    fn download_allowed_from_defaults_to_true() {
        assert!(download_allowed_from(Some("yes")));
        assert!(download_allowed_from(Some("1")));
        assert!(download_allowed_from(None));
    }

    #[test]
    fn hf_hub_dir_from_uses_hf_home_when_set() {
        let dir = hf_hub_dir_from(Some("/tmp/example-hf-home"));
        assert_eq!(dir, Some(PathBuf::from("/tmp/example-hf-home/hub")));
    }

    #[test]
    fn hf_hub_dir_from_falls_back_to_the_default_cache_when_unset() {
        let dir = hf_hub_dir_from(None);
        // Whatever the machine's home directory resolves to, the suffix must be the standard
        // huggingface cache layout.
        if let Some(dir) = dir {
            assert!(dir.ends_with(".cache/huggingface/hub"));
        }
    }

    #[test]
    fn cache_snapshot_dir_in_returns_none_for_a_repo_that_is_not_cached() {
        let empty = tempfile::tempdir().expect("tempdir");
        assert!(cache_snapshot_dir_in(empty.path(), POTION_MODEL_REPO).is_none());
    }

    #[test]
    fn cache_snapshot_dir_in_picks_the_lexicographically_last_snapshot() {
        let hub = tempfile::tempdir().expect("tempdir");
        let snapshots = hub
            .path()
            .join("models--minishlab--potion-code-16M-v2")
            .join("snapshots");
        std::fs::create_dir_all(snapshots.join("aaa")).unwrap();
        std::fs::create_dir_all(snapshots.join("zzz")).unwrap();

        let found = cache_snapshot_dir_in(hub.path(), POTION_MODEL_REPO).expect("a snapshot");
        assert_eq!(found, snapshots.join("zzz"));
    }
}
