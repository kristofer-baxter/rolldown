import mod from './commonjs.js';
import value, * as ns from './dep.cjs';
import * as proxyNs from './proxy-dep.cjs';
import assert from 'node:assert/strict';

assert.deepEqual(mod.slice(1), [2, 3], 'should import JSON file as expected');

assert.equal(mod.foo, undefined);
assert.equal(value.foo, 2);
assert.equal(ns.default.foo, 2);
assert.equal(ns.foo, 1);
assert.equal(proxyNs.a, 10);
assert.equal(proxyNs.later, undefined);
proxyNs.mutate();
assert.equal(proxyNs.later, undefined);
assert.equal(proxyNs.default.later, 20);
