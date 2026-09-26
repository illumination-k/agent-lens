#!/usr/bin/env node
// TypeScript type-checker oracle of the call-graph accuracy benchmark
// (issue #579). Type-checks every TS / JS source under -root with the
// TypeScript compiler API and writes the call edges between functions
// declared under root in the shared oracle contract (docs/callgraph-accuracy.md):
//
//   node oracle.mjs --root <dir> [--out <file.json>]
//
// Edge conventions (see README.md next to this file):
//   - a call is a CallExpression (including `super(...)`) or a NewExpression;
//     its callee is the declaration of the signature the checker resolves,
//     except that a call through a type-annotated `const` bound to a function
//     expression (`const f: Api["set"] = (x) => ...`, import aliases followed)
//     goes to that function, not to the annotation's signature.
//     An overload signature is mapped to the implementation; a callee with no
//     body (`declare function`, an implicit constructor) is dropped, as are
//     callees outside root, in node_modules or in a .d.ts. A call resolved to
//     an interface or abstract method instead gets a "dynamic" edge to that
//     method in every class under root that implements or extends its
//     declaring type (transitively): a CHA candidate set over nominal
//     heritage clauses (structural matches without a clause are not found).
//     A method call on a union-typed receiver (`r.map(f)`, `r: Ok | Err`)
//     gets a "dynamic" edge to the method of every member that has a body. Implicit calls (getters, JSX elements, tagged templates,
//     decorators, iterators) are not call expressions and have no edge.
//   - caller: the innermost function-like (named or anonymous) containing the
//     call, and the document sets caller_is_innermost_function: the scorer
//     maps caller_def_line to that function's node, or to the node enclosing
//     it. A call with no enclosing function (module top level) is dropped and
//     counted.
//   - call_line: the line of the callee's name (`foo` in `a.foo(x)`), else of
//     the callee expression.
//   - def lines: the first line of the declaration, decorators and modifiers
//     included, comments excluded; for a function or arrow expression that
//     initialises a variable, property or object-literal key, the line of that
//     declaration (`export const f = () => ...`).
//   - dispatch "static" when the callee expression names the callee: an
//     identifier or property whose symbol (import aliases followed) is the
//     function, the class method, or a `const` / `readonly` binding initialised
//     with it; `new C()` and `super()`. "dynamic" otherwise: a call through a
//     parameter, a `let`, a property or any other function value.
//   - analyzed_files: every source file under root the program type-checked.
//
// Logs go to stderr.

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import ts from "typescript";

const SOURCE_EXT = /\.(ts|tsx|mts|cts|js|jsx|mjs|cjs)$/;
const SKIP_DIRS = new Set(["node_modules", "dist", "build", "coverage", "vendor"]);

export function sourceFiles(root) {
  const out = [];
  const walk = (dir) => {
    for (const ent of fs.readdirSync(dir, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
      if (ent.name.startsWith(".")) continue;
      const full = path.join(dir, ent.name);
      if (ent.isDirectory()) {
        if (!SKIP_DIRS.has(ent.name)) walk(full);
      } else if (SOURCE_EXT.test(ent.name) && !/\.d\.[mc]?ts$/.test(ent.name)) {
        out.push(full);
      }
    }
  };
  walk(root);
  return out;
}

function compilerOptions(root) {
  const base = { allowJs: true, checkJs: false, noEmit: true, skipLibCheck: true, jsx: ts.JsxEmit.Preserve };
  const configPath = ts.findConfigFile(root, ts.sys.fileExists, "tsconfig.json");
  if (!configPath || path.dirname(configPath) !== root) return base;
  const { config, error } = ts.readConfigFile(configPath, ts.sys.readFile);
  if (error) return base;
  const parsed = ts.parseJsonConfigFileContent(config, ts.sys, root);
  return { ...parsed.options, ...base, jsx: parsed.options.jsx ?? base.jsx };
}

const isFunctionLike = (n) =>
  ts.isFunctionDeclaration(n) ||
  ts.isFunctionExpression(n) ||
  ts.isArrowFunction(n) ||
  ts.isMethodDeclaration(n) ||
  ts.isConstructorDeclaration(n) ||
  ts.isGetAccessorDeclaration(n) ||
  ts.isSetAccessorDeclaration(n);

const lineOf = (sf, pos) => sf.getLineAndCharacterOfPosition(pos).line + 1;

/** The node whose first line is the function's def line (see the header). */
export function defNode(fn) {
  const p = fn.parent;
  if ((ts.isArrowFunction(fn) || ts.isFunctionExpression(fn)) && p) {
    if (ts.isVariableDeclaration(p) && p.initializer === fn) {
      const list = p.parent;
      const stmt = list?.parent;
      return stmt && ts.isVariableStatement(stmt) && list.declarations.length === 1 ? stmt : p;
    }
    if ((ts.isPropertyDeclaration(p) || ts.isPropertyAssignment(p)) && p.initializer === fn) return p;
  }
  return fn;
}

/** The implementation behind a declaration, or undefined when it has no body. */
function implementation(decl, checker) {
  if (!decl || !isFunctionLike(decl)) return undefined;
  if (decl.body) return decl;
  const sym = decl.name ? checker.getSymbolAtLocation(decl.name) : decl.symbol;
  return (sym?.declarations ?? []).find((d) => isFunctionLike(d) && d.kind === decl.kind && d.body);
}

/**
 * `f(x)` where `f` is a const bound to a function expression runs that
 * function, whatever the const's type annotation says: with
 * `const f: Api['set'] = (x) => ...` the checker resolves the signature of
 * the annotation (`Api['set']`), not of the initializer.
 */
function constFunctionCallee(call, checker) {
  if (!ts.isCallExpression(call) || !ts.isIdentifier(call.expression)) return undefined;
  let sym = checker.getSymbolAtLocation(call.expression);
  if (sym && sym.flags & ts.SymbolFlags.Alias) sym = checker.getAliasedSymbol(sym);
  const binding = sym?.valueDeclaration;
  if (!binding || !ts.isVariableDeclaration(binding) || !binding.type || !isConstBinding(binding)) return undefined;
  const init = binding.initializer;
  return init && (ts.isArrowFunction(init) || ts.isFunctionExpression(init)) && init.body ? init : undefined;
}

/** The interface or abstract class member a bodiless signature declares, if any. */
function abstractMember(decl) {
  if (!decl) return undefined;
  if (ts.isMethodSignature(decl) || (ts.isMethodDeclaration(decl) && !decl.body)) return decl;
  if (ts.isFunctionTypeNode(decl) && decl.parent && ts.isPropertySignature(decl.parent)) return decl.parent;
  return undefined;
}

/** The binding a function is the value of: itself, or the const/readonly/key it initialises. */
function bindingOf(fn) {
  const node = defNode(fn);
  if (node === fn) return fn;
  if (ts.isVariableStatement(node)) return node.declarationList.declarations[0];
  return node;
}

function isConstBinding(decl) {
  if (ts.isVariableDeclaration(decl)) return (ts.getCombinedNodeFlags(decl) & ts.NodeFlags.Const) !== 0;
  if (ts.isPropertyDeclaration(decl)) return (ts.getCombinedModifierFlags(decl) & ts.ModifierFlags.Readonly) !== 0;
  return ts.isPropertyAssignment(decl);
}

function calleeName(expr) {
  let e = expr;
  while (ts.isParenthesizedExpression(e) || ts.isNonNullExpression(e) || ts.isAsExpression(e)) e = e.expression;
  if (ts.isPropertyAccessExpression(e)) return e.name;
  if (ts.isElementAccessExpression(e)) return e.argumentExpression;
  return e;
}

function dispatchOf(call, callee, checker) {
  if (ts.isNewExpression(call) || call.expression.kind === ts.SyntaxKind.SuperKeyword) return "static";
  const name = calleeName(call.expression);
  let sym = checker.getSymbolAtLocation(name);
  if (sym && sym.flags & ts.SymbolFlags.Alias) sym = checker.getAliasedSymbol(sym);
  const decls = sym?.declarations ?? [];
  const binding = bindingOf(callee);
  if (decls.includes(callee) || decls.some((d) => implementation(d, checker) === callee)) return "static";
  if (binding !== callee && decls.includes(binding) && isConstBinding(binding)) return "static";
  return "dynamic";
}

export function run(rootArg) {
  const root = fs.realpathSync(path.resolve(rootArg));
  const files = sourceFiles(root);
  const program = ts.createProgram(files, compilerOptions(root));
  const checker = program.getTypeChecker();
  const stats = {};
  const bump = (k) => (stats[k] = (stats[k] ?? 0) + 1);
  const rel = (f) => path.relative(root, f).split(path.sep).join("/");
  const underRoot = (f) => {
    const r = path.relative(root, f);
    return !r.startsWith("..") && !path.isAbsolute(r) && !r.split(path.sep).some((p) => SKIP_DIRS.has(p)) && !/\.d\.[mc]?ts$/.test(f);
  };

  // (heritage type symbol) -> classes under root that name it, directly or
  // through a base class / interface, in an `extends` or `implements` clause.
  let subclasses;
  const heritageSymbols = (decl, seen = new Set()) => {
    for (const clause of decl.heritageClauses ?? []) {
      for (const t of clause.types) {
        let sym = checker.getSymbolAtLocation(t.expression);
        if (sym && sym.flags & ts.SymbolFlags.Alias) sym = checker.getAliasedSymbol(sym);
        if (!sym || seen.has(sym)) continue;
        seen.add(sym);
        for (const d of sym.declarations ?? []) if (ts.isClassLike(d) || ts.isInterfaceDeclaration(d)) heritageSymbols(d, seen);
      }
    }
    return seen;
  };
  const implementationsOf = (member) => {
    if (!subclasses) {
      subclasses = new Map();
      for (const file of files) {
        const sf = program.getSourceFile(file);
        if (!sf) continue;
        const visit = (node) => {
          if (ts.isClassLike(node)) {
            for (const sym of heritageSymbols(node)) {
              if (!subclasses.has(sym)) subclasses.set(sym, []);
              subclasses.get(sym).push(node);
            }
          }
          ts.forEachChild(node, visit);
        };
        visit(sf);
      }
    }
    const owner = member.parent && (ts.isClassLike(member.parent) || ts.isInterfaceDeclaration(member.parent)) ? member.parent : undefined;
    const ownerSym = owner?.name && checker.getSymbolAtLocation(owner.name);
    const name = member.name?.getText();
    if (!ownerSym || !name) return [];
    const out = [];
    for (const cls of subclasses.get(ownerSym) ?? []) {
      for (const m of cls.members) {
        if ((ts.isMethodDeclaration(m) || ts.isPropertyDeclaration(m)) && m.name?.getText() === name) {
          const fn = ts.isMethodDeclaration(m) ? m : m.initializer;
          if (fn && isFunctionLike(fn) && fn.body) out.push(fn);
        }
      }
    }
    return out;
  };

  // `r.map(f)` with `r: Ok | Err`: the checker resolves one member's
  // signature, but either method may run. Returns every member's method.
  const unionMethods = (call) => {
    if (!ts.isCallExpression(call) || !ts.isPropertyAccessExpression(call.expression)) return [];
    const receiver = checker.getTypeAtLocation(call.expression.expression);
    if (!receiver.isUnion()) return [];
    const out = new Set();
    for (const t of receiver.types) {
      const prop = checker.getPropertyOfType(t, call.expression.name.text);
      for (const d of prop?.declarations ?? []) {
        const fn = ts.isPropertyDeclaration(d) ? d.initializer : d;
        const impl = fn && isFunctionLike(fn) ? implementation(fn, checker) : undefined;
        if (impl) out.add(impl);
      }
    }
    return [...out];
  };

  const edges = new Map();
  const analyzed = [];
  for (const file of files) {
    const sf = program.getSourceFile(file);
    if (!sf) continue;
    analyzed.push(rel(file));
    const visit = (node) => {
      if (ts.isCallExpression(node) || ts.isNewExpression(node)) record(sf, node);
      ts.forEachChild(node, visit);
    };
    visit(sf);
  }

  function record(sf, call) {
    if (ts.isCallExpression(call) && call.expression.kind === ts.SyntaxKind.ImportKeyword) return;
    const decl = checker.getResolvedSignature(call)?.declaration;
    if (decl && !underRoot(fs.realpathSync(decl.getSourceFile().fileName))) return bump("callee_outside_root");
    const callee = constFunctionCallee(call, checker) ?? implementation(decl, checker);
    const members = unionMethods(call);
    let targets;
    if (members.length > 1) targets = members.map((fn) => [fn, "dynamic"]);
    else if (callee) targets = [[callee, dispatchOf(call, callee, checker)]];
    else if (abstractMember(decl)) {
      targets = implementationsOf(abstractMember(decl)).map((fn) => [fn, "dynamic"]);
      if (!targets.length) return bump("abstract_callee_without_implementation");
    } else return bump("callee_without_body");
    let caller = call.parent;
    while (caller && !isFunctionLike(caller)) caller = caller.parent;
    if (!caller) return bump("caller_top_level");
    const nameNode = ts.isNewExpression(call) ? call.expression : calleeName(call.expression);
    for (const [fn, dispatch] of targets) {
      const calleeSf = fn.getSourceFile();
      const calleeFile = fs.realpathSync(calleeSf.fileName);
      if (!underRoot(calleeFile)) {
        bump("callee_outside_root");
        continue;
      }
      const edge = {
        caller_file: rel(sf.fileName),
        caller_def_line: lineOf(sf, defNode(caller).getStart(sf)),
        call_line: lineOf(sf, nameNode.getStart(sf)),
        callee_file: rel(calleeFile),
        callee_def_line: lineOf(calleeSf, defNode(fn).getStart(calleeSf)),
        dispatch,
      };
      edges.set(JSON.stringify(edge), edge);
    }
  }

  const keys = ["caller_file", "caller_def_line", "call_line", "callee_file", "callee_def_line", "dispatch"];
  const sorted = [...edges.values()].sort((a, b) => {
    for (const k of keys) if (a[k] !== b[k]) return a[k] < b[k] ? -1 : 1;
    return 0;
  });
  return {
    doc: { oracle: "typescript-checker", language: "typescript", kind: "type-checker", root, caller_is_innermost_function: true, edges: sorted, analyzed_files: analyzed.sort() },
    stats,
  };
}

function main(argv) {
  const args = {};
  for (let i = 0; i < argv.length; i += 2) args[argv[i].replace(/^--?/, "")] = argv[i + 1];
  if (!args.root) {
    console.error("oracle-ts: --root is required");
    return 2;
  }
  const { doc, stats } = run(args.root);
  const text = JSON.stringify(doc, null, 1) + "\n";
  if (args.out) fs.writeFileSync(args.out, text);
  else process.stdout.write(text);
  const dropped = Object.entries(stats).sort().map(([k, n]) => `${n} ${k}`).join(", ") || "nothing";
  console.error(`oracle-ts: ${doc.edges.length} edges from ${doc.analyzed_files.length} files; dropped: ${dropped}`);
  return 0;
}

if (process.argv[1] && fs.realpathSync(process.argv[1]) === fileURLToPath(import.meta.url)) {
  process.exitCode = main(process.argv.slice(2));
}
