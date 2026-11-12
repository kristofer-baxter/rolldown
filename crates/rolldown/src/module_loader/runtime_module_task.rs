use std::sync::Arc;

use arcstr::ArcStr;
use oxc::ast_visit::VisitJsMut;
use oxc::span::SourceType;
use oxc_index::IndexVec;
use rolldown_common::{
  EcmaView, ExportsKind, FlatOptions, ImportAttributes, ModuleDefFormat, ModuleIdx, ModuleType,
  NormalModule, StableModuleId, StmtEvalFlags, side_effects::DeterminedSideEffects,
  side_effects::HookSideEffects,
};
use rolldown_common::{
  ModuleLoaderMsg, RUNTIME_MODULE_ID, RUNTIME_MODULE_KEY, ResolvedId, RuntimeModuleBrief,
  RuntimeModuleTaskResult,
};
use rolldown_ecmascript::{EcmaAst, EcmaCompiler};
use rolldown_error::BuildResult;
use rolldown_utils::concat_string;
use rolldown_utils::indexmap::FxIndexSet;
use rolldown_utils::stabilize_id::stabilize_id;
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
  ast_scanner::{AstScanner, ScanResult},
  utils::{retained_memory, tweak_ast_for_scanning::PreProcessor},
};

use rolldown_fs::FileSystem;

use super::resolve_utils::resolve_dependencies;
use super::task_context::TaskContext;

const RUNTIME_BASE_JS: &str = include_str!("../runtime/runtime-base.js");
const RUNTIME_TAIL_JS: &str = include_str!("../runtime/runtime-tail.js");
const RUNTIME_HEAD_NODE_JS: &str = include_str!("../runtime/runtime-head-node.js");
const RUNTIME_TAIL_NODE_JS: &str = include_str!("../runtime/runtime-tail-node.js");

const UPSTREAM_ESM_HELPERS: &str = "export var __esm = (fn, res, err) =>
  function () {
    if (err) throw err[0];
    try {
      return (fn && (res = (0, fn[__getOwnPropNames(fn)[0]])((fn = 0))), res);
    } catch (e) {
      throw ((err = [e]), e);
    }
  };
export var __esmMin = (fn, res, err) => () => {
  if (err) throw err[0];
  try {
    return (fn && (res = fn((fn = 0))), res);
  } catch (e) {
    throw ((err = [e]), e);
  }
};";

const RUNTIME_SEMANTICS_ESM_HELPERS: &str =
  "export var __esm = (fn, deps) => __esmMin(fn, deps, true);
export var __esmMin = (fn, deps, named) => {
  let state = { fn, deps, named, started: false, res: void 0, err: void 0, _esm: 1 };
  return () => (__esmProbe ? ((__esmProbe = 0), state) : __esmRun(state));
};";

const UPSTREAM_EXPORT_AND_INTEROP_HELPERS: &str = "export var __exportAll = (all, no_symbols) => {
  let target = {};
  for (var name in all) {
    __defProp(target, name, { get: all[name], enumerable: true });
  }
  if (!no_symbols) {
    __defProp(target, Symbol.toStringTag, { value: 'Module' });
  }
  return target;
};
export var __copyProps = (to, from, except, desc) => {
  if ((from && typeof from === 'object') || typeof from === 'function') {
    for (var keys = __getOwnPropNames(from), i = 0, n = keys.length, key; i < n; i++) {
      key = keys[i];
      if (!__hasOwnProp.call(to, key) && key !== except) {
        __defProp(to, key, {
          get: ((k) => from[k]).bind(null, key),
          enumerable: !(desc = __getOwnPropDesc(from, key)) || desc.enumerable,
        });
      }
    }
  }
  return to;
};
export var __reExport = (target, mod, secondTarget) => (
  __copyProps(target, mod, 'default'), secondTarget && __copyProps(secondTarget, mod, 'default')
);
export var __toESM = (mod, isNodeMode, target) => (
  (target = mod != null ? __create(__getProtoOf(mod)) : {}),
  __copyProps(
    // `__esModule` alone is not enough: the module must own a `default` (#10360).
    isNodeMode || !mod || !mod.__esModule || !__hasOwnProp.call(mod, 'default')
      ? __defProp(target, 'default', { value: mod, enumerable: true })
      : target,
    mod,
  )
);";

const RUNTIME_SEMANTICS_EXPORT_AND_INTEROP_HELPERS: &str = "export var __exportAll = (all, no_symbols, fixed) => {
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
};";

fn get_runtime_base_js(runtime_semantics: bool) -> String {
  if runtime_semantics {
    let base = RUNTIME_BASE_JS
      .replace(UPSTREAM_ESM_HELPERS, RUNTIME_SEMANTICS_ESM_HELPERS)
      .replace(UPSTREAM_EXPORT_AND_INTEROP_HELPERS, RUNTIME_SEMANTICS_EXPORT_AND_INTEROP_HELPERS);
    format!("var Promise = globalThis.Promise;\nvar __moduleURL = globalThis.URL;\n{base}")
  } else {
    // Star namespaces need runtime completion even without the native-record protocol.
    // Keep ordinary interop and initialization unchanged. See internal-docs/runtime-helpers/implementation.md.
    let (ordinary_exports, _) = UPSTREAM_EXPORT_AND_INTEROP_HELPERS
      .split_once("export var __toESM")
      .expect("ordinary interop helper boundary");
    let (namespace_exports, _) = RUNTIME_SEMANTICS_EXPORT_AND_INTEROP_HELPERS
      .split_once("export var __toESM")
      .expect("namespace interop helper boundary");
    RUNTIME_BASE_JS.replace(ordinary_exports, namespace_exports)
  }
}

fn get_runtime_js(runtime_semantics: bool) -> String {
  let base = get_runtime_base_js(runtime_semantics);
  concat_string!(base, RUNTIME_TAIL_JS)
}

fn get_runtime_js_with_node_platform(runtime_semantics: bool) -> String {
  let base = get_runtime_base_js(runtime_semantics);
  concat_string!(RUNTIME_HEAD_NODE_JS, base, RUNTIME_TAIL_NODE_JS)
}

pub struct RuntimeModuleTask<Fs: FileSystem + Clone + 'static> {
  module_idx: ModuleIdx,
  ctx: Arc<TaskContext<Fs>>,
  flat_options: FlatOptions,
}

impl<Fs: FileSystem + Clone + 'static> RuntimeModuleTask<Fs> {
  pub fn new(
    module_idx: ModuleIdx,
    shared_context: Arc<TaskContext<Fs>>,
    flat_options: FlatOptions,
  ) -> Self {
    Self { module_idx, ctx: shared_context, flat_options }
  }

  #[tracing::instrument(name = "RuntimeNormalModuleTaskResult::run", level = "debug", skip_all)]
  pub async fn run(self) {
    if let Err(errs) = self.run_inner().await {
      // If the main thread is dead, nothing we can do to handle these send failures.
      self.ctx.send(ModuleLoaderMsg::BuildErrors(errs.into_vec().into_boxed_slice()));
    }
  }

  async fn run_inner(&self) -> BuildResult<()> {
    let runtime_semantics =
      crate::utils::native_records::runtime_module_semantics_enabled(&self.ctx.options);
    let source: String = if self.ctx.options.is_esm_format_with_node_platform() {
      get_runtime_js_with_node_platform(runtime_semantics)
    } else {
      get_runtime_js(runtime_semantics)
    };

    let original_source = source.clone();

    // Call transform hook on runtime module
    let mut sourcemap_chain = vec![];
    let mut side_effects: Option<HookSideEffects> = None;
    let mut module_type = ModuleType::Js;
    let mut code_changed_by_plugins: Option<Vec<String>> = Some(vec![]);

    let source: ArcStr = self
      .ctx
      .plugin_driver
      .transform(
        &RUNTIME_MODULE_ID,
        &ImportAttributes::default(),
        self.module_idx,
        source,
        &mut sourcemap_chain,
        &mut side_effects,
        &mut module_type,
        None,
        &mut code_changed_by_plugins,
      )
      .await?
      .into();

    // Track which plugins modified the runtime module, so we can provide
    // helpful error messages if symbol validation fails.
    let mut modified_by_plugins: Vec<String> = vec![];
    if let Some(plugin_names) = code_changed_by_plugins {
      if !plugin_names.is_empty() && source.as_str() != original_source {
        modified_by_plugins = plugin_names;
      }
    }

    let (mut ast, scan_result) = self.make_ecma_ast(RUNTIME_MODULE_KEY, &source)?;

    let ScanResult {
      named_imports,
      named_exports,
      stmt_infos,
      default_export_ref,
      namespace_object_ref,
      imports,
      import_records: raw_import_records,
      ast_usage,
      mut symbol_ref_db,
      new_url_references,
      rolldown_file_url_references,
      dummy_record_set,
      ecma_view_meta,
      ..
    } = scan_result;
    if self.ctx.compact_module_arenas {
      retained_memory::compact_module_arenas(&mut ast, &mut symbol_ref_db);
    }

    let determined_side_effects = match side_effects {
      Some(HookSideEffects::False) => DeterminedSideEffects::UserDefined(false),
      Some(HookSideEffects::NoTreeshake) => DeterminedSideEffects::NoTreeshake,
      Some(HookSideEffects::True) | None => {
        let has_side_effects = stmt_infos
          .iter()
          .any(|stmt_info| stmt_info.eval_flags.contains(StmtEvalFlags::UnknownSideEffect));
        DeterminedSideEffects::Analyzed(has_side_effects)
      }
    };

    let mut resolved_id = ResolvedId::make_dummy();
    resolved_id.id = RUNTIME_MODULE_ID;
    let module_type = ModuleType::Js;

    let resolved_deps = resolve_dependencies(
      &resolved_id,
      &self.ctx.options,
      &self.ctx.resolver,
      &self.ctx.plugin_driver,
      &raw_import_records,
      source.clone(),
      &mut vec![],
      &self.ctx.deferred_resolution_targets,
    )
    .await?;

    let module = NormalModule {
      idx: self.module_idx,
      repr_name: "rolldown_runtime".to_string(),
      stable_id: StableModuleId::new(&RUNTIME_MODULE_ID, &self.ctx.options.cwd),
      id: RUNTIME_MODULE_ID,

      debug_id: stabilize_id(RUNTIME_MODULE_KEY, &self.ctx.options.cwd),
      exec_order: u32::MAX,
      module_type,

      ecma_view: EcmaView {
        source,

        import_records: IndexVec::default(),
        sourcemap_chain: Arc::new(sourcemap_chain),
        // The internal runtime module `importers/imported` should be skip.
        importers: FxIndexSet::default(),
        importers_idx: FxIndexSet::default(),
        dynamic_importers: FxIndexSet::default(),
        dynamic_importers_idx: FxIndexSet::default(),
        imported_ids: FxIndexSet::default(),
        dynamically_imported_ids: FxIndexSet::default(),
        side_effects: determined_side_effects,
        named_imports,
        named_exports,
        imports,
        default_export_ref,
        exports_kind: ExportsKind::Esm,
        namespace_object_ref,
        def_format: ModuleDefFormat::EsmMjs,
        ast_usage,
        self_referenced_class_decl_symbol_ids: FxHashSet::default(),
        hashbang_range: None,
        meta: ecma_view_meta,
        new_url_references,
        namespace_in_checks: FxHashMap::default(),
        rolldown_file_url_references,
        this_expr_replace_map: FxHashMap::default(),
        cjs_this_ref: None,
        hmr_info: scan_result.hmr_info,
        hmr_hot_ref: None,
        directive_range: vec![],
        dummy_record_set,
        constant_export_map: FxHashMap::default(),
        enum_member_value_map: FxHashMap::default(),
        json_module_none_self_reference_included_symbol: None,
        cjs_reexport_import_record_ids: Vec::new(),
        dynamic_import_called_names: FxHashMap::default(),
      },
      // TODO(hyf0/hmr): We might need to find a better way to handle this.
      originative_resolved_id: resolved_id,
    };

    let mut runtime = RuntimeModuleBrief::new(self.module_idx, &symbol_ref_db.ast_scopes);
    runtime.set_modified_by_plugins(modified_by_plugins);
    let result = ModuleLoaderMsg::RuntimeNormalModuleDone(Box::new(RuntimeModuleTaskResult {
      ast,
      module,
      stmt_infos,
      runtime,
      resolved_deps,
      raw_import_records,
      local_symbol_ref_db: symbol_ref_db,
      warnings: vec![],
    }));

    // If the main thread is dead, nothing we can do to handle these send failures.
    self.ctx.send(result);

    Ok(())
  }

  fn make_ecma_ast(&self, filename: &str, source: &ArcStr) -> BuildResult<(EcmaAst, ScanResult)> {
    let source_type = SourceType::default();

    let mut ast = EcmaCompiler::parse(filename, source.clone(), source_type)?;

    ast.program.with_mut(|fields| {
      let mut pre_processor = PreProcessor::new(fields.allocator, false, None);
      pre_processor.visit_program(fields.program);
    });

    let scoping = ast.make_scoping();
    let facade_path = RUNTIME_MODULE_ID;
    // Always respect annotations in the runtime module, regardless of user config.
    // The runtime is trusted internal code.
    let runtime_flat_options = self.flat_options - FlatOptions::IgnoreAnnotations;
    let scan_result = ast.program.with_mut(|fields| {
      let program = &*fields.program;
      let scanner = AstScanner::new(
        self.module_idx,
        scoping,
        "rolldown_runtime",
        ModuleDefFormat::EsmMjs,
        source,
        &facade_path,
        &program.comments,
        &self.ctx.options,
        fields.allocator,
        runtime_flat_options,
      );
      scanner.scan(program)
    })?;

    Ok((ast, scan_result))
  }
}

#[cfg(test)]
mod runtime_fixture_tests {
  use std::{fs, path::Path};

  const FIXTURE_PATH: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/runtime/native-runtime-base.js");

  #[test]
  fn native_runtime_fixture_matches_assembly() {
    for block in [super::UPSTREAM_ESM_HELPERS, super::UPSTREAM_EXPORT_AND_INTEROP_HELPERS] {
      assert_eq!(
        super::RUNTIME_BASE_JS.matches(block).count(),
        1,
        "the native runtime substitution must match exactly once"
      );
    }
    let fixture = fs::read_to_string(FIXTURE_PATH)
      .expect("regenerate the committed fixture with regenerate_native_runtime_fixture");
    assert_eq!(fixture, super::get_runtime_base_js(true), "native runtime fixture drifted");
  }

  #[test]
  fn ordinary_runtime_keeps_initialization_and_interop() {
    let ordinary = super::get_runtime_base_js(false);
    assert!(ordinary.contains(super::UPSTREAM_ESM_HELPERS));
    let (_, interop) =
      super::UPSTREAM_EXPORT_AND_INTEROP_HELPERS.split_once("export var __toESM").unwrap();
    assert!(ordinary.contains(&format!("export var __toESM{interop}")));
    assert!(!ordinary.contains("var Promise = globalThis.Promise;"));
    let (namespace_exports, _) =
      super::RUNTIME_SEMANTICS_EXPORT_AND_INTEROP_HELPERS.split_once("export var __toESM").unwrap();
    assert!(ordinary.contains(namespace_exports));
    let (ordinary_exports, _) =
      super::UPSTREAM_EXPORT_AND_INTEROP_HELPERS.split_once("export var __toESM").unwrap();
    assert_eq!(super::RUNTIME_BASE_JS.matches(ordinary_exports).count(), 1);
    assert_eq!(ordinary.matches(namespace_exports).count(), 1);
  }

  #[test]
  #[ignore = "explicitly regenerates the committed native runtime fixture"]
  fn regenerate_native_runtime_fixture() {
    let path = Path::new(FIXTURE_PATH);
    fs::create_dir_all(path.parent().expect("runtime fixture directory"))
      .expect("create runtime fixture directory");
    fs::write(path, super::get_runtime_base_js(true)).expect("write native runtime fixture");
  }
}
