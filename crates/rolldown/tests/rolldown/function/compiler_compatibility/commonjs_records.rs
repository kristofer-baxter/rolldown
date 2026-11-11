use super::{Fixture, partial_scan};
use anyhow::{Context, Result};
use rolldown::{BundleOutput, Bundler, BundlerOptions};
use rolldown_common::{
  DevModeOptions, ExperimentalOptions, InputItem, Output, RawMinifyOptions, ScanMode,
  SourceMapType, TreeshakeOptions,
};
use serde_json::json;
use std::{collections::BTreeMap, fs, path::Path};

fn options(fixture: &Fixture) -> BundlerOptions {
  let mut options = fixture.options();
  options.preserve_modules = Some(true);
  options.strict_execution_order = Some(false);
  options.minify = Some(RawMinifyOptions::Bool(false));
  options.chunk_filenames = Some("[name].js".to_owned().into());
  options.experimental = Some(ExperimentalOptions {
    incremental_build: Some(true),
    resolve_new_url_to_asset: Some(true),
    ..Default::default()
  });
  options
}

fn publish(fixture: &Fixture, output: &BundleOutput, bundler: &Bundler) -> Result<()> {
  if let Some(change) = bundler.last_native_graph_change() {
    for name in &change.retired_filenames {
      let path = fixture.0.join("dist").join(name.as_str());
      if path.is_file() {
        fs::remove_file(path)?;
      }
    }
  }
  for asset in &output.assets {
    if !bundler.last_reused_chunk_filenames().contains(asset.filename()) {
      let path = fixture.0.join("dist").join(asset.filename());
      fs::create_dir_all(path.parent().context("output parent")?)?;
      fs::write(path, asset.content_as_bytes())?;
    }
  }
  Ok(())
}

fn dev_options(fixture: &Fixture) -> BundlerOptions {
  let mut options = options(fixture);
  options.treeshake = TreeshakeOptions::Boolean(false);
  options.experimental.as_mut().expect("native experimental options").dev_mode =
    Some(DevModeOptions {
      lazy: Some(false),
      implement: Some(
        "globalThis.__rolldown_runtime__ ??= new DevRuntime('fixture');\
       globalThis.__rolldown_runtime__.hooks = {\
         createModuleHotContext: () => ({ accept() {} }),\
         onModuleCacheRemoval() {}\
       };"
          .into(),
      ),
      ..Default::default()
    });
  options
}

#[test]
fn assembled_commonjs_helpers_retry_failures_and_keep_active_cycles() -> Result<()> {
  let runtime = fs::read_to_string(
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/runtime/native-runtime-base.js"),
  )?;
  let fixture = Fixture::new(&[("runtime.js", &runtime)])?;
  for helper in ["__commonJS", "__commonJSMin"] {
    let callback = if helper == "__commonJS" { "{init}" } else { "init" };
    fixture.write(
      "main.js",
      &format!(
        "import{{{helper} as make}}from'./runtime.js';\
       let attempts=0,failures=0,activeCycle,selfArgument;\
       function init(exports,module,self){{\
         attempts++;activeCycle=run()===exports;selfArgument=self===exports;\
         if(attempts===1){{exports.partial=1;throw Error('first attempt')}}\
         module.exports={{answer:42}};\
       }}\
       const run=make({callback},void 0,()=>failures++);\
       let message;try{{run()}}catch(error){{message=error.message}}\
       const retried=run();events.push([message,attempts,failures,activeCycle,\
         selfArgument,retried,run()===retried]);"
      ),
    )?;
    assert_eq!(
      fixture.execute("./main.js")?,
      json!([["first attempt",2,1,true,true,{"answer":42},true]]),
      "{helper} must discard failed exports without breaking active cycles"
    );
  }
  Ok(())
}

#[tokio::test]
async fn emitted_commonjs_retry_matches_node_and_preserves_cached_getters() -> Result<()> {
  let fixture = Fixture::new(&[
    (
      "retry.cjs",
      "globalThis.attempts=(globalThis.attempts??0)+1;exports.partial=1;\
       globalThis.activeCycle=require('./retry.cjs')===exports;\
       if(attempts===1)throw Error('first attempt');module.exports={answer:42};",
    ),
    (
      "getter.cjs",
      "globalThis.getterRuns=(globalThis.getterRuns??0)+1;\
       module.exports={get answer(){throw Error('getter')}};",
    ),
    (
      "main.cjs",
      "try{require('./retry.cjs')}catch(error){events.push(error.message)}\
       const value=require('./retry.cjs');\
       events.push(attempts,activeCycle,value,require('./retry.cjs')===value);\
       for(let i=0;i<2;i++)try{require('./getter.cjs').answer}catch(error){events.push(error.message)}\
       events.push(getterRuns);",
    ),
    ("observe.js", "await import('./dist/main.js');"),
  ])?;
  let expected = fixture.execute("./main.cjs")?;
  assert_eq!(expected, json!(["first attempt",2,true,{"answer":42},true,"getter","getter",1]));
  for native in [false, true] {
    for minify in [false, true] {
      let mut options = options(&fixture);
      options.input =
        Some(vec![InputItem { name: Some("main".into()), import: "./main.cjs".into() }]);
      options.minify = Some(RawMinifyOptions::Bool(minify));
      let mut bundler = Bundler::new(options)?;
      bundler.set_native_commonjs_records(native)?;
      fixture.output(&bundler.incremental_generate(ScanMode::Full).await?)?;
      assert_eq!(fixture.execute("./observe.js")?, expected, "native={native}, minify={minify}");
      bundler.close().await?;
    }
  }
  Ok(())
}

#[tokio::test]
async fn commonjs_factory_registration_does_not_evaluate_and_public_imports_share_identity()
-> Result<()> {
  let fixture = Fixture::new(&[
    (
      "main.js",
      "import value from'./a.cjs';events.push(value.answer);\
       const first=await import('./b.cjs'),second=await import('./b.cjs');\
       events.push(first===second,first.default===value);",
    ),
    ("a.cjs", "events.push('a');module.exports=require('./b.cjs');"),
    ("b.cjs", "events.push('b');exports.answer=42;"),
    ("observe.js", "await import('./dist/main.js');"),
  ])?;
  let expected = fixture.execute("./main.js")?;
  assert_eq!(expected, json!(["a", "b", 42, true, true]));
  let mut bundler = Bundler::new(options(&fixture))?;
  bundler.set_native_commonjs_records(true)?;
  let output = bundler.incremental_generate(ScanMode::Full).await?;
  fixture.output(&output)?;
  assert_eq!(fixture.execute("./observe.js")?, expected);
  fixture.write(
    "factory-observe.js",
    "const factory=await import('./dist/b.js.cjs-factory.js');\
     events.push('registered');const first=factory.__native_require();\
     const second=(await import('./dist/b.js')).default;events.push(first===second);",
  )?;
  assert_eq!(fixture.execute("./factory-observe.js")?, json!(["registered", "b", true]));
  for asset in &output.assets {
    if let Output::Chunk(chunk) = asset {
      for name in chunk.imports.iter().chain(&chunk.dynamic_imports) {
        assert!(
          output.assets.iter().any(|asset| asset.filename() == name),
          "missing import {name}"
        );
      }
    }
  }
  bundler.close().await?;
  Ok(())
}

#[tokio::test]
async fn commonjs_relinks_keep_order_identity_and_retire_factory_maps() -> Result<()> {
  for maps in [false, true] {
    for omit in [false, true] {
      let fixture = Fixture::new(&[
        ("main.js", "import a from'./a.cjs';import'./label.js';events.push(a.answer);"),
        ("a.cjs", "events.push('a');module.exports=require('./b.cjs');"),
        ("b.cjs", "events.push('b');exports.answer=Number('1');"),
        ("label.js", "events.push('label');"),
        ("child.js", "import'./b.cjs';import'./leaf.cjs';"),
        ("leaf.cjs", "events.push('leaf');exports.answer=7;"),
        ("observe.js", "await import('./dist/main.js');"),
      ])?;
      let mut options = dev_options(&fixture);
      options.sourcemap = maps.then_some(SourceMapType::File);
      let mut bundler = Bundler::new(options.clone())?;
      bundler.set_native_commonjs_records(true)?;
      bundler.set_omit_reused_chunk_bytes(omit);
      let mut retired_leaf = Vec::new();
      let mut saw_fast = false;
      for (round, (imported, value)) in
        [(false, 1), (true, 1), (true, 2), (false, 2), (true, 3), (false, 3)]
          .into_iter()
          .enumerate()
      {
        fixture.write(
          "label.js",
          &format!("{}events.push('label');", if imported { "import'./child.js';" } else { "" }),
        )?;
        fixture.write("b.cjs", &format!("events.push('b');exports.answer=Number('{value}');"))?;
        let scan = if round == 0 {
          ScanMode::Full
        } else {
          let mut changed = vec![fixture.0.join("label.js").to_string_lossy().as_ref().into()];
          if round == 2 || round == 4 {
            changed.push(fixture.0.join("b.cjs").to_string_lossy().as_ref().into());
          }
          ScanMode::Partial(changed)
        };
        let output = bundler.incremental_generate(scan).await?;
        saw_fast |= bundler.last_fast_native_relink();
        if imported {
          retired_leaf = output
            .assets
            .iter()
            .filter(|asset| asset.filename().starts_with("leaf.js"))
            .map(|asset| asset.filename().to_owned())
            .collect();
          assert_eq!(retired_leaf.len(), if maps { 3 } else { 2 }, "public/factory/map ownership");
        } else if round > 0 && omit {
          let change = bundler.last_native_graph_change().with_context(|| {
            format!(
              "retired graph delta round={round}, maps={maps}, omit={omit}, fallback={:?}: {:?}",
              bundler.last_native_relink_fallback(),
              bundler.last_native_relink_fallback_detail()
            )
          })?;
          for name in &retired_leaf {
            assert!(
              change.retired_filenames.iter().any(|retired| retired.as_str() == name),
              "round {round}: missing retirement {name}"
            );
          }
        }
        if omit {
          publish(&fixture, &output, &bundler)?;
        } else {
          fixture.output(&output)?;
        }
        if !imported {
          assert!(
            output.assets.iter().all(|asset| !asset.filename().starts_with("leaf.js")),
            "retired public/factory/map must disappear at round {round}"
          );
        }
        let mut fresh = Bundler::new(options.clone())?;
        fresh.set_native_commonjs_records(true)?;
        let expected = fresh.incremental_generate(ScanMode::Full).await?;
        let dist = fixture.0.join("dist");
        let mut directories = vec![dist.clone()];
        let mut actual = BTreeMap::new();
        while let Some(directory) = directories.pop() {
          for entry in fs::read_dir(directory)? {
            let path = entry?.path();
            if path.is_dir() {
              directories.push(path);
            } else {
              actual
                .insert(path.strip_prefix(&dist)?.to_string_lossy().into_owned(), fs::read(path)?);
            }
          }
        }
        let expected: BTreeMap<_, _> = expected
          .assets
          .iter()
          .map(|asset| (asset.filename().to_owned(), asset.content_as_bytes().to_vec()))
          .collect();
        assert_eq!(
          actual.keys().collect::<Vec<_>>(),
          expected.keys().collect::<Vec<_>>(),
          "published names: round={round}, maps={maps}, omit={omit}"
        );
        for (name, bytes) in &actual {
          assert_eq!(
            String::from_utf8_lossy(bytes),
            String::from_utf8_lossy(&expected[name]),
            "{name}: round={round}, maps={maps}, omit={omit}"
          );
        }
        fresh.close().await?;
        let expected = fixture.execute("./main.js")?;
        assert_eq!(
          fixture.execute("./observe.js")?,
          expected,
          "round={round}, maps={maps}, omit={omit}"
        );
      }
      assert_eq!(saw_fast, omit, "fast relinking requires omitted reused bytes");
      bundler.close().await?;
    }
  }
  Ok(())
}

#[tokio::test]
async fn native_commonjs_dev_wrappers_report_failures_and_reuse_replacements() -> Result<()> {
  let fixture = Fixture::new(&[
    ("main.js", "globalThis.load=()=>require('./dep.cjs');import.meta.hot.accept();"),
    (
      "dep.cjs",
      "globalThis.attempts=(globalThis.attempts??0)+1;\
      if(attempts===1){exports.partial=1;throw Error('first attempt')}\
      module.exports={answer:42};",
    ),
  ])?;
  for minify in [false, true] {
    let mut options = dev_options(&fixture);
    options.minify = Some(RawMinifyOptions::Bool(minify));
    let mut bundler = Bundler::new(options)?;
    bundler.set_native_commonjs_records(true)?;
    fixture.output(&bundler.incremental_generate(ScanMode::Full).await?)?;
    for replacement in [false, true] {
      fixture.write("observe.js",&format!(
        "import assert from'node:assert/strict';await import('./dist/main.js');\
         assert.equal(globalThis.attempts,undefined);\
         const runtime=globalThis.__rolldown_runtime__;let failedId,failures=0;\
         runtime.recordNativeCommonJsFailure=id=>{{failedId=id;failures++;runtime.moduleCache.delete(id)}};\
         assert.throws(()=>load(),/first attempt/);assert.equal(failures,1);\
         assert.ok(failedId.endsWith('dep.cjs'));assert.equal(runtime.moduleCache.has(failedId),false);\
         const replacement={{answer:99}};\
         if({replacement}){{\
           runtime.factories.set(failedId,id=>runtime.registerModule(id,{{exports:replacement}}));\
           assert.equal(runtime.initModule(failedId),replacement);\
         }}\
         const value=load();assert.equal(load(),value);\
         events.push(attempts,value.answer,value===replacement,failures);"
      ))?;
      assert_eq!(
        fixture.execute("./observe.js")?,
        if replacement { json!([1, 99, true, 1]) } else { json!([2, 42, false, 1]) },
        "minify={minify}, replacement={replacement}"
      );
    }
    bundler.close().await?;
  }
  Ok(())
}

#[tokio::test]
async fn native_commonjs_mode_rejects_unsupported_options_and_survives_full_scans() -> Result<()> {
  let fixture = Fixture::new(&[("main.js", "export const value=1;")])?;
  let mut invalid = Bundler::new(fixture.options())?;
  assert!(invalid.set_native_commonjs_records(true).is_err());
  invalid.set_native_commonjs_records(false)?;
  invalid.close().await?;
  let fixture = Fixture::new(&[
    ("main.js", "const value=require('./dep.cjs');events.push(value.answer);"),
    ("dep.cjs", "exports.answer=42;"),
  ])?;
  let mut bundler = Bundler::new(options(&fixture))?;
  bundler.set_native_commonjs_records(true)?;
  for scan in [ScanMode::Full, partial_scan(&fixture, "dep.cjs"), ScanMode::Full] {
    let output = bundler.incremental_generate(scan).await?;
    assert!(output.assets.iter().any(|asset| asset.filename().ends_with(".cjs-factory.js")));
  }
  bundler.set_native_commonjs_records(false)?;
  let output = bundler.incremental_generate(ScanMode::Partial(Vec::new())).await?;
  assert!(output.assets.iter().all(|asset| !asset.filename().contains(".cjs-factory.")));
  bundler.close().await?;
  Ok(())
}
