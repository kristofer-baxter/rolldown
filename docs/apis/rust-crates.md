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

The current compiler/plugin/common/fs/error/resolver package versions are all `1.2.16`. The compiler uses the Oxc `0.158.0` AST, allocator, semantic, transformer, and minifier family. Consumers that also use Oxc or React compiler integrations must align those dependencies to one compatible source and revision; registry and Git packages with the same version are still different Rust type identities. Verify the complete graph with `cargo metadata --locked` and duplicate dependencies with `cargo tree -d`. Workspace-level patches in this repository are not inherited by a consumer's root workspace, including the source-map vendor patch. Pin the maintained source-map implementation too if using its fork extensions.

The companion `rolldown_ecmascript`, `rolldown_sourcemap`, and `rolldown_utils` packages are also `1.2.16`. The separate Oxc resolver family is `11.24.3`, and `oxc_sourcemap` is `9.0.0` with this repository's vendor patch; these are not part of the `0.158.0` AST family.

No source synchronization script, independent compiler patch inventory, or regenerated vendor tree is part of this contract. Fixes and their standalone core/plugin regressions belong here first; consumer changes should select the qualified commit and adapt to the public API.

### Extension behavior

- Failed full or partial scans release outstanding `PluginContext::load` waiters, including when all receivers have been cancelled. The failed build reports its panic with a stable module path; the same bundler can recover on a later scan.
- Constant inlining preserves observable initialization of the declaring module and retained re-export paths across chunks. Explicitly side-effect-free packages remain eligible for elimination. Literal export certificates never permit a read to initialize an unrelated pure export early.
- Runtime-module semantics, native-record routing, temporal export cells, and HMR certificates retain their existing experimental option guards. Ordinary bundling does not opt into native-record behavior.
- `Bundle::set_mangle_properties` selects a build-scoped property-mangling override. Multi-chunk builds collect a single global mapping, preserve globally occupied property names, and reject late emitted JavaScript chunks because those chunks were absent from the global analysis. Property-mangled output is not reused from ordinary minification caches.
- Topology helpers use generic route and third-party package labels. Application-specific legacy chunk-name aliases belong in consumer configuration, not the compiler.

These are unstable Rust APIs. `ReplaceGlobalDefines` and its configuration are exported for AST-based define replacement, including shorthand `__proto__` own-property preservation. The fork follows the current Oxc allocator and AST APIs; older consumers must update together rather than mix AST families.

The experimental bundler API also exposes `has_native_cells`, `static_import_edges`, `last_standby`, `link_decisions_generation`, and `last_link_decisions_unchanged` for native-record and incremental-update coordination. Topology configuration includes `GroupNamingPolicy::ByConsumers { prefix }` and the `standalone_runtime_chunk` and `consumer_shared_chunk_names` fields. `Bundle::set_mangle_properties` accepts the current Oxc `ManglePropertiesOptions`; its global mapping implementation uses public Oxc APIs and does not require downstream-only option fields or constructors.

### Native runtime fixture

The committed `crates/rolldown/tests/fixtures/runtime/native-runtime-base.js` is the canonical native runtime base assembled by `get_runtime_base_js(true)` in `crates/rolldown/src/module_loader/runtime_module_task.rs`. The raw `src/runtime/runtime-base.js` uses ordinary/upstream helpers; native runtime semantics replace two helper blocks and capture the global Promise and URL constructors before parsing the runtime. Platform-specific assembly adds the require helper separately, so this base fixture is not a complete platform runtime.

Consumers testing native helper semantics can copy the generated fixture unchanged from their pinned fork commit and record its Git blob identity. They must not reconstruct the Rust substitutions or maintain another runtime implementation. Native namespace creation passes the third `fixed` argument explicitly (`0` for runtime-discovered star exports, `1` for fixed exports); a subsequent one-argument `__exportAll(namespace)` completes a tracked star namespace. Omitting `fixed` during creation selects the legacy getter-object helper, not a native star namespace.

Regenerate and check the fixture from the fork root (prefer an existing `just` recipe if available):

```sh
cargo test --locked -p rolldown --lib regenerate_native_runtime_fixture -j 2 -- --ignored
cargo test --locked -p rolldown --lib runtime_fixture_tests -j 2
```

The generator calls the real Rust assembly function. The drift check compares every byte, verifies both substitutions still match exactly once, and independently checks that ordinary-mode assembly leaves the raw base unchanged. Regeneration is explicitly ignored during normal tests; the drift check never rewrites the fixture.
