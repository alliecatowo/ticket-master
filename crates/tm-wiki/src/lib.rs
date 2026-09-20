//! The project wiki (`SPEC.md` §26): generated documentation pages assembled from real project
//! state, distinct from hand-written docs but tracked by the exact same staleness/provenance
//! machinery `tm-docs` already gives every doc (`SPEC.md` §9).
//!
//! This is a separate crate rather than a module inside `tm-docs` because the two crates have
//! genuinely different responsibility shapes. `tm-docs` is explicitly "pure logic over injected
//! state; nothing reads the wall clock or files directly" (see `crates/tm-docs/src/lib.rs`'s
//! module doc) — assembly is the caller's job. Wiki generation *is* that caller: it reads
//! `Store::view()`, walks `CodeIntel`'s symbol/history retrieval, and writes files to disk. Bolting
//! that onto `tm-docs` would give the one crate two responsibility shapes (pure staleness logic,
//! plus I/O-heavy assembly) instead of one crate depending on the other's types for the contract
//! it must honour. Concretely, this crate depends on `tm_docs::registry::{DocMode, DocRecord}`
//! and `tm_docs::reconcile::apply_regeneration` for the one hard rule it must never violate: a
//! `docs/wiki/*.md` page with `mode = "human"` (or `"maintained"`) is never rewritten. See
//! [`page::write_page`] — it calls `tm-docs`'s own gate function rather than re-implementing the
//! check `SPEC.md` §9 already tests.
//!
//! # Page families
//!
//! Five page families (`SPEC.md` §26.2), each assembled from a real, already-existing retrieval
//! source — no new static analysis or git access is added here:
//!
//! - [`architecture::pages`] — `architecture/<crate>`: module tree + public symbols, from
//!   [`tm_codeintel::CodeIntel::outline`].
//! - [`decisions::pages`] — `decisions/<file-stem>` (with the full supersession chain) plus a
//!   `decisions.md` index, assembled directly from `docs/decisions/D-NNN-*.md` — this repo's one
//!   real decision-doc convention, not `Store::view()`'s `decisions` map (see `decisions`'s own
//!   module doc for why).
//! - [`history::pages`] — `history/<path>`, from [`tm_codeintel::CodeIntel::history_why`].
//! - [`tickets::page`] — `tickets.md`, from `Store::view()`'s `tickets`/`milestones` maps.
//! - [`glossary::page`] — `glossary.md`, from ticket kinds in use and decision subjects.
//!
//! [`generate::run`] assembles every family and writes them under `docs/wiki/`, registering each
//! written page with `Store::register_doc` — the same write path any other doc's registration
//! goes through.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

/// `architecture/<crate>` pages.
pub mod architecture;

/// `decisions/` pages, parsed from `docs/decisions/D-NNN-*.md`, with supersession chains.
pub mod decisions;

/// Wiki generation orchestration: assemble every family and write it to disk ([`generate::run`]),
/// or preview the same assembly without writing anything ([`generate::dry_run`]).
pub mod generate;

/// `glossary` page.
pub mod glossary;

/// `history/<path>` pages.
pub mod history;

/// Page identity, rendering, and the on-disk human-mode write gate.
pub mod page;

/// `tickets` page.
pub mod tickets;

pub use generate::{default_history_paths, dry_run, run, GenerationReport, PageOutcome};
pub use page::{preview_write, write_page, WikiPage, WriteOutcome, WIKI_DIR};
