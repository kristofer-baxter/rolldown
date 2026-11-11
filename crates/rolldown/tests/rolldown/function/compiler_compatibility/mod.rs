use std::{
  borrow::Cow,
  fs,
  path::{Path, PathBuf},
  process::Command,
  sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
  },
  time::Duration,
};

use anyhow::{Context, Result, ensure};
use rolldown::{BundleOutput, Bundler, BundlerOptions};
use rolldown_common::{
  BundleMode, ChunkOptimizationOption, CommentsOptions, ExperimentalOptions, InlineConstOption,
  InputItem, ManglePropertiesPattern, ManglePropertiesPatterns, ModuleDefFormat,
  OptimizationOption, Output, OutputFormat, PreserveEntrySignatures, RawCompressOptions,
  RawMangleOptions, RawManglePropertiesOptions, RawMinifyOptions, RawMinifyOptionsDetailed,
  ScanMode,
};
use rolldown_plugin::{
  HookGenerateBundleArgs, HookLoadArgs, HookLoadOutput, HookLoadReturn, HookNoopReturn,
  HookResolveIdArgs, HookResolveIdOutput, HookResolveIdReturn, HookTransformArgs,
  HookTransformReturn, HookUsage, Plugin, PluginContext, SharedLoadPluginContext,
  SharedTransformPluginContext,
};
use tokio::{sync::Notify, task::JoinHandle};

#[cfg(feature = "experimental")]
mod bundled_hmr;

struct Fixture(PathBuf);

impl Fixture {
  fn new(files: &[(&str, &str)]) -> Result<Self> {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target").join(format!(
      "compiler-compatibility-{}-{}",
      std::process::id(),
      NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&path)?;
    let fixture = Self(path.canonicalize()?);
    fixture.write("package.json", r#"{"type":"module"}"#)?;
    for (name, source) in files {
      fixture.write(name, source)?;
    }
    Ok(fixture)
  }

  fn write(&self, name: &str, source: &str) -> Result<()> {
    let path = self.0.join(name);
    fs::create_dir_all(path.parent().context("fixture file parent")?)?;
    Ok(fs::write(path, source)?)
  }

  fn options(&self) -> BundlerOptions {
    BundlerOptions {
      cwd: Some(self.0.clone()),
      input: Some(vec![InputItem { name: Some("main".into()), import: "./main.js".into() }]),
      format: Some(OutputFormat::Esm),
      entry_filenames: Some("[name].js".to_string().into()),
      chunk_filenames: Some("chunks/[name].js".to_string().into()),
      ..Default::default()
    }
  }

  fn output(&self, output: &BundleOutput) -> Result<()> {
    let dist = self.0.join("dist");
    if dist.exists() {
      fs::remove_dir_all(&dist)?;
    }
    for asset in &output.assets {
      let path = dist.join(asset.filename());
      fs::create_dir_all(path.parent().context("output parent")?)?;
      fs::write(path, asset.content_as_bytes())?;
    }
    Ok(())
  }

  fn execute(&self, entry: &str) -> Result<serde_json::Value> {
    let script = format!(
      "globalThis.events=[];const timer=setTimeout(()=>{{throw Error('evaluation deadline')}},10000);\
       try{{await import({entry:?});console.log(JSON.stringify(events))}}finally{{clearTimeout(timer)}}"
    );
    let output = Command::new("node")
      .current_dir(&self.0)
      .args(["--input-type=module", "--eval", &script])
      .output()?;
    ensure!(
      output.status.success(),
      "bundle execution failed: {}",
      String::from_utf8_lossy(&output.stderr)
    );
    Ok(serde_json::from_slice(&output.stdout)?)
  }
}

impl Drop for Fixture {
  fn drop(&mut self) {
    let _ = fs::remove_dir_all(&self.0);
  }
}

fn partial_scan(fixture: &Fixture, name: &str) -> ScanMode<arcstr::ArcStr> {
  ScanMode::Partial(vec![fixture.0.join(name).to_string_lossy().as_ref().into()])
}

#[derive(Debug)]
struct DeferredMissing;

impl Plugin for DeferredMissing {
  fn name(&self) -> Cow<'static, str> {
    "deferred-missing".into()
  }

  fn register_hook_usage(&self) -> HookUsage {
    HookUsage::ResolveId
  }

  async fn resolve_id(
    &self,
    _ctx: &PluginContext,
    args: &HookResolveIdArgs<'_>,
  ) -> HookResolveIdReturn {
    if args.specifier.ends_with("missing.js") {
      return Err(rolldown::DeferredResolutionError::new("missing dependency".into()).into());
    }
    Ok(None)
  }
}

#[tokio::test]
async fn static_resolution_failures_fail_closed_with_and_without_native_semantics() -> Result<()> {
  for native in [false, true] {
    for deferred in [false, true] {
      let fixture = Fixture::new(&[("main.js", "import './missing.js';")])?;
      if !native {
        let mut ordinary = Bundler::with_plugins(
          fixture.options(),
          if deferred { vec![Plugin::new_shared(DeferredMissing)] } else { vec![] },
        )?;
        ensure!(ordinary.generate().await.is_err(), "ordinary static failure must not publish");
        ordinary.close().await?;
      }
      let mut options = fixture.options();
      options.experimental = Some(ExperimentalOptions {
        incremental_build: Some(true),
        resolve_new_url_to_asset: Some(native),
        ..Default::default()
      });
      let mut bundler = Bundler::with_plugins(
        options,
        if deferred { vec![Plugin::new_shared(DeferredMissing)] } else { vec![] },
      )?;
      ensure!(
        bundler.incremental_generate(ScanMode::Full).await.is_err(),
        "a full static failure published: native={native}, deferred={deferred}"
      );
      fixture.write("main.js", "events.push('ready');")?;
      bundler.incremental_generate(ScanMode::Full).await?;
      fixture.write("main.js", "import './missing.js';")?;
      ensure!(
        bundler.incremental_generate(partial_scan(&fixture, "main.js")).await.is_err(),
        "a partial static failure published: native={native}, deferred={deferred}"
      );
      fixture.write("main.js", "events.push('recovered');")?;
      fixture.output(&bundler.incremental_generate(partial_scan(&fixture, "main.js")).await?)?;
      assert_eq!(fixture.execute("./dist/main.js")?, serde_json::json!(["recovered"]));
      bundler.close().await?;
    }
  }
  Ok(())
}

#[tokio::test]
async fn deferred_dynamic_resolution_remains_lazy_without_native_semantics() -> Result<()> {
  let fixture = Fixture::new(&[("main.js", "export const load=()=>import('./missing.js');")])?;
  let mut bundler =
    Bundler::with_plugins(fixture.options(), vec![Plugin::new_shared(DeferredMissing)])?;
  fixture.output(&bundler.generate().await?)?;
  fixture.write(
    "run.js",
    "const m=await import('./dist/main.js');\
     try{await m.load()}catch(e){events.push(e.name)}",
  )?;
  assert_eq!(fixture.execute("./run.js")?, serde_json::json!(["TypeError"]));
  bundler.close().await?;
  Ok(())
}

#[tokio::test]
async fn cached_deferred_dynamic_failure_becomes_terminal_when_imported_statically() -> Result<()> {
  for native in [false, true] {
    let fixture = Fixture::new(&[("main.js", "export const load=()=>import('./missing.js');")])?;
    let mut options = fixture.options();
    options.experimental = Some(ExperimentalOptions {
      incremental_build: Some(true),
      resolve_new_url_to_asset: Some(native),
      ..Default::default()
    });
    let mut bundler =
      Bundler::with_plugins(options.clone(), vec![Plugin::new_shared(DeferredMissing)])?;
    bundler.incremental_generate(ScanMode::Full).await?;
    fixture.write("main.js", "import './missing.js';")?;
    ensure!(
      bundler.incremental_generate(partial_scan(&fixture, "main.js")).await.is_err(),
      "a cached dynamic failure became statically reachable: native={native}"
    );
    let mut fresh = Bundler::with_plugins(options, vec![Plugin::new_shared(DeferredMissing)])?;
    ensure!(fresh.generate().await.is_err());
    fresh.close().await?;
    fixture.write("main.js", "events.push('recovered');")?;
    fixture.output(&bundler.incremental_generate(partial_scan(&fixture, "main.js")).await?)?;
    assert_eq!(fixture.execute("./dist/main.js")?, serde_json::json!(["recovered"]));
    bundler.close().await?;
  }
  Ok(())
}

#[tokio::test]
async fn ordinary_dynamic_parse_and_link_failures_do_not_enable_native_isolation() -> Result<()> {
  for (dependency, route) in [
    ("export const broken = ;", "export const load=()=>import('./dependency.js');"),
    ("export const present = 1;", "export const load=()=>import('./route.js');"),
  ] {
    let fixture = Fixture::new(&[
      ("dependency.js", dependency),
      ("route.js", "export {missing} from './dependency.js';"),
      ("main.js", route),
    ])?;
    let mut bundler = Bundler::new(fixture.options())?;
    ensure!(bundler.generate().await.is_err(), "ordinary failure must not become native output");
    bundler.close().await?;
  }
  Ok(())
}

#[tokio::test]
async fn native_partial_scans_recheck_cached_barrel_failures_after_rollback_and_route_changes()
-> Result<()> {
  for cached_link in [false, true] {
    let fixture = Fixture::new(if cached_link {
      &[
        ("lib/package.json", r#"{"sideEffects":false}"#),
        ("lib/catalog.js", "export {value} from './a.js';export {missing} from './b.js';"),
        ("lib/a.js", "export const value='ready';"),
        ("lib/b.js", "export const present='present';"),
        ("route.js", "import{value}from'./lib/catalog.js';export{value};"),
        (
          "main.js",
          "import{present}from'./lib/b.js';events.push(present);export const load=()=>import('./route.js').then(m=>m.value);",
        ),
      ]
    } else {
      &[
        ("lib/package.json", r#"{"sideEffects":false}"#),
        ("lib/catalog.js", "import './missing.js';export * from './extra.js';"),
        ("lib/extra.js", "export const value=1;"),
        ("route.js", "import{value}from'./lib/catalog.js';export{value};"),
        ("main.js", "export const load=()=>import('./route.js').then(m=>m.value);"),
      ]
    })?;
    fixture.write(
      "run.js",
      "const m=await import('./dist/main.js');try{events.push(await m.load())}catch(e){events.push(e.name)}",
    )?;
    let mut options = fixture.options();
    options.experimental = Some(ExperimentalOptions {
      incremental_build: Some(true),
      resolve_new_url_to_asset: Some(true),
      lazy_barrel: Some(true),
      ..Default::default()
    });
    let plugins = || vec![Plugin::new_shared(DeferredMissing)];
    let mut bundler = Bundler::with_plugins(options.clone(), plugins())?;
    let first = bundler.incremental_generate(ScanMode::Full).await?;
    ensure!(
      first.assets.iter().all(|asset| {
        let code = String::from_utf8_lossy(asset.content_as_bytes());
        !code.contains("missing dependency") && !code.contains("__moduleLinkError")
      }),
      "unread barrel routes must not fail the first build"
    );
    fixture.output(&first)?;
    let expected_first =
      if cached_link { serde_json::json!(["present", "ready"]) } else { serde_json::json!([1]) };
    assert_eq!(fixture.execute("./run.js")?, expected_first);
    let updated = if cached_link {
      fixture
        .write("route.js", "import{missing}from'./lib/catalog.js';export const value=missing;")?;
      bundler.incremental_generate(partial_scan(&fixture, "route.js")).await?
    } else {
      fixture.write("lib/extra.js", "module.exports={value:2};")?;
      fixture.write(
        "main.js",
        "import './missing.js';export const load=()=>import('./route.js').then(m=>m.value);",
      )?;
      let changed = ["lib/extra.js", "main.js"]
        .map(|name| fixture.0.join(name).to_string_lossy().as_ref().into());
      ensure!(
        bundler.incremental_generate(ScanMode::Partial(changed.into())).await.is_err(),
        "the static failure must abort while the lazy barrel failure is staged"
      );
      fixture.write("main.js", "export const load=()=>import('./route.js').then(m=>m.value);")?;
      bundler.incremental_generate(partial_scan(&fixture, "main.js")).await?
    };
    fixture.output(&updated)?;
    let expected = if cached_link {
      serde_json::json!(["present", "SyntaxError"])
    } else {
      serde_json::json!(["TypeError"])
    };
    assert_eq!(fixture.execute("./run.js")?, expected);
    bundler.close().await?;
    let mut fresh = Bundler::with_plugins(options, plugins())?;
    fixture.output(&fresh.generate().await?)?;
    assert_eq!(fixture.execute("./run.js")?, expected);
    fresh.close().await?;
  }
  Ok(())
}

#[tokio::test]
async fn incremental_plain_object_pruning_discovers_new_importer_reads() -> Result<()> {
  let fixture = Fixture::new(&[
    ("owner.js", "export const tokens={live:'LIVE',dead:'NEWLY_READ_VALUE'};"),
    ("reader.js", "import{tokens}from'./owner.js';events.push(tokens.live);"),
    ("changed.js", "events.push('before');"),
    ("main.js", "import './reader.js';import './changed.js';"),
  ])?;
  let mut options = fixture.options();
  options.preserve_modules = Some(true);
  options.experimental = Some(ExperimentalOptions {
    incremental_build: Some(true),
    resolve_new_url_to_asset: Some(false),
    on_demand_wrapping: Some(true),
    ..Default::default()
  });
  let mut bundler = Bundler::new(options.clone())?;
  bundler.set_omit_reused_chunk_bytes(true);
  let first = bundler.incremental_generate(ScanMode::Full).await?;
  bundler.incremental_generate(ScanMode::Partial(vec![])).await?;
  ensure!(
    first.assets.iter().all(|asset| {
      !String::from_utf8_lossy(asset.content_as_bytes()).contains("NEWLY_READ_VALUE")
    }),
    "the initial link must prune the unread property"
  );
  fixture.write("changed.js", "import{tokens}from'./owner.js';events.push(tokens.dead);")?;
  let mut updated = bundler.incremental_generate(partial_scan(&fixture, "changed.js")).await?;
  for asset in &mut updated.assets {
    if let Output::Chunk(chunk) = asset
      && chunk.code.is_empty()
    {
      let original = first
        .assets
        .iter()
        .find_map(|asset| {
          let Output::Chunk(original) = asset else { return None };
          (original.filename == chunk.filename).then_some(original)
        })
        .context("omitted chunk existed in the first output")?;
      Arc::make_mut(chunk).code.clone_from(&original.code);
    }
  }
  fixture.output(&updated)?;
  assert_eq!(fixture.execute("./dist/main.js")?, serde_json::json!(["LIVE", "NEWLY_READ_VALUE"]));
  ensure!(!bundler.last_fast_native_relink(), "the cached read analysis must not be reused");
  ensure!(
    matches!(
      bundler.last_native_relink_fallback(),
      Some("plain_object_reads" | "structural" | "no_cached_link")
    ),
    "new reads require a fresh link: {:?}",
    bundler.last_native_relink_fallback()
  );
  let mut fresh = Bundler::new(options)?;
  fixture.output(&fresh.incremental_generate(ScanMode::Full).await?)?;
  assert_eq!(fixture.execute("./dist/main.js")?, serde_json::json!(["LIVE", "NEWLY_READ_VALUE"]));
  fresh.close().await?;
  bundler.close().await?;
  Ok(())
}

#[derive(Debug)]
struct ReplaceMangledChunk {
  replace: Arc<AtomicBool>,
  rename: bool,
}

impl Plugin for ReplaceMangledChunk {
  fn name(&self) -> Cow<'static, str> {
    "replace-mangled-chunk".into()
  }

  fn register_hook_usage(&self) -> HookUsage {
    HookUsage::GenerateBundle
  }

  async fn generate_bundle(
    &self,
    _ctx: &PluginContext,
    args: &mut HookGenerateBundleArgs<'_>,
  ) -> HookNoopReturn {
    if !self.replace.load(Ordering::SeqCst) {
      for output in args.bundle.iter_mut() {
        if let Output::Chunk(chunk) = output {
          let chunk = Arc::make_mut(chunk);
          chunk.name = "hook-metadata".into();
          if self.rename && chunk.filename.as_str() == "main.js" {
            chunk.filename = "renamed.js".into();
          }
        }
      }
      args.bundle.push(Output::Asset(Arc::new(rolldown_common::OutputAsset {
        filename: "hook.txt".into(),
        source: "asset".to_string().into(),
        names: vec![],
        original_file_names: vec![],
      })));
      return Ok(());
    }
    let read = regex::Regex::new(r"events\.push\(([\w$]+)\.([\w$]+)")?;
    for output in args.bundle.iter_mut() {
      let Output::Chunk(chunk) = output else { continue };
      if chunk.filename.as_str() != "main.js" {
        continue;
      }
      let captures = read.captures(&chunk.code).context("entry reads a shared object")?;
      let rewritten = chunk.code.replacen(
        &format!("events.push({}.{}", &captures[1], &captures[2]),
        &format!("events.push({}.shared_", &captures[1]),
        1,
      );
      let chunk = Arc::make_mut(chunk);
      chunk.code = format!("{rewritten}\n/* hook replacement */");
      if self.rename {
        chunk.filename = "renamed.js".into();
      }
    }
    Ok(())
  }
}

#[tokio::test]
async fn shared_property_mangling_allows_metadata_changes_but_rejects_code_replacements()
-> Result<()> {
  for override_options in [false, true] {
    for active in [false, true] {
      for rename in [false, true] {
        let fixture = Fixture::new(&[
          (
            "main.js",
            "import{value}from'./owner.js';events.push(value.shared_,value['occupied_']);",
          ),
          ("other.js", "import{value}from'./owner.js';events.push(value.shared_,value.occupied_);"),
          ("owner.js", "export const value={shared_:7,occupied_:8};"),
        ])?;
        let mut options = fixture.options();
        options
          .input
          .as_mut()
          .expect("entries")
          .push(InputItem { name: Some("other".into()), import: "./other.js".into() });
        let mangle = active
          .then(|| oxc::minifier::ManglePropertiesOptions::from_pattern("_$").expect("pattern"));
        options.minify = Some(RawMinifyOptions::Object(RawMinifyOptionsDetailed {
          mangle: None,
          mangle_properties: if override_options {
            None
          } else {
            mangle.clone().map(|options| {
              Box::new(RawManglePropertiesOptions {
                options,
                patterns: ManglePropertiesPatterns {
                  include: ManglePropertiesPattern { source: "_$".into(), flags: String::new() },
                  exclude: None,
                },
              })
            })
          },
          compress: Some(RawCompressOptions::default()),
          remove_whitespace: true,
          ascii_only: false,
        }));
        options.experimental =
          Some(ExperimentalOptions { incremental_build: Some(true), ..Default::default() });
        let replace = Arc::new(AtomicBool::new(true));
        let mut bundler = Bundler::with_plugins(
          options,
          vec![Plugin::new_shared(ReplaceMangledChunk { replace: Arc::clone(&replace), rename })],
        )?;
        let filename = if rename { "renamed.js" } else { "main.js" };
        let entry = format!("./dist/{filename}");
        let result = bundler
          .with_cached_bundle_experimental(BundleMode::IncrementalFullBuild, async |bundle| {
            if override_options {
              bundle.set_mangle_properties(mangle.clone());
            }
            let scanned = bundle.scan_modules(ScanMode::Full).await?;
            bundle.bundle_generate(scanned).await
          })
          .await;
        if active {
          match result {
            Ok(output) => {
              fixture.output(&output)?;
              anyhow::bail!(
                "same-name replacement escaped shared mapping: {:?}",
                fixture.execute(&entry)?
              );
            }
            Err(errors) => {
              let message = format!("{errors:?}");
              ensure!(
                message.contains(filename) && message.contains("generateBundle"),
                "the diagnostic must identify the late code change: {message}"
              );
            }
          }
        } else {
          fixture.output(&result?)?;
          assert_eq!(fixture.execute(&entry)?, serde_json::json!([7, 8]));
        }
        replace.store(false, Ordering::SeqCst);
        let recovered = bundler
          .with_cached_bundle_experimental(BundleMode::IncrementalFullBuild, async |bundle| {
            if override_options {
              bundle.set_mangle_properties(mangle.clone());
            }
            let scanned = bundle.scan_modules(ScanMode::Full).await?;
            bundle.bundle_generate(scanned).await
          })
          .await?;
        fixture.output(&recovered)?;
        assert_eq!(fixture.execute(&entry)?, serde_json::json!([7, 8]));
        ensure!(recovered.assets.iter().any(|asset| asset.filename() == "hook.txt"));
        fixture.output(&bundler.generate().await?)?;
        assert_eq!(fixture.execute(&entry)?, serde_json::json!([7, 8]));
        bundler.close().await?;
      }
    }
  }
  Ok(())
}

type Waiters = Arc<Mutex<Vec<JoinHandle<Result<()>>>>>;

#[tokio::test]
async fn strict_native_destructuring_preserves_getters_across_a_shared_runtime() -> Result<()> {
  for declaration in [
    "const { unused } = globalThis.input;",
    "const { unused: renamed } = globalThis.input;",
    "const { missing = globalThis.input.unused } = globalThis.input;",
    "const { nested: { unused } } = { nested: globalThis.input };",
    "const [unused] = [globalThis.input.unused];",
  ] {
    let fixture = Fixture::new(&[
      ("main.js", "import './destructure.js';export function read(){return 42}"),
      ("other.js", "export function read(){return 24}"),
      ("destructure.js", declaration),
    ])?;
    for (name, entry) in [("native.js", "./main.js"), ("bundled.js", "./dist/main.cjs")] {
      fixture.write(
        name,
        &format!(
          "const observed=[];globalThis.input={{get unused(){{observed.push('get');return 1}}}};\
         const main=await import({entry:?});events.push([main.read(),observed]);"
        ),
      )?;
    }
    let expected = fixture.execute("./native.js")?;
    assert_eq!(expected, serde_json::json!([[42, ["get"]]]));
    for native in [false, true] {
      for on_demand in [false, true] {
        let mut options = fixture.options();
        options.strict_execution_order = Some(true);
        options.format = Some(OutputFormat::Cjs);
        options.entry_filenames = Some("[name].cjs".to_string().into());
        options.chunk_filenames = Some("[name]-[hash].cjs".to_string().into());
        options.preserve_entry_signatures = Some(PreserveEntrySignatures::Strict);
        options
          .input
          .as_mut()
          .expect("entries")
          .push(InputItem { name: Some("other".into()), import: "./other.js".into() });
        options.experimental = Some(ExperimentalOptions {
          resolve_new_url_to_asset: Some(native),
          on_demand_wrapping: Some(on_demand),
          ..Default::default()
        });
        let mut bundler = Bundler::new(options)?;
        let output = bundler.generate().await?;
        let chunks = output.assets.iter().filter(|asset| matches!(asset, Output::Chunk(_))).count();
        if native {
          ensure!(chunks > 2, "native mode must exercise a shared runtime chunk");
        }
        fixture.output(&output)?;
        assert_eq!(fixture.execute("./bundled.js")?, expected, "{declaration}, native={native}");
        bundler.close().await?;
      }
    }
  }
  Ok(())
}

#[tokio::test]
async fn strict_native_cycle_folds_inlined_constant_owners_into_the_eager_entry() -> Result<()> {
  let fixture = Fixture::new(&[
    (
      "main.js",
      "import{shared,tagged}from'./shared.js';globalThis.order=['main'];\
      globalThis.value=`${shared}:${tagged()}`;\
      globalThis.lazy=import('./lazy-a.js').then(a=>import('./lazy-b.js')\
      .then(b=>import('./lazy-c.js').then(c=>[a.value,b.value,c.value])));",
    ),
    (
      "shared.js",
      "import{tag}from'./tag.js';export const shared='shared';export function tagged(){return tag}",
    ),
    (
      "tag.js",
      "import{shared}from'./shared.js';export const tag='tag';export function loop(){return shared}",
    ),
    (
      "lazy-a.js",
      "import{tag}from'./tag.js';globalThis.order.push('lazy-a');export const value=`a:${tag}`;",
    ),
    (
      "lazy-b.js",
      "import{tag}from'./tag.js';globalThis.order.push('lazy-b');export const value=`b:${tag}`;",
    ),
    ("lazy-c.js", "globalThis.order.push('lazy-c');export const value='c';"),
  ])?;
  for (name, entry) in [("native.js", "./main.js"), ("bundled.js", "./dist/main.js")] {
    fixture.write(
      name,
      &format!(
        "await import({entry:?});const lazy=await globalThis.lazy;\
       events.push({{value:globalThis.value,lazy,order:globalThis.order}});"
      ),
    )?;
  }
  let expected = fixture.execute("./native.js")?;
  assert_eq!(
    expected,
    serde_json::json!([{
      "value":"shared:tag","lazy":["a:tag","b:tag","c"],"order":["main","lazy-a","lazy-b","lazy-c"]
    }])
  );
  for native in [false, true] {
    for on_demand in [false, true] {
      let mut options = fixture.options();
      options.strict_execution_order = Some(true);
      options.experimental = Some(ExperimentalOptions {
        resolve_new_url_to_asset: Some(native),
        on_demand_wrapping: Some(on_demand),
        chunk_optimization: Some(ChunkOptimizationOption::Bool(true)),
        ..Default::default()
      });
      let mut bundler = Bundler::new(options)?;
      let output = bundler.generate().await?;
      let host = |name: &str| {
        output.assets.iter().position(|asset| {
          let Output::Chunk(chunk) = asset else { return false };
          chunk.module_ids.iter().any(|id| id.ends_with(name))
        })
      };
      if native {
        ensure!(host("/tag.js").is_some(), "native mode retains the constant owner");
        assert_eq!(host("/tag.js"), host("/shared.js"));
        assert_eq!(host("/tag.js"), host("/main.js"));
      } else {
        ensure!(host("/tag.js").is_none(), "ordinary mode may eliminate the inlined owner");
      }
      fixture.output(&output)?;
      assert_eq!(
        fixture.execute("./bundled.js")?,
        expected,
        "native={native}, wrapping={on_demand}"
      );
      bundler.close().await?;
    }
  }
  Ok(())
}

#[tokio::test]
async fn native_partial_scan_marks_a_dropped_require_dependency_failure_unreached() -> Result<()> {
  let fixture = Fixture::new(&[
    (
      "main.js",
      "import owner from'./owner.cjs';import{leaf}from'./leaf.js';events.push(owner,leaf);",
    ),
    ("owner.cjs", "module.exports=require('./dep.js');"),
    ("dep.js", "export const dep='dep';"),
    ("leaf.js", "export const leaf='leaf';"),
  ])?;
  let mut options = fixture.options();
  options.experimental = Some(ExperimentalOptions {
    incremental_build: Some(true),
    resolve_new_url_to_asset: Some(true),
    ..Default::default()
  });
  let mut bundler = Bundler::new(options.clone())?;
  bundler.incremental_generate(ScanMode::Full).await?;
  fixture.write("main.js", "import{leaf}from'./leaf.js';events.push(leaf);")?;
  fixture.write("dep.js", "import{absent}from'./leaf.js';export const dep=absent;")?;
  let changed = ScanMode::Partial(vec![
    fixture.0.join("main.js").to_string_lossy().as_ref().into(),
    fixture.0.join("dep.js").to_string_lossy().as_ref().into(),
  ]);
  let error = bundler.incremental_generate(changed).await.err().context("update must abort")?;
  ensure!(format!("{error:?}").contains("absent"), "the missing export must be diagnosed");
  ensure!(bundler.last_scan_failures_unreached(), "the entry dropped the require closure");
  let full = bundler.incremental_generate(ScanMode::Full).await?;
  fixture.output(&full)?;
  assert_eq!(fixture.execute("./dist/main.js")?, serde_json::json!(["leaf"]));
  let mut fresh = Bundler::new(options)?;
  fixture.output(&fresh.incremental_generate(ScanMode::Full).await?)?;
  assert_eq!(fixture.execute("./dist/main.js")?, serde_json::json!(["leaf"]));
  fresh.close().await?;
  bundler.close().await?;
  Ok(())
}

#[derive(Debug)]
struct ContextLoadPanic {
  helper: String,
  panic: Arc<AtomicBool>,
  started: Notify,
  release: Notify,
  cancel_waiter: bool,
  waiters: Waiters,
}

impl Plugin for ContextLoadPanic {
  fn name(&self) -> Cow<'static, str> {
    "context-load-panic".into()
  }

  fn register_hook_usage(&self) -> HookUsage {
    HookUsage::Load | HookUsage::Transform
  }

  async fn load(&self, _ctx: SharedLoadPluginContext, args: &HookLoadArgs<'_>) -> HookLoadReturn {
    if args.id == self.helper {
      self.started.notify_one();
      self.release.notified().await;
      assert!(!self.panic.load(Ordering::SeqCst), "context load panic");
    }
    Ok(None)
  }

  async fn transform(
    &self,
    ctx: SharedTransformPluginContext,
    args: &HookTransformArgs<'_>,
  ) -> HookTransformReturn {
    if args.code.contains("LOAD_HELPER") {
      let helper = self.helper.clone();
      let waiter =
        tokio::spawn(async move { ctx.load(&helper, None, ModuleDefFormat::Unknown).await });
      self.started.notified().await;
      if self.cancel_waiter && self.panic.load(Ordering::SeqCst) {
        waiter.abort();
        ensure!(waiter.await.is_err_and(|error| error.is_cancelled()));
      } else {
        self.waiters.lock().expect("waiter lock poisoned").push(waiter);
      }
      self.release.notify_one();
    }
    Ok(None)
  }
}

async fn finish_waiters(waiters: &Waiters, expected: usize) -> Result<()> {
  let handles = std::mem::take(&mut *waiters.lock().expect("waiter lock poisoned"));
  ensure!(handles.len() == expected, "expected {expected} waiters, got {}", handles.len());
  for mut handle in handles {
    let result = tokio::time::timeout(Duration::from_secs(10), &mut handle).await;
    if result.is_err() {
      handle.abort();
      let _ = handle.await;
    }
    result.context("this.load waiter was never released")???;
  }
  Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_load_panic_releases_live_and_cancelled_waiters_and_recovers() -> Result<()> {
  for (partial, cancel_waiter) in [(false, false), (true, false), (false, true), (true, true)] {
    let fixture = Fixture::new(&[
      ("main.js", "console.log('initial');"),
      ("helper.js", "export const helper = 1;"),
    ])?;
    let panic = Arc::new(AtomicBool::new(true));
    let waiters = Arc::new(Mutex::new(Vec::new()));
    let mut options = fixture.options();
    options.experimental =
      Some(ExperimentalOptions { incremental_build: Some(true), ..Default::default() });
    let mut bundler = Bundler::with_plugins(
      options,
      vec![Plugin::new_shared(ContextLoadPanic {
        helper: fixture.0.join("helper.js").to_string_lossy().into_owned(),
        panic: Arc::clone(&panic),
        started: Notify::new(),
        release: Notify::new(),
        cancel_waiter,
        waiters: Arc::clone(&waiters),
      })],
    )?;
    if partial {
      bundler.incremental_generate(ScanMode::Full).await?;
    }
    fixture.write("main.js", "// LOAD_HELPER\nconsole.log('updated');")?;
    let mode = if partial {
      ScanMode::Partial(vec![fixture.0.join("main.js").to_string_lossy().into_owned().into()])
    } else {
      ScanMode::Full
    };
    let failed =
      tokio::time::timeout(Duration::from_secs(10), bundler.incremental_generate(mode)).await?;
    let error = format!("{:?}", failed.err().context("hook must fail the scan")?);
    ensure!(error.contains("helper.js") && error.contains("context load panic"), "{error}");
    finish_waiters(&waiters, usize::from(!cancel_waiter)).await?;
    panic.store(false, Ordering::SeqCst);
    fixture.write("main.js", "// LOAD_HELPER\nconsole.log('recovered');")?;
    let recovered =
      tokio::time::timeout(Duration::from_secs(10), bundler.incremental_generate(ScanMode::Full))
        .await??;
    ensure!(
      recovered
        .assets
        .iter()
        .any(|asset| { String::from_utf8_lossy(asset.content_as_bytes()).contains("recovered") }),
      "recovery must publish the repaired entry"
    );
    finish_waiters(&waiters, 1).await?;
    bundler.close().await?;
  }
  Ok(())
}

#[tokio::test]
async fn inlined_constants_retain_owner_and_barrel_initialization_across_chunks() -> Result<()> {
  let fixture = Fixture::new(&[
    ("main.js", "import { SIZE } from './ui/index.js'; events.push(SIZE);"),
    ("namespace.js", "import * as ui from './ui/index.js'; events.push(ui.SIZE);"),
    ("direct.js", "import { SIZE } from './ui/button.js'; events.push(SIZE);"),
    ("ui/index.js", "export * from './button.js'; events.push('barrel');"),
    ("ui/button.css.js", "events.push('style');"),
  ])?;
  for owner in [
    "import './button.css.js'; export const SIZE = 'md';",
    "import './button.css.js'; export const SIZE = 'md'; events.push('button');",
  ] {
    fixture.write("ui/button.js", owner)?;
    for (strict, on_demand) in [(false, false), (true, false), (true, true)] {
      for kept_barrel in [false, true] {
        fixture.write(
          "ui/package.json",
          if kept_barrel {
            r#"{"sideEffects":["./index.js","./button.css.js"]}"#
          } else {
            r#"{"sideEffects":["./button.css.js"]}"#
          },
        )?;
        let mut options = fixture.options();
        options.input = Some(
          ["main", "namespace", "direct"]
            .into_iter()
            .map(|name| InputItem { name: Some(name.into()), import: format!("./{name}.js") })
            .collect(),
        );
        options.strict_execution_order = Some(strict);
        options.optimization = Some(OptimizationOption {
          inline_const: Some(InlineConstOption::Bool(true)),
          ..Default::default()
        });
        options.experimental =
          Some(ExperimentalOptions { on_demand_wrapping: Some(on_demand), ..Default::default() });
        let mut bundler = Bundler::new(options)?;
        fixture.output(&bundler.generate().await?)?;
        bundler.close().await?;
        for name in ["main", "namespace", "direct"] {
          let reference = if kept_barrel || name == "direct" { name } else { "direct" };
          let expected = fixture.execute(&format!("./{reference}.js"))?;
          let actual = fixture.execute(&format!("./dist/{name}.js"))?;
          ensure!(
            actual == expected,
            "{name}, strict={strict}, on_demand={on_demand}, kept_barrel={kept_barrel}, \
             owner={owner}: {actual} != {expected}"
          );
        }
      }
    }
  }
  fixture.write("ui/package.json", r#"{"sideEffects":false}"#)?;
  for on_demand in [false, true] {
    let mut options = fixture.options();
    options.strict_execution_order = Some(true);
    options.optimization = Some(OptimizationOption {
      inline_const: Some(InlineConstOption::Bool(true)),
      ..Default::default()
    });
    options.experimental =
      Some(ExperimentalOptions { on_demand_wrapping: Some(on_demand), ..Default::default() });
    let mut bundler = Bundler::new(options)?;
    fixture.output(&bundler.generate().await?)?;
    bundler.close().await?;
    assert_eq!(fixture.execute("./dist/main.js")?, serde_json::json!(["md"]));
  }
  Ok(())
}

#[tokio::test]
async fn literal_namespace_read_does_not_eagerly_initialize_a_later_pure_export() -> Result<()> {
  for pure in ["/* @__PURE__ */", ""] {
    let constants =
      format!("export const x=1;function mark(){{globalThis.value=0}}export const y={pure}mark();");
    let fixture = Fixture::new(&[
      ("main.js", "await import('./page-b.js');await import('./page-a.js');"),
      ("constants.js", &constants),
      ("bridge.js", "export * from './constants.js';"),
      (
        "page-b.js",
        "import*as bridge from './bridge.js';events.push(['b',bridge.x,globalThis.value??null]);",
      ),
      (
        "page-a.js",
        "import{x,y}from './constants.js';events.push(['a',x,y??null,globalThis.value]);",
      ),
    ])?;
    for preserved in [false, true] {
      fixture.write(
        "constants.js",
        &if preserved {
          format!(
            "import{{x,mark}}from'\\0native-cell:01';export{{x}};export const y={pure}mark();"
          )
        } else {
          constants.clone()
        },
      )?;
      let mut options = fixture.options();
      options.preserve_modules = Some(preserved);
      options.strict_execution_order = Some(!preserved);
      options.experimental = Some(ExperimentalOptions {
        resolve_new_url_to_asset: Some(true),
        on_demand_wrapping: Some(true),
        ..Default::default()
      });
      options.optimization = Some(OptimizationOption {
        inline_const: Some(InlineConstOption::Bool(true)),
        ..Default::default()
      });
      let mut bundler = Bundler::with_plugins(
        options,
        if preserved { vec![Plugin::new_shared(LiteralCell)] } else { vec![] },
      )?;
      fixture.output(&bundler.generate().await?)?;
      bundler.close().await?;
      let expected = serde_json::json!([
        ["b", 1, if pure.is_empty() { Some(0) } else { None }],
        ["a", 1, null, 0],
      ]);
      assert_eq!(
        fixture.execute("./dist/main.js")?,
        expected,
        "preserved={preserved}, pure={pure}"
      );
    }
  }
  Ok(())
}

#[derive(Debug)]
struct LiteralCell;

#[tokio::test]
async fn enum_initialization_observes_object_prototype_setters() -> Result<()> {
  let fixture = Fixture::new(&[(
    "main.js",
    "Object.defineProperty(Object.prototype,'A',{set(value){events.push(value)},configurable:true});\
     var E;(function(E){E[E.A=0]='A'})(E||(E={}));\
     events.push(Object.hasOwn(E,'A'),E[0]);delete Object.prototype.A;",
  )])?;
  let mut options = fixture.options();
  options.minify = Some(RawMinifyOptions::Bool(true));
  let mut bundler = Bundler::new(options)?;
  fixture.output(&bundler.generate().await?)?;
  bundler.close().await?;
  assert_eq!(fixture.execute("./dist/main.js")?, serde_json::json!([0, false, "A"]));
  Ok(())
}

#[tokio::test]
async fn defines_preserve_shorthand_proto_own_properties_and_destructured_prefixes() -> Result<()> {
  let fixture = Fixture::new(&[(
    "main.js",
    "const value={__proto__};const {MODE}=process.env;\
     events.push(Object.hasOwn(value,'__proto__'),\
     Object.getPrototypeOf(value)===Object.prototype,value.__proto__,MODE);",
  )])?;
  for minify in [false, true] {
    let mut options = fixture.options();
    options.define = Some(
      [
        ("__proto__".to_owned(), "7".to_owned()),
        ("process.env.MODE".to_owned(), "'test'".to_owned()),
      ]
      .into_iter()
      .collect(),
    );
    options.minify = Some(RawMinifyOptions::Bool(minify));
    let mut bundler = Bundler::new(options)?;
    fixture.output(&bundler.generate().await?)?;
    bundler.close().await?;
    assert_eq!(
      fixture.execute("./dist/main.js")?,
      serde_json::json!([true, true, 7, "test"]),
      "minify={minify}"
    );
  }
  Ok(())
}

#[tokio::test]
async fn defines_merge_dotted_children_without_changing_prototype_setters() -> Result<()> {
  for (parent, target, child, own_proto, null_proto, proto_value, inherited) in [
    ("({__proto__:null})", "process.env", "process.env.MODE", false, true, 0, 0),
    (r#"({"__proto__":null})"#, "process.env", "process.env.MODE", false, true, 0, 0),
    ("({__proto__:{inherited:1}})", "process.env", "process.env.MODE", false, false, 0, 1),
    (r#"({["__proto__"]:7})"#, "process.env", "process.env.MODE", true, false, 7, 0),
    ("({__proto__})", "process.env", "process.env.MODE", true, false, 7, 0),
    (r#"({__proto__:null,["__proto__"]:7})"#, "process.env", "process.env.MODE", true, true, 7, 0),
    (r#"({["__proto__"]:7,__proto__:null})"#, "process.env", "process.env.MODE", true, true, 7, 0),
    (
      "({nested:{__proto__:null}})",
      "process.env.nested",
      "process.env.nested.MODE",
      false,
      true,
      0,
      0,
    ),
  ] {
    let fixture = Fixture::new(&[(
      "main.js",
      &format!(
        "const value={target};\
         events.push(Object.getPrototypeOf(value)===null,Object.hasOwn(value,'__proto__'),\
         Object.hasOwn(value,'__proto__')?value.__proto__:0,value.MODE,\
         Object.hasOwn(value,'MODE'),Object.getPrototypeOf(value)?.inherited??0);"
      ),
    )])?;
    fixture.write(
      "run.js",
      "Object.defineProperty(globalThis,'__proto__',{value:7,configurable:true});\
       await import('./dist/main.js');",
    )?;
    for minify in [false, true] {
      let mut options = fixture.options();
      options.define = Some(
        [
          ("process.env".to_owned(), parent.to_owned()),
          (child.to_owned(), "'configured'".to_owned()),
        ]
        .into_iter()
        .collect(),
      );
      options.minify = Some(RawMinifyOptions::Bool(minify));
      let mut bundler = Bundler::new(options)?;
      fixture.output(&bundler.generate().await?)?;
      bundler.close().await?;
      assert_eq!(
        fixture.execute("./run.js")?,
        serde_json::json!([null_proto, own_proto, proto_value, "configured", true, inherited]),
        "parent={parent}, minify={minify}"
      );
    }
  }
  Ok(())
}

#[tokio::test]
async fn defines_keep_dotted_proto_children_distinct_from_prototype_setters() -> Result<()> {
  let fixture = Fixture::new(&[(
    "main.js",
    "const value=process.env;\
     events.push(Object.getPrototypeOf(value)===null,Object.hasOwn(value,'__proto__'),\
     value.__proto__?.MODE,value.__proto__?.EXTRA);",
  )])?;
  for minify in [false, true] {
    let mut options = fixture.options();
    options.define = Some(
      [
        ("process.env".to_owned(), "({__proto__:null})".to_owned()),
        ("process.env.__proto__".to_owned(), "({MODE:'configured'})".to_owned()),
        ("process.env.__proto__.EXTRA".to_owned(), "'extra'".to_owned()),
      ]
      .into_iter()
      .collect(),
    );
    options.minify = Some(RawMinifyOptions::Bool(minify));
    let mut bundler = Bundler::new(options)?;
    fixture.output(&bundler.generate().await?)?;
    bundler.close().await?;
    assert_eq!(
      fixture.execute("./dist/main.js")?,
      serde_json::json!([true, true, "configured", "extra"]),
      "minify={minify}"
    );
  }
  Ok(())
}

#[tokio::test]
async fn destructured_defines_preserve_object_evaluation_and_inherited_reads() -> Result<()> {
  for (prelude, value) in [
    ("", "({used:1,unused:(events.push('unused'),2)})"),
    ("", "({unused:(()=>{events.push('before');throw Error('unused')})(),used:1})"),
    ("", "({used:1,unused:missingDefineValue})"),
    ("", "({used:1,unused:later})"),
    ("", "({used:1,[(events.push('key'),'unused')]:2})"),
    ("", "({used:1,['unused']:(events.push('value'),2)})"),
    ("", "({get used(){events.push('get');return this.unused},unused:7})"),
    ("", "({__proto__:{get used(){events.push('inherited');return this.unused}},unused:7})"),
    ("", "({__proto__:null,unused:7})"),
    (
      "Object.defineProperty(Object.prototype,'used',{configurable:true,\
       get(){events.push('inherited');return this.unused}});",
      "({unused:7})",
    ),
    ("", "({used:1,...(events.push('spread'),{other:2}),unused:7})"),
    ("", "({used:1,unused:'pruned_unused_marker'})"),
  ] {
    for target in ["CONFIG", "globalThis.CONFIG", "import.meta.env"] {
      for assignment in [false, true] {
        let binding = if assignment {
          format!("let used;try{{({{used}}={target});events.push(used)}}")
        } else {
          format!("try{{const {{used}}={target};events.push(used)}}")
        };
        let source =
          format!("{prelude}{binding}catch(error){{events.push(error.name)}}const later=2;");
        let fixture = Fixture::new(&[("main.js", &source)])?;
        fixture.write("reference.js", &source.replace(target, value))?;
        let expected = fixture.execute("./reference.js")?;
        for minify in [false, true] {
          let mut options = fixture.options();
          options.define = Some(std::iter::once((target.to_owned(), value.to_owned())).collect());
          options.minify = Some(RawMinifyOptions::Bool(minify));
          let mut bundler = Bundler::new(options)?;
          let output = bundler.generate().await?;
          fixture.output(&output)?;
          bundler.close().await?;
          assert_eq!(
            fixture.execute("./dist/main.js")?,
            expected,
            "value={value}, target={target}, assignment={assignment}, minify={minify}"
          );
          if value.contains("pruned_unused_marker") {
            assert!(
              output.assets.iter().all(|asset| {
                !String::from_utf8_lossy(asset.content_as_bytes()).contains("pruned_unused_marker")
              }),
              "pure unused define property survived: target={target}, assignment={assignment}, \
               minify={minify}"
            );
          }
        }
      }
    }
  }
  Ok(())
}

#[tokio::test]
async fn defines_preserve_duplicate_property_evaluations_when_merging_children() -> Result<()> {
  for (parent, child, expected) in [
    (
      "({x:(events.push('first'),1),x:2})",
      "globalThis.CONFIG.MODE",
      serde_json::json!(["first", {"x":2,"MODE":"added"}]),
    ),
    (
      "({nested:{x:(events.push('first'),1),x:2}})",
      "globalThis.CONFIG.MODE",
      serde_json::json!(["first", {"nested":{"x":2},"MODE":"added"}]),
    ),
    (
      "({branch:{old:(events.push('first'),1)},branch:{current:2}})",
      "globalThis.CONFIG.branch.ADDED",
      serde_json::json!(["first", {"branch":{"current":2,"ADDED":"added"}}]),
    ),
    (
      "({branch:{old:(events.push('first'),1)},branch:2})",
      "globalThis.CONFIG.branch.ADDED",
      serde_json::json!(["first", {"branch":2}]),
    ),
    (
      "({branch:(events.push('first'),1),branch:{current:2}})",
      "globalThis.CONFIG.branch.ADDED",
      serde_json::json!(["first", {"branch":{"current":2,"ADDED":"added"}}]),
    ),
    (
      "({['__proto__']:(events.push('first'),1),['__proto__']:2})",
      "globalThis.CONFIG.MODE",
      serde_json::json!(["first", {"__proto__":2,"MODE":"added"}]),
    ),
  ] {
    let fixture =
      Fixture::new(&[("main.js", "const value=globalThis.CONFIG;events.push(value);")])?;
    for minify in [false, true] {
      let mut options = fixture.options();
      options.define = Some(
        [
          ("globalThis.CONFIG".to_owned(), parent.to_owned()),
          (child.to_owned(), "'added'".to_owned()),
        ]
        .into_iter()
        .collect(),
      );
      options.minify = Some(RawMinifyOptions::Bool(minify));
      let mut bundler = Bundler::new(options)?;
      fixture.output(&bundler.generate().await?)?;
      bundler.close().await?;
      assert_eq!(
        fixture.execute("./dist/main.js")?,
        expected,
        "parent={parent}, child={child}, minify={minify}"
      );
    }
  }
  Ok(())
}

#[tokio::test]
async fn defines_preserve_duplicate_evaluations_from_merged_child_objects() -> Result<()> {
  let fixture = Fixture::new(&[("main.js", "const value=globalThis.CONFIG;events.push(value);")])?;
  for (parent, expected) in [
    (
      "({branch:{kept:1}})",
      serde_json::json!(["first", "second", {"branch":{"kept":1,"x":2,"y":3}}]),
    ),
    (
      "({branch:{old:(events.push('earlier'),0)},branch:{kept:1}})",
      serde_json::json!(["earlier", "first", "second", {"branch":{"kept":1,"x":2,"y":3}}]),
    ),
  ] {
    for minify in [false, true] {
      let mut options = fixture.options();
      options.define = Some(
        [
          ("globalThis.CONFIG".to_owned(), parent.to_owned()),
          (
            "globalThis.CONFIG.branch".to_owned(),
            "({x:(events.push('first'),1),x:2,y:(events.push('second'),3)})".to_owned(),
          ),
        ]
        .into_iter()
        .collect(),
      );
      options.minify = Some(RawMinifyOptions::Bool(minify));
      let mut bundler = Bundler::new(options)?;
      fixture.output(&bundler.generate().await?)?;
      bundler.close().await?;
      assert_eq!(fixture.execute("./dist/main.js")?, expected, "parent={parent}, minify={minify}");
    }
  }
  Ok(())
}

#[tokio::test]
async fn defines_preserve_throwing_duplicate_property_evaluations() -> Result<()> {
  let fixture = Fixture::new(&[(
    "main.js",
    "try{const value=globalThis.CONFIG;events.push(value)}catch(error){events.push(error.message)}",
  )])?;
  for minify in [false, true] {
    let mut options = fixture.options();
    options.define = Some(
      [
        (
          "globalThis.CONFIG".to_owned(),
          "({x:(()=>{events.push('before');throw Error('parent')})(),x:2})".to_owned(),
        ),
        ("globalThis.CONFIG.MODE".to_owned(), "'added'".to_owned()),
      ]
      .into_iter()
      .collect(),
    );
    options.minify = Some(RawMinifyOptions::Bool(minify));
    let mut bundler = Bundler::new(options)?;
    fixture.output(&bundler.generate().await?)?;
    bundler.close().await?;
    assert_eq!(
      fixture.execute("./dist/main.js")?,
      serde_json::json!(["before", "parent"]),
      "minify={minify}"
    );
  }
  Ok(())
}

#[tokio::test]
async fn defines_match_cooked_template_keys_without_decoding_literal_backslashes() -> Result<()> {
  let fixture = Fixture::new(&[(
    "main.js",
    r#"events.push(
      process.env.MODE,
      process["env"]["MODE"],
      process[`env`][`MODE`],
      process[`e\u006ev`].MODE,
      process.env[`M\u004fDE`],
      process[`e\u{6e}v`][`M\u004fDE`],
      process[`e\\u006ev`].MODE,
      process[`${globalThis.envKey}`].MODE,
      process[`\uD800`].MODE
    );"#,
  )])?;
  fixture.write(
    "run.js",
    r#"process.env.MODE = "runtime";
       process["e\\u006ev"] = {MODE: "raw"};
       process["\uD800"] = {MODE: "surrogate"};
       globalThis.envKey = "env";
       await import("./dist/main.js");"#,
  )?;
  for (key, value) in
    [("process.env", r#"({"MODE":"configured"})"#), ("process.env.MODE", r#""configured""#)]
  {
    for minify in [false, true] {
      let mut options = fixture.options();
      options.define = Some(std::iter::once((key.to_owned(), value.to_owned())).collect());
      options.minify = Some(RawMinifyOptions::Bool(minify));
      let mut bundler = Bundler::new(options)?;
      fixture.output(&bundler.generate().await?)?;
      bundler.close().await?;
      assert_eq!(
        fixture.execute("./run.js")?,
        serde_json::json!([
          "configured",
          "configured",
          "configured",
          "configured",
          "configured",
          "configured",
          "raw",
          "runtime",
          "surrogate"
        ]),
        "define={key}, minify={minify}"
      );
    }
  }
  Ok(())
}

#[tokio::test]
async fn minify_keep_names_preserves_iife_and_inferred_property_stack_frames() -> Result<()> {
  let fixture = Fixture::new(&[(
    "main.js",
    r"events.push((function descriptiveIife(){return new Error().stack;})().split('\n')[1]);
      events.push(({descriptiveProperty:function(){return new Error().stack;}})
        .descriptiveProperty().split('\n')[1]);
      events.push(({descriptiveArrow:()=>new Error().stack})
        ['descriptiveArrow']().split('\n')[1]);",
  )])?;
  let names = ["descriptiveIife", "descriptiveProperty", "descriptiveArrow"];
  let reference = fixture.execute("./main.js")?;
  for (frame, name) in reference.as_array().context("reference stack frames")?.iter().zip(names) {
    ensure!(frame.as_str().context("stack frame")?.contains(name), "reference frame: {frame}");
  }
  for detailed in [false, true] {
    for keep_names in [true, false] {
      let mut options = fixture.options();
      options.keep_names = Some(keep_names);
      options.minify = Some(if detailed {
        RawMinifyOptions::Object(RawMinifyOptionsDetailed {
          mangle: Some(RawMangleOptions::default()),
          mangle_properties: None,
          compress: Some(RawCompressOptions::default()),
          remove_whitespace: true,
          ascii_only: false,
        })
      } else {
        RawMinifyOptions::Bool(true)
      });
      let mut bundler = Bundler::new(options)?;
      fixture.output(&bundler.generate().await?)?;
      bundler.close().await?;
      let observed = fixture.execute("./dist/main.js")?;
      let frames = observed.as_array().context("minified stack frames")?;
      ensure!(frames.len() == names.len(), "all stack observations must survive: {observed}");
      for (frame, name) in frames.iter().zip(names) {
        assert_eq!(
          frame.as_str().context("stack frame")?.contains(name),
          keep_names,
          "detailed={detailed}, keep_names={keep_names}, expected={name}, frame={frame}"
        );
      }
    }
  }
  Ok(())
}

#[tokio::test]
async fn closed_plain_objects_prune_unused_properties_but_not_escaped_shapes() -> Result<()> {
  let fixture = Fixture::new(&[
    ("tokens.js", "export const tokens={live:'LIVE',dead:'UNUSED_TOKEN'};"),
    (
      "view.js",
      "import{tokens}from'./tokens.js';\
      export const live=/*#__PURE__*/JSON.stringify({color:tokens.live});\
      export const dead=/*#__PURE__*/JSON.stringify({color:tokens.dead});",
    ),
    ("main.js", "import{live}from'./view.js';events.push(live);"),
  ])?;
  let mut bundler = Bundler::new(fixture.options())?;
  let output = bundler.generate().await?;
  ensure!(
    !output
      .assets
      .iter()
      .any(|asset| { String::from_utf8_lossy(asset.content_as_bytes()).contains("UNUSED_TOKEN") }),
    "unused literal properties should be pruned with the dead pure call"
  );
  fixture.output(&output)?;
  bundler.close().await?;
  assert_eq!(fixture.execute("./dist/main.js")?, serde_json::json!(["{\"color\":\"LIVE\"}"]));
  fixture.write(
    "main.js",
    "import{tokens}from'./tokens.js';events.push(Object.keys(tokens),tokens.dead);",
  )?;
  let mut bundler = Bundler::new(fixture.options())?;
  fixture.output(&bundler.generate().await?)?;
  bundler.close().await?;
  assert_eq!(
    fixture.execute("./dist/main.js")?,
    serde_json::json!([["live", "dead"], "UNUSED_TOKEN"])
  );
  Ok(())
}

#[tokio::test]
async fn bundled_lexical_cells_preserve_local_tdz_and_const_assignment_errors() -> Result<()> {
  let fixture = Fixture::new(&[(
    "main.js",
    "try{events.push(x)}catch(e){events.push(e.name)}\
     export const x=1;events.push(x);\
     try{x=2}catch(e){events.push(e.name)}events.push(x);",
  )])?;
  let mut options = fixture.options();
  options.strict_execution_order = Some(true);
  options.experimental = Some(ExperimentalOptions {
    resolve_new_url_to_asset: Some(true),
    on_demand_wrapping: Some(true),
    ..Default::default()
  });
  let mut bundler = Bundler::new(options)?;
  fixture.output(&bundler.generate().await?)?;
  bundler.close().await?;
  assert_eq!(
    fixture.execute("./dist/main.js")?,
    serde_json::json!(["ReferenceError", 1, "TypeError", 1])
  );
  Ok(())
}

#[tokio::test]
async fn final_output_preserves_annotation_options_independently_of_whitespace() -> Result<()> {
  let fixture =
    Fixture::new(&[("main.js", "export const value=/* @__PURE__ */unknownFactory();")])?;
  for remove_whitespace in [false, true] {
    for annotation in [None, Some(true), Some(false)] {
      let mut options = fixture.options();
      options.comments =
        annotation.map(|annotation| CommentsOptions { annotation, ..Default::default() });
      options.minify = Some(RawMinifyOptions::Object(RawMinifyOptionsDetailed {
        mangle: None,
        mangle_properties: None,
        compress: Some(RawCompressOptions::default()),
        remove_whitespace,
        ascii_only: false,
      }));
      let mut bundler = Bundler::new(options)?;
      let output = bundler.generate().await?;
      bundler.close().await?;
      let code = output
        .assets
        .iter()
        .filter_map(|asset| {
          let Output::Chunk(chunk) = asset else { return None };
          Some(chunk.code.as_str())
        })
        .collect::<Vec<_>>()
        .join("\n");
      ensure!(code.contains("unknownFactory"), "the exported initializer must remain: {code}");
      ensure!(
        code.contains("@__PURE__") == annotation.unwrap_or(true),
        "annotation={annotation:?}, remove_whitespace={remove_whitespace}:\n{code}"
      );
    }
  }
  Ok(())
}

#[tokio::test]
async fn property_mangling_override_ends_after_scan_only_operations() -> Result<()> {
  let fixture = Fixture::new(&[])?;
  let mut options = fixture.options();
  options.minify = Some(RawMinifyOptions::Bool(true));
  options.experimental =
    Some(ExperimentalOptions { incremental_build: Some(true), ..Default::default() });
  let mut bundler = Bundler::new(options)?;
  let source = "globalThis.value={shared_:7};events.push(globalThis.value.shared_);";
  for fail_scan in [true, false] {
    fixture.write("main.js", if fail_scan { "import './missing.js';" } else { source })?;
    let result = bundler
      .with_cached_bundle_experimental(BundleMode::IncrementalFullBuild, async |bundle| {
        bundle.set_mangle_properties(Some(
          oxc::minifier::ManglePropertiesOptions::from_pattern("_$").expect("valid pattern"),
        ));
        bundle.scan_modules(ScanMode::Full).await?;
        Ok(())
      })
      .await;
    ensure!(result.is_err() == fail_scan, "scan outcome: {result:?}");
    fixture.write("main.js", source)?;
    let output = bundler.incremental_generate(ScanMode::Full).await?;
    ensure!(
      output
        .assets
        .iter()
        .any(|asset| { String::from_utf8_lossy(asset.content_as_bytes()).contains("shared_") }),
      "a completed scan-only operation must not leave a property-mangling override"
    );
    fixture.output(&output)?;
    assert_eq!(fixture.execute("./dist/main.js")?, serde_json::json!([7]));
  }
  bundler.close().await?;
  Ok(())
}

#[derive(Debug)]
struct LateChunk(Arc<AtomicBool>);

impl Plugin for LateChunk {
  fn name(&self) -> Cow<'static, str> {
    "late-chunk".into()
  }

  fn register_hook_usage(&self) -> HookUsage {
    HookUsage::GenerateBundle
  }

  async fn generate_bundle(
    &self,
    _ctx: &PluginContext,
    args: &mut HookGenerateBundleArgs<'_>,
  ) -> HookNoopReturn {
    if self.0.load(Ordering::SeqCst) {
      let chunk = args
        .bundle
        .iter()
        .find_map(|output| {
          let Output::Chunk(chunk) = output else { return None };
          let mut chunk = (**chunk).clone();
          chunk.filename = "late.js".into();
          chunk.code = "events.push({shared_:9}.shared_);".into();
          Some(Output::Chunk(Arc::new(chunk)))
        })
        .context("at least one generated chunk")?;
      args.bundle.push(chunk);
    }
    Ok(())
  }
}

#[tokio::test]
async fn property_mangling_is_global_build_scoped_and_rejects_late_chunks() -> Result<()> {
  let fixture = Fixture::new(&[
    ("main.js", "import{value}from'./owner.js';events.push(value.shared_,value['occupied_']);"),
    ("other.js", "import{value}from'./owner.js';events.push(value.shared_,value.occupied_);"),
    ("owner.js", "export const value={shared_:7,occupied_:8};"),
  ])?;
  let mut options = fixture.options();
  options
    .input
    .as_mut()
    .expect("entry list")
    .push(InputItem { name: Some("other".into()), import: "./other.js".into() });
  options.minify = Some(RawMinifyOptions::Bool(true));
  options.experimental =
    Some(ExperimentalOptions { incremental_build: Some(true), ..Default::default() });
  let late = Arc::new(AtomicBool::new(false));
  let mut bundler =
    Bundler::with_plugins(options, vec![Plugin::new_shared(LateChunk(Arc::clone(&late)))])?;
  for (pattern, mangled) in [(Some("_$"), true), (None, false), (Some("^never$"), false)] {
    let mangle = pattern
      .map(oxc::minifier::ManglePropertiesOptions::from_pattern)
      .transpose()
      .map_err(anyhow::Error::msg)?;
    let output = bundler
      .with_cached_bundle_experimental(BundleMode::IncrementalFullBuild, async |bundle| {
        if mangle.is_some() {
          bundle.set_mangle_properties(mangle);
        }
        let scanned = bundle.scan_modules(ScanMode::Full).await?;
        bundle.bundle_generate(scanned).await
      })
      .await?;
    let combined = output
      .assets
      .iter()
      .map(|asset| String::from_utf8_lossy(asset.content_as_bytes()))
      .collect::<Vec<_>>()
      .join("\n");
    ensure!(combined.contains("occupied_"), "a globally quoted property must be reserved");
    ensure!(
      combined.contains("shared_") != mangled,
      "override leaked or did not mangle: {combined}"
    );
    fixture.output(&output)?;
    for entry in ["main", "other"] {
      assert_eq!(
        fixture.execute(&format!("./dist/{entry}.js"))?,
        serde_json::json!([7, 8]),
        "entry={entry}, pattern={pattern:?}:\n{combined}"
      );
    }
  }
  late.store(true, Ordering::SeqCst);
  let rejected = bundler
    .with_cached_bundle_experimental(BundleMode::IncrementalFullBuild, async |bundle| {
      bundle.set_mangle_properties(Some(
        oxc::minifier::ManglePropertiesOptions::from_pattern("_$").expect("valid pattern"),
      ));
      let scanned = bundle.scan_modules(ScanMode::Full).await?;
      bundle.bundle_generate(scanned).await
    })
    .await;
  ensure!(rejected.is_err(), "late JavaScript chunks must fail closed");
  late.store(false, Ordering::SeqCst);
  fixture.output(&bundler.incremental_generate(ScanMode::Full).await?)?;
  assert_eq!(fixture.execute("./dist/main.js")?, serde_json::json!([7, 8]));
  bundler.close().await?;
  Ok(())
}

impl Plugin for LiteralCell {
  fn name(&self) -> Cow<'static, str> {
    "literal-cell".into()
  }

  fn register_hook_usage(&self) -> HookUsage {
    HookUsage::ResolveId | HookUsage::Load
  }

  async fn resolve_id(
    &self,
    _ctx: &PluginContext,
    args: &HookResolveIdArgs<'_>,
  ) -> HookResolveIdReturn {
    Ok(
      (args.specifier == "\0native-cell:01")
        .then(|| HookResolveIdOutput::from_id("\0native-cell:01")),
    )
  }

  async fn load(&self, _ctx: SharedLoadPluginContext, args: &HookLoadArgs<'_>) -> HookLoadReturn {
    Ok((args.id == "\0native-cell:01").then(|| HookLoadOutput {
      code: "export const x=1;export function mark(){globalThis.value=0}".into(),
      ..Default::default()
    }))
  }
}
