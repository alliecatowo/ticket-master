+++
[doc]
id = "wiki/architecture/tm-workflow"
mode = "generated"
derived_from = ["crates/tm-workflow/src/**"]
+++

# Architecture: tm-workflow

## Module tree

- `crates/tm-workflow/src/commit.rs`
- `crates/tm-workflow/src/def.rs`
- `crates/tm-workflow/src/expand.rs`
- `crates/tm-workflow/src/lib.rs`
- `crates/tm-workflow/src/template.rs`

## Public symbols

### `crates/tm-workflow/src/commit.rs`

- `pub struct CommitOutcome`
- `pub fn content_hash(source: &str) -> String`
- `pub fn commit(
    store: &Store,
    ids: &dyn IdSource,
    actor: ParticipantId,
    def: &WorkflowDef,
    source: &str,
    proposal: &GraphCompilation,
) -> TmResult<CommitOutcome>`
- `pub fn ticket_has_settled(store: &Store, ticket: &TicketId) -> TmResult<bool>`

### `crates/tm-workflow/src/def.rs`

- `pub struct BudgetSpec`
- `impl BudgetSpec`
  - `pub fn to_budget(self) -> Budget`
- `pub enum ParamKind`
- `pub struct ParamDef`
- `pub enum ForEach`
- `impl ForEach`
  - `pub fn from_output_parts(&self) -> Option<(&str, &str)>`
- `pub enum JoinKind`
- `pub enum MergeStrategy`
- `pub struct JoinDef`
- `pub struct NodeDef`
- `pub struct WorkflowDef`
- `impl WorkflowDef`
  - `pub fn parse(source: &str) -> TmResult<Self>`
  - `pub fn validate(&self) -> TmResult<()>`
  - `pub fn is_one_by_one(&self) -> bool`

### `crates/tm-workflow/src/expand.rs`

- `pub fn node_id_of_ticket_ref(ticket_ref: &str) -> &str`
- `pub fn expand(
    def: &WorkflowDef,
    params: &BTreeMap<String, String>,
    _view: &ProjectView,
) -> TmResult<GraphCompilation>`
- `pub fn expand_fan_out(
    node: &NodeDef,
    params: &BTreeMap<String, String>,
    items: &[String],
) -> TmResult<Vec<ProposedTicket>>`

### `crates/tm-workflow/src/lib.rs`

- `pub mod commit;`
- `pub mod def;`
- `pub mod expand;`
- `pub mod template;`

### `crates/tm-workflow/src/template.rs`

- `pub fn render(
    template: &str,
    params: &BTreeMap<String, String>,
    item: Option<&str>,
) -> TmResult<String>`
