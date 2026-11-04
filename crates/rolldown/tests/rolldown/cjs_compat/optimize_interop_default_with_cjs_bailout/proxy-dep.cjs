let ownKeysCalls = 0;
const target = {
  a: 10,
  mutate() {
    target.later = 20;
  },
};
module.exports = new Proxy(target, {
  ownKeys(t) {
    ownKeysCalls++;
    if (ownKeysCalls > 1) {
      throw new Error('ownKeys called more than once');
    }
    return Reflect.ownKeys(t);
  },
});
