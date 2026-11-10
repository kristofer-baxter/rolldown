import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { test } from 'node:test';

const revision = '1'.repeat(40);
const directSource = `git+https://github.com/kristofer-baxter/oxc?rev=${revision}`;
const source = `${directSource}#${revision}`;
const checker = fileURLToPath(new URL('./check-oxc-sources.mjs', import.meta.url));

function fixture() {
  return {
    workspace_members: ['rolldown'],
    resolve: { nodes: [] },
    packages: [
      {
        id: 'rolldown',
        name: 'rolldown',
        dependencies: [{ name: 'oxc', source: directSource }],
      },
      ...['oxc', 'oxc_ast', 'oxc_napi', 'oxc_minify_napi', 'oxc_parser_napi', 'oxc_transform_napi'].map(
        (name) => ({ id: name, name, version: '0.158.0', source, dependencies: [] }),
      ),
      ...['oxc_index', 'oxc_resolver', 'oxc_resolver_napi', 'oxc_sourcemap'].map((name) => ({
        id: name,
        name,
        version: '9.0.0',
        source: name === 'oxc_sourcemap' ? null : 'registry+https://github.com/rust-lang/crates.io-index',
        dependencies: [],
      })),
    ],
  };
}

function check(metadata) {
  const result = spawnSync(process.execPath, [checker], {
    input: JSON.stringify(metadata),
    encoding: 'utf8',
  });
  assert.ifError(result.error);
  return result;
}

function replaceCompilerSource(metadata, replacement) {
  for (const pkg of metadata.packages) {
    if (pkg.source === source) pkg.source = replacement;
  }
}

test('one immutable compiler source permits independent Oxc projects', () => {
  const result = check(fixture());
  assert.equal(result.status, 0, result.stderr);
  assert.match(result.stdout, /6 unique coupled Oxc packages/);
});

for (const [name, mutate] of [
  ['registry duplicate', (m) => m.packages.push({ ...m.packages[2], source: 'registry+crates.io' })],
  ['registry-only family', (m) => replaceCompilerSource(m, 'registry+crates.io')],
  ['path-only family', (m) => replaceCompilerSource(m, null)],
  [
    'mixed NAPI revision',
    (m) => {
      m.packages[3].source = source.replaceAll(revision, '2'.repeat(40));
    },
  ],
  [
    'wrong fork',
    (m) => replaceCompilerSource(m, source.replace('kristofer-baxter', 'oxc-project')),
  ],
  [
    'mismatched resolved revision',
    (m) => replaceCompilerSource(m, `${directSource}#${'2'.repeat(40)}`),
  ],
  [
    'moving branch',
    (m) => replaceCompilerSource(m, `git+https://github.com/kristofer-baxter/oxc?branch=main#${revision}`),
  ],
  [
    'mixed compiler version',
    (m) => {
      m.packages[2].version = '0.157.0';
    },
  ],
  [
    'missing NAPI',
    (m) => {
      m.packages = m.packages.filter((p) => p.name !== 'oxc_transform_napi');
    },
  ],
  [
    'root-patch-only dependency',
    (m) => {
      m.packages[0].dependencies[0].source = 'registry+crates.io';
    },
  ],
  [
    'no workspace dependencies',
    (m) => {
      m.packages[0].dependencies = [];
    },
  ],
  [
    'no resolved graph',
    (m) => {
      m.resolve = null;
    },
  ],
]) {
  test(`rejects ${name}`, () => {
    const metadata = fixture();
    mutate(metadata);
    assert.notEqual(check(metadata).status, 0);
  });
}
