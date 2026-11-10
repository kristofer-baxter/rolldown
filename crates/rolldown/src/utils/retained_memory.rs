//! Right-sizes what an incremental build retains for every module until the session ends.
//!
//! A scan parses each module into an arena that starts at 16 KiB and doubles as it fills, and
//! leaves in it the nodes that the parser, semantic analysis and transforms discarded; the
//! module's scope tree has an arena of its own. A one-shot build drops both after rendering, but
//! an incremental build keeps them for later links, HMR patches and lazy compilation, so most of
//! what it retains for a graph of small modules is arena capacity that nothing uses. Its module
//! tasks replace both with copies in arenas sized to what the copies hold, unless a copy would
//! not be smaller. A copy keeps every node, scope, symbol and reference id, as the copy that each
//! render finalizes does. The link output that fast relinks reuse keeps its tables likewise
//! without the capacity that linking left in them.

use std::cell::RefCell;
use std::ptr;

use oxc::allocator::{Allocator, CloneIn};
use rayon::prelude::*;
use rolldown_common::{
  EcmaView, ModuleIdx, StmtInfos, SymbolOrMemberExprRef, SymbolRefDbForModule,
};
use rolldown_ecmascript::EcmaAst;

use crate::stages::link_stage::LinkStageOutput;

/// The largest scratch arena that a thread keeps between modules.
const SCRATCH_CAPACITY_LIMIT: usize = 256 * 1024;

thread_local! {
  static SCRATCH: RefCell<Allocator> = RefCell::default();
}

/// Replaces a module's AST and scope tree with copies in right-sized arenas, where they are
/// smaller.
pub fn compact_module_arenas(ast: &mut EcmaAst, symbols: &mut SymbolRefDbForModule) {
  compact_ast(ast);
  // Unlike the parser, the scope tree's builder copied every name into the tree's own arena, so
  // a copy holds nothing that the tree did not, in an arena sized by what the tree's arena used,
  // and never takes more memory than the arena that it replaces.
  let scoping = symbols.ast_scopes.scoping().clone_in_with_semantic_ids_with_another_arena();
  symbols.ast_scopes.set_scoping(scoping);
  symbols.classic_data.raw.shrink_to_fit();
  symbols.flags.shrink_to_fit();
}

/// Drops the excess capacity that scanning left in a module's view, symbol database, and
/// statement infos before `ModuleInfo` and the scan-stage snapshot clone them.
pub fn shrink_scanned_module(
  ecma_view: &mut EcmaView,
  symbols: &mut SymbolRefDbForModule,
  stmt_infos: &mut StmtInfos,
) {
  ecma_view.named_imports.shrink_to_fit();
  ecma_view.named_exports.shrink_to_fit();
  ecma_view.imports.shrink_to_fit();
  ecma_view.imported_ids.shrink_to_fit();
  ecma_view.dynamically_imported_ids.shrink_to_fit();
  ecma_view.hmr_info.deps.shrink_to_fit();
  ecma_view.sourcemap_chain.shrink_to_fit();
  ecma_view.self_referenced_class_decl_symbol_ids.shrink_to_fit();
  ecma_view.new_url_references.shrink_to_fit();
  ecma_view.namespace_in_checks.shrink_to_fit();
  ecma_view.rolldown_file_url_references.shrink_to_fit();
  ecma_view.this_expr_replace_map.shrink_to_fit();
  ecma_view.directive_range.shrink_to_fit();
  ecma_view.dummy_record_set.shrink_to_fit();
  ecma_view.constant_export_map.shrink_to_fit();
  ecma_view.enum_member_value_map.shrink_to_fit();
  ecma_view.import_attribute_map.shrink_to_fit();
  ecma_view.cjs_reexport_import_record_ids.shrink_to_fit();
  ecma_view.dynamic_import_called_names.shrink_to_fit();
  symbols.classic_data.raw.shrink_to_fit();
  symbols.flags.shrink_to_fit();
  for stmt_info in stmt_infos.infos.iter_mut() {
    stmt_info.declared_symbols.shrink_to_fit();
    for sym_or_mem in &mut stmt_info.referenced_symbols {
      if let SymbolOrMemberExprRef::MemberExpr(mem) = sym_or_mem {
        mem.prop_and_span_list.shrink_to_fit();
      }
    }
    stmt_info.referenced_symbols.shrink_to_fit();
    stmt_info.import_records.shrink_to_fit();
  }
  stmt_infos.infos.raw.shrink_to_fit();
}

/// Drops the excess capacity in a module's importer sets and import records after
/// `rebuild_importer_sets` populates them.
pub fn shrink_ecma_view_importers(ecma_view: &mut EcmaView) {
  ecma_view.import_records.raw.shrink_to_fit();
  ecma_view.importers.shrink_to_fit();
  ecma_view.importers_idx.shrink_to_fit();
  ecma_view.dynamic_importers.shrink_to_fit();
  ecma_view.dynamic_importers_idx.shrink_to_fit();
}

/// Replaces `ast` with a copy in an arena that the copy fills, unless that arena would be no
/// smaller than the AST's own. The parser borrows every name and unescaped literal from the
/// module's source, which a copy holds in its own arena, so a module of large literals keeps its
/// parse. `EcmaAst::clone_with_another_arena` sizes the arena by what the AST's own arena used,
/// discarded nodes included, so a first copy into a scratch arena that the thread reuses
/// measures the AST instead; a scratch arena that a large AST grew is dropped. A copy points its
/// program's source text at the module's source, as the parse does, rather than holding it.
/// Returns the arena capacity released (`0` when the AST was kept as-is).
pub fn compact_ast(ast: &mut EcmaAst) -> usize {
  let own_source = ast.program.with_mut(|fields| {
    let own_source = ptr::eq(fields.program.source_text, fields.source.as_str());
    if own_source {
      fields.program.source_text = "";
    }
    own_source
  });
  let size = SCRATCH.with_borrow_mut(|scratch| {
    let _ = ast.program().clone_in_with_semantic_ids(scratch);
    let size = scratch.used_bytes();
    if scratch.capacity() > SCRATCH_CAPACITY_LIMIT {
      *scratch = Allocator::default();
    } else {
      scratch.reset();
    }
    size
  });
  let retained = ast.program.borrow_owner().allocator.capacity();
  let allocator = (size < retained).then(|| Allocator::with_capacity(size));
  let mut released = 0;
  if let Some(allocator) = allocator.filter(|allocator| allocator.capacity() < retained) {
    let mut copy =
      EcmaAst::from_allocator_and_source(ast.source().clone(), allocator, |allocator| {
        ast.program().clone_in_with_semantic_ids(allocator)
      });
    let post_cap = copy.program.borrow_owner().allocator.capacity();
    copy.source_type = ast.source_type;
    *ast = copy;
    released = retained.saturating_sub(post_cap);
  }
  if own_source {
    ast.program.with_mut(|fields| fields.program.source_text = fields.source.as_str());
  }
  released
}

/// Drops the capacity that linking left in each module's statement, symbol, and linking metadata
/// tables. The tables indexed by module keep the room that fast relinks append modules into.
pub fn shrink_link_tables(link_output: &mut LinkStageOutput) {
  link_output.stmt_infos.raw.par_iter_mut().for_each(|stmt_infos| {
    for stmt_info in &mut stmt_infos.infos {
      stmt_info.declared_symbols.shrink_to_fit();
      for sym_or_mem in &mut stmt_info.referenced_symbols {
        if let SymbolOrMemberExprRef::MemberExpr(mem) = sym_or_mem {
          mem.prop_and_span_list.shrink_to_fit();
        }
      }
      stmt_info.referenced_symbols.shrink_to_fit();
      stmt_info.import_records.shrink_to_fit();
    }
    stmt_infos.infos.raw.shrink_to_fit();
  });
  link_output.metas.raw.par_iter_mut().for_each(|meta| {
    meta.dependencies = Default::default();
    meta.load_dependencies = Default::default();
    meta.execution_dependencies = Default::default();
    meta.native_reexport_by_canonical = std::sync::OnceLock::new();
    meta.resolved_exports.shrink_to_fit();
    meta.sorted_and_non_ambiguous_resolved_exports.shrink_to_fit();
    meta.referenced_symbols_by_entry_point_chunk.shrink_to_fit();
    for res in meta.resolved_member_expr_refs.values_mut() {
      res.prop_and_related_span_list.shrink_to_fit();
      res.depended_refs.shrink_to_fit();
    }
    meta.resolved_member_expr_refs.shrink_to_fit();
    meta.folded_namespace_in_checks.shrink_to_fit();
    meta.named_import_to_cjs_module.shrink_to_fit();
    meta.import_record_ns_to_cjs_module.shrink_to_fit();
    meta.star_exports_from_external_modules.shrink_to_fit();
    meta.shimmed_missing_exports.shrink_to_fit();
    meta.temporal_symbols.shrink_to_fit();
    meta.deferred_imports.shrink_to_fit();
  });
  link_output.module_table.modules.raw.par_iter_mut().for_each(|module| {
    if let Some(normal) = module.as_normal_mut() {
      // `imported_ids` and `dynamically_imported_ids` are only consumed by `to_module_info` on
      // the scan-stage snapshot, never on `link_output.module_table`.
      normal.ecma_view.imported_ids = Default::default();
      normal.ecma_view.dynamically_imported_ids = Default::default();
      normal.ecma_view.mutations.shrink_to_fit();
    }
  });
  for index in 0..link_output.symbol_db.inner().len() {
    if let Some(symbols) = link_output.symbol_db[ModuleIdx::from_usize(index)].as_mut() {
      symbols.classic_data.raw.shrink_to_fit();
      symbols.flags.shrink_to_fit();
    }
  }
  link_output.global_constant_symbol_map.shrink_to_fit();
  link_output.normal_symbol_exports_chain_map.shrink_to_fit();
  link_output.safely_merge_cjs_ns_map.shrink_to_fit();
}
