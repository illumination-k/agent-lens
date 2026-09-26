# oracle-ts

The TypeScript type-checker oracle of the call-graph accuracy benchmark (#579).
It type-checks every `.ts` / `.tsx` / `.mts` / `.cts` / `.js` / `.jsx` / `.mjs`
/ `.cjs` file under the root (no `node_modules`, `dist`, `build`, `coverage`,
`vendor`, dot-directories or `.d.ts`) as one program with the root's
`tsconfig.json` options (paths, jsx, decorators), resolves every call and `new`
with `checker.getResolvedSignature`, and writes the edges between functions
declared under the root in the shared oracle contract
(`"oracle": "typescript-checker"`, `"kind": "type-checker"`,
`"caller_is_innermost_function": true`).

```sh
npm ci
node oracle.mjs --root <dir> --out <file.json>
npm test   # oracle.test.mjs on an inline fixture
```

TypeScript is pinned to 5.9, the last major line with the JavaScript compiler
API (7.x is the native port). The checkout under test gets no `npm install`:
values typed by an absent dependency are `any` and their calls have no edge,
which only loses calls into code outside the root. Logs and the drop counts go
to stderr.

The edge conventions (caller, callee, def and call lines, dispatch) are in
the header comment of `oracle.mjs`; `docs/callgraph-accuracy.md` has the
contract and how the scorer uses it.
