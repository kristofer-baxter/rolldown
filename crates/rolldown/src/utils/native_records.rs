//! Output in which every preserved module is its own native browser module record.
//!
//! Enabled by `preserve_modules` without execution-order wrappers. An ECMAScript module with a statically
//! known export set then exposes the engine's own module namespace object: its record self-imports
//! it, and importers reach it through `import * as`, so static, dynamic, and `eval` routes observe
//! one identity with exactly the authored export keys.

use oxc::ast::ast::Statement;
use oxc_str::CompactStr;
use rolldown_common::{
  Chunk, ChunkIdx, ImportKind, ImportRecordIdx, ImportRecordMeta, Module, ModuleIdx, NormalModule,
  NormalizedBundlerOptions, OutputFormat, Specifier, SymbolOrMemberExprRef, SymbolRef, SymbolRefDb,
  WrapKind,
};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::{
  chunk_graph::ChunkGraph, stages::link_stage::LinkStageOutput, type_alias::IndexEcmaAst,
  types::linking_metadata::LinkingMetadata,
};

pub const NATIVE_CELL_PREFIX: &str = "\0native-cell:";
pub const NATIVE_JSON_RECORD_PREFIX: &str = "__native/json-values/";
pub const NATIVE_JSON_NAMESPACE_PREFIX: &str = "__native/json-namespaces/";

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct CommonJsDefinition {
  pub suffix: String,
  pub wrapper_export: CompactStr,
}

/// Keep CommonJS registration URLs stable when later imports activate a previously required
/// module. Adjacent records preserve the base of relative imports and source maps.
pub fn commonjs_definitions(
  options: &NormalizedBundlerOptions,
  link_output: &LinkStageOutput,
  chunk_graph: &ChunkGraph,
  emitted_asset_names: &[arcstr::ArcStr],
) -> anyhow::Result<FxHashMap<ChunkIdx, CommonJsDefinition>> {
  let mut definitions = FxHashMap::default();
  if !enabled(options) {
    return Ok(definitions);
  }
  let mut public_names = FxHashSet::default();
  for (idx, chunk) in chunk_graph.chunk_table.iter_enumerated() {
    if chunk
      .entry_module_idx()
      .is_some_and(|module| chunk_graph.module_to_chunk[module] != Some(idx))
    {
      continue;
    }
    if let Some(filename) = &chunk.preliminary_filename {
      anyhow::ensure!(
        public_names.insert(filename.as_str().to_ascii_lowercase()),
        "conflicting CommonJS evaluation record: {}",
        filename.as_str(),
      );
    }
  }
  let mut filenames: FxHashSet<_> = chunk_graph
    .chunk_table
    .iter_enumerated()
    .filter(|(idx, chunk)| {
      chunk
        .entry_module_idx()
        .is_none_or(|module| chunk_graph.module_to_chunk[module] == Some(*idx))
    })
    .flat_map(|(_, chunk)| {
      chunk
        .preliminary_filename
        .iter()
        .chain(chunk.preliminary_sourcemap_filename.iter())
        .map(|filename| filename.as_str().to_ascii_lowercase())
    })
    .collect();
  filenames.extend(emitted_asset_names.iter().map(|name| name.to_ascii_lowercase()));
  for (chunk_idx, chunk) in chunk_graph.chunk_table.iter_enumerated() {
    let Some(module_idx) = chunk.entry_module_idx() else {
      continue;
    };
    let meta = &link_output.metas[module_idx];
    if chunk_graph.module_to_chunk[module_idx] != Some(chunk_idx)
      || meta.wrap_kind() != WrapKind::Cjs
    {
      continue;
    }
    let mut wrapper_export = "__native_require".to_owned();
    let mut count = 2;
    while chunk
      .exports_to_other_chunks
      .values()
      .flatten()
      .any(|name| name.as_str() == wrapper_export)
    {
      wrapper_export = format!("__native_require{count}");
      count += 1;
    }
    let preliminary = chunk.preliminary_filename.as_ref().expect("chunk filename");
    let filename = preliminary.as_str();
    // Suffixes are appended after hash replacement; a hashed final component is not a literal
    // extension that the factory can reuse.
    let extension = std::path::Path::new(filename)
      .extension()
      .and_then(|extension| extension.to_str())
      .filter(|extension| {
        !preliminary.hash_placeholder().is_some_and(|placeholders| {
          placeholders.iter().any(|placeholder| extension.contains(placeholder.as_str()))
        })
      })
      .unwrap_or("js");
    let mut suffix = format!(".cjs-factory.{extension}");
    let mut count = 2;
    let default_map = chunk.preliminary_sourcemap_filename.is_none()
      && matches!(
        options.sourcemap,
        Some(rolldown_common::SourceMapType::File | rolldown_common::SourceMapType::Hidden)
      );
    while filenames.contains(&format!("{filename}{suffix}").to_ascii_lowercase())
      || default_map && filenames.contains(&format!("{filename}{suffix}.map").to_ascii_lowercase())
    {
      suffix = format!(".cjs-factory{count}.{extension}");
      count += 1;
    }
    filenames.insert(format!("{filename}{suffix}").to_ascii_lowercase());
    if default_map {
      filenames.insert(format!("{filename}{suffix}.map").to_ascii_lowercase());
    }
    definitions
      .insert(chunk_idx, CommonJsDefinition { suffix, wrapper_export: wrapper_export.into() });
  }
  Ok(definitions)
}

pub fn omitted_sourcemap_filenames(
  options: &NormalizedBundlerOptions,
  cache: &crate::types::scan_stage_cache::ScanStageCache,
) -> Vec<String> {
  if !cache.native_commonjs_records
    || !matches!(
      options.sourcemap,
      Some(rolldown_common::SourceMapType::File | rolldown_common::SourceMapType::Hidden)
    )
  {
    return Vec::new();
  }
  cache
    .cached_native_chunks
    .values()
    .filter(|cached| {
      cache.last_reused_chunk_filenames.contains(&cached.chunk.filename)
        && (cache.omit_reused_chunk_bytes || cached.sourcemap_asset.is_none())
    })
    .filter_map(|cached| cached.chunk.sourcemap_filename.clone())
    .collect()
}

pub fn validate_output_names(
  output: &[rolldown_common::Output],
  omitted_sourcemap_filenames: &[String],
) -> anyhow::Result<()> {
  let mut names = FxHashSet::default();
  let filenames = omitted_sourcemap_filenames
    .iter()
    .map(String::as_str)
    .chain(output.iter().map(rolldown_common::Output::filename));
  for filename in filenames {
    anyhow::ensure!(
      names.insert(filename.to_lowercase()),
      "conflicting native record output filename: {filename}",
    );
  }
  Ok(())
}

pub fn commonjs_definition_exports(chunk_graph: &ChunkGraph, idx: ChunkIdx) -> Vec<CompactStr> {
  let mut exports =
    super::chunk::render_chunk_exports::get_export_items(&chunk_graph.chunk_table[idx])
      .into_iter()
      .map(|(name, _)| name)
      .filter(|name| name.as_str() != "default")
      .collect::<Vec<_>>();
  exports.push(chunk_graph.native_commonjs_definitions[&idx].wrapper_export.clone());
  exports
}

/// Routes a preserved CommonJS dependency's ordinary bindings and require wrapper separately.
#[derive(Debug)]
pub struct CommonJsImportRoutes<'a> {
  pub primary_suffix: Option<&'a str>,
  pub registration: Option<SymbolRef>,
  pub definition: &'a CommonJsDefinition,
}

/// Registration edges must not evaluate a CommonJS record, even alongside authored ES imports.
/// See internal-docs/bundler-data-lifecycle/implementation.md for the shared route/cache invariant.
pub fn commonjs_import_routes<'a>(
  link_output: &LinkStageOutput,
  chunk_graph: &'a ChunkGraph,
  importer: &Chunk,
  importee: ChunkIdx,
) -> Option<CommonJsImportRoutes<'a>> {
  let definition = chunk_graph.native_commonjs_definitions.get(&importee)?;
  let target = chunk_graph.chunk_table[importee].entry_module_idx()?;
  let evaluates = importer.modules.iter().any(|idx| {
    link_output.module_table[*idx].as_normal().is_some_and(|module| {
      module
        .import_records
        .iter()
        .any(|record| record.kind == ImportKind::Import && record.resolved_module == Some(target))
    })
  });
  let registration = evaluates
    .then(|| link_output.metas[target].wrapper_ref.expect("CommonJS wrapper"))
    .map(|wrapper| link_output.symbol_db.canonical_ref_for(wrapper))
    .filter(|wrapper| {
      importer.imports_from_other_chunks.get(&importee).is_some_and(|items| {
        items
          .iter()
          .any(|item| link_output.symbol_db.canonical_ref_for(item.import_ref) == *wrapper)
      })
    });
  Some(CommonJsImportRoutes {
    primary_suffix: (!evaluates).then_some(definition.suffix.as_str()),
    registration,
    definition,
  })
}

#[must_use]
pub fn deferred_json_import_path(importer: &Chunk, importee: &Chunk) -> String {
  let filename = importee.preliminary_filename.as_ref().expect("chunk filename");
  let Some(facade) = filename.strip_prefix(NATIVE_JSON_RECORD_PREFIX) else {
    return importer.import_path_for(importee);
  };
  let absolute = std::path::Path::new(
    importee.absolute_preliminary_filename.as_ref().expect("absolute chunk filename"),
  );
  let root = absolute
    .ancestors()
    .nth(std::path::Path::new(filename.as_str()).components().count())
    .expect("output root");
  let path = importer.relative_path_for(&root.join(NATIVE_JSON_NAMESPACE_PREFIX).join(facade));
  if path.starts_with("../") { path } else { format!("./{path}") }
}

/// Bounds of the cell bypass certificate's closure walk; HMR's scan verdicts use the same ones.
pub const CELL_BYPASS_MAX_MODULES: usize = 256;
pub const CELL_BYPASS_MAX_EDGES: usize = 2048;

/// The trusted native-cell resolver admits these IDs only for a registered donor plan.
pub fn is_native_cell(id: &str) -> bool {
  id.strip_prefix(NATIVE_CELL_PREFIX).is_some_and(|donor| {
    !donor.is_empty()
      && donor.len() % 2 == 0
      && donor.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
  })
}

pub fn enabled(options: &NormalizedBundlerOptions) -> bool {
  (options.experimental.is_resolve_new_url_to_asset_enabled()
    || options.experimental.is_incremental_build_enabled()
    || options.is_dev_mode_enabled())
    && options.preserve_modules
    && !options.is_strict_execution_order_enabled()
    && matches!(options.format, OutputFormat::Esm)
}

pub fn runtime_module_semantics_enabled(options: &NormalizedBundlerOptions) -> bool {
  options.experimental.is_resolve_new_url_to_asset_enabled()
    && (options.is_strict_execution_order_enabled()
      || options.preserve_modules
      || options.is_dev_mode_enabled()
      || options.experimental.is_incremental_build_enabled())
}

/// Whether a native record leaves out the target of a record of `kind`, which names a module in
/// `import.meta.hot.accept()` without importing it: accepting a module never runs it, as in a
/// production build, which has no `import.meta.hot`, and so no patch has to run it either.
pub fn accept_only(options: &NormalizedBundlerOptions, kind: rolldown_common::ImportKind) -> bool {
  kind == rolldown_common::ImportKind::HotAccept && enabled(options)
}

pub fn has_native_namespace(
  options: &NormalizedBundlerOptions,
  module: &NormalModule,
  linking_info: &LinkingMetadata,
  symbol_db: &SymbolRefDb,
) -> bool {
  enabled(options)
    && !module.exports_kind.is_commonjs()
    && linking_info.wrap_kind() == WrapKind::None
    && !linking_info.has_dynamic_exports
    && linking_info.star_exports_from_external_modules.is_empty()
    && !super::import_options::is_json_module(module)
    && !reexports_commonjs_property(linking_info, symbol_db)
}

/// Whether an export reads a property of a CommonJS module's exports. A native record could
/// only declare it as a copied snapshot and would leak the interop binding, so the module
/// keeps its runtime namespace object, whose getters read the property live.
fn reexports_commonjs_property(linking_info: &LinkingMetadata, symbol_db: &SymbolRefDb) -> bool {
  match linking_info.reexports_commonjs_property_cache.load(std::sync::atomic::Ordering::Relaxed) {
    1 => false,
    2 => true,
    _ => {
      let res = linking_info.resolved_exports.values().any(|export| {
        export.came_from_commonjs
          || symbol_db.get(symbol_db.canonical_ref_for(export.symbol_ref)).namespace_alias.is_some()
      });
      linking_info
        .reexports_commonjs_property_cache
        .store(if res { 2 } else { 1 }, std::sync::atomic::Ordering::Relaxed);
      res
    }
  }
}

/// Whether `symbol` is the namespace object of a module exposing its native namespace.
pub fn is_native_namespace(
  options: &NormalizedBundlerOptions,
  link_output: &LinkStageOutput,
  symbol: SymbolRef,
) -> bool {
  if symbol.owner == link_output.runtime.id() {
    return false;
  }
  let Module::Normal(module) = &link_output.module_table[symbol.owner] else {
    return false;
  };
  module.namespace_object_ref == symbol
    && has_native_namespace(
      options,
      module,
      &link_output.metas[symbol.owner],
      &link_output.symbol_db,
    )
}

/// A leaf donor can remain available for later direct demand without being evaluated through a
/// star that contributes no namespace binding. Only the existing side-effects certificate permits
/// this omission; shadowing/ambiguity alone says nothing about evaluation. No donor body is removed.
pub fn omittable_star_evaluation(
  options: &NormalizedBundlerOptions,
  link_output: &LinkStageOutput,
  module: &NormalModule,
  record_idx: ImportRecordIdx,
) -> bool {
  let record = &module.import_records[record_idx];
  if record.kind != ImportKind::Import
    || !record.meta.contains(ImportRecordMeta::IsExportStar)
    || module.meta.has_eval()
  {
    return false;
  }
  let Some(donor) =
    record.resolved_module.and_then(|idx| link_output.module_table[idx].as_normal())
  else {
    return false;
  };
  let donor_meta = &link_output.metas[donor.idx];
  if donor.side_effects.has_side_effects()
    || donor.meta.has_eval()
    || !donor.import_records.is_empty()
    || donor_meta.is_tla_or_contains_tla_dependency
  {
    return false;
  }
  let meta = &link_output.metas[module.idx];
  if !has_native_namespace(options, module, meta, &link_output.symbol_db)
    || module.named_imports.values().any(|import| import.record_idx == record_idx)
    || meta.star_export_record_by_name.iter().any(|(name, origin)| {
      *origin == record_idx && meta.sorted_and_non_ambiguous_resolved_exports.contains_key(name)
    })
  {
    return false;
  }
  !meta.canonical_exports(false).any(|(_, export)| {
    link_output.symbol_db.canonical_ref_for(export.symbol_ref).owner == donor.idx
  }) && has_native_namespace(options, donor, donor_meta, &link_output.symbol_db)
}

/// Dependencies of a cell-only facade evaluation omission. The declarations and facade exports
/// are not removed: named bindings or fully resolved namespace reads must already have canonical
/// owners in trusted cell records.
/// HMR must invalidate this certificate when any returned module or the importer changes.
pub fn native_cell_evaluation_dependencies(
  options: &NormalizedBundlerOptions,
  link_output: &LinkStageOutput,
  asts: &IndexEcmaAst,
  importer: &NormalModule,
  record_idx: ImportRecordIdx,
) -> Option<Vec<ModuleIdx>> {
  let record = &importer.import_records[record_idx];
  if record.kind != ImportKind::Import
    || record.meta.contains(ImportRecordMeta::IsExportStar)
    || importer.meta.has_eval()
  {
    return None;
  }
  let target = record.resolved_module?;
  let target_module = link_output.module_table[target].as_normal()?;
  let target_meta = &link_output.metas[target];
  if target_module.import_records.is_empty()
    || target_module.side_effects.has_side_effects()
    || target_module.meta.has_eval()
    || target_meta.is_tla_or_contains_tla_dependency
    || target_meta.resolved_exports.len()
      != target_meta.sorted_and_non_ambiguous_resolved_exports.len()
  {
    return None;
  }
  let importer_meta = &link_output.metas[importer.idx];
  let has_potential_cell = importer.named_imports.iter().any(|(binding, import)| {
    import.record_idx == record_idx
      && match &import.imported {
        Specifier::Star => true,
        Specifier::Literal(name) => {
          name != "default" && {
            let canonical = link_output.symbol_db.canonical_ref_for(*binding);
            canonical.owner != target
              && link_output.module_table[canonical.owner]
                .as_normal()
                .is_some_and(|owner| is_native_cell(owner.id.as_str()))
          }
        }
      }
  });
  if !has_potential_cell
    || !has_native_namespace(options, importer, importer_meta, &link_output.symbol_db)
    || !has_native_namespace(options, target_module, target_meta, &link_output.symbol_db)
  {
    return None;
  }
  let scoping = link_output.symbol_db[importer.idx].as_ref()?.ast_scopes.scoping();
  let mut live_cell = false;
  for (binding, import) in &importer.named_imports {
    if import.record_idx != record_idx {
      continue;
    }
    if matches!(&import.imported, Specifier::Literal(name) if name == "default") {
      return None;
    }
    // A different consumer's use of the same canonical export must not revive an unused local.
    if scoping.get_resolved_references(binding.symbol).next().is_none() {
      continue;
    }
    match &import.imported {
      Specifier::Literal(name) => {
        if name == "default" {
          return None;
        }
        if !native_owner(link_output, asts, *binding, target)? {
          continue;
        }
      }
      Specifier::Star => {
        let mut covered = FxHashSet::default();
        let mut star_cell = false;
        for statement in link_output.stmt_infos[importer.idx].iter() {
          for reference in &statement.referenced_symbols {
            match reference {
              SymbolOrMemberExprRef::Symbol(symbol) if symbol == binding => return None,
              SymbolOrMemberExprRef::MemberExpr(member) if member.object_ref == *binding => {
                let resolution = member.resolution(&importer_meta.resolved_member_expr_refs)?;
                if member.is_write
                  || member.prop_and_span_list.len() != 1
                  || member.prop_and_span_list[0].name == "default"
                  || !resolution.prop_and_related_span_list.is_empty()
                  || resolution.target_commonjs_exported_symbol.is_some()
                {
                  return None;
                }
                star_cell |= native_owner(link_output, asts, resolution.resolved?, target)?;
                covered.insert(member.reference_id?);
              }
              _ => {}
            }
          }
        }
        // Semantic coverage also rejects opaque uses not represented by a scanner member ref.
        if !star_cell
          || covered.is_empty()
          || !scoping
            .get_resolved_reference_ids(binding.symbol)
            .iter()
            .all(|reference| covered.contains(reference))
        {
          return None;
        }
      }
    }
    live_cell = true;
  }
  if !live_cell {
    return None;
  }

  // Iterative, bounded DFS: a missed certificate retains normal native evaluation.
  let mut state = FxHashMap::default();
  let mut pending = vec![(target, false)];
  let mut edges = 0;
  let mut cyclic = false;
  while let Some((idx, leaving)) = pending.pop() {
    if leaving {
      state.insert(idx, 2_u8);
      continue;
    }
    match state.get(&idx) {
      Some(1) => {
        cyclic = true;
        continue;
      }
      Some(2) => continue,
      _ => {}
    }
    if idx == importer.idx || state.len() >= CELL_BYPASS_MAX_MODULES {
      return None;
    }
    let module = link_output.module_table[idx].as_normal()?;
    let meta = &link_output.metas[idx];
    if module.side_effects.has_side_effects()
      || module.meta.has_eval()
      || meta.is_tla_or_contains_tla_dependency
      || !has_native_namespace(options, module, meta, &link_output.symbol_db)
      || meta.resolved_exports.len() != meta.sorted_and_non_ambiguous_resolved_exports.len()
    {
      return None;
    }
    state.insert(idx, 1);
    pending.push((idx, true));
    for dependency in &module.import_records {
      edges += 1;
      if edges > CELL_BYPASS_MAX_EDGES {
        return None;
      }
      if dependency.kind == ImportKind::Import {
        pending.push((dependency.resolved_module?, false));
      } else {
        // This first slice proves no dynamic/require-induced evaluation or resolution obligations.
        return None;
      }
    }
  }
  if cyclic {
    // After traversal, over the whole closure: a module admitted before the DFS found a back
    // edge is as much a cycle member as the endpoints, and cross edges can hide other members of
    // an SCC. Binding-only nonleaves neither initialize nor read temporal bindings. Leaves keep
    // their original records and initialization order on later facade demand. A record whose
    // only imports are its own import-free cells is such a leaf: no cycle can pass through it.
    for &idx in state.keys() {
      let module = link_output.module_table[idx].as_normal()?;
      let leaf = module.import_records.iter().all(|record| {
        record
          .resolved_module
          .and_then(|cell| link_output.module_table[cell].as_normal())
          .is_some_and(|cell| is_native_cell(cell.id.as_str()) && cell.import_records.is_empty())
      });
      if !leaf && !asts.get(idx)?.as_ref()?.program().body.iter().all(binding_only_statement) {
        return None;
      }
    }
  }
  let mut dependencies: Vec<_> = state.into_keys().collect();
  dependencies.push(importer.idx);
  dependencies.sort_unstable();
  dependencies.dedup();
  Some(dependencies)
}

fn binding_only_statement(statement: &Statement<'_>) -> bool {
  match statement {
    Statement::ImportDeclaration(import) => {
      import.specifiers.as_ref().is_some_and(|specifiers| !specifiers.is_empty())
    }
    Statement::ExportFromDeclaration(export) => !export.specifiers.is_empty(),
    Statement::ExportAllDeclaration(_)
    | Statement::ExportNamedDeclaration(_)
    | Statement::EmptyStatement(_) => true,
    _ => false,
  }
}

/// The canonical owner of an imported binding a cell bypass may bind directly: `Some(true)` for
/// a native cell, `Some(false)` for a literal record (`is_literal_record`). A bypass needs at
/// least one cell binding; HMR classifies only records reaching a cell.
fn native_owner(
  link_output: &LinkStageOutput,
  asts: &IndexEcmaAst,
  binding: SymbolRef,
  target: ModuleIdx,
) -> Option<bool> {
  let canonical = link_output.symbol_db.canonical_ref_for(binding);
  if canonical.owner == target {
    return None;
  }
  let owner = link_output.module_table[canonical.owner].as_normal()?;
  if is_native_cell(owner.id.as_str()) {
    return Some(true);
  }
  if canonical == owner.namespace_object_ref {
    return None;
  }
  is_literal_record(owner, asts.get(owner.idx)?.as_ref()?.program()).then_some(false)
}

/// An import-free ECMAScript record whose whole body declares exported primitive-literal
/// constants. Its evaluation cannot throw, read another binding or be observed, and it has
/// completed before any importer runs, so binding an importer straight to it keeps every
/// observable of evaluating a donor that re-exports it. The declaration itself is kept and read
/// live; no value is folded.
pub fn is_literal_record(module: &NormalModule, program: &oxc::ast::ast::Program<'_>) -> bool {
  use oxc::ast::ast::{BindingPattern, Declaration, VariableDeclarationKind};
  module.import_records.is_empty()
    && !module.meta.has_eval()
    && module.exports_kind == rolldown_common::ExportsKind::Esm
    && !crate::utils::import_options::is_json_module(module)
    && program.body.iter().any(|statement| !matches!(statement, Statement::EmptyStatement(_)))
    && program.body.iter().all(|statement| match statement {
      Statement::EmptyStatement(_) => true,
      Statement::ExportDeclaration(export) => {
        matches!(&export.declaration, Declaration::VariableDeclaration(declaration)
        if declaration.kind == VariableDeclarationKind::Const
          && !declaration.declare
          && declaration.declarations.iter().all(|declarator| {
            matches!(declarator.id, BindingPattern::BindingIdentifier(_))
              && declarator
                .init
                .as_ref()
                .is_some_and(crate::utils::native_cell_plan::is_primitive_literal)
          }))
      }
      _ => false,
    })
}

/// Whether `module`'s record keeps an authored anonymous `export default function`/`class`
/// declaration so the engine itself names it `"default"`; the record reads the binding back
/// through a self-import instead of a renamed local plus `export { local as default }`. An
/// `export default <expression>` instead prints as a writable `let` (see
/// `LinkingMetadata::native_default_expression`).
pub fn has_native_default_export(
  options: &NormalizedBundlerOptions,
  module: &NormalModule,
  linking_info: &LinkingMetadata,
  symbol_db: &SymbolRefDb,
  record_entry: Option<rolldown_common::ModuleIdx>,
) -> bool {
  record_entry == Some(module.idx)
    && !linking_info.native_default_expression
    && has_native_namespace(options, module, linking_info, symbol_db)
    && symbol_db.is_facade_symbol(module.default_export_ref)
}

/// The chunk's modules, then under native records every module that a chunk module imports
/// without loading its chunk (a dynamic import target, certified cell route, or omitted star
/// evaluation), and transitively through static imports. The HMR walk propagates through
/// unevaluated donors to the evaluated importers bound through them, and the client checks an
/// eager dynamic import target's static closure before loading its records (#647), so their
/// edges must be known before an update.
pub fn dev_graph_rows(
  chunk_idx: ChunkIdx,
  chunk: &Chunk,
  module_table: &rolldown_common::ModuleTable,
  chunk_graph: &crate::chunk_graph::ChunkGraph,
  metas: &crate::types::linking_metadata::LinkingMetadataVec,
  native_records: bool,
) -> Vec<ModuleIdx> {
  let mut rows: Vec<ModuleIdx> = chunk.modules.clone();
  if !native_records {
    return rows;
  }
  let own = rows.len();
  let has_deferred_imports =
    chunk.modules.iter().any(|idx| !metas[*idx].deferred_imports.is_empty());
  let mut seen: FxHashSet<ModuleIdx> = rows.iter().copied().collect();
  let mut next = 0;
  while next < rows.len() {
    let is_own = next < own;
    let row = rows[next];
    next += 1;
    let Some(module) = module_table[row].as_normal() else {
      continue;
    };
    for record in &module.import_records {
      if !(record.kind.is_static() || (is_own && record.kind.is_dynamic())) {
        continue;
      }
      let Some(target) = record.resolved_module else {
        continue;
      };
      if record.kind.is_dynamic()
        && module_table[target].id().as_str().contains("?rolldown-lazy=native")
      {
        continue;
      }
      let Some(target_chunk) = chunk_graph.module_to_chunk[target] else {
        continue;
      };
      let eagerly_imported =
        !has_deferred_imports && chunk.imports_from_other_chunks.contains_key(&target_chunk);
      if target_chunk != chunk_idx
        && ((is_own && record.kind.is_dynamic()) || !eagerly_imported)
        && seen.insert(target)
      {
        rows.push(target);
      }
    }
  }
  rows
}
