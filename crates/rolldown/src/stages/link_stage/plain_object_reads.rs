//! Property-level tree shaking for top-level `const` object literals.
//!
//! ```js
//! // tokens.js
//! export const tokens = { brand: '#0f6cbd', legacy: 'unused' };
//! // styles.js
//! export const live = /*#__PURE__*/ makeStyles({ color: tokens.brand });
//! export const dead = /*#__PURE__*/ makeStyles({ color: tokens.legacy });
//! ```
//!
//! Reading an own data property of a plain object literal has no side effect, so `dead` can be
//! dropped once the analyzer knows `tokens.legacy` is such a read. With `dead` gone nothing reads
//! `legacy`, and the property itself can be dropped from `tokens`.
//!
//! Both steps need the object to stay the literal it was declared as: no code may reach the object
//! other than through static property reads, so nothing can add a getter, delete a property, change
//! its prototype, enumerate its keys or call one of its methods with the object as `this`. Every
//! other use of the binding, its export from an entry, a namespace or dynamic import of a module
//! that exports it, CommonJS, `eval` and wrapping all make the object escape.

use oxc::ast::ast::{
  BindingPattern, CallExpression, ChainElement, Declaration, ExportNamedDeclaration, Expression,
  IdentifierReference, MemberExpression, ObjectPropertyKind, PropertyKey, PropertyKind, Statement,
  TaggedTemplateExpression, UnaryExpression, UnaryOperator, VariableDeclaration,
  VariableDeclarationKind,
};
use oxc::ast_visit::{VisitJs, walk_js};
use oxc::semantic::SymbolId;
use oxc_str::CompactStr;
use rolldown_common::{
  GetLocalDb, ImportKind, ModuleIdx, ModuleType, ResolvedExport, Specifier, StmtInfoIdx, SymbolRef,
  WrapKind,
};
use rustc_hash::{FxHashMap, FxHashSet};

use super::LinkStage;
use crate::types::linking_metadata::LinkingMetadataVec;
use crate::utils::temporal_reads::temporal_read_argument;

/// The key of `object.key`, `object['key']` or ``object[`key`]``. Other computed keys go through
/// `ToPropertyKey`, which can run code and yield a different key.
pub(crate) fn static_read_key<'a>(member: &'a MemberExpression<'a>) -> Option<&'a str> {
  match member {
    MemberExpression::StaticMemberExpression(member) => Some(member.property.name.as_str()),
    MemberExpression::ComputedMemberExpression(member) => match &member.expression {
      Expression::StringLiteral(literal) => literal.value.as_str(),
      Expression::TemplateLiteral(template) if template.quasis.len() == 1 => {
        template.quasis[0].value.cooked.as_ref().and_then(|cooked| cooked.as_str())
      }
      _ => None,
    },
    MemberExpression::PrivateFieldExpression(_) => None,
  }
}

/// A top-level `const` bound to an object literal that never escapes.
#[derive(Debug, Default)]
pub struct PlainObject {
  /// Own data property names. `true` when the property's value is a literal, so dropping the
  /// property drops no side effect.
  pub keys: FxHashMap<CompactStr, bool>,
}

/// The objects that only static property reads reach, and where those reads are.
#[derive(Debug, Default)]
pub struct PlainObjectReads {
  /// Keyed by the declaring binding.
  pub objects: FxHashMap<SymbolRef, PlainObject>,
  /// Static property reads per module: the reading top-level statement, object and property.
  pub reads: FxHashMap<ModuleIdx, Vec<(StmtInfoIdx, SymbolRef, CompactStr)>>,
  /// Modules whose edit can change `objects` or `reads`: the declaring modules, the modules that
  /// reference a candidate and the modules that import from a module exporting one.
  pub guarded: FxHashSet<ModuleIdx>,
}

impl PlainObjectReads {
  /// Writes into each declaring module's metadata the literal properties that no included
  /// statement reads. Run once statement inclusion is final, before chunk reuse and finalization.
  pub fn plan_pruning(&self, metas: &mut LinkingMetadataVec) {
    let plan = self.compute_plan(metas);
    for meta in metas.iter_mut() {
      meta.pruned_object_properties.clear();
    }
    for (object, removable) in plan {
      metas[object.owner].pruned_object_properties.insert(object.symbol, removable);
    }
  }

  /// Whether `metas` already holds the plan that their statement inclusion implies.
  pub fn plan_is_current(&self, metas: &LinkingMetadataVec) -> bool {
    let plan = self.compute_plan(metas);
    let planned = metas.iter().map(|meta| meta.pruned_object_properties.len()).sum::<usize>();
    planned == plan.len()
      && plan.iter().all(|(object, removable)| {
        metas[object.owner].pruned_object_properties.get(&object.symbol) == Some(removable)
      })
  }

  /// Whether an edit of `module_idx` can change what this analysis found.
  pub fn guards(&self, module_idx: ModuleIdx) -> bool {
    self.guarded.contains(&module_idx)
  }

  fn compute_plan(
    &self,
    metas: &LinkingMetadataVec,
  ) -> FxHashMap<SymbolRef, FxHashSet<CompactStr>> {
    let mut live: FxHashMap<SymbolRef, FxHashSet<&CompactStr>> = FxHashMap::default();
    for (module_idx, reads) in &self.reads {
      for (stmt_idx, object, property) in reads {
        if metas[*module_idx].stmt_info_included.has_bit(*stmt_idx) {
          live.entry(*object).or_default().insert(property);
        }
      }
    }
    let mut plan = FxHashMap::default();
    for (object, plain) in &self.objects {
      if !metas[object.owner].is_included {
        continue;
      }
      let live = live.get(object);
      let removable = plain
        .keys
        .iter()
        .filter(|(key, prunable)| **prunable && !live.is_some_and(|live| live.contains(key)))
        .map(|(key, _)| key.clone())
        .collect::<FxHashSet<_>>();
      if !removable.is_empty() {
        plan.insert(*object, removable);
      }
    }
    plan
  }
}

impl LinkStage<'_> {
  #[tracing::instrument(level = "debug", skip_all)]
  pub(super) fn collect_plain_object_reads(&mut self) {
    self.plain_object_reads = PlainObjectReads::default();
    if self.options.treeshake.is_none() || self.options.is_dev_mode_enabled() {
      return;
    }

    let mut objects = FxHashMap::default();
    for module in self.module_table.modules.iter().filter_map(|module| module.as_normal()) {
      if !matches!(
        module.module_type,
        ModuleType::Js | ModuleType::Jsx | ModuleType::Ts | ModuleType::Tsx
      ) || module.exports_kind.is_commonjs()
        || module.meta.has_eval()
        || matches!(self.metas[module.idx].wrap_kind(), WrapKind::Cjs)
      {
        continue;
      }
      let Some(ast) = self.ast_table[module.idx].as_ref() else {
        continue;
      };
      ast.program.with_dependent(|_owner, dep| {
        if self.stmt_infos[module.idx].len() < dep.program.body.len() + 1 {
          return;
        }
        for stmt in &dep.program.body {
          let declaration = match stmt {
            Statement::VariableDeclaration(declaration) => declaration,
            Statement::ExportDeclaration(export) => match &export.declaration {
              Declaration::VariableDeclaration(declaration) => declaration,
              _ => continue,
            },
            _ => continue,
          };
          collect_declared_objects(module.idx, declaration, &mut objects);
        }
      });
    }
    if objects.is_empty() {
      return;
    }

    let mut escaped = FxHashSet::default();
    self.escape_observable_exports(&objects, &mut escaped);

    // Walk the declaring modules and every module importing a candidate.
    let mut walks: FxHashMap<ModuleIdx, FxHashMap<SymbolId, SymbolRef>> = FxHashMap::default();
    for object in objects.keys() {
      walks.entry(object.owner).or_default().insert(object.symbol, *object);
    }
    for module in self.module_table.modules.iter().filter_map(|module| module.as_normal()) {
      for local in module.named_imports.keys() {
        let canonical = self.symbols.canonical_ref_for(*local);
        if objects.contains_key(&canonical) {
          walks.entry(module.idx).or_default().insert(local.symbol, canonical);
        }
      }
    }

    let mut guarded: FxHashSet<ModuleIdx> = walks.keys().copied().collect();
    let mut reads: FxHashMap<ModuleIdx, Vec<(StmtInfoIdx, SymbolRef, CompactStr)>> =
      FxHashMap::default();
    for (module_idx, locals) in walks {
      let module = self.module_table.modules[module_idx].as_normal();
      let ast = self.ast_table[module_idx].as_ref();
      let (Some(module), Some(ast)) = (module, ast) else {
        escaped.extend(locals.values().copied());
        continue;
      };
      if module.meta.has_eval() || module.exports_kind.is_commonjs() {
        escaped.extend(locals.values().copied());
        continue;
      }
      let ast_scopes = &self.symbols.local_db(module_idx).ast_scopes;
      let stmt_count = self.stmt_infos[module_idx].len();
      ast.program.with_dependent(|_owner, dep| {
        if stmt_count < dep.program.body.len() + 1 {
          escaped.extend(locals.values().copied());
          return;
        }
        let mut walker = ReadWalker {
          locals: &locals,
          symbol_for: |reference: &IdentifierReference<'_>| {
            reference.reference_id.get().and_then(|id| ast_scopes.symbol_id_for(id))
          },
          stmt_idx: StmtInfoIdx::from_raw_unchecked(0),
          reads: Vec::new(),
          escaped: &mut escaped,
        };
        for (body_idx, stmt) in dep.program.body.iter().enumerate() {
          // `stmt_infos[0]` is the namespace statement. The program body starts at index 1.
          walker.stmt_idx = StmtInfoIdx::new(body_idx + 1);
          walker.visit_statement(stmt);
        }
        if !walker.reads.is_empty() {
          reads.insert(module_idx, walker.reads);
        }
      });
    }

    // A read of a key the object does not own reaches the prototype chain, where an inherited
    // getter receives the whole object as `this`.
    for (_, object, key) in reads.values().flatten() {
      if objects.get(object).is_some_and(|candidate| !candidate.keys.contains_key(key)) {
        escaped.insert(*object);
      }
    }
    objects.retain(|object, _| !escaped.contains(object));
    if objects.is_empty() {
      return;
    }
    for module_reads in reads.values_mut() {
      module_reads.retain(|(_, object, _)| objects.contains_key(object));
    }
    reads.retain(|_, module_reads| !module_reads.is_empty());

    // An edit of a module importing from one that exports a candidate can start reading it.
    let exporters = self
      .metas
      .iter_enumerated()
      .filter(|(_, meta)| {
        meta
          .resolved_exports
          .values()
          .flat_map(export_refs)
          .any(|symbol_ref| objects.contains_key(&self.symbols.canonical_ref_for(*symbol_ref)))
      })
      .map(|(module_idx, _)| module_idx)
      .collect::<FxHashSet<_>>();
    for module in self.module_table.modules.iter().filter_map(|module| module.as_normal()) {
      if module
        .import_records
        .iter()
        .any(|record| record.resolved_module.is_some_and(|idx| exporters.contains(&idx)))
      {
        guarded.insert(module.idx);
      }
    }
    self.plain_object_reads = PlainObjectReads { objects, reads, guarded };
  }

  /// Marks escaped the candidates that code outside the bundle's static imports can reach: through
  /// an entry's exports, a namespace object, a dynamic import or `require`, or CommonJS.
  fn escape_observable_exports(
    &self,
    objects: &FxHashMap<SymbolRef, PlainObject>,
    escaped: &mut FxHashSet<SymbolRef>,
  ) {
    let mut non_static_importees = FxHashSet::default();
    for module in self.module_table.modules.iter().filter_map(|module| module.as_normal()) {
      for record in module.import_records.iter() {
        if record.kind != ImportKind::Import
          && let Some(importee) = record.resolved_module
        {
          non_static_importees.insert(importee);
        }
      }
      for named_import in module.named_imports.values() {
        if matches!(named_import.imported, Specifier::Star)
          && let Some(importee) = module.import_records[named_import.record_idx].resolved_module
        {
          non_static_importees.insert(importee);
        }
      }
    }

    for (module_idx, meta) in self.metas.iter_enumerated() {
      let observable = self.entries.contains_key(&module_idx)
        || self.user_defined_entry_modules.contains(&module_idx)
        || non_static_importees.contains(&module_idx)
        || self.module_table.modules[module_idx]
          .as_normal()
          .is_none_or(|module| module.exports_kind.is_commonjs());
      if !observable {
        continue;
      }
      for symbol_ref in meta.resolved_exports.values().flat_map(export_refs) {
        let canonical = self.symbols.canonical_ref_for(*symbol_ref);
        if objects.contains_key(&canonical) {
          escaped.insert(canonical);
        }
      }
    }
  }
}

/// Every binding an export may resolve to, including ambiguous and CommonJS-conflicting ones.
fn export_refs(export: &ResolvedExport) -> impl Iterator<Item = &SymbolRef> {
  std::iter::once(&export.symbol_ref)
    .chain(export.potentially_ambiguous_symbol_refs.iter().flat_map(|refs| refs.iter()))
    .chain(export.cjs_conflicting_symbol_refs.iter().flat_map(|refs| refs.iter()))
}

fn collect_declared_objects(
  module_idx: ModuleIdx,
  declaration: &VariableDeclaration<'_>,
  objects: &mut FxHashMap<SymbolRef, PlainObject>,
) {
  if declaration.kind != VariableDeclarationKind::Const || declaration.declare {
    return;
  }
  for declarator in &declaration.declarations {
    let BindingPattern::BindingIdentifier(id) = &declarator.id else {
      continue;
    };
    let Some(symbol_id) = id.symbol_id.get() else {
      continue;
    };
    let Some(Expression::ObjectExpression(object)) = &declarator.init else {
      continue;
    };
    let mut keys = FxHashMap::default();
    let mut plain = true;
    for property in &object.properties {
      let ObjectPropertyKind::ObjectProperty(property) = property else {
        plain = false;
        break;
      };
      if property.kind != PropertyKind::Init || property.method || property.computed {
        plain = false;
        break;
      }
      let key = match &property.key {
        PropertyKey::StaticIdentifier(name) => CompactStr::from(name.name.as_str()),
        PropertyKey::StringLiteral(literal) => match literal.value.as_str() {
          Some(value) => CompactStr::from(value),
          None => {
            plain = false;
            break;
          }
        },
        _ => {
          plain = false;
          break;
        }
      };
      if key == "__proto__" {
        plain = false;
        break;
      }
      let prunable = is_literal(&property.value);
      // A duplicate key keeps the last value but evaluates every one.
      keys.entry(key).and_modify(|existing: &mut bool| *existing &= prunable).or_insert(prunable);
    }
    if plain {
      objects.insert(SymbolRef::from((module_idx, symbol_id)), PlainObject { keys });
    }
  }
}

/// Literals whose evaluation has no side effect and which hold no reference to other code.
fn is_literal(expr: &Expression<'_>) -> bool {
  match expr {
    Expression::StringLiteral(_)
    | Expression::NumericLiteral(_)
    | Expression::BooleanLiteral(_)
    | Expression::NullLiteral(_)
    | Expression::BigIntLiteral(_) => true,
    Expression::TemplateLiteral(template) => template.expressions.is_empty(),
    Expression::UnaryExpression(unary) => {
      matches!(unary.operator, UnaryOperator::UnaryNegation | UnaryOperator::UnaryPlus)
        && matches!(unary.argument, Expression::NumericLiteral(_))
    }
    Expression::ArrayExpression(array) => array
      .elements
      .iter()
      .all(|element| element.as_expression().is_some_and(is_literal)),
    Expression::ObjectExpression(object) => object.properties.iter().all(|property| {
      matches!(property, ObjectPropertyKind::ObjectProperty(property)
        if property.kind == PropertyKind::Init
          && !property.computed
          && matches!(property.key, PropertyKey::StaticIdentifier(_) | PropertyKey::StringLiteral(_))
          && is_literal(&property.value))
    }),
    Expression::ParenthesizedExpression(paren) => is_literal(&paren.expression),
    _ => false,
  }
}

struct ReadWalker<'w, F> {
  locals: &'w FxHashMap<SymbolId, SymbolRef>,
  symbol_for: F,
  stmt_idx: StmtInfoIdx,
  reads: Vec<(StmtInfoIdx, SymbolRef, CompactStr)>,
  escaped: &'w mut FxHashSet<SymbolRef>,
}

impl<F: Fn(&IdentifierReference<'_>) -> Option<SymbolId>> ReadWalker<'_, F> {
  fn candidate(&self, expr: &Expression<'_>) -> Option<SymbolRef> {
    let expr = temporal_read_argument(expr.get_inner_expression());
    let Expression::Identifier(reference) = expr.get_inner_expression() else {
      return None;
    };
    (self.symbol_for)(reference).and_then(|symbol| self.locals.get(&symbol).copied())
  }

  /// The candidate whose member `callee` is, for calls that would bind the object as `this`.
  fn member_callee(&self, callee: &Expression<'_>) -> Option<SymbolRef> {
    let member = match callee.get_inner_expression() {
      Expression::ChainExpression(chain) => match &chain.expression {
        ChainElement::CallExpression(_) => return None,
        element => element.as_member_expression()?,
      },
      expr => expr.as_member_expression()?,
    };
    self.candidate(member.object())
  }
}

impl<'a, F: Fn(&IdentifierReference<'_>) -> Option<SymbolId>> VisitJs<'a> for ReadWalker<'_, F> {
  fn visit_expression(&mut self, expr: &Expression<'a>) {
    if let Some(member) = expr.as_member_expression()
      && let Some(object) = self.candidate(member.object())
    {
      match static_read_key(member) {
        Some(name) => {
          self.reads.push((self.stmt_idx, object, CompactStr::from(name)));
          if let Expression::ComputedMemberExpression(computed) = expr {
            self.visit_expression(&computed.expression);
          }
        }
        None => {
          self.escaped.insert(object);
          walk_js::walk_expression(self, expr);
        }
      }
      return;
    }
    walk_js::walk_expression(self, expr);
  }

  fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
    if let Some(object) = self.member_callee(&call.callee) {
      self.escaped.insert(object);
    }
    walk_js::walk_call_expression(self, call);
  }

  fn visit_tagged_template_expression(&mut self, tagged: &TaggedTemplateExpression<'a>) {
    if let Some(object) = self.member_callee(&tagged.tag) {
      self.escaped.insert(object);
    }
    walk_js::walk_tagged_template_expression(self, tagged);
  }

  fn visit_unary_expression(&mut self, unary: &UnaryExpression<'a>) {
    if unary.operator == UnaryOperator::Delete
      && let Some(member) = unary.argument.get_inner_expression().as_member_expression()
      && let Some(object) = self.candidate(member.object())
    {
      self.escaped.insert(object);
    }
    walk_js::walk_unary_expression(self, unary);
  }

  // `export { tokens }` reaches the object through the module's exports, which
  // `escape_observable_exports` covers.
  fn visit_export_named_declaration(&mut self, _declaration: &ExportNamedDeclaration<'a>) {}

  fn visit_identifier_reference(&mut self, reference: &IdentifierReference<'a>) {
    if let Some(object) =
      (self.symbol_for)(reference).and_then(|symbol| self.locals.get(&symbol).copied())
    {
      self.escaped.insert(object);
    }
  }
}
