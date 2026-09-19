//! Artifacts and evidence: content-addressed storage (`SPEC.md` §4.6).
//!
//! Artifacts are addressed by their blake3 hash. Small ones (<= 64 KiB) live inline in SQLite;
//! larger ones spill to `<state_dir>/artifacts/<hash>` on disk, where `state_dir` is
//! `Store::state_dir()` (`<root>/.tm` for a repo-scoped project, `$TM_HOME/projects/<key>/` for a
//! global-scope one — see D-003). This module owns the pure hashing/threshold/path logic;
//! `Store` owns the actual filesystem and SQLite I/O.

use std::path::{Path, PathBuf};

use serde_json::Value as Json;
use tm_types::{ArtifactId, ParticipantId, TicketId, Timestamp};

/// Bytes at or below this size are stored inline in SQLite rather than spilled to disk.
pub const INLINE_LIMIT_BYTES: usize = 64 * 1024;

/// What kind of thing an artifact captures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// Captured stdout/stderr of a run command.
    CommandOutput,
    /// A unified diff / patch.
    Patch,
    /// An arbitrary file snapshot.
    File,
    /// A generated report (verification, audit, benchmark writeup).
    Report,
    /// A code/search index snapshot.
    Index,
    /// Benchmark results.
    Benchmark,
    /// A conversation/session transcript.
    Transcript,
}

/// Where an artifact's bytes actually live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtifactStorage {
    /// Bytes stored directly in the `artifacts` table.
    Inline(Vec<u8>),
    /// Bytes spilled to `<state_dir>/artifacts/<hash>`.
    OnDisk(PathBuf),
}

/// A content-addressed artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    /// Identity.
    pub id: ArtifactId,
    /// What kind of artifact this is.
    pub kind: ArtifactKind,
    /// MIME-ish media type, e.g. `text/plain`, `application/json`.
    pub media_type: String,
    /// Length of the artifact's bytes, regardless of storage location.
    pub bytes_len: u64,
    /// The blake3 hash of the artifact's bytes, hex-encoded.
    pub hash: String,
    /// Where the bytes live.
    pub storage: ArtifactStorage,
    /// Free-form metadata (e.g. the command that produced a `CommandOutput`).
    pub meta: Json,
}

/// What an [`Evidence`] record demonstrates about a ticket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    /// A test suite run.
    TestRun,
    /// A diff review.
    Diff,
    /// Raw command output.
    CommandOutput,
    /// A human or agent review writeup.
    Review,
    /// A human's direct attestation ([`tm_types::Predicate::HumanAttested`]).
    HumanAttestation,
}

/// Links a ticket to the artifact that proves something about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    /// The ticket this evidence is about.
    pub ticket: TicketId,
    /// What it demonstrates.
    pub kind: EvidenceKind,
    /// The artifact backing this evidence.
    pub artifact: ArtifactId,
    /// Who/what produced it.
    pub produced_by: ParticipantId,
    /// When it was recorded.
    pub ts: Timestamp,
    /// One-line human-readable summary.
    pub summary: String,
}

/// Hash `bytes` with blake3, returning the lowercase hex digest used as [`Artifact::hash`] and
/// the `ART-<hex12>` id suffix's source material (the full hash, not truncated — `IdSource`
/// truncates separately when minting an [`ArtifactId`]).
pub fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Decide storage placement for `bytes`: [`ArtifactStorage::Inline`] at or under
/// [`INLINE_LIMIT_BYTES`], otherwise [`ArtifactStorage::OnDisk`] at
/// `<state_dir>/artifacts/<hash>`.
pub fn plan_storage(state_dir: &Path, bytes: &[u8]) -> (String, ArtifactStorage) {
    let hash = hash_bytes(bytes);
    let storage = if bytes.len() <= INLINE_LIMIT_BYTES {
        ArtifactStorage::Inline(bytes.to_vec())
    } else {
        ArtifactStorage::OnDisk(state_dir.join("artifacts").join(&hash))
    };
    (hash, storage)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_bytes_consistency() {
        let bytes1 = b"hello world";
        let hash1 = hash_bytes(bytes1);
        let hash2 = hash_bytes(bytes1);
        assert_eq!(hash1, hash2, "hash_bytes should be deterministic");
    }

    #[test]
    fn test_hash_bytes_different_inputs() {
        let hash1 = hash_bytes(b"hello");
        let hash2 = hash_bytes(b"world");
        assert_ne!(
            hash1, hash2,
            "different inputs should produce different hashes"
        );
    }

    #[test]
    fn test_hash_bytes_empty() {
        let hash = hash_bytes(b"");
        assert!(!hash.is_empty(), "hash of empty bytes should not be empty");
        // blake3 hash of empty is well-known: af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262
        assert_eq!(
            hash,
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
    }

    #[test]
    fn test_plan_storage_small_bytes_inline() {
        let state_dir = Path::new("/test/project/.tm");
        let bytes = b"small content";
        assert!(bytes.len() <= INLINE_LIMIT_BYTES);

        let (hash, storage) = plan_storage(state_dir, bytes);

        assert!(!hash.is_empty(), "hash should not be empty");
        match storage {
            ArtifactStorage::Inline(content) => {
                assert_eq!(content, bytes, "inline storage should contain exact bytes");
            }
            ArtifactStorage::OnDisk(_) => panic!("small bytes should be stored inline"),
        }
    }

    #[test]
    fn test_plan_storage_empty_bytes_inline() {
        let state_dir = Path::new("/test/project/.tm");
        let bytes = b"";

        let (hash, storage) = plan_storage(state_dir, bytes);

        assert_eq!(
            hash,
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
        );
        match storage {
            ArtifactStorage::Inline(content) => {
                assert!(
                    content.is_empty(),
                    "inline storage of empty bytes should be empty"
                );
            }
            ArtifactStorage::OnDisk(_) => panic!("empty bytes should be stored inline"),
        }
    }

    #[test]
    fn test_plan_storage_at_inline_limit() {
        let state_dir = Path::new("/test/project/.tm");
        let bytes = vec![0u8; INLINE_LIMIT_BYTES];

        let (hash, storage) = plan_storage(state_dir, &bytes);

        assert!(!hash.is_empty());
        match storage {
            ArtifactStorage::Inline(content) => {
                assert_eq!(content.len(), INLINE_LIMIT_BYTES);
            }
            ArtifactStorage::OnDisk(_) => {
                panic!("bytes at INLINE_LIMIT_BYTES should be stored inline")
            }
        }
    }

    #[test]
    fn test_plan_storage_just_over_inline_limit() {
        let state_dir = Path::new("/test/project/.tm");
        let bytes = vec![0u8; INLINE_LIMIT_BYTES + 1];

        let (hash, storage) = plan_storage(state_dir, &bytes);

        assert!(!hash.is_empty());
        match storage {
            ArtifactStorage::Inline(_) => {
                panic!("bytes over INLINE_LIMIT_BYTES should be stored on disk")
            }
            ArtifactStorage::OnDisk(path) => {
                assert!(path.to_string_lossy().contains(".tm/artifacts"));
                assert!(path.to_string_lossy().ends_with(&hash));
            }
        }
    }

    #[test]
    fn test_plan_storage_large_bytes_ondisk() {
        let state_dir = Path::new("/test/project/.tm");
        let bytes = vec![0xFFu8; 1024 * 1024]; // 1 MiB

        let (hash, storage) = plan_storage(state_dir, &bytes);

        assert!(!hash.is_empty());
        match storage {
            ArtifactStorage::Inline(_) => panic!("large bytes should be stored on disk"),
            ArtifactStorage::OnDisk(path) => {
                let path_str = path.to_string_lossy();
                assert!(
                    path_str.contains(".tm"),
                    "on-disk path should contain .tm directory"
                );
                assert!(
                    path_str.contains("artifacts"),
                    "on-disk path should contain artifacts subdirectory"
                );
                assert!(
                    path_str.ends_with(&hash),
                    "on-disk path should end with hash: {}",
                    hash
                );
                // Verify path structure
                assert_eq!(path, state_dir.join("artifacts").join(&hash));
            }
        }
    }

    #[test]
    fn test_plan_storage_different_project_roots() {
        let bytes = b"test content";
        let root1 = Path::new("/project/one/.tm");
        let root2 = Path::new("/project/two/.tm");

        let (hash1, storage1) = plan_storage(root1, bytes);
        let (hash2, storage2) = plan_storage(root2, bytes);

        assert_eq!(
            hash1, hash2,
            "same bytes should produce same hash regardless of project"
        );
        // Both should be inline since bytes are small
        match (storage1, storage2) {
            (ArtifactStorage::Inline(c1), ArtifactStorage::Inline(c2)) => {
                assert_eq!(c1, c2);
            }
            _ => panic!("small bytes should always be inline"),
        }
    }

    #[test]
    fn test_plan_storage_path_with_spaces() {
        let state_dir = Path::new("/path with spaces/project/.tm");
        let bytes = vec![0u8; INLINE_LIMIT_BYTES + 100];

        let (_hash, storage) = plan_storage(state_dir, bytes.as_ref());

        match storage {
            ArtifactStorage::OnDisk(path) => {
                assert!(path.starts_with(state_dir));
            }
            _ => panic!("large bytes should be on disk"),
        }
    }

    #[test]
    fn test_artifact_kind_serialization() {
        let kinds = [
            ArtifactKind::CommandOutput,
            ArtifactKind::Patch,
            ArtifactKind::File,
            ArtifactKind::Report,
            ArtifactKind::Index,
            ArtifactKind::Benchmark,
            ArtifactKind::Transcript,
        ];

        for kind in &kinds {
            let json = serde_json::to_value(kind).expect("should serialize");
            let deserialized: ArtifactKind =
                serde_json::from_value(json).expect("should deserialize");
            assert_eq!(kind, &deserialized);
        }
    }

    #[test]
    fn test_evidence_kind_serialization() {
        let kinds = [
            EvidenceKind::TestRun,
            EvidenceKind::Diff,
            EvidenceKind::CommandOutput,
            EvidenceKind::Review,
            EvidenceKind::HumanAttestation,
        ];

        for kind in &kinds {
            let json = serde_json::to_value(kind).expect("should serialize");
            let deserialized: EvidenceKind =
                serde_json::from_value(json).expect("should deserialize");
            assert_eq!(kind, &deserialized);
        }
    }

    #[test]
    fn test_hash_bytes_hex_format() {
        let hash = hash_bytes(b"test");
        // blake3 hashes are 256-bit (32 bytes) = 64 hex characters
        assert_eq!(hash.len(), 64, "blake3 hash should be 64 hex characters");
        assert!(
            hash.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()),
            "hash should be lowercase hex"
        );
    }

    #[test]
    fn test_inline_limit_bytes_constant() {
        assert_eq!(INLINE_LIMIT_BYTES, 64 * 1024);
    }
}
