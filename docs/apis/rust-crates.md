# Rust Crates

Rolldown is also provided as a Rust crate on crates.io.

The main crate is the [rolldown](https://crates.io/crates/rolldown) crate.

## Maintenance Policy

Before using the crates, make sure to understand the following policies:

- The crates will not follow the semver contract. Breaking changes may be introduced freely in any version.
- The documentation for the crates will not be provided.
- Any issues that only affect for the Rust crates will not be worked on as a team and will be closed. That said, we will accept pull requests for those use cases.

The JS package is our focus and we would like to reduce the maintenance cost of the crates as much as possible. If there's a long-time contributor who is willing to take on the maintenance of this area, we are open to revisiting the last two policies.

## Maintained fork consumption

The upstream policy above still describes upstream support. This maintained fork owns its compiler and plugin extensions; downstream projects consume that implementation rather than copy and patch the sources independently.

Pin dependencies to a full commit in `https://github.com/kristofer-baxter/rolldown`. Crates that exchange Rust types must resolve from the same commit, notably `rolldown`, `rolldown_plugin`, `rolldown_common`, `rolldown_fs`, `rolldown_error`, and `rolldown_resolver`. A consumer's root `[patch.crates-io]` must also cover any other workspace crates that cross its API boundaries, such as `rolldown_ecmascript`, `rolldown_sourcemap`, or `rolldown_utils`. Path dependencies within the Git workspace resolve together; explicit registry dependencies elsewhere need the root patches.

The current compiler/plugin/common/fs/error/resolver package versions are all `1.2.16`. The compiler uses the Oxc `0.158.0` AST, allocator, semantic, transformer, and minifier family from `https://github.com/kristofer-baxter/oxc`. Every direct compiler-family dependency in `Cargo.toml`, including `oxc_napi`, `oxc_minify_napi`, `oxc_parser_napi`, and `oxc_transform_napi`, pins the same full Git revision. The remaining coupled compiler crates resolve through that Git workspace's internal dependencies; standalone Rolldown must not resolve a registry copy of any of them.

Cargo honors `[patch]` only in the top-level consuming workspace. A Git dependency's root patches and lockfile do not control the consumer's resolution. This fork therefore uses direct Git dependencies for Oxc rather than relying on a root Oxc patch: those dependency declarations still apply when another workspace consumes Rolldown. Consumers that also use Oxc or React compiler integrations must use the exact same canonical Git URL and revision. Their own root `[patch.crates-io]` entries may be needed to redirect third-party registry dependencies to that revision; such registry patches do not replace Rolldown's explicit Git pin. Registry and Git packages with the same version are still different Rust type identities.

Verify the full standalone graph, including the bindings' NAPI dependencies, from this fork root:

```sh
cargo metadata --locked --format-version 1 | node .github/scripts/check-oxc-sources.mjs
node --test .github/scripts/check-oxc-sources.test.mjs
```

The check rejects duplicate compiler identities, registry/path sources, differing revisions or versions, a missing NAPI compiler package, and workspace declarations that rely only on root patches. Use `cargo tree -d` to inspect unrelated duplicate dependencies. Consumers should separately verify their complete graph with `cargo metadata --locked`, because this standalone check cannot establish their crate identity.

The companion `rolldown_ecmascript`, `rolldown_sourcemap`, and `rolldown_utils` packages are also `1.2.16`. The independent `oxc_index` family is `5`, the separate resolver and resolver-NAPI family is `11.24.3`, and `oxc_sourcemap` is `9.0.0` with this repository's existing vendor patch. These are not part of the `0.158.0` compiler Git family. The source-map root patch is also not inherited downstream; pin the maintained source-map implementation in the consumer root if using its fork extensions.

No source synchronization script, independent compiler patch inventory, or regenerated vendor tree is part of this contract. Fixes and their standalone core/plugin regressions belong here first; consumer changes should select the qualified commit and adapt to the public API.

### Extension behavior

- Failed full or partial scans release outstanding `PluginContext::load` waiters, including when all receivers have been cancelled. The failed build reports its panic with a stable module path; the same bundler can recover on a later scan.
- Explicit `DeferredResolutionError` diagnostics remain lazy behind dynamic imports, but a static path to an entry or a host loading edge fails the scan, including in ordinary bundling. Native parse/link error isolation is a separate opt-in protocol; staging a typed resolution error does not enable it.
- Constant inlining preserves observable initialization of the declaring module and retained re-export paths across chunks. Explicitly side-effect-free packages remain eligible for elimination. Literal export certificates never permit a read to initialize an unrelated pure export early.
- Full minification forwards global `keep_names` to compression and identifier mangling independently, for both boolean and detailed minify options. Explicit per-stage name-preservation overrides remain supported; the global default is unchanged.
- Runtime-module semantics, native-record routing, temporal export cells, and HMR certificates retain their existing experimental option guards. Runtime-module semantics require `experimental.resolve_new_url_to_asset: true` together with strict execution order, preserved modules, dev mode, or incremental builds. Tests asserting native runtime chunk topology or native failure isolation must configure those options explicitly; ordinary bundling does not opt into that protocol.
- Bundled development startup records omit binding cells only when no live importer, entry export, local reader, write, or conservative `eval`/constant restriction observes the original bindings. The importer observation index is generation-local and invalidated on relink or memory compaction. Wrapped records use their evaluator for readiness; unwrapped records retain temporal flags independently of cell ownership. Consumers of the cell protocol must preserve reload-only (`0`) and read-only (`[get]`) admission rules. The common runtime remains a store/executor; client-specific update admission and cell transactions remain the client's responsibility.
- Property-level pruning retains the exporters of analyzed objects. An incremental relink falls back when a dirty or newly appended module imports one of those exporters, even if the previous link never saw that importer reading the object.
- `Bundle::set_mangle_properties` selects a build-scoped property-mangling override. Multi-chunk builds collect a single global mapping, preserve globally occupied property names, and reject JavaScript chunks added or rewritten by `generateBundle`: their code did not participate in the final shared mapping. Make code changes in `renderChunk` instead. Unchanged code with asset or metadata edits remains supported. Property-mangled output is not reused from ordinary minification caches.
- Topology helpers use generic route and third-party package labels. Application-specific legacy chunk-name aliases belong in consumer configuration, not the compiler.

These are unstable Rust APIs. `ReplaceGlobalDefines` and its configuration are exported for AST-based define replacement. Merging dotted children into an object define preserves prototype setters separately from computed or shorthand `__proto__` own properties, including nested objects. A dotted `__proto__` child creates an own property without replacing the prototype setter. Duplicate own properties retain their evaluation order, side effects, and exceptions; dotted children merge into the last matching own property without changing existing parent-versus-child conflict precedence. The fork follows the current Oxc allocator and AST APIs; older consumers must update together rather than mix AST families.

Destructuring object-valued defines preserves initializer effects, exceptions, and accessor or inherited reads. Pruning removes only unused primitive values when every requested key is an own data property and the object is immediately destructured or the assignment result is discarded. Observed assignment results remain complete. Awaited objects remain complete because promise assimilation can invoke an inherited `then` getter before destructuring. Objects with accessors, methods, spreads, dynamic keys, or prototype setters remain intact.

#### Structured DefinePlugin objects

Adapters that synthesize object replacement strings from actual property-wise webpack/Rspack DefinePlugin objects can opt into a separate macro contract with `ExperimentalOptions::define_object_destructuring: Option<Vec<String>>` (JavaScript: `experimental.defineObjectDestructuring?: string[]`). List the exact synthesized prefixes, for example `["CONFIG", "CONFIG.sub", "import.meta.env"]`, and provide an explicit object-valued `define` for every listed key. Nested prefixes are independent entries; marking `CONFIG` does not implicitly mark `CONFIG.sub`. Derive this list from the adapter's known structured input, never just from an expression parsing as an object literal. Arbitrary user-supplied JavaScript expression strings must remain unmarked.

For marked objects, immediate static object destructuring renders only the requested own property values, even when requested keys are missing. Omitted values do not execute. This intentionally differs from expression substitution and was checked against webpack 5.112.0. Identifier, dotted, statically computed string/cooked-template, and `import.meta` references are supported. Selection also applies through one direct `await` and to discarded destructuring assignments; nested discarded assignments union their requested properties. Await still assimilates the selected object, so an inherited `then` getter sees that object's selected shape, not the omitted values.

Whole-object reads, rest or dynamic-key patterns, observed assignment results, calls (including `Promise.resolve(...).then(...)`), sequences, conditionals, logical expressions, and a second nested `await` retain complete objects. A nested pattern selects its top-level property but does not recursively filter that property's object value. These boundaries preserve the reference macro's context behavior rather than treating this option as a general unsafe-pruning flag.

Configuration rejects missing or duplicate metadata entries, non-object replacements, duplicate property names, accessors, methods, spreads, dynamic property keys, prototype setters, `typeof` entries, and wildcards. Computed or shorthand `__proto__` own data properties are allowed. Dotted-child merging retains its existing conflict precedence before validating/selecting the resulting object. Metadata and selection plans are build-local; there is no ambient or inherited override. Direct AST users can opt in with `ReplaceGlobalDefinesConfig::new_with_object_destructuring(defines, keys)`; `new(defines)` retains ordinary expression semantics.

Calls to `Object.freeze`, `Object.seal`, and `Object.preventExtensions` on fresh, side-effect-free literals remain eligible for tree shaking. Their global callee reads still constrain module execution order, including when chunk modules are sorted by ID.

The experimental bundler API also exposes `has_native_cells`, `static_import_edges`, `last_standby`, `link_decisions_generation`, and `last_link_decisions_unchanged` for native-record and incremental-update coordination. Topology configuration includes `GroupNamingPolicy::ByConsumers { prefix }` and the `standalone_runtime_chunk` and `consumer_shared_chunk_names` fields. `Bundle::set_mangle_properties` accepts the current Oxc `ManglePropertiesOptions`; its global mapping implementation uses public Oxc APIs and does not require downstream-only option fields or constructors.

### Native runtime fixture

The committed `crates/rolldown/tests/fixtures/runtime/native-runtime-base.js` is the canonical native runtime base assembled by `get_runtime_base_js(true)` in `crates/rolldown/src/module_loader/runtime_module_task.rs`. The raw `src/runtime/runtime-base.js` uses ordinary/upstream helpers; native runtime semantics replace two helper blocks and capture the global Promise and URL constructors before parsing the runtime. Platform-specific assembly adds the require helper separately, so this base fixture is not a complete platform runtime.

Consumers testing native helper semantics can copy the generated fixture unchanged from their pinned fork commit and record its Git blob identity. They must not reconstruct the Rust substitutions or maintain another runtime implementation. Native namespace creation passes the third `fixed` argument explicitly (`0` for runtime-discovered star exports, `1` for fixed exports); a subsequent one-argument `__exportAll(namespace)` completes a tracked star namespace. Omitting `fixed` during creation selects the legacy getter-object helper, not a native star namespace.

Regenerate and check the fixture from the fork root (prefer an existing `just` recipe if available):

```sh
cargo test --locked -p rolldown --lib regenerate_native_runtime_fixture -j 2 -- --ignored
cargo test --locked -p rolldown --lib runtime_fixture_tests -j 2
```

The generator calls the real Rust assembly function. The drift check compares every byte, verifies both substitutions still match exactly once, and independently checks that ordinary-mode assembly leaves the raw base unchanged. Regeneration is explicitly ignored during normal tests; the drift check never rewrites the fixture. The runtime fixture directory is excluded from `vite.config.ts` formatting so automatic quote or wrapping changes cannot alter the generated bytes.
