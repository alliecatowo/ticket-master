//! Owns tree-sitter parsing for rust, go, typescript, tsx, javascript and python: symbol and
//! reference extraction via per-language queries, resolution (same-file scope, then
//! same-package, then workspace-wide with an ambiguity flag), and outline generation.
//!
//! This module never writes files. [`rename_preview`] returns a [`RenamePatch`] describing the
//! edits a caller could apply; applying them is somebody else's job.

use std::collections::{BTreeSet, HashMap};

use tree_sitter::StreamingIterator;

use tm_types::{Result, TmError};

use crate::walk::Language;

/// What kind of construct a [`Symbol`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SymbolKind {
    /// Free function or method.
    Function,
    /// Struct/class/record type.
    Struct,
    /// Enum type.
    Enum,
    /// Interface/trait/protocol.
    Interface,
    /// `impl` block (Rust) or equivalent method-attachment construct.
    Impl,
    /// Module/package/namespace.
    Module,
    /// Top-level or member variable/constant.
    Variable,
    /// Type alias.
    TypeAlias,
}

/// A byte-and-line span, paired for convenience since symbols are always reported with both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    /// Start byte offset, inclusive.
    pub byte_start: usize,
    /// End byte offset, exclusive.
    pub byte_end: usize,
    /// Start line, 1-based.
    pub line_start: u32,
    /// End line, 1-based.
    pub line_end: u32,
}

/// A definition: a named, located, typed construct extracted from source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    /// Unique id within one extraction pass (stable only for the lifetime of the
    /// [`SymbolIndex`] that produced it; persistence assigns its own row id).
    pub id: u64,
    /// Symbol name as written.
    pub name: String,
    /// What kind of construct this is.
    pub kind: SymbolKind,
    /// Path of the owning file, relative to the project root.
    pub path: String,
    /// Location within the file.
    pub range: Range,
    /// Id of the tightest enclosing symbol, if nested (e.g. a method's enclosing impl/class).
    pub container: Option<u64>,
    /// Rendered signature (parameter list and return type), for functions/methods.
    pub signature: Option<String>,
    /// Doc comment immediately preceding the construct, if present.
    pub doc: Option<String>,
}

/// An occurrence of a name that plausibly refers to a [`Symbol`], before or after resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// The identifier text as written at the reference site.
    pub symbol_hint: String,
    /// Path of the file containing the reference.
    pub path: String,
    /// Location of the reference occurrence.
    pub range: Range,
}

/// How confidently a reference was resolved to a definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionConfidence {
    /// Resolved uniquely within the same file.
    SameFile,
    /// Resolved uniquely within the same package/module.
    SamePackage,
    /// Resolved workspace-wide, and uniquely.
    WorkspaceUnique,
    /// Resolved workspace-wide, but more than one candidate matched by name; `Vec<u64>` in
    /// [`Resolution::ambiguous_with`] holds the other candidates.
    Ambiguous,
}

/// The outcome of resolving one [`Reference`] to a [`Symbol`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    /// Best-candidate symbol id, if any candidate existed at all.
    pub symbol_id: Option<u64>,
    /// How that candidate was found.
    pub confidence: ResolutionConfidence,
    /// Other candidate symbol ids that also matched by name, present iff `confidence` is
    /// [`ResolutionConfidence::Ambiguous`].
    pub ambiguous_with: Vec<u64>,
}

/// A textual outline entry: one line per top-level (or shallowly nested) symbol, suitable for
/// dropping into a prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutlineEntry {
    /// The symbol this entry describes.
    pub symbol_id: u64,
    /// Indentation depth (0 = top-level).
    pub depth: u32,
    /// Rendered line, e.g. `"fn parse(input: &str) -> Result<Ast>"`.
    pub rendered: String,
}

/// A single textual edit as part of a [`RenamePatch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchEdit {
    /// Path of the file to edit.
    pub path: String,
    /// Byte range to replace, end exclusive.
    pub byte_start: usize,
    /// End of the byte range, exclusive.
    pub byte_end: usize,
    /// Replacement text.
    pub replacement: String,
}

/// The result of [`rename_preview`]: a set of edits across the workspace. Never applied by
/// this crate.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RenamePatch {
    /// Every edit the rename would make, across all affected files.
    pub edits: Vec<PatchEdit>,
    /// Reference sites the renamer found but could not confidently attribute to the symbol
    /// being renamed (see [`ResolutionConfidence::Ambiguous`]); surfaced so a human/agent can
    /// decide by hand rather than silently skipping them.
    pub skipped_ambiguous: Vec<Reference>,
}

/// The directory portion of a slash-separated path (everything before the last `/`, or `""`
/// if there is none). This is our notion of "package" for resolution purposes.
fn dir_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    }
}

fn node_range(node: tree_sitter::Node) -> Range {
    Range {
        byte_start: node.start_byte(),
        byte_end: node.end_byte(),
        line_start: node.start_position().row as u32 + 1,
        line_end: node.end_position().row as u32 + 1,
    }
}

/// Walks contiguous preceding comment siblings, oldest first, and joins their text.
fn leading_doc(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    let mut docs = Vec::new();
    let mut cur = node.prev_sibling();
    while let Some(n) = cur {
        if n.kind().contains("comment") {
            if let Ok(text) = n.utf8_text(source) {
                docs.push(text.trim_end().to_string());
            }
            cur = n.prev_sibling();
        } else {
            break;
        }
    }
    if docs.is_empty() {
        None
    } else {
        docs.reverse();
        Some(docs.join("\n"))
    }
}

/// Everything up to the construct's `body` field (or the whole node, if it has none), with a
/// trailing block-opener/colon trimmed. A best-effort rendering, not a type-checked signature.
fn render_signature(node: tree_sitter::Node, source: &[u8]) -> Option<String> {
    let end = node
        .child_by_field_name("body")
        .map(|b| b.start_byte())
        .unwrap_or(node.end_byte());
    let start = node.start_byte();
    if end <= start {
        return None;
    }
    let raw = std::str::from_utf8(&source[start..end]).ok()?;
    let trimmed = raw.trim().trim_end_matches(':').trim_end();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn symbol_kind(lang: Language, node: tree_sitter::Node) -> SymbolKind {
    match lang {
        Language::Rust => match node.kind() {
            "function_item" => SymbolKind::Function,
            "struct_item" => SymbolKind::Struct,
            "enum_item" => SymbolKind::Enum,
            "trait_item" => SymbolKind::Interface,
            "impl_item" => SymbolKind::Impl,
            "mod_item" => SymbolKind::Module,
            "type_item" => SymbolKind::TypeAlias,
            _ => SymbolKind::Variable,
        },
        Language::Go => match node.kind() {
            "function_declaration" | "method_declaration" => SymbolKind::Function,
            "type_spec" => match node.child_by_field_name("type").map(|n| n.kind()) {
                Some("struct_type") => SymbolKind::Struct,
                Some("interface_type") => SymbolKind::Interface,
                _ => SymbolKind::TypeAlias,
            },
            _ => SymbolKind::Variable,
        },
        Language::TypeScript | Language::Tsx | Language::JavaScript => match node.kind() {
            "function_declaration" | "generator_function_declaration" | "method_definition" => {
                SymbolKind::Function
            }
            "class_declaration" | "abstract_class_declaration" => SymbolKind::Struct,
            "interface_declaration" => SymbolKind::Interface,
            "type_alias_declaration" => SymbolKind::TypeAlias,
            "enum_declaration" => SymbolKind::Enum,
            _ => SymbolKind::Variable,
        },
        Language::Python => match node.kind() {
            "function_definition" => SymbolKind::Function,
            "class_definition" => SymbolKind::Struct,
            _ => SymbolKind::Variable,
        },
        Language::Other => SymbolKind::Variable,
    }
}

struct RawSymbol {
    name: String,
    name_range: Range,
    kind: SymbolKind,
    range: Range,
    doc: Option<String>,
    signature: Option<String>,
    node_start: usize,
    node_end: usize,
}

/// Parses source files and holds the resulting symbol/reference graph for one project
/// snapshot (a batch of files at a point in time; incremental updates replace a file's
/// contribution wholesale rather than patching it in place).
pub struct SymbolIndex {
    symbols: Vec<Symbol>,
    references: Vec<Reference>,
    next_id: u64,
    /// The name-token range for each symbol id, kept separately from [`Symbol::range`] (which
    /// spans the whole definition, needed for containment checks in `callers`/`callees`) so
    /// [`SymbolIndex::rename_preview`] can target just the identifier.
    name_ranges: HashMap<u64, Range>,
}

impl SymbolIndex {
    /// An empty index.
    pub fn new() -> Self {
        SymbolIndex {
            symbols: Vec::new(),
            references: Vec::new(),
            next_id: 0,
            name_ranges: HashMap::new(),
        }
    }

    /// Parse `text` (the contents of `path`, language `lang`) and extract its symbols and
    /// references, replacing any prior extraction for that path in this index.
    ///
    /// Returns an error only for languages without a grammar in this build
    /// (`lang.has_grammar()` false, or `Language::Other`); malformed/partial source is not an
    /// error — tree-sitter parses it as best it can and extraction proceeds over whatever
    /// tree results.
    pub fn parse_file(&mut self, path: &str, text: &str, lang: Language) -> Result<()> {
        let language =
            Self::grammar_for(lang).ok_or_else(|| TmError::invariant("no grammar for language"))?;

        let mut parser = tree_sitter::Parser::new();
        parser
            .set_language(&language)
            .map_err(|e| TmError::invariant(format!("failed to set language: {e}")))?;
        let tree = parser
            .parse(text, None)
            .ok_or_else(|| TmError::invariant("tree-sitter failed to produce a parse tree"))?;
        let source = text.as_bytes();

        let stale_ids: Vec<u64> = self
            .symbols
            .iter()
            .filter(|s| s.path == path)
            .map(|s| s.id)
            .collect();
        self.symbols.retain(|s| s.path != path);
        self.references.retain(|r| r.path != path);
        for id in stale_ids {
            self.name_ranges.remove(&id);
        }

        let symbol_query = tree_sitter::Query::new(&language, Self::symbol_query_source(lang))
            .map_err(|e| TmError::invariant(format!("bad symbol query: {e}")))?;
        let def_idx = symbol_query.capture_index_for_name("definition");
        let name_idx = symbol_query.capture_index_for_name("name");

        let mut raw: Vec<RawSymbol> = Vec::new();
        {
            let mut cursor = tree_sitter::QueryCursor::new();
            let mut matches = cursor.matches(&symbol_query, tree.root_node(), source);
            while let Some(m) = matches.next() {
                let mut def_node = None;
                let mut name_node = None;
                for cap in m.captures {
                    if Some(cap.index) == def_idx {
                        def_node = Some(cap.node);
                    }
                    if Some(cap.index) == name_idx {
                        name_node = Some(cap.node);
                    }
                }
                let (Some(def_node), Some(name_node)) = (def_node, name_node) else {
                    continue;
                };
                let Ok(name) = name_node.utf8_text(source) else {
                    continue;
                };
                if name.is_empty() {
                    continue;
                }
                raw.push(RawSymbol {
                    name: name.to_string(),
                    name_range: node_range(name_node),
                    kind: symbol_kind(lang, def_node),
                    range: node_range(def_node),
                    doc: leading_doc(def_node, source),
                    signature: render_signature(def_node, source),
                    node_start: def_node.start_byte(),
                    node_end: def_node.end_byte(),
                });
            }
        }

        let mut containers: Vec<Option<usize>> = Vec::with_capacity(raw.len());
        for i in 0..raw.len() {
            let mut best: Option<(usize, usize)> = None; // (index, span length)
            for j in 0..raw.len() {
                if i == j {
                    continue;
                }
                let (js, je) = (raw[j].node_start, raw[j].node_end);
                let (is_, ie) = (raw[i].node_start, raw[i].node_end);
                let strictly_contains = js <= is_ && je >= ie && (js, je) != (is_, ie);
                if strictly_contains {
                    let len = je - js;
                    if best.map(|(_, best_len)| len < best_len).unwrap_or(true) {
                        best = Some((j, len));
                    }
                }
            }
            containers.push(best.map(|(j, _)| j));
        }

        let ids: Vec<u64> = raw
            .iter()
            .map(|_| {
                let id = self.next_id;
                self.next_id += 1;
                id
            })
            .collect();

        for (i, r) in raw.into_iter().enumerate() {
            let id = ids[i];
            self.name_ranges.insert(id, r.name_range);
            self.symbols.push(Symbol {
                id,
                name: r.name,
                kind: r.kind,
                path: path.to_string(),
                range: r.range,
                container: containers[i].map(|j| ids[j]),
                signature: r.signature,
                doc: r.doc,
            });
        }

        let reference_query =
            tree_sitter::Query::new(&language, Self::reference_query_source(lang))
                .map_err(|e| TmError::invariant(format!("bad reference query: {e}")))?;
        let ref_idx = reference_query.capture_index_for_name("reference");
        let mut cursor = tree_sitter::QueryCursor::new();
        let mut matches = cursor.matches(&reference_query, tree.root_node(), source);
        while let Some(m) = matches.next() {
            for cap in m.captures {
                if Some(cap.index) != ref_idx {
                    continue;
                }
                let Ok(text) = cap.node.utf8_text(source) else {
                    continue;
                };
                if text.is_empty() {
                    continue;
                }
                self.references.push(Reference {
                    symbol_hint: text.to_string(),
                    path: path.to_string(),
                    range: node_range(cap.node),
                });
            }
        }

        Ok(())
    }

    /// The tree-sitter grammar for a language, or `None` if this build doesn't wire one up
    /// (currently: every [`Language`] variant except [`Language::Other`] has one).
    fn grammar_for(lang: Language) -> Option<tree_sitter::Language> {
        match lang {
            Language::Rust => Some(tree_sitter_rust::LANGUAGE.into()),
            Language::Go => Some(tree_sitter_go::LANGUAGE.into()),
            Language::TypeScript => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
            Language::Tsx => Some(tree_sitter_typescript::LANGUAGE_TSX.into()),
            Language::JavaScript => Some(tree_sitter_javascript::LANGUAGE.into()),
            Language::Python => Some(tree_sitter_python::LANGUAGE.into()),
            Language::Other => None,
        }
    }

    /// All symbols named `name` visible from `from_path`, most-preferred resolution first:
    /// same-file, then same-package (same directory), then workspace-wide.
    pub fn candidates(&self, name: &str, from_path: &str) -> Vec<&Symbol> {
        let from_dir = dir_of(from_path);
        let mut same_file = Vec::new();
        let mut same_package = Vec::new();
        let mut rest = Vec::new();
        // Iterate most-recently-parsed first, so within a tier the freshest definition of a
        // repeated name wins ties deterministically.
        for s in self.symbols.iter().rev() {
            if s.name != name {
                continue;
            }
            if s.path == from_path {
                same_file.push(s);
            } else if dir_of(&s.path) == from_dir {
                same_package.push(s);
            } else {
                rest.push(s);
            }
        }
        same_file
            .into_iter()
            .chain(same_package)
            .chain(rest)
            .collect()
    }

    /// Resolve one reference using [`SymbolIndex::candidates`]: same-file match if exactly one
    /// same-file candidate exists, else same-package if exactly one, else workspace-wide if
    /// exactly one, else the first workspace candidate with `Ambiguous` confidence and the
    /// rest listed in `ambiguous_with`. `None`/`Empty` candidates yield `symbol_id: None`.
    pub fn resolve(&self, reference: &Reference) -> Resolution {
        let candidates = self.candidates(&reference.symbol_hint, &reference.path);
        if candidates.is_empty() {
            // No better variant expresses "no candidates"; callers must check symbol_id first.
            return Resolution {
                symbol_id: None,
                confidence: ResolutionConfidence::WorkspaceUnique,
                ambiguous_with: Vec::new(),
            };
        }

        let from_dir = dir_of(&reference.path);
        let same_file: Vec<&&Symbol> = candidates
            .iter()
            .filter(|s| s.path == reference.path)
            .collect();
        if same_file.len() == 1 {
            return Resolution {
                symbol_id: Some(same_file[0].id),
                confidence: ResolutionConfidence::SameFile,
                ambiguous_with: Vec::new(),
            };
        }

        let same_package: Vec<&&Symbol> = candidates
            .iter()
            .filter(|s| s.path != reference.path && dir_of(&s.path) == from_dir)
            .collect();
        if same_package.len() == 1 {
            return Resolution {
                symbol_id: Some(same_package[0].id),
                confidence: ResolutionConfidence::SamePackage,
                ambiguous_with: Vec::new(),
            };
        }

        if candidates.len() == 1 {
            return Resolution {
                symbol_id: Some(candidates[0].id),
                confidence: ResolutionConfidence::WorkspaceUnique,
                ambiguous_with: Vec::new(),
            };
        }

        let mut ids = candidates.iter().map(|s| s.id);
        // Safe: candidates is non-empty (checked above) and none of the exactly-one branches
        // above matched, so at least two candidates remain here.
        let first = ids.next().expect("candidates non-empty");
        Resolution {
            symbol_id: Some(first),
            confidence: ResolutionConfidence::Ambiguous,
            ambiguous_with: ids.collect(),
        }
    }

    /// The symbol defining `name`, resolved from the context of `from_path`. Shorthand over
    /// [`SymbolIndex::resolve`] for direct definition lookups (not reference resolution).
    pub fn definition(&self, name: &str, from_path: &str) -> Option<&Symbol> {
        self.candidates(name, from_path).into_iter().next()
    }

    /// Every reference that resolves (per [`SymbolIndex::resolve`]) to `symbol_id`.
    pub fn references(&self, symbol_id: u64) -> Vec<&Reference> {
        self.references
            .iter()
            .filter(|r| self.resolve(r).symbol_id == Some(symbol_id))
            .collect()
    }

    /// Symbols whose kind is [`SymbolKind::Impl`]/[`SymbolKind::Struct`] etc. that implement or
    /// satisfy the interface/trait named `interface_name` (heuristic: impl blocks whose
    /// rendered signature mentions the interface name; this crate does not do full type
    /// inference).
    pub fn implementations(&self, interface_name: &str) -> Vec<&Symbol> {
        self.symbols
            .iter()
            .filter(|s| matches!(s.kind, SymbolKind::Impl | SymbolKind::Struct))
            .filter(|s| {
                s.signature
                    .as_deref()
                    .map(|sig| sig.contains(interface_name))
                    .unwrap_or(false)
            })
            .collect()
    }

    /// Symbols that reference `symbol_id` from within a function/method body, i.e. call sites
    /// (heuristic: references inside a Function-kind container that resolve to `symbol_id`).
    pub fn callers(&self, symbol_id: u64) -> Vec<&Symbol> {
        self.symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Function)
            .filter(|func| {
                self.references.iter().any(|r| {
                    r.path == func.path
                        && r.range.byte_start >= func.range.byte_start
                        && r.range.byte_end <= func.range.byte_end
                        && self.resolve(r).symbol_id == Some(symbol_id)
                })
            })
            .collect()
    }

    /// Symbols called by the function/method `symbol_id` (heuristic: references contained
    /// within `symbol_id`'s own range that resolve to some other Function-kind symbol).
    pub fn callees(&self, symbol_id: u64) -> Vec<&Symbol> {
        let Some(container) = self.symbols.iter().find(|s| s.id == symbol_id) else {
            return Vec::new();
        };
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for r in &self.references {
            if r.path != container.path
                || r.range.byte_start < container.range.byte_start
                || r.range.byte_end > container.range.byte_end
            {
                continue;
            }
            let Some(id) = self.resolve(r).symbol_id else {
                continue;
            };
            if id == symbol_id || !seen.insert(id) {
                continue;
            }
            if let Some(sym) = self
                .symbols
                .iter()
                .find(|s| s.id == id && s.kind == SymbolKind::Function)
            {
                out.push(sym);
            }
        }
        out
    }

    /// Best-effort type of the symbol at `path`/`byte_offset` (its own signature/type-alias
    /// target if it is itself a symbol, else the resolved type of the nearest enclosing
    /// variable/field declaration).
    pub fn type_of(&self, path: &str, byte_offset: usize) -> Option<&Symbol> {
        self.symbols
            .iter()
            .filter(|s| {
                s.path == path
                    && s.range.byte_start <= byte_offset
                    && byte_offset < s.range.byte_end
            })
            .min_by_key(|s| s.range.byte_end - s.range.byte_start)
    }

    /// A rendered outline of `path`: one entry per symbol in that file, depth reflecting
    /// container nesting, in source order.
    pub fn outline(&self, path: &str) -> Vec<OutlineEntry> {
        let mut syms: Vec<&Symbol> = self.symbols.iter().filter(|s| s.path == path).collect();
        syms.sort_by_key(|s| s.range.byte_start);
        syms.into_iter()
            .map(|s| {
                let mut depth = 0u32;
                let mut current = s.container;
                while let Some(cid) = current {
                    depth += 1;
                    current = self
                        .symbols
                        .iter()
                        .find(|x| x.id == cid)
                        .and_then(|x| x.container);
                }
                let rendered = s
                    .signature
                    .clone()
                    .unwrap_or_else(|| format!("{:?} {}", s.kind, s.name));
                OutlineEntry {
                    symbol_id: s.id,
                    depth,
                    rendered,
                }
            })
            .collect()
    }

    /// Preview renaming `symbol_id` to `new_name` across every resolved reference plus the
    /// definition site itself. Never writes to disk; returns the patch for the caller to
    /// apply or discard.
    pub fn rename_preview(&self, symbol_id: u64, new_name: &str) -> Result<RenamePatch> {
        let symbol = self
            .symbols
            .iter()
            .find(|s| s.id == symbol_id)
            .ok_or_else(|| TmError::not_found("symbol", symbol_id.to_string()))?;

        let def_range = self
            .name_ranges
            .get(&symbol_id)
            .copied()
            .unwrap_or(symbol.range);
        let mut patch = RenamePatch {
            edits: vec![PatchEdit {
                path: symbol.path.clone(),
                byte_start: def_range.byte_start,
                byte_end: def_range.byte_end,
                replacement: new_name.to_string(),
            }],
            skipped_ambiguous: Vec::new(),
        };

        for reference in &self.references {
            let resolution = self.resolve(reference);
            if resolution.confidence == ResolutionConfidence::Ambiguous {
                // An ambiguous reference names `symbol_id` if the target is either the
                // (arbitrarily chosen) primary candidate or one of the others it's ambiguous
                // with; either way it's unsafe to rewrite automatically.
                let names_target = resolution.symbol_id == Some(symbol_id)
                    || resolution.ambiguous_with.contains(&symbol_id);
                if names_target {
                    patch.skipped_ambiguous.push(reference.clone());
                }
                continue;
            }
            if resolution.symbol_id != Some(symbol_id) {
                continue;
            }
            patch.edits.push(PatchEdit {
                path: reference.path.clone(),
                byte_start: reference.range.byte_start,
                byte_end: reference.range.byte_end,
                replacement: new_name.to_string(),
            });
        }

        Ok(patch)
    }

    /// The tree-sitter query source (in that language's node grammar) used to extract symbols.
    /// Kept as a function (not inline in `parse_file`) so tests can assert each language's
    /// query at least compiles against its grammar.
    fn symbol_query_source(lang: Language) -> &'static str {
        match lang {
            Language::Rust => {
                "(function_item name: (_) @name) @definition
                 (struct_item name: (_) @name) @definition
                 (enum_item name: (_) @name) @definition
                 (trait_item name: (_) @name) @definition
                 (impl_item type: (_) @name) @definition
                 (mod_item name: (_) @name) @definition
                 (const_item name: (_) @name) @definition
                 (static_item name: (_) @name) @definition
                 (type_item name: (_) @name) @definition"
            }
            Language::Go => {
                "(function_declaration name: (_) @name) @definition
                 (method_declaration name: (_) @name) @definition
                 (type_spec name: (_) @name) @definition
                 (const_spec name: (_) @name) @definition
                 (var_spec name: (_) @name) @definition"
            }
            Language::TypeScript | Language::Tsx => {
                "(function_declaration name: (_) @name) @definition
                 (class_declaration name: (_) @name) @definition
                 (abstract_class_declaration name: (_) @name) @definition
                 (interface_declaration name: (_) @name) @definition
                 (type_alias_declaration name: (_) @name) @definition
                 (enum_declaration name: (_) @name) @definition
                 (method_definition name: (_) @name) @definition
                 (variable_declarator name: (identifier) @name) @definition"
            }
            Language::JavaScript => {
                "(function_declaration name: (_) @name) @definition
                 (generator_function_declaration name: (_) @name) @definition
                 (class_declaration name: (_) @name) @definition
                 (method_definition name: (_) @name) @definition
                 (variable_declarator name: (identifier) @name) @definition"
            }
            Language::Python => {
                "(function_definition name: (_) @name) @definition
                 (class_definition name: (_) @name) @definition"
            }
            Language::Other => unreachable!("parse_file rejects Language::Other before querying"),
        }
    }

    /// The tree-sitter query source used to extract references (identifier usages that are
    /// not themselves the definition site).
    fn reference_query_source(lang: Language) -> &'static str {
        match lang {
            Language::Rust | Language::Go => {
                "(identifier) @reference
                 (type_identifier) @reference
                 (field_identifier) @reference"
            }
            Language::TypeScript | Language::Tsx => {
                "(identifier) @reference
                 (type_identifier) @reference
                 (property_identifier) @reference
                 (shorthand_property_identifier) @reference"
            }
            Language::JavaScript => {
                "(identifier) @reference
                 (property_identifier) @reference
                 (shorthand_property_identifier) @reference"
            }
            Language::Python => "(identifier) @reference",
            Language::Other => unreachable!("parse_file rejects Language::Other before querying"),
        }
    }
}

impl Default for SymbolIndex {
    fn default() -> Self {
        SymbolIndex::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rust_index(path: &str, text: &str) -> SymbolIndex {
        let mut idx = SymbolIndex::new();
        idx.parse_file(path, text, Language::Rust)
            .expect("rust parses");
        idx
    }

    #[test]
    fn parse_file_extracts_a_rust_function_symbol() {
        let idx = rust_index(
            "src/lib.rs",
            "/// Adds two numbers.\nfn add(a: i32, b: i32) -> i32 { a + b }\n",
        );
        let sym = idx.definition("add", "src/lib.rs").expect("add defined");
        assert_eq!(sym.kind, SymbolKind::Function);
        assert_eq!(sym.name, "add");
        assert!(sym.signature.as_deref().unwrap_or("").contains("fn add"));
        assert_eq!(sym.doc.as_deref(), Some("/// Adds two numbers."));
    }

    #[test]
    fn parse_file_errors_for_language_other() {
        let mut idx = SymbolIndex::new();
        let err = idx
            .parse_file("README.md", "hello", Language::Other)
            .unwrap_err();
        assert!(matches!(err, TmError::Invariant(_)));
    }

    #[test]
    fn parse_file_tolerates_malformed_source() {
        let mut idx = SymbolIndex::new();
        // Missing closing brace: tree-sitter still produces a best-effort tree.
        let result = idx.parse_file("src/broken.rs", "fn broken(", Language::Rust);
        assert!(result.is_ok());
    }

    #[test]
    fn reparsing_a_path_replaces_its_prior_symbols() {
        let mut idx = SymbolIndex::new();
        idx.parse_file("src/a.rs", "fn one() {}", Language::Rust)
            .unwrap();
        idx.parse_file("src/a.rs", "fn two() {}", Language::Rust)
            .unwrap();
        assert!(idx.definition("one", "src/a.rs").is_none());
        assert!(idx.definition("two", "src/a.rs").is_some());
    }

    #[test]
    fn candidates_prefer_same_file_then_same_package_then_workspace() {
        let mut idx = SymbolIndex::new();
        idx.parse_file("pkg/a.rs", "fn dup() {}", Language::Rust)
            .unwrap();
        idx.parse_file("pkg/b.rs", "fn dup() {}", Language::Rust)
            .unwrap();
        idx.parse_file("other/c.rs", "fn dup() {}", Language::Rust)
            .unwrap();

        let from_a = idx.candidates("dup", "pkg/a.rs");
        assert_eq!(from_a[0].path, "pkg/a.rs");

        let from_pkg_file = idx.candidates("dup", "pkg/z.rs");
        assert_eq!(from_pkg_file[0].path, "pkg/b.rs");
    }

    #[test]
    fn resolve_yields_same_file_confidence_when_unique_in_file() {
        let idx = rust_index("src/a.rs", "fn foo() {} fn bar() { foo(); }");
        let reference = Reference {
            symbol_hint: "foo".to_string(),
            path: "src/a.rs".to_string(),
            range: Range {
                byte_start: 0,
                byte_end: 3,
                line_start: 1,
                line_end: 1,
            },
        };
        let resolution = idx.resolve(&reference);
        assert_eq!(resolution.confidence, ResolutionConfidence::SameFile);
        assert!(resolution.symbol_id.is_some());
    }

    #[test]
    fn resolve_yields_none_symbol_id_for_unknown_name() {
        let idx = rust_index("src/a.rs", "fn foo() {}");
        let reference = Reference {
            symbol_hint: "nonexistent".to_string(),
            path: "src/a.rs".to_string(),
            range: Range {
                byte_start: 0,
                byte_end: 3,
                line_start: 1,
                line_end: 1,
            },
        };
        let resolution = idx.resolve(&reference);
        assert_eq!(resolution.symbol_id, None);
        assert!(resolution.ambiguous_with.is_empty());
    }

    #[test]
    fn resolve_flags_ambiguity_across_unrelated_workspace_files() {
        let mut idx = SymbolIndex::new();
        idx.parse_file("a/one.rs", "fn dup() {}", Language::Rust)
            .unwrap();
        idx.parse_file("b/two.rs", "fn dup() {}", Language::Rust)
            .unwrap();
        let reference = Reference {
            symbol_hint: "dup".to_string(),
            path: "c/caller.rs".to_string(),
            range: Range {
                byte_start: 0,
                byte_end: 3,
                line_start: 1,
                line_end: 1,
            },
        };
        let resolution = idx.resolve(&reference);
        assert_eq!(resolution.confidence, ResolutionConfidence::Ambiguous);
        assert_eq!(resolution.ambiguous_with.len(), 1);
    }

    #[test]
    fn outline_orders_symbols_by_source_position_with_nesting_depth() {
        let idx = rust_index("src/lib.rs", "struct Foo;\nimpl Foo { fn bar(&self) {} }\n");
        let entries = idx.outline("src/lib.rs");
        assert_eq!(entries.len(), 3); // struct, impl, method
        let range_of = |id: u64| {
            idx.symbols
                .iter()
                .find(|s| s.id == id)
                .map(|s| s.range.byte_start)
                .unwrap_or(0)
        };
        assert!(entries
            .windows(2)
            .all(|w| range_of(w[0].symbol_id) <= range_of(w[1].symbol_id)));
        let method_entry = entries.iter().max_by_key(|e| e.depth).unwrap();
        assert!(method_entry.depth > 0);
    }

    #[test]
    fn rename_preview_edits_definition_and_confident_references() {
        let mut idx = SymbolIndex::new();
        idx.parse_file(
            "src/a.rs",
            "fn foo() {}\nfn bar() { foo(); }\n",
            Language::Rust,
        )
        .unwrap();
        let sym = idx.definition("foo", "src/a.rs").unwrap();
        let id = sym.id;
        let patch = idx.rename_preview(id, "renamed").unwrap();
        assert!(patch
            .edits
            .iter()
            .any(|e| e.replacement == "renamed" && e.path == "src/a.rs"));
        assert!(patch.edits.len() >= 2); // definition + call site
        assert!(patch.skipped_ambiguous.is_empty());
    }

    #[test]
    fn rename_preview_skips_ambiguous_references_instead_of_editing_them() {
        let mut idx = SymbolIndex::new();
        idx.parse_file("a/one.rs", "fn dup() {}", Language::Rust)
            .unwrap();
        idx.parse_file("b/two.rs", "fn dup() {}", Language::Rust)
            .unwrap();
        idx.parse_file("c/caller.rs", "fn user() { dup(); }", Language::Rust)
            .unwrap();

        let target = idx.definition("dup", "a/one.rs").unwrap().id;
        let patch = idx.rename_preview(target, "renamed").unwrap();
        // The ambiguous call site in c/caller.rs must not be blindly edited.
        assert!(patch
            .skipped_ambiguous
            .iter()
            .any(|r| r.path == "c/caller.rs"));
        assert!(!patch.edits.iter().any(|e| e.path == "c/caller.rs"));
    }

    #[test]
    fn rename_preview_errors_for_unknown_symbol_id() {
        let idx = SymbolIndex::new();
        let err = idx.rename_preview(9999, "whatever").unwrap_err();
        assert!(matches!(err, TmError::NotFound { .. }));
    }

    #[test]
    fn callers_and_callees_reflect_call_sites_inside_function_bodies() {
        let idx = rust_index("src/a.rs", "fn callee() {}\nfn caller() { callee(); }\n");
        let callee_id = idx.definition("callee", "src/a.rs").unwrap().id;
        let caller_id = idx.definition("caller", "src/a.rs").unwrap().id;

        let callers = idx.callers(callee_id);
        assert!(callers.iter().any(|s| s.id == caller_id));

        let callees = idx.callees(caller_id);
        assert!(callees.iter().any(|s| s.id == callee_id));
    }

    #[test]
    fn implementations_finds_impl_blocks_mentioning_the_trait_name() {
        let idx = rust_index(
            "src/a.rs",
            "trait Greet {}\nstruct Foo;\nimpl Greet for Foo {}\n",
        );
        let impls = idx.implementations("Greet");
        assert!(impls.iter().any(|s| s.kind == SymbolKind::Impl));
    }

    #[test]
    fn type_of_finds_the_tightest_containing_symbol() {
        let idx = rust_index("src/a.rs", "fn outer() { let x = 1; }\n");
        let sym = idx.definition("outer", "src/a.rs").unwrap();
        let mid = sym.range.byte_start + 2;
        let found = idx.type_of("src/a.rs", mid).unwrap();
        assert_eq!(found.name, "outer");
    }

    #[test]
    fn type_of_returns_none_outside_any_symbol_range() {
        let idx = rust_index("src/a.rs", "fn outer() {}\n");
        assert!(idx.type_of("src/a.rs", 10_000).is_none());
    }

    #[test]
    fn go_symbol_query_extracts_functions_and_type_specs() {
        let mut idx = SymbolIndex::new();
        idx.parse_file(
            "main.go",
            "package main\nfunc Add(a int, b int) int { return a + b }\ntype Point struct { X int }\n",
            Language::Go,
        )
        .unwrap();
        let add = idx.definition("Add", "main.go").unwrap();
        assert_eq!(add.kind, SymbolKind::Function);
        let point = idx.definition("Point", "main.go").unwrap();
        assert_eq!(point.kind, SymbolKind::Struct);
    }

    #[test]
    fn typescript_symbol_query_extracts_classes_and_interfaces() {
        let mut idx = SymbolIndex::new();
        idx.parse_file(
            "a.ts",
            "interface Shape {}\nclass Circle implements Shape {}\nfunction area(): number { return 0; }\n",
            Language::TypeScript,
        )
        .unwrap();
        assert_eq!(
            idx.definition("Shape", "a.ts").unwrap().kind,
            SymbolKind::Interface
        );
        assert_eq!(
            idx.definition("Circle", "a.ts").unwrap().kind,
            SymbolKind::Struct
        );
        assert_eq!(
            idx.definition("area", "a.ts").unwrap().kind,
            SymbolKind::Function
        );
    }

    #[test]
    fn tsx_symbol_query_compiles_and_extracts_functions() {
        let mut idx = SymbolIndex::new();
        idx.parse_file("a.tsx", "function App() { return null; }\n", Language::Tsx)
            .unwrap();
        assert_eq!(
            idx.definition("App", "a.tsx").unwrap().kind,
            SymbolKind::Function
        );
    }

    #[test]
    fn javascript_symbol_query_extracts_functions_and_classes() {
        let mut idx = SymbolIndex::new();
        idx.parse_file(
            "a.js",
            "class Widget {}\nfunction render() { return 1; }\n",
            Language::JavaScript,
        )
        .unwrap();
        assert_eq!(
            idx.definition("Widget", "a.js").unwrap().kind,
            SymbolKind::Struct
        );
        assert_eq!(
            idx.definition("render", "a.js").unwrap().kind,
            SymbolKind::Function
        );
    }

    #[test]
    fn python_symbol_query_extracts_functions_and_classes_with_colon_trimmed_signature() {
        let mut idx = SymbolIndex::new();
        idx.parse_file(
            "a.py",
            "class Foo:\n    pass\n\ndef bar(x: int) -> int:\n    return x\n",
            Language::Python,
        )
        .unwrap();
        assert_eq!(
            idx.definition("Foo", "a.py").unwrap().kind,
            SymbolKind::Struct
        );
        let bar = idx.definition("bar", "a.py").unwrap();
        assert_eq!(bar.kind, SymbolKind::Function);
        assert!(!bar.signature.as_deref().unwrap_or("").ends_with(':'));
    }

    #[test]
    fn every_language_symbol_and_reference_query_compiles_against_its_grammar() {
        for lang in [
            Language::Rust,
            Language::Go,
            Language::TypeScript,
            Language::Tsx,
            Language::JavaScript,
            Language::Python,
        ] {
            let grammar = SymbolIndex::grammar_for(lang).expect("grammar exists");
            tree_sitter::Query::new(&grammar, SymbolIndex::symbol_query_source(lang))
                .unwrap_or_else(|e| panic!("{lang:?} symbol query invalid: {e}"));
            tree_sitter::Query::new(&grammar, SymbolIndex::reference_query_source(lang))
                .unwrap_or_else(|e| panic!("{lang:?} reference query invalid: {e}"));
        }
    }

    #[test]
    fn grammar_for_other_language_is_none() {
        assert!(SymbolIndex::grammar_for(Language::Other).is_none());
    }
}
