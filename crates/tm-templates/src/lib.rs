#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! Project scaffolding templates (`SPEC.md` §27): deterministic, reviewable, versioned starting
//! points a project can be seeded from, distinct from Genesis's spec-to-graph compilation
//! (`tm-genesis`). A template is ordinary software — a file tree plus a manifest, a
//! `verify.toml` proving it builds, and a `skill.md` teaching its stack's idioms — not something
//! re-derived by a model on every use.
//!
//! - [`manifest`]: [`manifest::TemplateManifest`], parsed from `manifest.toml`, plus the blake3
//!   content checksum a registry pin is checked against.
//! - [`template`]: [`template::Template`], a manifest plus the on-disk directory it came from.
//! - [`apply`]: copy a template's `files/` into a destination, substituting `{{param}}`
//!   placeholders.
//! - [`verify`]: run a template's `verify.toml` through `tm_context::command::run` — the same
//!   gated, authority-checked path every other command in this codebase runs through.
//! - [`skill`]: load a template's `skill.md` as a `tm_context::pack::Section` — data a worker
//!   can read, never instructions injected into a system prompt.
//! - [`registry`]: [`registry::TemplateRegistry`], `templates.toml`'s source/version/checksum
//!   pins, so a template can't silently drift out from under a project that depends on it.

pub mod apply;
pub mod manifest;
pub mod registry;
pub mod skill;
pub mod template;
pub mod verify;

pub use apply::apply;
pub use manifest::{ParamSpec, ParamType, TemplateManifest};
pub use registry::{RegistryEntry, TemplateRegistry, TemplateSource};
pub use template::Template;
pub use verify::{VerifyCheck, VerifyOutcome, VerifyReport, VerifySpec};
