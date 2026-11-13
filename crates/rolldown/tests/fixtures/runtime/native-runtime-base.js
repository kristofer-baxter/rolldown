var Promise = globalThis.Promise;
var __moduleURL = globalThis.URL;
export var __create = Object.create;
export var __defProp = Object.defineProperty;
export var __name = (target, value) => __defProp(target, 'name', { value, configurable: true });
export var __getOwnPropDesc = Object.getOwnPropertyDescriptor;
export var __getOwnPropNames = Object.getOwnPropertyNames;
export var __getProtoOf = Object.getPrototypeOf;
export var __hasOwnProp = Object.prototype.hasOwnProperty;
export var __esm = (fn, deps) => __esmMin(fn, deps, true);
export var __esmMin = (fn, deps, named) => {
  let state = { fn, deps, named, started: false, res: void 0, err: void 0, _esm: 1 };
  return () => (__esmProbe ? ((__esmProbe = 0), state) : __esmRun(state));
};
export var __commonJS = (cb, mod, onFailure) =>
  function () {
    if (!mod) {
      mod = { exports: {} };
      try {
        (0, cb[__getOwnPropNames(cb)[0]])(mod.exports, mod, mod.exports);
        cb = null;
      } catch (error) {
        mod = void 0;
        onFailure?.(error);
        throw error;
      }
    }
    return mod.exports;
  };
export var __commonJSMin = (cb, mod, onFailure) => () => {
  if (!mod) {
    mod = { exports: {} };
    try {
      cb(mod.exports, mod, mod.exports);
      cb = null;
    } catch (error) {
      mod = void 0;
      onFailure?.(error);
      throw error;
    }
  }
  return mod.exports;
};
// Default runtime uses `{}` for compact output (`crates/rolldown/tests/test262_failures.json`);
// null-prototype namespace semantics are provided by `runtime-extra-dev-common.js` when enabled.
export var __exportAll = (all, no_symbols, fixed) => {
  let star = __starNamespaces.get(all);
  if (star) {
    Object.preventExtensions(star[1]);
    return all;
  }
  if (fixed === void 0) {
    let target = {};
    for (var name in all) {
      __defProp(target, name, { get: all[name], enumerable: true });
    }
    if (!no_symbols) {
      __defProp(target, Symbol.toStringTag, { value: 'Module' });
    }
    return target;
  }
  let target = __create(null);
  let names = __getOwnPropNames(all).sort();
  for (let name of names) {
    __defProp(target, name, { value: undefined, writable: true, enumerable: true });
  }
  if (!no_symbols) __defProp(target, Symbol.toStringTag, { value: 'Module' });
  if (fixed) Object.preventExtensions(target);
  let namespace = new Proxy(target, {
    get: (target, key) => __hasOwnProp.call(all, key) ? all[key]() : target[key],
    ownKeys: (target) => {
      let keys = fixed ? names : __getOwnPropNames(target).sort();
      return no_symbols ? keys : [...keys, Symbol.toStringTag];
    },
    getOwnPropertyDescriptor(target, key) {
      let descriptor = __getOwnPropDesc(target, key);
      if (descriptor && __hasOwnProp.call(all, key)) descriptor.value = all[key]();
      return descriptor;
    },
    set: () => false,
    defineProperty(target, key, descriptor) {
      if (!__hasOwnProp.call(all, key)) {
        return __hasOwnProp.call(target, key) && Reflect.defineProperty(target, key, descriptor);
      }
      let value = all[key]();
      return !descriptor.configurable && descriptor.enumerable !== false &&
        descriptor.writable !== false && !('get' in descriptor) && !('set' in descriptor) &&
        (!('value' in descriptor) || Object.is(descriptor.value, value));
    },
    preventExtensions: (target) => !Object.isExtensible(target),
    setPrototypeOf: (_, prototype) => prototype === null,
  });
  if (!fixed) __starNamespaces.set(namespace, [all, target]);
  return namespace;
};
export var __copyProps = (to, from, except, desc, own) => {
  if ((from && typeof from === 'object') || typeof from === 'function') {
    let star = __starNamespaces.get(to);
    var keys = __getOwnPropNames(from), i = 0, n = keys.length, key;
    if (own) for (; i < n; i++) own.add(keys[i]);
    for (i = 0; i < n; i++) {
      key = keys[i];
      if (key === except) continue;
      if (star) {
        if (!__hasOwnProp.call(star[0], key) && Object.isExtensible(star[1])) {
          __defProp(star[0], key, { value: ((k) => from[k]).bind(null, key), enumerable: true });
          __defProp(star[1], key, { value: undefined, writable: true, enumerable: true });
        }
      } else if (!__hasOwnProp.call(to, key)) {
        __defProp(to, key, {
          get: ((k) => from[k]).bind(null, key),
          enumerable: __starNamespaces.has(from) || !(desc = __getOwnPropDesc(from, key)) || desc.enumerable,
        });
      }
    }
  }
  return to;
};
export var __reExport = (target, mod, secondTarget) => (
  __copyProps(target, mod, 'default'), secondTarget && __copyProps(secondTarget, mod, 'default')
);
export var __toESM = (mod, isNodeMode, key, target) => {
  let modes = key && __dynamicNamespaces.get(key);
  let mode = isNodeMode ? 1 : 0;
  if (modes && modes.has(mode)) return modes.get(mode);
  let ownWhenCreated;
  if (mod != null && (typeof mod == 'object' || typeof mod == 'function')) {
    let proto = __getProtoOf(mod);
    let hide = !__hasOwnProp.call(mod, 'then') && !(proto != null && 'then' in proto);
    ownWhenCreated = new Set();
    target = __create(
      new Proxy(__create(proto), {
        get: (_, name) =>
          hide && name === 'then'
            ? undefined
            : __hasOwnProp.call(mod, name) && !ownWhenCreated.has(name)
              ? Reflect.get(proto || {}, name, mod)
              : Reflect.get(mod, name, mod),
        has: (_, name) =>
          !(hide && name === 'then')
          && (__hasOwnProp.call(mod, name) && !ownWhenCreated.has(name)
            ? proto != null && Reflect.has(proto, name)
            : Reflect.has(mod, name)),
        ownKeys: () => {
          let names = [];
          for (let name in __create(proto)) names.push(name);
          return names;
        },
        getOwnPropertyDescriptor: (_, name) => {
          for (let from = proto, desc; from != null; from = __getProtoOf(from))
            if ((desc = __getOwnPropDesc(from, name))) return { ...desc, configurable: true };
        },
        defineProperty: () => false,
        preventExtensions: () => false,
        setPrototypeOf: () => false,
      }),
    );
  } else target = mod != null ? __create(__getProtoOf(mod)) : {};
  __copyProps(
    isNodeMode || !mod || !mod.__esModule || !__hasOwnProp.call(mod, 'default')
      ? __defProp(target, 'default', { value: mod, enumerable: true })
      : target,
    mod,
    void 0,
    void 0,
    ownWhenCreated,
  );
  if (key) {
    if (!modes) __dynamicNamespaces.set(key, modes = new Map());
    modes.set(mode, target);
  }
  return target;
};
export var __toCommonJS = (mod) =>
  __hasOwnProp.call(mod, 'module.exports')
    ? mod['module.exports']
    : __copyProps(__defProp({}, '__esModule', { value: true }), mod);
// `__share*`: the registry behind `codeSplitting.experimentalInlineCommonChunks`. A chunk factory
// is registered under its record id by every file that carries it; the first registration wins,
// `__share_require` runs the winning factory once and hands every reader the same exports object.
// See internal-docs/inline-common-chunks/implementation.md.
export var __share_factories = /* @__PURE__ */ __create(null);
export var __share_records = /* @__PURE__ */ __create(null);
export var __share = (id, factory) => {
  if (!(id in __share_factories)) __share_factories[id] = factory;
};
export var __share_require = (id) => {
  var record = __share_records[id];
  if (record) {
    // A failed factory rethrows the same value on every read, including `undefined`; a factory
    // that is still running hands back its partially filled exports object.
    if (record.failed) throw record.error;
    return record.exports;
  }
  var factory = __share_factories[id];
  if (!factory) throw new Error('Shared chunk "' + id + '" was not registered before it was required.');
  record = __share_records[id] = { exports: {}, failed: false, error: void 0 };
  try {
    factory(record.exports);
  } catch (error) {
    record.failed = true;
    record.error = error;
    throw error;
  }
  return record.exports;
};
export var __share_export = (target, all) => {
  for (var name in all) __defProp(target, name, { get: all[name], enumerable: true });
};
export var __toBinaryNode = (base64) => new Uint8Array(Buffer.from(base64, 'base64'));
export var __toBinary = /* @__PURE__ */ (() => {
  var table = new Uint8Array(128);
  for (var i = 0; i < 64; i++) {
    table[i < 26 ? i + 65 : i < 52 ? i + 71 : i < 62 ? i - 4 : i * 4 - 205] = i;
  }
  return (base64) => {
    var n = base64.length,
      bytes = new Uint8Array((((n - (base64[n - 1] == '=') - (base64[n - 2] == '=')) * 3) / 4) | 0);
    for (var i = 0, j = 0; i < n; ) {
      var c0 = table[base64.charCodeAt(i++)],
        c1 = table[base64.charCodeAt(i++)];
      var c2 = table[base64.charCodeAt(i++)],
        c3 = table[base64.charCodeAt(i++)];
      bytes[j++] = (c0 << 2) | (c1 >> 4);
      bytes[j++] = (c1 << 4) | (c2 >> 2);
      bytes[j++] = (c2 << 6) | c3;
    }
    return bytes;
  };
})();
export var __moduleSyntaxError = SyntaxError;
export var __moduleTypeError = TypeError;
export var __moduleResolutionErrors = /* @__PURE__ */ new WeakSet();
export var __moduleLinkErrors = /* @__PURE__ */ new WeakSet();
export var __moduleLinkError = message => {
  var error = new __moduleSyntaxError(message);
  __moduleLinkErrors.add(error);
  return error;
};
export var __rethrowModuleLinkError = error => {
  if (__moduleLinkErrors.has(error)) throw new __moduleSyntaxError(error.message);
  throw error;
};
export var __importNamespace = async (initialize, namespace) => {
  await 0;
  await initialize();
  return namespace;
};
export var __computedImport = /* @__PURE__ */ ((__moduleURL = globalThis.URL) => (specifier, options, candidates, base) => {
  try {
    // ToString follows both authored arguments, but precedes attribute processing.
    specifier = `${specifier}`;
  } catch (error) {
    return (async () => { throw error; })();
  }
  try {
    var attributes = { __proto__: null };
    for (var [key, value] of __readImportAttributes(options)) attributes[key] = value;
    options = { with: attributes };
    var absolute, origin;
    for (var [request, load, self] of candidates) {
      if (specifier === request) return load(options);
      if (self && specifier === base()) return load(options);
      if (!request.startsWith("./") && !request.startsWith("../")) continue;
      if (origin === void 0) origin = base();
      if (absolute === void 0) absolute = new __moduleURL(specifier, origin).href;
      if (absolute === new __moduleURL(request, origin).href) return load(options);
    }
    throw new __moduleTypeError("Unknown captured dynamic import: " + specifier + " (declare runtime-computed targets as native:dynamic-import-contexts candidates)");
  } catch (error) {
    return (async () => { throw error; })();
  }
})();
var __readImportAttributes = options => {
    var entries = [];
    if (options !== void 0) {
      if (options === null || typeof options !== "object" && typeof options !== "function")
        throw new __moduleTypeError("Import options must be an object");
      var attributes = options.with;
      if (attributes !== void 0) {
        if (attributes === null || typeof attributes !== "object" && typeof attributes !== "function")
          throw new __moduleTypeError("Import attributes must be an object");
        for (var key of __getOwnPropNames(attributes)) {
          if (!__getOwnPropDesc(attributes, key)?.enumerable) continue;
          entries[entries.length] = [key, attributes[key]];
        }
        for (var entry of entries)
          if (typeof entry[1] !== "string")
            throw new __moduleTypeError("Import attribute values must be strings");
      }
    }
    return entries;
};
export var __withImportOptions = (options, load, type = false) => {
  try {
    var entries = __readImportAttributes(options);
    for (var [key, value] of entries)
      if (key !== "type" || value !== (type === true ? "json" : type))
        throw new __moduleTypeError("Unsupported import attribute: " + key);
    return load();
  } catch (error) {
    return (async () => { throw error; })();
  }
};
export var __rejectModuleResolution = async (message, options) => {
  if (options !== void 0) {
    if (options === null || typeof options !== "object" && typeof options !== "function")
      throw new __moduleTypeError("Import options must be an object");
    var attributes = options.with;
    if (attributes !== void 0) {
      if (attributes === null || typeof attributes !== "object" && typeof attributes !== "function")
        throw new __moduleTypeError("Import attributes must be an object");
      for (var key of __getOwnPropNames(attributes))
        if (__getOwnPropDesc(attributes, key)?.enumerable && typeof attributes[key] !== "string")
          throw new __moduleTypeError("Import attribute values must be strings");
    }
  }
  var error = new __moduleTypeError(message);
  __moduleResolutionErrors.add(error);
  throw error;
};
export var __checkTDZ = (initialized, name, readonly) => {
  if (!initialized) throw new ReferenceError("Cannot access '" + name + "' before initialization");
  if (readonly) throw new TypeError("Assignment to constant variable.");
};
export var __startAsync = (pending, result) => {
  if (result) {
    // A later synchronous sibling may throw before the parent reaches the join.
    result.catch(() => {});
    pending.push(result);
  }
};
export var __finishAsync = /* @__PURE__ */ ((__Promise = Promise) => (pending, execute) =>
  pending.length ? __Promise.all(pending).then(execute) : execute())();
var __asyncModules = [];
export var __esmAsync = (fn, state = {}) => () => {
  let root = state;
  while (root.parent) root = root.parent;
  if (root.active) {
    // An active backedge joins the intervening modules to one evaluation cycle.
    for (let index = __asyncModules.indexOf(root) + 1; index < __asyncModules.length; index++) {
      __asyncModules[index].parent = root;
    }
  }
  if (state.started) {
    if (root.error) throw root.error[0];
    // Initial DFS edges use local completion; later imports await the cycle root.
    return root.active ? state.result : root.result;
  }
  state.started = state.active = true;
  __asyncModules.push(state);
  try {
    const execute = typeof fn === "function" ? fn : fn[__getOwnPropNames(fn)[0]];
    fn = 0;
    return state.result = execute();
  } catch (error) {
    state.error = [error];
    throw error;
  } finally {
    state.active = false;
    __asyncModules.pop();
  }
};
var __esmProbe = 0;
var __esmRun = root => {
  if (root.err) throw root.err[0];
  if (!root.started) {
    let stack = [], pending = [], evaluation = {}, order = 0;
    let start = node => {
      node.started = true;
      node.evaluation = evaluation;
      node.order = node.ancestor = order++;
      pending.push(node);
      stack.push({ node, idx: 0 });
    };
    let fail = error => {
      let err = [error];
      for (let node of pending) {
        node.err = err;
        node.evaluation = 0;
      }
      pending.length = 0;
      throw error;
    };
    start(root);
    while (stack.length) {
      let top = stack[stack.length - 1];
      let node = top.node;
      let deps = node.deps;
      if (deps && top.idx < deps.length) {
        let depFn = deps[top.idx++];
        let dep;
        __esmProbe = 1;
        try {
          dep = depFn();
        } catch (e) {
          __esmProbe = 0;
          fail(e);
        }
        __esmProbe = 0;
        if (!dep || !dep._esm) continue;
        if (dep.err) fail(dep.err[0]);
        if (!dep.started) {
          start(dep);
        } else if (dep.evaluation === evaluation) {
          node.ancestor = Math.min(node.ancestor, dep.order);
        }
        continue;
      }
      stack.pop();
      node.deps = 0;
      let fn = node.fn;
      if (fn && !node.err) {
        node.fn = 0;
        try {
          node.res = node.named ? (0, fn[__getOwnPropNames(fn)[0]])() : fn();
        } catch (e) {
          fail(e);
        }
      }
      if (node.ancestor === node.order) {
        let completed;
        do {
          completed = pending.pop();
          completed.evaluation = 0;
        } while (completed !== node);
      }
      if (stack.length) {
        let parent = stack[stack.length - 1].node;
        parent.ancestor = Math.min(parent.ancestor, node.ancestor);
      }
    }
  }
  return root.res;
};
var __starNamespaces = /* @__PURE__ */ new WeakMap();
var __dynamicNamespaces = /* @__PURE__ */ new WeakMap();
var __jsonNamespaces = /* @__PURE__ */ new WeakMap();
export var __jsonExports = (all, no_symbols) => {
  let init = __jsonNamespaces.get(all);
  if (init) return (init(), all);
  let target = __create(null), getMod = all, ready = 0, names;
  init = () => {
    if (!ready) {
      let mod = getMod();
      ready = 1;
      names = ['default'];
      if (mod && typeof mod === 'object') {
        for (let key of __getOwnPropNames(mod)) {
          if (key && key !== 'default') names.push(key);
        }
      }
      names.sort();
      for (let name of names) {
        __defProp(target, name, {
          value: name === 'default' ? mod : mod[name],
          writable: true,
          enumerable: true,
        });
      }
      if (!no_symbols) __defProp(target, Symbol.toStringTag, { value: 'Module' });
      Object.preventExtensions(target);
    }
    return target;
  };
  let namespace = new Proxy(target, {
    get: (_, key) => init()[key],
    has: (_, key) => key in init(),
    ownKeys: () => (init(), no_symbols ? names : [...names, Symbol.toStringTag]),
    getOwnPropertyDescriptor: (_, key) => __getOwnPropDesc(init(), key),
    deleteProperty: (_, key) => Reflect.deleteProperty(init(), key),
    isExtensible: () => (init(), false),
    set: () => false,
    defineProperty(_, key, descriptor) {
      init();
      if (typeof key === 'symbol' || !__hasOwnProp.call(target, key)) {
        return __hasOwnProp.call(target, key) && Reflect.defineProperty(target, key, descriptor);
      }
      return !descriptor.configurable && descriptor.enumerable !== false &&
        descriptor.writable !== false && !('get' in descriptor) && !('set' in descriptor) &&
        (!('value' in descriptor) || Object.is(descriptor.value, target[key]));
    },
    preventExtensions: () => (init(), true),
    setPrototypeOf: (_, prototype) => prototype === null,
  });
  __jsonNamespaces.set(namespace, init);
  return namespace;
};
