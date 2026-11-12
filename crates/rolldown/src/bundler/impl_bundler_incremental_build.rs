#[cfg(feature = "experimental")]
use super::Bundler;
#[cfg(feature = "experimental")]
use crate::{Bundle, types::bundle_output::BundleOutput};
#[cfg(feature = "experimental")]
use arcstr::ArcStr;
#[cfg(feature = "experimental")]
use rolldown_common::{BundleMode, ScanMode};
#[cfg(feature = "experimental")]
use rolldown_error::BuildResult;
#[cfg(feature = "experimental")]
use std::mem;

#[cfg(feature = "experimental")]
impl Bundler {
  pub(crate) async fn with_cached_bundle<T>(
    &mut self,
    bundle_mode: BundleMode,
    with_fn: impl AsyncFnOnce(&mut Bundle) -> BuildResult<T>,
  ) -> BuildResult<T> {
    let cache = mem::take(&mut self.cache);
    let mut bundle = self.bundle_factory.create_bundle(bundle_mode, Some(cache))?;
    // Do NOT `?` the build result here. The cache must be moved back into the
    // `Bundler` on every outcome; bailing on `Err` would drop `bundle` and leave
    // `Bundler::cache` at the `default()` (snapshot = None) that `mem::take`
    // installed above, making the next HMR cycle panic in `get_snapshot()`.
    // See internal-docs/bundler-data-lifecycle/implementation.md ("Cache integrity on a failed build").
    let ret = with_fn(&mut bundle).await;
    // A failed scan or scan-only pass never reaches `bundle_write`/`bundle_generate`, whose
    // `finalize_output` clears JS-side caches on exit. Load promises must not outlive the build
    // whose native module infos they refer to.
    let ret = match ret {
      Ok(val) => match bundle.invalidate_js_side_cache_after_scan_only().await {
        Ok(()) => Ok(val),
        Err(err) => Err(err),
      },
      Err(errs) => {
        bundle.invalidate_js_side_cache_on_early_exit().await;
        Err(errs)
      }
    };
    bundle.set_mangle_properties(None);
    if ret.is_err() && bundle.cache.native_commonjs_records {
      bundle.cache.invalidate_native_relink();
      bundle.cache.cached_native_chunks.clear();
    }
    self.cache = bundle.cache;
    ret
  }

  pub fn set_omit_reused_chunk_bytes(&mut self, omit: bool) {
    self.cache.omit_reused_chunk_bytes = omit;
  }

  /// Opts native dev-server records into separate CommonJS registration and evaluation.
  /// Ordinary preserve-modules layout is unchanged. Generation rejects CommonJS tail addons.
  ///
  /// # Errors
  /// Requires incremental preserved ESM output without strict execution-order wrapping.
  pub fn set_native_commonjs_records(&mut self, enabled: bool) -> anyhow::Result<()> {
    anyhow::ensure!(
      !enabled
        || self.options.experimental.is_incremental_build_enabled()
          && crate::utils::native_records::enabled(&self.options),
      "native CommonJS records require incremental preserved ESM output without strict execution order"
    );
    if self.cache.native_commonjs_records != enabled {
      self.cache.native_commonjs_records = enabled;
      self.invalidate_native_relink();
    }
    Ok(())
  }

  /// Keeps the arenas that scans build and the capacity that linking leaves, instead of
  /// right-sizing what the incremental build retains, for tests that compare the two.
  pub fn set_keep_unused_capacity(&mut self, keep: bool) {
    self.cache.keep_unused_capacity = keep;
  }

  /// The bytes that the retained ASTs use, and the capacity of their arenas.
  pub fn retained_ast_bytes(&self) -> (usize, usize) {
    (self.cache.ast_used_bytes(), self.cache.ast_capacity_bytes())
  }

  /// The number of chunks whose `render_export_items_index_vec` map is still populated in the
  /// cached native relink state after a build or relink.
  pub fn retained_relink_export_item_maps(&self) -> usize {
    self.cache.retained_relink_export_item_maps()
  }

  pub fn last_reused_chunk_filenames(&self) -> &rustc_hash::FxHashSet<ArcStr> {
    &self.cache.last_reused_chunk_filenames
  }

  pub fn last_fast_native_relink(&self) -> bool {
    self.cache.last_fast_native_relink
  }

  /// Core linking and generation wall times of the last build, including fast-path fallbacks.
  pub fn last_core_stage_times(&self) -> (std::time::Duration, std::time::Duration) {
    (self.cache.last_core_link_time, self.cache.last_core_generate_time)
  }

  /// Retained finalized-module source entries hit by the last generation.
  pub fn last_reused_finalized_modules(&self) -> usize {
    self
      .cache
      .current_module_was_hit
      .iter()
      .filter(|idx| {
        self.cache.cached_finalized_modules.get(*idx).is_some_and(|entry| entry.sources.0.is_some())
      })
      .count()
  }

  pub fn last_link_decisions_unchanged(&self) -> bool {
    self.cache.last_link_decisions_unchanged
  }

  pub fn link_decisions_generation(&self) -> u64 {
    self.cache.link_decisions_generation
  }

  /// The constants that the last link inlined for the CommonJS exports of each module, by the
  /// module's stable id and the export's name, which the records of a page that loaded its output
  /// hold until the page reloads (#693).
  pub fn inlined_commonjs_constants(
    &self,
  ) -> Option<&std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>> {
    self.cache.inlined_commonjs_constants.as_ref()
  }

  /// Whether the last link split the default object of each JSON module that it reaches into the
  /// bindings of its properties, by the module's stable id, which decides how the records of a
  /// page that loaded its output read the properties until the page reloads (#660).
  pub fn split_json_defaults(&self) -> Option<&std::collections::BTreeMap<String, bool>> {
    self.cache.split_json_defaults.as_ref()
  }

  /// Whether the records of the last link's output evaluate each wrapped CommonJS module as they
  /// evaluate, or leave it to its first `require`, by the module's stable id, which a page that
  /// loaded the output did once (#660).
  pub fn commonjs_evaluations(&self) -> Option<&std::collections::BTreeMap<String, bool>> {
    self.cache.commonjs_evaluations.as_ref()
  }

  /// How the records of the last link's output convert the exports of each CommonJS module that ES
  /// modules import, by the module's stable id, as the records of a page that loaded the output
  /// keep doing until the page reloads (#660).
  pub fn commonjs_interop(
    &self,
  ) -> Option<&std::collections::BTreeMap<String, crate::CommonjsInterop>> {
    self.cache.commonjs_interop.as_ref()
  }

  /// Why the last generation linked the whole graph rather than relinking only what the update
  /// changed: the label of the check that rejected the fast native relink, or of what kept the
  /// generation from trying one, such as `no_cached_link`.
  pub fn last_native_relink_fallback(&self) -> Option<&'static str> {
    self.cache.last_native_relink_fallback
  }

  /// What the check behind [`Self::last_native_relink_fallback`] rejected, such as
  /// `src/a.ts imports src/b.ts, protected`, when the check names it.
  pub fn last_native_relink_fallback_detail(&self) -> Option<&str> {
    self
      .cache
      .last_native_relink_fallback
      .and(self.cache.last_native_relink_fallback_detail.as_deref())
  }

  /// Whether the last scan, which failed, failed only in modules that the graph that it leaves no
  /// longer reaches, which a fresh build of the revision never loads (#690).
  pub fn last_scan_failures_unreached(&self) -> bool {
    self.cache.last_scan_failures_unreached
  }

  /// The stable ids of the factories that the last HMR update's patches list for their clients to
  /// hold aside until an update passes on to their modules.
  pub fn last_standby(&self) -> &rustc_hash::FxHashSet<ArcStr> {
    &self.cache.last_standby
  }

  pub fn last_rebuilt_chunk_filenames(&self) -> &[ArcStr] {
    &self.cache.last_rebuilt_chunk_filenames
  }

  pub fn last_rebuilt_chunk_outputs(&self) -> &[rolldown_common::Output] {
    &self.cache.last_rebuilt_chunk_outputs
  }

  /// The modules that the last fast native relink appended to and retired from the output,
  /// and the retired records' files.
  pub fn last_native_graph_change(&self) -> Option<&crate::NativeGraphChange> {
    self.cache.last_native_graph_change.as_ref()
  }

  /// Discards the cached native relink state: the next generation links the whole graph.
  pub fn invalidate_native_relink(&mut self) {
    self.cache.invalidate_native_relink();
  }

  /// Whether a scan of the compiler's graph held a module through whose records a page may load a
  /// record that it never prunes, as the target of an `import()` that a loader that the page saved
  /// still makes, or that of an `import defer` whose namespace a saved read still runs, also once
  /// the graph's entries no longer reach it (`ScanStageCache::late_records`).
  pub fn late_records(&self) -> bool {
    self.cache.late_records
  }

  /// Records that the pages of the compiler's graph may hold such records that an earlier
  /// compiler's graph led them to load, as when this compiler takes over their session (#732).
  pub fn note_late_records(&mut self) {
    self.cache.late_records = true;
  }

  pub async fn with_cached_bundle_experimental<T>(
    &mut self,
    bundle_mode: BundleMode,
    with_fn: impl AsyncFnOnce(&mut Bundle) -> BuildResult<T>,
  ) -> BuildResult<T> {
    self.with_cached_bundle(bundle_mode, with_fn).await
  }

  pub async fn incremental_write(
    &mut self,
    scan_mode: ScanMode<ArcStr>,
  ) -> BuildResult<BundleOutput> {
    self.incremental_bundle(true, scan_mode).await
  }

  pub async fn incremental_generate(
    &mut self,
    scan_mode: ScanMode<ArcStr>,
  ) -> BuildResult<BundleOutput> {
    self.incremental_bundle(false, scan_mode).await
  }

  async fn incremental_bundle(
    &mut self,
    is_write: bool,
    scan_mode: ScanMode<ArcStr>,
  ) -> BuildResult<BundleOutput> {
    // No snapshot, no graph to build on: scan fully. Deciding before the
    // `BundleMode` choice applies the `IncrementalFullBuild` reset rules to
    // the plugin driver state.
    let scan_mode = if self.cache.has_snapshot() { scan_mode } else { ScanMode::Full };
    let is_full_scan = scan_mode.is_full();
    let bundle_mode = match scan_mode {
      ScanMode::Full => BundleMode::IncrementalFullBuild,
      ScanMode::Partial(_) => BundleMode::IncrementalBuild,
    };
    let output = self
      .with_cached_bundle(bundle_mode, async |bundle| {
        let middle_output = bundle.scan_modules(scan_mode).await?;
        if is_write {
          bundle.bundle_write(middle_output).await
        } else {
          bundle.bundle_generate(middle_output).await
        }
      })
      .await;
    if is_full_scan && output.is_ok() {
      // The new output has the lost edit, and the task that started this build told every
      // client to reload; see internal-docs/bundler-data-lifecycle/implementation.md.
      self.lost_hmr_update = false;
    }
    output
  }
}
