//! Run the `quint` CLI and turn its JSON IR into a [`ParsedFile`].
//!
//! Quint's only parser is the TypeScript one inside the `quint` npm
//! package, so this adapter shells out to `quint parse --out` rather than
//! re-implementing the grammar. The binary is looked up as `quint` on
//! `PATH` unless [`QUINT_BIN_ENV`] names another one.
//!
//! Source text without a file ([`parse_source`]) is written to a scratch
//! directory first. Its relative `import ... from "./x"` declarations
//! cannot resolve there; that is harmless, because `quint parse` still
//! emits every module of the file and only a syntax error (`QNT000`)
//! makes the IR untrustworthy. [`parse_path`] parses the file in place
//! so imports resolve and the name table reaches across files.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::ir::{Decl, Id, Module, ParseOutput, SourceMap};

/// Environment variable naming the `quint` executable to run.
pub const QUINT_BIN_ENV: &str = "AGENT_LENS_QUINT";

/// Failure to obtain a Quint IR. `Clone` so a cached failure can be
/// handed to every extractor that asks for the same source.
#[derive(Debug, Clone, thiserror::Error)]
pub enum QuintError {
    #[error(
        "quint CLI not found (`{bin}`); install it with `npm i -g @informalsystems/quint` or set {QUINT_BIN_ENV}"
    )]
    NotInstalled { bin: String },
    #[error("failed to run quint: {0}")]
    Io(String),
    #[error("quint parse produced no IR: {0}")]
    Failed(String),
    #[error("failed to read quint IR: {0}")]
    InvalidIr(String),
    #[error("failed to parse Quint source: {0}")]
    Syntax(String),
}

/// One `.qnt` file's modules plus the lookups the extractors need.
#[derive(Debug, Clone, Default)]
pub struct ParsedFile {
    /// Modules declared in this file, in source order.
    pub modules: Vec<Module>,
    /// Node id → 1-based `(start_line, end_line)`, for this file's nodes.
    pub spans: HashMap<Id, (usize, usize)>,
    /// Name-use id → id of the definition it resolves to.
    pub resolved: HashMap<Id, Id>,
    /// Definition id → `(module, definition name)`, over every module the
    /// parse loaded (imported files included).
    pub def_module: HashMap<Id, (String, String)>,
}

impl ParsedFile {
    /// 1-based `(start_line, end_line)` of node `id`, when this file's
    /// source map covers it.
    pub fn span(&self, id: Id) -> Option<(usize, usize)> {
        self.spans.get(&id).copied()
    }
}

type ParseResult = Result<Arc<ParsedFile>, QuintError>;

/// Parse in-memory Quint source. Results are memoised per source text for
/// the life of the process, since every extractor asks for the same file
/// and each `quint` run costs a Node start-up.
pub fn parse_source(source: &str) -> Result<Arc<ParsedFile>, QuintError> {
    static CACHE: OnceLock<Mutex<HashMap<String, ParseResult>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(hit) = lock(cache).get(source) {
        return hit.clone();
    }
    let parsed = parse_source_with(&quint_bin(), source).map(Arc::new);
    lock(cache).insert(source.to_owned(), parsed.clone());
    parsed
}

/// Parse the `.qnt` file at `path` in place, resolving its imports.
pub fn parse_path(path: &Path) -> Result<ParsedFile, QuintError> {
    let scratch = ScratchDir::create()?;
    run_quint(&quint_bin(), path, &scratch)
}

fn quint_bin() -> String {
    std::env::var(QUINT_BIN_ENV).unwrap_or_else(|_| "quint".to_owned())
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn parse_source_with(bin: &str, source: &str) -> Result<ParsedFile, QuintError> {
    let scratch = ScratchDir::create()?;
    let input = scratch.path.join("input.qnt");
    std::fs::write(&input, source).map_err(|e| QuintError::Io(e.to_string()))?;
    run_quint(bin, &input, &scratch)
}

fn run_quint(bin: &str, input: &Path, scratch: &ScratchDir) -> Result<ParsedFile, QuintError> {
    let out = scratch.path.join("out.json");
    let map = scratch.path.join("map.json");
    let input = std::fs::canonicalize(input).map_err(|e| QuintError::Io(e.to_string()))?;
    let output = Command::new(bin)
        .arg("parse")
        .arg(&input)
        .arg("--out")
        .arg(&out)
        .arg("--source-map")
        .arg(&map)
        .output()
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => QuintError::NotInstalled {
                bin: bin.to_owned(),
            },
            _ => QuintError::Io(e.to_string()),
        })?;
    // `quint parse` exits 1 on any diagnostic, name-resolution ones
    // included, yet still writes the IR. Only a missing IR is a failure
    // of the run itself.
    let Ok(ir) = std::fs::read_to_string(&out) else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(QuintError::Failed(stderr.trim().to_owned()));
    };
    let ir: ParseOutput =
        serde_json::from_str(&ir).map_err(|e| QuintError::InvalidIr(e.to_string()))?;
    if let Some(err) = ir
        .errors
        .iter()
        .find(|e| e.explanation.starts_with("[QNT000]"))
    {
        return Err(QuintError::Syntax(err.explanation.clone()));
    }
    let map = std::fs::read_to_string(&map).map_err(|e| QuintError::Io(e.to_string()))?;
    let map: SourceMap =
        serde_json::from_str(&map).map_err(|e| QuintError::InvalidIr(e.to_string()))?;
    Ok(assemble(ir, &map, &input))
}

/// Build a [`ParsedFile`] from captured `quint parse` output, as if
/// `input` had been parsed. Lets tests run without the CLI.
#[cfg(test)]
pub(crate) fn from_captured(ir: &str, map: &str, input: &Path) -> ParsedFile {
    let ir: ParseOutput = serde_json::from_str(ir).unwrap();
    let map: SourceMap = serde_json::from_str(map).unwrap();
    assemble(ir, &map, input)
}

fn assemble(ir: ParseOutput, map: &SourceMap, input: &Path) -> ParsedFile {
    let own_source: Vec<&str> = map
        .source_index
        .iter()
        .filter(|(_, path)| same_file(Path::new(path), input))
        .map(|(key, _)| key.as_str())
        .collect();
    let mut spans = HashMap::new();
    for (id, (source, start, end)) in &map.map {
        let source = match source {
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        if !own_source.contains(&source.as_str()) {
            continue;
        }
        if let Ok(id) = id.parse::<Id>() {
            spans.insert(id, (start.line + 1, end.line + 1));
        }
    }
    let mut def_module = HashMap::new();
    for module in &ir.modules {
        for decl in &module.declarations {
            if let Decl::Def(def) = decl {
                def_module.insert(def.id, (module.name.clone(), def.name.clone()));
            }
        }
    }
    let resolved = ir
        .table
        .iter()
        .filter_map(|(use_id, entry)| Some((use_id.parse::<Id>().ok()?, entry.id)))
        .collect();
    let mut modules: Vec<Module> = ir
        .modules
        .into_iter()
        .filter(|m| spans.contains_key(&m.id))
        .collect();
    modules.sort_by_key(|m| spans.get(&m.id).map_or(0, |s| s.0));
    ParsedFile {
        modules,
        spans,
        resolved,
        def_module,
    }
}

fn same_file(reported: &Path, input: &Path) -> bool {
    reported == input || std::fs::canonicalize(reported).is_ok_and(|p| p == input)
}

/// A private directory under the system temp dir, removed on drop.
struct ScratchDir {
    path: PathBuf,
}

impl ScratchDir {
    fn create() -> Result<Self, QuintError> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let base = std::env::temp_dir();
        loop {
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = base.join(format!("agent-lens-quint-{}-{n}", std::process::id()));
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(QuintError::Io(e.to_string())),
            }
        }
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.path) {
            tracing::debug!(path = %self.path.display(), error = %e, "failed to remove quint scratch dir");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_binary_is_reported_as_not_installed() {
        let err = parse_source_with("agent-lens-no-such-quint", "module m {}").unwrap_err();
        assert!(matches!(err, QuintError::NotInstalled { .. }), "{err}");
        assert!(err.to_string().contains(QUINT_BIN_ENV));
    }

    #[test]
    fn scratch_dirs_are_removed_on_drop() {
        let path = ScratchDir::create().unwrap().path.clone();
        assert!(!path.exists());
    }

    /// The public extractors over a real `quint`, when one is on `PATH`.
    #[test]
    fn live_public_extractors_see_the_fixture() {
        let source = include_str!("../testdata/counter.qnt");
        let units = match crate::extract_complexity_units(source) {
            Err(QuintError::NotInstalled { .. }) => return,
            other => other.unwrap(),
        };
        assert!(
            units
                .iter()
                .any(|u| u.name == "classify" && u.cyclomatic == 3)
        );
        let wrappers = crate::find_wrappers(source).unwrap();
        assert!(wrappers.iter().any(|w| w.name == "forward"));
        let refs = crate::extract_call_refs(source).unwrap();
        assert!(refs.iter().any(|r| r.callee == "double"));
        let mut parser = crate::QuintParser::new();
        let fns = lens_domain::LanguageParser::extract_functions(&mut parser, source).unwrap();
        assert!(fns.iter().any(|f| f.name == "double"));
    }

    /// Runs against a real `quint` when one is on `PATH`; CI without
    /// Node skips it, the captured fixtures cover the IR handling there.
    #[test]
    fn live_quint_parses_source_and_flags_syntax_errors() {
        let ok = parse_source_with("quint", "module m {\n  val x = 1\n}\n");
        if matches!(ok, Err(QuintError::NotInstalled { .. })) {
            return;
        }
        let file = ok.unwrap();
        assert_eq!(file.modules.len(), 1);
        let err = parse_source_with("quint", "module m {\n  val x = 1 +\n}\n").unwrap_err();
        assert!(matches!(err, QuintError::Syntax(_)), "{err}");
        // An import that cannot resolve from the scratch dir is not fatal.
        let file = parse_source_with(
            "quint",
            "module m {\n  import a.* from \"./a\"\n  val y = 2\n}\n",
        )
        .unwrap();
        assert_eq!(file.modules[0].name, "m");
    }
}
