use rolldown_common::{SymbolOrMemberExprRef, SymbolRef, WrapKind};
use rustc_hash::FxHashSet;

use super::GenerateStage;
use super::order_analysis::OrderWrapPlan;

impl GenerateStage<'_> {
  /// Leave the temporal dead zone of unwrapped modules to the engine, in a build without strict
  /// execution order (the link stage marks no candidate in a strict one).
  ///
  /// A module that no wrapper hoists keeps its top-level `let`, `const` and `class` declarations
  /// in the chunk's module scope, and another chunk reads them through a live `import` binding.
  /// The engine then throws on every read before initialization that the minifier keeps, as it
  /// does in Rollup's output, so the flags and guards that the link stage planned for the module
  /// are dead weight. A wrapped module's declarations become hoisted `var`s and keep them.
  ///
  /// Must run after the last execution-order analysis, which fixes the set of order wrappers,
  /// and before anything derives links or synthetic statements from the flags.
  pub(super) fn enforce_unwrapped_temporal_bindings_natively(
    &mut self,
    plan: Option<&OrderWrapPlan>,
  ) {
    let link_output = &mut *self.link_output;
    let mut released = FxHashSet::default();
    for (module_idx, meta) in link_output.metas.iter_mut_enumerated() {
      if !meta.engine_temporal {
        continue;
      }
      if !matches!(meta.wrap_kind(), WrapKind::None)
        || plan.is_some_and(|plan| plan.contains(&module_idx))
      {
        meta.engine_temporal = false;
        continue;
      }
      released.extend(meta.temporal_symbols.drain().map(|(_, flag)| flag));
      meta.temporal_constants.clear();
    }
    if released.is_empty() {
      return;
    }
    let guarded: FxHashSet<SymbolRef> =
      link_output.metas.iter().flat_map(|meta| meta.temporal_symbols.values().copied()).collect();
    let check = link_output.runtime.resolve_symbol("__checkTDZ");
    let is_symbol = |reference: &SymbolOrMemberExprRef, set: &FxHashSet<SymbolRef>| matches!(reference, SymbolOrMemberExprRef::Symbol(symbol) if set.contains(symbol));
    let runtime = link_output.runtime.id();
    for (module_idx, infos) in link_output.stmt_infos.iter_mut_enumerated() {
      if module_idx == runtime {
        continue;
      }
      for info in &mut infos.infos {
        info.referenced_symbols.retain(|reference| !is_symbol(reference, &released));
        // A statement names the helper only for the flags beside it.
        if !info.referenced_symbols.iter().any(|reference| is_symbol(reference, &guarded)) {
          info.referenced_symbols.retain(
            |reference| !matches!(reference, SymbolOrMemberExprRef::Symbol(symbol) if *symbol == check),
          );
        }
      }
    }
    // With no flag left, nothing can call the helper that tree shaking kept for the guards.
    if guarded.is_empty() {
      for statement in link_output.stmt_infos[runtime].declared_stmts_by_symbol(&check).to_vec() {
        link_output.metas[runtime].stmt_info_included.clear_bit(statement);
      }
    }
  }
}
