//! Artifacts and evidence: content-addressed storage plus the records that link a ticket to
//! proof of something about it.
//!
//! Artifacts up to 64 KiB live inline in SQLite; larger ones spill to
//! `.tm/artifacts/<hash>` (`SPEC.md` §4.6). Hashing is `blake3`, giving natural deduplication:
//! storing the same bytes twice yields the same `ArtifactId`-independent content hash (the
//! `ArtifactId` itself is still a fresh random id per store call — see `IMPL` note on
//! `store.rs::store_artifact` — but `hash` lets callers detect identical content cheaply). This
//! module owns the pure classification/threshold logic; `store.rs` owns the actual filesystem
//! and SQLite I/O, since content-addressed spill-to-disk is I/O by definition.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tm_types::{ArtifactId, ParticipantId, TicketId, Timestamp};

/// Bytes at or below this size are stored inline in the `artifacts.inline` column; larger bytes
/// spill to `.tm/artifacts/<hash>` on disk. `SPEC.md` §4.6.
pub const INLINE_LIMIT_BYTES: usize = 64 * 1024;

/// What kind of thing an artifact's bytes represent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ArtifactKind {
    /// Captured stdout/stderr of a command.
    CommandOutput,
    /// A unified diff / patch.
    Patch,
    /// An arbitrary file snapshot.
    File,
    /// A structured or prose report.
    Report,
    /// A generated index (e.g. code intelligence output).
    Index,
    /// Benchmark results.
    Benchmark,
    /// A conversation/session transcript.
    Transcript,
}

/// Where an artifact's bytes actually live.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ArtifactStorage {
    /// Stored directly in the `artifacts.inline` column.
    Inline(Vec<u8>),
    /// Spilled to disk at this path (relative to the project root's `.tm/artifacts/` directory).
    OnDisk(PathBuf),
}

/// A content-addressed piece of evidence or output. See `SPEC.md` §4.6.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Artifact {
    /// Stable identifier.
    pub id: ArtifactId,
    /// What kind of content this is.
    pub kind: ArtifactKind,
    /// IANA media type of the content, e.g. `text/plain`, `application/json`.
    pub media_type: String,
    /// Length of the content in bytes.
    pub bytes_len: u64,
    /// `blake3` hash of the content, hex-encoded lowercase.
    pub hash: String,
    /// Where the bytes actually live.
    pub storage: ArtifactStorage,
    /// Arbitrary structured metadata (e.g. exit code, command line).
    pub meta: Value,
}

/// A record linking a ticket to an artifact that proves something about it. Verifiers read
/// evidence; workers produce it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    /// The ticket this evidence is about.
    pub ticket: TicketId,
    /// What kind of evidence this is.
    pub kind: EvidenceKind,
    /// The artifact carrying the actual content.
    pub artifact: ArtifactId,
    /// Who produced this evidence. Enforced elsewhere (`invariants.rs`) to differ from any
    /// auditor certifying the same ticket, per the "a worker never certifies itself" rule.
    pub produced_by: ParticipantId,
    /// When this evidence was produced.
    pub ts: Timestamp,
    /// Short human-readable summary of what this evidence shows.
    pub summary: String,
}

/// What kind of proof a piece of [`Evidence`] constitutes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvidenceKind {
    /// Output of an automated test run.
    TestRun,
    /// A diff showing the change made.
    Diff,
    /// Captured command output.
    CommandOutput,
    /// A reviewer's written assessment.
    Review,
    /// A human's explicit attestation.
    HumanAttestation,
}

/// `blake3` hash of `bytes`, hex-encoded lowercase — the canonical content address used for
/// `Artifact::hash` and the on-disk spill filename.
pub fn content_hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Decide where `bytes` of this size should live: [`ArtifactStorage::Inline`] at or under
/// [`INLINE_LIMIT_BYTES`], otherwise a would-be [`ArtifactStorage::OnDisk`] path under
/// `artifacts_dir` named by `content_hash(bytes)`. Pure sizing decision; actual disk I/O (writing
/// the file) is `store.rs::store_artifact`'s job, which is why this returns the *path* rather
/// than performing the write.
pub fn classify_storage(bytes: &[u8], artifacts_dir: &std::path::Path) -> ArtifactStorage {
    if bytes.len() <= INLINE_LIMIT_BYTES {
        ArtifactStorage::Inline(bytes.to_vec())
    } else {
        ArtifactStorage::OnDisk(artifacts_dir.join(content_hash(bytes)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn content_hash_empty() {
        let hash = content_hash(b"");
        assert!(!hash.is_empty());
        assert_eq!(hash.len(), 64); // blake3 hex digest is 64 chars
        assert_eq!(hash, hash.to_lowercase()); // lowercase as documented
    }

    #[test]
    fn content_hash_deterministic() {
        let data = b"hello world";
        let hash1 = content_hash(data);
        let hash2 = content_hash(data);
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn content_hash_different_inputs() {
        let hash1 = content_hash(b"hello");
        let hash2 = content_hash(b"world");
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn classify_storage_small_empty() {
        let artifacts_dir = Path::new(".tm/artifacts");
        let result = classify_storage(b"", artifacts_dir);
        match result {
            ArtifactStorage::Inline(data) => assert_eq!(data.len(), 0),
            ArtifactStorage::OnDisk(_) => panic!("empty bytes should be inline"),
        }
    }

    #[test]
    fn classify_storage_small_data() {
        let artifacts_dir = Path::new(".tm/artifacts");
        let small_data = b"small";
        let result = classify_storage(small_data, artifacts_dir);
        match result {
            ArtifactStorage::Inline(data) => {
                assert_eq!(data.len(), small_data.len());
                assert_eq!(data.as_slice(), small_data);
            }
            ArtifactStorage::OnDisk(_) => panic!("small data should be inline"),
        }
    }

    #[test]
    fn classify_storage_at_limit() {
        let artifacts_dir = Path::new(".tm/artifacts");
        let at_limit = vec![42u8; INLINE_LIMIT_BYTES];
        let result = classify_storage(&at_limit, artifacts_dir);
        match result {
            ArtifactStorage::Inline(data) => {
                assert_eq!(data.len(), INLINE_LIMIT_BYTES);
            }
            ArtifactStorage::OnDisk(_) => panic!("data at inline limit should be inline"),
        }
    }

    #[test]
    fn classify_storage_just_over_limit() {
        let artifacts_dir = Path::new(".tm/artifacts");
        let over_limit = vec![99u8; INLINE_LIMIT_BYTES + 1];
        let result = classify_storage(&over_limit, artifacts_dir);
        match result {
            ArtifactStorage::Inline(_) => panic!("data over limit should be on disk"),
            ArtifactStorage::OnDisk(path) => {
                assert!(path.starts_with(artifacts_dir));
                let expected_hash = content_hash(&over_limit);
                assert!(path.ends_with(&expected_hash));
            }
        }
    }

    #[test]
    fn classify_storage_large_data() {
        let artifacts_dir = Path::new(".tm/artifacts");
        let large_data = vec![7u8; 1024 * 1024]; // 1 MiB
        let result = classify_storage(&large_data, artifacts_dir);
        match result {
            ArtifactStorage::Inline(_) => panic!("large data should be on disk"),
            ArtifactStorage::OnDisk(path) => {
                assert!(path.starts_with(artifacts_dir));
                let expected_hash = content_hash(&large_data);
                assert_eq!(path.file_name().unwrap(), expected_hash);
            }
        }
    }

    #[test]
    fn classify_storage_on_disk_preserves_content_hash() {
        let artifacts_dir = Path::new("/project/.tm/artifacts");
        let data = vec![123u8; INLINE_LIMIT_BYTES + 100];
        let hash = content_hash(&data);
        let result = classify_storage(&data, artifacts_dir);
        match result {
            ArtifactStorage::OnDisk(path) => {
                let filename = path.file_name().unwrap().to_string_lossy();
                assert_eq!(filename.as_ref(), hash);
            }
            ArtifactStorage::Inline(_) => panic!("should be on disk"),
        }
    }

    #[test]
    fn classify_storage_inline_preserves_bytes() {
        let artifacts_dir = Path::new(".tm/artifacts");
        let original = b"test data for inline storage";
        let result = classify_storage(original, artifacts_dir);
        match result {
            ArtifactStorage::Inline(data) => {
                assert_eq!(data.as_slice(), original);
            }
            ArtifactStorage::OnDisk(_) => panic!("should be inline"),
        }
    }

    #[test]
    fn artifact_kind_serialize() {
        assert_eq!(
            serde_json::to_string(&ArtifactKind::CommandOutput).unwrap(),
            "\"CommandOutput\""
        );
        assert_eq!(
            serde_json::to_string(&ArtifactKind::Patch).unwrap(),
            "\"Patch\""
        );
        assert_eq!(
            serde_json::to_string(&ArtifactKind::File).unwrap(),
            "\"File\""
        );
        assert_eq!(
            serde_json::to_string(&ArtifactKind::Report).unwrap(),
            "\"Report\""
        );
        assert_eq!(
            serde_json::to_string(&ArtifactKind::Index).unwrap(),
            "\"Index\""
        );
        assert_eq!(
            serde_json::to_string(&ArtifactKind::Benchmark).unwrap(),
            "\"Benchmark\""
        );
        assert_eq!(
            serde_json::to_string(&ArtifactKind::Transcript).unwrap(),
            "\"Transcript\""
        );
    }

    #[test]
    fn evidence_kind_serialize() {
        assert_eq!(
            serde_json::to_string(&EvidenceKind::TestRun).unwrap(),
            "\"TestRun\""
        );
        assert_eq!(
            serde_json::to_string(&EvidenceKind::Diff).unwrap(),
            "\"Diff\""
        );
        assert_eq!(
            serde_json::to_string(&EvidenceKind::CommandOutput).unwrap(),
            "\"CommandOutput\""
        );
        assert_eq!(
            serde_json::to_string(&EvidenceKind::Review).unwrap(),
            "\"Review\""
        );
        assert_eq!(
            serde_json::to_string(&EvidenceKind::HumanAttestation).unwrap(),
            "\"HumanAttestation\""
        );
    }

    #[test]
    fn artifact_storage_inline_variant() {
        let data = vec![1, 2, 3];
        let storage = ArtifactStorage::Inline(data.clone());
        assert_eq!(storage, ArtifactStorage::Inline(data));
    }

    #[test]
    fn artifact_storage_on_disk_variant() {
        let path = PathBuf::from(".tm/artifacts/abc123");
        let storage = ArtifactStorage::OnDisk(path.clone());
        assert_eq!(storage, ArtifactStorage::OnDisk(path));
    }
}
