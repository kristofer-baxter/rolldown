use std::sync::{Arc, atomic::AtomicU32};

use anyhow::{Result, ensure};
use rolldown::{Bundler, BundlerOptions};
use rolldown_common::{
  ClientHmrInput, DevModeOptions, ExperimentalOptions, HmrStampTable, HmrUpdate, InputItem,
  RawMinifyOptions, ScanMode, TreeshakeOptions, WatcherChangeKind,
};
use rolldown_utils::indexmap::FxIndexMap;

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
async fn bundled_hmr_refreshes_wide_import_observation_after_relink() -> Result<()> {
  let fixture = Fixture::new(&[])?;
  let mut imports = String::new();
  for index in 0..64 {
    imports.push_str(&format!("import {{value as value{index}}} from './leaf{index}.js';"));
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
      &Default::default(),
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
    await import('./lazy.mjs');
    assert.equal(globalThis.routeRuns,1,'lazy code executes only on activation');
    const route=record('route.js');
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
