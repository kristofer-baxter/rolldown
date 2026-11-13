//! Proofs that a read of a top-level `let`, `const` or `class` runs after its initialization, so
//! that it needs no guard where a wrapper hoisted the declaration out of the engine's TDZ.
//!
//! A module's function declarations are its only code that can run before its body starts: every
//! other closure, class and initializer is created by the body. Once the body has started, each
//! dependency outside the module's own import cycle has finished evaluating. So a read outside a
//! top-level function declaration is initialized when it reads
//!
//! - another module's binding, through an import of a module outside this module's import
//!   cycle: that module, and everything it re-exports from, has finished evaluating. Importing
//!   the same binding from a barrel inside the cycle proves nothing, because the barrel may not
//!   have reached the binding's module yet; or
//! - this module's binding, from a top-level statement after the one that declares it, or from
//!   one before it when no statement from the read's to the declaration's runs any code: the
//!   closure that holds the read then cannot be called until the binding is initialized.
//!
//! "Acyclic" alone does not prove the first for a read inside a function declaration: a module
//! of a cycle upstream can call it while the dependencies of both are still unevaluated.

use oxc::{
  ast::ast,
  ast_visit::VisitJs,
  semantic::{ReferenceId, Scoping},
};
use oxc_index::IndexVec;
use rolldown_common::{
  ImportKind, IndexModules, ModuleIdx, NormalModule, StmtInfoIdx, StmtInfos, SymbolRef, SymbolRefDb,
};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::types::linking_metadata::LinkingMetadataVec;

/// The strongly connected component of each module in the graph of `import` and `require()`.
pub(super) fn cycle_components(modules: &IndexModules) -> IndexVec<ModuleIdx, u32> {
  let mut graph = petgraph::prelude::DiGraphMap::<ModuleIdx, ()>::new();
  for module in modules.iter().filter_map(|module| module.as_normal()) {
    graph.add_node(module.idx);
    for record in &module.import_records {
      if matches!(record.kind, ImportKind::Import | ImportKind::Require)
        && let Some(importee) = record.resolved_module
        && modules[importee].is_normal()
      {
        graph.add_edge(module.idx, importee, ());
      }
    }
  }
  let mut components = oxc_index::index_vec![u32::MAX; modules.len()];
  for (component, members) in petgraph::algo::tarjan_scc(&graph).into_iter().enumerate() {
    for module in members {
      components[module] = u32::try_from(component).expect("fewer than 2^32 modules");
    }
  }
  components
}

/// The modules that an `import` or `require()` cycle passes through, a module that imports
/// itself among them.
pub(super) fn cyclic_modules(modules: &IndexModules) -> FxHashSet<ModuleIdx> {
  let components = cycle_components(modules);
  let mut sizes = FxHashMap::<u32, u32>::default();
  for component in &components {
    *sizes.entry(*component).or_default() += 1;
  }
  modules
    .iter()
    .filter_map(|module| module.as_normal())
    .filter(|module| {
      sizes[&components[module.idx]] > 1
        || module.import_records.iter().any(|record| {
          matches!(record.kind, ImportKind::Import | ImportKind::Require)
            && record.resolved_module == Some(module.idx)
        })
    })
    .map(|module| module.idx)
    .collect()
}

#[derive(Default)]
pub(super) struct ProvenReads {
  /// The identifiers that read a guarded binding after its initialization.
  pub(super) references: FxHashSet<ReferenceId>,
  /// For a top-level statement and a guarded binding that its identifiers name, whether all of
  /// them are in `references`.
  pub(super) statements: FxHashMap<(StmtInfoIdx, SymbolRef), bool>,
}

pub(super) struct ProofInput<'a> {
  pub(super) module: &'a NormalModule,
  pub(super) scoping: &'a Scoping,
  pub(super) symbols: &'a SymbolRefDb,
  pub(super) metas: &'a LinkingMetadataVec,
  pub(super) statements: &'a StmtInfos,
  pub(super) components: &'a IndexVec<ModuleIdx, u32>,
  /// The guarded bindings that are still the declarations the module's author wrote.
  pub(super) authored: &'a FxHashSet<SymbolRef>,
}

struct Prover<'a> {
  input: &'a ProofInput<'a>,
  /// For each statement of the program, the number of statements before it that run code.
  /// A range of statements runs no code when the count does not grow across it.
  active_before: Vec<u32>,
  statement: StmtInfoIdx,
  /// Inside a top-level function declaration, which exists before the module's body runs.
  hoisted: bool,
  proven: ProvenReads,
}

/// Whether evaluating `expression` runs no code and reads no binding.
fn inert_expression(expression: &ast::Expression<'_>, depth: u8) -> bool {
  use ast::Expression;
  if depth > 8 {
    return false;
  }
  match expression {
    Expression::ArrowFunctionExpression(_)
    | Expression::FunctionExpression(_)
    | Expression::BooleanLiteral(_)
    | Expression::NumericLiteral(_)
    | Expression::StringLiteral(_)
    | Expression::NullLiteral(_)
    | Expression::BigIntLiteral(_)
    | Expression::RegExpLiteral(_) => true,
    Expression::TemplateLiteral(template) => template.expressions.is_empty(),
    Expression::ParenthesizedExpression(inner) => inert_expression(&inner.expression, depth + 1),
    Expression::ObjectExpression(object) => {
      object.properties.iter().all(|property| match property {
        ast::ObjectPropertyKind::ObjectProperty(property) => {
          !property.computed && inert_expression(&property.value, depth + 1)
        }
        ast::ObjectPropertyKind::SpreadProperty(_) => false,
      })
    }
    Expression::ArrayExpression(array) => array.elements.iter().all(|element| match element {
      ast::ArrayExpressionElement::SpreadElement(_) => false,
      ast::ArrayExpressionElement::Elision(_) => true,
      element => {
        element.as_expression().is_some_and(|element| inert_expression(element, depth + 1))
      }
    }),
    _ => false,
  }
}

fn inert_declaration(declaration: &ast::VariableDeclaration<'_>) -> bool {
  declaration.declarations.iter().all(|declarator| {
    matches!(declarator.id, ast::BindingPattern::BindingIdentifier(_))
      && declarator.init.as_ref().is_none_or(|init| inert_expression(init, 0))
  })
}

/// Whether evaluating a top-level `statement` runs no code: it only declares functions and
/// bindings whose initializers are functions and literals.
fn inert_statement(statement: &ast::Statement<'_>) -> bool {
  use ast::{Declaration, Statement};
  if super::temporal_bindings::top_level_function_declaration(statement).is_some() {
    return true;
  }
  match statement {
    Statement::EmptyStatement(_)
    | Statement::ImportDeclaration(_)
    | Statement::ExportAllDeclaration(_)
    | Statement::ExportFromDeclaration(_)
    | Statement::ExportNamedDeclaration(_) => true,
    Statement::VariableDeclaration(declaration) => inert_declaration(declaration),
    Statement::ExportDeclaration(export) => match &export.declaration {
      Declaration::VariableDeclaration(declaration) => inert_declaration(declaration),
      _ => false,
    },
    Statement::ExportDefaultDeclaration(export) => {
      export.declaration.as_expression().is_some_and(|expression| inert_expression(expression, 0))
    }
    _ => false,
  }
}

impl Prover<'_> {
  /// Whether a read of `binding` through the module's own `local` symbol, from the statement
  /// being visited, runs after the binding's initialization.
  fn initialized_before(&self, local: SymbolRef, binding: SymbolRef) -> bool {
    let input = self.input;
    let module = input.module.idx;
    if binding.owner == module {
      return input.authored.contains(&binding)
        && input.statements.declared_stmts_by_symbol(&binding).iter().max().is_some_and(
          |declaration| {
            // Statement `n` of the program has index `n + 1` here: the range from the read's
            // statement through the declaration's runs no code.
            self.statement > *declaration
              || self
                .active_before
                .get(self.statement.index() - 1)
                .zip(self.active_before.get(declaration.index()))
                .is_some_and(|(before, after)| before == after)
          },
        );
    }
    // The module that this one imports the binding from, which may only re-export it.
    let Some(imported) = input
      .module
      .named_imports
      .get(&local)
      .and_then(|import| input.module.import_records[import.record_idx].resolved_module)
    else {
      return false;
    };
    input.components[imported] != input.components[module]
      && !input.metas[binding.owner].is_tla_or_contains_tla_dependency
      && !input.metas[module].is_tla_or_contains_tla_dependency
  }
}

impl<'ast> VisitJs<'ast> for Prover<'_> {
  fn visit_identifier_reference(&mut self, identifier: &ast::IdentifierReference<'ast>) {
    let input = self.input;
    let Some(reference_id) = identifier.reference_id.get() else {
      return;
    };
    let reference = input.scoping.get_reference(reference_id);
    let Some(symbol) = reference.symbol_id() else {
      return;
    };
    if input.scoping.symbol_scope_id(symbol) != input.scoping.root_scope_id() {
      return;
    }
    let local: SymbolRef = (input.module.idx, symbol).into();
    let binding = input.symbols.canonical_ref_for(local);
    if !input.metas[binding.owner].temporal_symbols.contains_key(&binding) {
      return;
    }
    let proven = !self.hoisted
      && reference.is_read()
      && !reference.is_write()
      && self.initialized_before(local, binding);
    *self.proven.statements.entry((self.statement, binding)).or_insert(true) &= proven;
    if proven {
      self.proven.references.insert(reference_id);
    }
  }
}

pub(super) fn prove_reads(input: &ProofInput<'_>, program: &ast::Program<'_>) -> ProvenReads {
  let mut prover = Prover {
    input,
    active_before: std::iter::once(0)
      .chain(program.body.iter().scan(0, |active, statement| {
        *active += u32::from(!inert_statement(statement));
        Some(*active)
      }))
      .collect(),
    statement: StmtInfoIdx::new(0),
    hoisted: false,
    proven: ProvenReads::default(),
  };
  for (index, statement) in program.body.iter().enumerate() {
    // The namespace object's statement comes first, then one per statement of the program.
    prover.statement = StmtInfoIdx::new(index + 1);
    prover.hoisted = super::temporal_bindings::top_level_function_declaration(statement).is_some();
    prover.visit_statement(statement);
  }
  prover.proven
}
