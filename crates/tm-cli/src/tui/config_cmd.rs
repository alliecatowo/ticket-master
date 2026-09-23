//! `/config` in the chat: show, get, and set project configuration (D-021).
//!
//! The read surface mirrors `tm harness show` and `tm provider list`; `set` runs the exact
//! validation path as `tm harness set` (parse the TOML value, apply it at the dotted path,
//! reparse, validate) and writes the validated candidate that `promote` reads. Nothing here
//! touches the network, spends provider budget, or reads a secret:
//! `harness.toml` carries routing and weights, never credentials.

use tm_tui::chat::transcript::NoticeLevel;
use tm_types::Role;

use crate::ops::load_role_table;
use crate::project::{Project, Scope};

/// What `/config ...` asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigAction {
    /// Dashboard: session, project, harness, and routing snapshot.
    Show,
    /// Read one dotted key out of `harness.toml`.
    Get(String),
    /// Validate `key = value` the way `tm harness set` does.
    Set(String, String),
    /// Anything else: usage.
    Help,
}

/// Parse the argument of `/config` (everything after the name, trimmed).
pub fn parse_config_args(arg: &str) -> ConfigAction {
    let mut words = arg.split_whitespace();
    match words.next() {
        None => ConfigAction::Show,
        Some("show") if words.next().is_none() => ConfigAction::Show,
        Some("get") => match words.next() {
            Some(key) if words.next().is_none() => ConfigAction::Get(key.to_string()),
            _ => ConfigAction::Help,
        },
        Some("set") => {
            let key = words.next();
            let value: String = words.collect::<Vec<_>>().join(" ");
            match (key, value.is_empty()) {
                (Some(key), false) => ConfigAction::Set(key.to_string(), value),
                _ => ConfigAction::Help,
            }
        }
        _ => ConfigAction::Help,
    }
}

/// Read `harness.toml`'s [`tm_harness::HarnessConfig`], or the built-in default when the
/// project has none yet (same fallback `tm harness show` documents: defaults apply).
fn load_harness_config(project: &Project) -> Result<tm_harness::HarnessConfig, String> {
    let path = project.state_dir.join("harness.toml");
    if !path.is_file() {
        return Ok(tm_harness::HarnessConfig::default());
    }
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("Could not read {}: {e}", path.display()))?;
    tm_harness::HarnessConfig::parse(&content).map_err(|e| format!("Invalid harness.toml: {e:?}"))
}

/// The config as a plain TOML table, for dotted-key navigation shared by `get` and `set`.
fn config_table(project: &Project) -> Result<toml::Table, String> {
    let config = load_harness_config(project)?;
    let serialized =
        toml::to_string(&config).map_err(|e| format!("Could not serialize config: {e}"))?;
    toml::from_str::<toml::Table>(&serialized).map_err(|e| format!("Bad config shape: {e}"))
}

/// Look `dotted` up in `doc`.
fn lookup_key<'a>(doc: &'a toml::Table, dotted: &str) -> Option<&'a toml::Value> {
    let mut current = doc;
    let mut keys = dotted.split('.');
    let last = keys.next_back()?;
    for key in keys {
        current = current.get(key)?.as_table()?;
    }
    current.get(last)
}

/// The `/config` (or `/config show`) dashboard: session, project, harness, routing.
pub fn dashboard(
    project: &Project,
    session: &str,
    model: &str,
    ticket: Option<&str>,
) -> Vec<(NoticeLevel, String)> {
    let mut out = Vec::new();
    let scope = match project.scope {
        Scope::Repo => "repository",
        Scope::Global => "global",
    };
    out.push((
        NoticeLevel::Info,
        format!(
            "Project: {} ({scope})\nState: {}\nSession: {session}\nModel: {model}\nTicket: {}",
            project.root.display(),
            project.state_dir.display(),
            ticket.unwrap_or("none"),
        ),
    ));
    let harness_path = project.state_dir.join("harness.toml");
    let harness_line = if harness_path.is_file() {
        match load_harness_config(project) {
            Ok(_) => format!("Harness: {} (valid)", harness_path.display()),
            Err(e) => format!("Harness: {} ({e})", harness_path.display()),
        }
    } else {
        "Harness: no harness.toml yet — built-in defaults apply".to_string()
    };
    let epochs = match project.store.harness_epochs() {
        Ok(rows) => format!("Epochs: {} promoted", rows.len()),
        Err(e) => format!("Epochs: unreadable ({e})"),
    };
    out.push((NoticeLevel::Info, format!("{harness_line}\n{epochs}")));
    match load_role_table(Some(project)) {
        Ok(table) => {
            let fast: Vec<String> = table
                .candidates_for(Role::CoderFast)
                .iter()
                .map(|c| format!("{}/{}", c.provider, c.model))
                .collect();
            out.push((
                NoticeLevel::Info,
                if fast.is_empty() {
                    "coder.fast routes nowhere.".to_string()
                } else {
                    format!("coder.fast → {}", fast.join(", "))
                },
            ));
        }
        Err(e) => out.push((
            NoticeLevel::Warning,
            format!("Could not load role routing: {e}"),
        )),
    }
    out.push((
        NoticeLevel::Info,
        "Usage: /config get <dotted.key> · /config set <key> <toml-value>".to_string(),
    ));
    out
}

/// `/config get <dotted.key>`.
pub fn config_get(project: &Project, key: &str) -> (NoticeLevel, String) {
    match config_table(project) {
        Ok(doc) => match lookup_key(&doc, key) {
            Some(value) => (NoticeLevel::Info, format!("{key} = {value}")),
            None => (
                NoticeLevel::Warning,
                format!("No such key: {key}. Keys mirror `tm harness set`'s dotted paths."),
            ),
        },
        Err(e) => (NoticeLevel::Warning, e),
    }
}

/// `/config set <key> <toml-value>`: validate and persist the same harness candidate as CLI.
pub fn config_set(project: &Project, key: &str, value: &str) -> (NoticeLevel, String) {
    let path = project.state_dir.join("harness.toml");
    let parsed: Result<toml::Value, _> = toml::from_str(value);
    let value = match parsed {
        Ok(value) => value,
        Err(e) => {
            return (NoticeLevel::Warning, format!("Invalid TOML value: {e}"));
        }
    };
    let base = match config_table(project) {
        Ok(doc) => doc,
        Err(e) => return (NoticeLevel::Warning, e),
    };
    let mut doc = base;
    let keys: Vec<&str> = key.split('.').collect();
    if keys.is_empty() {
        return (
            NoticeLevel::Warning,
            "Usage: /config set <dotted.key> <toml-value>".to_string(),
        );
    }
    let mut current = &mut doc;
    for part in &keys[..keys.len() - 1] {
        let entry = current
            .entry(part.to_string())
            .or_insert_with(|| toml::Value::Table(Default::default()));
        match entry.as_table_mut() {
            Some(table) => current = table,
            None => {
                return (
                    NoticeLevel::Warning,
                    format!("{part} is set, but it is not a table — cannot descend into {key}."),
                );
            }
        }
    }
    if let Some(last) = keys.last() {
        current.insert(last.to_string(), value);
    }
    let reserialized = match toml::to_string_pretty(&doc) {
        Ok(s) => s,
        Err(e) => {
            return (
                NoticeLevel::Warning,
                format!("Could not serialize updated config: {e}"),
            );
        }
    };
    match tm_harness::HarnessConfig::parse(&reserialized) {
        Ok(updated) => {
            if let Err(e) = updated.validate() {
                return (NoticeLevel::Warning, format!("Validation failed: {e:?}"));
            }
            match std::fs::write(&path, reserialized) {
                Ok(()) => (NoticeLevel::Success, format!("Saved {key}; promote the next harness epoch to apply.")),
                Err(e) => (NoticeLevel::Warning, format!("Could not save {}: {e}", path.display())),
            }
        }
        Err(e) => (
            NoticeLevel::Warning,
            format!("Invalid updated config: {e:?}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn config_args_parse_show_get_set_and_help() {
        assert_eq!(parse_config_args(""), ConfigAction::Show);
        assert_eq!(parse_config_args("show"), ConfigAction::Show);
        assert_eq!(
            parse_config_args("get routing_weights.recency"),
            ConfigAction::Get("routing_weights.recency".to_string())
        );
        assert_eq!(
            parse_config_args("set routing_weights.recency 0.5"),
            ConfigAction::Set("routing_weights.recency".to_string(), "0.5".to_string())
        );
        // A TOML value may itself contain spaces.
        assert_eq!(
            parse_config_args("set commands.allow [\"git status\"]"),
            ConfigAction::Set("commands.allow".to_string(), "[\"git status\"]".to_string())
        );
        for bad in [
            "get",
            "get a b",
            "set",
            "set onlykey",
            "frobnicate",
            "show extra",
        ] {
            assert_eq!(parse_config_args(bad), ConfigAction::Help, "{bad:?}");
        }
    }

    fn test_project(root: &std::path::Path) -> Project {
        std::fs::create_dir_all(root.join(".tm")).expect("mkdir .tm");
        let clock: Arc<dyn tm_types::Clock> = Arc::new(tm_types::SystemClock);
        let ids: Arc<dyn tm_types::IdSource> = Arc::new(tm_types::CounterIds::seeded(1));
        let store = Arc::new(
            tm_core::Store::open_with(root, clock.clone(), ids.clone()).expect("open store"),
        );
        Project::for_test(root, store, clock, ids)
    }

    #[test]
    fn dashboard_reports_a_project_without_harness() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = test_project(dir.path());
        let notices = dashboard(&project, "S-9", "devpass/m", Some("T-3"));
        let text = notices
            .iter()
            .map(|(_, t)| t.clone())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Session: S-9"), "{text}");
        assert!(text.contains("Model: devpass/m"), "{text}");
        assert!(text.contains("Ticket: T-3"), "{text}");
        assert!(text.contains("no harness.toml yet"), "{text}");
        assert!(text.contains("coder.fast →"), "{text}");
    }

    #[test]
    fn get_and_set_need_a_harness_file_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = test_project(dir.path());
        // Defaults apply, so `get` still answers from the built-in table.
        let (level, text) = config_get(&project, "schema_version");
        assert_eq!(level, NoticeLevel::Info, "{text}");
        assert!(text.contains("schema_version"), "{text}");
        let (level, text) = config_get(&project, "no.such.key");
        assert_eq!(level, NoticeLevel::Warning, "{text}");
        // But `set` refuses without a file, exactly like `tm harness set`.
        let (level, text) = config_set(&project, "routing_weights.recency", "0.5");
        assert_eq!(level, NoticeLevel::Warning, "{text}");
        assert!(text.contains("No harness.toml yet"), "{text}");
    }
}
