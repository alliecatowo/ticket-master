//! Re-export: the implementation lives in `tm_types::sanitize` so non-TUI code (CLI output,
//! notifications) can use it too.

pub use tm_types::sanitize::sanitize;
