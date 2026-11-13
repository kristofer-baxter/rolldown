use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Arc, atomic::AtomicU32};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use rolldown::{BundleOutput, Bundler, BundlerOptions};
use rolldown_common::{
  ChunkOptimizationOption, ClientHmrInput, CodeSplittingMode, DevModeOptions, ExperimentalOptions,
  HmrStampTable, HmrUpdate, InputItem, ManualCodeSplittingOptions, MatchGroup, MatchGroupName,
  MatchGroupTest, ModuleType, OutputFormat, Platform, RawMinifyOptions, ScanMode, SourceMapType,
  TreeshakeOptions, WatcherChangeKind,
};
use rolldown_utils::indexmap::FxIndexMap;
use rustc_hash::FxHashSet;

use super::{Fixture, partial_scan};

fn options(fixture: &Fixture, native: bool, wrapped: bool) -> BundlerOptions {
  let mut options = fixture.options();
  options.preserve_modules = Some(native);
  options.strict_execution_order = Some(wrapped);
  options.minify = Some(RawMinifyOptions::Bool(false));
  options.treeshake = TreeshakeOptions::Boolean(false);
  options.experimental = Some(ExperimentalOptions {
    resolve_new_url_to_asset: Some(true),
    incremental_build: Some(true),
    on_demand_wrapping: Some(false),
    dev_mode: Some(DevModeOptions {
      lazy: Some(false),
      implement: Some(
        "globalThis.__rolldown_runtime__ ??= new DevRuntime('compiler-fixture');\
         globalThis.__rolldown_runtime__.hooks = {\
           createModuleHotContext: () => ({ accept() {} }),\
           onModuleCacheRemoval() {}\
         };"
          .into(),
      ),
      ..Default::default()
    }),
    ..Default::default()
  });
  options
}

fn probe(fixture: &Fixture, code: &str) -> Result<serde_json::Value> {
  fixture.write(
    "probe.mjs",
    &format!(
      "import assert from 'node:assert/strict';\
       await import('./dist/main.js');\
       const runtime=globalThis.__rolldown_runtime__;\
       const record=(name)=>{{\
         const entry=[...runtime.moduleCache].find(([id])=>id.endsWith(name));\
         assert.ok(entry, `missing record ${{name}}`);\
         return entry[1].exportsHolder;\
       }};{code}"
    ),
  )?;
  fixture.execute("./probe.mjs")
}

#[tokio::test]
async fn hmr_linked_source_kind_preserves_top_level_this_and_namespace_shape() -> Result<()> {
  for native in [true, false] {
    for (kind, context) in [
      ("linked-commonjs", ""),
      ("scanned-commonjs", ""),
      ("linked-esm", ""),
      ("scanned-esm", ""),
      ("linked-commonjs", "globalThis"),
      ("linked-esm", "globalThis"),
    ] {
      let commonjs = kind.ends_with("commonjs");
      let source = |value| {
        let syntax = match kind {
          "scanned-commonjs" => "module.exports=exports;",
          "scanned-esm" => "export{};",
          _ => "",
        };
        format!(
          "globalThis.depRuns=(globalThis.depRuns??0)+{value};\
           if(this){{this.marker={value};globalThis.thisSeen=true}}\
           globalThis.arrowIsThis=(()=>this)()===this;\
           globalThis.functionReceiver=({{value:17,read(){{return this.value}}}}).read();\
           globalThis.classReceiver=(class{{static value=23;static read(){{return this.value}}}}).read();\
           {syntax}"
        )
      };
      let importee = if kind == "linked-commonjs" { "driver" } else { "dep" };
      let fixture = Fixture::new(&[
        (
          "main.js",
          &format!(
            "import './legacy/{importee}.js';\
             globalThis.load=()=>import('./legacy/dep.js');import.meta.hot.accept();"
          ),
        ),
        ("legacy/package.json", "{}"),
        ("legacy/dep.js", &source(1)),
        ("legacy/driver.js", "require('./dep.js');module.exports={};"),
      ])?;
      fixture.write(
        "reference.mjs",
        &if commonjs {
          "const dep=(await import('node:module')).createRequire(import.meta.url)('./legacy/dep.js');\
           events.push(depRuns,thisSeen,dep.marker,arrowIsThis,functionReceiver,classReceiver);"
            .to_owned()
        } else {
          fixture.write("reference-esm.mjs", &format!("{}export{{}};", source(1)))?;
          "await import('./reference-esm.mjs');\
           events.push(depRuns,globalThis.thisSeen??null,null,arrowIsThis,functionReceiver,classReceiver);"
            .to_owned()
        },
      )?;
      assert_eq!(
        fixture.execute("./reference.mjs")?,
        serde_json::json!([
          1,
          if commonjs { Some(true) } else { None },
          if commonjs { Some(1) } else { None },
          true,
          17,
          23
        ]),
        "Node source-kind reference: {kind}"
      );
      let mut config = options(&fixture, native, !native);
      config.context = Some(context.into());
      let mut bundler = Bundler::new(config)?;
      fixture.output(&bundler.incremental_generate(ScanMode::Full).await?)?;
      let mut stamps = HmrStampTable::default();
      let (evaluated, ..) = bundler.compute_top_level_evaluated_modules(&stamps);
      let mut shipped = evaluated.clone();
      let next = Arc::new(AtomicU32::new(0));
      for value in [10, 100] {
        fixture.write("legacy/dep.js", &source(value))?;
        let changes = FxIndexMap::from_iter([(
          fixture.0.join("legacy/dep.js").to_string_lossy().into_owned(),
          WatcherChangeKind::Update,
        )]);
        let updates = bundler
          .compute_hmr_update_for_file_changes(
            &changes,
            &[ClientHmrInput {
              client_id: "fixture",
              shipped: &shipped,
              top_level_evaluated: &evaluated,
            }],
            &mut stamps,
            Arc::clone(&next),
            false,
            false,
          )
          .await?;
        ensure!(updates.len() == 1, "one client must receive one update");
        let HmrUpdate::Patch(patch) = &updates[0].update else {
          anyhow::bail!("expected a real factory patch: {:?}", updates[0].update)
        };
        fixture.write(&format!("patch-{value}.mjs"), &patch.code)?;
        shipped.extend(patch.carried.iter().cloned());
      }
      let observed = probe(
        &fixture,
        &format!(
          r"
          const commonjs={commonjs};
          const customContext={custom_context};
          const [id]=[...runtime.moduleCache].find(([id])=>id.endsWith('legacy/dep.js'));
          const shape=ns=>[Object.prototype.toString.call(ns),Object.getPrototypeOf(ns)===null,
            Object.isExtensible(ns),Object.keys(ns).sort()];
          const initial=shape(await globalThis.load());
          assert.deepEqual(initial,commonjs
            ? ['[object Object]',false,true,['default','marker']]
            : ['[object Module]',true,false,[]]);
          const check=(value,total)=>{{
            const raw=runtime.loadExports(id);
            const ns=commonjs?runtime.__toESM(raw,true):raw;
            assert.deepEqual(shape(ns),initial,'initial/patch namespace parity');
            assert.equal(globalThis.depRuns,total);
            assert.equal(globalThis.thisSeen,commonjs||customContext?true:undefined);
            assert.equal(commonjs?ns.default.marker:undefined,commonjs?value:undefined);
            assert.equal(globalThis.marker,!commonjs&&customContext?value:undefined);
            assert.equal(globalThis.arrowIsThis,true);
            assert.equal(globalThis.functionReceiver,17);
            assert.equal(globalThis.classReceiver,23);
          }};
          await check(1,1);
          for(const [value,total] of [[10,11],[100,111]]){{
            await import(`./patch-${{value}}.mjs`);
            // Exercise the generated compiler records without a consumer HMR transaction engine.
            runtime.removeModuleCache(id);
            runtime.initModule(id);
            await check(value,total);
          }}
          events.push('initial and two patched generations agree');
          ",
          custom_context = !context.is_empty()
        ),
      )?;
      assert_eq!(
        observed,
        serde_json::json!(["initial and two patched generations agree"]),
        "native={native}, kind={kind}, context={context:?}"
      );
      bundler.close().await?;
    }
  }
  Ok(())
}

#[tokio::test]
async fn bundled_hmr_refreshes_wide_import_observation_after_relink() -> Result<()> {
  let fixture = Fixture::new(&[])?;
  let mut imports = String::new();
  for index in 0..64 {
    write!(imports, "import {{value as value{index}}} from './leaf{index}.js';")?;
    fixture.write(
      &format!("leaf{index}.js"),
      &format!("globalThis.leafRuns=(globalThis.leafRuns??0)+1;export const value={index};"),
    )?;
  }
  let main =
    |selected| format!("{imports}globalThis.selected=value{selected};import.meta.hot.accept();");
  fixture.write("main.js", &main(0))?;
  let mut bundler = Bundler::new(options(&fixture, false, true))?;
  for (generation, selected) in [0, 63, 0].into_iter().enumerate() {
    let mode = if generation == 0 {
      ScanMode::Full
    } else {
      fixture.write("main.js", &main(selected))?;
      partial_scan(&fixture, "main.js")
    };
    fixture.output(&bundler.incremental_generate(mode).await?)?;
    let observed = probe(
      &fixture,
      r"
      assert.equal(globalThis.leafRuns,64);
      events.push(globalThis.selected);
      for(let index=0;index<64;index++){
        const holder=record(`leaf${index}.js`);
        assert.equal(holder.bindings===true,index===globalThis.selected,`leaf${index} ownership`);
        assert.equal(holder.temporal,undefined,'wrapped records use evaluator completion');
        assert.equal(typeof holder.evaluate,'function');
      }
      ",
    )?;
    ensure!(observed == serde_json::json!([selected]), "generation {generation}: {observed}");
  }
  bundler.close().await?;
  Ok(())
}

#[tokio::test]
async fn bundled_hmr_keeps_escaped_readers_writable_across_factory_generations() -> Result<()> {
  for native in [false, true] {
    for kind in ["let", "const", "function", "self-import"] {
      let source = |value| {
        let declaration = match kind {
          "function" => format!("export function value(){{return {value}}}"),
          "self-import" => {
            format!("import {{value as selfValue}} from './value.js';export let value={value};")
          }
          _ => format!("export {kind} value={value};"),
        };
        let read = if kind == "self-import" { "selfValue" } else { "value" };
        format!(
          "{declaration}globalThis.savedRead??=()=>typeof {read}==='function'?{read}():{read};\
           import.meta.hot.accept();"
        )
      };
      let fixture = Fixture::new(&[
        ("main.js", "import './value.js';import.meta.hot.accept('./value.js',()=>{});"),
        ("value.js", &source(1)),
      ])?;
      let mut bundler = Bundler::new(options(&fixture, native, !native))?;
      fixture.output(&bundler.incremental_generate(ScanMode::Full).await?)?;
      let mut stamps = HmrStampTable::default();
      let (evaluated, ..) = bundler.compute_top_level_evaluated_modules(&stamps);
      let mut shipped = evaluated.clone();
      let next = Arc::new(AtomicU32::new(0));
      for value in [2, 3] {
        fixture.write("value.js", &source(value))?;
        let changes = FxIndexMap::from_iter([(
          fixture.0.join("value.js").to_string_lossy().into_owned(),
          WatcherChangeKind::Update,
        )]);
        let updates = bundler
          .compute_hmr_update_for_file_changes(
            &changes,
            &[ClientHmrInput {
              client_id: "fixture",
              shipped: &shipped,
              top_level_evaluated: &evaluated,
            }],
            &mut stamps,
            Arc::clone(&next),
            false,
            false,
          )
          .await?;
        ensure!(updates.len() == 1, "one client must receive one update");
        let HmrUpdate::Patch(patch) = &updates[0].update else {
          anyhow::bail!("expected a real factory patch: {:?}", updates[0].update)
        };
        fixture.write(&format!("patch-{value}.mjs"), &patch.code)?;
        shipped.extend(patch.carried.iter().cloned());
      }
      let observed = probe(
        &fixture,
        r"
        const [id]=[...runtime.moduleCache].find(([id])=>id.endsWith('value.js'));
        const initial=record('value.js');
        assert.equal(initial.native===true||initial.bindings===true,true);
        assert.equal(typeof initial.cells.value[1],'function','escaped reader needs a writable cell');
        events.push(globalThis.savedRead());
        for(const value of [2,3]){
          await import(`./patch-${value}.mjs`);
          runtime.removeModuleCache(id);
          runtime.initModule(id);
          const fresh=record('value.js');
          // Exercise the compiler's cell ABI, not a consumer-specific HMR transaction engine.
          initial.cells.value[1](fresh.exports.value);
          events.push(globalThis.savedRead());
          const current=fresh.exports.value;
          events.push(typeof current==='function'?current():current);
        }
        ",
      )?;
      ensure!(
        observed == serde_json::json!([1, 2, 2, 3, 3]),
        "native={native}, declaration={kind}: {observed}"
      );
      bundler.close().await?;
    }
  }
  Ok(())
}

#[tokio::test]
async fn bundled_hmr_unwrapped_temporal_flags_survive_pending_and_failed_evaluation() -> Result<()>
{
  for reject in [false, true] {
    let fixture = Fixture::new(&[
      ("main.js", "globalThis.mainRan=true;"),
      ("slow-entry.js", "import './slow.js';"),
      ("slow.js", "await globalThis.pending;export let value=1;"),
    ])?;
    let mut config = options(&fixture, false, false);
    config
      .input
      .as_mut()
      .expect("entry list")
      .push(InputItem { name: Some("slow-entry".into()), import: "./slow-entry.js".into() });
    let mut bundler = Bundler::new(config)?;
    fixture.output(&bundler.incremental_generate(ScanMode::Full).await?)?;
    let observed = probe(
      &fixture,
      &format!(
        r"
        let resolve,reject;
        globalThis.pending=new Promise((yes,no)=>{{resolve=yes;reject=no}});
        const loading=import('./dist/slow-entry.js');
        let settled=false;
        void loading.then(()=>{{settled=true}},()=>{{settled=true}});
        while(![...runtime.moduleCache.keys()].some(id=>id.endsWith('slow.js'))){{
          assert.equal(settled,false,'slow must suspend');
          await new Promise(resolve=>setImmediate(resolve));
        }}
        const holder=record('slow.js');
        assert.equal(holder.evaluate,undefined,'test must exercise unwrapped readiness');
        assert.equal(holder.bindings,undefined,'readiness must not depend on binding ownership');
        assert.equal(holder.cells,undefined);
        assert.ok(holder.temporal,'unwrapped records need independent temporal flags');
        assert.equal(Object.values(holder.temporal).every(ready=>ready()),false);
        if({reject}){{
          reject(Error('slow failed'));
          await assert.rejects(loading,/slow failed/);
          assert.equal(Object.values(holder.temporal).every(ready=>ready()),false);
          events.push('failed');
        }}else{{
          resolve();
          await loading;
          assert.equal(holder.exports.value,1);
          assert.equal(Object.values(holder.temporal).every(ready=>ready()),true);
          events.push('ready');
        }}
        "
      ),
    )?;
    let expected = if reject { "failed" } else { "ready" };
    ensure!(observed == serde_json::json!([expected]), "unwrapped readiness: {observed}");
    bundler.close().await?;
  }
  Ok(())
}

#[tokio::test]
async fn bundled_hmr_lazy_factories_read_latest_used_and_unobserved_exports() -> Result<()> {
  let fixture = Fixture::new(&[
    (
      "main.js",
      "import {value as used} from './shared.js';\
       import {value as unused} from './unobserved.js';\
       globalThis.savedRead=()=>used;\
       globalThis.openRoute=()=>import('./route.js');\
       import.meta.hot.accept(['./shared.js','./unobserved.js'],()=>{});",
    ),
    ("shared.js", "export let value=1;"),
    ("unobserved.js", "export let value=10;"),
    (
      "route.js",
      "import {value as used} from './shared.js';\
       import {value as unused} from './unobserved.js';\
       globalThis.routeRuns=(globalThis.routeRuns??0)+1;\
       export const values=[used,unused];",
    ),
  ])?;
  let mut config = options(&fixture, false, true);
  config
    .experimental
    .as_mut()
    .expect("experimental options")
    .dev_mode
    .as_mut()
    .expect("development mode")
    .lazy = Some(true);
  let mut bundler = Bundler::new(config)?;
  fixture.output(&bundler.incremental_generate(ScanMode::Full).await?)?;
  let mut stamps = HmrStampTable::default();
  let (evaluated, ..) = bundler.compute_top_level_evaluated_modules(&stamps);
  let mut shipped = evaluated.clone();
  let next = Arc::new(AtomicU32::new(0));
  for value in [2, 3] {
    fixture.write("shared.js", &format!("export let value={value};"))?;
    fixture.write("unobserved.js", &format!("export let value={};", value * 10))?;
    let changes = ["shared.js", "unobserved.js"]
      .into_iter()
      .map(|name| (fixture.0.join(name).to_string_lossy().into_owned(), WatcherChangeKind::Update))
      .collect();
    let updates = bundler
      .compute_hmr_update_for_file_changes(
        &changes,
        &[ClientHmrInput {
          client_id: "fixture",
          shipped: &shipped,
          top_level_evaluated: &evaluated,
        }],
        &mut stamps,
        Arc::clone(&next),
        false,
        false,
      )
      .await?;
    ensure!(updates.len() == 1, "one client must receive one update");
    let HmrUpdate::Patch(patch) = &updates[0].update else {
      anyhow::bail!("expected a real factory patch: {:?}", updates[0].update)
    };
    fixture.write(&format!("patch-{value}.mjs"), &patch.code)?;
    shipped.extend(patch.carried.iter().cloned());
  }
  let proxy = format!("{}?rolldown-lazy=1", fixture.0.join("route.js").display());
  bundler
    .lazy_compilation_context
    .as_ref()
    .expect("lazy compilation context")
    .mark_as_fetched(&proxy);
  let lazy = bundler
    .compile_lazy_entry(
      &proxy,
      "fixture",
      &shipped,
      &FxHashSet::default(),
      &evaluated,
      Some(&evaluated),
      None,
      None,
      None,
      &stamps,
      Arc::clone(&next),
    )
    .await?;
  ensure!(
    lazy.carried.iter().any(|(id, _)| id.ends_with("route.js")),
    "lazy payload must carry the actual route factory"
  );
  ensure!(
    lazy.carried.iter().all(|(id, _)| !id.ends_with("shared.js") && !id.ends_with("unobserved.js")),
    "current startup donors must not be re-shipped with the lazy route"
  );
  fixture.write("lazy.mjs", &lazy.code)?;
  let observed = probe(
    &fixture,
    r"
    const shared=record('shared.js');
    assert.equal(shared.bindings,true);
    assert.equal(record('unobserved.js').bindings,undefined);
    assert.equal(record('unobserved.js').cells,undefined);
    assert.equal(globalThis.routeRuns,undefined);
    for(const value of [2,3]){
      await import(`./patch-${value}.mjs`);
      for(const name of ['shared.js','unobserved.js']){
        const [id]=[...runtime.moduleCache].find(([id])=>id.endsWith(name));
        runtime.removeModuleCache(id);
        runtime.initModule(id);
      }
      shared.cells.value[1](record('shared.js').exports.value);
      events.push(globalThis.savedRead());
    }
    runtime.loadLazyChunk=()=>import('./lazy.mjs');
    const routeNs=await globalThis.openRoute();
    assert.equal(globalThis.routeRuns,1,'lazy code executes only on activation');
    const route=record('route.js');
    assert.deepEqual(routeNs.values,route.exports.values);
    events.push(route.exports.values);
    ",
  )?;
  ensure!(observed == serde_json::json!([2, 3, [3, 30]]), "lazy route state: {observed}");
  bundler.close().await?;
  Ok(())
}

#[tokio::test]
async fn bundled_hmr_omission_preserves_written_and_readonly_constant_cells() -> Result<()> {
  for (source, assertion) in [
    ("export let value=1;value=2;", "assert.equal(holder.cells.value,0);"),
    (
      "export const {value,other}={value:1,other:2};if(false)other=3;",
      "assert.equal(holder.cells.value.length,1);assert.equal(holder.cells.other.length,1);",
    ),
  ] {
    let fixture = Fixture::new(&[
      ("main.js", "import './value.js';import.meta.hot.accept('./value.js',()=>{});"),
      ("value.js", source),
    ])?;
    let mut bundler = Bundler::new(options(&fixture, false, true))?;
    fixture.output(&bundler.incremental_generate(ScanMode::Full).await?)?;
    probe(
      &fixture,
      &format!("const holder=record('value.js');{assertion}events.push('guarded');"),
    )?;
    bundler.close().await?;
  }
  Ok(())
}

fn asset_map(output: &BundleOutput) -> BTreeMap<String, String> {
  output
    .assets
    .iter()
    .map(|asset| {
      (asset.filename().to_owned(), String::from_utf8_lossy(asset.content_as_bytes()).into_owned())
    })
    .collect()
}

#[tokio::test]
async fn bundled_and_native_builds_record_and_reset_core_stage_times() -> Result<()> {
  for native in [false, true] {
    let fixture = Fixture::new(&[
      (
        "main.js",
        "import {value} from './dep.js';globalThis.value=value;\
         import.meta.hot.accept('./dep.js',()=>{});",
      ),
      ("dep.js", "export const value=1;"),
      ("extra.js", "export const extra=10;"),
    ])?;
    let mut bundler = Bundler::new(options(&fixture, native, !native))?;
    if native {
      bundler.set_omit_reused_chunk_bytes(true);
    }

    let started = Instant::now();
    bundler.incremental_generate(ScanMode::Full).await?;
    let elapsed = started.elapsed();
    let (link, generate) = bundler.last_core_stage_times();
    ensure!(link > Duration::ZERO && generate > Duration::ZERO, "initial build clocks must tick");
    ensure!(link + generate <= elapsed, "initial clocks exceeded wall time");

    // Scan-only HMR patch merges without running link/generate, resetting stage clocks.
    let mut stamps = HmrStampTable::default();
    let (evaluated, ..) = bundler.compute_top_level_evaluated_modules(&stamps);
    let next = Arc::new(AtomicU32::new(0));
    fixture.write("dep.js", "export const value=2;")?;
    let changes = FxIndexMap::from_iter([(
      fixture.0.join("dep.js").to_string_lossy().into_owned(),
      WatcherChangeKind::Update,
    )]);
    bundler
      .compute_hmr_update_for_file_changes(
        &changes,
        &[ClientHmrInput {
          client_id: "fixture",
          shipped: &evaluated,
          top_level_evaluated: &evaluated,
        }],
        &mut stamps,
        Arc::clone(&next),
        false,
        false,
      )
      .await?;
    ensure!(
      bundler.last_core_stage_times() == (Duration::ZERO, Duration::ZERO),
      "scan-only HMR update must reset stage clocks"
    );

    // Relink (fast native relink when `native == true`, bundled generate when `native == false`).
    let started = Instant::now();
    bundler.incremental_generate(ScanMode::Partial(Vec::new())).await?;
    let elapsed = started.elapsed();
    if native {
      ensure!(bundler.last_fast_native_relink(), "expected fast native relink on leaf value edit");
    }
    let (link, generate) = bundler.last_core_stage_times();
    ensure!(link > Duration::ZERO && generate > Duration::ZERO, "relink clocks must tick");
    ensure!(link + generate <= elapsed, "relink clocks retained prior work");

    // Force a fast-relink rejection fallback in native mode by converting `dep.js` into CommonJS,
    // then verify fallback accumulation and clean reset on the following build.
    fixture.write("dep.js", "module.exports={value:3};")?;
    let started = Instant::now();
    bundler.incremental_generate(partial_scan(&fixture, "dep.js")).await?;
    let elapsed = started.elapsed();
    if native {
      ensure!(!bundler.last_fast_native_relink(), "CommonJS conversion must reject fast relink");
    }
    let (fallback_link, fallback_generate) = bundler.last_core_stage_times();
    ensure!(
      fallback_link > Duration::ZERO && fallback_generate > Duration::ZERO,
      "fallback build must record accumulated core link and generate durations"
    );
    ensure!(fallback_link + fallback_generate <= elapsed, "fallback clocks exceeded wall time");

    fixture.write("extra.js", "export const extra=20;")?;
    let started = Instant::now();
    bundler.incremental_generate(partial_scan(&fixture, "extra.js")).await?;
    let elapsed = started.elapsed();
    let (next_link, next_generate) = bundler.last_core_stage_times();
    ensure!(next_link > Duration::ZERO && next_generate > Duration::ZERO);
    ensure!(next_link + next_generate <= elapsed, "clocks failed to reset on subsequent build");
    bundler.close().await?;
  }
  Ok(())
}

#[tokio::test]
async fn development_mode_reuses_unchanged_finalized_modules_and_preserves_other_modes()
-> Result<()> {
  let fixture = Fixture::new(&[
    (
      "main.js",
      "import {value as a} from './a.js';import {value as b} from './b.js';\
       import {anchor} from './stable.js';events.push(a,b,anchor);import.meta.hot.accept();",
    ),
    ("a.js", "export const value=1;"),
    ("b.js", "export const value=10;"),
    ("stable.js", "const collision='cache-anchor';\nexport {collision as anchor};"),
  ])?;

  let mut bundled_opts = options(&fixture, false, true);
  bundled_opts.sourcemap = Some(SourceMapType::File);
  let mut bundled = Bundler::new(bundled_opts.clone())?;
  let initial = bundled.incremental_generate(ScanMode::Full).await?;
  ensure!(bundled.last_reused_finalized_modules() == 0, "cold build cannot report reused sources");
  fixture.output(&initial)?;
  assert_eq!(fixture.execute("./dist/main.js")?, serde_json::json!([1, 10, "cache-anchor"]));

  for (path, value, expected) in [
    ("a.js", 2, serde_json::json!([2, 10, "cache-anchor"])),
    ("a.js", 3, serde_json::json!([3, 10, "cache-anchor"])),
    ("b.js", 20, serde_json::json!([3, 20, "cache-anchor"])),
  ] {
    fixture.write(path, &format!("export const value={value};"))?;
    let cached_out = bundled.incremental_generate(partial_scan(&fixture, path)).await?;
    ensure!(
      bundled.last_reused_finalized_modules() >= 2,
      "unchanged bundled development modules must reuse finalized sources"
    );
    let mut fresh = Bundler::new(bundled_opts.clone())?;
    let fresh_out = fresh.incremental_generate(ScanMode::Full).await?;
    fresh.close().await?;
    assert_eq!(asset_map(&cached_out), asset_map(&fresh_out), "byte parity failed after {path}");
    fixture.output(&cached_out)?;
    assert_eq!(fixture.execute("./dist/main.js")?, expected);
  }
  bundled.close().await?;

  // Native records mode (`preserve_modules = true`, `strict_execution_order = false`) keeps its
  // dedicated single-module native chunk cache and never retains `CachedFinalizedModule` sources.
  let mut native = Bundler::new(options(&fixture, true, false))?;
  native.incremental_generate(ScanMode::Full).await?;
  fixture.write("a.js", "export const value=4;")?;
  native.incremental_generate(partial_scan(&fixture, "a.js")).await?;
  ensure!(
    native.last_reused_finalized_modules() == 0,
    "native records mode must not populate finalized module sources"
  );
  native.close().await?;

  // Non-development incremental mode retains digests only (`sources` stays `None`).
  fixture.write(
    "main.js",
    "import {value as a} from './a.js';import {anchor} from './stable.js';events.push(a,anchor);",
  )?;
  let mut prod_opts = fixture.options();
  prod_opts.experimental =
    Some(ExperimentalOptions { incremental_build: Some(true), ..Default::default() });
  let mut prod = Bundler::new(prod_opts)?;
  prod.incremental_generate(ScanMode::Full).await?;
  fixture.write("a.js", "export const value=5;")?;
  prod.incremental_generate(partial_scan(&fixture, "a.js")).await?;
  ensure!(
    prod.last_reused_finalized_modules() == 0,
    "non-development mode must keep finalized module sources empty"
  );
  prod.close().await?;
  Ok(())
}

#[tokio::test]
async fn development_mode_reuses_modules_across_colliding_graph_edits_and_nested_renames()
-> Result<()> {
  const MODULES: usize = 8;
  let fixture = Fixture::new(&[])?;
  let write_graph = |added: bool| -> Result<()> {
    let mut main = String::new();
    let mut values = Vec::new();
    for index in 0..MODULES {
      writeln!(main, "import {{value as value{index}}} from './module{index}.js';")?;
      values.push(format!("value{index}"));
      fixture.write(&format!("module{index}.js"), &format!("export const value={index};"))?;
    }
    fixture.write("added.js", "export const value=99;")?;
    if added {
      main.push_str("import {value as added} from './added.js';\n");
      values.push("added".into());
    }
    write!(main, "events.push({});import.meta.hot.accept();", values.join(","))?;
    fixture.write("main.js", &main)
  };
  write_graph(false)?;
  let opts = options(&fixture, false, true);
  let mut cached = Bundler::new(opts.clone())?;
  cached.incremental_generate(ScanMode::Full).await?;
  for (added, expected) in [
    (true, serde_json::json!([0, 1, 2, 3, 4, 5, 6, 7, 99])),
    (false, serde_json::json!([0, 1, 2, 3, 4, 5, 6, 7])),
  ] {
    write_graph(added)?;
    let actual = cached.incremental_generate(partial_scan(&fixture, "main.js")).await?;
    let reused = cached.last_reused_finalized_modules();
    ensure!(
      reused >= MODULES - 1,
      "added={added} reused only {reused} of {MODULES} unchanged colliding modules"
    );
    let mut fresh = Bundler::new(opts.clone())?;
    let expected_out = fresh.incremental_generate(ScanMode::Full).await?;
    fresh.close().await?;
    assert_eq!(asset_map(&actual), asset_map(&expected_out));
    fixture.output(&actual)?;
    assert_eq!(fixture.execute("./dist/main.js")?, expected);
  }
  cached.close().await?;

  // Inner-scope capture rename in an unchanged module must invalidate its cached source.
  let main_src = |blocker: bool| {
    format!(
      "import {{read}} from './reader.js';import {{value as other}} from './b.js';\
       {}events.push(read(10),other);import.meta.hot.accept();",
      if blocker { "import './blocker.js';" } else { "" }
    )
  };
  let nested_fixture = Fixture::new(&[
    ("main.js", &main_src(false)),
    ("a.js", "export const value=1;"),
    ("b.js", "export const value=2;"),
    (
      "reader.js",
      "import {value} from './a.js';export function read(input){return [value,input];}",
    ),
    ("blocker.js", ""),
  ])?;
  let nested_opts = options(&nested_fixture, false, true);
  let mut probe_bundler = Bundler::new(nested_opts.clone())?;
  let probe_assets = asset_map(&probe_bundler.incremental_generate(ScanMode::Full).await?);
  probe_bundler.close().await?;
  let imported_name = probe_assets["main.js"]
    .split(|c: char| !c.is_ascii_alphanumeric() && !matches!(c, '_' | '$'))
    .find(|name| {
      name.strip_prefix("value$").is_some_and(|suffix| {
        suffix.len() == 8 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
      })
    })
    .context("missing module-stable imported binding name")?
    .to_owned();

  nested_fixture.write(
    "reader.js",
    &format!(
      "import {{value}} from './a.js';\
       export function read({imported_name}){{return [value,{imported_name}];}}"
    ),
  )?;
  nested_fixture.write(
    "blocker.js",
    &format!("const {imported_name}$1=0;globalThis.blockerValue={imported_name}$1;"),
  )?;
  let mut session = Bundler::new(nested_opts.clone())?;
  let initial = asset_map(&session.incremental_generate(ScanMode::Full).await?);
  ensure!(initial["main.js"].contains(&format!("({imported_name}$1)")));

  nested_fixture.write("main.js", &main_src(true))?;
  let updated = session.incremental_generate(partial_scan(&nested_fixture, "main.js")).await?;
  session.close().await?;
  let mut fresh = Bundler::new(nested_opts)?;
  let expected_updated = fresh.incremental_generate(ScanMode::Full).await?;
  fresh.close().await?;
  ensure!(
    asset_map(&expected_updated)["main.js"].contains(&format!("({imported_name}$2)")),
    "the added root binding must shift the unchanged reader's inner parameter rename"
  );
  assert_eq!(asset_map(&updated), asset_map(&expected_updated));
  nested_fixture.output(&updated)?;
  assert_eq!(nested_fixture.execute("./dist/main.js")?, serde_json::json!([[1, 10], 2]));
  Ok(())
}

#[tokio::test]
async fn development_mode_cache_tracks_importer_observation_and_export_sets() -> Result<()> {
  let entry = |observes: bool| {
    let observed = if observes { "[anchor,value]" } else { "[anchor]" };
    format!(
      "import {{value}} from './source.js';import {{anchor}} from './stable.js';\
       globalThis.read=()=>{observed};import.meta.hot.accept();"
    )
  };
  let fixture = Fixture::new(&[
    ("main.js", &entry(false)),
    ("source.js", "export let value=1;"),
    ("stable.js", "export const anchor=10;"),
  ])?;
  let opts = options(&fixture, false, true);
  let mut cached = Bundler::new(opts.clone())?;
  cached.incremental_generate(ScanMode::Full).await?;
  for observes in [true, false, true] {
    fixture.write("main.js", &entry(observes))?;
    let actual = cached.incremental_generate(partial_scan(&fixture, "main.js")).await?;
    ensure!(
      cached.last_reused_finalized_modules() > 0,
      "changing importer observation must keep unrelated dependencies cached"
    );
    let mut fresh = Bundler::new(opts.clone())?;
    let expected = fresh.incremental_generate(ScanMode::Full).await?;
    fresh.close().await?;
    assert_eq!(
      asset_map(&actual),
      asset_map(&expected),
      "cached binding registration differs when observes={observes}"
    );
    fixture.output(&actual)?;
    let check = probe(
      &fixture,
      &format!(
        "const holder=record('source.js');\
         assert.equal(holder.bindings===true,{observes});\
         events.push(globalThis.read());"
      ),
    )?;
    let expected_read =
      if observes { serde_json::json!([[10, 1]]) } else { serde_json::json!([[10]]) };
    assert_eq!(check, expected_read);
  }
  cached.close().await?;

  // Export-set / re-export changes must also invalidate HMR-registered exports.
  let barrel_fixture = Fixture::new(&[
    (
      "main.js",
      "import * as values from './barrel.js';import {anchor} from './stable.js';\
       events.push(anchor,Object.entries(values));import.meta.hot.accept();",
    ),
    ("barrel.js", "export * from './source.js';"),
    ("source.js", "export const value=1;"),
    ("other.js", "export const moved=30;"),
    ("stable.js", "const collision='anchor';export {collision as anchor};"),
  ])?;
  let barrel_opts = options(&barrel_fixture, false, true);
  let mut barrel_cached = Bundler::new(barrel_opts.clone())?;
  barrel_cached.incremental_generate(ScanMode::Full).await?;
  for (path, source, expected_entries) in [
    (
      "source.js",
      "export let value=3;export const extra='added';",
      serde_json::json!([["extra", "added"], ["value", 3]]),
    ),
    ("barrel.js", "export {value as alias} from './source.js';", serde_json::json!([["alias", 3]])),
    (
      "source.js",
      "const collision=4;export {collision as value};export const extra='added';",
      serde_json::json!([["alias", 4]]),
    ),
    ("barrel.js", "export * from './other.js';", serde_json::json!([["moved", 30]])),
    (
      "barrel.js",
      "export * from './source.js';",
      serde_json::json!([["extra", "added"], ["value", 4]]),
    ),
  ] {
    barrel_fixture.write(path, source)?;
    let actual = barrel_cached.incremental_generate(partial_scan(&barrel_fixture, path)).await?;
    let mut fresh = Bundler::new(barrel_opts.clone())?;
    let expected = fresh.incremental_generate(ScanMode::Full).await?;
    fresh.close().await?;
    assert_eq!(asset_map(&actual), asset_map(&expected), "export parity failed on {path}");
    barrel_fixture.output(&actual)?;
    assert_eq!(
      barrel_fixture.execute("./dist/main.js")?,
      serde_json::json!(["anchor", expected_entries])
    );
  }
  barrel_cached.close().await?;
  Ok(())
}

#[tokio::test]
async fn development_mode_cache_tracks_new_url_assets() -> Result<()> {
  let fixture = Fixture::new(&[
    ("main.js", "export {image} from './consumer.js';export {stable} from './stable.js';"),
    ("consumer.js", "export const image=new URL('./logo.svg',import.meta.url).href;"),
    ("stable.js", "export function stable(){return 42;}"),
    ("logo.svg", "<svg>one</svg>"),
  ])?;
  let make_options = || {
    let mut opts = options(&fixture, false, false);
    opts.module_types = Some(std::iter::once(("svg".to_owned(), ModuleType::Asset)).collect());
    opts
  };
  let asset_name = |output: &BTreeMap<String, String>| -> Result<String> {
    let names = output.keys().filter(|name| name.ends_with(".svg")).collect::<Vec<_>>();
    ensure!(names.len() == 1, "expected one emitted SVG, got {names:?}");
    Ok(names[0].clone())
  };
  let mut cached = Bundler::new(make_options())?;
  let initial = asset_map(&cached.incremental_generate(ScanMode::Full).await?);
  let mut previous = asset_name(&initial)?;
  ensure!(initial["main.js"].contains(&previous));
  let mut served = initial;
  for contents in ["<svg>two</svg>", "<svg>three</svg>", "<svg>one</svg>"] {
    fixture.write("logo.svg", contents)?;
    let actual = asset_map(&cached.incremental_generate(partial_scan(&fixture, "logo.svg")).await?);
    let mut fresh = Bundler::new(make_options())?;
    let expected = asset_map(&fresh.incremental_generate(ScanMode::Full).await?);
    fresh.close().await?;
    let filename = asset_name(&expected)?;
    ensure!(filename != previous, "asset content hash did not change");
    ensure!(
      actual["main.js"].contains(&filename) && !actual["main.js"].contains(&previous),
      "cached URL consumer kept {previous} instead of {filename}"
    );
    served.extend(actual);
    for (name, bytes) in &expected {
      ensure!(served.get(name) == Some(bytes), "cached output differs from fresh build for {name}");
    }
    ensure!(
      cached.last_reused_finalized_modules() > 0,
      "unchanged modules stopped reusing their finalized sources"
    );
    previous = filename;
  }
  cached.close().await?;
  Ok(())
}

#[tokio::test]
async fn development_mode_cache_tracks_async_importer_names() -> Result<()> {
  let main = |collision: bool| {
    let name = if collision { "__asyncDependencies0" } else { "unrelatedCounter" };
    format!(
      "import {{value}} from './async.js';import {{stable}} from './stable.js';\
       let {name}=1;globalThis.collision=()=>++{name};\
       events.push(value,stable(),globalThis.collision());"
    )
  };
  let fixture = Fixture::new(&[
    ("async.js", "await Promise.resolve();export const value=7;"),
    ("stable.js", "export function stable(){return 42;}"),
    ("main.js", &main(false)),
  ])?;
  let opts = options(&fixture, false, true);
  let mut cached = Bundler::new(opts.clone())?;
  let initial = asset_map(&cached.incremental_generate(ScanMode::Full).await?);
  ensure!(initial["main.js"].contains("__asyncDependencies0"));
  for collision in [true, false, true] {
    fixture.write("main.js", &main(collision))?;
    let actual = cached.incremental_generate(partial_scan(&fixture, "main.js")).await?;
    let mut fresh = Bundler::new(opts.clone())?;
    let expected = fresh.incremental_generate(ScanMode::Full).await?;
    fresh.close().await?;
    assert_eq!(
      asset_map(&actual),
      asset_map(&expected),
      "cached async temporary differs when collision={collision}"
    );
    fixture.output(&actual)?;
    assert_eq!(fixture.execute("./dist/main.js")?, serde_json::json!([7, 42, 2]));
  }
  cached.close().await?;
  Ok(())
}

#[tokio::test]
async fn development_mode_cache_tracks_tagged_json_policy_and_import_targets() -> Result<()> {
  let importer = |attribute: bool| {
    format!(
      "import data from './proxy.js'{};export const value=data.foo;",
      if attribute { " with {type:'json'}" } else { "" }
    )
  };
  let fixture = Fixture::new(&[
    (
      "proxy.js",
      "import \"\\u0000rolldown/json-module\";const data=JSON.parse(\"{\\\"foo\\\":1}\");\
       export default data;const value0=data[\"foo\"];export{value0 as \"foo\"};",
    ),
    (
      "main.mjs",
      "import * as data from './proxy.js';import {value} from './auto.js';\
       events.push(Object.keys(data).sort(),data.default.foo,value);",
    ),
    ("auto.js", &importer(true)),
  ])?;
  // Keep `auto.js` in `javascript/auto` mode rather than strict ESM.
  fixture.write("package.json", "{}")?;
  let mut opts = options(&fixture, false, false);
  opts.input = Some(vec![InputItem { name: Some("main".into()), import: "./main.mjs".into() }]);
  let mut cached = Bundler::new(opts.clone())?;
  cached.incremental_generate(ScanMode::Full).await?;
  for (attribute, expected_keys) in [
    (false, serde_json::json!(["default", "foo"])),
    (true, serde_json::json!(["default"])),
    (false, serde_json::json!(["default", "foo"])),
  ] {
    fixture.write("auto.js", &importer(attribute))?;
    let actual = cached.incremental_generate(partial_scan(&fixture, "auto.js")).await?;
    let mut fresh = Bundler::new(opts.clone())?;
    let expected = fresh.incremental_generate(ScanMode::Full).await?;
    fresh.close().await?;
    assert_eq!(
      asset_map(&actual),
      asset_map(&expected),
      "cached tagged JSON namespace differs when attribute={attribute}"
    );
    fixture.output(&actual)?;
    assert_eq!(fixture.execute("./dist/main.js")?, serde_json::json!([expected_keys, 1, 1]));
  }
  cached.close().await?;

  for main in [
    "export const load=options=>import('./target.js',options);",
    "export const load=()=>require('./target.js');",
  ] {
    let target_src = |marker: &str| format!("{marker}const data={{foo:1}};export default data;");
    let target_fixture = Fixture::new(&[("main.mjs", main), ("target.js", &target_src(""))])?;
    target_fixture.write("package.json", "{}")?;
    let mut target_opts = options(&target_fixture, false, false);
    target_opts.input =
      Some(vec![InputItem { name: Some("main".into()), import: "./main.mjs".into() }]);
    let mut target_cached = Bundler::new(target_opts.clone())?;
    target_cached.incremental_generate(ScanMode::Full).await?;
    for marker in [
      "import \"\\u0000rolldown/json-module\";",
      "import \"\\u0000rolldown/text-module\";",
      "import \"\\u0000rolldown/raw-text-module\";",
      "",
      "import \"\\u0000rolldown/json-module\";",
    ] {
      target_fixture.write("target.js", &target_src(marker))?;
      let actual =
        target_cached.incremental_generate(partial_scan(&target_fixture, "target.js")).await?;
      let mut fresh = Bundler::new(target_opts.clone())?;
      let expected = fresh.incremental_generate(ScanMode::Full).await?;
      fresh.close().await?;
      assert_eq!(
        asset_map(&actual),
        asset_map(&expected),
        "cached import target handling differs for {main} with {marker:?}"
      );
    }
    target_cached.close().await?;
  }
  Ok(())
}

#[tokio::test]
async fn development_mode_cache_tracks_dynamic_shared_chunk_exports_and_rejects_non_esm()
-> Result<()> {
  for strict in [false, true] {
    let other_src = |both: bool| {
      if both {
        "import {aaa,bbb} from './a-shared.mjs';globalThis.value=aaa()+bbb();"
      } else {
        "import {bbb} from './a-shared.mjs';globalThis.value=bbb();"
      }
    };
    let fixture = Fixture::new(&[
      ("main.mjs", "export const load=()=>import('./z-shared.mjs');"),
      ("a-shared.mjs", "export function aaa(){return 1;}export function bbb(){return 2;}"),
      ("z-shared.mjs", "export function read(){return 7;}"),
      ("early.mjs", "import {read} from './z-shared.mjs';globalThis.early=read();"),
      ("other.mjs", other_src(true)),
    ])?;
    let make_options = || {
      let mut opts = options(&fixture, false, strict);
      opts.chunk_filenames = None;
      opts.minify_internal_exports = Some(true);
      opts.input = Some(
        ["early", "other", "main"]
          .into_iter()
          .map(|name| InputItem { name: Some(name.into()), import: format!("./{name}.mjs") })
          .collect(),
      );
      opts.code_splitting = Some(CodeSplittingMode::Advanced(ManualCodeSplittingOptions {
        groups: Some(vec![MatchGroup {
          name: MatchGroupName::Static("shared".into()),
          test: Some(MatchGroupTest::Function(Arc::new(|ids: Vec<String>| {
            Box::pin(async move { Ok(ids.iter().map(|id| id.ends_with("-shared.mjs")).collect()) })
          }))),
          min_share_count: Some(1),
          include_dependencies_recursively: Some(false),
          ..Default::default()
        }]),
        ..Default::default()
      }));
      opts
    };
    let mut cached = Bundler::new(make_options())?;
    let initial = asset_map(&cached.incremental_generate(ScanMode::Full).await?);
    ensure!(initial["main.js"].contains("./shared-"));
    for both in [false, true, false] {
      fixture.write("other.mjs", other_src(both))?;
      let actual =
        asset_map(&cached.incremental_generate(partial_scan(&fixture, "other.mjs")).await?);
      let mut fresh = Bundler::new(make_options())?;
      let expected = asset_map(&fresh.incremental_generate(ScanMode::Full).await?);
      fresh.close().await?;
      assert_eq!(
        actual, expected,
        "cached shared-chunk imports differ when strict={strict}, both={both}"
      );
      ensure!(
        cached.last_reused_finalized_modules() > 0,
        "unrelated modules stopped reusing their finalized sources"
      );
    }
    cached.close().await?;
  }

  let fixture = Fixture::new(&[("main.js", "export const value=1;")])?;
  for format in [OutputFormat::Cjs, OutputFormat::Iife, OutputFormat::Umd] {
    let mut opts = options(&fixture, false, false);
    opts.format = Some(format);
    let error = Bundler::new(opts).err().context("non-ESM development output must be rejected")?;
    let message = error.to_string();
    ensure!(
      message.contains("experimental.devMode")
        && message.contains("only supports the \"esm\" format"),
      "unexpected format admission error: {message}"
    );
  }
  Ok(())
}

#[tokio::test]
async fn development_mode_cache_tracks_simulated_facade_and_dynamic_namespaces() -> Result<()> {
  // 1. Single-chunk bundled development rebuilds across partial and full dynamic-import reads.
  let dev_fixture = Fixture::new(&[
    (
      "main.js",
      "import {stable} from './stable.js';const {a}=await import('./target.js');\
       events.push(a,globalThis.internal,stable());import.meta.hot.accept();",
    ),
    (
      "target.js",
      "export let a=10;export let b=20;\
       export function internal(){return [a,b];}globalThis.internal=internal();",
    ),
    ("stable.js", "export function stable(){return 42;}"),
  ])?;
  let mut dev_opts = options(&dev_fixture, false, true);
  dev_opts.treeshake = TreeshakeOptions::Boolean(true);
  dev_opts.code_splitting = Some(CodeSplittingMode::Bool(false));
  let mut dev_cached = Bundler::new(dev_opts.clone())?;
  let dev_initial = dev_cached.incremental_generate(ScanMode::Full).await?;
  dev_fixture.output(&dev_initial)?;
  assert_eq!(dev_fixture.execute("./dist/main.js")?, serde_json::json!([10, [10, 20], 42]));

  for (main_src, expected) in [
    (
      "import {stable} from './stable.js';const {b}=await import('./target.js');\
       events.push(b,globalThis.internal,stable());import.meta.hot.accept();",
      serde_json::json!([20, [10, 20], 42]),
    ),
    (
      "import {stable} from './stable.js';const ns=await import('./target.js');\
       events.push(Object.keys(ns).sort(),ns.a,ns.b,stable());import.meta.hot.accept();",
      serde_json::json!([["a", "b", "internal"], 10, 20, 42]),
    ),
    (
      "import {stable} from './stable.js';const {a}=await import('./target.js');\
       events.push(a,globalThis.internal,stable());import.meta.hot.accept();",
      serde_json::json!([10, [10, 20], 42]),
    ),
  ] {
    dev_fixture.write("main.js", main_src)?;
    let actual = dev_cached.incremental_generate(partial_scan(&dev_fixture, "main.js")).await?;
    ensure!(
      dev_cached.last_reused_finalized_modules() > 0,
      "unchanged sibling modules must reuse finalized sources"
    );
    let mut fresh = Bundler::new(dev_opts.clone())?;
    let expected_out = fresh.incremental_generate(ScanMode::Full).await?;
    fresh.close().await?;
    assert_eq!(asset_map(&actual), asset_map(&expected_out));
    dev_fixture.output(&actual)?;
    assert_eq!(dev_fixture.execute("./dist/main.js")?, expected);
  }
  dev_cached.close().await?;

  // 2. Simulated-facade export-set and semantic-namespace transitions on an unchanged module
  //    whose exports `a` and `b` remain internally referenced across every build.
  let facade_fixture = Fixture::new(&[
    (
      "main.js",
      "import './other.js';const {a}=await import('./target-shared.js');\
       events.push(a,globalThis.internal,globalThis.helperVal);",
    ),
    ("other.js", "import {helper} from './helper-shared.js';globalThis.helperVal=helper();"),
    (
      "target-shared.js",
      "export let a=10;export let b=20;\
       export function internal(){return [a,b];}globalThis.internal=internal();",
    ),
    ("helper-shared.js", "export function helper(){return 7;}"),
  ])?;
  let make_facade_opts = || {
    let mut opts = facade_fixture.options();
    opts.minify = Some(RawMinifyOptions::Bool(false));
    opts.treeshake = TreeshakeOptions::Boolean(true);
    opts.experimental =
      Some(ExperimentalOptions { incremental_build: Some(true), ..Default::default() });
    opts.code_splitting = Some(CodeSplittingMode::Advanced(ManualCodeSplittingOptions {
      groups: Some(vec![MatchGroup {
        name: MatchGroupName::Static("shared".into()),
        test: Some(MatchGroupTest::Function(Arc::new(|ids: Vec<String>| {
          Box::pin(async move { Ok(ids.iter().map(|id| id.ends_with("-shared.js")).collect()) })
        }))),
        min_share_count: Some(1),
        include_dependencies_recursively: Some(false),
        ..Default::default()
      }]),
      ..Default::default()
    }));
    opts
  };
  let mut facade_cached = Bundler::new(make_facade_opts())?;
  let facade_initial = facade_cached.incremental_generate(ScanMode::Full).await?;
  facade_fixture.output(&facade_initial)?;
  assert_eq!(facade_fixture.execute("./dist/main.js")?, serde_json::json!([10, [10, 20], 7]));

  for (main_src, expected) in [
    (
      "import './other.js';const {b}=await import('./target-shared.js');\
       events.push(b,globalThis.internal,globalThis.helperVal);",
      serde_json::json!([20, [10, 20], 7]),
    ),
    (
      "import './other.js';const ns=await import('./target-shared.js');\
       events.push(Object.keys(ns).sort(),ns.a,ns.b,globalThis.helperVal);",
      serde_json::json!([["a", "b", "internal"], 10, 20, 7]),
    ),
    (
      "import './other.js';import * as ns from './target-shared.js';\
       events.push(Object.keys(ns).sort(),ns.a,ns.b,globalThis.helperVal);",
      serde_json::json!([["a", "b", "internal"], 10, 20, 7]),
    ),
    (
      "import './other.js';const {a}=await import('./target-shared.js');\
       events.push(a,globalThis.internal,globalThis.helperVal);",
      serde_json::json!([10, [10, 20], 7]),
    ),
  ] {
    facade_fixture.write("main.js", main_src)?;
    let actual =
      facade_cached.incremental_generate(partial_scan(&facade_fixture, "main.js")).await?;
    let mut fresh = Bundler::new(make_facade_opts())?;
    let expected_out = fresh.incremental_generate(ScanMode::Full).await?;
    fresh.close().await?;
    assert_eq!(asset_map(&actual), asset_map(&expected_out));
    facade_fixture.output(&actual)?;
    assert_eq!(facade_fixture.execute("./dist/main.js")?, expected);
  }
  facade_cached.close().await?;
  Ok(())
}

#[tokio::test]
async fn development_mode_cache_tracks_absorbed_facade_and_star_namespace_completion() -> Result<()>
{
  // Absorbed simulated-facade namespace (`SimulateFacadeChunk`) where `target_exports` stays
  // the sole exported symbol of the shared common chunk while narrowed properties change from
  // `{ a }` to `{ b }` to the full namespace.
  let absorbed_fixture = Fixture::new(&[
    (
      "main.js",
      "import {helper} from './helper.js';const {a,absent}=await import('./target.js');\
       events.push(a,absent??null,globalThis.internal,helper());",
    ),
    (
      "preload.js",
      "import {helper} from './helper.js';import {internal} from './target.js';\
       globalThis.preloaded=[helper(),internal()];",
    ),
    (
      "target.js",
      "export let a=10;export let b=20;\
       export function internal(){return [a,b];}globalThis.internal=internal();",
    ),
    ("helper.js", "export function helper(){return 7;}"),
  ])?;
  let make_absorbed_opts = || {
    let mut opts = absorbed_fixture.options();
    opts.input = Some(vec![
      InputItem { name: Some("main".into()), import: "./main.js".into() },
      InputItem { name: Some("preload".into()), import: "./preload.js".into() },
    ]);
    opts.minify = Some(RawMinifyOptions::Bool(false));
    opts.treeshake = TreeshakeOptions::Boolean(true);
    opts.experimental = Some(ExperimentalOptions {
      incremental_build: Some(true),
      chunk_optimization: Some(ChunkOptimizationOption::Bool(true)),
      ..Default::default()
    });
    opts
  };
  let mut absorbed_cached = Bundler::new(make_absorbed_opts())?;
  let absorbed_initial = absorbed_cached.incremental_generate(ScanMode::Full).await?;
  absorbed_fixture.output(&absorbed_initial)?;
  assert_eq!(
    absorbed_fixture.execute("./dist/main.js")?,
    serde_json::json!([10, null, [10, 20], 7])
  );

  for (main_src, expected) in [
    (
      "import {helper} from './helper.js';const {b,absent}=await import('./target.js');\
       events.push(b,absent??null,globalThis.internal,helper());",
      serde_json::json!([20, null, [10, 20], 7]),
    ),
    (
      "import {helper} from './helper.js';const ns=await import('./target.js');\
       events.push(Object.keys(ns).sort(),ns.a,ns.b,helper());",
      serde_json::json!([["a", "b", "internal"], 10, 20, 7]),
    ),
    (
      "import {helper} from './helper.js';import * as ns from './target.js';\
       events.push(Object.keys(ns).sort(),ns.a,ns.b,helper());",
      serde_json::json!([["a", "b", "internal"], 10, 20, 7]),
    ),
    (
      "import {helper} from './helper.js';const {a,absent}=await import('./target.js');\
       events.push(a,absent??null,globalThis.internal,helper());",
      serde_json::json!([10, null, [10, 20], 7]),
    ),
  ] {
    absorbed_fixture.write("main.js", main_src)?;
    let actual =
      absorbed_cached.incremental_generate(partial_scan(&absorbed_fixture, "main.js")).await?;
    let mut fresh = Bundler::new(make_absorbed_opts())?;
    let expected_out = fresh.incremental_generate(ScanMode::Full).await?;
    fresh.close().await?;
    assert_eq!(asset_map(&actual), asset_map(&expected_out));
    absorbed_fixture.output(&actual)?;
    assert_eq!(absorbed_fixture.execute("./dist/main.js")?, expected);
  }
  absorbed_cached.close().await?;

  // 4. Bundled development mode re-finalizes an unchanged CJS star-export barrel when a dynamic
  //    importer flips `star_exports_complete_namespace`, while keeping unrelated modules cached.
  let star_fixture = Fixture::new(&[
    (
      "main.js",
      "import * as ns from './barrel.js';import {anchor} from './stable.js';\
       events.push(ns.x,anchor);import.meta.hot.accept();",
    ),
    ("barrel.js", "export * from './cjs.cjs';"),
    ("cjs.cjs", "exports.x=7;"),
    ("stable.js", "export const anchor=99;"),
  ])?;
  let mut star_opts = options(&star_fixture, false, false);
  star_opts.platform = Some(Platform::Browser);
  star_opts.code_splitting = Some(CodeSplittingMode::Bool(false));
  star_opts.experimental.as_mut().unwrap().resolve_new_url_to_asset = Some(false);
  let mut star_cached = Bundler::new(star_opts.clone())?;
  let star_initial = star_cached.incremental_generate(ScanMode::Full).await?;
  star_fixture.output(&star_initial)?;
  assert_eq!(star_fixture.execute("./dist/main.js")?, serde_json::json!([7, 99]));

  for main_src in [
    "import {anchor} from './stable.js';const ns=await import('./barrel.js');\
     events.push(ns.x,anchor);import.meta.hot.accept();",
    "import * as ns from './barrel.js';import {anchor} from './stable.js';\
     events.push(ns.x,anchor);import.meta.hot.accept();",
  ] {
    star_fixture.write("main.js", main_src)?;
    let actual = star_cached.incremental_generate(partial_scan(&star_fixture, "main.js")).await?;
    ensure!(
      star_cached.last_reused_finalized_modules() > 0,
      "unrelated modules must keep reusing finalized sources"
    );
    let mut fresh = Bundler::new(star_opts.clone())?;
    let expected_out = fresh.incremental_generate(ScanMode::Full).await?;
    fresh.close().await?;
    assert_eq!(asset_map(&actual), asset_map(&expected_out));
    star_fixture.output(&actual)?;
    assert_eq!(star_fixture.execute("./dist/main.js")?, serde_json::json!([7, 99]));
  }
  star_cached.close().await?;
  Ok(())
}

#[tokio::test]
async fn hmr_reshaped_exports_emit_reshaped_hook_and_reload_on_moved_binding() -> Result<()> {
  let fixture = Fixture::new(&[
    (
      "main.js",
      "import { a } from './barrel.js';import './solo.js';\
       events.push(a);import.meta.hot.accept();",
    ),
    ("barrel.js", "export * from './leaf.js';export { x as kept } from './one.js';"),
    ("leaf.js", "export const a = 1;"),
    ("solo.js", "export const unheld = 10;import.meta.hot.accept();"),
    ("one.js", "export const x = 'one';"),
    ("two.js", "export const x = 'two';"),
  ])?;
  let mut bundler = Bundler::new(options(&fixture, true, false))?;
  fixture.output(&bundler.incremental_generate(ScanMode::Full).await?)?;
  let mut stamps = HmrStampTable::default();
  let (evaluated, ..) = bundler.compute_top_level_evaluated_modules(&stamps);
  let mut shipped = evaluated.clone();
  let next = Arc::new(AtomicU32::new(0));

  // Reshaping `leaf.js` (held through `barrel.js`) and `solo.js` (imported for side effects only)
  // patches the page and lists both in `reshaped(all, unheld)`, with only `solo.js` in `unheld`.
  fixture.write("leaf.js", "export const a = 1;export const added = 2;")?;
  fixture.write(
    "solo.js",
    "export const unheld = 10;export const extra = 20;import.meta.hot.accept();",
  )?;
  let changes = FxIndexMap::from_iter([
    (fixture.0.join("leaf.js").to_string_lossy().into_owned(), WatcherChangeKind::Update),
    (fixture.0.join("solo.js").to_string_lossy().into_owned(), WatcherChangeKind::Update),
  ]);
  let updates = bundler
    .compute_hmr_update_for_file_changes(
      &changes,
      &[ClientHmrInput {
        client_id: "fixture",
        shipped: &shipped,
        top_level_evaluated: &evaluated,
      }],
      &mut stamps,
      Arc::clone(&next),
      false,
      false,
    )
    .await?;
  ensure!(updates.len() == 1);
  let HmrUpdate::Patch(patch) = &updates[0].update else {
    anyhow::bail!("expected reshaped export edit to patch, got {:?}", updates[0].update);
  };
  ensure!(
    patch.code.contains(
      "__rolldown_runtime__.reshaped?.([\"barrel.js\",\"leaf.js\",\"solo.js\"],[\"solo.js\"]);"
    ),
    "unexpected reshaped hook payload:\n{}",
    patch.code
  );
  shipped.extend(patch.carried.iter().cloned());

  // Moving a kept export (`kept`) to a different binding reloads cleanly.
  fixture.write("barrel.js", "export * from './leaf.js';export { x as kept } from './two.js';")?;
  let moved_changes = FxIndexMap::from_iter([(
    fixture.0.join("barrel.js").to_string_lossy().into_owned(),
    WatcherChangeKind::Update,
  )]);
  let moved_updates = bundler
    .compute_hmr_update_for_file_changes(
      &moved_changes,
      &[ClientHmrInput {
        client_id: "fixture",
        shipped: &shipped,
        top_level_evaluated: &evaluated,
      }],
      &mut stamps,
      Arc::clone(&next),
      false,
      false,
    )
    .await?;
  ensure!(moved_updates.len() == 1);
  let HmrUpdate::FullReload { reason } = &moved_updates[0].update else {
    anyhow::bail!("expected moved export binding to reload, got {:?}", moved_updates[0].update);
  };
  ensure!(
    reason.contains("the binding of its export kept moved"),
    "unexpected reload reason: {reason}"
  );
  bundler.close().await?;
  Ok(())
}

#[tokio::test]
async fn lazy_chunk_cache_reuses_unchanged_closures_in_native_cell_graphs() -> Result<()> {
  let fixture = Fixture::new(&[
    (
      "main.js",
      "import { active } from './active.js';import { shared } from './cycle-a.js';\
       events.push(active, shared());globalThis.loadRoute = () => import('./route.js');\
       import.meta.hot.accept();",
    ),
    ("active.js", "export const active = 'a0';import.meta.hot.accept();"),
    ("cycle-a.js", "import { b } from './cycle-b.js';export function shared() { return b(); }"),
    (
      "cycle-b.js",
      "import { shared } from './cycle-a.js';export const tag = 'cell';\
       export function b() { return typeof shared + ':' + tag; }",
    ),
    (
      "route.js",
      "import { leaf } from './leaf.js';import { tag } from './cycle-b.js';\
       export const read = () => `${leaf}:${tag}`;",
    ),
    ("leaf.js", "export const leaf = 'l0';"),
  ])?;
  let mut bundler = Bundler::new(options(&fixture, true, false))?;
  fixture.output(&bundler.incremental_generate(ScanMode::Full).await?)?;
  let mut stamps = HmrStampTable::default();
  let (evaluated, ..) = bundler.compute_top_level_evaluated_modules(&stamps);
  let next = Arc::new(AtomicU32::new(0));
  let route_id = fixture.0.join("route.js").to_string_lossy().into_owned();

  let rerun = FxHashSet::default();
  let first = bundler
    .compile_lazy_entry(
      &route_id,
      "native",
      &evaluated,
      &rerun,
      &evaluated,
      None,
      None,
      None,
      None,
      &stamps,
      Arc::clone(&next),
    )
    .await?;

  // Editing an unrelated module outside `route.js`'s closure reuses the cached lazy chunk even
  // though the graph contains native cell modules (`cycle-a.js` <-> `cycle-b.js`).
  fixture.write("active.js", "export const active = 'a1';import.meta.hot.accept();")?;
  let changes = FxIndexMap::from_iter([(
    fixture.0.join("active.js").to_string_lossy().into_owned(),
    WatcherChangeKind::Update,
  )]);
  bundler
    .compute_hmr_update_for_file_changes(
      &changes,
      &[ClientHmrInput {
        client_id: "fixture",
        shipped: &evaluated,
        top_level_evaluated: &evaluated,
      }],
      &mut stamps,
      Arc::clone(&next),
      false,
      false,
    )
    .await?;
  let reused = bundler
    .compile_lazy_entry(
      &route_id,
      "native",
      &evaluated,
      &rerun,
      &evaluated,
      None,
      None,
      None,
      None,
      &stamps,
      Arc::clone(&next),
    )
    .await?;
  assert_eq!(reused.filename, first.filename, "unchanged closure must reuse cached lazy chunk");

  // Editing `leaf.js` inside `route.js`'s closure invalidates the cached lazy chunk.
  fixture.write("leaf.js", "export const leaf = 'l1';")?;
  let leaf_changes = FxIndexMap::from_iter([(
    fixture.0.join("leaf.js").to_string_lossy().into_owned(),
    WatcherChangeKind::Update,
  )]);
  bundler
    .compute_hmr_update_for_file_changes(
      &leaf_changes,
      &[ClientHmrInput {
        client_id: "fixture",
        shipped: &evaluated,
        top_level_evaluated: &evaluated,
      }],
      &mut stamps,
      Arc::clone(&next),
      false,
      false,
    )
    .await?;
  let rerendered = bundler
    .compile_lazy_entry(
      &route_id,
      "native",
      &evaluated,
      &rerun,
      &evaluated,
      None,
      None,
      None,
      None,
      &stamps,
      Arc::clone(&next),
    )
    .await?;
  assert_ne!(rerendered.filename, reused.filename, "closure edit must render a fresh lazy chunk");
  bundler.close().await?;
  Ok(())
}
