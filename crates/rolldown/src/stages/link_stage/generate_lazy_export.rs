use indexmap::map::Entry;
use oxc::allocator::GetAllocator;
use oxc::ast::builder::AstBuilder;
use oxc::{
  allocator::{ReplaceWith, TakeIn},
  ast::ast::{self, Expression, Statement},
  semantic::{SemanticBuilder, Stats},
  span::SPAN,
};
use oxc_str::CompactStr;
use rolldown_common::{
  EcmaModuleAstUsage, EntryPointKind, ExportsKind, GetLocalDbMut, ImportKind, ImportRecordMeta,
  LocalExport, Module, ModuleIdx, ModuleType, NormalModule, Specifier, StmtInfo, StmtInfoIdx,
  StmtInfos, SymbolOrMemberExprRef, SymbolRef, SymbolRefDbForModule, TaggedSymbolRef, WrapKind,
  dynamic_import_usage::DynamicImportExportsUsage,
};
use rolldown_ecmascript::EcmaAst;
use rolldown_ecmascript_utils::{ExpressionFactoryExt as _, StatementFactoryExt as _};
use rolldown_utils::ecmascript::legitimize_json_local_binding_name;
#[cfg(not(target_family = "wasm"))]
use rolldown_utils::rayon::IndexedParallelIterator;
use rolldown_utils::{
  indexmap::FxIndexMap,
  rayon::{IntoParallelRefMutIterator, ParallelIterator},
};
use rustc_hash::FxHashSet;
use smallvec::smallvec;

use super::LinkStage;

/// Index of the first statement after the namespace statement (index 0).
const FIRST_TOP_LEVEL_STMT_IDX: StmtInfoIdx = StmtInfoIdx::from_raw_unchecked(1);

struct LazyModuleInfo {
  idx: ModuleIdx,
  exports_kind: ExportsKind,
  is_json: bool,
}

impl LinkStage<'_> {
  #[tracing::instrument(level = "debug", skip_all)]
  pub(super) fn generate_lazy_export(&mut self) {
    let lazy_modules = append_only_vec::AppendOnlyVec::new();
    self
      .module_table
      .modules
      .par_iter_mut()
      .zip(self.stmt_infos.par_iter_mut())
      .filter_map(|(m, stmt_infos)| m.as_normal_mut().map(|m| (m, stmt_infos)))
      .filter(|(module, _)| module.meta.has_lazy_export())
      .for_each(|(module, stmt_infos)| {
        let default_symbol_ref = module.default_export_ref;
        let is_json = matches!(module.module_type, ModuleType::Json);
        if !is_json || module.exports_kind == ExportsKind::CommonJs {
          update_module_default_export_info(module, stmt_infos, default_symbol_ref);
        }
        lazy_modules.push(LazyModuleInfo {
          idx: module.idx,
          exports_kind: module.exports_kind,
          is_json,
        });

        // generate `module.exports = expr`
        if module.exports_kind == ExportsKind::CommonJs {
          // since the wrap arguments are generate on demand, we need to insert the module ref usage here.
          stmt_infos.infos[FIRST_TOP_LEVEL_STMT_IDX].eval_flags = true.into();
          module.ecma_view.ast_usage.insert(EcmaModuleAstUsage::ModuleRef);
        }
      });

    for LazyModuleInfo { idx: module_idx, exports_kind, is_json } in lazy_modules {
      if self.options.experimental.is_incremental_build_enabled() {
        if let Some(ecma_ast) = self.ast_table[module_idx].as_ref() {
          self
            .pre_lazy_export_asts
            .entry(module_idx)
            .or_insert_with(|| ecma_ast.clone_with_another_arena());
        }
      }
      let Some(ecma_ast) = &mut self.ast_table[module_idx] else { unreachable!() };
      if matches!(exports_kind, ExportsKind::CommonJs) {
        replace_first_expr_stmt(ecma_ast, LazyExportWrap::CjsExport);
        continue;
      }
      // ExportsKind == Esm && ModuleType == Json
      if is_json {
        unwrap_json_module(self, module_idx);
        if json_object_expr_to_esm(self, module_idx) {
          continue;
        }
        // if json is not a ObjectExpression, we will fallback to normal esm lazy export transform
        let stmt_infos = &mut self.stmt_infos[module_idx];
        let module = &mut self.module_table[module_idx];
        let module = module.as_normal_mut().unwrap();
        let default_export_ref = module.default_export_ref;
        update_module_default_export_info(module, stmt_infos, default_export_ref);
      }

      // shadowing the previous mutable ref, to avoid reference mutable ref twice at the same time.
      let Some(ecma_ast) = &mut self.ast_table[module_idx] else { unreachable!() };
      replace_first_expr_stmt(ecma_ast, LazyExportWrap::EsmDefault);

      // Ensure exports_kind is set to Esm for all modules that generate ESM export syntax.
      // This is needed for proper CJS export rendering in preserveModules mode.
      let module = &mut self.module_table[module_idx];
      let module = module.as_normal_mut().unwrap();
      module.exports_kind = ExportsKind::Esm;
    }
  }
}

#[derive(Clone, Copy)]
enum LazyExportWrap {
  CjsExport,
  EsmDefault,
}

/// Takes the expression of the first statement (which must be an `ExpressionStatement`)
/// and replaces the statement with either `module.exports = expr` or `export default expr`.
fn replace_first_expr_stmt(ecma_ast: &mut EcmaAst, kind: LazyExportWrap) {
  ecma_ast.program.with_mut(|fields| {
    let ast_builder = AstBuilder::new(fields.allocator);
    let Some(stmt) = fields.program.body.first_mut() else { unreachable!() };
    stmt.replace_with(|old| {
      let ast::Statement::ExpressionStatement(expr_stmt) = old else { unreachable!() };
      let expr = expr_stmt.unbox().expression;
      match kind {
        LazyExportWrap::CjsExport => Statement::new_module_exports_stmt(expr, &ast_builder),
        LazyExportWrap::EsmDefault => Statement::new_export_default_stmt(expr, &ast_builder),
      }
    });
  });
}

/// Returns the owned expression with any wrapping `(...)` parentheses removed.
fn into_without_parentheses(mut expr: Expression<'_>) -> Expression<'_> {
  while let Expression::ParenthesizedExpression(paren_expr) = expr {
    expr = paren_expr.unbox().expression;
  }
  expr
}

fn update_module_default_export_info(
  module: &mut NormalModule,
  stmt_infos: &mut StmtInfos,
  default_symbol_ref: SymbolRef,
) {
  module.named_exports.insert(
    "default".into(),
    LocalExport { span: SPAN, referenced: default_symbol_ref, came_from_commonjs: false },
  );
  stmt_infos
    .declare_symbol_for_stmt(FIRST_TOP_LEVEL_STMT_IDX, TaggedSymbolRef::normal(default_symbol_ref));
}

enum SplitJsonKeys {
  All,
  Only(FxHashSet<CompactStr>),
}

impl SplitJsonKeys {
  fn contains(&self, key: &str) -> bool {
    match self {
      Self::All => !key.is_empty(),
      Self::Only(set) => !key.is_empty() && key != "default" && set.contains(key),
    }
  }
}

/// Determines which top-level JSON object keys can be bound as individual `SymbolRef`s in the
/// graph, whether any reachable module statically imports or requires `module_idx`, and whether
/// the JSON module's namespace exposes top-level properties.
///
/// Standalone dynamic-only JSON entry chunks (`ChunkMeta::DynamicImported` without a public/emitted
/// entry) do not rely on chunk-level named exports for `Object.keys(await import('./d.json'))`:
/// `GenerateStage::preserve_dynamic_entry_namespaces` (`dynamic_already_loaded.rs`) always publishes
/// `module.namespace_object_ref` (`__jsonExports(() => default, no_symbols)`) and rewrites dynamic
/// `import()` callers to extract that namespace object whenever `runtime_module_semantics_enabled`
/// is active. Any JSON module that also has a non-`DynamicImport` entry (`UserDefined`,
/// `EmittedUserDefined`, or `EmittedPredefinedChunk`, regardless of `preserveEntrySignatures`) falls
/// back to `SplitJsonKeys::All` because `preserve_dynamic_entry_namespaces` skips multi-role entry
/// chunks (`meta != ChunkMeta::DynamicImported`).
fn needed_json_keys(link_staged: &LinkStage, module_idx: ModuleIdx) -> (SplitJsonKeys, bool, bool) {
  if !crate::utils::native_records::runtime_module_semantics_enabled(link_staged.options) {
    return (SplitJsonKeys::All, true, false);
  }
  let Some(module) = link_staged.module_table[module_idx].as_normal() else {
    return (SplitJsonKeys::All, true, false);
  };
  let json_ns_has_props = crate::utils::import_options::json_namespace_has_properties(
    module,
    &link_staged.module_table.modules,
    |idx| link_staged.unreachable_modules.reaches(idx),
  );
  if link_staged.options.is_dev_mode_enabled() || link_staged.options.preserve_modules {
    return (SplitJsonKeys::All, true, json_ns_has_props);
  }
  let has_non_dynamic_entry = link_staged.entries.get(&module_idx).is_some_and(|entries| {
    entries.iter().any(|entry| !matches!(entry.kind, EntryPointKind::DynamicImport))
  });
  if has_non_dynamic_entry {
    return (SplitJsonKeys::All, true, json_ns_has_props);
  }

  let mut has_static_importer = false;
  let mut needed = FxHashSet::default();

  for &importer_idx in &module.ecma_view.importers_idx {
    if !link_staged.unreachable_modules.reaches(importer_idx) {
      continue;
    }
    let Some(importer) = link_staged.module_table[importer_idx].as_normal() else {
      continue;
    };
    for (rec_idx, rec) in importer.import_records.iter_enumerated() {
      if rec.resolved_module != Some(module_idx) {
        continue;
      }
      if rec.kind == ImportKind::Require && !rec.meta.contains(ImportRecordMeta::IsRequireUnused) {
        has_static_importer = true;
        continue;
      }
      if rec.kind != ImportKind::Import {
        continue;
      }
      has_static_importer = true;
      if rec.meta.contains(ImportRecordMeta::IsExportStar) {
        return (SplitJsonKeys::All, true, json_ns_has_props);
      }
      for named_import in importer.named_imports.values() {
        if named_import.record_idx != rec_idx {
          continue;
        }
        match &named_import.imported {
          Specifier::Literal(name) if name.as_str() != "default" => {
            needed.insert(name.clone());
          }
          Specifier::Literal(_) => {
            if importer
              .named_exports
              .values()
              .any(|export| export.referenced == named_import.imported_as)
            {
              return (SplitJsonKeys::All, true, json_ns_has_props);
            }
            let escapes_locally = link_staged.stmt_infos[importer.idx].iter().any(|stmt_info| {
              stmt_info.referenced_symbols.iter().any(|reference| match reference {
                SymbolOrMemberExprRef::Symbol(sym) => *sym == named_import.imported_as,
                SymbolOrMemberExprRef::MemberExpr(member) => {
                  member.is_write && member.object_ref == named_import.imported_as
                }
              })
            });
            if escapes_locally {
              continue;
            }
            for stmt_info in link_staged.stmt_infos[importer.idx].iter() {
              for reference in &stmt_info.referenced_symbols {
                if let SymbolOrMemberExprRef::MemberExpr(member) = reference
                  && !member.is_write
                  && member.object_ref == named_import.imported_as
                  && let Some(first) = member.prop_and_span_list.first()
                  && first.name.as_str() != "default"
                {
                  needed.insert(first.name.clone());
                }
              }
            }
          }
          Specifier::Star => {
            if !json_ns_has_props {
              continue;
            }
            if importer
              .named_exports
              .values()
              .any(|export| export.referenced == named_import.imported_as)
            {
              return (SplitJsonKeys::All, true, json_ns_has_props);
            }
            for stmt_info in link_staged.stmt_infos[importer.idx].iter() {
              for reference in &stmt_info.referenced_symbols {
                if let SymbolOrMemberExprRef::MemberExpr(member) = reference
                  && member.object_ref == named_import.imported_as
                {
                  if member.prop_and_span_list.is_empty() {
                    if let Some(key) = importer.ecma_view.namespace_in_checks.get(&member.node_id)
                      && key.as_str() != "default"
                    {
                      needed.insert(key.clone());
                    }
                  } else if let Some(first) = member.prop_and_span_list.first()
                    && first.name.as_str() != "default"
                  {
                    needed.insert(first.name.clone());
                  }
                }
              }
            }
          }
        }
      }
    }
  }

  needed.insert(CompactStr::new_const("then"));
  // `create_exports_for_ecma_modules` upgrades `Partial` to `Complete` when any reachable dynamic
  // importer invokes a method on the namespace (`ns.method()`, where `this` is the namespace). In
  // that case the full namespace will be constructed via `__jsonExports(() => default, ...)` rather
  // than partial `__exportAll` getters, so only genuine non-receiver `Partial` imports need per-key
  // bindings.
  let dynamic_call_widens_to_complete =
    module.ecma_view.dynamic_importers_idx.iter().any(|&importer_idx| {
      link_staged.unreachable_modules.reaches(importer_idx)
        && link_staged.module_table[importer_idx].as_normal().is_some_and(|importer| {
          importer
            .ecma_view
            .dynamic_import_called_names
            .keys()
            .any(|rec_idx| importer.import_records[*rec_idx].resolved_module == Some(module_idx))
        })
    });
  if json_ns_has_props
    && !dynamic_call_widens_to_complete
    && let Some(DynamicImportExportsUsage::Partial(names)) =
      link_staged.dynamic_import_exports_usage_map.get(&module_idx)
  {
    for name in names {
      if name.as_str() != "default" {
        needed.insert(name.clone());
      }
    }
  }

  (SplitJsonKeys::Only(needed), has_static_importer, json_ns_has_props)
}

/// return true if the json is a ObjectExpression whose top-level keys were split into bindings
fn json_object_expr_to_esm(link_staged: &mut LinkStage, module_idx: ModuleIdx) -> bool {
  let is_dev_mode = link_staged.options.is_dev_mode_enabled();
  let (mut split_keys, has_static_importer, json_ns_has_props) =
    needed_json_keys(link_staged, module_idx);
  let module = &mut link_staged.module_table[module_idx];
  let Module::Normal(module) = module else {
    return false;
  };

  let ecma_ast = link_staged.ast_table[module_idx].as_mut().unwrap();
  // (local, (exported, legal_ident))
  let mut declaration_binding_names: FxIndexMap<CompactStr, (CompactStr, bool)> =
    FxIndexMap::default();
  let (transformed, is_plain_json_object) = ecma_ast.program.with_mut(|fields| {
    let mut index_map = FxIndexMap::default();
    let ast_builder = AstBuilder::new(fields.allocator);
    let program = fields.program;
    let Some(ast::Statement::ExpressionStatement(stmt)) = program.body.first_mut() else {
      unreachable!()
    };
    if !matches!(stmt.expression.without_parentheses(), Expression::ObjectExpression(_)) {
      return (false, false);
    }
    let is_plain_json_object = !is_dev_mode
      && match stmt.expression.without_parentheses() {
        Expression::ObjectExpression(obj_expr) => obj_expr.properties.iter().all(|prop| {
          matches!(
            prop,
            ast::ObjectPropertyKind::ObjectProperty(p)
              if p.kind == ast::PropertyKind::Init
                && !p.method
                && (!p.computed || matches!(&p.key, ast::PropertyKey::StringLiteral(_)))
                && p.key
                  .static_name()
                .and_then(oxc::ast::StaticPropertyName::into_cow_str)
                  .is_some()
          )
        }),
        _ => false,
      };
    if !is_plain_json_object {
      split_keys = SplitJsonKeys::All;
    }
    let any_split = match stmt.expression.without_parentheses() {
      Expression::ObjectExpression(obj_expr) => obj_expr.properties.iter().any(|prop| {
        let ast::ObjectPropertyKind::ObjectProperty(p) = prop else { return true };
        let Some(key) = p.key.static_name().and_then(oxc::ast::StaticPropertyName::into_cow_str)
        else {
          return false;
        };
        split_keys.contains(key.as_ref())
      }),
      _ => false,
    };
    if !any_split {
      if let Expression::ObjectExpression(obj_expr) = stmt.expression.without_parentheses_mut() {
        for prop in &mut obj_expr.properties {
          if let ast::ObjectPropertyKind::ObjectProperty(p) = prop
            && p.key.static_name().is_some_and(|k| k == "__proto__")
          {
            p.computed = true;
          }
        }
      }
      return (false, is_plain_json_object);
    }

    // Take the single-statement body by value; this leaves `program.body` empty.
    let Some(ast::Statement::ExpressionStatement(stmt)) =
      program.body.take_in(&ast_builder.allocator()).into_iter().next()
    else {
      unreachable!();
    };
    let Expression::ObjectExpression(mut obj_expr) =
      into_without_parentheses(stmt.unbox().expression)
    else {
      unreachable!();
    };

    // convert {"a": "b", "c": "d"} to
    // {"a": b, "c": d}
    // and collect related info
    for property in &mut obj_expr.properties {
      match property {
        ast::ObjectPropertyKind::ObjectProperty(property) => {
          let key = property.key.static_name().expect("should be static name");
          // JSON and `transform_ast` hooks can supply keys with lone surrogates. An export name
          // can't contain one, so such a key stays only on the default export.
          let Some(key) = key.into_cow_str() else {
            continue;
          };
          if !split_keys.contains(key.as_ref()) {
            if key == "__proto__" {
              property.computed = true;
            }
            continue;
          }
          let legitimized_ident =
            legitimize_json_local_binding_name(&key, &declaration_binding_names);

          let is_legal_ident = legitimized_ident.as_str() == key;

          declaration_binding_names
            .insert(legitimized_ident.clone(), (CompactStr::new(&key), is_legal_ident));

          let value = std::mem::replace(
            &mut property.value,
            Expression::new_id_ref_expr(SPAN, legitimized_ident.as_str(), &ast_builder),
          );
          // TODO(shulaoda): Waiting for oxc transform to support the ES feature `ShorthandProperties`.
          if key == "__proto__" {
            property.computed = true;
          } else if is_legal_ident {
            property.shorthand = is_legal_ident;
            property.key = ast::PropertyKey::new_static_identifier(
              SPAN,
              oxc::ast::ast::Str::from_str_in(legitimized_ident.as_ref(), &ast_builder),
              &ast_builder,
            );
          }
          match index_map.entry(legitimized_ident) {
            Entry::Occupied(mut occ) => {
              *occ.get_mut() = value;
            }
            Entry::Vacant(vac) => {
              vac.insert(value);
            }
          }
        }
        ast::ObjectPropertyKind::SpreadProperty(_) => unreachable!(),
      }
    }
    // recreate Json Module
    let stmts = index_map
      .into_iter()
      // declaration
      .map(|(local, v)| Statement::new_var_decl(local.as_str(), v, &ast_builder))
      // export default json module
      .chain(std::iter::once(Statement::new_export_default_stmt(
        Expression::ObjectExpression(obj_expr),
        &ast_builder,
      )))
      // export all declaration
      .chain(std::iter::once(Statement::new_export_named_stmt(
        declaration_binding_names.iter(),
        &ast_builder,
      )));
    program.body.extend(stmts);
    (true, is_plain_json_object)
  });

  let use_json_default_ns = is_plain_json_object && json_ns_has_props;
  link_staged.metas[module_idx].json_object_default_namespace = use_json_default_ns;
  link_staged.metas[module_idx].eager_json_namespace_snapshot =
    use_json_default_ns && has_static_importer;

  if !transformed {
    return false;
  }
  let original_symbol_ref_db = std::mem::take(link_staged.symbols.local_db_mut(module_idx));
  // recreate semantic data
  #[expect(clippy::cast_possible_truncation)]
  let scoping = ecma_ast.make_symbol_table_and_scope_tree_with_semantic_builder(
    SemanticBuilder::new().with_stats(Stats {
      nodes: declaration_binding_names.len().next_power_of_two() as u32,
      scopes: 1,
      symbols: declaration_binding_names.len() as u32,
      references: declaration_binding_names.len() as u32 * 2u32,
    }),
  );

  // update semantic data of module
  let root_scope_id = scoping.root_scope_id();
  let mut symbol_ref_db = SymbolRefDbForModule::new(scoping, module_idx, root_scope_id);
  // Re-create facade symbols in the new scoping. The JSON module was re-parsed above,
  // producing a new Scoping with fresh symbol IDs, so old facade IDs are invalid.
  // We allocate new IDs by name and update the module's references.
  let mut recreate_facade = |old_ref: SymbolRef| -> SymbolRef {
    symbol_ref_db.create_facade_root_symbol_ref(original_symbol_ref_db.symbol_name(old_ref.symbol))
  };
  module.namespace_object_ref = recreate_facade(module.namespace_object_ref);
  module.default_export_ref = recreate_facade(module.default_export_ref);
  if let Some(hot_ref) = module.hmr_hot_ref {
    module.hmr_hot_ref = Some(recreate_facade(hot_ref));
  }
  if link_staged.options.experimental.is_incremental_build_enabled() {
    link_staged.pre_lazy_export_symbols.entry(module_idx).or_insert(original_symbol_ref_db);
  }
  let namespace_object_ref = module.namespace_object_ref;
  let default_export_ref = module.default_export_ref;

  // update module stmts info
  // clear stmt info, since we need to split `ObjectExpression` into multiple decl, the original stmt info is invalid.
  // preserve the first one, which is `NamespaceRef`
  let stmt_infos = &mut link_staged.stmt_infos[module_idx];
  let stmt_info = stmt_infos.drain(FIRST_TOP_LEVEL_STMT_IDX..);
  let mut all_declared_symbols =
    stmt_info.flat_map(|info| info.referenced_symbols).collect::<Vec<_>>();
  for (local, (exported, _)) in &declaration_binding_names {
    let symbol_id =
      symbol_ref_db.scoping().get_root_binding(local.as_str().into()).expect("should have binding");
    let symbol_ref: SymbolRef = (module_idx, symbol_id).into();
    all_declared_symbols.push(SymbolOrMemberExprRef::from(symbol_ref));
    let stmt_info =
      StmtInfo::default().with_declared_symbols(smallvec![TaggedSymbolRef::normal(symbol_ref)]);
    stmt_infos.add_stmt_info(stmt_info);
    module.named_exports.insert(
      exported.clone(),
      LocalExport { span: SPAN, referenced: symbol_ref, came_from_commonjs: false },
    );
  }
  // declare default export statement
  let stmt_info = StmtInfo::default()
    .with_declared_symbols(smallvec![TaggedSymbolRef::normal(default_export_ref)])
    .with_referenced_symbols(all_declared_symbols.clone());

  stmt_infos.add_stmt_info(stmt_info);
  module.named_exports.insert(
    "default".into(),
    LocalExport { span: SPAN, referenced: default_export_ref, came_from_commonjs: false },
  );

  // declare namespace object statement
  module.exports_kind = ExportsKind::Esm;
  stmt_infos.replace_namespace_stmt_info(
    StmtInfo::default()
      .with_declared_symbols(smallvec![TaggedSymbolRef::normal(namespace_object_ref)])
      .with_referenced_symbols(all_declared_symbols),
  );

  link_staged.symbols.store_local_db(module_idx, symbol_ref_db);
  true
}

/// Removes the wrapper that `wrap_modules` created for an ESM JSON module, for any JSON root. A
/// JSON module declares only data, so no code can see a difference if it runs at load time. Call
/// this function before the JSON transform, which can replace the statement infos.
fn unwrap_json_module(link_staged: &mut LinkStage, module_idx: ModuleIdx) {
  let meta = &mut link_staged.metas[module_idx];
  if let Some(wrapper_stmt_idx) = meta.wrapper_stmt_info.take() {
    // The wrapper statement has no AST node, so an empty statement info removes it.
    *link_staged.stmt_infos[module_idx].get_mut(wrapper_stmt_idx) = StmtInfo::default();
  }
  meta.wrapper_ref = None;
  meta.set_wrap_kind(WrapKind::None);
}
