use super::Fixture;
use anyhow::{Context, Result};
use std::{
  collections::{BTreeMap, BTreeSet},
  fs,
  path::Path,
  process::Command,
  sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
  },
};

fn write(root: &Path, name: &str, source: impl AsRef<[u8]>) -> Result<()> {
  let path = root.join(name);
  fs::create_dir_all(path.parent().context("output parent")?)?;
  Ok(fs::write(path, source)?)
}

fn node_import_value(
  entries: &[url::Url],
  mappings: &BTreeMap<String, String>,
  setup: &str,
  expression: &str,
) -> Result<String> {
  anyhow::ensure!(mappings.is_empty(), "standalone fixtures need no host import mappings");
  let entries = serde_json::to_string(&entries.iter().map(url::Url::as_str).collect::<Vec<_>>())?;
  let script = format!(
    "{setup};const timer=setTimeout(()=>{{throw Error('evaluation deadline')}},10000);\
         try{{for(const entry of {entries})await import(entry);\
         console.log(String({expression}))}}finally{{clearTimeout(timer)}}"
  );
  let output = Command::new("node").args(["--input-type=module", "--eval", &script]).output()?;
  anyhow::ensure!(
    output.status.success(),
    "Node execution failed: {}",
    String::from_utf8_lossy(&output.stderr)
  );
  Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[tokio::test]
async fn preserved_commonjs_entries_keep_output_extensions_and_shebangs() -> Result<()> {
  #[derive(Debug)]
  struct ShebangBanner;
  impl rolldown_plugin::Plugin for ShebangBanner {
    fn name(&self) -> std::borrow::Cow<'static, str> {
      "shebang-banner".into()
    }

    fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
      rolldown_plugin::HookUsage::Banner
    }

    async fn banner(
      &self,
      _: &rolldown_plugin::PluginContext,
      _: &rolldown_plugin::HookAddonArgs,
    ) -> rolldown_plugin::HookInjectionOutputReturn {
      Ok(Some("#!/usr/bin/env node".into()))
    }
  }

  for extension in ["js", "mjs"] {
    for variant in ["source", "banner", "post_banner", "hook"] {
      let root = Fixture::new(&[])?;
      let path = root.0.as_path().canonicalize()?;
      let header = if variant == "source" { "#!/usr/bin/env node\n" } else { "" };
      write(
        &path,
        "main.cjs",
        format!("{header}module.exports={{answer:42}};globalThis.read=()=>module.exports.answer;"),
      )?;
      let mut options = rolldown::BundlerOptions {
        cwd: Some(path),
        input: Some(vec![rolldown_common::InputItem {
          name: Some("main".into()),
          import: "./main.cjs".into(),
        }]),
        preserve_modules: Some(true),
        strict_execution_order: Some(false),
        entry_filenames: Some(format!("[name].{extension}").into()),
        chunk_filenames: Some(format!("[name].{extension}").into()),
        minify: Some(rolldown_common::RawMinifyOptions::Bool(false)),
        banner: (variant == "banner").then(|| {
          rolldown_common::AddonOutputOption::String(Some("#!/usr/bin/env node".to_owned()))
        }),
        post_banner: (variant == "post_banner").then(|| {
          rolldown_common::AddonOutputOption::String(Some("#!/usr/bin/env node".to_owned()))
        }),
        ..Default::default()
      };
      options.experimental.get_or_insert_default().incremental_build = Some(true);
      let mut bundler = if variant == "hook" {
        rolldown::Bundler::with_plugins(
          options,
          vec![<ShebangBanner as rolldown_plugin::Plugin>::new_shared(ShebangBanner)],
        )
      } else {
        rolldown::Bundler::new(options)
      }
      .map_err(|error| anyhow::anyhow!("{error:?}"))?;
      bundler.set_omit_reused_chunk_bytes(true);
      bundler.set_native_commonjs_records(true)?;
      let directory = Fixture::new(&[])?;
      let package = if extension == "mjs" { "commonjs" } else { "module" };
      write(directory.0.as_path(), "package.json", format!(r#"{{"type":"{package}"}}"#))?;
      let out = url::Url::from_directory_path(directory.0.as_path())
        .map_err(|()| anyhow::anyhow!("output is not a directory URL"))?;
      for warm in [false, true] {
        let scan = if warm {
          rolldown_common::ScanMode::Partial(Vec::new())
        } else {
          rolldown_common::ScanMode::Full
        };
        let output =
          bundler.incremental_generate(scan).await.map_err(|error| anyhow::anyhow!("{error:?}"))?;
        let entry = output
          .assets
          .iter()
          .find_map(|asset| match asset {
            rolldown_common::Output::Chunk(chunk) if chunk.is_entry => Some(chunk),
            _ => None,
          })
          .context("missing public entry")?;
        assert_eq!(
          entry.code.lines().next(),
          Some("#!/usr/bin/env node"),
          "{variant} {extension} warm={warm}"
        );
        let factory = format!("{}.cjs-factory.{extension}", entry.filename);
        assert!(output.assets.iter().any(|asset| asset.filename() == factory));
        let reused = bundler.last_reused_chunk_filenames();
        if warm && variant == "source" {
          assert!(reused.contains(factory.as_str()), "fixture must reuse the factory");
        }
        for asset in &output.assets {
          if !reused.contains(asset.filename()) {
            write(directory.0.as_path(), asset.filename(), asset.content_as_bytes())?;
          }
        }
        node_import_value(&[out.join(&entry.filename)?], &BTreeMap::new(), "", "globalThis.read()")
          .with_context(|| format!("{variant} {extension} warm={warm}"))?;
      }
      bundler.close().await.map_err(|error| anyhow::anyhow!("{error:?}"))?;
    }
  }
  Ok(())
}

#[tokio::test]
async fn native_commonjs_records_preserve_generic_addons_and_reject_native_tail_addons()
-> Result<()> {
  #[derive(Debug)]
  struct EntryFooter;
  impl rolldown_plugin::Plugin for EntryFooter {
    fn name(&self) -> std::borrow::Cow<'static, str> {
      "entry-footer".into()
    }
    fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
      rolldown_plugin::HookUsage::Footer
    }
    async fn footer(
      &self,
      _: &rolldown_plugin::PluginContext,
      args: &rolldown_plugin::HookAddonArgs,
    ) -> rolldown_plugin::HookInjectionOutputReturn {
      Ok(args.chunk.is_entry.then(|| "globalThis.start();".to_owned()))
    }
  }
  for variant in ["outro", "footer", "post_footer", "hook"] {
    for native in [false, true] {
      let root = Fixture::new(&[])?;
      let path = root.0.as_path().canonicalize()?;
      write(
        &path,
        "main.cjs",
        "globalThis.order=['body'];globalThis.start=()=>globalThis.order.push('footer');module.exports={};",
      )?;
      let mut options = rolldown::BundlerOptions {
        cwd: Some(path),
        input: Some(vec![rolldown_common::InputItem {
          name: Some("main".into()),
          import: "./main.cjs".into(),
        }]),
        preserve_modules: Some(true),
        strict_execution_order: Some(false),
        ..Default::default()
      };
      options.experimental.get_or_insert_default().incremental_build = Some(true);
      let addon = rolldown_common::AddonOutputOption::Fn(Arc::new(|chunk| {
        Box::pin(async move { Ok(chunk.is_entry.then(|| "globalThis.start();".to_owned())) })
      }));
      match variant {
        "outro" => options.outro = Some(addon),
        "footer" => options.footer = Some(addon),
        "post_footer" => options.post_footer = Some(addon),
        _ => {}
      }
      let mut bundler = if variant == "hook" {
        rolldown::Bundler::with_plugins(
          options,
          vec![<EntryFooter as rolldown_plugin::Plugin>::new_shared(EntryFooter)],
        )
      } else {
        rolldown::Bundler::new(options)
      }
      .map_err(|error| anyhow::anyhow!("{error:?}"))?;
      bundler.set_native_commonjs_records(native)?;
      let output = bundler.generate().await;
      bundler.close().await.map_err(|error| anyhow::anyhow!("{error:?}"))?;
      if native {
        let error = output.err().context("native tail addon unexpectedly succeeded")?;
        assert!(
          format!("{error:?}").contains("native CommonJS records do not support"),
          "{variant}: {error:?}"
        );
        continue;
      }
      let output = output.map_err(|error| anyhow::anyhow!("{error:?}"))?;
      let directory = Fixture::new(&[])?;
      write(directory.0.as_path(), "package.json", r#"{"type":"module"}"#)?;
      for asset in &output.assets {
        write(directory.0.as_path(), asset.filename(), asset.content_as_bytes())?;
      }
      let out = url::Url::from_directory_path(directory.0.as_path())
        .map_err(|()| anyhow::anyhow!("output is not a directory URL"))?;
      assert_eq!(
        node_import_value(
          &[out.join("main.js")?],
          &BTreeMap::new(),
          "",
          "globalThis.order.join(',')",
        )?,
        "body,footer",
        "{variant}"
      );
    }
  }
  Ok(())
}

#[tokio::test]
async fn native_commonjs_hashed_filename_imports_resolve_to_emitted_assets() -> Result<()> {
  use oxc::ast::ast::Statement;

  for template in ["[name].[hash]", "[name].[hash].mjs"] {
    let root = Fixture::new(&[])?;
    let path = root.0.as_path().canonicalize()?;
    write(&path, "main.cjs", "module.exports=require('./dep.cjs');")?;
    write(&path, "dep.cjs", "exports.answer=42;")?;
    let mut options = rolldown::BundlerOptions {
      cwd: Some(path),
      input: Some(vec![rolldown_common::InputItem {
        name: Some("main".into()),
        import: "./main.cjs".into(),
      }]),
      preserve_modules: Some(true),
      strict_execution_order: Some(false),
      entry_filenames: Some(template.to_owned().into()),
      chunk_filenames: Some(template.to_owned().into()),
      ..Default::default()
    };
    options.experimental.get_or_insert_default().incremental_build = Some(true);
    let mut bundler =
      rolldown::Bundler::new(options).map_err(|error| anyhow::anyhow!("{error:?}"))?;
    bundler.set_native_commonjs_records(true)?;
    let output = bundler.generate().await.map_err(|error| anyhow::anyhow!("{error:?}"))?;
    bundler.close().await.map_err(|error| anyhow::anyhow!("{error:?}"))?;
    let directory = Fixture::new(&[])?;
    for asset in &output.assets {
      assert!(!asset.filename().contains("!~{"), "unresolved filename: {}", asset.filename());
      write(directory.0.as_path(), asset.filename(), asset.content_as_bytes())?;
    }
    for asset in &output.assets {
      let rolldown_common::Output::Chunk(chunk) = asset else {
        continue;
      };
      let allocator = oxc::allocator::Allocator::default();
      let parsed =
        oxc::parser::Parser::new(&allocator, &chunk.code, oxc::span::SourceType::mjs()).parse();
      assert!(parsed.diagnostics.is_empty(), "{}: {:?}", chunk.filename, parsed.diagnostics);
      let from = directory.0.as_path().join(chunk.filename.as_str());
      for statement in &parsed.program.body {
        let source = match statement {
          Statement::ImportDeclaration(import) => Some(&import.source),
          Statement::ExportFromDeclaration(export) => Some(&export.source),
          Statement::ExportAllDeclaration(export) => Some(&export.source),
          _ => None,
        };
        if let Some(source) = source {
          let source = source.value.as_str().context("non-UTF8 emitted import")?;
          let target = from.parent().context("output has no directory")?.join(source);
          assert!(target.is_file(), "{template}: {} imports missing {source}", chunk.filename);
        }
      }
    }
  }
  Ok(())
}

#[derive(Debug)]
struct AssetPlugin {
  stage: &'static str,
  second: Arc<std::sync::atomic::AtomicBool>,
}
impl rolldown_plugin::Plugin for AssetPlugin {
  fn name(&self) -> std::borrow::Cow<'static, str> {
    "factory-name-asset".into()
  }
  fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
    rolldown_plugin::HookUsage::BuildStart | rolldown_plugin::HookUsage::GenerateBundle
  }
  async fn build_start(
    &self,
    ctx: &rolldown_plugin::PluginContext,
    _: &rolldown_plugin::HookBuildStartArgs<'_>,
  ) -> rolldown_plugin::HookNoopReturn {
    if matches!(self.stage, "sync" | "async" | "map") {
      let filename = if self.second.load(std::sync::atomic::Ordering::Relaxed) {
        "dep.js.cjs-factory2.js"
      } else {
        "dep.js.cjs-factory.js"
      };
      let filename =
        if self.stage == "map" { format!("{filename}.map") } else { filename.to_owned() };
      let asset = rolldown_common::EmittedAsset {
        file_name: Some(filename.into()),
        source: "asset payload".to_owned().into(),
        ..Default::default()
      };
      if self.stage == "async" {
        ctx.emit_file_async(asset).await?;
      } else {
        ctx.emit_file(asset, None, None)?;
      }
    }
    Ok(())
  }
  async fn generate_bundle(
    &self,
    _: &rolldown_plugin::PluginContext,
    args: &mut rolldown_plugin::HookGenerateBundleArgs<'_>,
  ) -> rolldown_plugin::HookNoopReturn {
    if matches!(self.stage, "late" | "inline-map")
      || matches!(self.stage, "late-map" | "late-map-full")
        && self.second.load(std::sync::atomic::Ordering::Relaxed)
    {
      let filename = if self.stage == "inline-map" {
        "maps/dep.map"
      } else if self.stage == "late" {
        "dep.js.cjs-factory.js"
      } else {
        "dep.js.cjs-factory.js.map"
      };
      args.bundle.push(rolldown_common::Output::Asset(Arc::new(rolldown_common::OutputAsset {
        filename: filename.into(),
        source: "late asset".to_owned().into(),
        names: Vec::new(),
        original_file_names: Vec::new(),
      })));
    }
    Ok(())
  }
}
#[tokio::test]
async fn native_commonjs_factories_avoid_emitted_assets_and_reject_late_collisions() -> Result<()> {
  for (stage, omit_bytes) in [
    ("sync", true),
    ("async", true),
    ("map", true),
    ("map", false),
    ("late", true),
    ("late-map", true),
    ("late-map", false),
    ("late-map-full", true),
    ("late-map-full", false),
    ("inline-map", true),
    ("inline-map", false),
  ] {
    let late_map = matches!(stage, "late-map" | "late-map-full");
    let inline_map = stage == "inline-map";
    let root = Fixture::new(&[])?;
    let path = root.0.as_path().canonicalize()?;
    write(
      &path,
      "entry.cjs",
      "module.exports=require('./dep.cjs');globalThis.read=()=>module.exports.answer;",
    )?;
    write(&path, "dep.cjs", "exports.answer=42;")?;
    let second = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut options = rolldown::BundlerOptions {
      cwd: Some(path.clone()),
      input: Some(vec![rolldown_common::InputItem {
        name: Some("entry".into()),
        import: "./entry.cjs".into(),
      }]),
      preserve_modules: Some(true),
      strict_execution_order: Some(false),
      minify: Some(rolldown_common::RawMinifyOptions::Bool(false)),
      sourcemap: if inline_map {
        Some(rolldown_common::SourceMapType::Inline)
      } else {
        (stage == "map" || late_map).then_some(rolldown_common::SourceMapType::File)
      },
      sourcemap_filenames: inline_map.then(|| "maps/[name].map".to_owned().into()),
      ..Default::default()
    };
    options.experimental.get_or_insert_default().incremental_build = Some(true);
    let mut bundler = rolldown::Bundler::with_plugins(
      options,
      vec![<AssetPlugin as rolldown_plugin::Plugin>::new_shared(AssetPlugin {
        stage,
        second: Arc::clone(&second),
      })],
    )
    .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    bundler.set_native_commonjs_records(true)?;
    bundler.set_omit_reused_chunk_bytes(omit_bytes);
    let directory = Fixture::new(&[])?;
    write(directory.0.as_path(), "package.json", r#"{"type":"module"}"#)?;
    let out = url::Url::from_directory_path(directory.0.as_path())
      .map_err(|()| anyhow::anyhow!("output is not a directory URL"))?;
    let mut dep_id: Option<rolldown_common::ModuleId> = None;
    for round in 0..5 {
      if round == 2 {
        second.store(true, std::sync::atomic::Ordering::Relaxed);
      } else if round == 4 {
        second.store(false, std::sync::atomic::Ordering::Relaxed);
      }
      if stage == "late-map-full" && round > 0 {
        bundler.invalidate_native_relink();
      }
      let scan = if round == 0 {
        rolldown_common::ScanMode::Full
      } else if late_map && round == 2 {
        write(&path, "dep.cjs", "exports.answer=43;")?;
        rolldown_common::ScanMode::Partial(vec![
          dep_id.as_ref().context("missing dependency module ID")?.as_str().into(),
        ])
      } else {
        rolldown_common::ScanMode::Partial(Vec::new())
      };
      let result = bundler.incremental_generate(scan).await;
      if stage == "late" || late_map && (2..4).contains(&round) {
        let error = result.err().context("late output collision unexpectedly succeeded")?;
        assert!(
          format!("{error:?}").contains("conflicting native record output filename"),
          "{error:?}"
        );
        if late_map {
          assert!(format!("{error:?}").contains("dep.js.cjs-factory.js.map"), "{error:?}");
        }
        continue;
      }
      let output = result.map_err(|error| anyhow::anyhow!("{error:?}"))?;
      let names =
        output.assets.iter().map(rolldown_common::Output::filename).collect::<BTreeSet<_>>();
      assert_eq!(names.len(), output.assets.len(), "duplicate outputs in {stage} round {round}");
      let factory = if late_map || inline_map {
        "dep.js.cjs-factory.js"
      } else if round >= 2 {
        "dep.js.cjs-factory3.js"
      } else {
        "dep.js.cjs-factory2.js"
      };
      assert!(names.contains(factory), "{stage} round {round}: {names:?}");
      let definition = output
        .assets
        .iter()
        .find_map(|asset| match asset {
          rolldown_common::Output::Chunk(chunk) if chunk.filename.as_str() == factory => {
            Some(chunk)
          }
          _ => None,
        })
        .context("missing dependency factory")?;
      if round == 0 {
        dep_id = definition.module_ids.first().cloned();
      }
      if inline_map {
        assert_eq!(
          definition.sourcemap_filename.as_deref(),
          Some("maps/dep.map"),
          "inline-map metadata must not reserve an external map file"
        );
        if omit_bytes && round == 1 {
          assert!(
            bundler.last_reused_chunk_filenames().contains(factory),
            "the inline-map case must exercise reused metadata"
          );
        }
      }
      if late_map && round == 1 {
        if omit_bytes {
          assert!(
            bundler.last_reused_chunk_filenames().contains(factory),
            "{stage} round {round} should reuse {factory}"
          );
        }
        assert_eq!(
          names.contains("dep.js.cjs-factory.js.map"),
          !omit_bytes,
          "{stage} round {round} should follow omit_bytes={omit_bytes}"
        );
      }
      for asset in &output.assets {
        if !bundler.last_reused_chunk_filenames().contains(asset.filename()) {
          write(directory.0.as_path(), asset.filename(), asset.content_as_bytes())?;
        }
      }
      let value =
        node_import_value(&[out.join("entry.js")?], &BTreeMap::new(), "", "globalThis.read()")?;
      assert_eq!(
        value,
        if late_map && round >= 2 { "43" } else { "42" },
        "{stage} round {round} omit_bytes={omit_bytes}"
      );
    }
    bundler.close().await.map_err(|error| anyhow::anyhow!("{error:?}"))?;
  }
  Ok(())
}

#[derive(Debug, Clone, Copy)]
enum Scenario {
  BuildStart,
  OutputHook,
  MutatedOutputHook,
  DuplicateOwners,
  LaterExplicitOwner,
  RawEmitter,
  CleanupFailure,
  WriteFailure,
}

#[derive(Debug)]
struct ManifestPlugin {
  version: Arc<AtomicUsize>,
  collision: Arc<AtomicBool>,
  scenario: Scenario,
}
impl ManifestPlugin {
  fn emit_manifest(&self, ctx: &rolldown_plugin::PluginContext) -> rolldown_plugin::HookNoopReturn {
    let version = self.version.load(Ordering::Relaxed);
    if matches!(self.scenario, Scenario::RawEmitter) {
      ctx.file_emitter().emit_file(
        rolldown_common::EmittedAsset {
          file_name: Some("manifest.json".into()),
          source: format!("{{\"version\":{version}}}").into(),
          ..Default::default()
        },
        None,
        None,
      )?;
      return Ok(());
    }
    let later_owner = matches!(self.scenario, Scenario::LaterExplicitOwner);
    if later_owner && version == 1 {
      return Ok(());
    }
    if !later_owner || version >= 3 {
      let reference = ctx.emit_file(
        rolldown_common::EmittedAsset {
          file_name: Some("manifest.json".into()),
          source: format!("{{\"version\":{version}}}").into(),
          ..Default::default()
        },
        None,
        None,
      )?;
      assert_eq!(ctx.get_file_name(&reference)?.as_str(), "manifest.json");
    }
    if matches!(self.scenario, Scenario::DuplicateOwners | Scenario::LaterExplicitOwner) {
      let reference = ctx.emit_file(
        rolldown_common::EmittedAsset {
          name: Some("generated.json".into()),
          source: "automatic asset".to_owned().into(),
          ..Default::default()
        },
        None,
        None,
      )?;
      assert_eq!(ctx.get_file_name(&reference)?.as_str(), "manifest.json");
    }
    Ok(())
  }
}
impl rolldown_plugin::Plugin for ManifestPlugin {
  fn name(&self) -> std::borrow::Cow<'static, str> {
    "retry-manifest".into()
  }
  fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
    rolldown_plugin::HookUsage::BuildStart | rolldown_plugin::HookUsage::GenerateBundle
  }
  async fn build_start(
    &self,
    ctx: &rolldown_plugin::PluginContext,
    _: &rolldown_plugin::HookBuildStartArgs<'_>,
  ) -> rolldown_plugin::HookNoopReturn {
    if !matches!(self.scenario, Scenario::OutputHook | Scenario::MutatedOutputHook) {
      self.emit_manifest(ctx)?;
    }
    Ok(())
  }
  async fn generate_bundle(
    &self,
    ctx: &rolldown_plugin::PluginContext,
    args: &mut rolldown_plugin::HookGenerateBundleArgs<'_>,
  ) -> rolldown_plugin::HookNoopReturn {
    if matches!(self.scenario, Scenario::MutatedOutputHook)
      && self.version.load(Ordering::Relaxed) == 3
    {
      for output in args.bundle.iter_mut() {
        if let rolldown_common::Output::Asset(asset) = output
          && asset.filename.as_str() == "manifest.json"
        {
          Arc::make_mut(asset).source = "modified by hook".to_owned().into();
        }
      }
    }
    if matches!(self.scenario, Scenario::OutputHook | Scenario::MutatedOutputHook) {
      self.emit_manifest(ctx)?;
    }
    if self.collision.load(Ordering::Relaxed)
      && !matches!(self.scenario, Scenario::CleanupFailure | Scenario::WriteFailure)
    {
      args.bundle.push(rolldown_common::Output::Asset(Arc::new(rolldown_common::OutputAsset {
        filename: "entry.js.cjs-factory.js".into(),
        source: "collision".to_owned().into(),
        names: Vec::new(),
        original_file_names: Vec::new(),
      })));
    }
    Ok(())
  }
}
#[tokio::test]
async fn native_commonjs_output_failures_preserve_emitted_asset_updates() -> Result<()> {
  for (force_full_link, recovered_version, scenario) in [
    (false, 2, Scenario::BuildStart),
    (true, 2, Scenario::BuildStart),
    (false, 3, Scenario::BuildStart),
    (true, 3, Scenario::BuildStart),
    (false, 2, Scenario::DuplicateOwners),
    (true, 2, Scenario::DuplicateOwners),
    (false, 3, Scenario::OutputHook),
    (true, 3, Scenario::OutputHook),
    (false, 3, Scenario::MutatedOutputHook),
    (true, 3, Scenario::MutatedOutputHook),
    (false, 3, Scenario::LaterExplicitOwner),
    (true, 3, Scenario::LaterExplicitOwner),
    (false, 2, Scenario::RawEmitter),
    (true, 2, Scenario::RawEmitter),
    (false, 2, Scenario::CleanupFailure),
    (true, 2, Scenario::CleanupFailure),
    (false, 3, Scenario::CleanupFailure),
    (true, 3, Scenario::CleanupFailure),
    (false, 2, Scenario::WriteFailure),
    (true, 2, Scenario::WriteFailure),
  ] {
    let root = Fixture::new(&[])?;
    let published = Fixture::new(&[])?;
    let writing = matches!(scenario, Scenario::WriteFailure);
    write(root.0.as_path(), "entry.cjs", "module.exports=42;")?;
    let version = Arc::new(AtomicUsize::new(1));
    let collision = Arc::new(AtomicBool::new(false));
    let mut options = rolldown::BundlerOptions {
      cwd: Some(root.0.as_path().canonicalize()?),
      dir: writing.then(|| published.0.as_path().to_string_lossy().into_owned()),
      clean_dir: writing.then_some(false),
      input: Some(vec![rolldown_common::InputItem {
        name: Some("entry".into()),
        import: "./entry.cjs".into(),
      }]),
      preserve_modules: Some(true),
      strict_execution_order: Some(false),
      minify: Some(rolldown_common::RawMinifyOptions::Bool(false)),
      asset_filenames: matches!(scenario, Scenario::DuplicateOwners | Scenario::LaterExplicitOwner)
        .then(|| "manifest.json".to_owned().into()),
      invalidate_js_side_cache: matches!(scenario, Scenario::CleanupFailure).then(|| {
        let failed = Arc::clone(&collision);
        rolldown_common::InvalidateJsSideCache::new(Arc::new(move || {
          let failed = Arc::clone(&failed);
          Box::pin(async move {
            anyhow::ensure!(!failed.load(Ordering::Relaxed), "cleanup rejected");
            Ok(())
          })
        }))
      }),
      ..Default::default()
    };
    options.experimental.get_or_insert_default().incremental_build = Some(true);
    let mut bundler = rolldown::Bundler::with_plugins(
      options,
      vec![<ManifestPlugin as rolldown_plugin::Plugin>::new_shared(ManifestPlugin {
        version: Arc::clone(&version),
        collision: Arc::clone(&collision),
        scenario,
      })],
    )
    .map_err(|error| anyhow::anyhow!("{error:?}"))?;
    bundler.set_native_commonjs_records(true)?;
    bundler.set_omit_reused_chunk_bytes(!writing);
    for round in 0..6 {
      let current = if round < 2 {
        1
      } else if round < 4 {
        2
      } else {
        recovered_version
      };
      version.store(current, Ordering::Relaxed);
      collision.store((2..4).contains(&round), Ordering::Relaxed);
      if writing {
        let manifest = published.0.as_path().join("manifest.json");
        if round == 2 {
          std::fs::remove_file(&manifest)?;
          std::fs::create_dir(&manifest)?;
        } else if round == 4 {
          std::fs::remove_dir(&manifest)?;
          std::fs::write(&manifest, r#"{"version":1}"#)?;
        }
      }
      if force_full_link {
        bundler.invalidate_native_relink();
      }
      let scan = if round == 0 {
        rolldown_common::ScanMode::Full
      } else {
        rolldown_common::ScanMode::Partial(Vec::new())
      };
      let result = if writing {
        bundler.incremental_write(scan).await
      } else {
        bundler.incremental_generate(scan).await
      };
      if matches!(scenario, Scenario::DuplicateOwners | Scenario::RawEmitter)
        || (2..4).contains(&round)
        || matches!(scenario, Scenario::LaterExplicitOwner) && round >= 4
      {
        let error = result.err().with_context(|| {
          format!("{scenario:?} round {round}: expected failure unexpectedly succeeded")
        })?;
        let expected = if matches!(scenario, Scenario::RawEmitter) {
          "native output was emitted without a plugin reference"
        } else if matches!(scenario, Scenario::CleanupFailure) {
          "cleanup rejected"
        } else if writing {
          "Failed to write file"
        } else {
          "conflicting native record output filename"
        };
        assert!(format!("{error:?}").contains(expected), "{error:?}");
        continue;
      }
      let output = result.map_err(|error| anyhow::anyhow!("{error:?}"))?;
      if matches!(scenario, Scenario::LaterExplicitOwner) {
        assert!(output.assets.iter().all(|asset| asset.filename() != "manifest.json"));
        continue;
      }
      if matches!(round, 1 | 5) {
        assert!(
          output.assets.iter().all(|asset| asset.filename() != "manifest.json"),
          "published unchanged emissions should not be replayed"
        );
      }
      if !writing {
        for asset in &output.assets {
          if !bundler.last_reused_chunk_filenames().contains(asset.filename()) {
            write(published.0.as_path(), asset.filename(), asset.content_as_bytes())?;
          }
        }
      }
      assert_eq!(
        std::fs::read_to_string(published.0.as_path().join("manifest.json"))?,
        format!("{{\"version\":{current}}}"),
        "{scenario:?} round {round}, force_full_link={force_full_link}, recovery={recovered_version}"
      );
    }
    bundler.close().await.map_err(|error| anyhow::anyhow!("{error:?}"))?;
  }
  Ok(())
}

#[tokio::test]
async fn native_commonjs_output_failures_reject_duplicate_emitted_entries() -> Result<()> {
  #[derive(Debug)]
  struct DuplicateEntries;
  impl rolldown_plugin::Plugin for DuplicateEntries {
    fn name(&self) -> std::borrow::Cow<'static, str> {
      "duplicate-entries".into()
    }
    fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
      rolldown_plugin::HookUsage::BuildStart
    }
    async fn build_start(
      &self,
      ctx: &rolldown_plugin::PluginContext,
      _: &rolldown_plugin::HookBuildStartArgs<'_>,
    ) -> rolldown_plugin::HookNoopReturn {
      for id in ["./a.cjs", "./b.cjs"] {
        ctx.emit_chunk(rolldown_common::EmittedChunk {
          id: id.into(),
          file_name: Some("dup.js".into()),
          ..Default::default()
        })?;
      }
      Ok(())
    }
  }
  let root = Fixture::new(&[])?;
  write(root.0.as_path(), "main.mjs", "export const entry=1;")?;
  write(root.0.as_path(), "a.cjs", "module.exports=1;")?;
  write(root.0.as_path(), "b.cjs", "module.exports=2;")?;
  let mut options = rolldown::BundlerOptions {
    cwd: Some(root.0.as_path().canonicalize()?),
    input: Some(vec![rolldown_common::InputItem {
      name: Some("main".into()),
      import: "./main.mjs".into(),
    }]),
    preserve_modules: Some(true),
    strict_execution_order: Some(false),
    minify: Some(rolldown_common::RawMinifyOptions::Bool(false)),
    ..Default::default()
  };
  options.experimental.get_or_insert_default().incremental_build = Some(true);
  let mut bundler = rolldown::Bundler::with_plugins(
    options,
    vec![<DuplicateEntries as rolldown_plugin::Plugin>::new_shared(DuplicateEntries)],
  )
  .map_err(|error| anyhow::anyhow!("{error:?}"))?;
  bundler.set_native_commonjs_records(true)?;
  bundler.set_omit_reused_chunk_bytes(true);
  for scan in [rolldown_common::ScanMode::Full, rolldown_common::ScanMode::Partial(Vec::new())] {
    let error = bundler
      .incremental_generate(scan)
      .await
      .err()
      .context("duplicate public entry filenames unexpectedly succeeded")?;
    assert!(
      format!("{error:?}").contains("conflicting CommonJS evaluation record: dup.js"),
      "{error:?}"
    );
  }
  bundler.close().await.map_err(|error| anyhow::anyhow!("{error:?}"))?;
  Ok(())
}

#[tokio::test]
async fn native_commonjs_plugin_emitted_facades_remain_entries() -> Result<()> {
  #[derive(Debug)]
  struct EmitCommonJs;
  impl rolldown_plugin::Plugin for EmitCommonJs {
    fn name(&self) -> std::borrow::Cow<'static, str> {
      "emit-commonjs".into()
    }
    fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
      rolldown_plugin::HookUsage::BuildStart
    }
    async fn build_start(
      &self,
      ctx: &rolldown_plugin::PluginContext,
      _: &rolldown_plugin::HookBuildStartArgs<'_>,
    ) -> rolldown_plugin::HookNoopReturn {
      ctx.emit_chunk(rolldown_common::EmittedChunk {
        name: Some("emitted".into()),
        id: "./emitted.cjs".into(),
        ..Default::default()
      })?;
      Ok(())
    }
  }
  let root = Fixture::new(&[])?;
  let path = root.0.as_path().canonicalize()?;
  write(&path, "main.mjs", "export const main=1;")?;
  write(
    &path,
    "emitted.cjs",
    "module.exports={answer:42};globalThis.read=()=>module.exports.answer;",
  )?;
  let mut options = rolldown::BundlerOptions {
    cwd: Some(path),
    input: Some(vec![rolldown_common::InputItem {
      name: Some("main".into()),
      import: "./main.mjs".into(),
    }]),
    preserve_modules: Some(true),
    strict_execution_order: Some(false),
    ..Default::default()
  };
  options.experimental.get_or_insert_default().incremental_build = Some(true);
  let mut bundler = rolldown::Bundler::with_plugins(
    options,
    vec![<EmitCommonJs as rolldown_plugin::Plugin>::new_shared(EmitCommonJs)],
  )
  .map_err(|error| anyhow::anyhow!("{error:?}"))?;
  bundler.set_native_commonjs_records(true)?;
  let output = bundler.generate().await.map_err(|error| anyhow::anyhow!("{error:?}"))?;
  bundler.close().await.map_err(|error| anyhow::anyhow!("{error:?}"))?;
  let emitted = output
    .assets
    .iter()
    .find_map(|asset| match asset {
      rolldown_common::Output::Chunk(chunk)
        if chunk.facade_module_id.as_deref().is_some_and(|id| id.ends_with("emitted.cjs")) =>
      {
        Some(chunk)
      }
      _ => None,
    })
    .context("missing emitted CommonJS facade")?;
  assert!(emitted.is_entry, "plugin-emitted facade lost its entry classification");
  let directory = Fixture::new(&[])?;
  write(directory.0.as_path(), "package.json", r#"{"type":"module"}"#)?;
  for asset in &output.assets {
    write(directory.0.as_path(), asset.filename(), asset.content_as_bytes())?;
  }
  let out = url::Url::from_directory_path(directory.0.as_path())
    .map_err(|()| anyhow::anyhow!("output is not a directory URL"))?;
  node_import_value(&[out.join(&emitted.filename)?], &BTreeMap::new(), "", "globalThis.read()")?;
  Ok(())
}
