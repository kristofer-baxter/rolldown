#!/usr/bin/env node

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';

// These projects have independent release/source identities, not the compiler's AST identity.
const independent = new Set(['oxc_index', 'oxc_resolver', 'oxc_resolver_napi', 'oxc_sourcemap']);
const coupled = (name) => (name === 'oxc' || name.startsWith('oxc_')) && !independent.has(name);
const metadata = JSON.parse(readFileSync(0, 'utf8'));
assert(metadata.resolve, 'provide full cargo metadata, not --no-deps output');
const family = metadata.packages.filter((pkg) => coupled(pkg.name));
const names = new Set(family.map((pkg) => pkg.name));
for (const name of ['oxc', 'oxc_napi', 'oxc_minify_napi', 'oxc_parser_napi', 'oxc_transform_napi']) {
  assert(names.has(name), `standalone workspace metadata is missing ${name}`);
}
assert.equal(names.size, family.length, 'duplicate coupled Oxc package identities');
const sources = new Set(family.map((pkg) => pkg.source));
assert.equal(sources.size, 1, 'coupled Oxc packages must share one Git source');
const [source] = sources;
const pin = /^git\+https:\/\/github\.com\/kristofer-baxter\/oxc\?rev=([0-9a-f]{40})#\1$/.exec(
  source,
);
assert(pin, 'coupled Oxc packages must use the canonical fork and matching immutable revision');
assert.equal(new Set(family.map((pkg) => pkg.version)).size, 1, 'mixed compiler versions');

const directSource = source.slice(0, source.lastIndexOf('#'));
const members = new Set(metadata.workspace_members);
let directDependencies = 0;
for (const pkg of metadata.packages.filter((pkg) => members.has(pkg.id))) {
  for (const dependency of pkg.dependencies.filter((dependency) => coupled(dependency.name))) {
    assert.equal(
      dependency.source,
      directSource,
      `${pkg.name}/${dependency.name} must pin Git directly; root patches are not inherited downstream`,
    );
    directDependencies++;
  }
}
assert(directDependencies > 0, 'metadata contains no workspace compiler dependencies');
console.log(
  `PASS: ${family.length} unique coupled Oxc packages, including NAPI, at ${pin[1]}; no registry/path duplicates`,
);
