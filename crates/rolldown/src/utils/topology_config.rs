//! Declarative topology heuristics, overfetch bounds, and semantic grouping policies for
//! `generate_stage`.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CycleMergingPolicy {
  #[default]
  Forbid,
  AllowWrapped,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CommonChunkCoalescing {
  #[default]
  ByEntrySet,
  Off,
  Aggressive,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DependencyBoundary {
  /// Default Rolldown behavior (`include_dependencies_recursively: true`).
  #[default]
  All,
  /// Only seed modules (`include_dependencies_recursively: false`).
  None,
  /// App/workspace seed modules only recursively pull app/workspace modules (stopping at
  /// `node_modules`), while vendor seed modules only pull vendor dependencies.
  SameOriginKind,
  /// Seed modules only recursively pull dependencies within the same `package.json` name.
  SamePackage,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkSizeBudget {
  pub min_bytes: u32,
  pub target_bytes: Option<u32>,
  pub max_bytes: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GroupNamingPolicy {
  Static(String),
  ByPackage { prefix: String, disambiguate_version: bool },
  ByDirectory { base_name: String, prefix: Option<String>, depth: u8 },
  ByEcosystem { prefix: String },
  ByConsumers { prefix: String },
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GroupRuleConfig {
  pub packages: Vec<String>,
  pub paths: Vec<String>,
  pub naming: Option<GroupNamingPolicy>,
  pub boundary: DependencyBoundary,
  pub target_bytes: Option<u32>,
  pub overfetch_budget_bytes: Option<u32>,
  pub is_mf_shared: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TopologyConfig {
  pub chunk_size: Option<ChunkSizeBudget>,
  pub max_initial_requests: Option<u16>,
  pub max_async_requests: Option<u16>,
  pub overfetch_budget_bytes: Option<u32>,
  pub cycle_merging: Option<CycleMergingPolicy>,
  pub common_chunk_coalescing: Option<CommonChunkCoalescing>,
  /// Per-match-group semantic rule, indexed by `match_group_index`.
  pub group_rules: Vec<GroupRuleConfig>,
  /// Per-match-group dependency boundary, indexed by `match_group_index`.
  pub group_boundaries: Vec<DependencyBoundary>,
  /// Per-match-group target size override, indexed by `match_group_index`.
  pub group_target_bytes: Vec<Option<u32>>,
  /// Per-match-group overfetch budget override, indexed by `match_group_index`.
  pub group_overfetch_budget_bytes: Vec<Option<u32>>,
  /// Package names (`package.json#name`) isolated as Module Federation shared singletons.
  /// Non-shared groups (`DependencyBoundary != All` or lacking an explicit `mf-shared-*` allow)
  /// never recursively pull modules belonging to these packages.
  pub mf_shared_packages: Vec<String>,
  /// Per-match-group flag indicating whether the group is itself a synthesized `mf-shared-*` group
  /// allowed to capture `mf_shared_packages`.
  pub group_is_mf_shared: Vec<bool>,
  /// Whether UserDefinedEntry chunks represent Module Federation remote container entries that must
  /// not absorb non-eager exposed modules or shared chunks.
  pub preserve_container_entries: bool,
  /// Optional standalone runtime chunk name (such as `"runtime"`), preventing runtime merging.
  #[serde(default)]
  pub standalone_runtime_chunk: Option<String>,
  /// Whether shared chunks with multiple consumers are named `<prefix>_<consumer>_<consumer>`.
  #[serde(default)]
  pub consumer_shared_chunk_names: bool,
}

pub const TOPOLOGY_KEY_HEADER: &str = "\0__native_topology_config_json:";

fn kebab_case_segment(value: &str) -> String {
  let mut expanded = String::with_capacity(value.len() + 8);
  let bytes = value.as_bytes();
  for (i, ch) in value.char_indices() {
    if ch.is_ascii_uppercase()
      && i > 0
      && (bytes[i - 1].is_ascii_lowercase() || bytes[i - 1].is_ascii_digit())
    {
      expanded.push('-');
    }
    expanded.push(ch.to_ascii_lowercase());
  }
  let mut out = String::with_capacity(expanded.len());
  let mut prev_dash = false;
  for ch in expanded.chars() {
    if ch.is_ascii_alphanumeric() {
      out.push(ch);
      prev_dash = false;
    } else if !prev_dash && !out.is_empty() {
      out.push('-');
      prev_dash = true;
    }
  }
  while out.ends_with('-') {
    out.pop();
  }
  out
}

fn is_pathless_route_segment(segment: &str) -> bool {
  segment.starts_with('_')
    || (segment.starts_with('(') && segment.ends_with(')') && segment.len() > 2)
}

fn route_path_segment_name(segment: &str) -> String {
  let trimmed = segment.strip_suffix('_').unwrap_or(segment);
  let unwrapped = trimmed.strip_prefix('{').and_then(|s| s.strip_suffix('}')).unwrap_or(trimmed);
  let after_dash = unwrapped.strip_prefix('-').unwrap_or(unwrapped);
  if let Some(param) = after_dash.strip_prefix('$') {
    if param.is_empty() { "splat".to_owned() } else { kebab_case_segment(param) }
  } else {
    kebab_case_segment(trimmed)
  }
}

/// Computes the deterministic `routeChunkName` for a TanStack Router `?tsr-split=<grouping>` module ID
/// when `<grouping>` uses named route properties (`component`, `loader`, `errorComponent`, etc.).
/// Retains the established `root`, `home`, `code`, and `autopilot` labels for their exact
/// `_coreShell` route paths; all other paths use generic route/layout labels.
pub fn route_split_chunk_name(id: &str) -> Option<String> {
  let normalized = id.replace('\\', "/");
  let (before_query, query) = normalized.split_once("?tsr-split=")?;
  let grouping_str = query.split(['&', '#']).next().unwrap_or(query);
  if grouping_str.is_empty() {
    return None;
  }
  let mut grouping: Vec<&str> = grouping_str.split("---").collect();
  if grouping.is_empty()
    || !grouping
      .iter()
      .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphabetic()))
  {
    return None;
  }
  let route_rel = if let Some(idx) = before_query.rfind("/src/routes/") {
    &before_query[idx + "/src/routes/".len()..]
  } else if let Some(idx) = before_query.rfind("/routes/") {
    &before_query[idx + "/routes/".len()..]
  } else if let Some(rest) = before_query.strip_prefix("src/routes/") {
    rest
  } else {
    before_query.strip_prefix("routes/")?
  };
  let route_file = route_rel
    .strip_suffix(".tsx")
    .or_else(|| route_rel.strip_suffix(".ts"))
    .or_else(|| route_rel.strip_suffix(".jsx"))
    .or_else(|| route_rel.strip_suffix(".js"))?;
  if route_file.is_empty() || route_file == "__root" {
    return None;
  }
  let suffix = if grouping.contains(&"component") {
    String::new()
  } else {
    grouping.sort_unstable();
    let kinds: Vec<String> = grouping.iter().map(|g| kebab_case_segment(g)).collect();
    format!("--{}", kinds.join("-"))
  };
  let established = match route_file {
    "_coreShell/_home/index" => Some("root"),
    "_coreShell/_home/home" => Some("home"),
    "_coreShell/_code/code.index" => Some("code"),
    "_coreShell/_autopilot/autopilot.index" => Some("autopilot"),
    _ => None,
  };
  if let Some(name) = established {
    return Some(format!("{name}{suffix}"));
  }
  let mut segments: Vec<&str> = route_file.split('/').flat_map(|part| part.split('.')).collect();
  let layout = segments.last() == Some(&"route");
  if layout {
    segments.pop();
  }
  if let Some(&last) = segments.last()
    && is_pathless_route_segment(last)
  {
    let inner = last.strip_prefix('(').and_then(|s| s.strip_suffix(')')).unwrap_or(last);
    return Some(format!("layout-{}{suffix}", kebab_case_segment(inner)));
  }
  let path_part: Vec<String> = segments
    .into_iter()
    .filter(|seg| !is_pathless_route_segment(seg))
    .map(route_path_segment_name)
    .filter(|s| !s.is_empty())
    .collect();
  let joined = if path_part.is_empty() { "index".to_owned() } else { path_part.join("-") };
  Some(format!("{}-{joined}{suffix}", if layout { "layout" } else { "route" }))
}

/// Extracts `/* webpackChunkName: "..." */` from the `import(...)` call at `import_expr_span`.
pub fn extract_webpack_chunk_name(
  source: &str,
  import_expr_span: oxc::span::Span,
) -> Option<String> {
  let start = (import_expr_span.start as usize).min(source.len());
  let end = (import_expr_span.end as usize).min(source.len());
  if start >= end {
    return None;
  }
  let window = &source[start..end];
  let mut rest = window;
  while let Some(c_start) = rest.find("/*") {
    let after_open = &rest[c_start + 2..];
    let Some(c_end) = after_open.find("*/") else {
      break;
    };
    let comment = &after_open[..c_end];
    if let Some(key_idx) = comment.find("webpackChunkName") {
      let tail = comment[key_idx + "webpackChunkName".len()..].trim_start();
      if let Some(after_colon) = tail.strip_prefix(':') {
        let after_colon = after_colon.trim_start();
        if let Some(quote) = after_colon.chars().next().filter(|&c| c == '"' || c == '\'') {
          let value_rest = &after_colon[1..];
          if let Some(end_quote) = value_rest.find(quote) {
            let candidate = value_rest[..end_quote].trim();
            if !candidate.is_empty()
              && candidate.len() <= 128
              && candidate
                .bytes()
                .next()
                .is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
              && candidate
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
            {
              return Some(candidate.to_owned());
            }
          }
        }
      }
    }
    rest = &after_open[c_end + 2..];
  }
  None
}

/// Reviewed third-party `@tanstack/devtools` and `@tanstack/query-devtools` dynamic import labels.
pub fn devtools_origin_label(importer_id: &str, request: &str) -> Option<&'static str> {
  let normalized = importer_id.replace('\\', "/");
  if normalized.contains("/node_modules/@tanstack/devtools/") {
    if let Some(rest) = request.strip_prefix("./mount-impl/")
      && rest.ends_with(".js")
      && !rest.contains('/')
    {
      return Some("tanstack-devtools-mount");
    }
    if let Some(rest) = request.strip_prefix("../devtools/")
      && rest.ends_with(".js")
      && !rest.contains('/')
    {
      return Some("tanstack-devtools-panel");
    }
  } else if normalized.contains("/node_modules/@tanstack/query-devtools/") {
    if let Some(rest) = request.strip_prefix("./DevtoolsComponent-")
      && rest.ends_with(".js")
      && !rest.contains('/')
    {
      return Some("tanstack-query-devtools-floating");
    }
    if let Some(rest) = request.strip_prefix("./DevtoolsPanelComponent-")
      && rest.ends_with(".js")
      && !rest.contains('/')
    {
      return Some("tanstack-query-devtools-panel");
    }
  }
  None
}

/// Formats a consumer-set shared chunk name `<prefix>_<label>_<label>` (shortened to `<= 80` chars
/// with `<prefix>_<first>_<n>-more-<hash8>` when longer).
pub fn format_consumer_shared_chunk_name(prefix: &str, mut labels: Vec<String>) -> Option<String> {
  labels.sort();
  labels.dedup();
  if labels.is_empty() {
    return None;
  }
  let chunk_set_key = labels.join("_");
  let mut name = format!("{prefix}_{chunk_set_key}");
  if name.len() > 80 {
    let hash = rolldown_utils::xxhash::xxhash_with_base(chunk_set_key.as_bytes(), 16);
    let hash8 = &hash[..8.min(hash.len())];
    name = format!("{prefix}_{}_{}-more-{hash8}", labels[0], labels.len() - 1);
  }
  Some(name)
}

#[expect(clippy::needless_pass_by_value, reason = "downstream callers pass owned TopologyConfig")]
pub fn attach(options: &mut rolldown_common::BundlerOptions, config: TopologyConfig) {
  let encoded = serde_json::to_string(&config).expect("TopologyConfig is always serializable");
  let tag = format!("{TOPOLOGY_KEY_HEADER}{encoded}");
  let labels = options.drop_labels.get_or_insert_default();
  labels.retain(|label| !label.starts_with(TOPOLOGY_KEY_HEADER));
  labels.push(tag);
}

pub fn get_from_raw(options: &rolldown_common::BundlerOptions) -> Option<Arc<TopologyConfig>> {
  for label in options.drop_labels.as_deref().unwrap_or_default() {
    if let Some(payload) = label.strip_prefix(TOPOLOGY_KEY_HEADER)
      && let Ok(parsed) = serde_json::from_str::<TopologyConfig>(payload)
    {
      return Some(Arc::new(parsed));
    }
  }
  None
}

pub fn get(options: &rolldown_common::NormalizedBundlerOptions) -> Option<Arc<TopologyConfig>> {
  for label in &options.drop_labels {
    if let Some(payload) = label.strip_prefix(TOPOLOGY_KEY_HEADER)
      && let Ok(parsed) = serde_json::from_str::<TopologyConfig>(payload)
    {
      return Some(Arc::new(parsed));
    }
  }
  None
}

#[cfg(test)]
mod route_name_tests {
  use super::route_split_chunk_name;

  #[test]
  fn route_split_names_keep_established_aliases_and_groupings() {
    let cases = [
      ("_coreShell/_home/index.tsx", "component", "root"),
      ("_coreShell/_home/home.tsx", "component", "home"),
      ("_coreShell/_code/code.index.tsx", "component", "code"),
      ("_coreShell/_autopilot/autopilot.index.tsx", "component", "autopilot"),
      ("(development-auth)/loginDev.tsx", "component", "route-login-dev"),
      ("_homeStandalone/showcase.$.tsx", "loader", "route-showcase-splat--loader"),
      ("_homeStandalone/showcase.$.tsx", "component---pendingComponent", "route-showcase-splat"),
      ("_coreShell/route.tsx", "component", "layout-core-shell"),
      ("_coreShell/_home/route.tsx", "notFoundComponent", "layout-home--not-found-component"),
    ];
    for (path, grouping, expected) in cases {
      let id = format!("/app/src/routes/{path}?tsr-split={grouping}");
      assert_eq!(route_split_chunk_name(&id).as_deref(), Some(expected), "{id}");
    }
  }

  #[test]
  fn route_split_aliases_are_exact_and_keep_normalization_and_suffixes() {
    for (id, expected) in [
      (r"C:\app\src\routes\_coreShell\_home\index.ts?tsr-split=loader", "root--loader"),
      (
        "routes/_coreShell/_code/code.index.jsx?tsr-split=loader---errorComponent&v=1",
        "code--error-component-loader",
      ),
      ("src/routes/_coreShell/_home/home.js?tsr-split=component---loader", "home"),
      ("/app/routes/_coreShell/_home/index-copy.tsx?tsr-split=component", "route-index-copy"),
      ("/app/routes/_other/_home/index.tsx?tsr-split=component", "route-index"),
      ("/app/routes/products.$id.tsx?tsr-split=loader", "route-products-id--loader"),
    ] {
      assert_eq!(route_split_chunk_name(id).as_deref(), Some(expected), "{id}");
    }
    for id in [
      "/app/src/routes/__root.tsx?tsr-split=component",
      "/app/src/routes/_coreShell/_home/index.tsx?tsr-split=",
      "/app/src/routes/_coreShell/_home/index.tsx?tsr-split=component---",
      "/app/src/routes/_coreShell/_home/index.json?tsr-split=component",
    ] {
      assert_eq!(route_split_chunk_name(id), None, "{id}");
    }
  }
}
