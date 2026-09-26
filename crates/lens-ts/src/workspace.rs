//! JS/TS monorepo workspaces: resolving `import "@acme/ui"` to source.
//!
//! In a workspace (npm / yarn / bun `package.json#workspaces`, or
//! `pnpm-workspace.yaml`) sibling packages import each other by package
//! name rather than by relative path, so a module graph that only follows
//! `./` and `../` stops at every package boundary — exactly the edges a
//! monorepo's architecture is about. [`Workspace`] maps those bare
//! specifiers back to the member package's source file.
//!
//! Only workspace members resolve. Everything else — `react`, `node:fs`,
//! a package in `node_modules` — stays external, as before.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// `exports` conditions in the order a source-level resolver should try
/// them. `types` is deliberately absent: a `.d.ts` is not the module.
const EXPORT_CONDITIONS: &[&str] = &[
    "source",
    "development",
    "import",
    "module",
    "default",
    "require",
    "node",
    "browser",
];

/// Build-output directories whose files usually mirror `src/`. A manifest
/// that points `main` at `./dist/index.js` still names `src/index.ts`
/// when the package has not been built.
const BUILD_DIRS: &[&str] = &["dist", "lib", "build", "out"];

/// Directories never searched for member packages.
const SKIPPED_DIRS: &[&str] = &["node_modules", ".git"];

/// The member packages of one JS/TS workspace, keyed by package name.
#[derive(Debug, Clone, Default)]
pub(crate) struct Workspace {
    packages: HashMap<String, Package>,
}

#[derive(Debug, Clone)]
struct Package {
    dir: PathBuf,
    manifest: Value,
}

impl Workspace {
    /// Find the workspace `start` belongs to by walking its ancestors (as
    /// spelled, so every path found stays in the caller's path space) up
    /// to the first directory that declares one. `None` outside one.
    pub(crate) fn discover(start: &Path) -> Option<Self> {
        start
            .ancestors()
            .skip(usize::from(start.is_file()))
            .find_map(|dir| {
                let patterns = workspace_patterns(dir)?;
                Some(Self::from_patterns(dir, &patterns))
            })
    }

    fn from_patterns(root: &Path, patterns: &[String]) -> Self {
        let mut include = Vec::new();
        let mut exclude = Vec::new();
        for pattern in patterns {
            match pattern.strip_prefix('!') {
                Some(negated) => exclude.extend(expand_pattern(root, negated)),
                None => include.extend(expand_pattern(root, pattern)),
            }
        }
        let mut packages = HashMap::new();
        for dir in include {
            if exclude.contains(&dir) {
                continue;
            }
            let Some(manifest) = read_json(&dir.join("package.json")) else {
                continue;
            };
            let Some(name) = manifest.get("name").and_then(Value::as_str) else {
                continue;
            };
            packages
                .entry(name.to_owned())
                .or_insert(Package { dir, manifest });
        }
        Self { packages }
    }

    /// The source file a bare `specifier` names, when it names a member
    /// package (or a subpath of one) and that file exists.
    pub(crate) fn resolve(&self, specifier: &str) -> Option<PathBuf> {
        let (name, subpath) = split_package_specifier(specifier)?;
        let package = self.packages.get(name)?;
        package
            .candidates(subpath)
            .into_iter()
            .find_map(|target| resolve_in_package(&package.dir, &target))
    }
}

impl Package {
    /// Package-relative targets for `subpath` (empty for the package
    /// itself), most authoritative first: `exports`, then the legacy
    /// entry fields, then the conventional `src/` layout.
    fn candidates(&self, subpath: &str) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(exports) = self.manifest.get("exports") {
            let key = if subpath.is_empty() {
                ".".to_owned()
            } else {
                format!("./{subpath}")
            };
            exports_targets(exports, &key, &mut out);
        }
        if subpath.is_empty() {
            for field in ["source", "module", "main"] {
                if let Some(target) = self.manifest.get(field).and_then(Value::as_str) {
                    out.push(target.to_owned());
                }
            }
            out.extend(["./src/index".to_owned(), "./index".to_owned()]);
        } else {
            out.extend([format!("./src/{subpath}"), format!("./{subpath}")]);
        }
        out
    }
}

/// Collect the targets `exports` maps `key` to.
fn exports_targets(exports: &Value, key: &str, out: &mut Vec<String>) {
    let is_subpath_map = exports
        .as_object()
        .is_some_and(|map| map.keys().any(|k| k.starts_with('.')));
    if !is_subpath_map {
        // A bare string or a conditions object is the `"."` export alone.
        if key == "." {
            condition_targets(exports, None, out);
        }
        return;
    }
    let Some(map) = exports.as_object() else {
        return;
    };
    if let Some(value) = map.get(key) {
        condition_targets(value, None, out);
        return;
    }
    // `"./*": "./src/*.ts"` — the one-wildcard pattern form.
    for (pattern, value) in map {
        let Some((prefix, suffix)) = pattern.split_once('*') else {
            continue;
        };
        if let Some(matched) = key
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_suffix(suffix))
        {
            condition_targets(value, Some(matched), out);
        }
    }
}

/// Flatten one `exports` value — a string, an array of fallbacks, or a
/// conditions object — into targets, substituting `*` when given.
fn condition_targets(value: &Value, wildcard: Option<&str>, out: &mut Vec<String>) {
    match value {
        Value::String(target) => out.push(match wildcard {
            Some(matched) => target.replace('*', matched),
            None => target.clone(),
        }),
        Value::Array(items) => {
            for item in items {
                condition_targets(item, wildcard, out);
            }
        }
        Value::Object(conditions) => {
            for condition in EXPORT_CONDITIONS {
                if let Some(nested) = conditions.get(*condition) {
                    condition_targets(nested, wildcard, out);
                }
            }
        }
        _ => {}
    }
}

/// Resolve one package-relative target to an existing source file.
fn resolve_in_package(dir: &Path, target: &str) -> Option<PathBuf> {
    let target = target.strip_prefix("./").unwrap_or(target);
    if target.starts_with('/') || target.split('/').any(|part| part == "..") {
        return None;
    }
    let joined = dir.join(target);
    if let Some(found) = existing_source(&joined) {
        return Some(found);
    }
    // `./dist/index.js` before a build: try the mirrored `src/` file.
    let (first, rest) = target.split_once('/')?;
    if !BUILD_DIRS.contains(&first) {
        return None;
    }
    existing_source(&dir.join("src").join(rest))
}

/// `path` itself when it is a source file, else the file or `index` the
/// TS resolver would pick for an extensionless `path`.
fn existing_source(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return is_source_file(path).then(|| path.to_path_buf());
    }
    crate::coupling::probe_module(path).filter(|found| is_source_file(found))
}

/// A code file the parser handles, and not a declaration file.
fn is_source_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    crate::parser::Dialect::from_path(path).is_some()
        && ![".d.ts", ".d.mts", ".d.cts"]
            .iter()
            .any(|suffix| name.ends_with(suffix))
}

/// `@scope/name/sub/path` → (`@scope/name`, `sub/path`); `name/sub` →
/// (`name`, `sub`). `None` for anything that is not a bare package
/// specifier (relative, absolute, or a `node:` style URL).
fn split_package_specifier(specifier: &str) -> Option<(&str, &str)> {
    if specifier.is_empty()
        || specifier.starts_with('.')
        || specifier.starts_with('/')
        || specifier.contains(':')
    {
        return None;
    }
    let name_len = if specifier.starts_with('@') {
        let slash = specifier.find('/')?;
        specifier[slash + 1..]
            .find('/')
            .map_or(specifier.len(), |i| slash + 1 + i)
    } else {
        specifier.find('/').unwrap_or(specifier.len())
    };
    let (name, rest) = specifier.split_at(name_len);
    Some((name, rest.trim_start_matches('/')))
}

/// The member globs `dir` declares, if it is a workspace root.
fn workspace_patterns(dir: &Path) -> Option<Vec<String>> {
    if let Ok(yaml) = std::fs::read_to_string(dir.join("pnpm-workspace.yaml")) {
        return Some(pnpm_packages(&yaml));
    }
    let manifest = read_json(&dir.join("package.json"))?;
    let workspaces = manifest.get("workspaces")?;
    // Yarn classic also allows `{ "packages": [...] }`.
    let list = workspaces.get("packages").unwrap_or(workspaces);
    Some(
        list.as_array()?
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
    )
}

/// The `packages:` list of a `pnpm-workspace.yaml`, in either block
/// (`- "apps/*"`) or flow (`[apps/*, packages/*]`) style. Only that one
/// key is read, so a full YAML parser is not needed.
fn pnpm_packages(yaml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_packages = false;
    for line in yaml.lines() {
        let content = strip_yaml_comment(line);
        if content.trim().is_empty() {
            continue;
        }
        let indented = content.starts_with([' ', '\t']);
        if !indented {
            in_packages = false;
            if let Some(rest) = content.strip_prefix("packages:") {
                let rest = rest.trim();
                if let Some(flow) = rest.strip_prefix('[') {
                    let flow = flow.trim_end_matches(']');
                    out.extend(flow.split(',').map(unquote).filter(|p| !p.is_empty()));
                } else {
                    in_packages = true;
                }
            }
            continue;
        }
        if in_packages && let Some(item) = content.trim().strip_prefix('-') {
            let item = unquote(item);
            if !item.is_empty() {
                out.push(item);
            }
        }
    }
    out
}

fn strip_yaml_comment(line: &str) -> &str {
    match line.find(" #") {
        Some(i) => &line[..i],
        None if line.trim_start().starts_with('#') => "",
        None => line,
    }
}

fn unquote(value: &str) -> String {
    value.trim().trim_matches(['"', '\'']).to_owned()
}

/// Directories under `root` a workspace glob matches. Supports literal
/// segments, `*` inside a segment, and `**` for any depth.
fn expand_pattern(root: &Path, pattern: &str) -> Vec<PathBuf> {
    let pattern = pattern.trim_start_matches("./").trim_end_matches('/');
    let mut current = vec![root.to_path_buf()];
    for segment in pattern.split('/').filter(|s| !s.is_empty()) {
        let mut next = Vec::new();
        for dir in &current {
            if segment == "**" {
                collect_descendants(dir, &mut next);
            } else if segment.contains('*') {
                next.extend(
                    subdirs(dir)
                        .into_iter()
                        .filter(|d| file_name(d).is_some_and(|n| wildcard_match(segment, n))),
                );
            } else {
                let joined = dir.join(segment);
                if joined.is_dir() {
                    next.push(joined);
                }
            }
        }
        next.sort();
        next.dedup();
        current = next;
    }
    current
}

fn collect_descendants(dir: &Path, out: &mut Vec<PathBuf>) {
    out.push(dir.to_path_buf());
    for child in subdirs(dir) {
        collect_descendants(&child, out);
    }
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .map(|entry| entry.path())
        .filter(|path| {
            file_name(path).is_some_and(|n| !n.starts_with('.') && !SKIPPED_DIRS.contains(&n))
        })
        .collect();
    out.sort();
    out
}

fn file_name(path: &Path) -> Option<&str> {
    path.file_name().and_then(|n| n.to_str())
}

/// Match `name` against a segment glob where `*` is any run of characters.
fn wildcard_match(pattern: &str, name: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or("");
    let Some(mut rest) = name.strip_prefix(first) else {
        return false;
    };
    let mut parts: Vec<&str> = parts.collect();
    let Some(last) = parts.pop() else {
        return rest.is_empty();
    };
    for part in parts {
        match rest.find(part) {
            Some(i) => rest = &rest[i + part.len()..],
            None => return false,
        }
    }
    rest.len() >= last.len() && rest.ends_with(last)
}

fn read_json(path: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case("@acme/ui", Some(("@acme/ui", "")))]
    #[case("@acme/ui/button", Some(("@acme/ui", "button")))]
    #[case("@acme/ui/a/b", Some(("@acme/ui", "a/b")))]
    #[case("lodash", Some(("lodash", "")))]
    #[case("lodash/fp", Some(("lodash", "fp")))]
    #[case("./util", None)]
    #[case("../util", None)]
    #[case("/abs", None)]
    #[case("node:fs", None)]
    #[case("@scope", None)]
    #[case("", None)]
    fn splits_package_specifiers(#[case] spec: &str, #[case] expected: Option<(&str, &str)>) {
        assert_eq!(split_package_specifier(spec), expected);
    }

    #[rstest]
    #[case("*", "anything", true)]
    #[case("pkg-*", "pkg-ui", true)]
    #[case("pkg-*", "app-ui", false)]
    #[case("*-kit", "ui-kit", true)]
    #[case("*-kit", "ui-kits", false)]
    #[case("a*b*c", "axxbyyc", true)]
    #[case("a*b*c", "ac", false)]
    #[case("exact", "exact", true)]
    #[case("exact", "exactly", false)]
    // The middle part is consumed, so it cannot also satisfy the suffix.
    #[case("*ab*b", "ab", false)]
    fn matches_segment_wildcards(#[case] pattern: &str, #[case] name: &str, #[case] hit: bool) {
        assert_eq!(wildcard_match(pattern, name), hit);
    }

    #[rstest]
    #[case::block(
        "packages:\n  - 'apps/*'\n  - \"packages/*\" # libs\n  - '!**/test/**'\ncatalog:\n  - x\n",
        vec!["apps/*", "packages/*", "!**/test/**"],
    )]
    #[case::flow("packages: [apps/*, 'libs/*']\n", vec!["apps/*", "libs/*"])]
    #[case::none("catalog:\n  react: ^19\n", vec![])]
    // A column-0 comment inside the list does not end it.
    #[case::comment_line("packages:\n# apps\n  - apps/*\n", vec!["apps/*"])]
    fn reads_pnpm_packages(#[case] yaml: &str, #[case] expected: Vec<&str>) {
        assert_eq!(pnpm_packages(yaml), expected);
    }

    fn package(manifest: &str) -> Package {
        Package {
            dir: PathBuf::from("pkg"),
            manifest: serde_json::from_str(manifest).unwrap(),
        }
    }

    const SRC_ROOT: [&str; 2] = ["./src/index", "./index"];

    #[rstest]
    #[case::string_export(r#"{"exports": "./a.js"}"#, "", vec!["./a.js"])]
    // Conditions in resolver order; `types` never a candidate.
    #[case::conditions(
        r#"{"exports": {"types": "./t.d.ts", "default": "./d.js", "import": "./i.js"}}"#,
        "",
        vec!["./i.js", "./d.js"],
    )]
    #[case::fallback_array(r#"{"exports": ["./a.js", {"import": "./b.js"}]}"#, "", vec!["./a.js", "./b.js"])]
    #[case::subpath_map(
        r#"{"exports": {".": "./root.js", "./x": {"import": "./x.js"}}}"#,
        "",
        vec!["./root.js"],
    )]
    #[case::subpath_hit(
        r#"{"exports": {".": "./root.js", "./x": "./x.js"}}"#,
        "x",
        vec!["./x.js"],
    )]
    #[case::wildcard(r#"{"exports": {"./*": "./src/*.ts"}}"#, "a/b", vec!["./src/a/b.ts"])]
    // A conditions-only `exports` is the root export, never a subpath.
    #[case::conditions_not_subpath(r#"{"exports": {"import": "./i.js"}}"#, "x", vec![])]
    #[case::entry_fields(
        r#"{"main": "./m.js", "module": "./mod.js", "source": "./s.ts"}"#,
        "",
        vec!["./s.ts", "./mod.js", "./m.js"],
    )]
    fn lists_candidates_in_resolution_order(
        #[case] manifest: &str,
        #[case] subpath: &str,
        #[case] expected: Vec<&str>,
    ) {
        let mut expected: Vec<String> = expected.into_iter().map(str::to_owned).collect();
        if subpath.is_empty() {
            expected.extend(SRC_ROOT.map(str::to_owned));
        } else {
            expected.extend([format!("./src/{subpath}"), format!("./{subpath}")]);
        }
        assert_eq!(package(manifest).candidates(subpath), expected);
    }

    #[test]
    fn resolve_in_package_stays_inside_the_package() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("pkg");
        write(&pkg.join("src/index.ts"), "");
        write(&pkg.join("src/types.d.ts"), "");
        write(&pkg.join("data.json"), "");
        write(&dir.path().join("outside.ts"), "");
        let outside = dir.path().join("outside.ts");

        assert_eq!(
            resolve_in_package(&pkg, "./src/index.ts"),
            Some(pkg.join("src/index.ts"))
        );
        // Build output mirrors `src/`; another directory does not.
        assert_eq!(
            resolve_in_package(&pkg, "./dist/index.js"),
            Some(pkg.join("src/index.ts"))
        );
        assert_eq!(resolve_in_package(&pkg, "./other/index.js"), None);
        assert_eq!(resolve_in_package(&pkg, "../outside.ts"), None);
        assert_eq!(resolve_in_package(&pkg, outside.to_str().unwrap()), None);
        // Declaration files and non-code files are not modules.
        assert_eq!(resolve_in_package(&pkg, "./src/types.d.ts"), None);
        assert_eq!(resolve_in_package(&pkg, "./data.json"), None);
    }

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn resolves_exports_entry_fields_and_src_fallbacks() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            &root.join("package.json"),
            r#"{"workspaces": ["packages/*", "!packages/ignored"]}"#,
        );
        // `exports` with conditions, `types` skipped, wildcard subpaths.
        write(
            &root.join("packages/ui/package.json"),
            r#"{"name": "@acme/ui", "exports": {
                ".": {"types": "./dist/index.d.ts", "import": "./dist/index.js"},
                "./icons/*": "./src/icons/*.tsx"
            }}"#,
        );
        write(&root.join("packages/ui/src/index.ts"), "");
        write(&root.join("packages/ui/src/icons/star.tsx"), "");
        write(&root.join("packages/ui/src/button.ts"), "");
        // Legacy `main`, present on disk.
        write(
            &root.join("packages/core/package.json"),
            r#"{"name": "core", "main": "main.js"}"#,
        );
        write(&root.join("packages/core/main.js"), "");
        write(
            &root.join("packages/ignored/package.json"),
            r#"{"name": "ignored"}"#,
        );
        write(&root.join("packages/ignored/index.ts"), "");

        let ws = Workspace::discover(&root.join("packages/ui/src/index.ts")).unwrap();
        let ui = root.join("packages/ui");
        assert_eq!(ws.resolve("@acme/ui"), Some(ui.join("src/index.ts")));
        assert_eq!(
            ws.resolve("@acme/ui/icons/star"),
            Some(ui.join("src/icons/star.tsx"))
        );
        // A subpath `exports` does not list still resolves under `src/`.
        assert_eq!(
            ws.resolve("@acme/ui/button"),
            Some(ui.join("src/button.ts"))
        );
        assert_eq!(ws.resolve("core"), Some(root.join("packages/core/main.js")));
        assert_eq!(ws.resolve("ignored"), None, "negated pattern");
        assert_eq!(ws.resolve("react"), None, "not a member");
    }

    #[test]
    fn discovers_pnpm_workspaces_with_nested_globs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            &root.join("pnpm-workspace.yaml"),
            "packages:\n  - libs/**\n",
        );
        write(
            &root.join("libs/group/util/package.json"),
            r#"{"name": "util"}"#,
        );
        write(&root.join("libs/group/util/src/index.ts"), "");
        write(
            &root.join("libs/node_modules/dep/package.json"),
            r#"{"name": "dep"}"#,
        );
        write(&root.join("libs/node_modules/dep/index.js"), "");

        let ws = Workspace::discover(&root.join("libs/group/util/src")).unwrap();
        assert_eq!(
            ws.resolve("util"),
            Some(root.join("libs/group/util/src/index.ts"))
        );
        assert_eq!(ws.resolve("dep"), None, "node_modules is never a member");
    }

    #[test]
    fn no_workspace_outside_one() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("package.json"), r#"{"name": "solo"}"#);
        assert!(Workspace::discover(&dir.path().join("index.ts")).is_none());
    }
}
