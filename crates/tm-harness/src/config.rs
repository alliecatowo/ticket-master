//! The `harness.toml` schema: complete defaults, strict validation, and a stable config hash.
//!
//! [`HarnessConfig`] is the whole of a harness epoch's content ([`crate::epoch::HarnessEpoch`]
//! wraps one alongside promotion metadata). Every field has a default, so a fresh project gets a
//! working harness with no configuration at all. Every struct here is `#[serde(deny_unknown_fields)]`:
//! a typo in a policy knob is a parse error, not a silently ignored key. [`HarnessConfig::config_hash`]
//! is what [`crate::epoch::HarnessEpoch`] and session pinning compare by, so it must be a pure,
//! deterministic function of the parsed value — independent of key order or formatting in the
//! source TOML.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use tm_types::Role;

/// A stable content hash of a [`HarnessConfig`], used to detect whether two configs (or a config
/// and its on-disk source) are the same for pinning and promotion purposes.
///
/// Wraps the lowercase hex encoding of a blake3 digest of the config's canonical serialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ConfigHash(pub [u8; 32]);

impl ConfigHash {
    /// The hash as lowercase hex.
    pub fn to_hex(&self) -> String {
        blake3::Hash::from(self.0).to_hex().to_string()
    }
}

impl fmt::Display for ConfigHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

/// Why parsing or validating a `harness.toml` document failed.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The document was not valid TOML, or contained an unknown key, a wrong-typed value, or a
    /// missing required key. `toml`'s deserializer already enforces `deny_unknown_fields` per
    /// struct, so a typo'd policy knob lands here rather than being dropped.
    #[error("harness.toml is invalid: {0}")]
    Toml(String),
    /// The document parsed but failed a semantic invariant [`HarnessConfig::validate`] checks
    /// (e.g. a weight outside `[0, 1]`, a zero timeout, an empty role mapping for a required role).
    #[error("harness.toml is invalid: {field}: {message}")]
    Validation {
        /// Dotted path to the offending field, e.g. `"routing.semantic_weight"`.
        field: String,
        /// Human-readable description of the violated invariant.
        message: String,
    },
}

/// Preferred tools and their invocation limits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolPreferences {
    /// Shell used to run `RunCommand` actions, e.g. `"/bin/zsh"`.
    pub default_shell: String,
    /// Tool name to prefer for exact/literal lookups (e.g. `"rg"`, `"zvec_grep_rg"`).
    pub exact_match_tool: String,
    /// Tool name to prefer for semantic/fuzzy lookups (e.g. `"zvec_grep_search"`).
    pub semantic_search_tool: String,
    /// Unified vs. side-by-side diff rendering when the harness shows an edit.
    pub diff_style: DiffStyle,
    /// Upper bound on tool calls issued concurrently by one worker.
    pub max_parallel_tool_calls: u32,
}

/// How a diff is rendered to a reviewer or in a transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffStyle {
    /// A single unified hunk-style diff.
    Unified,
    /// Two columns, before and after.
    SideBySide,
}

/// Weights the retrieval layer uses to rank and route search results.
///
/// Each `*_weight` field is expected to lie in `[0.0, 1.0]`; [`HarnessConfig::validate`] enforces
/// this. The weights need not sum to 1 — they scale independent signal contributions, not a
/// probability distribution.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingWeights {
    /// Weight given to an exact anchor match (identifier, filename, literal).
    pub exact_anchor_weight: f64,
    /// Weight given to semantic/embedding similarity.
    pub semantic_weight: f64,
    /// Weight given to recency of the matched content.
    pub recency_weight: f64,
    /// Weight given to path proximity to the ticket's declared scope.
    pub path_proximity_weight: f64,
    /// Minimum combined relevance score a hit must clear to be surfaced at all.
    pub min_relevance_score: f64,
}

/// Policy governing how much and what kind of material gets compiled into a worker's context.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextPolicy {
    /// Hard cap on tokens compiled into a single worker's context.
    pub max_context_tokens: u32,
    /// Hard cap on bytes read from any single file when compiling context.
    pub max_file_bytes: u64,
    /// Fraction of `max_context_tokens` at which the compiler starts summarizing/dropping
    /// low-relevance material instead of truncating outright. In `[0.0, 1.0]`.
    pub compaction_threshold: f64,
    /// Whether to include the working tree's git diff against the ticket's base in context.
    pub include_git_diff: bool,
    /// Whether to include summaries of related tickets (dependencies, siblings) in context.
    pub include_related_tickets: bool,
}

/// A model and its guardrails for one [`Role`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleBinding {
    /// Provider-qualified model identifier, e.g. `"anthropic/claude-sonnet-5"`.
    pub model: String,
    /// How strictly the resolved model must match [`Role::default_tolerance`]'s intent; a
    /// harness may relax this per role (e.g. allow a cheaper substitute) but never widen it
    /// past what `tm-provider` accepts for the role.
    pub tolerance: tm_types::Tolerance,
    /// Per-call token budget for workers running this role.
    pub max_tokens: u32,
}

/// Role→capability mapping: which model backs each [`Role`], and with what limits.
///
/// Serializes as a TOML table keyed by [`Role::config_key`] (e.g. `[roles.coder_fast]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, transparent)]
pub struct RoleMapping {
    /// One binding per configured role. Roles absent from the map have no default binding and
    /// must be rejected at the point of use, not silently substituted.
    pub bindings: BTreeMap<Role, RoleBinding>,
}

impl RoleMapping {
    /// The binding configured for `role`, if any.
    pub fn get(&self, role: Role) -> Option<&RoleBinding> {
        self.bindings.get(&role)
    }
}

/// Defaults applied to verification when a ticket doesn't override them.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationDefaults {
    /// Require a `TestsPass` predicate to hold before a ticket may close, when the ticket's
    /// scope has a discoverable test suite.
    pub require_tests_pass: bool,
    /// Enforce that the auditor participant differs from the executor (belt-and-suspenders on
    /// top of `tm-core`'s hard invariant of the same name).
    pub require_auditor_distinct_from_executor: bool,
    /// Default timeout for evaluating one machine-checkable predicate.
    pub default_predicate_timeout_seconds: u32,
    /// Maximum number of automatic re-verification attempts before escalating to a human.
    pub max_verification_retries: u32,
    /// Whether `HumanAttested` predicates are accepted at all in this project.
    pub human_attestation_allowed: bool,
}

/// Named prompt fragments injected into worker system/context prompts.
///
/// `system_preamble` and `closing_reminder` are the two fragments every role's prompt includes;
/// `extra` holds any additional named fragments a harness author wants to reference from role- or
/// command-specific templates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptFragments {
    /// Prepended to every worker's system prompt.
    pub system_preamble: String,
    /// Appended as a final reminder before a worker's turn ends.
    pub closing_reminder: String,
    /// Additional named fragments, keyed by fragment name.
    #[serde(default)]
    pub extra: BTreeMap<String, String>,
}

/// How shell commands issued by workers are run and retried.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandPolicy {
    /// Wall-clock timeout for one command invocation.
    pub shell_timeout_seconds: u32,
    /// Truncate captured stdout/stderr beyond this many bytes.
    pub max_output_bytes: u64,
    /// Whether a command that fails with a transient-looking error is retried automatically.
    pub retry_on_transient_failure: bool,
    /// Maximum automatic retries per command.
    pub max_retries: u32,
    /// Refuse commands that require an interactive TTY outright, rather than hanging.
    pub deny_interactive: bool,
}

/// How workers are expected to read and edit files.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditingPolicy {
    /// Above this many changed lines, prefer a patch/diff-style edit over a full rewrite.
    pub max_diff_lines: u32,
    /// Require a file to have been read in the current session before it may be edited.
    pub require_read_before_edit: bool,
    /// Prefer minimal patches over rewriting whole files when both are possible.
    pub prefer_patch_over_rewrite: bool,
    /// Whether harness-managed formatters run automatically after a file write.
    pub auto_format_on_save: bool,
}

/// The complete `harness.toml` schema: one project's harness policy.
///
/// `schema_version` guards forward compatibility of the file format itself, independent of
/// [`crate::epoch::HarnessEpoch::number`], which versions the *content* of a promoted config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessConfig {
    /// Format version of this document. Bumped only when the schema itself changes shape.
    pub schema_version: u32,
    /// Tool preferences.
    pub tools: ToolPreferences,
    /// Search routing weights.
    pub routing: RoutingWeights,
    /// Context compilation policy.
    pub context: ContextPolicy,
    /// Role→capability mapping.
    pub roles: RoleMapping,
    /// Verification policy defaults.
    pub verification: VerificationDefaults,
    /// Prompt fragments.
    pub prompts: PromptFragments,
    /// Command handling policy.
    pub commands: CommandPolicy,
    /// Editing behavior policy.
    pub editing: EditingPolicy,
}

/// The schema version this crate writes and expects; see [`HarnessConfig::schema_version`].
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

impl Default for HarnessConfig {
    /// A complete, working default harness. Every project gets this if `harness.toml` is absent.
    fn default() -> Self {
        HarnessConfig {
            schema_version: CURRENT_SCHEMA_VERSION,
            tools: ToolPreferences {
                default_shell: "/bin/sh".to_string(),
                exact_match_tool: "rg".to_string(),
                semantic_search_tool: "zvec_grep_search".to_string(),
                diff_style: DiffStyle::Unified,
                max_parallel_tool_calls: 4,
            },
            routing: RoutingWeights {
                exact_anchor_weight: 0.4,
                semantic_weight: 0.4,
                recency_weight: 0.1,
                path_proximity_weight: 0.1,
                min_relevance_score: 0.2,
            },
            context: ContextPolicy {
                max_context_tokens: 100_000,
                max_file_bytes: 1_000_000,
                compaction_threshold: 0.8,
                include_git_diff: true,
                include_related_tickets: true,
            },
            roles: RoleMapping {
                bindings: BTreeMap::new(),
            },
            verification: VerificationDefaults {
                require_tests_pass: true,
                require_auditor_distinct_from_executor: true,
                default_predicate_timeout_seconds: 600,
                max_verification_retries: 2,
                human_attestation_allowed: true,
            },
            prompts: PromptFragments {
                system_preamble: String::new(),
                closing_reminder: String::new(),
                extra: BTreeMap::new(),
            },
            commands: CommandPolicy {
                shell_timeout_seconds: 120,
                max_output_bytes: 1_000_000,
                retry_on_transient_failure: true,
                max_retries: 1,
                deny_interactive: true,
            },
            editing: EditingPolicy {
                max_diff_lines: 400,
                require_read_before_edit: true,
                prefer_patch_over_rewrite: true,
                auto_format_on_save: false,
            },
        }
    }
}

impl HarnessConfig {
    /// Parse and validate a `harness.toml` document.
    pub fn parse(source: &str) -> Result<HarnessConfig, ConfigError> {
        let config: HarnessConfig =
            toml::from_str(source).map_err(|e| ConfigError::Toml(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    /// Check semantic invariants `serde` can't express: weight ranges, non-zero timeouts,
    /// non-empty required collections.
    pub fn validate(&self) -> Result<(), ConfigError> {
        // Validate routing weights
        if !(0.0..=1.0).contains(&self.routing.exact_anchor_weight) {
            return Err(ConfigError::Validation {
                field: "routing.exact_anchor_weight".to_string(),
                message: "must be in [0.0, 1.0]".to_string(),
            });
        }
        if !(0.0..=1.0).contains(&self.routing.semantic_weight) {
            return Err(ConfigError::Validation {
                field: "routing.semantic_weight".to_string(),
                message: "must be in [0.0, 1.0]".to_string(),
            });
        }
        if !(0.0..=1.0).contains(&self.routing.recency_weight) {
            return Err(ConfigError::Validation {
                field: "routing.recency_weight".to_string(),
                message: "must be in [0.0, 1.0]".to_string(),
            });
        }
        if !(0.0..=1.0).contains(&self.routing.path_proximity_weight) {
            return Err(ConfigError::Validation {
                field: "routing.path_proximity_weight".to_string(),
                message: "must be in [0.0, 1.0]".to_string(),
            });
        }
        if !(0.0..=1.0).contains(&self.routing.min_relevance_score) {
            return Err(ConfigError::Validation {
                field: "routing.min_relevance_score".to_string(),
                message: "must be in [0.0, 1.0]".to_string(),
            });
        }

        // Validate context policy
        if !(0.0..=1.0).contains(&self.context.compaction_threshold) {
            return Err(ConfigError::Validation {
                field: "context.compaction_threshold".to_string(),
                message: "must be in [0.0, 1.0]".to_string(),
            });
        }
        if self.context.max_context_tokens == 0 {
            return Err(ConfigError::Validation {
                field: "context.max_context_tokens".to_string(),
                message: "must be greater than 0".to_string(),
            });
        }
        if self.context.max_file_bytes == 0 {
            return Err(ConfigError::Validation {
                field: "context.max_file_bytes".to_string(),
                message: "must be greater than 0".to_string(),
            });
        }

        // Validate verification defaults
        if self.verification.default_predicate_timeout_seconds == 0 {
            return Err(ConfigError::Validation {
                field: "verification.default_predicate_timeout_seconds".to_string(),
                message: "must be greater than 0".to_string(),
            });
        }

        // Validate command policy
        if self.commands.shell_timeout_seconds == 0 {
            return Err(ConfigError::Validation {
                field: "commands.shell_timeout_seconds".to_string(),
                message: "must be greater than 0".to_string(),
            });
        }

        // Validate editing policy
        if self.editing.max_diff_lines == 0 {
            return Err(ConfigError::Validation {
                field: "editing.max_diff_lines".to_string(),
                message: "must be greater than 0".to_string(),
            });
        }

        Ok(())
    }

    /// A stable hash of this config's content, independent of source formatting or key order.
    pub fn config_hash(&self) -> ConfigHash {
        let bytes = serde_json::to_vec(self)
            .expect("HarnessConfig is serializable; serde_json::to_vec cannot fail");
        let hash = blake3::hash(&bytes);
        ConfigHash(*hash.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_config() -> HarnessConfig {
        HarnessConfig::default()
    }

    #[test]
    fn parse_valid_minimal_config() {
        let source = r#"
schema_version = 1
[tools]
default_shell = "/bin/sh"
exact_match_tool = "rg"
semantic_search_tool = "zvec_grep_search"
diff_style = "unified"
max_parallel_tool_calls = 4
[routing]
exact_anchor_weight = 0.4
semantic_weight = 0.4
recency_weight = 0.1
path_proximity_weight = 0.1
min_relevance_score = 0.2
[context]
max_context_tokens = 100000
max_file_bytes = 1000000
compaction_threshold = 0.8
include_git_diff = true
include_related_tickets = true
[roles]
[verification]
require_tests_pass = true
require_auditor_distinct_from_executor = true
default_predicate_timeout_seconds = 600
max_verification_retries = 2
human_attestation_allowed = true
[prompts]
system_preamble = ""
closing_reminder = ""
[commands]
shell_timeout_seconds = 120
max_output_bytes = 1000000
retry_on_transient_failure = true
max_retries = 1
deny_interactive = true
[editing]
max_diff_lines = 400
require_read_before_edit = true
prefer_patch_over_rewrite = true
auto_format_on_save = false
"#;
        let config = HarnessConfig::parse(source);
        assert!(config.is_ok(), "Failed to parse valid config: {config:?}");
    }

    #[test]
    fn parse_unknown_field_rejected() {
        let source = r#"
schema_version = 1
unknown_field = "should error"
[tools]
default_shell = "/bin/sh"
exact_match_tool = "rg"
semantic_search_tool = "zvec_grep_search"
diff_style = "unified"
max_parallel_tool_calls = 4
[routing]
exact_anchor_weight = 0.4
semantic_weight = 0.4
recency_weight = 0.1
path_proximity_weight = 0.1
min_relevance_score = 0.2
[context]
max_context_tokens = 100000
max_file_bytes = 1000000
compaction_threshold = 0.8
include_git_diff = true
include_related_tickets = true
[roles]
[verification]
require_tests_pass = true
require_auditor_distinct_from_executor = true
default_predicate_timeout_seconds = 600
max_verification_retries = 2
human_attestation_allowed = true
[prompts]
system_preamble = ""
closing_reminder = ""
[commands]
shell_timeout_seconds = 120
max_output_bytes = 1000000
retry_on_transient_failure = true
max_retries = 1
deny_interactive = true
[editing]
max_diff_lines = 400
require_read_before_edit = true
prefer_patch_over_rewrite = true
auto_format_on_save = false
"#;
        let config = HarnessConfig::parse(source);
        assert!(config.is_err(), "Should reject unknown field");
        match config.unwrap_err() {
            ConfigError::Toml(_) => {}
            _ => panic!("Expected Toml error for unknown field"),
        }
    }

    #[test]
    fn parse_invalid_toml() {
        let source = r#"
schema_version = 1
[tools
invalid toml
"#;
        let config = HarnessConfig::parse(source);
        assert!(config.is_err(), "Should reject invalid TOML");
        match config.unwrap_err() {
            ConfigError::Toml(_) => {}
            _ => panic!("Expected Toml error for invalid syntax"),
        }
    }

    #[test]
    fn validate_exact_anchor_weight_below_range() {
        let mut config = minimal_config();
        config.routing.exact_anchor_weight = -0.1;
        let result = config.validate();
        assert!(result.is_err());
        match result.unwrap_err() {
            ConfigError::Validation { field, message } => {
                assert_eq!(field, "routing.exact_anchor_weight");
                assert_eq!(message, "must be in [0.0, 1.0]");
            }
            _ => panic!("Expected validation error"),
        }
    }

    #[test]
    fn validate_exact_anchor_weight_above_range() {
        let mut config = minimal_config();
        config.routing.exact_anchor_weight = 1.5;
        let result = config.validate();
        assert!(result.is_err());
        match result.unwrap_err() {
            ConfigError::Validation { field, message } => {
                assert_eq!(field, "routing.exact_anchor_weight");
                assert_eq!(message, "must be in [0.0, 1.0]");
            }
            _ => panic!("Expected validation error"),
        }
    }

    #[test]
    fn validate_semantic_weight_out_of_range() {
        let mut config = minimal_config();
        config.routing.semantic_weight = 2.0;
        let result = config.validate();
        assert!(result.is_err());
        match result.unwrap_err() {
            ConfigError::Validation { field, message } => {
                assert_eq!(field, "routing.semantic_weight");
                assert_eq!(message, "must be in [0.0, 1.0]");
            }
            _ => panic!("Expected validation error"),
        }
    }

    #[test]
    fn validate_recency_weight_out_of_range() {
        let mut config = minimal_config();
        config.routing.recency_weight = -1.0;
        let result = config.validate();
        assert!(result.is_err());
        match result.unwrap_err() {
            ConfigError::Validation { field, message } => {
                assert_eq!(field, "routing.recency_weight");
                assert_eq!(message, "must be in [0.0, 1.0]");
            }
            _ => panic!("Expected validation error"),
        }
    }

    #[test]
    fn validate_path_proximity_weight_out_of_range() {
        let mut config = minimal_config();
        config.routing.path_proximity_weight = 1.5;
        let result = config.validate();
        assert!(result.is_err());
        match result.unwrap_err() {
            ConfigError::Validation { field, message } => {
                assert_eq!(field, "routing.path_proximity_weight");
                assert_eq!(message, "must be in [0.0, 1.0]");
            }
            _ => panic!("Expected validation error"),
        }
    }

    #[test]
    fn validate_min_relevance_score_out_of_range() {
        let mut config = minimal_config();
        config.routing.min_relevance_score = 1.1;
        let result = config.validate();
        assert!(result.is_err());
        match result.unwrap_err() {
            ConfigError::Validation { field, message } => {
                assert_eq!(field, "routing.min_relevance_score");
                assert_eq!(message, "must be in [0.0, 1.0]");
            }
            _ => panic!("Expected validation error"),
        }
    }

    #[test]
    fn validate_compaction_threshold_out_of_range() {
        let mut config = minimal_config();
        config.context.compaction_threshold = 1.2;
        let result = config.validate();
        assert!(result.is_err());
        match result.unwrap_err() {
            ConfigError::Validation { field, message } => {
                assert_eq!(field, "context.compaction_threshold");
                assert_eq!(message, "must be in [0.0, 1.0]");
            }
            _ => panic!("Expected validation error"),
        }
    }

    #[test]
    fn validate_max_context_tokens_zero() {
        let mut config = minimal_config();
        config.context.max_context_tokens = 0;
        let result = config.validate();
        assert!(result.is_err());
        match result.unwrap_err() {
            ConfigError::Validation { field, message } => {
                assert_eq!(field, "context.max_context_tokens");
                assert_eq!(message, "must be greater than 0");
            }
            _ => panic!("Expected validation error"),
        }
    }

    #[test]
    fn validate_max_file_bytes_zero() {
        let mut config = minimal_config();
        config.context.max_file_bytes = 0;
        let result = config.validate();
        assert!(result.is_err());
        match result.unwrap_err() {
            ConfigError::Validation { field, message } => {
                assert_eq!(field, "context.max_file_bytes");
                assert_eq!(message, "must be greater than 0");
            }
            _ => panic!("Expected validation error"),
        }
    }

    #[test]
    fn validate_default_predicate_timeout_zero() {
        let mut config = minimal_config();
        config.verification.default_predicate_timeout_seconds = 0;
        let result = config.validate();
        assert!(result.is_err());
        match result.unwrap_err() {
            ConfigError::Validation { field, message } => {
                assert_eq!(field, "verification.default_predicate_timeout_seconds");
                assert_eq!(message, "must be greater than 0");
            }
            _ => panic!("Expected validation error"),
        }
    }

    #[test]
    fn validate_shell_timeout_zero() {
        let mut config = minimal_config();
        config.commands.shell_timeout_seconds = 0;
        let result = config.validate();
        assert!(result.is_err());
        match result.unwrap_err() {
            ConfigError::Validation { field, message } => {
                assert_eq!(field, "commands.shell_timeout_seconds");
                assert_eq!(message, "must be greater than 0");
            }
            _ => panic!("Expected validation error"),
        }
    }

    #[test]
    fn validate_max_diff_lines_zero() {
        let mut config = minimal_config();
        config.editing.max_diff_lines = 0;
        let result = config.validate();
        assert!(result.is_err());
        match result.unwrap_err() {
            ConfigError::Validation { field, message } => {
                assert_eq!(field, "editing.max_diff_lines");
                assert_eq!(message, "must be greater than 0");
            }
            _ => panic!("Expected validation error"),
        }
    }

    #[test]
    fn validate_weights_at_boundary_zero() {
        let mut config = minimal_config();
        config.routing.exact_anchor_weight = 0.0;
        config.routing.semantic_weight = 0.0;
        config.routing.recency_weight = 0.0;
        config.routing.path_proximity_weight = 0.0;
        config.routing.min_relevance_score = 0.0;
        let result = config.validate();
        assert!(result.is_ok(), "Weights at 0.0 should be valid");
    }

    #[test]
    fn validate_weights_at_boundary_one() {
        let mut config = minimal_config();
        config.routing.exact_anchor_weight = 1.0;
        config.routing.semantic_weight = 1.0;
        config.routing.recency_weight = 1.0;
        config.routing.path_proximity_weight = 1.0;
        config.routing.min_relevance_score = 1.0;
        let result = config.validate();
        assert!(result.is_ok(), "Weights at 1.0 should be valid");
    }

    #[test]
    fn config_hash_deterministic() {
        let config1 = minimal_config();
        let config2 = minimal_config();
        assert_eq!(
            config1.config_hash(),
            config2.config_hash(),
            "Same configs should produce same hash"
        );
    }

    #[test]
    fn config_hash_sensitive_to_changes() {
        let mut config1 = minimal_config();
        let config2 = minimal_config();
        config1.routing.exact_anchor_weight = 0.5;
        assert_ne!(
            config1.config_hash(),
            config2.config_hash(),
            "Different configs should produce different hashes"
        );
    }

    #[test]
    fn config_hash_hex_representation() {
        let config = minimal_config();
        let hash = config.config_hash();
        let hex = hash.to_hex();
        assert_eq!(hex.len(), 64, "blake3 hash should be 64 hex characters");
        assert!(
            hex.chars().all(|c| c.is_ascii_hexdigit()),
            "Hash should be valid hex"
        );
    }

    #[test]
    fn empty_roles_allowed() {
        let config = minimal_config();
        assert!(
            config.validate().is_ok(),
            "Empty role bindings should be valid"
        );
    }

    #[test]
    fn parse_validates_before_returning() {
        let source = r#"
schema_version = 1
[tools]
default_shell = "/bin/sh"
exact_match_tool = "rg"
semantic_search_tool = "zvec_grep_search"
diff_style = "unified"
max_parallel_tool_calls = 4
[routing]
exact_anchor_weight = 2.0
semantic_weight = 0.4
recency_weight = 0.1
path_proximity_weight = 0.1
min_relevance_score = 0.2
[context]
max_context_tokens = 100000
max_file_bytes = 1000000
compaction_threshold = 0.8
include_git_diff = true
include_related_tickets = true
[roles]
[verification]
require_tests_pass = true
require_auditor_distinct_from_executor = true
default_predicate_timeout_seconds = 600
max_verification_retries = 2
human_attestation_allowed = true
[prompts]
system_preamble = ""
closing_reminder = ""
[commands]
shell_timeout_seconds = 120
max_output_bytes = 1000000
retry_on_transient_failure = true
max_retries = 1
deny_interactive = true
[editing]
max_diff_lines = 400
require_read_before_edit = true
prefer_patch_over_rewrite = true
auto_format_on_save = false
"#;
        let config = HarnessConfig::parse(source);
        assert!(config.is_err(), "Parse should fail validation");
        match config.unwrap_err() {
            ConfigError::Validation { .. } => {}
            e => panic!("Expected validation error, got: {e:?}"),
        }
    }

    #[test]
    fn config_hash_display() {
        let config = minimal_config();
        let hash = config.config_hash();
        let display_str = format!("{}", hash);
        let hex_str = hash.to_hex();
        assert_eq!(display_str, hex_str, "Display should match to_hex");
    }
}
