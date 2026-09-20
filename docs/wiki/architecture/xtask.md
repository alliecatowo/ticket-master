+++
[doc]
id = "wiki/architecture/xtask"
mode = "generated"
derived_from = ["crates/xtask/src/**"]
+++

# Architecture: xtask

## Module tree

- `crates/xtask/src/drift.rs`
- `crates/xtask/src/hygiene.rs`
- `crates/xtask/src/main.rs`

## Public symbols

### `crates/xtask/src/drift.rs`

- `pub fn changed_rs_files(root: &Path, base: &str) -> Result<Vec<String>>`
- `pub fn file_at_rev(root: &Path, rev: &str, relpath: &str) -> Result<Option<String>>`
- `pub fn extract_enum_variant_counts(src: &str) -> HashMap<String, usize>`
- `pub fn find_stale_cardinality_asserts(old: &str, new: &str) -> Vec<(usize, String, usize, String)>`
- `pub fn run(root: &Path, base: &str) -> Result<Vec<String>>`

### `crates/xtask/src/hygiene.rs`

- `pub fn run_all(root: &Path) -> Vec<String>`
- `pub fn check_time_and_rand(root: &Path) -> Vec<String>`
- `pub fn check_events_mutation(root: &Path) -> Vec<String>`
- `pub fn check_network_in_tests(root: &Path) -> Vec<String>`
- `pub fn check_unwrap_expect(root: &Path) -> Vec<String>`
- `pub fn check_crate_descriptions(root: &Path) -> Vec<String>`
- `pub fn check_dot_tm_literals(root: &Path) -> Vec<String>`
- `pub fn check_decision_doc_references(root: &Path) -> Vec<String>`
