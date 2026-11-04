use oxc::{
  ast::ast::{self, Expression},
  ast::builder::GetAstBuilder,
  span::SPAN,
};
use rolldown_common::{ImportKind, IndexModules, ModuleIdx, NormalModule};

pub fn is_json_module(module: &NormalModule) -> bool {
  module.module_type == rolldown_common::ModuleType::Json
    || module.import_records.iter().any(|record| record.module_request == "\0rolldown/json-module")
}

/// Whether the namespace of the JSON module `module` holds its top-level properties beside
/// `default`. Rspack gives a JSON namespace those properties for an importer that isn't strict
/// ESM (`javascript/auto`: neither `.mjs` nor `.mts`, outside a `"type": "module"` package),
/// and localization loaders read their string tables that way, as
/// `(await import(`./translations/${locale}/Strings.json`)).strings`. So one such
/// importer that imports `module` statically or with `import()`, without a static `with`
/// clause, gives the namespace those properties. A strict importer and an explicit
/// `with { type: "json" }` import otherwise get only `default`, as a native JSON module's
/// namespace has. The namespace object is the module's own, so every importer sees the same one.
/// The proxy that module HMR puts between a lazy `import()` and its target stands for the
/// modules that import it. Only importers that `reaches` accepts count, the modules that the
/// graph reaches: a fresh graph doesn't load the others (#660).
pub fn json_namespace_has_properties(
  module: &NormalModule,
  modules: &IndexModules,
  reaches: impl Fn(ModuleIdx) -> bool,
) -> bool {
  is_json_module(module)
    && importers(module, modules).filter(|importer| reaches(importer.idx)).any(|importer| {
      if is_lazy_proxy(importer) {
        importers(importer, modules)
          .filter(|authored| reaches(authored.idx))
          .any(|authored| imports_as_javascript_auto(authored, importer))
      } else {
        imports_as_javascript_auto(importer, module)
      }
    })
}

/// Whether `module` is the proxy that module HMR puts between a lazy `import()` and its target.
pub fn is_lazy_proxy(module: &NormalModule) -> bool {
  module.id.contains(LAZY_PROXY_QUERY)
}

/// The marker of the proxy that module HMR puts between a lazy `import()` and its target.
const LAZY_PROXY_QUERY: &str = "?rolldown-lazy=1";

/// The modules that import `module` statically or with `import()`.
fn importers<'a>(
  module: &'a NormalModule,
  modules: &'a IndexModules,
) -> impl Iterator<Item = &'a NormalModule> {
  module
    .ecma_view
    .importers_idx
    .iter()
    .chain(&module.ecma_view.dynamic_importers_idx)
    .filter_map(|&importer_idx| modules[importer_idx].as_normal())
}

/// Whether `importer` isn't strict ESM and imports `target` statically or with `import()`,
/// without a static `with` clause.
fn imports_as_javascript_auto(importer: &NormalModule, target: &NormalModule) -> bool {
  !importer.ecma_view.def_format.is_esm()
    && importer.import_records.iter_enumerated().any(|(record_idx, record)| {
      record.resolved_module == Some(target.idx)
        && matches!(record.kind, ImportKind::Import | ImportKind::DynamicImport)
        && !importer.ecma_view.import_attribute_map.contains_key(&record_idx)
    })
}

pub fn is_text_module(module: &NormalModule) -> bool {
  module.module_type == rolldown_common::ModuleType::Text
    || module.import_records.iter().any(|record| record.module_request == "\0rolldown/text-module")
}

pub fn dynamic_import_is_text(
  import: &ast::ImportExpression<'_>,
  owner: &NormalModule,
  modules: &IndexModules,
) -> bool {
  owner
    .imports
    .get(&import.node_id())
    .and_then(|record| owner.import_records[*record].resolved_module)
    .and_then(|target| modules[target].as_normal())
    .is_some_and(is_text_module)
}

pub fn dynamic_import_is_json(
  import: &ast::ImportExpression<'_>,
  owner: &NormalModule,
  modules: &IndexModules,
) -> Option<bool> {
  let record = owner.imports.get(&import.node_id())?;
  let target = owner.import_records[*record].resolved_module?;
  let module = modules[target].as_normal()?;
  Some(is_json_module(module))
}

pub fn rewrite_hmr_import_attribute_target<'a>(
  call: &mut ast::CallExpression<'a>,
  owner: &NormalModule,
  modules: &IndexModules,
  builder: &impl GetAstBuilder<'a>,
) {
  if !matches!(call.arguments.get(2), Some(ast::Argument::StringLiteral(marker))
    if marker.value == "\0native:import-attributes")
  {
    return;
  }
  let Some(ast::Argument::ArrowFunctionExpression(load)) = call.arguments.get(1) else {
    return;
  };
  let ast::ArrowFunctionBody::CallExpression(cache) = &load.body else {
    return;
  };
  let Some(ast::Argument::ArrowFunctionExpression(fetch)) = cache.arguments.get(1) else {
    return;
  };
  let ast::ArrowFunctionBody::ImportExpression(import) = &fetch.body else {
    return;
  };
  let json = dynamic_import_is_json(import, owner, modules).unwrap_or(false);
  let text = dynamic_import_is_text(import, owner, modules);
  // Keep actual-target validation outside the cached namespace fetch in both finalizers.
  call.arguments[2] = ast::Argument::from(if text {
    Expression::new_string_literal(SPAN, "text", None, builder)
  } else {
    Expression::new_boolean_literal(SPAN, json, builder)
  });
}

pub fn is_empty_import_options(expr: &ast::Expression<'_>) -> bool {
  let Expression::ObjectExpression(obj) = expr.without_parentheses() else {
    return false;
  };
  if obj.properties.len() != 1 {
    return false;
  }
  let ast::ObjectPropertyKind::ObjectProperty(prop) = &obj.properties[0] else {
    return false;
  };
  if prop.computed
    || prop.method
    || !matches!(prop.kind, ast::PropertyKind::Init)
    || prop.key.static_name().as_ref().and_then(|s| s.as_str()) != Some("with")
  {
    return false;
  }
  matches!(
    prop.value.without_parentheses(),
    Expression::ObjectExpression(with_obj) if with_obj.properties.is_empty()
  )
}

fn static_import_options_with_type<'a>(expr: &'a ast::Expression<'a>) -> Option<&'a str> {
  let Expression::ObjectExpression(obj) = expr.without_parentheses() else {
    return None;
  };
  if obj.properties.len() != 1 {
    return None;
  }
  let ast::ObjectPropertyKind::ObjectProperty(prop) = &obj.properties[0] else {
    return None;
  };
  if prop.computed
    || prop.method
    || !matches!(prop.kind, ast::PropertyKind::Init)
    || prop.key.static_name().as_ref().and_then(|s| s.as_str()) != Some("with")
  {
    return None;
  }
  let Expression::ObjectExpression(with_obj) = prop.value.without_parentheses() else {
    return None;
  };
  if with_obj.properties.len() != 1 {
    return None;
  }
  let ast::ObjectPropertyKind::ObjectProperty(type_prop) = &with_obj.properties[0] else {
    return None;
  };
  if type_prop.computed
    || type_prop.method
    || !matches!(type_prop.kind, ast::PropertyKind::Init)
    || type_prop.key.static_name().as_ref().and_then(|s| s.as_str()) != Some("type")
  {
    return None;
  }
  let Expression::StringLiteral(str_lit) = type_prop.value.without_parentheses() else {
    return None;
  };
  str_lit.value.as_str()
}

pub fn is_json_type_import_options(expr: &ast::Expression<'_>) -> bool {
  static_import_options_with_type(expr) == Some("json")
}

pub fn is_text_type_import_options(expr: &ast::Expression<'_>) -> bool {
  static_import_options_with_type(expr) == Some("text")
}

pub fn is_statically_satisfied_import_options(
  expr: &ast::Expression<'_>,
  json: bool,
  text: bool,
) -> bool {
  is_empty_import_options(expr)
    || (text && is_text_type_import_options(expr))
    || (!text && json && is_json_type_import_options(expr))
}

pub fn thunk<'a>(expression: Expression<'a>, builder: &impl GetAstBuilder<'a>) -> Expression<'a> {
  Expression::new_arrow_function_expression(
    SPAN,
    false,
    None,
    ast::FormalParameters::boxed(
      SPAN,
      ast::FormalParameterKind::ArrowFormalParameters,
      [],
      None,
      builder,
    ),
    None,
    ast::ArrowFunctionBody::from(expression),
    builder,
  )
}

#[cfg(test)]
mod tests {
  use super::is_statically_satisfied_import_options;
  use oxc::{allocator::Allocator, parser::Parser, span::SourceType};

  fn check(expr_src: &str, json: bool, text: bool) -> bool {
    let allocator = Allocator::default();
    let expr =
      Parser::new(&allocator, expr_src, SourceType::mjs()).parse_expression().expect("valid expr");
    is_statically_satisfied_import_options(&expr, json, text)
  }

  #[test]
  fn statically_satisfied_import_options_rejects_getters_and_mismatched_types() {
    assert!(!check("({})", false, false));
    assert!(check("({ with: {} })", false, false));
    assert!(check("({ with: { type: 'json' } })", true, false));
    assert!(!check("({ with: { type: 'json' } })", false, false));
    assert!(!check("(globalThis.count++, {})", false, false));
    assert!(!check("({ with: getAttrs() })", false, false));
    assert!(!check("({ get with() { return {}; } })", false, false));
    assert!(!check("({ with: { get type() { return 'json'; } } })", true, false));
    assert!(!check("({ ...opts })", false, false));
  }
}
