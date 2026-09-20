+++
[doc]
id = "wiki/architecture/xtask"
mode = "generated"
derived_from = ["crates/xtask/src/**"]
+++

# Architecture: xtask

## Module tree

- `crates/xtask/src/hygiene.rs`
- `crates/xtask/src/main.rs`

## Public symbols

### `crates/xtask/src/hygiene.rs`

- `pub fn run_all(root: &Path) -> Vec<String>`
- `pub fn check_time_and_rand(root: &Path) -> Vec<String>`
- `pub fn check_events_mutation(root: &Path) -> Vec<String>`
- `pub fn check_network_in_tests(root: &Path) -> Vec<String>`
- `pub fn check_unwrap_expect(root: &Path) -> Vec<String>`
- `pub fn check_crate_descriptions(root: &Path) -> Vec<String>`
