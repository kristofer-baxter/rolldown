import assert from 'node:assert';

import('./target.js', {}).then((target) => {
  assert.strictEqual(target.value, 42);
});

const marker = new Error('with-getter');
import('./target.js', {
  get with() {
    throw marker;
  },
}).then(
  () => assert.fail('expected getter rejection'),
  (err) => assert.strictEqual(err, marker),
);
