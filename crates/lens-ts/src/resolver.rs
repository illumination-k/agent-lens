//! Import-specifier resolution for TS/JS, on top of [`oxc_resolver`].
//!
//! `oxc_resolver` implements the full Node / TypeScript algorithm: the
//! nearest `tsconfig.json`'s `paths` and `baseUrl` (with `extends` and
//! project references), `package.json` `exports` / `imports` maps,
//! extension probing, `index` files, and the TS ESM convention of
//! importing `./util.js` for `util.ts`. [`ModuleResolver`] narrows its
//! answer to what a source analysis can use: a TS/JS source file of the
//! project, never a declaration file, an asset, or a dependency under
//! `node_modules`.
//!
//! A JS/TS workspace member is the one case the resolver's answer is not
//! the one an analysis wants: without an install it cannot see the
//! member at all, and with one it lands on the member's built `dist/`,
//! which is not source. [`Workspace`] maps the package name straight
//! onto the member's `src/`, so it is asked first.

use std::path::{Component, Path, PathBuf};

use oxc_resolver::{ResolveOptions, Resolver, TsconfigDiscovery};

use crate::parser::MODULE_EXTENSIONS;
use crate::workspace::{Workspace, is_source_file};

/// Resolves import specifiers written in one project's files.
///
/// Build one per analysis and share it: the underlying resolver caches
/// every directory, manifest, and tsconfig it reads, and is `Sync`.
pub struct ModuleResolver {
    inner: Resolver,
    workspace: Workspace,
    /// The directory relative paths were spelled against, so a relative
    /// importer gets a relative answer back (the resolver itself only
    /// works on absolute paths).
    cwd: PathBuf,
}

impl std::fmt::Debug for ModuleResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModuleResolver")
            .field("workspace", &self.workspace)
            .finish_non_exhaustive()
    }
}

impl ModuleResolver {
    /// A resolver for the project `start` (a file or directory) lives
    /// in. The JS/TS workspace, if any, is discovered from `start`'s
    /// ancestors; tsconfigs are discovered per importing file.
    pub fn new(start: &Path) -> Self {
        let extensions = MODULE_EXTENSIONS
            .iter()
            .map(|ext| format!(".{ext}"))
            .collect::<Vec<_>>();
        let options = ResolveOptions {
            tsconfig: Some(TsconfigDiscovery::Auto),
            extensions: extensions.clone(),
            // `import "./util.js"` names `util.ts` under TS ESM rules.
            extension_alias: [".js", ".mjs", ".cjs", ".jsx"]
                .iter()
                .map(|ext| ((*ext).to_owned(), extensions.clone()))
                .collect(),
            condition_names: ["source", "development", "import", "module", "default"]
                .map(str::to_owned)
                .to_vec(),
            main_fields: ["source", "module", "main"].map(str::to_owned).to_vec(),
            // Keep answers in the caller's path space: resolving a
            // symlink would move a file out from under the analysis root.
            symlinks: false,
            ..ResolveOptions::default()
        };
        Self {
            inner: Resolver::new(options),
            workspace: Workspace::discover(start).unwrap_or_default(),
            cwd: std::env::current_dir().unwrap_or_default(),
        }
    }

    /// The project source file `specifier`, imported from `importer`,
    /// names. `None` for an external package, a Node builtin, an asset,
    /// or a target that does not exist.
    pub fn resolve(&self, importer: &Path, specifier: &str) -> Option<PathBuf> {
        if !is_followable_specifier(specifier) {
            return None;
        }
        let specifier = strip_query(specifier);
        // A member's source beats whatever an install linked into
        // `node_modules`, which is its build output at best.
        if !is_relative_specifier(specifier)
            && let Some(found) = self.workspace.resolve(specifier)
        {
            return Some(found);
        }
        let absolute = if importer.is_absolute() {
            importer.to_path_buf()
        } else {
            self.cwd.join(importer)
        };
        if let Ok(resolution) = self.inner.resolve_file(&absolute, specifier) {
            let found = resolution.path();
            if is_source_file(found) && !in_node_modules(found) {
                return Some(self.respell(importer, found));
            }
        }
        None
    }

    /// `found` in the spelling of `importer`: relative to the working
    /// directory when the importer was relative.
    fn respell(&self, importer: &Path, found: &Path) -> PathBuf {
        let found = normalize_path(found);
        if importer.is_absolute() {
            return found;
        }
        found
            .strip_prefix(&self.cwd)
            .map_or(found.clone(), Path::to_path_buf)
    }
}

pub(crate) fn is_relative_specifier(specifier: &str) -> bool {
    specifier.starts_with("./") || specifier.starts_with("../") || specifier == "."
}

/// Relative specifiers, plus bare ones (`@acme/ui`, `@/lib/db`,
/// `lodash`) that a tsconfig path, a package self-reference, or a
/// workspace member may map to a project file. Absolute paths and
/// URL-style specifiers (`node:fs`, `https://…`) never do.
pub(crate) fn is_followable_specifier(specifier: &str) -> bool {
    is_relative_specifier(specifier)
        || !(specifier.is_empty() || specifier.starts_with('/') || specifier.contains(':'))
}

/// Drop a bundler query or fragment (`./logo.svg?url`).
fn strip_query(specifier: &str) -> &str {
    specifier
        .split_once(['?', '#'])
        .map_or(specifier, |(path, _)| path)
}

fn in_node_modules(path: &Path) -> bool {
    path.components()
        .any(|c| c.as_os_str() == std::ffi::OsStr::new("node_modules"))
}

/// Fold `.` and `..` components without touching the filesystem.
pub(crate) fn normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component.as_os_str());
                }
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    if normalized.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        normalized
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use tempfile::TempDir;

    fn project(files: &[(&str, &str)]) -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (rel, content) in files {
            let path = dir.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        dir
    }

    /// Every specifier shape the issue (#578) names, in one fixture:
    /// relative, tsconfig `paths` (inherited through `extends`), an
    /// `exports` subpath of a workspace member, a member that is only
    /// built to `dist/`, TS ESM `.js` → `.ts`, and the shapes that must
    /// stay external.
    #[rstest]
    #[case::relative("apps/web/src/main.ts", "./util", Some("apps/web/src/util.ts"))]
    #[case::relative_index("apps/web/src/main.ts", "./lib", Some("apps/web/src/lib/index.ts"))]
    #[case::ts_esm_js_extension("apps/web/src/main.ts", "./util.js", Some("apps/web/src/util.ts"))]
    #[case::tsconfig_paths("apps/web/src/main.ts", "@/lib", Some("apps/web/src/lib/index.ts"))]
    #[case::tsconfig_paths_file("apps/web/src/main.ts", "@/util", Some("apps/web/src/util.ts"))]
    #[case::tsconfig_extends("apps/web/src/main.ts", "~shared/log", Some("shared/log.ts"))]
    #[case::exports_subpath(
        "apps/web/src/main.ts",
        "@acme/ui/icons/star",
        Some("packages/ui/src/icons/star.tsx")
    )]
    #[case::exports_root("apps/web/src/main.ts", "@acme/ui", Some("packages/ui/src/index.ts"))]
    #[case::unbuilt_main("apps/web/src/main.ts", "core", Some("packages/core/src/index.ts"))]
    #[case::asset("apps/web/src/main.ts", "./styles.css", None)]
    #[case::asset_query("apps/web/src/main.ts", "./logo.svg?url", None)]
    #[case::declaration_only("apps/web/src/main.ts", "./types", None)]
    #[case::external("apps/web/src/main.ts", "react", None)]
    #[case::builtin("apps/web/src/main.ts", "node:fs", None)]
    #[case::missing("apps/web/src/main.ts", "./nope", None)]
    fn resolves_project_sources_only(
        #[case] importer: &str,
        #[case] specifier: &str,
        #[case] expected: Option<&str>,
    ) {
        let dir = project(&[
            (
                "package.json",
                r#"{"workspaces": ["apps/*", "packages/*"]}"#,
            ),
            (
                "tsconfig.base.json",
                r#"{"compilerOptions": {"paths": {
                    "@/*": ["./apps/web/src/*"],
                    "~shared/*": ["./shared/*"]
                }}}"#,
            ),
            ("shared/log.ts", "export const log = 1;"),
            ("apps/web/package.json", r#"{"name": "web"}"#),
            (
                "apps/web/tsconfig.json",
                // JSONC, and `paths` inherited: relative to the base config.
                "{\n  // web\n  \"extends\": \"../../tsconfig.base.json\"\n}",
            ),
            ("apps/web/src/main.ts", ""),
            ("apps/web/src/util.ts", ""),
            ("apps/web/src/lib/index.ts", ""),
            ("apps/web/src/types.d.ts", ""),
            ("apps/web/src/styles.css", ""),
            ("apps/web/src/logo.svg", ""),
            (
                "packages/ui/package.json",
                r#"{"name": "@acme/ui", "exports": {
                    ".": {"types": "./dist/index.d.ts", "import": "./src/index.ts"},
                    "./icons/*": "./src/icons/*.tsx"
                }}"#,
            ),
            ("packages/ui/src/index.ts", ""),
            ("packages/ui/src/icons/star.tsx", ""),
            (
                "packages/core/package.json",
                r#"{"name": "core", "main": "./dist/index.js"}"#,
            ),
            ("packages/core/src/index.ts", ""),
            (
                "node_modules/react/package.json",
                r#"{"name": "react", "main": "index.js"}"#,
            ),
            ("node_modules/react/index.js", ""),
        ]);
        let root = dir.path();
        let resolver = ModuleResolver::new(root);
        assert_eq!(
            resolver.resolve(&root.join(importer), specifier),
            expected.map(|rel| root.join(rel)),
        );
    }

    /// A member linked into `node_modules` (what an install does) still
    /// resolves to its source, not to the link or its build output.
    #[cfg(unix)]
    #[test]
    fn linked_workspace_member_resolves_to_source() {
        let dir = project(&[
            ("package.json", r#"{"workspaces": ["packages/*"]}"#),
            ("packages/app/src/main.ts", ""),
            (
                "packages/ui/package.json",
                r#"{"name": "ui", "main": "./dist/index.js"}"#,
            ),
            ("packages/ui/dist/index.js", ""),
            ("packages/ui/src/index.ts", ""),
        ]);
        let root = dir.path();
        std::fs::create_dir_all(root.join("node_modules")).unwrap();
        std::os::unix::fs::symlink(root.join("packages/ui"), root.join("node_modules/ui")).unwrap();

        let resolver = ModuleResolver::new(root);
        assert_eq!(
            resolver.resolve(&root.join("packages/app/src/main.ts"), "ui"),
            Some(root.join("packages/ui/src/index.ts")),
        );
    }

    #[rstest]
    #[case("./util", true)]
    #[case("../util", true)]
    #[case("@acme/ui", true)]
    #[case("@/lib/db", true)]
    #[case("lodash", true)]
    #[case("", false)]
    #[case("/abs/path", false)]
    #[case("node:fs", false)]
    fn followable_specifiers(#[case] specifier: &str, #[case] expected: bool) {
        assert_eq!(is_followable_specifier(specifier), expected);
    }

    #[test]
    fn normalize_path_folds_dot_components() {
        assert_eq!(
            normalize_path(Path::new("src/./routes/../main.ts")),
            PathBuf::from("src/main.ts")
        );
    }
}
