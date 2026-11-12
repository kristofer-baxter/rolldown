use oxc::allocator::GetAllocator;
use oxc::ast::builder::AstBuilder;
use oxc::{
  ast::ast,
  span::{GetSpan, SPAN},
};
use rolldown_common::{IndexModules, ModuleIdx, NormalModule, SymbolRefDb, lazy_proxy_target};
use rolldown_ecmascript::CJS_MODULE_REF;
#[cfg(feature = "experimental")]
use rolldown_ecmascript::CJS_ROLLDOWN_MODULE_REF;
use rolldown_ecmascript_utils::ExpressionExt;
use rustc_hash::FxHashSet;

#[cfg(feature = "experimental")]
use crate::hmr::hmr_ast_finalizer::HmrAstFinalizer;
use crate::module_finalizers::ScopeHoistingFinalizer;
use crate::types::linking_metadata::LinkingMetadataVec;
use crate::utils::native_record_ast::{self, Cell};

/// Dev mode retains declarations for future updates, but an unused binding import must still
/// honor its dependency's side-effects contract. Run after linking so invalid imports are checked.
pub fn unused_side_effect_free_import(
  module: &NormalModule,
  record_idx: rolldown_common::ImportRecordIdx,
  modules: &rolldown_common::IndexModules,
  scoping: &oxc::semantic::Scoping,
) -> bool {
  unused_pure_import(module, record_idx, |idx| pure_module(modules, idx), scoping)
}

/// Whether `modules` gives module `idx` no side effects: a normal module that its table
/// classifies so.
pub fn pure_module(
  modules: &rolldown_common::IndexModules,
  idx: rolldown_common::ModuleIdx,
) -> bool {
  modules[idx].as_normal().is_some_and(|module| !module.side_effects.has_side_effects())
}

/// [`unused_side_effect_free_import`], where `pure` says whether a module has no side effects
/// once linked, which it asks last. A table that the link stage hasn't classified, as the
/// scan's, gives a module whose own statements have none, `DeterminedSideEffects::Analyzed(false)`,
/// none even when its imports reach side effects, which linking gives it (`linked_side_effects`),
/// and which keep an unused import of it (#697).
pub fn unused_pure_import(
  module: &NormalModule,
  record_idx: rolldown_common::ImportRecordIdx,
  pure: impl Fn(rolldown_common::ModuleIdx) -> bool,
  scoping: &oxc::semantic::Scoping,
) -> bool {
  use rolldown_common::{ImportKind, ImportRecordMeta};

  let record = &module.import_records[record_idx];
  if module.meta.has_eval()
    || record.kind != ImportKind::Import
    || record.meta.intersects(ImportRecordMeta::IsReExportOnly | ImportRecordMeta::IsExportStar)
  {
    return false;
  }
  let mut has_binding = false;
  for (binding, import) in &module.named_imports {
    if import.record_idx != record_idx {
      continue;
    }
    has_binding = true;
    if scoping.get_resolved_references(binding.symbol).next().is_some()
      || module.named_exports.values().any(|export| export.referenced == *binding)
    {
      return false;
    }
  }
  has_binding && record.resolved_module.is_some_and(pure)
}

/// The scope tree that decides whether development output drops one of `module`'s imports
/// ([`unused_side_effect_free_import`]). A rescan leaves the module's in the snapshot's
/// `symbols`, and a fast relink moves the last relinked scan's into the cached link output
/// (`cached`), leaving the snapshot's empty. The snapshot's comes first, since a rollback to a
/// superseded build's base rescans a module without relinking it, and it is the only one for a
/// module that the update `rescanned`. Either counts only where it holds every binding that the
/// module imports.
#[cfg(feature = "experimental")]
pub fn import_scoping<'a>(
  symbols: &'a rolldown_common::SymbolRefDb,
  cached: Option<&'a crate::types::scan_stage_cache::CachedNativeRelinkState>,
  module: &NormalModule,
  rescanned: bool,
) -> Option<&'a oxc::semantic::Scoping> {
  let covers = |db: &&rolldown_common::SymbolRefDbForModule| {
    let symbols = db.ast_scopes.scoping().symbols_len();
    module.named_imports.keys().all(|binding| binding.symbol.index() < symbols)
  };
  let scanned = symbols.inner().get(module.idx).and_then(Option::as_ref).filter(covers);
  let linked = || {
    let cached = cached?;
    let linked = cached.link_output.module_table.modules.get(module.idx)?.as_normal()?;
    if linked.stable_id.as_str() != module.stable_id.as_str()
      || linked.import_records.len() != module.import_records.len()
    {
      return None;
    }
    cached.link_output.symbol_db.inner().get(module.idx)?.as_ref().filter(covers)
  };
  if rescanned { scanned } else { scanned.or_else(linked) }.map(|db| db.ast_scopes.scoping())
}

/// Whether development output drops `module`'s import `record_idx` as an unused import of a
/// module without side effects, which `pure` says once linked ([`unused_pure_import`]), by
/// `scoping`, or, without a scope tree, whether it may: an import that binds names of such a
/// module and re-exports none of them.
#[cfg(feature = "experimental")]
pub fn drops_import(
  module: &NormalModule,
  record_idx: rolldown_common::ImportRecordIdx,
  pure: impl Fn(rolldown_common::ModuleIdx) -> bool,
  scoping: Option<&oxc::semantic::Scoping>,
) -> bool {
  use rolldown_common::{ImportKind, ImportRecordMeta};

  if let Some(scoping) = scoping {
    return unused_pure_import(module, record_idx, pure, scoping);
  }
  let record = &module.import_records[record_idx];
  !module.meta.has_eval()
    && record.kind == ImportKind::Import
    && !record.meta.intersects(ImportRecordMeta::IsReExportOnly | ImportRecordMeta::IsExportStar)
    && module.named_imports.iter().any(|(_, import)| import.record_idx == record_idx)
    && !module.named_imports.iter().any(|(binding, import)| {
      import.record_idx == record_idx
        && module.named_exports.values().any(|export| export.referenced == *binding)
    })
    && record.resolved_module.is_some_and(pure)
}

/// The records of `module`'s `import defer` declarations in `program`, which link their targets
/// without evaluating them.
#[cfg(feature = "experimental")]
pub fn deferred_records(
  module: &NormalModule,
  program: &ast::Program,
) -> rustc_hash::FxHashSet<rolldown_common::ImportRecordIdx> {
  program
    .body
    .iter()
    .filter_map(|statement| match statement {
      ast::Statement::ImportDeclaration(import)
        if import.phase == Some(ast::ImportPhase::Defer) =>
      {
        module.imports.get(&import.node_id()).copied()
      }
      _ => None,
    })
    .collect()
}

/// Whether `program` calls `import.meta.hot.invalidate()` anywhere, as a module may in the
/// callback with which it accepts its own updates: the update then goes on to the modules that
/// import it, as though it accepted none.
#[cfg(feature = "experimental")]
pub fn calls_hot_invalidate(program: &ast::Program) -> bool {
  use oxc::ast_visit::{VisitJs, walk_js};
  struct Invalidates(bool);
  impl<'a> VisitJs<'a> for Invalidates {
    fn visit_call_expression(&mut self, call: &ast::CallExpression<'a>) {
      if let ast::Expression::StaticMemberExpression(invalidate) = &call.callee
        && invalidate.property.name == "invalidate"
        && let ast::Expression::StaticMemberExpression(hot) = &invalidate.object
        && hot.property.name == "hot"
        && matches!(hot.object, ast::Expression::ImportMeta(_))
      {
        self.0 = true;
        return;
      }
      walk_js::walk_call_expression(self, call);
    }
  }
  let mut invalidates = Invalidates(false);
  invalidates.visit_program(program);
  invalidates.0
}

/// The runtime matches accepted deps by stable id, so each recorded dep is rewritten to it.
pub fn rewrite_hot_accept_deps<'ast>(
  call_expr: &mut ast::CallExpression<'ast>,
  module: &NormalModule,
  modules: &IndexModules,
  builder: &AstBuilder<'ast>,
) {
  if !call_expr.callee.is_import_meta_hot_accept() {
    return;
  }
  let rewrite = |expr: &mut ast::Expression<'ast>| {
    let Some(request) = expr.as_static_module_request() else { return };
    let Some(record_idx) =
      module.hmr_info.module_request_to_import_record_idx.get(request.as_ref())
    else {
      return;
    };
    let Some(module_idx) = module.import_records[*record_idx].resolved_module else { return };
    *expr = ast::Expression::new_string_literal(
      expr.span(),
      ast::Str::from_str_in(modules[module_idx].stable_id(), builder),
      None,
      builder,
    );
  };
  match call_expr.arguments.first_mut() {
    Some(ast::Argument::ArrayExpression(array_expression)) => {
      array_expression
        .elements
        .iter_mut()
        .filter_map(|element| element.as_expression_mut())
        .for_each(rewrite);
    }
    Some(argument) => {
      if let Some(expr) = argument.as_expression_mut() {
        rewrite(expr);
      }
    }
    None => {}
  }
}

#[cfg(feature = "experimental")]
pub static MODULE_EXPORTS_NAME_FOR_ESM: &str = "__rolldown_exports__";
#[cfg(feature = "experimental")]
pub static MODULE_ID_PARAM_FOR_HMR: &str = "__rolldown_module_id__";

pub trait HmrAstBuilder<'any, 'ast> {
  fn builder(&self) -> AstBuilder<'ast>;

  fn module(&self) -> &NormalModule;

  /// The kind that this module registers as: the link's, which a chunk's module table holds
  /// and an HMR render takes from its stage.
  fn exports_kind(&self) -> rolldown_common::ExportsKind {
    self.module().exports_kind
  }

  // `${ns_name}` in `var ${ns_name} = ...`
  fn binding_name_for_namespace_object_ref_atom(&self) -> ast::Str<'ast>;

  // `$hot_name` in var `$hot_name = __rolldown_runtime__.createModuleHotContext($stable_id);`
  fn alias_name_for_import_meta_hot(&self) -> ast::Str<'ast>;

  fn cjs_module_name() -> &'static str;

  fn initial_evaluation(&self) -> Option<ast::Expression<'ast>> {
    None
  }

  /// Whether `exports` is this record's own native module namespace object.
  fn native_record(&self) -> bool {
    false
  }

  /// Whether this CommonJS module is its own native record, which registers its `module`.
  fn native_commonjs_record(&self) -> bool {
    false
  }

  /// Whether this module is emitted as its own native browser module record.
  fn native_module_record(&self) -> bool {
    false
  }

  fn owns_initial_bindings(&self) -> bool {
    self.native_record()
  }

  /// A native record's export binding cells, which a committed hot update writes. See
  /// `ScopeHoistingFinalizer::native_cells`.
  fn native_cells(&self) -> Option<ast::Expression<'ast>> {
    None
  }

  /// A native record's lowered-binding initialization flags, which other records read. See
  /// `ScopeHoistingFinalizer::native_temporal`.
  fn native_temporal(&self) -> Option<ast::Expression<'ast>> {
    None
  }

  /// How to refer to the current module id at the emission site.
  ///
  /// The HMR/lazy path wraps each module body in `registerFactory(id, function () { … })`,
  /// so inside the body the id is available as an identifier (`__rolldown_module_id__`) passed in
  /// by the runtime. The main-bundle path has no such wrapper, so it still needs to emit the
  /// stable id as a string literal.
  fn module_id_argument(&self) -> ast::Argument<'ast> {
    ast::Argument::new_string_literal(
      SPAN,
      ast::Str::from_str_in(&self.module().stable_id, &self.builder()),
      None,
      &self.builder(),
    )
  }

  /// `__rolldown_runtime__.registerModule(moduleId[, module])`
  fn create_register_module_stmt(&self) -> ast::Statement<'ast> {
    let module_exports = match self.exports_kind() {
      rolldown_common::ExportsKind::Esm => {
        let binding_name_for_namespace_object_ref_atom =
          self.binding_name_for_namespace_object_ref_atom();
        let namespace_object_ref_expr = ast::Expression::new_identifier(
          SPAN,
          binding_name_for_namespace_object_ref_atom,
          &self.builder(),
        );

        let mut properties = vec![ast::ObjectPropertyKind::new_object_property(
          SPAN,
          ast::PropertyKind::Init,
          ast::PropertyKey::new_static_identifier(SPAN, "exports", &self.builder()),
          namespace_object_ref_expr,
          true,
          false,
          false,
          &self.builder(),
        )];
        if self.native_record() {
          properties.push(ast::ObjectPropertyKind::new_object_property(
            SPAN,
            ast::PropertyKind::Init,
            ast::PropertyKey::new_static_identifier(SPAN, "native", &self.builder()),
            ast::Expression::new_boolean_literal(SPAN, true, &self.builder()),
            false,
            false,
            false,
            &self.builder(),
          ));
        }
        if self.owns_initial_bindings() && !self.native_record() {
          properties.push(ast::ObjectPropertyKind::new_object_property(
            SPAN,
            ast::PropertyKind::Init,
            ast::PropertyKey::new_static_identifier(SPAN, "bindings", &self.builder()),
            ast::Expression::new_boolean_literal(SPAN, true, &self.builder()),
            false,
            false,
            false,
            &self.builder(),
          ));
        }
        if let Some(cells) = self.native_cells() {
          properties.push(ast::ObjectPropertyKind::new_object_property(
            SPAN,
            ast::PropertyKind::Init,
            ast::PropertyKey::new_static_identifier(SPAN, "cells", &self.builder()),
            cells,
            false,
            false,
            false,
            &self.builder(),
          ));
        }
        if let Some(temporal) = self.native_temporal() {
          properties.push(ast::ObjectPropertyKind::new_object_property(
            SPAN,
            ast::PropertyKind::Init,
            ast::PropertyKey::new_static_identifier(SPAN, "temporal", &self.builder()),
            temporal,
            false,
            false,
            false,
            &self.builder(),
          ));
        }
        if let Some(evaluate) = self.initial_evaluation() {
          properties.push(ast::ObjectPropertyKind::new_object_property(
            SPAN,
            ast::PropertyKind::Init,
            ast::PropertyKey::new_static_identifier(SPAN, "evaluate", &self.builder()),
            evaluate,
            true,
            false,
            false,
            &self.builder(),
          ));
        }
        Some(ast::Argument::new_object_expression(
          SPAN,
          oxc::allocator::Vec::from_iter_in(properties, &self.builder()),
          &self.builder(),
        ))
      }
      rolldown_common::ExportsKind::CommonJs => {
        // `module`
        Some(ast::Argument::new_identifier(SPAN, Self::cjs_module_name(), &self.builder()))
      }
      rolldown_common::ExportsKind::None => None,
    };

    let mut arguments = oxc::allocator::Vec::from_iter_in(
      std::iter::once(self.module_id_argument()).chain(module_exports),
      &self.builder(),
    );
    if self.exports_kind().is_commonjs() {
      arguments.push(if self.native_commonjs_record() {
        ast::Argument::new_string_literal(
          SPAN,
          ast::Str::from_str_in("cjs", &self.builder()),
          None,
          &self.builder(),
        )
      } else {
        ast::Argument::new_numeric_literal(
          SPAN,
          1.0,
          None,
          ast::NumberBase::Decimal,
          &self.builder(),
        )
      });
    }

    // __rolldown_runtime__.registerModule(moduleId[, module[, 1]])
    // moduleId is either `__rolldown_module_id__` (HMR/lazy path) or the stable-id
    // string literal (main-bundle path).
    let register_call = ast::Expression::new_call_expression(
      SPAN,
      ast::Expression::new_identifier(SPAN, "__rolldown_runtime__.registerModule", &self.builder()),
      None,
      arguments,
      false,
      &self.builder(),
    );

    ast::Statement::new_expression_statement(SPAN, register_call, &self.builder())
  }

  /// `__rolldown_runtime__.createModuleHotContext($stable_id)`
  fn create_module_hot_context_call(&self) -> ast::Expression<'ast> {
    ast::Expression::new_call_expression(
      SPAN,
      ast::Expression::new_identifier(
        SPAN,
        "__rolldown_runtime__.createModuleHotContext",
        &self.builder(),
      ),
      None,
      [self.module_id_argument()],
      false,
      &self.builder(),
    )
  }

  /// `const $hot_name = __rolldown_runtime__.createModuleHotContext($stable_id);`
  ///
  /// A native record's own body runs after its static dependencies, yet a cyclic dependency may
  /// call one of the record's hoisted functions first. There the binding is a hoisted `var`
  /// that `import.meta.hot` initializes on first use (`$hot_name ??= …`), so the record keeps one
  /// hot context per evaluation instead of reading a binding in its temporal dead zone.
  fn create_module_hot_context_initializer_stmt(&self) -> ast::Statement<'ast> {
    let name = self.alias_name_for_import_meta_hot();
    let native = self.native_module_record();
    let init = if native {
      ast::Expression::new_logical_expression(
        SPAN,
        ast::Expression::new_identifier(SPAN, name, &self.builder()),
        ast::LogicalOperator::Coalesce,
        self.create_module_hot_context_call(),
        &self.builder(),
      )
    } else {
      self.create_module_hot_context_call()
    };
    ast::Statement::new_variable_declaration(
      SPAN,
      if native { ast::VariableDeclarationKind::Var } else { ast::VariableDeclarationKind::Const },
      oxc::allocator::Vec::from_value_in(
        ast::VariableDeclarator::new(
          SPAN,
          ast::BindingPattern::new_binding_identifier(SPAN, name, &self.builder()),
          None,
          Some(init),
          false,
          &self.builder(),
        ),
        &self.builder(),
      ),
      false,
      &self.builder(),
    )
  }
}

#[cfg(feature = "experimental")]
impl<'any, 'ast> HmrAstBuilder<'any, 'ast> for HmrAstFinalizer<'any, 'ast> {
  fn builder(&self) -> AstBuilder<'ast> {
    AstBuilder::new(self.ast_builder.allocator())
  }

  fn module(&self) -> &NormalModule {
    self.module
  }

  fn exports_kind(&self) -> rolldown_common::ExportsKind {
    self.exports_kind
  }

  fn binding_name_for_namespace_object_ref_atom(&self) -> ast::Str<'ast> {
    ast::Str::from(MODULE_EXPORTS_NAME_FOR_ESM)
  }

  fn alias_name_for_import_meta_hot(&self) -> ast::Str<'ast> {
    ast::Str::from_str_in(&format!("hot_{}", self.module.repr_name), &self.builder())
  }

  fn cjs_module_name() -> &'static str {
    CJS_ROLLDOWN_MODULE_REF
  }

  /// HMR/lazy path: each module body is wrapped in
  /// `registerFactory(id, function (__rolldown_module_id__) { … })`,
  /// so the id is in lexical scope as a parameter.
  fn module_id_argument(&self) -> ast::Argument<'ast> {
    ast::Argument::new_identifier(SPAN, MODULE_ID_PARAM_FOR_HMR, &self.builder())
  }
}

fn bundled_observed_imports(
  importer: &NormalModule,
  scoping: &oxc::semantic::Scoping,
) -> FxHashSet<ModuleIdx> {
  use rolldown_common::{ImportKind, ImportRecordMeta};

  let mut observed = FxHashSet::default();
  for record in &importer.import_records {
    if (record.meta.intersects(ImportRecordMeta::IsReExportOnly | ImportRecordMeta::IsExportStar)
      || (matches!(record.kind, ImportKind::Require | ImportKind::DynamicImport)
        && !record
          .meta
          .intersects(ImportRecordMeta::IsRequireUnused | ImportRecordMeta::DeadDynamicImport)))
      && let Some(target) = record.resolved_module
    {
      observed.insert(target);
    }
  }
  if importer.named_imports.is_empty() {
    return observed;
  }
  let reexports: FxHashSet<_> =
    importer.named_exports.values().map(|export| export.referenced).collect();
  for (binding, import) in &importer.named_imports {
    if (scoping.get_resolved_references(binding.symbol).next().is_some()
      || reexports.contains(binding))
      && let Some(target) = importer.import_records[import.record_idx].resolved_module
    {
      observed.insert(target);
    }
  }
  observed
}

pub fn bundled_exports_observed_by_importers(
  module: &NormalModule,
  modules: &IndexModules,
  linking_infos: &LinkingMetadataVec,
  symbol_db: &SymbolRefDb,
) -> bool {
  for &importer_idx in &module.importers_idx {
    let Some(importer_info) = linking_infos.get(importer_idx).filter(|info| info.is_included)
    else {
      continue;
    };
    let Some(importer) = modules.get(importer_idx).and_then(|m| m.as_normal()) else {
      continue;
    };
    if importer.meta.has_eval() {
      return true;
    }
    let Some(scoping) = symbol_db
      .inner()
      .get(importer_idx)
      .and_then(Option::as_ref)
      .map(|db| db.ast_scopes.scoping())
    else {
      return true;
    };
    let observed = importer_info
      .bundled_observed_imports
      .get_or_init(|| bundled_observed_imports(importer, scoping));
    if observed.contains(&module.idx) {
      return true;
    }
  }
  false
}

impl ScopeHoistingFinalizer<'_, '_> {
  fn bundled_exports_need_initial_bindings(&self) -> bool {
    let scoping = self.scope.scoping();
    let root = scoping.root_scope_id();
    let mut has_own_export = false;
    for export in self.ctx.linking_info.resolved_exports.values() {
      let canonical = self.ctx.symbol_db.canonical_ref_for(export.symbol_ref);
      if canonical.owner != self.ctx.idx {
        continue;
      }
      has_own_export = true;
      if self.ctx.module.meta.has_eval() {
        return true;
      }
      let flags = scoping.symbol_flags(canonical.symbol);
      if !self.ctx.symbol_db.is_facade_symbol(canonical)
        && scoping.symbol_scope_id(canonical.symbol) == root
        && (flags.is_import()
          || (flags.is_const_variable() && !self.native_let_symbols.contains(&canonical.symbol))
          || scoping.get_resolved_references(canonical.symbol).next().is_some())
      {
        return true;
      }
    }
    if !has_own_export {
      return false;
    }
    if self.ctx.chunk.entry_module_idx() == Some(self.ctx.idx)
      || !self.ctx.linking_info.referenced_symbols_by_entry_point_chunk.is_empty()
    {
      return true;
    }
    bundled_exports_observed_by_importers(
      self.ctx.module,
      self.ctx.modules,
      self.ctx.linking_infos,
      self.ctx.symbol_db,
    )
  }
}

impl<'any, 'ast> HmrAstBuilder<'any, 'ast> for ScopeHoistingFinalizer<'any, 'ast> {
  fn owns_initial_bindings(&self) -> bool {
    self.native_record()
      || (crate::utils::native_records::runtime_module_semantics_enabled(self.ctx.options)
        && self.ctx.options.is_dev_mode_enabled()
        && !crate::utils::native_records::enabled(self.ctx.options)
        && self.exports_kind() == rolldown_common::ExportsKind::Esm
        && self.bundled_exports_need_initial_bindings())
  }

  fn native_commonjs_record(&self) -> bool {
    self.native_module_record()
  }

  fn native_module_record(&self) -> bool {
    self.ctx.chunk.entry_module_idx() == Some(self.ctx.idx)
      && crate::utils::native_records::enabled(self.ctx.options)
  }

  fn native_record(&self) -> bool {
    self.ctx.chunk.entry_module_idx() == Some(self.ctx.idx)
      && crate::utils::native_records::has_native_namespace(
        self.ctx.options,
        self.ctx.module,
        self.ctx.linking_info,
        self.ctx.symbol_db,
      )
  }

  /// `{ name: [get, set] | [get] | 0 }` for every export the record's own module declares.
  /// `[get, set]` reads and writes a mutable top-level binding; `[get]` is a binding a hot
  /// update cannot rebind (an imported binding, a constant its own module assigns, or the
  /// engine-named anonymous default declaration), so the update must leave its value identical; `0` is a binding its own module assigns after
  /// declaration (or that direct `eval` may assign), which a later generation could not keep
  /// in sync, so any update to the record requires a reload. A re-exported name has no cell:
  /// the engine forwards it to its declaring record.
  fn native_cells(&self) -> Option<ast::Expression<'ast>> {
    if !self.owns_initial_bindings() {
      return None;
    }
    let scoping = self.scope.scoping();
    let root = scoping.root_scope_id();
    let has_eval = self.ctx.module.meta.has_eval();
    let namespace = self.canonical_name_for(self.ctx.module.namespace_object_ref);
    let mut entries = Vec::new();
    for name in self.ctx.linking_info.sorted_and_non_ambiguous_resolved_exports.keys() {
      let Some(export) = self.ctx.linking_info.resolved_exports.get(name) else { continue };
      let canonical = self.ctx.symbol_db.canonical_ref_for(export.symbol_ref);
      if canonical.owner != self.ctx.idx {
        continue;
      }
      let cell = if canonical == self.ctx.module.default_export_ref
        && self.ctx.linking_info.native_default_expression
      {
        // `export default <expression>` prints as `let <local>`, which nothing else assigns.
        let local = self.canonical_name_for(canonical);
        if has_eval { Cell::Reload } else { Cell::GetSet(local) }
      } else if self.ctx.symbol_db.is_facade_symbol(canonical) {
        if !self.native_record() && canonical == self.ctx.module.default_export_ref {
          if has_eval { Cell::Reload } else { Cell::GetSet(self.canonical_name_for(canonical)) }
        } else {
          Cell::Facade(namespace)
        }
      } else if scoping.symbol_scope_id(canonical.symbol) != root {
        continue;
      } else {
        let local = self.canonical_name_for(canonical);
        let flags = scoping.symbol_flags(canonical.symbol);
        let written = has_eval
          || scoping
            .get_resolved_references(canonical.symbol)
            .any(oxc::semantic::Reference::is_write);
        if flags.is_import()
          || flags.is_const_variable() && !self.native_let_symbols.contains(&canonical.symbol)
        {
          if has_eval { Cell::Reload } else { Cell::Get(local) }
        } else if written {
          Cell::Reload
        } else {
          Cell::GetSet(local)
        }
      };
      entries.push((name.as_str(), cell));
    }
    Some(native_record_ast::cells(&self.builder(), entries))
  }

  /// `{ name: () => flag }` for every export binding of this record whose declaration a
  /// transform lowered out of the engine's TDZ. Another record cannot import the flag without
  /// adding a key to this namespace, so it asks the runtime with this namespace instead; before
  /// this registration runs the record has not started and the binding is uninitialized.
  fn native_temporal(&self) -> Option<ast::Expression<'ast>> {
    // Wrapped bundled records track completion through their evaluator. Unwrapped records
    // still need these flags to reject pending or failed initialization.
    if !self.native_record()
      && (!crate::utils::native_records::runtime_module_semantics_enabled(self.ctx.options)
        || self.ctx.order_wrap_state.esm_init_target(self.ctx.idx, self.ctx.linking_info).is_some())
    {
      return None;
    }
    // Ordered by each name's JSON string, the key order the registration has always used.
    let mut entries = std::collections::BTreeMap::new();
    for export in self.ctx.linking_info.resolved_exports.values() {
      let canonical = self.ctx.symbol_db.canonical_ref_for(export.symbol_ref);
      if canonical.owner != self.ctx.idx {
        continue;
      }
      if let Some(flag) = self.ctx.linking_info.temporal_symbols.get(&canonical) {
        let name = canonical.name(self.ctx.symbol_db);
        let key = serde_json::to_string(name).expect("name JSON");
        entries.insert(key, (name, self.canonical_name_for(*flag)));
      }
    }
    if entries.is_empty() {
      return None;
    }
    Some(native_record_ast::temporal(&self.builder(), entries.into_values()))
  }

  fn initial_evaluation(&self) -> Option<ast::Expression<'ast>> {
    if !crate::utils::native_records::runtime_module_semantics_enabled(self.ctx.options) {
      return None;
    }
    let target = self.ctx.order_wrap_state.esm_init_target(self.ctx.idx, self.ctx.linking_info)?;
    Some(ast::Expression::new_identifier(
      SPAN,
      ast::Str::from_str_in(self.canonical_name_for(target.wrapper_ref), &self.builder()),
      &self.builder(),
    ))
  }

  fn builder(&self) -> AstBuilder<'ast> {
    AstBuilder::new(self.ast_builder.allocator())
  }

  fn module(&self) -> &NormalModule {
    self.ctx.module
  }

  fn binding_name_for_namespace_object_ref_atom(&self) -> ast::Str<'ast> {
    let name = self.canonical_name_for(self.ctx.module.namespace_object_ref);
    ast::Str::from_str_in(name, &self.builder())
  }

  fn alias_name_for_import_meta_hot(&self) -> ast::Str<'ast> {
    let name =
      self.canonical_name_for(self.ctx.module.hmr_hot_ref.expect("HMR hot ref should be set"));
    ast::Str::from_str_in(name, &self.builder())
  }

  fn cjs_module_name() -> &'static str {
    CJS_MODULE_REF
  }
}

/// `__rolldown_runtime__.requestLazy("<stable_real_id>", "<proxy_module_id>"[, 1])`
///
/// The one shape both codegen paths emit for a lazy boundary, so a boundary never becomes a
/// chunk the browser fetches.
///
/// The emitted code carries ids only. The runtime's `loadLazyChunk` builds the request, so the
/// dev server decides the URL (Vite adds its `base`).
///
/// `is_node_mode` is the importer's interop choice, as in a production build. Whether the
/// importee is CommonJS is unknown here, so the runtime decides that from its registration.
pub fn create_request_lazy_call<'ast, B>(
  proxy_module_id: &str,
  stable_proxy_id: &str,
  is_node_mode: bool,
  builder: &B,
) -> ast::Expression<'ast>
where
  B: oxc::ast::builder::GetAstBuilder<'ast> + GetAllocator<'ast>,
{
  let mut arguments = oxc::allocator::Vec::from_iter_in(
    [
      ast::Argument::new_string_literal(
        SPAN,
        ast::Str::from_str_in(
          lazy_proxy_target(stable_proxy_id).unwrap_or(stable_proxy_id),
          builder,
        ),
        None,
        builder,
      ),
      ast::Argument::new_string_literal(
        SPAN,
        ast::Str::from_str_in(proxy_module_id, builder),
        None,
        builder,
      ),
    ],
    builder,
  );
  if is_node_mode {
    arguments.push(ast::Argument::new_numeric_literal(
      SPAN,
      1.0,
      None,
      ast::NumberBase::Decimal,
      builder,
    ));
  }

  ast::Expression::new_call_expression(
    SPAN,
    ast::Expression::new_identifier(SPAN, "__rolldown_runtime__.requestLazy", builder),
    None,
    arguments,
    false,
    builder,
  )
}
