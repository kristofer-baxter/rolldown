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

const inheritedMarker = new Error('inherited-with-getter');
let inheritedRejection;
let explicitEmptyWith;
Object.defineProperty(Object.prototype, 'with', {
  configurable: true,
  get() {
    throw inheritedMarker;
  },
});
try {
  inheritedRejection = import('./target.js', {});
  explicitEmptyWith = import('./target.js', { with: {} });
} finally {
  delete Object.prototype.with;
}
inheritedRejection.then(
  () => assert.fail('expected inherited getter rejection'),
  (err) => assert.strictEqual(err, inheritedMarker),
);
explicitEmptyWith.then((target) => {
  assert.strictEqual(target.value, 42);
});
