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
  BundleMode, ExperimentalOptions, InlineConstOption, InputItem, ModuleDefFormat,
  OptimizationOption, Output, OutputFormat, RawMinifyOptions, ScanMode,
};
use rolldown_plugin::{
  HookGenerateBundleArgs, HookLoadArgs, HookLoadOutput, HookLoadReturn, HookNoopReturn,
  HookResolveIdArgs, HookResolveIdOutput, HookResolveIdReturn, HookTransformArgs,
  HookTransformReturn, HookUsage, Plugin, PluginContext, SharedLoadPluginContext,
  SharedTransformPluginContext,
};
use tokio::{sync::Notify, task::JoinHandle};

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

type Waiters = Arc<Mutex<Vec<JoinHandle<Result<()>>>>>;

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
