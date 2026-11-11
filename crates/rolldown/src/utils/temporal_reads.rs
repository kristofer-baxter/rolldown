use oxc::{
  allocator::{Allocator, TakeIn},
  ast::{
    ast::{self, Declaration, ExportDefaultDeclarationKind, Expression, Statement},
    builder::AstBuilder,
  },
  ast_visit::{VisitJs, VisitJsMut, walk_js_mut},
  semantic::Scoping,
  span::SPAN,
  syntax::symbol::SymbolFlags,
};
use rustc_hash::FxHashSet;

// An internal AST intrinsic, never an authored identifier or emitted runtime call.
const TEMPORAL_READ: &str = "\0native_temporal_read";

struct RootSymbolRefCollector<'a> {
  scoping: &'a Scoping,
  root_scope_id: oxc::semantic::ScopeId,
  refs: FxHashSet<oxc::semantic::SymbolId>,
  has_direct_eval: bool,
}

impl<'ast> VisitJs<'ast> for RootSymbolRefCollector<'_> {
  fn visit_with_statement(&mut self, _stmt: &ast::WithStatement<'ast>) {
    self.has_direct_eval = true;
  }

  fn visit_identifier_reference(&mut self, ident: &ast::IdentifierReference<'ast>) {
    if ident.name == "eval"
      && ident
        .reference_id
        .get()
        .is_some_and(|ref_id| self.scoping.get_reference(ref_id).symbol_id().is_none())
    {
      self.has_direct_eval = true;
    }
    if let Some(symbol_id) =
      ident.reference_id.get().and_then(|ref_id| self.scoping.get_reference(ref_id).symbol_id())
      && self.scoping.symbol_scope_id(symbol_id) == self.root_scope_id
    {
      self.refs.insert(symbol_id);
    }
  }
}

fn collect_root_symbol_refs(
  scoping: &Scoping,
  root_scope_id: oxc::semantic::ScopeId,
  visit: impl FnOnce(&mut RootSymbolRefCollector<'_>),
) -> (FxHashSet<oxc::semantic::SymbolId>, bool) {
  let mut collector = RootSymbolRefCollector {
    scoping,
    root_scope_id,
    refs: FxHashSet::default(),
    has_direct_eval: false,
  };
  visit(&mut collector);
  (collector.refs, collector.has_direct_eval)
}

fn top_level_function_declaration<'a, 'ast>(
  mut statement: &'a Statement<'ast>,
) -> Option<&'a ast::Function<'ast>> {
  while let Statement::LabeledStatement(labeled) = statement {
    statement = &labeled.body;
  }
  match statement {
    Statement::FunctionDeclaration(func) => Some(func),
    Statement::ExportDeclaration(export) => match &export.declaration {
      Declaration::FunctionDeclaration(func) => Some(func),
      _ => None,
    },
    Statement::ExportDefaultDeclaration(export) => match &export.declaration {
      ExportDefaultDeclarationKind::FunctionDeclaration(func) => Some(func),
      _ => None,
    },
    _ => None,
  }
}

fn safe_post_declaration_symbols(
  program: &ast::Program<'_>,
  scoping: &Scoping,
) -> FxHashSet<oxc::semantic::SymbolId> {
  let root_scope_id = scoping.root_scope_id();
  let mut early_refs = FxHashSet::default();
  for statement in &program.body {
    if let Some(func) = top_level_function_declaration(statement) {
      let (refs, has_eval) = collect_root_symbol_refs(scoping, root_scope_id, |collector| {
        collector.visit_function(func, oxc::semantic::ScopeFlags::Function);
      });
      if has_eval {
        return FxHashSet::default();
      }
      early_refs.extend(refs);
    }
  }

  let mut safe = FxHashSet::default();
  for statement in &program.body {
    if let Some(func) = top_level_function_declaration(statement)
      && func.id.is_some()
    {
      continue;
    }
    let var_decl = match statement {
      Statement::VariableDeclaration(decl) => Some(decl),
      Statement::ExportDeclaration(export) => match &export.declaration {
        Declaration::VariableDeclaration(decl) => Some(decl),
        _ => None,
      },
      _ => None,
    };
    if let Some(decl) = var_decl {
      if decl.kind.is_lexical() {
        for declarator in &decl.declarations {
          if let Some(init) = &declarator.init {
            let (refs, has_eval) =
              collect_root_symbol_refs(scoping, root_scope_id, |c| c.visit_expression(init));
            if has_eval {
              return FxHashSet::default();
            }
            early_refs.extend(refs);
          }
          if let ast::BindingPattern::BindingIdentifier(binding) = &declarator.id {
            let sym = binding.symbol_id();
            let writable_ok = !decl.kind.is_const()
              || !scoping.get_resolved_references(sym).any(oxc::semantic::Reference::is_write);
            if writable_ok && !early_refs.contains(&sym) {
              safe.insert(sym);
            }
          } else {
            let (refs, has_eval) = collect_root_symbol_refs(scoping, root_scope_id, |c| {
              c.visit_binding_pattern(&declarator.id);
            });
            if has_eval {
              return FxHashSet::default();
            }
            early_refs.extend(refs);
          }
        }
      } else {
        let (refs, has_eval) = collect_root_symbol_refs(scoping, root_scope_id, |c| {
          c.visit_variable_declaration(decl);
        });
        if has_eval {
          return FxHashSet::default();
        }
        early_refs.extend(refs);
      }
      continue;
    }
    let class_decl = match statement {
      Statement::ClassDeclaration(class) => Some(class),
      Statement::ExportDeclaration(export) => match &export.declaration {
        Declaration::ClassDeclaration(class) => Some(class),
        _ => None,
      },
      _ => None,
    };
    if let Some(class) = class_decl {
      let (refs, has_eval) =
        collect_root_symbol_refs(scoping, root_scope_id, |c| c.visit_class(class));
      if has_eval {
        return FxHashSet::default();
      }
      early_refs.extend(refs);
      if let Some(binding) = &class.id {
        let sym = binding.symbol_id();
        if !early_refs.contains(&sym)
          && !scoping.get_resolved_references(sym).any(oxc::semantic::Reference::is_write)
        {
          safe.insert(sym);
        }
      }
      continue;
    }
    let (refs, has_eval) =
      collect_root_symbol_refs(scoping, root_scope_id, |c| c.visit_statement(statement));
    if has_eval {
      return FxHashSet::default();
    }
    early_refs.extend(refs);
  }
  safe
}

pub fn preserve_temporal_reads<'ast>(
  program: &mut ast::Program<'ast>,
  scoping: &Scoping,
  allocator: &'ast Allocator,
  manual_pure_functions: Option<&FxHashSet<String>>,
) -> bool {
  struct Preserve<'a, 'ast> {
    scoping: &'a Scoping,
    symbols: FxHashSet<oxc::semantic::SymbolId>,
    builder: AstBuilder<'ast>,
    manual_pure_functions: Option<&'a FxHashSet<String>>,
    changed: bool,
  }
  impl<'ast> Preserve<'_, 'ast> {
    fn restore_manual_pure_root(&mut self, expression: &mut Expression<'ast>) {
      let Some(functions) = self.manual_pure_functions.filter(|functions| !functions.is_empty())
      else {
        return;
      };
      let mut root = expression;
      let mut has_callee_arguments = false;
      loop {
        if matches!(root, Expression::Identifier(_))
          || matches!(root, Expression::CallExpression(call) if temporal_read_call_argument(call).is_some())
        {
          break;
        }
        root = match root {
          Expression::CallExpression(call) => {
            has_callee_arguments |= !call.arguments.is_empty();
            &mut call.callee
          }
          Expression::StaticMemberExpression(member) => &mut member.object,
          Expression::ComputedMemberExpression(member) => &mut member.object,
          Expression::ChainExpression(chain) => match &mut chain.expression {
            ast::ChainElement::CallExpression(call) => {
              has_callee_arguments |= !call.arguments.is_empty();
              &mut call.callee
            }
            ast::ChainElement::StaticMemberExpression(member) => &mut member.object,
            ast::ChainElement::ComputedMemberExpression(member) => &mut member.object,
            _ => return,
          },
          _ => return,
        };
      }
      if let Expression::Identifier(identifier) = temporal_read_argument(root)
        && functions.contains(identifier.name.as_str())
      {
        if has_callee_arguments {
          // Oxc's manual-purity chain shortcut does not inspect inner call arguments.
          // Keep the barrier instead of exposing those arguments to that shortcut.
          self.preserve_identifier(root);
        } else {
          // See internal-docs/inline-common-chunks/implementation.md. Only the promised
          // chain root is transparent: authored arguments, keys and spreads retain barriers.
          unwrap_temporal_read(root);
        }
      }
    }
    fn preserve_identifier(&mut self, expression: &mut Expression<'ast>) {
      let Expression::Identifier(identifier) = expression else {
        return;
      };
      let Some(symbol) = identifier
        .reference_id
        .get()
        .and_then(|reference| self.scoping.get_reference(reference).symbol_id())
      else {
        return;
      };
      if !self.symbols.contains(&symbol) {
        return;
      }
      let value = expression.take_in(&self.builder);
      *expression = Expression::new_call_expression(
        SPAN,
        Expression::new_identifier(SPAN, TEMPORAL_READ, &self.builder),
        None,
        [ast::Argument::from(value)],
        false,
        &self.builder,
      );
      self.changed = true;
    }
  }
  impl<'ast> VisitJsMut<'ast> for Preserve<'_, 'ast> {
    fn visit_expression(&mut self, expression: &mut Expression<'ast>) {
      walk_js_mut::walk_expression(self, expression);
      match expression {
        Expression::CallExpression(call) => self.restore_manual_pure_root(&mut call.callee),
        Expression::NewExpression(call) => self.restore_manual_pure_root(&mut call.callee),
        Expression::TaggedTemplateExpression(tag) => self.restore_manual_pure_root(&mut tag.tag),
        Expression::StaticMemberExpression(_)
        | Expression::ComputedMemberExpression(_)
        | Expression::ChainExpression(_) => self.restore_manual_pure_root(expression),
        _ => {}
      }
      self.preserve_identifier(expression);
    }
  }
  let safe_symbols = safe_post_declaration_symbols(program, scoping);
  let mut symbols: FxHashSet<_> = scoping
    .symbol_ids()
    .filter(|&symbol| {
      scoping.symbol_scope_id(symbol) == scoping.root_scope_id()
        && scoping
          .symbol_flags(symbol)
          .intersects(SymbolFlags::BlockScopedVariable | SymbolFlags::Class | SymbolFlags::Import)
        && !safe_symbols.contains(&symbol)
    })
    .collect();
  for statement in &program.body {
    if let Statement::ImportDeclaration(import) = statement
      && let Some(specifiers) = &import.specifiers
    {
      for specifier in specifiers {
        if let ast::ImportDeclarationSpecifier::ImportNamespaceSpecifier(namespace) = specifier {
          symbols.remove(&namespace.local.symbol_id());
        }
      }
    }
  }
  let mut preserve = Preserve {
    scoping,
    symbols,
    builder: AstBuilder::new(allocator),
    manual_pure_functions,
    changed: false,
  };
  preserve.visit_program(program);
  preserve.changed
}

/// The expression a temporal read wraps, or `expression` itself when it is not one.
pub fn temporal_read_argument<'b, 'ast>(expression: &'b Expression<'ast>) -> &'b Expression<'ast> {
  if let Expression::CallExpression(call) = expression
    && let Some(argument) = temporal_read_call_argument(call)
  {
    return argument;
  }
  expression
}

pub fn temporal_read_call_argument<'b, 'ast>(
  call: &'b ast::CallExpression<'ast>,
) -> Option<&'b Expression<'ast>> {
  if matches!(&call.callee, Expression::Identifier(identifier) if identifier.name == TEMPORAL_READ)
    && let [argument] = call.arguments.as_slice()
    && let Some(argument) = argument.as_expression()
  {
    return Some(argument);
  }
  None
}

pub fn unwrap_temporal_read(expression: &mut Expression<'_>) {
  if let Expression::CallExpression(call) = expression
    && matches!(&call.callee, Expression::Identifier(identifier) if identifier.name == TEMPORAL_READ)
  {
    *expression = call.arguments.pop().expect("temporal read argument").into_expression();
  }
}
