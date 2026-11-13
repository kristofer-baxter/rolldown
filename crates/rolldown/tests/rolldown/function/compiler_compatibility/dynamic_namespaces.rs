use std::{borrow::Cow, path::Path, process::Command, sync::Arc};

use rolldown::{BundleOutput, Bundler, BundlerOptions};
use rolldown_common::{
  CodeSplittingMode, EmittedChunk, ExperimentalOptions, InputItem, ManualCodeSplittingOptions,
  MatchGroup, MatchGroupName, MatchGroupTest, Platform, PreserveEntrySignatures, ScanMode,
};
use rolldown_plugin::{HookBuildStartArgs, HookNoopReturn, HookUsage, Plugin, PluginContext};
use serde_json::{Value, json};

use super::Fixture;

fn run(root: &Path, script: &str) -> Value {
  let output = Command::new("node")
    .args(["--input-type=module", "-e", script])
    .current_dir(root)
    .output()
    .expect("execute Node reference or generated bundle");
  assert!(
    output.status.success(),
    "Node in {}:\n{}",
    root.display(),
    String::from_utf8_lossy(&output.stderr)
  );
  serde_json::from_slice(&output.stdout).expect("Node prints one JSON value")
}

fn output(fixture: &Fixture, output: &BundleOutput, script: &str) -> Value {
  fixture.output(output).unwrap();
  run(&fixture.0.join("dist"), script)
}

fn options(fixture: &Fixture, lazy: bool, strict: bool, split: bool) -> BundlerOptions {
  let mut options = fixture.options();
  options.platform = Some(Platform::Browser);
  options.strict_execution_order = Some(strict);
  options.code_splitting = Some(CodeSplittingMode::Bool(split));
  options.experimental =
    Some(ExperimentalOptions { lazy_barrel: Some(lazy), ..Default::default() });
  options
}

async fn build(fixture: &Fixture, options: BundlerOptions, script: &str) -> (Value, Vec<String>) {
  let mut bundler = Bundler::new(options).unwrap();
  let result = bundler.generate().await.unwrap();
  let watch_files = bundler.watch_files().iter().map(|file| file.to_string()).collect();
  bundler.close().await.unwrap();
  (output(fixture, &result, script), watch_files)
}

const PROBE: &str = "export function probe(ns){return {
  prototype:Object.getPrototypeOf(ns),extensible:Object.isExtensible(ns),
  sealed:Object.isSealed(ns),frozen:Object.isFrozen(ns),inherited:'toString' in ns,
  star:'b' in ns,keys:Object.keys(ns).filter(k=>k!=='module.exports'),
  a:Object.getOwnPropertyDescriptor(ns,'a'),b:Object.getOwnPropertyDescriptor(ns,'b'),
  live:[ns.count,(ns.bump(),ns.count),Object.getOwnPropertyDescriptor(ns,'count').value],
  added:Reflect.defineProperty(ns,'added',{value:1}),set:Reflect.set(ns,'a',1)
}}";

fn namespace_fixture(then: &str, star: &str) -> Fixture {
  Fixture::new(&[
    ("probe.js", PROBE),
    ("extra.cjs", "globalThis.extraRan=true;exports.b=7;"),
    ("inner.js", "export * from './extra.cjs';"),
    ("other.js", "export const other=1;"),
    (
      "lib.js",
      &format!(
        "import{{probe}}from'./probe.js';export const a=42;export let count=0;
        export const bump=()=>{{count++}};export * from '{star}';{then}"
      ),
    ),
    (
      "main.js",
      "import{probe}from'./probe.js';export const load=async()=>{
      const ns=await import('./lib.js');return ns.fromThen??probe(ns)};
      globalThis.load=load;",
    ),
  ])
  .unwrap()
}

#[tokio::test]
async fn dynamic_commonjs_stars_match_node_namespaces_and_then_assimilation() {
  let script = "const m=await import('./main.js');console.log(JSON.stringify(await m.load()))";
  for then in [
    "export function then(resolve){resolve({fromThen:probe(this)})}",
    "export const then=1;",
    "export * as then from './other.js';",
    "",
  ] {
    for star in ["./extra.cjs", "./inner.js"] {
      let fixture = namespace_fixture(then, star);
      let expected = run(&fixture.0, script);
      assert_eq!(expected["b"]["value"], 7);
      for lazy in [false, true] {
        for split in [false, true] {
          for strict in [false, true] {
            let observed = build(&fixture, options(&fixture, lazy, strict, split), script).await.0;
            assert_eq!(
              observed, expected,
              "then={then},star={star},lazy={lazy},strict={strict},split={split}"
            );
          }
        }
      }
    }
  }
}

#[tokio::test]
async fn bare_dynamic_commonjs_star_keeps_its_runtime_exports() {
  let fixture = Fixture::new(&[
    ("main.js", "export const load=()=>import('./star.js');"),
    ("star.js", "export * from './extra.cjs';"),
    ("extra.cjs", "exports.b=7;"),
  ])
  .unwrap();
  let script = "const m=await import('./main.js'),ns=await m.load();
    console.log(JSON.stringify({b:ns.b,prototype:Object.getPrototypeOf(ns),
    extensible:Object.isExtensible(ns),keys:Object.keys(ns).filter(k=>k!=='module.exports')}))";
  let expected = run(&fixture.0, script);
  for preserve in [false, true] {
    for split in [false, true] {
      if preserve && !split {
        continue;
      }
      let mut opts = options(&fixture, false, true, split);
      opts.preserve_modules = Some(preserve);
      assert_eq!(
        build(&fixture, opts, script).await.0,
        expected,
        "preserve={preserve},split={split}"
      );
    }
  }
}

#[tokio::test]
async fn explicit_and_preserved_inputs_keep_static_signatures_and_dynamic_star_names() {
  for then in [
    "export function then(resolve){resolve({fromThen:probe(this)})}",
    "export const then=1;",
    "export * as then from './other.js';",
    "",
  ] {
    let fixture = namespace_fixture(then, "./extra.cjs");
    let script="import * as lib from './lib.js';await import('./main.js');
      console.log(JSON.stringify({loaded:await globalThis.load(),
      statics:[lib.a,typeof lib.then],keys:Object.keys(lib).filter(k=>k!=='module.exports'&&k!=='b')}))";
    let expected = run(&fixture.0, script);
    for strict in [false, true] {
      for signature in [
        None,
        Some(PreserveEntrySignatures::Strict),
        Some(PreserveEntrySignatures::AllowExtension),
        Some(PreserveEntrySignatures::False),
      ] {
        let mut opts = options(&fixture, false, strict, true);
        opts
          .input
          .as_mut()
          .unwrap()
          .push(InputItem { name: Some("lib".into()), import: "./lib.js".into() });
        opts.preserve_entry_signatures = signature;
        let observed = build(&fixture, opts, script).await.0;
        assert_eq!(
          observed["loaded"], expected["loaded"],
          "then={then},strict={strict},signature={signature:?}"
        );
        assert_eq!(observed["statics"], expected["statics"]);
        if matches!(signature, None | Some(PreserveEntrySignatures::Strict)) {
          assert_eq!(observed["keys"], expected["keys"]);
        }
      }
      let mut opts = options(&fixture, false, strict, true);
      opts.preserve_modules = Some(true);
      // An explicit input guarantees a stable public filename even when it has no `then`.
      opts
        .input
        .as_mut()
        .unwrap()
        .push(InputItem { name: Some("lib".into()), import: "./lib.js".into() });
      assert_eq!(
        build(&fixture, opts, script).await.0,
        expected,
        "preserve,then={then},strict={strict}"
      );
    }
  }
}

#[tokio::test]
async fn self_imports_use_the_completed_namespace_without_an_extra_chunk() {
  let fixture = Fixture::new(&[
    (
      "main.js",
      "export * from './extra.cjs';export const then=1;export const load=()=>import('./main.js');",
    ),
    ("extra.cjs", "exports.b=7;"),
  ])
  .unwrap();
  let script = "import * as entry from './main.js';const ns=await entry.load();
    console.log(JSON.stringify({b:ns.b,then:ns.then,prototype:Object.getPrototypeOf(ns),
    extensible:Object.isExtensible(ns),keys:Object.keys(ns).filter(k=>k!=='module.exports')}))";
  let expected = run(&fixture.0, script);
  for strict in [false, true] {
    for split in [false, true] {
      let mut bundler = Bundler::new(options(&fixture, false, strict, split)).unwrap();
      let result = bundler.generate().await.unwrap();
      bundler.close().await.unwrap();
      assert_eq!(
        result
          .assets
          .iter()
          .filter(|asset| matches!(asset, rolldown_common::Output::Chunk(_)))
          .count(),
        1
      );
      assert_eq!(output(&fixture, &result, script), expected, "strict={strict},split={split}");
    }
  }
}

#[tokio::test]
async fn cyclic_integrity_operations_cannot_close_a_star_namespace_early() {
  let fixture = Fixture::new(&[
    ("main.js", "import './lib.js';export {late} from './cycle.js';"),
    ("lib.js", "import './cycle.js';export const a=1;export * from './extra.cjs';"),
    (
      "cycle.js",
      "import * as ns from './lib.js';Object.isExtensible(ns);Reflect.preventExtensions(ns);
      export const late=()=>({b:ns.b,keys:Object.keys(ns).filter(k=>k!=='module.exports'),
      extensible:Object.isExtensible(ns),sealed:Object.isSealed(ns)});",
    ),
    ("extra.cjs", "exports.b=7;"),
  ])
  .unwrap();
  let script = "console.log(JSON.stringify((await import('./main.js')).late()))";
  let expected = run(&fixture.0, script);
  for lazy in [false, true] {
    for split in [false, true] {
      assert_eq!(build(&fixture, options(&fixture, lazy, true, split), script).await.0, expected);
    }
  }
}

#[derive(Debug)]
struct EmitThen;

impl Plugin for EmitThen {
  fn name(&self) -> Cow<'static, str> {
    "emit-then".into()
  }
  fn register_hook_usage(&self) -> HookUsage {
    HookUsage::BuildStart
  }
  async fn build_start(
    &self,
    ctx: &PluginContext,
    _args: &HookBuildStartArgs<'_>,
  ) -> HookNoopReturn {
    ctx.emit_chunk(EmittedChunk {
      name: Some("emitted".into()),
      id: "./emitted.js".into(),
      preserve_entry_signatures: Some(PreserveEntrySignatures::AllowExtension),
      ..Default::default()
    })?;
    Ok(())
  }
}

#[tokio::test]
async fn grouped_inputs_read_their_namespace_not_a_neighboring_exported_then() {
  let fixture = Fixture::new(&[
    ("main.js", "export const load=()=>import('./lib.js');"),
    ("lib.js", "export const a=42;export * from './extra.cjs';"),
    ("extra.cjs", "exports.b=7;"),
    ("emitted.js", "export function then(resolve){resolve('wrong')}"),
  ])
  .unwrap();
  let script = "const ns=await(await import('./main.js')).load();console.log(JSON.stringify({
    a:ns.a,b:ns.b,then:typeof ns.then,prototype:Object.getPrototypeOf(ns),
    keys:Object.keys(ns).filter(k=>k!=='module.exports')}))";
  let expected = run(&fixture.0, script);
  for strict in [false, true] {
    let mut opts = options(&fixture, false, strict, true);
    opts
      .input
      .as_mut()
      .unwrap()
      .push(InputItem { name: Some("lib".into()), import: "./lib.js".into() });
    opts.code_splitting = Some(CodeSplittingMode::Advanced(ManualCodeSplittingOptions {
      groups: Some(vec![MatchGroup {
        name: MatchGroupName::Static("group".into()),
        test: Some(MatchGroupTest::Function(Arc::new(|ids: Vec<String>| {
          Box::pin(async move {
            Ok(
              ids
                .iter()
                .map(|id| {
                  ["lib.js", "extra.cjs", "emitted.js"]
                    .iter()
                    .any(|name| Path::new(id).ends_with(name))
                })
                .collect(),
            )
          })
        }))),
        ..Default::default()
      }]),
      ..Default::default()
    }));
    let mut bundler = Bundler::with_plugins(opts, vec![EmitThen::new_shared(EmitThen)]).unwrap();
    let result = bundler.generate().await.unwrap();
    bundler.close().await.unwrap();
    assert_eq!(output(&fixture, &result, script), expected, "strict={strict}");
  }
}

#[tokio::test]
async fn dropped_dynamic_imports_do_not_extend_an_explicit_inputs_public_signature() {
  for strict in [false, true] {
    let fixture = namespace_fixture("export const then=42;", "./extra.cjs");
    fixture
      .write("helpers.js", "export const kept=1;export const load=()=>import('./lib.js');")
      .unwrap();
    fixture.write("dropped.js", "import{kept}from'./helpers.js';export const value=kept;").unwrap();
    let script = "import*as lib from'./lib.js';console.log(JSON.stringify(Object.keys(lib)))";
    let expected = json!(["a", "bump", "count", "then"]);
    let mut opts = options(&fixture, false, strict, true);
    opts.input = Some(
      ["dropped", "lib"]
        .into_iter()
        .map(|name| InputItem { name: Some(name.into()), import: format!("./{name}.js") })
        .collect(),
    );
    let (observed, _) = build(&fixture, opts, script).await;
    assert_eq!(observed, expected, "strict={strict}");
  }
}

#[tokio::test]
async fn lazy_barrels_keep_inlined_static_constant_owners_but_not_unused_leaves() {
  for strict in [false, true] {
    let fixture = barrel_fixture(
      "import{a}from'pure';export const start=a;
       export const load=async()=>{const{b}=await import('pure');return b};
       export const later=()=>import('./later.js');",
      "export{a}from'./a.js';export{b}from'./b.js';export{c}from'./c.js';export{d}from'./d.js';",
    );
    fixture
      .write("node_modules/pure/a.js", "(globalThis.evaluated??=[]).push('a');export const a='a';")
      .unwrap();
    fixture.write("later.js", "export{c}from'pure';").unwrap();
    let script = "const m=await import('./main.js');const startup=globalThis.evaluated?.slice();
      const read=await m.load();console.log(JSON.stringify({start:m.start,startup,read,all:globalThis.evaluated}));";
    let (observed, _) = build(&fixture, options(&fixture, true, strict, true), script).await;
    assert_eq!(
      observed,
      json!({"start":"a","startup":["a"],"read":7,"all":["a","b"]}),
      "strict={strict}"
    );
  }
}

fn barrel_fixture(main: &str, index: &str) -> Fixture {
  Fixture::new(&[
    ("main.js", main),
    (
      "node_modules/pure/package.json",
      r#"{"name":"pure","type":"module","main":"index.js","sideEffects":false}"#,
    ),
    ("node_modules/pure/index.js", index),
    ("node_modules/pure/a.js", "(globalThis.evaluated??=[]).push('a');export const a={n:42};"),
    ("node_modules/pure/b.js", "(globalThis.evaluated??=[]).push('b');export const b=7;"),
    ("node_modules/pure/c.js", "(globalThis.evaluated??=[]).push('c');export const c=3;"),
    ("node_modules/pure/d.js", "(globalThis.evaluated??=[]).push('d');export const d=4;"),
    ("node_modules/pure/setup.js", "export const setup=()=>{globalThis.setup=true};"),
    (
      "node_modules/pure/extra.cjs",
      "exports.extra=1;exports.b=7;exports.then=function(resolve){resolve({a:{n:this.b}})};",
    ),
  ])
  .unwrap()
}

#[tokio::test]
async fn lazy_barrel_dynamic_reads_do_not_replace_static_demands_or_eagerly_run_other_leaves() {
  for load in [
    "const {b}=await import('pure');return [b]",
    "return [(await import('pure')).b]",
    "return Object.keys(await import('pure')).sort()",
  ] {
    let fixture = barrel_fixture(
      &format!(
        "import{{a}}from'pure';export const start=a.n;export const load=async()=>{{{load}}};
      export const later=()=>import('./later.js');"
      ),
      "export{a}from'./a.js';export{b}from'./b.js';export{c}from'./c.js';export{d}from'./d.js';",
    );
    fixture.write("later.js", "import{c}from'pure';export const value=c;").unwrap();
    let escaping = load.contains("Object.keys");
    let script="const m=await import('./main.js');const startup=[...globalThis.evaluated];
      const read=await m.load();console.log(JSON.stringify({start:m.start,startup,read,all:globalThis.evaluated}))";
    for lazy in [false, true] {
      let (observed, watch) = build(&fixture, options(&fixture, lazy, true, true), script).await;
      if !lazy {
        assert_eq!(observed["start"], 42);
        assert_eq!(
          observed["read"],
          if escaping { json!(["a", "b", "c", "d"]) } else { json!([7]) }
        );
        continue;
      }
      assert_eq!(
        observed,
        json!({"start":42,"startup":["a"],
        "read":if escaping{json!(["a","b","c","d"])}else{json!([7])},
        "all":if escaping{json!(["a","b","c","d"])}else{json!(["a","b"])}}),
        "load={load},lazy={lazy}"
      );
      if lazy {
        assert_eq!(watch.iter().any(|id| Path::new(id).ends_with("pure/d.js")), escaping);
      }
    }
  }
}

#[tokio::test]
async fn pure_barrels_leave_dynamic_leaves_lazy_while_effectful_barrels_keep_them() {
  let fixture = barrel_fixture(
    "import{a}from'pure';import{used}from'plain';export const start=[a.n,used];
    export const load=async()=>[(await import('pure')).b,(await import('plain')).other];",
    "export{a}from'./a.js';export{b}from'./b.js';",
  );
  fixture
    .write("node_modules/plain/package.json", r#"{"type":"module","main":"index.js"}"#)
    .unwrap();
  fixture
    .write(
      "node_modules/plain/index.js",
      "export{used}from'./used.js';export{other}from'./other.js';",
    )
    .unwrap();
  fixture
    .write(
      "node_modules/plain/used.js",
      "globalThis.evaluated.push('plain/used');export const used=1;",
    )
    .unwrap();
  fixture
    .write(
      "node_modules/plain/other.js",
      "globalThis.evaluated.push('plain/other');export const other=2;",
    )
    .unwrap();
  let script = "const m=await import('./main.js');const startup=[...globalThis.evaluated];
    const read=await m.load();console.log(JSON.stringify({startup,read,all:globalThis.evaluated}))";
  for lazy in [false, true] {
    let observed = build(&fixture, options(&fixture, lazy, true, true), script).await.0;
    let startup = if lazy {
      json!(["a", "plain/used", "plain/other"])
    } else {
      json!(["a", "b", "plain/used", "plain/other"])
    };
    let all = if lazy { json!(["a", "plain/used", "plain/other", "b"]) } else { startup.clone() };
    assert_eq!(observed, json!({"startup":startup,"read":[7,2],"all":all}), "lazy={lazy}");
  }
}

#[tokio::test]
async fn lazy_barrel_dynamic_stars_promote_complete_bodies_and_assimilate_then() {
  let script = "const m=await import('./main.js');console.log(JSON.stringify({
    value:await m.load(),setup:globalThis.setup??false,midSetup:globalThis.midSetup??false}))";
  for (index, inner, main, expected) in [
    (
      "export*from'./inner.js';",
      "import{setup}from'./setup.js';export{a}from'./a.js';export*from'./only-extra.cjs';setup();",
      "import{extra}from'pure';export const load=async()=>extra;",
      json!({"value":1,"setup":true,"midSetup":false}),
    ),
    (
      "export*from'./mid.js';",
      "import{setup}from'./setup.js';export{a}from'./a.js';export*from'./only-extra.cjs';setup();",
      "import{extra}from'pure';export const load=async()=>extra;",
      json!({"value":1,"setup":true,"midSetup":true}),
    ),
    (
      "export{a}from'./a.js';export{b}from'./b.js';export*from'./extra.cjs';",
      "",
      "export const load=()=>import('pure').then(({a})=>a.n);",
      json!({"value":7,"setup":false,"midSetup":false}),
    ),
  ] {
    let fixture = barrel_fixture(main, index);
    fixture.write("node_modules/pure/inner.js", inner).unwrap();
    fixture
      .write("node_modules/pure/mid.js", "globalThis.midSetup=true;export*from'./inner.js';")
      .unwrap();
    fixture.write("node_modules/pure/only-extra.cjs", "exports.extra=1;").unwrap();
    for lazy in [false, true] {
      assert_eq!(
        build(&fixture, options(&fixture, lazy, true, true), script).await.0,
        expected,
        "index={index},lazy={lazy}"
      );
    }
  }
}

#[tokio::test]
async fn incremental_barrels_reconsider_static_stars_that_become_commonjs() {
  let fixture = barrel_fixture(
    "import{extra}from'pure';export const load=async()=>extra;",
    "export*from'./inner.js';",
  );
  fixture
    .write(
      "node_modules/pure/inner.js",
      "import{setup}from'./setup.js';export{a}from'./a.js';export*from'./extra.js';setup();",
    )
    .unwrap();
  fixture.write("node_modules/pure/extra.js", "export const extra=1;").unwrap();
  let mut opts = options(&fixture, true, true, true);
  opts.experimental.as_mut().unwrap().incremental_build = Some(true);
  let mut bundler = Bundler::new(opts).unwrap();
  let script = "console.log(JSON.stringify({value:await(await import('./main.js')).load(),setup:globalThis.setup??false}))";
  let first = bundler.generate().await.unwrap();
  assert_eq!(output(&fixture, &first, script), json!({"value":1,"setup":false}));
  let id = bundler
    .watch_files()
    .iter()
    .find(|id| Path::new(id.as_str()).ends_with("pure/extra.js"))
    .unwrap()
    .clone();
  fixture.write("node_modules/pure/extra.js", "module.exports={extra:1};").unwrap();
  let second = bundler.incremental_generate(ScanMode::Partial(vec![id])).await.unwrap();
  assert_eq!(output(&fixture, &second, script), json!({"value":1,"setup":true}));
  bundler.close().await.unwrap();
}

#[tokio::test]
async fn broken_dynamic_barrel_closures_reject_at_import_and_recover_incrementally() {
  for setup in ["export const setup=()=>{;", "export const other=1;"] {
    let fixture = barrel_fixture(
      "export const load=()=>import('./importer.js').then(m=>m.value,e=>e.name);",
      "import{setup}from'./setup.js';export*from'./extra.js';setup();",
    );
    fixture.write("importer.js", "import{extra}from'pure';export const value=extra;").unwrap();
    fixture.write("node_modules/pure/setup.js", setup).unwrap();
    fixture.write("node_modules/pure/extra.js", "module.exports={extra:2};").unwrap();
    let script = "console.log(JSON.stringify(await(await import('./main.js')).load()))";
    assert_eq!(run(&fixture.0, script), "SyntaxError");
    let opts = options(&fixture, true, true, true);
    assert_eq!(build(&fixture, opts, script).await.0, "SyntaxError");
    fixture.write("node_modules/pure/extra.js", "export const extra=1;").unwrap();
    let mut opts = options(&fixture, true, true, true);
    opts.experimental.as_mut().unwrap().incremental_build = Some(true);
    let mut bundler = Bundler::new(opts).unwrap();
    let first = bundler.generate().await.unwrap();
    assert_eq!(output(&fixture, &first, script), 1);
    assert!(
      !bundler.watch_files().iter().any(|id| Path::new(id.as_str()).ends_with("pure/setup.js"))
    );
    let id = bundler
      .watch_files()
      .iter()
      .find(|id| Path::new(id.as_str()).ends_with("pure/extra.js"))
      .unwrap()
      .clone();
    fixture.write("node_modules/pure/extra.js", "module.exports={extra:2};").unwrap();
    let second = bundler.incremental_generate(ScanMode::Partial(vec![id])).await.unwrap();
    assert_eq!(output(&fixture, &second, script), "SyntaxError");
    fixture.write("node_modules/pure/setup.js", "export const setup=()=>{};").unwrap();
    let recovered = bundler
      .incremental_generate(super::partial_scan(&fixture, "node_modules/pure/setup.js"))
      .await
      .unwrap();
    assert_eq!(output(&fixture, &recovered, script), 2);
    bundler.close().await.unwrap();
  }
}

#[tokio::test]
async fn deferred_barrel_diagnostics_still_fail_closed_for_static_and_ordinary_builds() {
  for setup in ["export const setup=()=>{;", "export const other=1;"] {
    for (lazy, static_import) in [(false, false), (false, true), (true, true)] {
      let fixture = barrel_fixture(
        if static_import {
          "import{extra}from'./importer.js';export const value=extra;"
        } else {
          "export const load=()=>import('./importer.js');"
        },
        "import{setup}from'./setup.js';export*from'./extra.js';setup();",
      );
      fixture
        .write(
          "importer.js",
          "import{extra}from'pure';export const extraValue=extra;export{extra};",
        )
        .unwrap();
      fixture.write("node_modules/pure/setup.js", setup).unwrap();
      fixture.write("node_modules/pure/extra.js", "module.exports={extra:2};").unwrap();
      let mut bundler = Bundler::new(options(&fixture, lazy, true, true)).unwrap();
      assert!(
        bundler.generate().await.is_err(),
        "lazy={lazy},static={static_import},setup={setup}"
      );
      bundler.close().await.unwrap();
    }
  }
  let fixture = Fixture::new(&[("main.js", "export const invalid=;")]).unwrap();
  let mut bundler = Bundler::new(options(&fixture, true, true, true)).unwrap();
  assert!(
    bundler.generate().await.is_err(),
    "an input's own parse error must remain a build error"
  );
  bundler.close().await.unwrap();
}

#[tokio::test]
async fn dynamic_entry_json_chunks_avoid_per_key_expansion_and_keep_namespace_keys() {
  use std::fmt::Write as _;

  let mut locale_json = String::from(
    "{\"__proto__\":\"proto-val\",\"default\":\"inner-default\",\"\":\"empty-val\",\"validIdent\":\"ident-val\"",
  );
  for idx in 0..200 {
    write!(
      locale_json,
      ",\"Section.Action.Key_{idx:03}\":\"Localized string value number {idx:03}\""
    )
    .unwrap();
  }
  locale_json.push('}');
  let raw_json_len = locale_json.len();

  for strict in [true, false] {
    let fixture = Fixture::new(&[
      ("package.json", "{}"),
      ("locale.json", &locale_json),
      ("receiver.json", r#"{"a":1,"b":2}"#),
      ("entry-false.json", r#"{"entryKey":"ok","extraKey":7}"#),
      ("strict/package.json", r#"{"type":"module"}"#),
      ("strict/strict-locale.json", &locale_json),
      ("strict/reader.mjs", "export const loadStrict = () => import('./strict-locale.json');"),
      (
        "main.js",
        "import { loadStrict } from './strict/reader.mjs';
         globalThis.run = async function run() {
           const dynLocale = await import('./locale.json');
           const k = 'Section.Action.Key_199';
           const rec = await import('./receiver.json');
           try { rec.toString(); } catch {}
           const tag = Object.prototype.toString.call(rec);
           const dual = await import('./entry-false.json');
           const strictNs = await loadStrict();
           return {
             keyCount: Object.keys(dynLocale).length,
             dynamicLookup: dynLocale[k],
             protoProp: dynLocale.__proto__,
             protoNull: Object.getPrototypeOf(dynLocale) === null,
             sealed: !Object.isExtensible(dynLocale) && Object.isSealed(dynLocale),
             receiverTag: tag,
             receiverKeys: Object.keys(rec),
             dualKeys: Object.keys(dual),
             dualVal: dual.entryKey,
             strictKeys: Object.keys(strictNs),
           };
         }",
      ),
    ])
    .unwrap();

    let mut opts = fixture.options();
    opts.input = Some(vec![
      InputItem { name: Some("main".into()), import: "./main.js".into() },
      InputItem { name: Some("entry-false".into()), import: "./entry-false.json".into() },
    ]);
    opts.preserve_entry_signatures = Some(PreserveEntrySignatures::False);
    opts.strict_execution_order = Some(strict);
    opts.experimental = Some(ExperimentalOptions {
      resolve_new_url_to_asset: Some(true),
      incremental_build: (!strict).then_some(true),
      ..Default::default()
    });

    let mut bundler = Bundler::new(opts).unwrap();
    let generated = bundler.generate().await.unwrap();
    bundler.close().await.unwrap();

    let locale_chunk = generated
      .assets
      .iter()
      .find_map(|asset| match asset {
        rolldown_common::Output::Chunk(c)
          if c.module_ids.iter().any(|id| id.as_str().ends_with("/locale.json")) =>
        {
          Some(&c.code)
        }
        _ => None,
      })
      .expect("dynamic locale chunk");
    assert!(
      locale_chunk.len() < raw_json_len * 3 / 2,
      "strict={strict}: expected compact JSON chunk ({} bytes vs {raw_json_len}):\n{locale_chunk}",
      locale_chunk.len()
    );
    assert!(
      !locale_chunk.contains("Section_Action_Key_"),
      "strict={strict}: unexpected per-key var expansion:\n{locale_chunk}"
    );

    let script = "await import('./main.js'); console.log(JSON.stringify(await globalThis.run()))";
    let actual = output(&fixture, &generated, script);
    assert_eq!(
      actual,
      json!({
        "keyCount": 203,
        "dynamicLookup": "Localized string value number 199",
        "protoProp": "proto-val",
        "protoNull": true,
        "sealed": true,
        "receiverTag": "[object Module]",
        "receiverKeys": ["a", "b", "default"],
        "dualKeys": ["default", "entryKey", "extraKey"],
        "dualVal": "ok",
        "strictKeys": ["default"],
      }),
      "strict={strict}"
    );
  }
}
