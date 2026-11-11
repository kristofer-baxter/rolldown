use std::sync::Arc;

use oxc::ast::ast::{
  AccessorProperty, AssignmentExpression, AssignmentTarget, AssignmentTargetProperty,
  BindingPattern, ChainElement, ComputedMemberExpression, Expression, ExpressionStatement,
  Function, IdentifierReference, MemberExpression, ObjectAssignmentTarget, ObjectPattern,
  ObjectProperty, ObjectPropertyKind, Program, PropertyDefinition, PropertyKey, PropertyKind,
  SimpleAssignmentTarget, Span, StaticBlock, StaticMemberExpression, UnaryOperator,
  VariableDeclarator,
};
use oxc::ast_visit::{VisitJsMut, walk_js_mut};
use oxc::diagnostics::{Diagnostics, OxcDiagnostic};
use oxc::parser::Parser;
use oxc::semantic::{ReferenceFlags, ScopeFlags, Scoping};
use oxc::span::{GetSpan, SPAN, SourceType};
use oxc::syntax::identifier::is_identifier_name;
use oxc::syntax::node::NodeId;
use oxc::syntax::reference::Reference;
use oxc_allocator::{Address, Allocator, ArenaBox, GetAddress, ReplaceWith, UnstableAddress};
use oxc_str::{CompactStr, Str};
use rustc_hash::FxHashMap;

use crate::utils::native_cell_plan::is_primitive_literal;

/// Configuration for [ReplaceGlobalDefines].
#[derive(Debug, Clone)]
pub struct ReplaceGlobalDefinesConfig(Arc<ReplaceGlobalDefinesConfigImpl>);

static THIS_STR: Str<'static> = Str::new_const("this");

#[derive(Debug)]
struct IdentifierDefine {
  identifier_defines: FxHashMap<CompactStr, CompactStr>,
  has_this_expr_define: bool,
}

#[derive(Debug)]
struct ReplaceGlobalDefinesConfigImpl {
  identifier: IdentifierDefine,
  dot: FxHashMap<CompactStr, Vec<DotDefine>>,
  meta_property: FxHashMap<CompactStr, Vec<MetaPropertyDefine>>,
  meta_property_wildcard: Vec<MetaPropertyDefine>,
  typeof_defines: Vec<TypeofDefine>,
  import_meta: Option<CompactStr>,
  /// Single-identifier prefixes of dotted defines (e.g. `CROSS` from `CROSS.X`), mapping
  /// prefix -> (property -> replacement code) for destructuring assignments.
  identifier_prefixes: FxHashMap<CompactStr, FxHashMap<CompactStr, CompactStr>>,
  /// Dotted prefixes of dotted defines (e.g. `process.env` from `process.env.NODE_ENV`),
  /// bucketed by the prefix's last segment (`env`).
  dot_prefixes: FxHashMap<CompactStr, Vec<DotPrefixDefine>>,
  /// `import.meta.<parts>` prefixes of dotted defines (e.g. `import.meta.env` from
  /// `import.meta.env.KEY`), bucketed by the prefix's last segment (`env`).
  meta_property_prefixes: FxHashMap<CompactStr, Vec<MetaPrefixDefine>>,
  /// Properties defined directly under `import.meta` (e.g. `env` from `import.meta.env`),
  /// for `const { env } = import.meta`.
  import_meta_prefix: FxHashMap<CompactStr, CompactStr>,
}

#[derive(Debug)]
struct TypeofDefine {
  parts: Vec<CompactStr>,
  value: CompactStr,
}

#[derive(Debug)]
pub struct DotDefine {
  pub parts: Vec<CompactStr>,
  pub value: CompactStr,
}

#[derive(Debug)]
struct DotPrefixDefine {
  parts: Vec<CompactStr>,
  props: FxHashMap<CompactStr, CompactStr>,
}

#[derive(Debug)]
pub struct MetaPropertyDefine {
  pub parts: Vec<CompactStr>,
  pub value: CompactStr,
  pub postfix_wildcard: bool,
}

#[derive(Debug)]
struct MetaPrefixDefine {
  parts: Vec<CompactStr>,
  props: FxHashMap<CompactStr, CompactStr>,
}

impl MetaPropertyDefine {
  pub fn new(parts: Vec<CompactStr>, value: CompactStr, postfix_wildcard: bool) -> Self {
    Self { parts, value, postfix_wildcard }
  }
}

impl DotDefine {
  fn new(parts: Vec<CompactStr>, value: CompactStr) -> Self {
    Self { parts, value }
  }
}

enum IdentifierType {
  Identifier,
  DotDefines { key: CompactStr, parts: Vec<CompactStr> },
  Typeof { parts: Vec<CompactStr> },
  ImportMetaWithParts { key: CompactStr, parts: Vec<CompactStr>, postfix_wildcard: bool },
  ImportMeta(bool),
}

enum ObjectTreeNode {
  Leaf(String),
  Nested(Vec<(ObjectTreeKey, ObjectTreeNode)>),
}

#[derive(PartialEq, Eq)]
enum ObjectTreeKey {
  OwnProperty(String),
  PrototypeSetter,
}

fn to_property_key(key: &str) -> String {
  if key == "__proto__" {
    "[\"__proto__\"]".to_owned()
  } else {
    serde_json::to_string(key).unwrap_or_else(|_| format!("\"{key}\""))
  }
}

fn serialize_object_tree(entries: &[(ObjectTreeKey, ObjectTreeNode)]) -> String {
  let mut out = String::from("{");
  for (idx, (key, node)) in entries.iter().enumerate() {
    if idx > 0 {
      out.push(',');
    }
    match key {
      ObjectTreeKey::OwnProperty(key) => out.push_str(&to_property_key(key)),
      ObjectTreeKey::PrototypeSetter => out.push_str("\"__proto__\""),
    }
    out.push(':');
    match node {
      ObjectTreeNode::Leaf(code) => out.push_str(code),
      ObjectTreeNode::Nested(children) => out.push_str(&serialize_object_tree(children)),
    }
  }
  out.push('}');
  out
}

fn parse_static_object_tree<'a>(
  expr: &'a Expression<'a>,
  source: &str,
) -> Option<Vec<(ObjectTreeKey, ObjectTreeNode)>> {
  let Expression::ObjectExpression(obj) = expr.without_parentheses() else {
    return None;
  };
  let mut entries = Vec::with_capacity(obj.properties.len());
  for prop in &obj.properties {
    let ObjectPropertyKind::ObjectProperty(prop) = prop else {
      return None;
    };
    if prop.kind != PropertyKind::Init || prop.method {
      return None;
    }
    let key =
      ReplaceGlobalDefines::static_property_key_name(&prop.key, prop.computed)?.into_string();
    let key = if key == "__proto__" && !prop.computed && !prop.shorthand {
      ObjectTreeKey::PrototypeSetter
    } else {
      ObjectTreeKey::OwnProperty(key)
    };
    let node = if let Some(nested) = parse_static_object_tree(&prop.value, source) {
      ObjectTreeNode::Nested(nested)
    } else {
      let span = prop.value.span();
      ObjectTreeNode::Leaf(source.get(span.start as usize..span.end as usize)?.to_owned())
    };
    // Earlier duplicate values still evaluate even though the last property wins.
    entries.push((key, node));
  }
  Some(entries)
}

fn insert_object_tree_path(
  entries: &mut Vec<(ObjectTreeKey, ObjectTreeNode)>,
  parts: &[&str],
  value: &str,
  allocator: &Allocator,
) {
  let Some((&first, rest)) = parts.split_first() else {
    return;
  };
  if rest.is_empty() {
    if let Some((_, existing)) = entries
      .iter_mut()
      .rev()
      .find(|(k, _)| matches!(k, ObjectTreeKey::OwnProperty(key) if key == first))
    {
      if matches!(existing, ObjectTreeNode::Nested(_))
        && let Ok(parsed) = Parser::new(allocator, value, SourceType::default()).parse_expression()
        && let Some(sub_entries) = parse_static_object_tree(&parsed, value)
        && let ObjectTreeNode::Nested(existing_children) = existing
      {
        let additions = sub_entries
          .into_iter()
          .filter(|(k, _)| !existing_children.iter().any(|(ek, _)| ek == k))
          .collect::<Vec<_>>();
        existing_children.extend(additions);
      }
      return;
    }
    let node = if value.trim_start().starts_with(['{', '('])
      && let Ok(parsed) = Parser::new(allocator, value, SourceType::default()).parse_expression()
      && let Some(sub_entries) = parse_static_object_tree(&parsed, value)
    {
      ObjectTreeNode::Nested(sub_entries)
    } else {
      ObjectTreeNode::Leaf(value.to_owned())
    };
    entries.push((ObjectTreeKey::OwnProperty(first.to_owned()), node));
    return;
  }
  if let Some((_, existing)) = entries
    .iter_mut()
    .rev()
    .find(|(k, _)| matches!(k, ObjectTreeKey::OwnProperty(key) if key == first))
  {
    if let ObjectTreeNode::Nested(children) = existing {
      insert_object_tree_path(children, rest, value, allocator);
    }
    return;
  }
  let mut children = Vec::new();
  insert_object_tree_path(&mut children, rest, value, allocator);
  entries.push((ObjectTreeKey::OwnProperty(first.to_owned()), ObjectTreeNode::Nested(children)));
}

#[inline(never)]
fn merge_dotted_keys_into_object<S: AsRef<str>>(
  allocator: &Allocator,
  key: &str,
  value: &str,
  defines: &[(S, S)],
) -> Option<String> {
  let trimmed = value.trim();
  if !(trimmed.starts_with('{') || trimmed.starts_with("({")) {
    return None;
  }
  let has_dotted_child = defines.iter().any(|(other_key, other_val)| {
    let other_key = other_key.as_ref();
    !other_key.starts_with("typeof ")
      && !other_key.ends_with(".*")
      && !other_val.as_ref().trim().is_empty()
      && other_key.strip_prefix(key).is_some_and(|rest| rest.starts_with('.'))
  });
  if !has_dotted_child {
    return None;
  }
  let parsed = Parser::new(allocator, value, SourceType::default()).parse_expression().ok()?;
  let mut tree = parse_static_object_tree(&parsed, value)?;
  let mut children: Vec<(Vec<&str>, &str)> = defines
    .iter()
    .filter_map(|(other_key, other_val)| {
      let other_key = other_key.as_ref();
      let other_val = other_val.as_ref();
      if other_key.starts_with("typeof ")
        || other_key.ends_with(".*")
        || other_val.trim().is_empty()
      {
        return None;
      }
      let rest = other_key.strip_prefix(key)?.strip_prefix('.')?;
      let parts: Vec<&str> = rest.split('.').collect();
      if parts.is_empty() || !parts.iter().all(|part| is_identifier_name(part)) {
        return None;
      }
      Some((parts, other_val))
    })
    .collect();
  children.sort_by_key(|(parts, _)| parts.len());
  for (parts, child_val) in children {
    insert_object_tree_path(&mut tree, &parts, child_val, allocator);
  }
  let serialized = serialize_object_tree(&tree);
  let merged = if trimmed.starts_with('(') { format!("({serialized})") } else { serialized };
  ReplaceGlobalDefinesConfig::check_value(allocator, &merged).ok()?;
  Some(merged)
}

impl ReplaceGlobalDefinesConfig {
  /// # Errors
  ///
  /// * key is not a supported identifier, property chain, or `typeof` expression
  /// * value has a syntax error
  #[inline(never)]
  pub fn new<S: AsRef<str>>(defines: &[(S, S)]) -> Result<Self, Diagnostics> {
    let allocator = Allocator::default();
    let mut identifier_defines: FxHashMap<CompactStr, CompactStr> = FxHashMap::default();
    let mut dot_defines: FxHashMap<CompactStr, Vec<DotDefine>> = FxHashMap::default();
    let mut typeof_defines = vec![];
    let mut meta_property: FxHashMap<CompactStr, Vec<MetaPropertyDefine>> = FxHashMap::default();
    let mut meta_property_wildcard: Vec<MetaPropertyDefine> = vec![];
    let mut import_meta = None;
    let mut has_this_expr_define = false;
    let mut identifier_prefixes: FxHashMap<CompactStr, FxHashMap<CompactStr, CompactStr>> =
      FxHashMap::default();
    let mut dot_prefixes: FxHashMap<CompactStr, Vec<DotPrefixDefine>> = FxHashMap::default();
    let mut meta_property_prefixes: FxHashMap<CompactStr, Vec<MetaPrefixDefine>> =
      FxHashMap::default();
    let mut import_meta_prefix: FxHashMap<CompactStr, CompactStr> = FxHashMap::default();

    for (key, value) in defines {
      let key = key.as_ref();
      let raw_value = value.as_ref();
      Self::check_value(&allocator, raw_value)?;
      let merged = merge_dotted_keys_into_object(&allocator, key, raw_value, defines);
      let value = merged.as_deref().unwrap_or(raw_value);

      if !key.starts_with("typeof ")
        && !key.ends_with(".*")
        && !value.trim().is_empty()
        && let Some((prefix, prop)) = key.rsplit_once('.')
        && !prefix.is_empty()
        && is_identifier_name(prop)
        && let Ok(prefix_type) = Self::check_key(prefix)
      {
        let prop_str = CompactStr::new(prop);
        let val_str = CompactStr::new(value);
        match prefix_type {
          IdentifierType::Identifier if prefix != "this" => {
            identifier_prefixes
              .entry(CompactStr::new(prefix))
              .or_default()
              .entry(prop_str)
              .or_insert(val_str);
          }
          IdentifierType::DotDefines { key: bucket_key, parts } => {
            let bucket = dot_prefixes.entry(bucket_key).or_default();
            if let Some(existing) = bucket.iter_mut().find(|d| d.parts == parts) {
              existing.props.entry(prop_str).or_insert(val_str);
            } else {
              let mut props = FxHashMap::default();
              props.insert(prop_str, val_str);
              bucket.push(DotPrefixDefine { parts, props });
            }
          }
          IdentifierType::ImportMetaWithParts {
            key: bucket_key,
            parts,
            postfix_wildcard: false,
          } => {
            let bucket = meta_property_prefixes.entry(bucket_key).or_default();
            if let Some(existing) = bucket.iter_mut().find(|d| d.parts == parts) {
              existing.props.entry(prop_str).or_insert(val_str);
            } else {
              let mut props = FxHashMap::default();
              props.insert(prop_str, val_str);
              bucket.push(MetaPrefixDefine { parts, props });
            }
          }
          IdentifierType::ImportMeta(false) => {
            import_meta_prefix.entry(prop_str).or_insert(val_str);
          }
          _ => {}
        }
      }

      match Self::check_key(key)? {
        IdentifierType::Identifier => {
          has_this_expr_define |= key == "this";
          identifier_defines.entry(CompactStr::new(key)).or_insert_with(|| CompactStr::new(value));
        }
        IdentifierType::DotDefines { key, parts } => {
          dot_defines.entry(key).or_default().push(DotDefine::new(parts, CompactStr::new(value)));
        }
        IdentifierType::Typeof { parts } => {
          typeof_defines.push(TypeofDefine { parts, value: CompactStr::new(value) });
        }
        IdentifierType::ImportMetaWithParts { key, parts, postfix_wildcard } => {
          let define = MetaPropertyDefine::new(parts, CompactStr::new(value), postfix_wildcard);
          if postfix_wildcard {
            meta_property_wildcard.push(define);
          } else {
            meta_property.entry(key).or_default().push(define);
          }
        }
        IdentifierType::ImportMeta(postfix_wildcard) => {
          if postfix_wildcard {
            meta_property_wildcard.push(MetaPropertyDefine::new(
              vec![],
              CompactStr::new(value),
              postfix_wildcard,
            ));
          } else {
            import_meta = Some(CompactStr::new(value));
          }
        }
      }
    }

    Ok(Self(Arc::new(ReplaceGlobalDefinesConfigImpl {
      identifier: IdentifierDefine { identifier_defines, has_this_expr_define },
      dot: dot_defines,
      meta_property,
      meta_property_wildcard,
      typeof_defines,
      import_meta,
      identifier_prefixes,
      dot_prefixes,
      meta_property_prefixes,
      import_meta_prefix,
    })))
  }

  fn check_key(key: &str) -> Result<IdentifierType, Diagnostics> {
    if let Some(argument) = key.strip_prefix("typeof ") {
      let parts: Vec<&str> = argument.split('.').collect();
      let parts = Self::check_identifier_parts(key, &parts)?;
      return Ok(IdentifierType::Typeof { parts });
    }

    let parts: Vec<&str> = key.split('.').collect();
    assert_ne!(parts.len(), 0);

    if parts.len() == 1 {
      if !is_identifier_name(parts[0]) {
        return Err(
          vec![OxcDiagnostic::error(format!("The define key `{key}` is not an identifier."))]
            .into(),
        );
      }
      return Ok(IdentifierType::Identifier);
    }
    let normalized_parts_len =
      if parts[parts.len() - 1] == "*" { parts.len() - 1 } else { parts.len() };
    let is_import_meta = parts[0] == "import" && parts[1] == "meta";

    let compact_parts = Self::check_identifier_parts(key, &parts[0..normalized_parts_len])?;
    let key = CompactStr::new(parts[normalized_parts_len - 1]);
    if is_import_meta {
      match normalized_parts_len {
        2 => Ok(IdentifierType::ImportMeta(normalized_parts_len != parts.len())),
        _ => Ok(IdentifierType::ImportMetaWithParts {
          key,
          parts: compact_parts.into_iter().skip(2).collect(),
          postfix_wildcard: normalized_parts_len != parts.len(),
        }),
      }
    } else if normalized_parts_len != parts.len() {
      Err(
        vec![OxcDiagnostic::error(
          "The postfix wildcard is only allowed for `import.meta`.".to_string(),
        )]
        .into(),
      )
    } else {
      Ok(IdentifierType::DotDefines { key, parts: compact_parts })
    }
  }

  fn check_identifier_parts(key: &str, parts: &[&str]) -> Result<Vec<CompactStr>, Diagnostics> {
    for part in parts {
      if !is_identifier_name(part) {
        return Err(
          vec![OxcDiagnostic::error(format!(
            "The define key `{key}` contains an invalid identifier `{part}`."
          ))]
          .into(),
        );
      }
    }
    Ok(parts.iter().map(|part| CompactStr::new(part)).collect())
  }

  fn check_value(allocator: &Allocator, source_text: &str) -> Result<(), Diagnostics> {
    Parser::new(allocator, source_text, SourceType::default()).parse_expression()?;
    Ok(())
  }
}

#[must_use]
pub struct ReplaceGlobalDefinesReturn {
  pub scoping: Scoping,
  pub changed: bool,
}

pub struct ReplaceGlobalDefines<'a> {
  allocator: &'a Allocator,
  config: ReplaceGlobalDefinesConfig,
  ast_node_lock: Option<Address>,
  changed: bool,
  scoping: Option<Scoping>,
  non_arrow_function_depth: u32,
  class_this_depth: u32,
  destructuring_keys: Option<Vec<CompactStr>>,
}

/// `{ __proto__ }` defines an own property, unlike `{ __proto__: value }`.
pub fn expand_shorthand_property<'a>(prop: &mut ObjectProperty<'a>, allocator: &'a Allocator) {
  prop.shorthand = false;
  if let PropertyKey::StaticIdentifier(key) = &prop.key
    && key.name == "__proto__"
  {
    let builder = oxc::ast::builder::AstBuilder::new(allocator);
    prop.key = PropertyKey::new_string_literal(key.span, "__proto__", None, &builder);
    prop.computed = true;
  }
}

pub fn expand_changed_shorthand_property<'a>(
  prop: &mut ObjectProperty<'a>,
  allocator: &'a Allocator,
) {
  if prop.shorthand {
    let keep_shorthand = match (&prop.key, &prop.value) {
      (PropertyKey::StaticIdentifier(key), Expression::Identifier(value)) => key.name == value.name,
      _ => false,
    };
    if !keep_shorthand {
      expand_shorthand_property(prop, allocator);
    }
  }
}

impl<'a> VisitJsMut<'a> for ReplaceGlobalDefines<'a> {
  fn visit_expression_statement(&mut self, stmt: &mut ExpressionStatement<'a>) {
    if self.ast_node_lock.is_some() {
      return;
    }
    // An empty key set proves the assignment result is discarded, not an observable object.
    self.destructuring_keys = match stmt.expression.get_inner_expression() {
      Expression::AssignmentExpression(assign)
        if matches!(assign.left, AssignmentTarget::ObjectAssignmentTarget(_)) =>
      {
        Some(Vec::new())
      }
      _ => None,
    };
    self.visit_expression(&mut stmt.expression);
    self.destructuring_keys = None;
  }

  fn visit_object_property(&mut self, prop: &mut ObjectProperty<'a>) {
    walk_js_mut::walk_object_property(self, prop);
    expand_changed_shorthand_property(prop, self.allocator);
  }

  fn visit_expression(&mut self, expr: &mut Expression<'a>) {
    if self.ast_node_lock.is_some() {
      return;
    }
    self.enter_expression(expr);
    walk_js_mut::walk_expression(self, expr);
    if self.ast_node_lock == Some(expr.address()) {
      self.ast_node_lock = None;
    }
    if matches!(expr, Expression::ChainExpression(_)) {
      Self::unwrap_chain_expression_if_no_optional(expr);
    }
  }

  fn visit_assignment_expression(&mut self, node: &mut AssignmentExpression<'a>) {
    if !self.enter_assignment_expression(node) {
      return;
    }
    let outer_keys = self.destructuring_keys.take();
    self.visit_assignment_target(&mut node.left);
    self.prepare_assignment_destructuring(&node.left, &node.right, outer_keys);
    self.visit_expression(&mut node.right);
    self.destructuring_keys = None;
  }

  fn visit_function(&mut self, func: &mut Function<'a>, flags: ScopeFlags) {
    self.non_arrow_function_depth += 1;
    walk_js_mut::walk_function(self, func, flags);
    self.non_arrow_function_depth -= 1;
  }

  fn visit_property_definition(&mut self, property: &mut PropertyDefinition<'a>) {
    self.visit_decorators(&mut property.decorators);
    self.visit_property_key(&mut property.key);
    if let Some(value) = &mut property.value {
      self.class_this_depth += 1;
      self.visit_expression(value);
      self.class_this_depth -= 1;
    }
  }

  fn visit_static_block(&mut self, block: &mut StaticBlock<'a>) {
    self.class_this_depth += 1;
    walk_js_mut::walk_static_block(self, block);
    self.class_this_depth -= 1;
  }

  fn visit_accessor_property(&mut self, property: &mut AccessorProperty<'a>) {
    self.visit_decorators(&mut property.decorators);
    self.visit_property_key(&mut property.key);
    if let Some(value) = &mut property.value {
      self.class_this_depth += 1;
      self.visit_expression(value);
      self.class_this_depth -= 1;
    }
  }

  fn visit_variable_declarator(&mut self, declarator: &mut VariableDeclarator<'a>) {
    if self.ast_node_lock.is_some() {
      return;
    }
    self.destructuring_keys = None;
    self.visit_binding_pattern(&mut declarator.id);
    if let Some(init) = &mut declarator.init {
      self.prepare_declarator_destructuring(&declarator.id, init);
      self.visit_expression(init);
      self.destructuring_keys = None;
    }
  }
}

impl<'a> ReplaceGlobalDefines<'a> {
  pub fn new(allocator: &'a Allocator, config: ReplaceGlobalDefinesConfig) -> Self {
    Self {
      allocator,
      config,
      ast_node_lock: None,
      changed: false,
      scoping: None,
      non_arrow_function_depth: 0,
      class_this_depth: 0,
      destructuring_keys: None,
    }
  }

  #[inline(never)]
  fn enter_expression(&mut self, expr: &mut Expression<'a>) {
    let has_destructuring = self.destructuring_keys.is_some();
    let keep_destructuring = has_destructuring
      && match expr {
        Expression::ParenthesizedExpression(_) => true,
        Expression::AssignmentExpression(assign) => {
          matches!(assign.left, AssignmentTarget::ObjectAssignmentTarget(_))
        }
        _ => false,
      };
    let is_replaced = (!self.config.0.typeof_defines.is_empty()
      && self.replace_typeof_defines(expr))
      || self.replace_identifier_defines(expr)
      || self.replace_dot_defines(expr)
      || (has_destructuring && self.replace_destructuring_prefix_defines(expr));
    if has_destructuring && (!keep_destructuring || is_replaced) {
      self.destructuring_keys = None;
    }
    if is_replaced {
      self.mark_as_changed();
      self.ast_node_lock = Some(expr.address());
    }
  }

  #[inline(never)]
  fn enter_assignment_expression(&mut self, node: &mut AssignmentExpression<'a>) -> bool {
    if self.ast_node_lock.is_some() {
      return false;
    }
    if self.replace_define_with_assignment_expr(node) {
      self.mark_as_changed();
      self.ast_node_lock = Some(node.unstable_address());
      return false;
    }
    true
  }

  fn can_destructure_rhs(&self, mut expr: &Expression<'a>) -> bool {
    loop {
      match expr {
        Expression::ParenthesizedExpression(paren) => expr = &paren.expression,
        Expression::AssignmentExpression(assign)
          if matches!(assign.left, AssignmentTarget::ObjectAssignmentTarget(_)) =>
        {
          expr = &assign.right;
        }
        Expression::Identifier(ident) => {
          return self.config.0.identifier.identifier_defines.contains_key(ident.name.as_str())
            || self.config.0.identifier_prefixes.contains_key(ident.name.as_str());
        }
        Expression::StaticMemberExpression(member) => {
          let prop = member.property.name.as_str();
          return self.config.0.dot.contains_key(prop)
            || self.config.0.dot_prefixes.contains_key(prop)
            || self.config.0.meta_property.contains_key(prop)
            || self.config.0.meta_property_prefixes.contains_key(prop);
        }
        Expression::ImportMeta(_) => {
          return self.config.0.import_meta.is_some()
            || !self.config.0.import_meta_prefix.is_empty();
        }
        _ => return false,
      }
    }
  }

  #[inline(never)]
  fn prepare_assignment_destructuring(
    &mut self,
    left: &AssignmentTarget<'a>,
    right: &Expression<'a>,
    outer_keys: Option<Vec<CompactStr>>,
  ) {
    if let Some(outer) = outer_keys
      && let AssignmentTarget::ObjectAssignmentTarget(pat) = left
      && self.can_destructure_rhs(right)
    {
      if let Some(mut keys) = Self::collect_object_assignment_keys(pat) {
        for k in outer {
          if !keys.contains(&k) {
            keys.push(k);
          }
        }
        self.destructuring_keys = Some(keys);
      } else {
        self.destructuring_keys = None;
      }
    } else {
      self.destructuring_keys = None;
    }
  }

  #[inline(never)]
  fn prepare_declarator_destructuring(&mut self, id: &BindingPattern<'a>, init: &Expression<'a>) {
    if let BindingPattern::ObjectPattern(pat) = id
      && self.can_destructure_rhs(init)
    {
      self.destructuring_keys = Self::collect_object_pattern_keys(pat);
    } else {
      self.destructuring_keys = None;
    }
  }

  fn collect_object_pattern_keys(pat: &ObjectPattern<'a>) -> Option<Vec<CompactStr>> {
    if pat.rest.is_some() {
      return None;
    }
    let mut keys = Vec::with_capacity(pat.properties.len());
    for prop in &pat.properties {
      let key = Self::static_property_key_name(&prop.key, prop.computed)?;
      if !keys.contains(&key) {
        keys.push(key);
      }
    }
    Some(keys)
  }

  fn collect_object_assignment_keys(pat: &ObjectAssignmentTarget<'a>) -> Option<Vec<CompactStr>> {
    if pat.rest.is_some() {
      return None;
    }
    let mut keys = Vec::with_capacity(pat.properties.len());
    for prop in &pat.properties {
      let key = match prop {
        AssignmentTargetProperty::AssignmentTargetPropertyIdentifier(ident) => {
          CompactStr::new(ident.binding.name.as_str())
        }
        AssignmentTargetProperty::AssignmentTargetPropertyProperty(prop) => {
          Self::static_property_key_name(&prop.name, prop.computed)?
        }
      };
      if !keys.contains(&key) {
        keys.push(key);
      }
    }
    Some(keys)
  }

  fn static_property_key_name(key: &PropertyKey<'a>, computed: bool) -> Option<CompactStr> {
    if !computed {
      return key.static_name().and_then(|name| Some(CompactStr::new(name.as_str()?)));
    }
    match key {
      PropertyKey::StringLiteral(lit) => Some(CompactStr::new(lit.value.as_str()?)),
      PropertyKey::TemplateLiteral(lit) if lit.expressions.is_empty() && lit.quasis.len() == 1 => {
        let text = match lit.quasis[0].value.cooked {
          Some(cooked) => cooked.as_str()?,
          None => lit.quasis[0].value.raw.as_str(),
        };
        Some(CompactStr::new(text))
      }
      _ => None,
    }
  }

  fn mark_as_changed(&mut self) {
    self.changed = true;
  }

  fn scoping(&self) -> &Scoping {
    self.scoping.as_ref().unwrap()
  }

  #[inline(never)]
  #[expect(clippy::missing_panics_doc)]
  pub fn build(
    &mut self,
    scoping: Scoping,
    program: &mut Program<'a>,
  ) -> ReplaceGlobalDefinesReturn {
    self.scoping = Some(scoping);
    self.visit_program(program);
    let scoping = self.scoping.take().unwrap();
    ReplaceGlobalDefinesReturn { scoping, changed: self.changed }
  }

  fn parse_value(&mut self, source_text: &str) -> Expression<'a> {
    let source_text = self.allocator.alloc_str(source_text);
    let mut expr =
      Parser::new(self.allocator, source_text, SourceType::default()).parse_expression().unwrap();

    let scoping = self.scoping.as_mut().unwrap();
    UpdateReplacedExpression { scoping }.visit_expression(&mut expr);

    expr
  }

  fn replace_identifier_defines(&mut self, expr: &mut Expression<'a>) -> bool {
    match expr {
      Expression::Identifier(ident) => {
        if let Some(new_expr) = self.replace_identifier_define_impl(ident) {
          *expr = self.destructing_dot_define_optimizer(new_expr);
          return true;
        }
      }
      Expression::ThisExpression(_)
        if self.config.0.identifier.has_this_expr_define
          && should_replace_this_expr(self.current_scope_flags()) =>
      {
        if let Some(value) = self.config.0.identifier.identifier_defines.get("this") {
          let value = value.clone();
          let value = self.parse_value(&value);
          *expr = self.destructing_dot_define_optimizer(value);
          return true;
        }
      }
      _ => {}
    }
    false
  }

  fn replace_typeof_defines(&mut self, expr: &mut Expression<'a>) -> bool {
    let Expression::UnaryExpression(unary) = expr else {
      return false;
    };
    if unary.operator != UnaryOperator::Typeof {
      return false;
    }

    let scoping = self.scoping();
    let scope_flags = self.current_scope_flags();
    let value = self
      .config
      .0
      .typeof_defines
      .iter()
      .find(|define| Self::is_typeof_define(scoping, scope_flags, define, &unary.argument))
      .map(|define| define.value.clone());
    if let Some(value) = value {
      *expr = self.parse_value(&value);
      return true;
    }
    false
  }

  fn is_typeof_define(
    scoping: &Scoping,
    scope_flags: ScopeFlags,
    define: &TypeofDefine,
    argument: &Expression<'a>,
  ) -> bool {
    let argument = argument.without_parentheses();
    if define.parts.len() == 1 {
      return match argument {
        Expression::Identifier(ident) => {
          ident.name == define.parts[0].as_str()
            && Self::is_global_or_ambient_reference(scoping, ident)
        }
        Expression::ThisExpression(_) => {
          define.parts[0].as_str() == "this" && should_replace_this_expr(scope_flags)
        }
        _ => false,
      };
    }

    let member = match argument {
      Expression::StaticMemberExpression(member) => {
        DotDefineMemberExpression::StaticMemberExpression(member)
      }
      Expression::ComputedMemberExpression(member) => {
        DotDefineMemberExpression::ComputedMemberExpression(member)
      }
      Expression::ChainExpression(chain) => {
        let Some(member) = chain.expression.as_member_expression() else {
          return false;
        };
        match member {
          MemberExpression::StaticMemberExpression(member) => {
            DotDefineMemberExpression::StaticMemberExpression(member)
          }
          MemberExpression::ComputedMemberExpression(member) => {
            DotDefineMemberExpression::ComputedMemberExpression(member)
          }
          MemberExpression::PrivateFieldExpression(_) => return false,
        }
      }
      Expression::ImportMeta(_) => {
        return define.parts.len() == 2
          && define.parts[0].as_str() == "import"
          && define.parts[1].as_str() == "meta";
      }
      _ => return false,
    };
    Self::is_dot_define_parts(scoping, scope_flags, &define.parts, member)
  }

  fn replace_identifier_define_impl(
    &mut self,
    ident: &ArenaBox<'_, IdentifierReference<'_>>,
  ) -> Option<Expression<'a>> {
    if !Self::is_global_or_ambient_reference(self.scoping(), ident) {
      return None;
    }
    let value = self.config.0.identifier.identifier_defines.get(ident.name.as_str())?.clone();
    let value = self.parse_value(&value);
    Some(value)
  }

  fn replace_define_with_assignment_expr(&mut self, node: &mut AssignmentExpression<'a>) -> bool {
    let new_left = node
      .left
      .as_simple_assignment_target_mut()
      .and_then(|item| match item {
        SimpleAssignmentTarget::ComputedMemberExpression(computed_member_expr) => {
          self.replace_dot_computed_member_expr_no_optimize(computed_member_expr)
        }
        SimpleAssignmentTarget::StaticMemberExpression(member) => {
          self.replace_dot_static_member_expr_no_optimize(member)
        }
        SimpleAssignmentTarget::AssignmentTargetIdentifier(ident) => {
          self.replace_identifier_define_impl(ident)
        }
        _ => None,
      })
      .and_then(assignment_target_from_expr);
    if let Some(new_left) = new_left {
      node.left = new_left;
      return true;
    }
    false
  }

  fn replace_dot_defines(&mut self, expr: &mut Expression<'a>) -> bool {
    match expr {
      Expression::ChainExpression(chain) => {
        let Some(new_expr) =
          chain.expression.as_member_expression_mut().and_then(|item| match item {
            MemberExpression::ComputedMemberExpression(computed_member_expr) => {
              self.replace_dot_computed_member_expr(computed_member_expr)
            }
            MemberExpression::StaticMemberExpression(member) => {
              self.replace_dot_static_member_expr(member)
            }
            MemberExpression::PrivateFieldExpression(_) => None,
          })
        else {
          return false;
        };
        *expr = new_expr;
        return true;
      }
      Expression::StaticMemberExpression(member) => {
        if let Some(new_expr) = self.replace_dot_static_member_expr(member) {
          *expr = new_expr;
          return true;
        }
      }
      Expression::ComputedMemberExpression(member) => {
        if let Some(new_expr) = self.replace_dot_computed_member_expr(member) {
          *expr = new_expr;
          return true;
        }
      }
      Expression::ImportMeta(_) => {
        if let Some(replacement) = self.config.0.import_meta.clone() {
          let value = self.parse_value(&replacement);
          *expr = self.destructing_dot_define_optimizer(value);
          return true;
        }
      }
      _ => {}
    }
    false
  }

  #[cold]
  #[inline(never)]
  fn replace_destructuring_prefix_defines(&mut self, expr: &mut Expression<'a>) -> bool {
    let Some(needed_keys) = &self.destructuring_keys else {
      return false;
    };
    if needed_keys.is_empty() {
      return false;
    }
    let scoping = self.scoping();
    let scope_flags = self.current_scope_flags();
    let props: Option<&FxHashMap<CompactStr, CompactStr>> = match expr {
      Expression::Identifier(ident) => {
        if !Self::is_global_or_ambient_reference(scoping, ident) {
          return false;
        }
        self.config.0.identifier_prefixes.get(ident.name.as_str())
      }
      Expression::StaticMemberExpression(member) => {
        let leaf = member.property.name.as_str();
        self
          .config
          .0
          .dot_prefixes
          .get(leaf)
          .and_then(|bucket| {
            bucket
              .iter()
              .find(|d| {
                Self::is_dot_define_parts(
                  scoping,
                  scope_flags,
                  &d.parts,
                  DotDefineMemberExpression::StaticMemberExpression(member),
                )
              })
              .map(|d| &d.props)
          })
          .or_else(|| {
            self.config.0.meta_property_prefixes.get(leaf).and_then(|bucket| {
              bucket
                .iter()
                .find(|d| Self::is_meta_property_parts(&d.parts, false, member))
                .map(|d| &d.props)
            })
          })
      }
      Expression::ComputedMemberExpression(member) => {
        let Some(leaf) = static_property_name_of_computed_expr(member) else {
          return false;
        };
        self.config.0.dot_prefixes.get(leaf.as_str()).and_then(|bucket| {
          bucket
            .iter()
            .find(|d| {
              Self::is_dot_define_parts(
                scoping,
                scope_flags,
                &d.parts,
                DotDefineMemberExpression::ComputedMemberExpression(member),
              )
            })
            .map(|d| &d.props)
        })
      }
      Expression::ImportMeta(_) => {
        (!self.config.0.import_meta_prefix.is_empty()).then_some(&self.config.0.import_meta_prefix)
      }
      _ => None,
    };
    let Some(props) = props else {
      return false;
    };
    if !needed_keys.iter().all(|k| props.contains_key(k.as_str())) {
      return false;
    }
    let mut code = String::from("({");
    for (idx, key) in needed_keys.iter().enumerate() {
      if idx > 0 {
        code.push(',');
      }
      code.push_str(&to_property_key(key.as_str()));
      code.push(':');
      code.push_str(props[key.as_str()].as_str());
    }
    code.push_str("})");
    *expr = self.parse_value(&code);
    true
  }

  fn replace_dot_computed_member_expr(
    &mut self,
    member: &ComputedMemberExpression<'a>,
  ) -> Option<Expression<'a>> {
    let value = self.replace_dot_computed_member_expr_no_optimize(member)?;
    Some(self.destructing_dot_define_optimizer(value))
  }

  fn replace_dot_computed_member_expr_no_optimize(
    &mut self,
    member: &ComputedMemberExpression<'a>,
  ) -> Option<Expression<'a>> {
    let leaf = static_property_name_of_computed_expr(member)?;
    let value = self.find_dot_define_value(
      leaf.as_str(),
      DotDefineMemberExpression::ComputedMemberExpression(member),
    )?;
    Some(self.parse_value(&value))
  }

  fn replace_dot_static_member_expr(
    &mut self,
    member: &StaticMemberExpression<'a>,
  ) -> Option<Expression<'a>> {
    let value = self.replace_dot_static_member_expr_impl(member)?;
    Some(self.destructing_dot_define_optimizer(value))
  }

  fn replace_dot_static_member_expr_no_optimize(
    &mut self,
    member: &StaticMemberExpression<'a>,
  ) -> Option<Expression<'a>> {
    self.replace_dot_static_member_expr_impl(member)
  }

  fn replace_dot_static_member_expr_impl(
    &mut self,
    member: &StaticMemberExpression<'a>,
  ) -> Option<Expression<'a>> {
    let leaf = member.property.name.as_str();
    let value = self
      .find_dot_define_value(leaf, DotDefineMemberExpression::StaticMemberExpression(member))
      .or_else(|| self.find_meta_property_value(leaf, member))?;
    Some(self.parse_value(&value))
  }

  fn find_dot_define_value(
    &self,
    leaf: &str,
    member: DotDefineMemberExpression<'_, 'a>,
  ) -> Option<CompactStr> {
    let bucket = self.config.0.dot.get(leaf)?;
    let scoping = self.scoping();
    let scope_flags = self.current_scope_flags();
    bucket
      .iter()
      .find(|dot_define| Self::is_dot_define(scoping, scope_flags, dot_define, member))
      .map(|dot_define| dot_define.value.clone())
  }

  fn find_meta_property_value(
    &self,
    leaf: &str,
    member: &StaticMemberExpression<'a>,
  ) -> Option<CompactStr> {
    if let Some(bucket) = self.config.0.meta_property.get(leaf)
      && let Some(define) =
        bucket.iter().find(|define| Self::is_meta_property_define(define, member))
    {
      return Some(define.value.clone());
    }
    if !self.config.0.meta_property_wildcard.is_empty() && Self::is_import_meta_member(member) {
      return self
        .config
        .0
        .meta_property_wildcard
        .iter()
        .find(|define| Self::is_meta_property_define(define, member))
        .map(|define| define.value.clone());
    }
    None
  }

  fn is_import_meta_member(member: &StaticMemberExpression<'a>) -> bool {
    matches!(member.get_first_object(), Expression::ImportMeta(_))
  }

  pub fn is_meta_property_define(
    meta_define: &MetaPropertyDefine,
    member: &StaticMemberExpression<'a>,
  ) -> bool {
    Self::is_meta_property_parts(&meta_define.parts, meta_define.postfix_wildcard, member)
  }

  fn is_meta_property_parts(
    parts: &[CompactStr],
    postfix_wildcard: bool,
    member: &StaticMemberExpression<'a>,
  ) -> bool {
    enum WildCardStatus {
      None,
      Pending,
      Matched,
    }
    if parts.is_empty() && postfix_wildcard {
      return matches!(&member.object, Expression::ImportMeta(_));
    }
    debug_assert_ne!(parts.len(), 0);

    let mut current_part_member_expression = Some(member);
    let mut cur_part_name: &str = &member.property.name;
    let mut is_full_match = true;
    let mut i = parts.len() - 1;
    let mut has_matched_part = false;
    let mut wildcard_status =
      if postfix_wildcard { WildCardStatus::Pending } else { WildCardStatus::None };
    loop {
      let part = &parts[i];
      let matched = cur_part_name == part;
      if matched {
        has_matched_part = true;
      } else {
        is_full_match = false;
        if matches!(wildcard_status, WildCardStatus::None | WildCardStatus::Matched)
          || has_matched_part
        {
          return false;
        }
        wildcard_status = WildCardStatus::Matched;
      }

      current_part_member_expression = if let Some(member) = current_part_member_expression {
        match member.object.without_parentheses() {
          Expression::StaticMemberExpression(member) => {
            cur_part_name = &member.property.name;
            Some(member)
          }
          Expression::ImportMeta(_) => {
            if postfix_wildcard {
              return has_matched_part && !is_full_match;
            }
            return i == 0;
          }
          Expression::Identifier(_) => {
            return false;
          }
          _ => None,
        }
      } else {
        return false;
      };

      if i == 0 && matched {
        break;
      }

      if matched {
        i -= 1;
      }
    }

    false
  }

  fn current_scope_flags(&self) -> ScopeFlags {
    if self.non_arrow_function_depth > 0 {
      ScopeFlags::Function
    } else if self.class_this_depth > 0 {
      ScopeFlags::ClassStaticBlock
    } else {
      ScopeFlags::Top
    }
  }

  fn unwrap_chain_expression_if_no_optional(expr: &mut Expression<'a>) {
    let Expression::ChainExpression(chain) = &*expr else {
      return;
    };

    let (optional, mut current) = match &chain.expression {
      ChainElement::CallExpression(c) => (c.optional, Some(&c.callee)),
      ChainElement::TSNonNullExpression(ts) => (false, Some(&ts.expression)),
      _ => match chain.expression.as_member_expression() {
        Some(m) => (m.optional(), Some(m.object())),
        None => return,
      },
    };
    if optional {
      return;
    }

    while let Some(e) = current {
      match e {
        Expression::StaticMemberExpression(m) => {
          if m.optional {
            return;
          }
          current = Some(&m.object);
        }
        Expression::ComputedMemberExpression(m) => {
          if m.optional {
            return;
          }
          current = Some(&m.object);
        }
        Expression::PrivateFieldExpression(m) => {
          if m.optional {
            return;
          }
          current = Some(&m.object);
        }
        Expression::CallExpression(c) => {
          if c.optional {
            return;
          }
          current = Some(&c.callee);
        }
        Expression::TSNonNullExpression(non_null) => {
          current = Some(&non_null.expression);
        }
        _ => break,
      }
    }

    expr.replace_with(|chain_expr| {
      let Expression::ChainExpression(chain) = chain_expr else { unreachable!() };
      Expression::from(chain.unbox().expression)
    });
  }

  pub fn is_dot_define<'b>(
    scoping: &Scoping,
    scope_flags: ScopeFlags,
    dot_define: &DotDefine,
    member: DotDefineMemberExpression<'b, 'a>,
  ) -> bool {
    Self::is_dot_define_parts(scoping, scope_flags, &dot_define.parts, member)
  }

  fn is_dot_define_parts<'b>(
    scoping: &Scoping,
    scope_flags: ScopeFlags,
    parts: &[CompactStr],
    member: DotDefineMemberExpression<'b, 'a>,
  ) -> bool {
    debug_assert!(parts.len() > 1);
    let should_replace_this_expr = should_replace_this_expr(scope_flags);
    let Some(cur_part_name) = member.name() else {
      return false;
    };
    let mut cur_part_name: &str = cur_part_name.as_str();
    let mut current_part_member_expression = Some(member);

    for (i, part) in parts.iter().enumerate().rev() {
      if cur_part_name != part {
        return false;
      }
      if i == 0 {
        break;
      }

      current_part_member_expression = if let Some(member) = current_part_member_expression {
        match member.object().without_parentheses() {
          Expression::StaticMemberExpression(member) => {
            cur_part_name = &member.property.name;
            Some(DotDefineMemberExpression::StaticMemberExpression(member))
          }
          Expression::ComputedMemberExpression(computed_member) => {
            static_property_name_of_computed_expr(computed_member).map(|name| {
              cur_part_name = name.as_str();
              DotDefineMemberExpression::ComputedMemberExpression(computed_member)
            })
          }
          Expression::Identifier(ident) => {
            if !Self::is_global_or_ambient_reference(scoping, ident) {
              return false;
            }
            cur_part_name = &ident.name;
            None
          }
          Expression::ThisExpression(_) if should_replace_this_expr => {
            cur_part_name = THIS_STR.as_str();
            None
          }
          Expression::ImportMeta(_) => {
            if i >= 2 && parts[i - 1].as_str() == "meta" && parts[i - 2].as_str() == "import" {
              return i == 2;
            }
            None
          }
          _ => None,
        }
      } else {
        return false;
      };
    }

    current_part_member_expression.is_none()
  }

  #[inline(never)]
  fn destructing_dot_define_optimizer(&self, mut expr: Expression<'a>) -> Expression<'a> {
    let Some(needed_keys) = &self.destructuring_keys else {
      return expr;
    };
    let obj = match &mut expr {
      Expression::ObjectExpression(obj) => obj,
      Expression::ParenthesizedExpression(paren) => {
        let Expression::ObjectExpression(obj) = &mut paren.expression else {
          return expr;
        };
        obj
      }
      _ => return expr,
    };

    let mut defined_keys = Vec::with_capacity(obj.properties.len());
    for prop in &obj.properties {
      let ObjectPropertyKind::ObjectProperty(prop) = prop else {
        return expr;
      };
      if prop.kind != PropertyKind::Init || prop.method {
        return expr;
      }
      let Some(key) = Self::static_property_key_name(&prop.key, prop.computed) else {
        return expr;
      };
      if key == "__proto__" && !prop.computed && !prop.shorthand {
        return expr;
      }
      defined_keys.push(key);
    }
    // Accessors and inherited reads can observe properties absent from the pattern.
    if !needed_keys.iter().all(|key| defined_keys.contains(key)) {
      return expr;
    }

    obj.properties.retain(|prop| match prop {
      ObjectPropertyKind::ObjectProperty(prop) => {
        !is_primitive_literal(&prop.value)
          || Self::static_property_key_name(&prop.key, prop.computed)
            .is_none_or(|s| needed_keys.iter().any(|k| k == &s))
      }
      ObjectPropertyKind::SpreadProperty(_) => true,
    });
    expr
  }

  fn is_global_or_ambient_reference(scoping: &Scoping, ident: &IdentifierReference<'_>) -> bool {
    scoping
      .get_reference(ident.reference_id())
      .symbol_id()
      .is_none_or(|symbol_id| scoping.symbol_flags(symbol_id).is_ambient())
  }
}

#[derive(Debug, Clone, Copy)]
pub enum DotDefineMemberExpression<'b, 'ast: 'b> {
  StaticMemberExpression(&'b StaticMemberExpression<'ast>),
  ComputedMemberExpression(&'b ComputedMemberExpression<'ast>),
}

impl<'b, 'a> DotDefineMemberExpression<'b, 'a> {
  fn name(&self) -> Option<Str<'a>> {
    match self {
      DotDefineMemberExpression::StaticMemberExpression(expr) => {
        Some(expr.property.name.as_arena_str())
      }
      DotDefineMemberExpression::ComputedMemberExpression(expr) => {
        static_property_name_of_computed_expr(expr)
      }
    }
  }

  fn object(&self) -> &'b Expression<'a> {
    match self {
      DotDefineMemberExpression::StaticMemberExpression(expr) => &expr.object,
      DotDefineMemberExpression::ComputedMemberExpression(expr) => &expr.object,
    }
  }
}

fn static_property_name_of_computed_expr<'b, 'a: 'b>(
  expr: &'b ComputedMemberExpression<'a>,
) -> Option<Str<'a>> {
  match &expr.expression {
    Expression::StringLiteral(lit) => lit.value.as_arena_str(),
    Expression::TemplateLiteral(lit) if lit.expressions.is_empty() && lit.quasis.len() == 1 => {
      match lit.quasis[0].value.cooked {
        Some(cooked) => cooked.as_arena_str(),
        None => Some(lit.quasis[0].value.raw),
      }
    }
    _ => None,
  }
}

const fn should_replace_this_expr(scope_flags: ScopeFlags) -> bool {
  !scope_flags.contains(ScopeFlags::ClassStaticBlock)
    && (!scope_flags.contains(ScopeFlags::Function) || scope_flags.contains(ScopeFlags::Arrow))
}

fn assignment_target_from_expr(expr: Expression<'_>) -> Option<AssignmentTarget<'_>> {
  match expr {
    Expression::ComputedMemberExpression(expr) => {
      Some(AssignmentTarget::ComputedMemberExpression(expr))
    }
    Expression::StaticMemberExpression(expr) => {
      Some(AssignmentTarget::StaticMemberExpression(expr))
    }
    Expression::Identifier(ident) => Some(AssignmentTarget::AssignmentTargetIdentifier(ident)),
    _ => None,
  }
}

struct UpdateReplacedExpression<'a> {
  scoping: &'a mut Scoping,
}

impl VisitJsMut<'_> for UpdateReplacedExpression<'_> {
  fn visit_identifier_reference(&mut self, ident: &mut IdentifierReference<'_>) {
    let reference =
      Reference::new(NodeId::DUMMY, self.scoping.root_scope_id(), ReferenceFlags::Read);
    let reference_id = self.scoping.create_reference(reference);
    self.scoping.add_root_unresolved_reference(ident.name, reference_id);
    ident.set_reference_id(reference_id);
    walk_js_mut::walk_identifier_reference(self, ident);
  }

  fn visit_span(&mut self, span: &mut Span) {
    *span = SPAN;
  }
}
