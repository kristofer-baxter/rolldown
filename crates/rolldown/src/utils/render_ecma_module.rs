use std::sync::Arc;

use rolldown_common::{
  AttachDebugInfo, ModuleRenderOutput, NormalModule, NormalizedBundlerOptions,
  SourcemapChainElement,
};
use rolldown_error::BuildDiagnostic;
use rolldown_sourcemap::{
  Source, SourceMap, SourceMapBuilder, SourceMapSource, collapse_sourcemaps, empty_sourcemap,
};
use rolldown_utils::concat_string;

pub struct RenderEcmaModuleOutput {
  pub sources: Option<Arc<[Box<dyn Source + Send + Sync>]>>,
  pub warnings: Vec<BuildDiagnostic>,
}

pub fn render_ecma_module(
  module: &NormalModule,
  options: &NormalizedBundlerOptions,
  render_output: ModuleRenderOutput,
) -> RenderEcmaModuleOutput {
  if render_output.code.is_empty() {
    return RenderEcmaModuleOutput { sources: None, warnings: vec![] };
  }
  let mut sources: Vec<Box<dyn rolldown_sourcemap::Source + Send + Sync>> = Vec::with_capacity(6);
  // Mark only where a module starts, like esbuild and bun, unless developer-bundler's
  // native-record / HMR runtime semantics are active (which parse `//#region` / `//#endregion`).
  let use_region_pairs = crate::utils::native_records::runtime_module_semantics_enabled(options);
  let attach_debug_info = matches!(
    options.experimental.attach_debug_info,
    Some(AttachDebugInfo::Simple | AttachDebugInfo::Full)
  );
  if attach_debug_info {
    let prefix = if use_region_pairs { "//#region " } else { "// " };
    sources.push(Box::new(concat_string!(prefix, module.debug_id)));
  }

  // A native cell is virtual, but its loaded map points at its donor's authored file.
  let enable_sourcemap = options.sourcemap.is_some()
    && (!module.is_virtual() || crate::utils::native_records::is_native_cell(&module.id));
  let mut warnings = vec![];

  // Because oxc codegen sourcemap is last of sourcemap chain,
  // If here no extra sourcemap need remapping, we using it as final module sourcemap.
  // So here make sure using correct `source_name` and `source_content.

  if enable_sourcemap {
    if let Some(sourcemap) = collapse_module_sourcemap(
      &module.sourcemap_chain,
      render_output.map,
      module.id.as_str(),
      &mut warnings,
    ) {
      sources.push(Box::new(
        SourceMapSource::new(render_output.code, sourcemap)
          .with_pre_compute_sourcemap_data(options.is_sourcemap_enabled()),
      ));
    } else {
      sources.push(Box::new(render_output.code));
    }
  } else {
    sources.push(Box::new(render_output.code));
  }

  if attach_debug_info && use_region_pairs {
    sources.push(Box::new("//#endregion"));
  }

  RenderEcmaModuleOutput { sources: Some(Arc::from(sources.into_boxed_slice())), warnings }
}

/// Collapses a module's `sourcemap_chain` together with the oxc codegen map
/// (`codegen_map`, always the last element of the chain) into the final module
/// sourcemap. Returns `None` when there is no sourcemap to emit.
///
/// Like Rollup, the result keeps only the sources and names that a traced token
/// references. So a transform that returns `{ mappings: '' }` or omits its map
/// removes the module's earlier code, and the names that came with it.
///
/// A `SOURCEMAP_BROKEN` warning is pushed onto `warnings` for every plugin that
/// omitted its sourcemap, since that breaks the mapping chain for `module_id`.
pub fn collapse_module_sourcemap(
  sourcemap_chain: &[SourcemapChainElement],
  codegen_map: Option<SourceMap>,
  module_id: &str,
  warnings: &mut Vec<BuildDiagnostic>,
) -> Option<SourceMap> {
  if sourcemap_chain.is_empty() {
    return codegen_map;
  }

  let empty = empty_sourcemap();
  let mut owned_chain: Vec<&SourceMap> = Vec::with_capacity(sourcemap_chain.len() + 1);

  let mut original_content: Option<&str> = None;
  for element in sourcemap_chain {
    match element {
      SourcemapChainElement::Transform((_, sourcemap)) | SourcemapChainElement::Load(sourcemap) => {
        owned_chain.push(sourcemap);
      }
      SourcemapChainElement::Omitted { plugin_name, .. } => {
        owned_chain.push(&empty);
        warnings.push(
          BuildDiagnostic::sourcemap_broken(plugin_name.to_string(), Some(module_id.to_string()))
            .with_severity_warning(),
        );
      }
      SourcemapChainElement::Null { original_content: content, .. } => {
        // `map: null` does not remap positions, so it contributes nothing
        // to `collapse_sourcemaps`. We only keep its pre-transform content as a
        // fallback when there is no real map to provide one.
        if original_content.is_none() {
          original_content = Some(content);
        }
      }
    }
  }
  if owned_chain.is_empty() {
    // Only `map: null` transforms touched this module: keep the codegen
    // map's positions but swap in the pre-transform source content so the
    // transformed/injected code does not leak into `sourcesContent`.
    codegen_map.map(|mut map| {
      if let Some(content) = original_content {
        map.set_source_contents(vec![Some(content)]);
      }
      map
    })
  } else {
    if let Some(sourcemap) = codegen_map.as_ref() {
      owned_chain.push(sourcemap);
    }
    let collapsed = if let [map] = owned_chain.as_slice() {
      (*map).clone().into_owned()
    } else {
      collapse_sourcemaps(&owned_chain)
    };
    Some(prune_unreferenced_sourcemap_entries(collapsed))
  }
}

fn prune_unreferenced_sourcemap_entries(sourcemap: SourceMap) -> SourceMap {
  let mut parts = sourcemap.into_parts();
  let mut source_used = vec![false; parts.sources.len()];
  let mut name_used = vec![false; parts.names.len()];
  let mut source_out_of_range = false;
  let mut name_out_of_range = false;
  for token in &parts.tokens {
    if let Some(source_id) = token.get_source_id() {
      if let Some(used) = source_used.get_mut(source_id as usize) {
        *used = true;
      } else {
        source_out_of_range = true;
      }
    }
    if let Some(name_id) = token.get_name_id() {
      if let Some(used) = name_used.get_mut(name_id as usize) {
        *used = true;
      } else {
        name_out_of_range = true;
      }
    }
  }
  let all_sources_used = !source_out_of_range && source_used.iter().all(|&u| u);
  let all_names_used = !name_out_of_range && name_used.iter().all(|&u| u);
  if all_sources_used && all_names_used {
    return SourceMap::from_parts(parts);
  }
  let mut new_source_ids = Vec::with_capacity(parts.sources.len());
  let mut kept_sources = Vec::new();
  let mut kept_contents = Vec::new();
  let mut has_any_content = false;
  let mut source_contents = parts.source_contents.into_iter();
  for (idx, source) in parts.sources.into_iter().enumerate() {
    let content = source_contents.next().flatten();
    if source_used[idx] {
      new_source_ids.push(Some(u32::try_from(kept_sources.len()).expect("source count fits u32")));
      kept_sources.push(source);
      has_any_content |= content.is_some();
      kept_contents.push(content);
    } else {
      new_source_ids.push(None);
    }
  }
  parts.sources = kept_sources;
  parts.source_contents = if has_any_content { kept_contents } else { Vec::new() };

  let mut new_name_ids = Vec::with_capacity(parts.names.len());
  let mut kept_names = Vec::new();
  for (idx, name) in parts.names.into_iter().enumerate() {
    if name_used[idx] {
      new_name_ids.push(Some(u32::try_from(kept_names.len()).expect("name count fits u32")));
      kept_names.push(name);
    } else {
      new_name_ids.push(None);
    }
  }
  parts.names = kept_names;

  if !parts.tokens.is_empty() {
    let mut token_builder = SourceMapBuilder::default();
    for token in &parts.tokens {
      let source_id =
        token.get_source_id().and_then(|id| new_source_ids.get(id as usize).copied().flatten());
      let name_id = if source_id.is_some() {
        token.get_name_id().and_then(|id| new_name_ids.get(id as usize).copied().flatten())
      } else {
        None
      };
      token_builder.add_token(
        token.get_dst_line(),
        token.get_dst_col(),
        if source_id.is_some() { token.get_src_line() } else { 0 },
        if source_id.is_some() { token.get_src_col() } else { 0 },
        source_id,
        name_id,
      );
    }
    parts.tokens = token_builder.into_sourcemap().into_parts().tokens;
  }
  parts.token_chunks = None;
  SourceMap::from_parts(parts)
}

#[cfg(test)]
mod tests {
  use super::collapse_module_sourcemap;
  use arcstr::ArcStr;
  use insta::assert_snapshot;
  use rolldown_common::{PluginIdx, SourcemapChainElement};
  use rolldown_sourcemap::{SourceMap, SourceMapBuilder, empty_sourcemap};

  const MODULE_ID: &str = "/project/src/index.js";

  fn plugin_idx() -> PluginIdx {
    PluginIdx::from_usize(0)
  }

  /// Builds a sourcemap for `source`/`content` with two tokens at columns 0 and 6 of the
  /// first line. The tokens are identity mappings so they line up across maps and survive
  /// `collapse_sourcemaps`' source-view lookups.
  fn map(source: &str, content: &str) -> SourceMap {
    let mut builder = SourceMapBuilder::default();
    let source_id = builder.add_source_and_content(source, content);
    builder.add_token(0, 0, 0, 0, Some(source_id), None);
    builder.add_token(0, 6, 0, 6, Some(source_id), None);
    builder.into_sourcemap().into_owned()
  }

  /// The oxc codegen map is always the last element of the chain and, in production, uses
  /// the module id as its source name (see `NormalModule::render`).
  fn codegen_map(content: &str) -> SourceMap {
    map(MODULE_ID, content)
  }

  fn omitted() -> SourcemapChainElement {
    SourcemapChainElement::Omitted { plugin_idx: plugin_idx(), plugin_name: ArcStr::from("plugin") }
  }

  fn null(original_content: &str) -> SourcemapChainElement {
    SourcemapChainElement::Null {
      plugin_idx: plugin_idx(),
      original_content: ArcStr::from(original_content),
    }
  }

  #[test]
  fn empty_chain_returns_codegen_map_unchanged() {
    let mut warnings = vec![];
    let result =
      collapse_module_sourcemap(&[], Some(codegen_map("const a = 1;\n")), MODULE_ID, &mut warnings)
        .unwrap();
    assert_snapshot!(result.to_json_string(), @r#"{"version":3,"names":[],"sources":["/project/src/index.js"],"sourcesContent":["const a = 1;\n"],"mappings":"AAAA,MAAM"}"#);
    assert!(warnings.is_empty());

    assert!(collapse_module_sourcemap(&[], None, MODULE_ID, &mut warnings).is_none());
  }

  #[test]
  fn omitted_only_with_codegen_map_uses_module_id_as_source() {
    let mut warnings = vec![];
    let result = collapse_module_sourcemap(
      &[omitted()],
      Some(codegen_map("const a = 1;\n")),
      MODULE_ID,
      &mut warnings,
    )
    .unwrap();
    assert_snapshot!(result.to_json_string(), @r#"{"version":3,"names":[],"sources":[],"mappings":""}"#);
    // An omitted sourcemap breaks the chain, so a `SOURCEMAP_BROKEN` warning is emitted.
    assert_eq!(warnings.len(), 1);
  }

  #[test]
  fn null_only_with_codegen_map_swaps_source_content() {
    // `map: null` keeps the codegen map's positions/sources but swaps in the pre-transform
    // source content so the transformed code does not leak into `sourcesContent`.
    let mut warnings = vec![];
    let result = collapse_module_sourcemap(
      &[null("const a = 1;\n")],
      Some(codegen_map("a(1);\n")),
      MODULE_ID,
      &mut warnings,
    )
    .unwrap();
    assert_snapshot!(result.to_json_string(), @r#"{"version":3,"names":[],"sources":["/project/src/index.js"],"sourcesContent":["const a = 1;\n"],"mappings":"AAAA,MAAM"}"#);
    assert!(warnings.is_empty());
  }

  #[test]
  fn null_only_without_codegen_map_returns_none() {
    let mut warnings = vec![];
    assert!(
      collapse_module_sourcemap(&[null("const a = 1;\n")], None, MODULE_ID, &mut warnings)
        .is_none()
    );
  }

  #[test]
  fn real_map_is_not_replaced_by_placeholder() {
    // A real `Load`/`Transform` map is kept as-is; its sources/content win over the module
    // id placeholder and its mappings flow through the collapse.
    let mut warnings = vec![];
    let load = SourcemapChainElement::Load(map("loaded.js", "const a = 1;\n"));
    let result =
      collapse_module_sourcemap(&[load], Some(codegen_map("a(1);\n")), MODULE_ID, &mut warnings)
        .unwrap();
    assert_snapshot!(result.to_json_string(), @r#"{"version":3,"names":[],"sources":["loaded.js"],"sourcesContent":["const a = 1;\n"],"mappings":"AAAA,MAAM"}"#);
    assert!(warnings.is_empty());
  }

  #[test]
  fn map_without_mappings_drops_earlier_sources_and_names() {
    // `{ mappings: '' }` or an omitted map after a real map leaves no token that reaches the
    // original code, so its source, content and names must not survive (#11176).
    let mut builder = SourceMapBuilder::default();
    let source_id = builder.add_source_and_content(MODULE_ID, "const secret = 1;\n");
    let name_id = builder.add_name("secret");
    builder.add_token(0, 6, 0, 6, Some(source_id), Some(name_id));
    let earlier =
      SourcemapChainElement::Transform((plugin_idx(), builder.into_sourcemap().into_owned()));
    for last in [SourcemapChainElement::Transform((plugin_idx(), empty_sourcemap())), omitted()] {
      let mut warnings = vec![];
      let result = collapse_module_sourcemap(
        &[earlier.clone(), last],
        Some(codegen_map("proxy();\n")),
        MODULE_ID,
        &mut warnings,
      )
      .unwrap();
      assert_eq!(result.to_json_string(), r#"{"version":3,"names":[],"sources":[],"mappings":""}"#);
    }
  }
}
