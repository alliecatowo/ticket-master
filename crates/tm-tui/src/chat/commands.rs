//! The chat's slash commands: the table, the popup's filter, and the submit-time parser.
//!
//! One table drives all three (the `/` popup, the help overlay, and parsing a submitted line), so
//! a command can never be runnable but undiscoverable, or listed but unparseable.

/// What a slash command does. The chat screen handles [`CommandId::Help`] itself; every other id
/// is handed to the application as a [`crate::screens::chat::ChatAction::Command`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CommandId {
    /// Toggle the shortcuts panel.
    Help,
    /// Start a fresh conversation.
    Clear,
    /// Pick a past conversation to continue.
    Resume,
    /// Summarize the conversation so far to free context.
    Compact,
    /// Show or switch the model.
    Model,
    /// Choose the fast or deep chat role.
    Level,
    /// Model, provider, cwd, scope, mode, ticket, tokens.
    Status,
    /// This session's tokens, turn by turn.
    Cost,
    /// Connect a provider: pick one, see what to set (D-021).
    Connect,
    /// Which provider answers this chat, and what is configured (D-021).
    Provider,
    /// Show and edit project configuration (D-021).
    Config,
    /// Write an instructions file for this project.
    Init,
    /// Hand work to a background worker as a ticket.
    Bg,
    /// Open the tickets screen (Claude Code's agent view).
    Tickets,
    /// Attach this conversation to a ticket.
    Attach,
    /// Detach from the attached ticket.
    Detach,
    /// Record a project decision.
    Decide,
    /// Token use by section, and the attached ticket's prefetched context pack.
    Context,
    /// Toggle the Ctrl+T task checklist.
    Todos,
    /// Search this project's code.
    Search,
    /// Review the working tree's uncommitted changes.
    Review,
    /// Open the project's AGENTS.md in `$EDITOR`.
    Memory,
    /// Save the conversation as a markdown file.
    Export,
    /// Run `tm doctor`'s checks inline.
    Doctor,
    /// Show or set the auto/plan/ask mode.
    Permissions,
    /// List workflows, or start one as a background ticket.
    Workflow,
    /// Open the Kanban board.
    Board,
    /// Open the Milestones tab.
    Milestones,
    /// Open the Timeline tab.
    Timeline,
    /// Open the dependency graph.
    Deps,
    /// Show a ticket's summary inline.
    Ticket,
    /// Activate and queue a ticket.
    Run,
    /// Quit tm.
    Exit,
}

/// Whether a command takes an argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arg {
    /// No argument; anything typed after the name is ignored.
    None,
    /// A required argument, described by its placeholder (`<ticket>`).
    Required(&'static str),
    /// An optional argument (`/model [name]`).
    Optional(&'static str),
}

/// One slash command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlashCommand {
    /// What it does.
    pub id: CommandId,
    /// Its canonical name, without the slash.
    pub name: &'static str,
    /// Other names that run the same command.
    pub aliases: &'static [&'static str],
    /// Its argument, if any.
    pub arg: Arg,
    /// A one-line description for the popup and help.
    pub description: &'static str,
}

/// Every slash command, in the order the popup lists them with an empty filter.
pub const COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        id: CommandId::Help,
        name: "help",
        aliases: &[],
        arg: Arg::None,
        description: "Show the keyboard shortcuts",
    },
    SlashCommand {
        id: CommandId::Clear,
        name: "clear",
        aliases: &["new"],
        arg: Arg::None,
        description: "Start a fresh conversation",
    },
    SlashCommand {
        id: CommandId::Resume,
        name: "resume",
        aliases: &["continue"],
        arg: Arg::None,
        description: "Resume a past conversation",
    },
    SlashCommand {
        id: CommandId::Compact,
        name: "compact",
        aliases: &[],
        arg: Arg::Optional("[focus]"),
        description: "Summarize the conversation to free up context",
    },
    SlashCommand {
        id: CommandId::Model,
        name: "model",
        aliases: &[],
        arg: Arg::Optional("[model]"),
        description: "Show or switch the model",
    },
    SlashCommand {
        id: CommandId::Level,
        name: "level",
        aliases: &[],
        arg: Arg::Required("<fast|deep>"),
        description: "Switch chat between a quick model and a more careful one",
    },
    SlashCommand {
        id: CommandId::Status,
        name: "status",
        aliases: &[],
        arg: Arg::None,
        description: "Model, provider, directory, mode and ticket",
    },
    SlashCommand {
        id: CommandId::Cost,
        name: "cost",
        aliases: &[],
        arg: Arg::None,
        description: "Tokens this session has used, turn by turn",
    },
    SlashCommand {
        id: CommandId::Connect,
        name: "connect",
        aliases: &["auth"],
        arg: Arg::Optional("[provider]"),
        description: "Connect a provider: pick one, see what to set",
    },
    SlashCommand {
        id: CommandId::Provider,
        name: "provider",
        aliases: &[],
        arg: Arg::None,
        description: "Which provider answers this chat, and what is configured",
    },
    SlashCommand {
        id: CommandId::Config,
        name: "config",
        aliases: &["settings"],
        arg: Arg::Optional("[show|get|set]"),
        description: "Show and edit project configuration",
    },
    SlashCommand {
        id: CommandId::Init,
        name: "init",
        aliases: &[],
        arg: Arg::None,
        description: "Write an AGENTS.md with instructions for this project",
    },
    SlashCommand {
        id: CommandId::Bg,
        name: "bg",
        aliases: &["background"],
        arg: Arg::Optional("[task]"),
        description: "Hand work to a background worker as a ticket",
    },
    SlashCommand {
        id: CommandId::Tickets,
        name: "tickets",
        aliases: &["agents", "home"],
        arg: Arg::None,
        description: "Background tickets and workers",
    },
    SlashCommand {
        id: CommandId::Attach,
        name: "attach",
        aliases: &[],
        arg: Arg::Required("<ticket>"),
        description: "Attach this conversation to a ticket",
    },
    SlashCommand {
        id: CommandId::Detach,
        name: "detach",
        aliases: &[],
        arg: Arg::None,
        description: "Detach from the attached ticket",
    },
    SlashCommand {
        id: CommandId::Decide,
        name: "decide",
        aliases: &[],
        arg: Arg::Required("<text>"),
        description: "Record a project decision",
    },
    SlashCommand {
        id: CommandId::Context,
        name: "context",
        aliases: &[],
        arg: Arg::None,
        description: "Token use by section, and the ticket's prefetched context pack",
    },
    SlashCommand {
        id: CommandId::Todos,
        name: "todos",
        aliases: &[],
        arg: Arg::None,
        description: "Toggle the task checklist",
    },
    SlashCommand {
        id: CommandId::Search,
        name: "search",
        aliases: &[],
        arg: Arg::Required("<query>"),
        description: "Search this project's code",
    },
    SlashCommand {
        id: CommandId::Review,
        name: "review",
        aliases: &[],
        arg: Arg::Optional("[focus]"),
        description: "Review the working tree's uncommitted changes",
    },
    SlashCommand {
        id: CommandId::Memory,
        name: "memory",
        aliases: &[],
        arg: Arg::None,
        description: "Open this project's AGENTS.md in $EDITOR",
    },
    SlashCommand {
        id: CommandId::Export,
        name: "export",
        aliases: &[],
        arg: Arg::Optional("[path]"),
        description: "Save this conversation as a markdown file",
    },
    SlashCommand {
        id: CommandId::Doctor,
        name: "doctor",
        aliases: &[],
        arg: Arg::None,
        description: "Run tm doctor's checks inline",
    },
    SlashCommand {
        id: CommandId::Permissions,
        name: "permissions",
        aliases: &[],
        arg: Arg::Optional("[mode]"),
        description: "Show or set the auto/plan/ask mode",
    },
    SlashCommand {
        id: CommandId::Workflow,
        name: "workflow",
        aliases: &[],
        arg: Arg::Optional("[name]"),
        description: "List workflows, or start one as a background ticket",
    },
    SlashCommand {
        id: CommandId::Board,
        name: "board",
        aliases: &[],
        arg: Arg::None,
        description: "Open the Kanban board",
    },
    SlashCommand {
        id: CommandId::Milestones,
        name: "milestones",
        aliases: &[],
        arg: Arg::None,
        description: "Open the Milestones tab",
    },
    SlashCommand {
        id: CommandId::Timeline,
        name: "timeline",
        aliases: &[],
        arg: Arg::None,
        description: "Open the Timeline tab",
    },
    SlashCommand {
        id: CommandId::Deps,
        name: "deps",
        aliases: &[],
        arg: Arg::None,
        description: "Open the dependency graph",
    },
    SlashCommand {
        id: CommandId::Ticket,
        name: "ticket",
        aliases: &[],
        arg: Arg::Required("<ticket>"),
        description: "Show a ticket's summary",
    },
    SlashCommand {
        id: CommandId::Run,
        name: "run",
        aliases: &[],
        arg: Arg::Required("<ticket>"),
        description: "Activate and queue a ticket",
    },
    SlashCommand {
        id: CommandId::Exit,
        name: "exit",
        aliases: &["quit"],
        arg: Arg::None,
        description: "Quit tm",
    },
];

/// One popup row: a command, and the alias the query matched through (if not its name).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    /// The matched command.
    pub command: &'static SlashCommand,
    /// Set when the query matched an alias rather than the canonical name.
    pub via_alias: Option<&'static str>,
}

/// The commands matching `query` (the text after the `/`), best first.
///
/// Ranking: a name prefix, then an alias prefix, then a name/alias substring, then a
/// subsequence (`/dt` finds `detach`). An empty query lists every command in table order.
pub fn filter(query: &str) -> Vec<Match> {
    let query = query.trim().to_ascii_lowercase();
    let mut ranked: Vec<(u8, usize, Match)> = Vec::new();
    for (order, command) in COMMANDS.iter().enumerate() {
        let names = std::iter::once((command.name, None))
            .chain(command.aliases.iter().map(|a| (*a, Some(*a))));
        let mut best: Option<(u8, Option<&'static str>)> = None;
        for (name, alias) in names {
            let rank = if query.is_empty() || name.starts_with(&query) {
                if alias.is_none() {
                    0
                } else {
                    1
                }
            } else if name.contains(&query) {
                2
            } else if is_subsequence(&query, name) {
                3
            } else {
                continue;
            };
            if best.is_none_or(|(b, _)| rank < b) {
                best = Some((rank, alias));
            }
        }
        if let Some((rank, via_alias)) = best {
            ranked.push((rank, order, Match { command, via_alias }));
        }
    }
    ranked.sort_by_key(|(rank, order, _)| (*rank, *order));
    ranked.into_iter().map(|(_, _, m)| m).collect()
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    needle.chars().all(|n| chars.any(|h| h == n))
}

/// Look a command up by exact name or alias.
pub fn lookup(name: &str) -> Option<&'static SlashCommand> {
    let name = name.to_ascii_lowercase();
    COMMANDS
        .iter()
        .find(|c| c.name == name || c.aliases.contains(&name.as_str()))
}

/// What a submitted line starting with `/` turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    /// A known command with its (trimmed) argument, possibly empty.
    Known {
        /// The command.
        command: &'static SlashCommand,
        /// Everything after the name, trimmed.
        arg: String,
    },
    /// A known command that requires an argument that was not given.
    MissingArg(&'static SlashCommand),
    /// Not a command this table knows.
    Unknown(String),
}

/// Parse a submitted line. Returns `None` when `line` is not a slash command at all.
pub fn parse(line: &str) -> Option<Parsed> {
    let rest = line.trim().strip_prefix('/')?;
    let (name, arg) = match rest.split_once(char::is_whitespace) {
        Some((name, arg)) => (name, arg.trim()),
        None => (rest, ""),
    };
    // `/usr/bin is missing python` is a message about a path, not a command.
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return None;
    }
    Some(match lookup(name) {
        Some(command) => match command.arg {
            Arg::Required(_) if arg.is_empty() => Parsed::MissingArg(command),
            _ => Parsed::Known {
                command,
                arg: arg.to_string(),
            },
        },
        None => Parsed::Unknown(name.to_string()),
    })
}

/// The usage string for `command`: `/attach <ticket>`.
pub fn usage(command: &SlashCommand) -> String {
    match command.arg {
        Arg::None => format!("/{}", command.name),
        Arg::Required(placeholder) | Arg::Optional(placeholder) => {
            format!("/{} {placeholder}", command.name)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(matches: &[Match]) -> Vec<&'static str> {
        matches.iter().map(|m| m.command.name).collect()
    }

    #[test]
    fn empty_query_lists_everything_in_table_order() {
        assert_eq!(
            names(&filter("")),
            COMMANDS.iter().map(|c| c.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn prefix_filters_and_ranks_name_before_alias() {
        assert_eq!(
            names(&filter("de")),
            vec!["detach", "decide", "deps", "model", "provider"]
        );
        assert_eq!(names(&filter("att")), vec!["attach"]);
        let home = filter("hom");
        assert_eq!(names(&home), vec!["tickets"]);
        assert_eq!(home[0].via_alias, Some("home"));
        assert_eq!(names(&filter("re")), vec!["resume", "review", "provider"]);
    }

    #[test]
    fn aliases_and_subsequences_match() {
        assert_eq!(names(&filter("quit")), vec!["exit"]);
        assert_eq!(filter("agents")[0].command.id, CommandId::Tickets);
        assert!(names(&filter("dt")).contains(&"detach"));
    }

    #[test]
    fn nonsense_matches_nothing() {
        assert!(filter("zzz").is_empty());
    }

    #[test]
    fn parse_known_unknown_and_missing_args() {
        match parse("/attach T-4") {
            Some(Parsed::Known { command, arg }) => {
                assert_eq!(command.id, CommandId::Attach);
                assert_eq!(arg, "T-4");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(
            matches!(parse("/attach"), Some(Parsed::MissingArg(c)) if c.id == CommandId::Attach)
        );
        assert!(
            matches!(parse("/tickets"), Some(Parsed::Known { command, .. }) if command.id == CommandId::Tickets)
        );
        assert!(
            matches!(parse("/connect"), Some(Parsed::Known { command, arg }) if command.id == CommandId::Connect && arg.is_empty())
        );
        assert!(
            matches!(parse("/connect anthropic"), Some(Parsed::Known { command, arg }) if command.id == CommandId::Connect && arg == "anthropic")
        );
        assert!(
            matches!(parse("/auth openai"), Some(Parsed::Known { command, arg }) if command.id == CommandId::Connect && arg == "openai")
        );
        assert!(
            matches!(parse("/provider"), Some(Parsed::Known { command, .. }) if command.id == CommandId::Provider)
        );
        assert!(
            matches!(parse("/config"), Some(Parsed::Known { command, arg }) if command.id == CommandId::Config && arg.is_empty())
        );
        assert!(
            matches!(parse("/config get routing_weights.recency"), Some(Parsed::Known { command, arg }) if command.id == CommandId::Config && arg == "get routing_weights.recency")
        );
        assert!(
            matches!(parse("/settings"), Some(Parsed::Known { command, .. }) if command.id == CommandId::Config)
        );
        assert_eq!(parse("/nope"), Some(Parsed::Unknown("nope".to_string())));
        assert_eq!(parse("not a command"), None);
        assert_eq!(parse("/"), None);
        assert_eq!(parse("/usr/bin is broken"), None);
    }

    #[test]
    fn optional_arguments_parse_with_or_without_a_value() {
        assert!(
            matches!(parse("/model"), Some(Parsed::Known { command, arg }) if command.id == CommandId::Model && arg.is_empty())
        );
        assert!(
            matches!(parse("/bg fix the build"), Some(Parsed::Known { command, arg }) if command.id == CommandId::Bg && arg == "fix the build")
        );
        assert_eq!(
            usage(lookup("model").expect("model is in the table")),
            "/model [model]"
        );
        for name in [
            "help",
            "clear",
            "resume",
            "compact",
            "model",
            "status",
            "cost",
            "connect",
            "provider",
            "config",
            "init",
            "bg",
            "context",
            "todos",
            "search",
            "review",
            "memory",
            "export",
            "doctor",
            "permissions",
            "workflow",
            "exit",
        ] {
            assert!(lookup(name).is_some(), "/{name} is a command");
        }
    }

    #[test]
    fn memory_export_doctor_permissions_and_workflow_are_in_the_table() {
        assert!(
            matches!(parse("/memory"), Some(Parsed::Known { command, arg }) if command.id == CommandId::Memory && arg.is_empty())
        );
        assert!(
            matches!(parse("/export"), Some(Parsed::Known { command, arg }) if command.id == CommandId::Export && arg.is_empty())
        );
        assert!(
            matches!(parse("/export notes.md"), Some(Parsed::Known { command, arg }) if command.id == CommandId::Export && arg == "notes.md")
        );
        assert!(
            matches!(parse("/doctor"), Some(Parsed::Known { command, .. }) if command.id == CommandId::Doctor)
        );
        assert!(
            matches!(parse("/permissions"), Some(Parsed::Known { command, arg }) if command.id == CommandId::Permissions && arg.is_empty())
        );
        assert!(
            matches!(parse("/permissions plan"), Some(Parsed::Known { command, arg }) if command.id == CommandId::Permissions && arg == "plan")
        );
        assert!(
            matches!(parse("/workflow"), Some(Parsed::Known { command, arg }) if command.id == CommandId::Workflow && arg.is_empty())
        );
        assert!(
            matches!(parse("/workflow release"), Some(Parsed::Known { command, arg }) if command.id == CommandId::Workflow && arg == "release")
        );
    }

    #[test]
    fn context_and_todos_are_in_the_table() {
        assert!(
            matches!(parse("/context"), Some(Parsed::Known { command, .. }) if command.id == CommandId::Context)
        );
        assert!(
            matches!(parse("/todos"), Some(Parsed::Known { command, .. }) if command.id == CommandId::Todos)
        );
    }

    #[test]
    fn search_requires_a_query_and_review_focus_is_optional() {
        assert!(
            matches!(parse("/search dependency graph"), Some(Parsed::Known { command, arg }) if command.id == CommandId::Search && arg == "dependency graph")
        );
        assert!(
            matches!(parse("/search"), Some(Parsed::MissingArg(c)) if c.id == CommandId::Search)
        );
        assert!(
            matches!(parse("/review"), Some(Parsed::Known { command, arg }) if command.id == CommandId::Review && arg.is_empty())
        );
        assert!(
            matches!(parse("/review error handling"), Some(Parsed::Known { command, arg }) if command.id == CommandId::Review && arg == "error handling")
        );
    }

    #[test]
    fn usage_shows_the_argument_placeholder() {
        let attach = lookup("attach").expect("attach is in the table");
        assert_eq!(usage(attach), "/attach <ticket>");
        assert_eq!(
            usage(lookup("clear").expect("clear is in the table")),
            "/clear"
        );
    }

    #[test]
    fn pm_view_and_ticket_commands_are_in_the_table() {
        assert!(names(&filter("mil")).contains(&"milestones"));
        for name in ["board", "milestones", "timeline", "deps"] {
            assert!(
                matches!(parse(&format!("/{name}")), Some(Parsed::Known { command, arg }) if arg.is_empty() && command.name == name)
            );
        }
        assert!(
            matches!(parse("/ticket T-1"), Some(Parsed::Known { command, arg }) if command.id == CommandId::Ticket && arg == "T-1")
        );
        assert!(
            matches!(parse("/ticket"), Some(Parsed::MissingArg(c)) if c.id == CommandId::Ticket)
        );
        assert!(
            matches!(parse("/run T-3"), Some(Parsed::Known { command, arg }) if command.id == CommandId::Run && arg == "T-3")
        );
        assert!(matches!(parse("/run"), Some(Parsed::MissingArg(c)) if c.id == CommandId::Run));
    }
}
