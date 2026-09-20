+++
[doc]
id = "wiki/architecture/tm-templates"
mode = "generated"
derived_from = ["crates/tm-templates/src/**"]
+++

# Architecture: tm-templates

## Module tree

- `crates/tm-templates/src/apply.rs`
- `crates/tm-templates/src/lib.rs`
- `crates/tm-templates/src/manifest.rs`
- `crates/tm-templates/src/registry.rs`
- `crates/tm-templates/src/skill.rs`
- `crates/tm-templates/src/template.rs`
- `crates/tm-templates/src/verify.rs`

## Public symbols

### `crates/tm-templates/src/apply.rs`

- `pub fn apply(template: &Template, params: &BTreeMap<String, String>, dest: &Path) -> Result<()>`

### `crates/tm-templates/src/lib.rs`

- `pub mod apply;`
- `pub mod manifest;`
- `pub mod registry;`
- `pub mod skill;`
- `pub mod template;`
- `pub mod verify;`

### `crates/tm-templates/src/manifest.rs`

- `pub enum ParamType`
- `pub struct ParamSpec`
- `pub struct TemplateManifest`
- `pub fn load(dir: &Path) -> Result<TemplateManifest>`
- `pub fn checksum_dir(dir: &Path) -> Result<String>`

### `crates/tm-templates/src/registry.rs`

- `pub enum TemplateSource`
- `pub struct RegistryEntry`
- `pub struct TemplateRegistry`
- `impl TemplateRegistry`
  - `pub fn load(path: &Path) -> Result<TemplateRegistry>`
  - `pub fn find(&self, id: &str) -> Option<&RegistryEntry>`
  - `pub fn resolve(&self, id: &str, base_dir: &Path) -> Result<Template>`
  - `pub fn resolve_all(&self, base_dir: &Path) -> Vec<(String, Result<Template>)>`

### `crates/tm-templates/src/skill.rs`

- `pub fn load(template: &Template) -> Result<Option<Section>>`

### `crates/tm-templates/src/template.rs`

- `pub struct Template`
- `impl Template`
  - `pub fn load(root: &Path) -> Result<Template>`
  - `pub fn files_dir(&self) -> PathBuf`
  - `pub fn verify_path(&self) -> PathBuf`
  - `pub fn skill_path(&self) -> PathBuf`

### `crates/tm-templates/src/verify.rs`

- `pub struct VerifyCheck`
- `pub struct VerifySpec`
- `pub fn load(path: &Path) -> Result<VerifySpec>`
- `pub struct VerifyOutcome`
- `pub struct VerifyReport`
- `pub fn run(
    spec: &VerifySpec,
    dest: &Path,
    auth: &Authority,
    cache: &dyn CommandCache,
    executor: &dyn CommandExecutor,
    clock: &dyn Clock,
    ids: &dyn IdSource,
    actor: &ParticipantId,
) -> Result<VerifyReport>`
- `pub struct MemoryCommandCache`
- `impl MemoryCommandCache`
  - `pub fn new() -> Self`
- `pub struct ProcessCommandExecutor;`
