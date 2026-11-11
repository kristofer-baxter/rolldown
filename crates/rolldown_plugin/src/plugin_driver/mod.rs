mod build_hooks;
mod hook_orders;
mod output_hooks;
mod plugin_driver_factory;
mod watch_hooks;

pub use plugin_driver_factory::PluginDriverFactory;

use std::{
  ops::Deref,
  sync::{Arc, Mutex},
};

use anyhow::Context;
use arcstr::ArcStr;
use dashmap::DashMap;
use rolldown_common::{
  ModuleDefFormat, ModuleId, ModuleIdx, ModuleInfo, ModuleLoaderMsg, Output, PluginIdx,
  SharedFileEmitter, SharedModuleInfoDashMap, WatchPath,
};
use rolldown_error::BuildDiagnostic;
use rolldown_utils::dashmap::FxDashSet;
use tokio::sync::broadcast;

use crate::{
  __inner::SharedPluginable,
  PluginContext,
  plugin_driver::hook_orders::PluginHookOrders,
  type_aliases::{IndexPluginContext, IndexPluginable},
  types::build_timings::BuildTimings,
};

pub type SharedPluginDriver = Arc<PluginDriver>;

#[derive(Debug, Default)]
pub struct EmittedAssetNames {
  state: Mutex<(u64, std::collections::BTreeMap<ArcStr, ArcStr>)>,
}

impl EmittedAssetNames {
  pub(crate) fn record(
    &self,
    reference: ArcStr,
    filename: impl FnOnce() -> anyhow::Result<ArcStr>,
  ) -> anyhow::Result<()> {
    // Looking up before locking can overwrite a concurrent deduplication's shorter name.
    let mut state = self.state.lock().expect("emitted asset names poisoned");
    let filename = filename()?;
    if state.1.get(&reference) != Some(&filename) {
      state.1.insert(reference, filename);
      state.0 = state.0.wrapping_add(1);
    }
    Ok(())
  }

  fn clear(&self) {
    let mut state = self.state.lock().expect("emitted asset names poisoned");
    if !state.1.is_empty() {
      state.1.clear();
      state.0 = state.0.wrapping_add(1);
    }
  }

  fn snapshot(&self) -> (u64, Vec<ArcStr>) {
    let state = self.state.lock().expect("emitted asset names poisoned");
    (state.0, state.1.values().cloned().collect())
  }

  fn generation(&self) -> u64 {
    self.state.lock().expect("emitted asset names poisoned").0
  }

  fn owners_since(&self, generation: Option<u64>) -> anyhow::Result<Option<EmittedOutputOwners>> {
    let state = self.state.lock().expect("emitted asset names poisoned");
    if generation == Some(state.0) {
      return Ok(None);
    }
    let mut references = std::collections::BTreeMap::new();
    for (reference, filename) in &state.1 {
      anyhow::ensure!(
        references.insert(filename.to_lowercase(), reference.clone()).is_none(),
        "conflicting native record output filename: {filename} (distinct emitted references)"
      );
    }
    Ok(Some(EmittedOutputOwners { generation: state.0, references }))
  }
}

#[cfg(test)]
mod emitted_asset_names_tests {
  use super::EmittedAssetNames;
  use arcstr::ArcStr;
  use rolldown_common::{EmittedAsset, FileEmitter, FilenameTemplate, NormalizedBundlerOptions};
  use std::sync::{Arc, TryLockError, mpsc};

  fn emit_named(emitter: &FileEmitter, name: &str) -> anyhow::Result<ArcStr> {
    emitter.emit_file(
      EmittedAsset {
        name: Some(name.into()),
        source: "identical contents".to_owned().into(),
        ..Default::default()
      },
      Some(FilenameTemplate::new("[name][extname]".to_owned(), "output.assetFileNames")),
      Some(name.into()),
    )
  }

  #[test]
  fn commonjs_emitted_names_follow_concurrent_deduplication() -> anyhow::Result<()> {
    let emitter = Arc::new(FileEmitter::new(Arc::new(NormalizedBundlerOptions::default())));
    emitter.enter_build_phase();
    let reference = emit_named(&emitter, "long-asset-name.txt")?;
    let names = Arc::new(EmittedAssetNames::default());
    let (observed_tx, observed_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let first = {
      let emitter = Arc::clone(&emitter);
      let names = Arc::clone(&names);
      let reference = reference.clone();
      std::thread::spawn(move || {
        names.record(reference.clone(), || {
          let filename = emitter.get_file_name(&reference)?;
          observed_tx.send(filename.clone())?;
          resume_rx.recv()?;
          Ok(filename)
        })
      })
    };
    let observed = observed_rx.recv()?;
    let lookup_holds_lock = matches!(names.state.try_lock(), Err(TryLockError::WouldBlock));
    let shorter_reference = emit_named(&emitter, "a.txt")?;
    let (recorded_tx, recorded_rx) = mpsc::channel();
    let second = {
      let emitter = Arc::clone(&emitter);
      let names = Arc::clone(&names);
      let reference = shorter_reference.clone();
      std::thread::spawn(move || -> anyhow::Result<()> {
        let result = names.record(reference.clone(), || emitter.get_file_name(&reference));
        recorded_tx.send(())?;
        result
      })
    };
    if !lookup_holds_lock {
      recorded_rx.recv()?;
    }
    resume_tx.send(())?;
    first.join().expect("first name recorder panicked")?;
    second.join().expect("second name recorder panicked")?;
    assert!(lookup_holds_lock, "filename lookup must share the tracking update's lock");
    assert_eq!(observed.as_str(), "long-asset-name.txt");
    assert_eq!(reference, shorter_reference, "identical assets must share a reference");
    assert_eq!(emitter.get_file_name(&reference)?.as_str(), "a.txt");
    assert_eq!(names.snapshot().1, vec![ArcStr::from("a.txt")]);
    Ok(())
  }
}

#[derive(Debug)]
struct EmittedOutputOwners {
  generation: u64,
  references: std::collections::BTreeMap<String, ArcStr>,
}

#[derive(Debug, Default)]
pub struct PendingEmittedOutputs {
  active: bool,
  replay_pending: bool,
  owners: Option<EmittedOutputOwners>,
  outputs: std::collections::BTreeMap<ArcStr, Output>,
  exposed: std::collections::BTreeMap<ArcStr, String>,
  untracked_output: Option<String>,
}

#[derive(Debug)]
pub struct EmittedOutputGuard {
  state: Arc<Mutex<PendingEmittedOutputs>>,
  committed: bool,
}

impl EmittedOutputGuard {
  pub fn commit(mut self) {
    let mut state = self.state.lock().expect("emitted outputs poisoned");
    state.outputs.clear();
    state.exposed.clear();
    state.active = false;
    state.replay_pending = false;
    self.committed = true;
  }
}

impl Drop for EmittedOutputGuard {
  fn drop(&mut self) {
    if !self.committed {
      let mut state = self.state.lock().expect("emitted outputs poisoned");
      state.active = false;
      state.replay_pending = false;
    }
  }
}

pub struct PluginDriver {
  plugins: IndexPluginable,
  contexts: IndexPluginContext,
  hook_orders: PluginHookOrders,
  pub(crate) should_skip_user_plugins_for_lazy_proxy_modules: bool,
  pub(crate) lazy_compilation_plugin_idx: Option<PluginIdx>,
  pub file_emitter: SharedFileEmitter,
  pub(crate) asset_names: Arc<EmittedAssetNames>,
  pub(crate) emitted_outputs: Arc<Mutex<PendingEmittedOutputs>>,
  pub watch_files: Arc<FxDashSet<ArcStr>>,
  pub module_infos: SharedModuleInfoDashMap,
  /// Module dependencies tracked during load/transform hooks for HMR invalidation
  pub transform_dependencies: Arc<DashMap<ModuleIdx, Arc<FxDashSet<WatchPath>>>>,
  context_load_completion_manager: ContextLoadCompletionManager,
  pub(crate) tx: Arc<Mutex<Option<tokio::sync::mpsc::UnboundedSender<ModuleLoaderMsg>>>>,
  /// Outlives the `Bundle` through `BundleHandle`, which is how the binding reads them
  /// once the build has finished.
  pub build_timings: BuildTimings,
}

impl PluginDriver {
  pub fn clear(&self) {
    self.watch_files.clear();
    self.module_infos.clear();
    // Note: transform_dependencies is NOT cleared here - it's preserved across incremental builds
    // by BundleFactory which manages its lifecycle (reset on full builds only)
    self.context_load_completion_manager.clear();
    self.file_emitter.clear();
    self.asset_names.clear();
    *self.emitted_outputs.lock().expect("emitted outputs poisoned") =
      PendingEmittedOutputs::default();
  }

  pub fn emitted_asset_filenames(&self) -> (u64, Vec<ArcStr>) {
    self.asset_names.snapshot()
  }

  pub fn emitted_asset_names_generation(&self) -> u64 {
    self.asset_names.generation()
  }

  pub fn begin_output_emissions(
    &self,
    enabled: bool,
  ) -> anyhow::Result<Option<EmittedOutputGuard>> {
    let mut state = self.emitted_outputs.lock().expect("emitted outputs poisoned");
    anyhow::ensure!(!state.active, "an emitted-output publication is already active");
    if let Some(filename) = &state.untracked_output {
      anyhow::bail!("native output was emitted without a plugin reference: {filename}");
    }
    if !enabled && state.outputs.is_empty() {
      return Ok(None);
    }
    self.refresh_emitted_output_owners(&mut state)?;
    state.active = true;
    state.replay_pending = true;
    state.exposed.clear();
    Ok(Some(EmittedOutputGuard { state: Arc::clone(&self.emitted_outputs), committed: false }))
  }

  fn refresh_emitted_output_owners(&self, state: &mut PendingEmittedOutputs) -> anyhow::Result<()> {
    if let Some(owners) =
      self.asset_names.owners_since(state.owners.as_ref().map(|o| o.generation))?
    {
      state.owners = Some(owners);
    }
    Ok(())
  }

  pub fn add_emitted_files(
    &self,
    bundle: &mut Vec<Output>,
    warnings: &mut Vec<BuildDiagnostic>,
  ) -> anyhow::Result<()> {
    let mut state = self.emitted_outputs.lock().expect("emitted outputs poisoned");
    if !state.active {
      self.file_emitter.add_additional_files(bundle, warnings);
      return Ok(());
    }
    self.refresh_emitted_output_owners(&mut state)?;
    let mut emitted = Vec::new();
    self.file_emitter.add_additional_files(&mut emitted, warnings);
    let mut current = Vec::with_capacity(emitted.len());
    for output in emitted {
      let reference = state
        .owners
        .as_ref()
        .expect("emitted owners refreshed")
        .references
        .get(&output.filename().to_lowercase())
        .cloned();
      let Some(reference) = reference else {
        state.untracked_output = Some(output.filename().to_owned());
        anyhow::bail!(
          "native output was emitted without a plugin reference: {}",
          output.filename()
        );
      };
      state.outputs.insert(reference.clone(), output.clone());
      current.push((reference, output));
    }
    if state.replay_pending {
      state.replay_pending = false;
      current = state.outputs.iter().map(|(id, output)| (id.clone(), output.clone())).collect();
      current.sort_unstable_by(|(_, a), (_, b)| a.filename().cmp(b.filename()));
    }
    if current.is_empty() {
      return Ok(());
    }
    let mut positions = std::collections::BTreeMap::new();
    for (index, output) in bundle.iter().enumerate() {
      positions
        .entry(output.filename().to_lowercase())
        .and_modify(|position| *position = None)
        .or_insert(Some(index));
    }
    for (reference, output) in current {
      let filename = output.filename().to_owned();
      let key = filename.to_lowercase();
      let existing = positions.get(&key);
      let replace = if let Some(exposed) = state.exposed.get(&reference) {
        anyhow::ensure!(exposed == &filename, "emitted reference changed its filename: {filename}");
        match existing {
          Some(Some(index)) => Some(*index),
          Some(None) => anyhow::bail!("conflicting native record output filename: {filename}"),
          None => None,
        }
      } else {
        anyhow::ensure!(
          existing.is_none(),
          "conflicting native record output filename: {filename}"
        );
        None
      };
      if let Some(index) = replace {
        bundle[index] = output;
      } else {
        positions.insert(key, Some(bundle.len()));
        bundle.push(output);
      }
      state.exposed.insert(reference, filename);
    }
    Ok(())
  }

  pub fn set_module_info(&self, module_id: &ModuleId, module_info: Arc<ModuleInfo>) {
    self.module_infos.insert(module_id.as_arc_str().into(), module_info);
  }

  pub fn set_context_load_modules_tx(
    &self,
    tx: Option<tokio::sync::mpsc::UnboundedSender<ModuleLoaderMsg>>,
  ) -> anyhow::Result<()> {
    *self.tx.lock().ok().context("Failed to acquire PluginDriver tx lock")? = tx;
    Ok(())
  }

  pub fn mark_context_load_modules_loaded(&self, module_id: ModuleId) {
    self.context_load_completion_manager.mark_completion(module_id);
  }

  pub fn invalidate_context_load_module(&self, module_id: &ModuleId) {
    self.context_load_completion_manager.invalidate(module_id);
  }

  pub async fn wait_for_module_load_completion(&self, specifier: &str) {
    self.context_load_completion_manager.wait_for_completion(specifier.into()).await;
  }

  pub fn iter_plugin_with_context_by_order<'me>(
    &'me self,
    ordered_plugins: &'me [PluginIdx],
  ) -> impl ExactSizeIterator<Item = (PluginIdx, &'me SharedPluginable, &'me PluginContext)> + 'me
  {
    ordered_plugins.iter().copied().map(move |idx| {
      let plugin = &self.plugins[idx];
      let context = &self.contexts[idx];
      (idx, plugin, context)
    })
  }

  pub fn plugins(&self) -> &IndexPluginable {
    &self.plugins
  }

  /// developer-bundler: the format whose CommonJS interop the module `id` follows, in place of
  /// `format`, its own; see `Plugin::interop_module_def_format`.
  pub fn interop_module_def_format(&self, id: &str, format: ModuleDefFormat) -> ModuleDefFormat {
    self
      .plugins
      .iter()
      .find_map(|plugin| plugin.call_interop_module_def_format(id, format))
      .unwrap_or(format)
  }

  pub fn add_transform_dependency(&self, module_idx: ModuleIdx, dependency: WatchPath) {
    self
      .transform_dependencies
      .entry(module_idx)
      .or_insert_with(|| Arc::new(FxDashSet::default()))
      .insert(dependency);
  }
}

impl Deref for PluginDriver {
  type Target = PluginHookOrders;
  fn deref(&self) -> &Self::Target {
    &self.hook_orders
  }
}

#[derive(Default)]
struct ContextLoadCompletionManager {
  notifiers: DashMap<ModuleId, ContextLoadCompletionState>,
}

enum ContextLoadCompletionState {
  Pending(broadcast::Sender<()>),
  Completed,
}

#[cfg(test)]
mod completion_tests {
  use std::{
    future::Future,
    pin::pin,
    task::{Context, Poll, Waker},
  };

  use super::ContextLoadCompletionManager;
  use rolldown_common::ModuleId;

  #[test]
  fn completion_survives_cancelled_receivers_and_invalidation() {
    let manager = ContextLoadCompletionManager::default();
    let id = ModuleId::new("cancelled.js");
    let mut context = Context::from_waker(Waker::noop());
    {
      let mut waiter = pin!(manager.wait_for_completion(id.clone()));
      assert!(waiter.as_mut().poll(&mut context).is_pending());
    }
    manager.mark_completion(id.clone());
    let mut completed = pin!(manager.wait_for_completion(id.clone()));
    assert_eq!(completed.as_mut().poll(&mut context), Poll::Ready(()));
    manager.invalidate(&id);
    let mut retry = pin!(manager.wait_for_completion(id.clone()));
    assert!(retry.as_mut().poll(&mut context).is_pending());
    manager.mark_completion(id);
    assert_eq!(retry.as_mut().poll(&mut context), Poll::Ready(()));
  }

  #[test]
  fn completion_wakes_all_live_receivers() {
    let manager = ContextLoadCompletionManager::default();
    let id = ModuleId::new("live.js");
    let mut context = Context::from_waker(Waker::noop());
    let mut first = pin!(manager.wait_for_completion(id.clone()));
    let mut second = pin!(manager.wait_for_completion(id.clone()));
    assert!(first.as_mut().poll(&mut context).is_pending());
    assert!(second.as_mut().poll(&mut context).is_pending());
    manager.mark_completion(id);
    assert_eq!(first.as_mut().poll(&mut context), Poll::Ready(()));
    assert_eq!(second.as_mut().poll(&mut context), Poll::Ready(()));
  }
}

impl ContextLoadCompletionManager {
  pub async fn wait_for_completion(&self, module_id: ModuleId) {
    let mut rx = match self.notifiers.entry(module_id) {
      dashmap::Entry::Vacant(guard) => {
        let (tx, rx) = broadcast::channel(1);
        guard.insert(ContextLoadCompletionState::Pending(tx));
        rx
      }
      dashmap::Entry::Occupied(mut guard) => match guard.get_mut() {
        ContextLoadCompletionState::Pending(sender) => sender.subscribe(),
        ContextLoadCompletionState::Completed => {
          /* no need to wait */
          return;
        }
      },
    };

    if let Err(err) = rx.recv().await {
      // This happens when `.invalidate` is called before `.mark_completion` is called, which is not expected
      debug_assert!(
        false,
        "The sender was dropped while waiting for module load completion: {err}"
      );
      tracing::warn!("The sender was dropped while waiting for module load completion");
    }
  }

  pub fn mark_completion(&self, module_id: ModuleId) {
    match self.notifiers.entry(module_id) {
      dashmap::Entry::Vacant(guard) => {
        guard.insert(ContextLoadCompletionState::Completed);
      }
      dashmap::Entry::Occupied(mut guard) => match guard.get_mut() {
        ContextLoadCompletionState::Pending(sender) => {
          if sender.send(()).is_err() {
            tracing::debug!("Module load completed after its context-load waiters were cancelled");
          }
          *guard.get_mut() = ContextLoadCompletionState::Completed;
        }
        ContextLoadCompletionState::Completed => {
          // This happens if `.mark_completion` is called multiple times, which is not expected
          debug_assert!(false, "mark_completion was called even though it was already completed");
          tracing::warn!("mark_completion was called even though it was already completed");
        }
      },
    }
  }

  pub fn invalidate(&self, module_id: &ModuleId) {
    self.notifiers.remove(module_id);
  }

  pub fn clear(&self) {
    self.notifiers.clear();
  }
}

/// The `CONTEXT_call_id` of a hook-call span at `target`, or `None` when nothing can observe the
/// span. Once a dispatcher has been set (globally or scoped), tracing evaluates a span's fields
/// only for an enabled span, so the id is always generated then. Before that, tracing's `log`
/// fallback evaluates them for every span and drops the record unless the `log` logger is enabled
/// at `TRACE` for the span's target, so the id is generated only in that case. Every observer
/// therefore receives the same field it did with an unconditional id, without a UUID per hook
/// call that no one reads.
#[doc(hidden)]
pub fn hook_call_id(target: &str) -> Option<String> {
  (tracing::dispatcher::has_been_set() || log::log_enabled!(target: target, log::Level::Trace))
    .then(rolldown_utils::uuid::uuid_v4)
}
