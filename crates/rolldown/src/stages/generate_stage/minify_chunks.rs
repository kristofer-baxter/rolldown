use std::{collections::BTreeMap, sync::OnceLock};

use oxc::{
  codegen::{self, CodegenOptions, CommentOptions},
  minifier::MinifierOptions as OxcMinifierOptions,
};
use oxc_allocator::AllocatorPool;
use rolldown_common::{
  InstantiatedChunk, InstantiationKind, MinifyOptions, NormalizedBundlerOptions, Output,
};
use rolldown_ecmascript::EcmaCompiler;
use rolldown_error::BuildResult;
use rolldown_sourcemap::collapse_sourcemaps;
use rolldown_utils::rayon::{IntoParallelRefMutIterator, ParallelIterator};
use rustc_hash::FxHashSet;

use crate::type_alias::IndexInstantiatedChunks;

use super::GenerateStage;

type ChunkMinifyResult =
  Option<(rolldown_common::ChunkIdx, [u8; 32], Vec<String>, arcstr::ArcStr, bool)>;

impl GenerateStage<'_> {
  #[tracing::instrument(level = "debug", skip_all)]
  pub fn minify_chunks(
    &mut self,
    chunks: &mut IndexInstantiatedChunks,
    skip_sourcemap_chunks: &FxHashSet<rolldown_common::ChunkIdx>,
  ) -> BuildResult<Option<BTreeMap<String, Option<String>>>> {
    let options = self.options;
    let Some((compress, base_minify_options, remove_whitespace, ascii_only)) =
      chunk_minify_options(&options.minify)
    else {
      return Ok(None);
    };
    let minify_options = OxcMinifierOptions {
      mangle: base_minify_options.mangle.clone(),
      compress: base_minify_options.compress.clone(),
      mangle_properties: self
        .cache
        .as_ref()
        .and_then(|c| c.mangle_properties_override.clone())
        .or_else(|| base_minify_options.mangle_properties.clone()),
    };
    let allocator_pool = AllocatorPool::new(rayon::current_num_threads());
    let source_type = options.format.source_type().with_jsx(true);
    let eval_modules = self
      .link_output
      .module_table
      .iter()
      .filter_map(|module| module.as_normal())
      .filter(|module| module.meta.has_eval())
      .map(|module| module.id.as_str())
      .collect::<FxHashSet<_>>();

    let property_mangle_cache = OnceLock::new();
    let is_multi_chunk =
      chunks.iter().filter(|chunk| is_minifiable_ecma_chunk(chunk)).take(2).count() > 1;
    let cache_ref = self.cache.as_deref();
    let (shared_mangle_properties, mut any_chunk_mutates_object_prototype) =
      collect_shared_mangle_properties(
        chunks,
        minify_options.mangle_properties.as_ref(),
        is_multi_chunk && minify_options.compress.is_some(),
        source_type,
      )?;
    if let Some(shared_opts) = &shared_mangle_properties {
      let _ = property_mangle_cache.set(shared_opts.cache.clone());
    }
    let mut proto_verdicts: Option<rustc_hash::FxHashMap<[u8; 32], bool>> = None;
    if is_multi_chunk && shared_mangle_properties.is_none() && minify_options.compress.is_some() {
      use rolldown_utils::{
        hash_placeholder::{HASH_PLACEHOLDER_LEFT_FINDER, visit_with_placeholders_defaulted},
        rayon::IntoParallelRefIterator,
      };
      use xxhash_rust::xxh3::Xxh3;
      let verdicts: Vec<BuildResult<Option<([u8; 32], bool)>>> = chunks
        .par_iter()
        .map(|chunk| -> BuildResult<Option<([u8; 32], bool)>> {
          if !is_minifiable_ecma_chunk(chunk) {
            return Ok(None);
          }
          let pre_str = chunk.content.try_as_inner_str()?;
          let mut h0 = Xxh3::with_seed(0x9e37_79b9_7f4a_7c15);
          let mut h1 = Xxh3::with_seed(0x6a09_e667_f3bc_c908);
          visit_with_placeholders_defaulted(
            pre_str,
            &HASH_PLACEHOLDER_LEFT_FINDER,
            |_| true,
            |bytes| {
              h0.update(bytes);
              h1.update(bytes);
            },
          );
          let mut raw_hash = [0u8; 32];
          raw_hash[..16].copy_from_slice(&h0.digest128().to_le_bytes());
          raw_hash[16..].copy_from_slice(&h1.digest128().to_le_bytes());
          if let Some(c) = cache_ref
            && let Some(&cached_mutates) = c.cached_chunk_prototype_mutations.get(&raw_hash)
          {
            return Ok(Some((raw_hash, cached_mutates)));
          }
          let allocator_guard = allocator_pool.get();
          let parsed = oxc::parser::Parser::new(&allocator_guard, pre_str, source_type)
            .with_options(oxc::parser::ParseOptions {
              preserve_parens: false,
              ..oxc::parser::ParseOptions::default()
            })
            .parse();
          let mutates = parsed.fatal_error
            || !parsed.diagnostics.is_empty()
            || oxc::minifier::program_may_mutate_object_prototype(&parsed.program);
          Ok(Some((raw_hash, mutates)))
        })
        .collect();
      let mut map = rustc_hash::FxHashMap::default();
      for res in verdicts {
        if let Some((raw_hash, mutates)) = res? {
          if mutates {
            any_chunk_mutates_object_prototype = true;
          }
          map.insert(raw_hash, mutates);
        }
      }
      proto_verdicts = Some(map);
    }
    let mut minify_options = minify_options;
    if any_chunk_mutates_object_prototype && let Some(compress) = &mut minify_options.compress {
      compress.set_inline_enum_members(false);
    }
    let deferred_modules = self
      .link_output
      .module_table
      .iter()
      .filter_map(|module| module.as_normal())
      .filter(|module| !self.link_output.metas[module.idx].deferred_imports.is_empty())
      .map(|module| module.id.as_str())
      .collect::<FxHashSet<_>>();
    let can_cache_minify = minify_options.mangle_properties.is_none();
    let minify_results: Vec<ChunkMinifyResult> = chunks
      .par_iter_mut()
      .map(|chunk| -> BuildResult<ChunkMinifyResult> {
        if !is_minifiable_ecma_chunk(chunk) {
          return Ok(None);
        }
        let pre_state = if can_cache_minify {
          use rolldown_utils::hash_placeholder::{
            HASH_PLACEHOLDER_LEFT_FINDER, extract_hash_placeholders, replace_placeholder_with_hash,
            visit_with_placeholders_defaulted,
          };
          use xxhash_rust::xxh3::Xxh3;
          let pre_str = chunk.content.try_as_inner_str()?;
          let mut h0 = Xxh3::with_seed(0x9e37_79b9_7f4a_7c15);
          let mut h1 = Xxh3::with_seed(0x6a09_e667_f3bc_c908);
          h0.update(&[u8::from(any_chunk_mutates_object_prototype)]);
          h1.update(&[u8::from(any_chunk_mutates_object_prototype)]);
          visit_with_placeholders_defaulted(
            pre_str,
            &HASH_PLACEHOLDER_LEFT_FINDER,
            |_| true,
            |bytes| {
              h0.update(bytes);
              h1.update(bytes);
            },
          );
          let mut out = [0u8; 32];
          out[..16].copy_from_slice(&h0.digest128().to_le_bytes());
          out[16..].copy_from_slice(&h1.digest128().to_le_bytes());
          let placeholders: Vec<String> =
            extract_hash_placeholders(pre_str, &HASH_PLACEHOLDER_LEFT_FINDER)
              .into_iter()
              .map(str::to_string)
              .collect();
          if let Some(c) = cache_ref
            && let Some(&digest) = c.current_chunk_map_digests.get(&chunk.originate_from)
            && let Some(hit) = c.cached_chunk_sourcemaps.get(&digest)
            && hit.pre_minify_hash == out
            && hit.pre_minify_placeholders.len() == placeholders.len()
            && let Some(ref post_code) = hit.post_minify_content
            && (options.sourcemap.is_none()
              || skip_sourcemap_chunks.contains(&chunk.originate_from))
          {
            let final_post_code = if hit.pre_minify_placeholders == placeholders {
              post_code.clone()
            } else {
              let remap: rustc_hash::FxHashMap<String, &str> = hit
                .pre_minify_placeholders
                .iter()
                .cloned()
                .zip(placeholders.iter().map(String::as_str))
                .collect();
              let replaced = replace_placeholder_with_hash(
                post_code.as_str(),
                &remap,
                &HASH_PLACEHOLDER_LEFT_FINDER,
              );
              arcstr::ArcStr::from(replaced.as_ref())
            };
            chunk.content = final_post_code.to_string().into();
            return Ok(Some((chunk.originate_from, out, placeholders, final_post_code, true)));
          }
          Some((out, placeholders))
        } else {
          None
        };
        let mut minify_options = minify_options.clone();
        if let Some(shared_mangle_props) = &shared_mangle_properties {
          minify_options.mangle_properties = Some(shared_mangle_props.clone());
        }
        if let InstantiationKind::Ecma(meta) = &chunk.kind
          && meta.rendered_chunk.module_ids.iter().any(|id| eval_modules.contains(id.as_str()))
        {
          // Oxc DCE drops writes in initializer closures even when another closure's direct
          // eval observes the outer binding. Keep compression off for these module records.
          minify_options.compress = None;
        }
        if let InstantiationKind::Ecma(meta) = &chunk.kind
          && meta.rendered_chunk.module_ids.iter().any(|id| deferred_modules.contains(id.as_str()))
          && let Some(compress) = &mut minify_options.compress
        {
          // Even an unused deferred binding contributes an evaluation dependency: deleting
          // its request changes ReadyForSyncExecution for async/cyclic module graphs.
          compress.treeshake.invalid_import_side_effects = true;
        }
        let codegen_options = CodegenOptions {
          minify: remove_whitespace,
          ascii_only: options.charset.is_ascii() || ascii_only,
          comments: CommentOptions {
            normal: !remove_whitespace,
            jsdoc: options.comments.jsdoc && !remove_whitespace,
            annotation: options.comments.annotation,
            legal: if options.comments.legal || !remove_whitespace {
              codegen::LegalComment::Inline
            } else {
              codegen::LegalComment::None
            },
          },
          ..CodegenOptions::default()
        };

        let allocator_guard = allocator_pool.get();
        // The minify map borrows the pre-minify `chunk.content` (as `sourcesContent`,
        // which the collapse discards), so collapse before swapping in the minified
        // content instead of paying an `into_owned` copy of the whole chunk text.
        let (minified_content, collapsed_map) = {
          // TODO: Do we need to ensure `chunk.preliminary_filename` to be absolute path?
          let (minified_content, new_map, chunk_property_mangle_cache) =
            EcmaCompiler::dce_or_minify(
              &allocator_guard,
              chunk.content.try_as_inner_str()?,
              source_type,
              chunk.map.is_some(),
              chunk.preliminary_filename.as_str(),
              compress,
              minify_options,
              codegen_options,
            )?;
          if let Some(cache) = chunk_property_mangle_cache {
            let _ = property_mangle_cache.set(cache);
          }
          let collapsed_map = match (&chunk.map, &new_map) {
            (Some(origin_map), Some(new_map)) => Some(collapse_sourcemaps(&[origin_map, new_map])),
            _ => {
              // TODO: Map is dirty. Should we reset the `chunk.map` to `None`?
              None
            }
          };
          (minified_content, collapsed_map)
        };
        let res = pre_state.map(|(ph, pls)| {
          (chunk.originate_from, ph, pls, arcstr::ArcStr::from(minified_content.as_str()), false)
        });
        chunk.content = minified_content.into();
        if let Some(map) = collapsed_map {
          chunk.map = Some(map);
        }
        Ok(res)
      })
      .collect::<BuildResult<Vec<_>>>()?;
    if let Some(c) = self.cache.as_deref_mut() {
      if let Some(verdicts) = proto_verdicts {
        c.cached_chunk_prototype_mutations = verdicts;
      }
      for (chunk_idx, pre_hash, pls, post_code, _was_hit) in minify_results.into_iter().flatten() {
        c.current_chunk_minify_state.insert(chunk_idx, (pre_hash, pls, Some(post_code)));
      }
    }

    Ok(property_mangle_cache.into_inner().map(|cache| {
      cache
        .into_iter()
        .map(|(original, target)| {
          (original.into_string(), target.map(oxc_str::CompactStr::into_string))
        })
        .collect()
    }))
  }

  /// Captures code identity before hooks, only when shared property mangling requires it.
  pub(crate) fn snapshot_mangled_chunks(
    options: &NormalizedBundlerOptions,
    has_mangle_override: bool,
    outputs: &[Output],
  ) -> Option<FxHashSet<String>> {
    let mangle_active = has_mangle_override
      || chunk_minify_options(&options.minify)
        .is_some_and(|(_, minify_options, _, _)| minify_options.mangle_properties.is_some());
    mangle_active.then(|| {
      outputs
        .iter()
        .filter_map(|output| match output {
          Output::Chunk(chunk) => Some(chunk.code.clone()),
          Output::Asset(_) => None,
        })
        .collect()
    })
  }

  /// Rejects JavaScript code that bypassed the shared property map, regardless of filename.
  pub(crate) fn validate_mangle_properties_output(
    minified_chunks: Option<&FxHashSet<String>>,
    outputs: &[Output],
  ) -> BuildResult<()> {
    if let Some(minified_chunks) = minified_chunks
      && let Some(changed) = outputs.iter().find_map(|output| {
        let Output::Chunk(chunk) = output else { return None };
        (!test_d_ts_pattern(chunk.filename.as_str()) && !minified_chunks.contains(&chunk.code))
          .then_some(&chunk.filename)
      })
    {
      return Err(
        rolldown_error::BuildDiagnostic::invalid_option(
          rolldown_error::InvalidOptionType::ManglePropertiesWithLateChunkChanges(
            changed.to_string(),
          ),
        )
        .into(),
      );
    }
    Ok(())
  }
}

fn collect_shared_mangle_properties(
  chunks: &IndexInstantiatedChunks,
  mangle_properties: Option<&oxc::minifier::ManglePropertiesOptions>,
  check_proto_mutations: bool,
  source_type: oxc::span::SourceType,
) -> BuildResult<(Option<oxc::minifier::ManglePropertiesOptions>, bool)> {
  let Some(mangle_properties) = mangle_properties else {
    return Ok((None, false));
  };
  let prop_allocator = oxc::allocator::Allocator::default();
  let mut mangle_props = mangle_properties.clone();
  let mut programs = Vec::new();
  let mut any_chunk_mutates_object_prototype = false;
  for chunk in chunks.iter().filter(|chunk| is_minifiable_ecma_chunk(chunk)) {
    let pre_str = chunk.content.try_as_inner_str()?;
    let parsed = oxc::parser::Parser::new(&prop_allocator, pre_str, source_type)
      .with_options(oxc::parser::ParseOptions {
        preserve_parens: false,
        ..oxc::parser::ParseOptions::default()
      })
      .parse();
    if parsed.fatal_error || !parsed.diagnostics.is_empty() {
      return Err(
        anyhow::anyhow!(
          "failed to parse chunk {} for shared property mangling",
          chunk.preliminary_filename.as_str()
        )
        .into(),
      );
    }
    if check_proto_mutations
      && !any_chunk_mutates_object_prototype
      && oxc::minifier::program_may_mutate_object_prototype(&parsed.program)
    {
      any_chunk_mutates_object_prototype = true;
    }
    if !mangle_props.mangle_quoted {
      use oxc::ast_visit::Visit;
      QuotedPropertyReservations(&mut mangle_props.reserved).visit_program(&parsed.program);
    }
    programs.push(parsed.program);
  }
  let mut prop_mangler =
    oxc::minifier::PropertyMangler::new_in(mangle_props.clone(), &prop_allocator);
  for program in &programs {
    prop_mangler.collect(program);
  }
  let mut accepted_names: Vec<String> =
    prop_mangler.assign().keys().map(|key| regex::escape(key.as_str())).collect();
  accepted_names.sort_unstable();
  let shared_cache = prop_mangler.into_cache();
  // Freeze global eligibility so an occupied name cannot become a new local candidate.
  let pattern = if accepted_names.is_empty() {
    r"\b\B".to_string()
  } else {
    format!("^(?:{})$", accepted_names.join("|"))
  };
  mangle_props.include = oxc::minifier::ManglePropertiesOptions::from_pattern(&pattern)
    .map_err(anyhow::Error::msg)?
    .include;
  mangle_props.cache = shared_cache;
  Ok((Some(mangle_props), any_chunk_mutates_object_prototype))
}

/// Oxc's occupied-name set protects generated targets, not source names. A string that is
/// deliberately not rewritten must also reserve its original spelling across all chunks.
struct QuotedPropertyReservations<'a>(&'a mut FxHashSet<oxc::str::CompactStr>);

impl<'ast> oxc::ast_visit::Visit<'ast> for QuotedPropertyReservations<'_> {
  fn visit_string_literal(&mut self, literal: &oxc::ast::ast::StringLiteral<'ast>) {
    if let Some(value) = literal.value.as_str() {
      self.0.insert(value.into());
    }
  }

  fn visit_template_literal(&mut self, literal: &oxc::ast::ast::TemplateLiteral<'ast>) {
    if literal.expressions.is_empty()
      && let [quasi] = literal.quasis.as_slice()
      && let Some(value) = quasi.value.cooked.and_then(oxc_str::JSStr::as_str)
    {
      self.0.insert(value.into());
    }
    oxc::ast_visit::walk::walk_template_literal(self, literal);
  }
}

fn chunk_minify_options(
  options: &MinifyOptions,
) -> Option<(bool, &OxcMinifierOptions, bool, bool)> {
  match options {
    MinifyOptions::Disabled => None,
    MinifyOptions::DeadCodeEliminationOnly(options) => Some((false, options, false, false)),
    MinifyOptions::Enabled(options) => {
      Some((true, &options.options, options.remove_whitespace, options.ascii_only))
    }
  }
}

fn is_minifiable_ecma_chunk(chunk: &InstantiatedChunk) -> bool {
  matches!(chunk.kind, InstantiationKind::Ecma(_))
    && !test_d_ts_pattern(chunk.preliminary_filename.as_str())
}

fn test_d_ts_pattern(input: &str) -> bool {
  input.ends_with(".d.ts") || input.ends_with(".d.cts") || input.ends_with(".d.mts")
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_edge_cases() {
    assert!(test_d_ts_pattern(".d.ts"));
    assert!(test_d_ts_pattern(".d.cts"));
    assert!(test_d_ts_pattern(".d.mts"));
  }

  #[test]
  fn test_invalid_patterns_wrong_extension() {
    assert!(!test_d_ts_pattern(".d.tsx"));
    assert!(!test_d_ts_pattern(".d.ctsx"));
    assert!(!test_d_ts_pattern(".d.mtsx"));
    assert!(!test_d_ts_pattern(".d.cjs"));
  }

  #[test]
  fn test_invalid_patterns_missing_d() {
    assert!(!test_d_ts_pattern(".c.ts"));
    assert!(!test_d_ts_pattern(".m.ts"));
    assert!(!test_d_ts_pattern("abc.ts"));
    assert!(!test_d_ts_pattern("d.ts"));
  }

  #[test]
  fn test_invalid_patterns_extra_chars() {
    assert!(!test_d_ts_pattern(".da.ts"));
    assert!(!test_d_ts_pattern(".d.ats"));
    assert!(!test_d_ts_pattern(".d.tsa"));
  }

  #[test]
  fn test_invalid_patterns_short_input() {
    assert!(!test_d_ts_pattern(".d"));
    assert!(!test_d_ts_pattern(".t"));
    assert!(!test_d_ts_pattern("."));
    assert!(!test_d_ts_pattern(""));
    assert!(!test_d_ts_pattern(".ts")); // added test
  }
}
