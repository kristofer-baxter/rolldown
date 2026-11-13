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

#[tokio::test]
async fn define_introduced_static_require_requests_execute_in_native_records() -> Result<()> {
  check_defined_static_requests("require").await
}

#[tokio::test]
async fn define_introduced_static_dynamic_import_requests_execute_in_native_records() -> Result<()>
{
  check_defined_static_requests("import").await
}

async fn check_defined_static_requests(kind: &str) -> Result<()> {
  for (request, filename) in [
    ("'./dep.cjs'", "dep.cjs"),
    ("'./' + 'dep.cjs'", "dep.cjs"),
    ("('./' + 'dep') + `.cjs`", "dep.cjs"),
    ("(`./` + ('dep' + `.cjs`))", "dep.cjs"),
    ("'./\\uD83D' + '\\uDE00.cjs'", "😀.cjs"),
    ("'./\\uD800' + '.cjs'", "�.cjs"),
  ] {
    for (dev, native) in [(true, true), (false, true), (false, false)] {
      for treeshake in [false, true] {
        let value = format!("{kind}({request})");
        let read = if kind == "require" {
          "globalThis.read=()=>VALUE+0;"
        } else {
          "globalThis.read=async()=>(await VALUE).default+0;"
        };
        let source = format!("{read}{}", if dev { "import.meta.hot.accept();" } else { "" });
        let fixture = Fixture::new(&[
          ("main.mjs", &source),
          (filename, "module.exports=1;globalThis.depRuns=(globalThis.depRuns??0)+1;"),
          (
            "observe.js",
            "await import('./dist/main.js');events.push(await read(),await read(),depRuns);",
          ),
        ])?;
        let mut options = if dev { dev_options(&fixture) } else { options(&fixture) };
        options.input.as_mut().expect("entry")[0].import = "./main.mjs".into();
        options.treeshake = TreeshakeOptions::Boolean(treeshake);
        options.define = Some(std::iter::once(("VALUE".to_owned(), value.clone())).collect());
        let mut bundler = Bundler::new(options)?;
        bundler.set_native_commonjs_records(native)?;
        let case = format!("{value}, dev={dev}, native={native}, treeshake={treeshake}");
        for version in [1, 2] {
          let scan = if version == 1 {
            ScanMode::Full
          } else {
            fixture
              .write(filename, "module.exports=2;globalThis.depRuns=(globalThis.depRuns??0)+1;")?;
            partial_scan(&fixture, filename)
          };
          let output = bundler.incremental_generate(scan).await?;
          publish(&fixture, &output, &bundler)?;
          assert_eq!(
            fixture.execute("./observe.js").with_context(|| case.clone())?,
            json!([version, version, 1]),
            "{case}, version={version}"
          );
        }
        bundler.close().await?;
      }
    }
  }
  Ok(())
}

#[tokio::test]
async fn define_introduced_runtime_requests_preserve_specifier_evaluation() -> Result<()> {
  for (request, effects) in [
    ("'./' + globalThis.depPath", Vec::new()),
    ("(events.push('specifier'),'./dep.cjs')", vec!["specifier"]),
    ("'./' + (events.push('specifier'),'dep.cjs')", vec!["specifier"]),
    ("'./' + {toString(){events.push('coerce');return 'dep.cjs'}}", vec!["coerce"]),
    ("(()=>{events.push('specifier');return './dep.cjs'})()", vec!["specifier"]),
    ("('./dep.cjs','./dep.cjs')", Vec::new()),
    ("`./${'dep'}.cjs`", Vec::new()),
    ("'./' + ['dep.cjs']", Vec::new()),
  ] {
    for kind in ["require", "import"] {
      let mut expected = effects.iter().copied().map(serde_json::Value::from).collect::<Vec<_>>();
      expected.push(json!("runtime"));
      if kind == "require" {
        expected.push(json!("./dep.cjs"));
      }
      expected.push(json!(9));
      check_defined_runtime_request(kind, &format!("{kind}({request})"), json!(expected)).await?;
    }
  }
  for (value, expected) in [
    ("require?.('./'+'dep.cjs')", json!(["runtime", "./dep.cjs", 9])),
    ("require(...['./'+'dep.cjs'])", json!(["runtime", "./dep.cjs", 9])),
    ("globalThis.require('./'+'dep.cjs')", json!(["runtime", "./dep.cjs", 9])),
    (
      "((require)=>require('./'+'dep.cjs'))(globalThis.require)",
      json!(["runtime", "./dep.cjs", 9]),
    ),
    (
      "require('./'+'dep.cjs',events.push('argument'))",
      json!(["argument", "runtime", "./dep.cjs", 9]),
    ),
  ] {
    check_defined_runtime_request("require", value, expected).await?;
  }
  check_defined_runtime_request("ignored-import", "'./'+'dep.cjs'", json!(["runtime", 9])).await?;
  Ok(())
}

async fn check_defined_runtime_request(
  kind: &str,
  value: &str,
  expected: serde_json::Value,
) -> Result<()> {
  for native in [true, false] {
    let read = match kind {
      "require" => "globalThis.read=()=>VALUE+0;",
      "ignored-import" => {
        "globalThis.read=async()=>(await import(/* @vite-ignore */ VALUE)).default+0;"
      }
      _ => "globalThis.read=async()=>(await VALUE).default+0;",
    };
    let fixture = Fixture::new(&[
      ("main.mjs", &format!("{read}import.meta.hot.accept();")),
      ("dep.cjs", "throw Error('runtime-only target must not enter the graph');"),
      (
        "observe.js",
        "globalThis.depPath='dep.cjs';\
         globalThis.require=request=>{events.push('runtime',String(request));return 9};\
         await import('./dist/main.js');events.push(await read());",
      ),
    ])?;
    let mut options = dev_options(&fixture);
    options.input.as_mut().expect("entry")[0].import = "./main.mjs".into();
    options.define = Some(std::iter::once(("VALUE".to_owned(), value.to_owned())).collect());
    let mut bundler = Bundler::new(options)?;
    bundler.set_native_commonjs_records(native)?;
    let output = bundler.incremental_generate(ScanMode::Full).await?;
    assert!(
      output.assets.iter().all(|output| !matches!(output, Output::Chunk(chunk)
        if chunk.facade_module_id.as_ref().is_some_and(|id| id.ends_with("dep.cjs")))),
      "{value}, native={native} must remain a runtime request"
    );
    fixture.output(&output)?;
    fixture.write("dist/dep.cjs", "events.push('runtime');module.exports=9;")?;
    assert_eq!(fixture.execute("./observe.js")?, expected, "{value}, native={native}");
    bundler.close().await?;
  }
  Ok(())
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
async fn unused_commonjs_imports_do_not_evaluate_deferred_requires() -> Result<()> {
  for native in [false, true] {
    for treeshake in [false, true] {
      for minify in [false, true] {
        for pure in [false, true] {
          for import in [
            "",
            "import unused from './lib/dep.cjs';",
            "import './lib/dep.cjs';",
            "import used from './lib/dep.cjs';export const eager=used;",
          ] {
            let fixture = Fixture::new(&[
              ("main.js", &format!("{import}export const load=()=>require('./lib/dep.cjs');")),
              ("lib/package.json", &format!(r#"{{"sideEffects":{}}}"#, !pure)),
              ("lib/dep.cjs", "events.push('dep');module.exports=42;"),
              (
                "observe.js",
                "const main=await import('./dist/main.js');events.push('initial');\
                 events.push(main.load(),main.load());",
              ),
            ])?;
            let mut options = options(&fixture);
            options.treeshake = TreeshakeOptions::Boolean(treeshake);
            options.minify = Some(RawMinifyOptions::Bool(minify));
            let mut bundler = Bundler::new(options)?;
            bundler.set_native_commonjs_records(native)?;
            fixture.output(&bundler.incremental_generate(ScanMode::Full).await?)?;
            let actual = fixture.execute("./observe.js")?;
            // Opt-out records keep their existing evaluating default export.
            let eager =
              !import.is_empty() && (!native || !treeshake || !pure || import.contains("export"));
            let expected = if eager {
              json!(["dep", "initial", 42, 42])
            } else {
              json!(["initial", "dep", 42, 42])
            };
            assert_eq!(
              actual, expected,
              "native={native}, treeshake={treeshake}, minify={minify}, pure={pure}, import={import:?}"
            );
            bundler.close().await?;
          }
        }
      }
    }
  }
  Ok(())
}

#[tokio::test]
async fn unused_commonjs_imports_follow_retained_edges_across_relinks() -> Result<()> {
  for omit in [false, true] {
    let fixture = Fixture::new(&[
      ("main.js", ""),
      ("lib/package.json", r#"{"sideEffects":false}"#),
      ("lib/dep.cjs", "events.push('dep');module.exports=42;"),
      (
        "observe.js",
        "const main=await import('./dist/main.js');events.push('initial');\
         events.push(main.load(),main.load());",
      ),
    ])?;
    let mut options = options(&fixture);
    options.treeshake = TreeshakeOptions::Boolean(true);
    let mut bundler = Bundler::new(options)?;
    bundler.set_native_commonjs_records(true)?;
    bundler.set_omit_reused_chunk_bytes(omit);
    for (round, import) in [
      "import unused from './lib/dep.cjs';",
      "import used from './lib/dep.cjs';export const eager=used;",
      "import unused from './lib/dep.cjs';",
      "",
      "import './lib/dep.cjs';",
      "import used from './lib/dep.cjs';export const eager=used;",
    ]
    .into_iter()
    .enumerate()
    {
      fixture
        .write("main.js", &format!("{import}export const load=()=>require('./lib/dep.cjs');"))?;
      let scan = if round == 0 { ScanMode::Full } else { partial_scan(&fixture, "main.js") };
      publish(&fixture, &bundler.incremental_generate(scan).await?, &bundler)?;
      let expected = if import.contains("export") {
        json!(["dep", "initial", 42, 42])
      } else {
        json!(["initial", "dep", 42, 42])
      };
      assert_eq!(fixture.execute("./observe.js")?, expected, "round={round}, omit={omit}");
    }
    bundler.close().await?;
  }
  Ok(())
}

#[tokio::test]
async fn mixed_commonjs_imports_and_requires_match_node() -> Result<()> {
  for imports_first in [false, true] {
    for import in ["import value,* as namespace from './dep.cjs';", "import './dep.cjs';"] {
      let require = "events.push('body');const required=require('./dep.cjs');";
      let observe = if import.contains("namespace") {
        "events.push(value===required,namespace.default===required);"
      } else {
        ""
      };
      let source = format!(
        "{}{}events.push(required.answer,required.activeCycle);{observe}\
         events.push((await import('./dep.cjs')).default===required);",
        if imports_first { import } else { require },
        if imports_first { require } else { import },
      );
      let fixture = Fixture::new(&[
        ("main.js", &source),
        (
          "dep.cjs",
          "events.push('dep');exports.answer=42;\
           exports.activeCycle=require('./cycle.cjs').active;",
        ),
        ("cycle.cjs", "exports.active=require('./dep.cjs').answer===42;"),
        (
          "reference.js",
          &format!(
            "import{{createRequire}}from'node:module';const require=createRequire(import.meta.url);{source}"
          ),
        ),
        ("observe.js", "await import('./dist/main.js');"),
      ])?;
      let expected = fixture.execute("./reference.js")?;
      assert_eq!(expected[0], "dep", "static evaluation precedes body in both source orders");
      for native in [false, true] {
        for minify in [false, true] {
          let mut options = options(&fixture);
          options.minify = Some(RawMinifyOptions::Bool(minify));
          let mut bundler = Bundler::new(options)?;
          bundler.set_native_commonjs_records(native)?;
          let output = bundler.incremental_generate(ScanMode::Full).await?;
          fixture.output(&output)?;
          assert_eq!(
            fixture.execute("./observe.js")?,
            expected,
            "imports_first={imports_first}, import={import}, native={native}, minify={minify}"
          );
          bundler.close().await?;
        }
      }
    }
  }
  Ok(())
}

#[tokio::test]
async fn mixed_commonjs_import_failures_leave_require_retries_usable() -> Result<()> {
  for imports_first in [false, true] {
    let import = "import value from './dep.cjs';";
    let require = "events.push('body');const required=require('./dep.cjs');";
    let source = format!(
      "{}{}events.push(required===value);",
      if imports_first { import } else { require },
      if imports_first { require } else { import },
    );
    let fixture = Fixture::new(&[
      ("main.js", &source),
      (
        "reference-main.js",
        &format!(
          "import{{createRequire}}from'node:module';const require=createRequire(import.meta.url);{source}"
        ),
      ),
      (
        "dep.cjs",
        "globalThis.attempts=(globalThis.attempts??0)+1;events.push(`dep:${attempts}`);\
         exports.partial=true;exports.active=require('./cycle.cjs').active;\
         if(attempts===1)throw Error('first attempt');exports.answer=42;",
      ),
      ("cycle.cjs", "exports.active=require('./dep.cjs').partial===true;"),
      ("retry.cjs", "const value=require('./dep.cjs');events.push(value.answer,value.active);"),
      (
        "reference.js",
        "try{await import('./reference-main.js')}catch(error){events.push(error.message)}\
         await import('./retry.cjs');",
      ),
      (
        "observe.js",
        "try{await import('./dist/main.js')}catch(error){events.push(error.message)}\
         await import('./dist/retry.js');",
      ),
    ])?;
    let expected = fixture.execute("./reference.js")?;
    assert_eq!(expected, json!(["dep:1", "first attempt", "dep:2", 42, true]));
    let mut options = options(&fixture);
    options.input = Some(vec![
      InputItem { name: Some("main".into()), import: "./main.js".into() },
      InputItem { name: Some("retry".into()), import: "./retry.cjs".into() },
    ]);
    let mut bundler = Bundler::new(options)?;
    bundler.set_native_commonjs_records(true)?;
    fixture.output(&bundler.incremental_generate(ScanMode::Full).await?)?;
    assert_eq!(fixture.execute("./observe.js")?, expected, "imports_first={imports_first}");
    bundler.close().await?;
  }
  Ok(())
}

#[tokio::test]
async fn mixed_commonjs_imports_match_node_across_relinks() -> Result<()> {
  for omit in [false, true] {
    let fixture = Fixture::new(&[
      ("main.js", ""),
      ("dep.cjs", "events.push('dep');exports.answer=42;"),
      ("observe.js", "await import('./dist/main.js');"),
    ])?;
    let mut bundler = Bundler::new(dev_options(&fixture))?;
    bundler.set_native_commonjs_records(true)?;
    bundler.set_omit_reused_chunk_bytes(omit);
    for (round, (import, require)) in
      [(true, false), (true, true), (false, true), (true, true), (true, false)]
        .into_iter()
        .enumerate()
    {
      let source = format!(
        "{}events.push('body');{}events.push({});",
        if import { "import value from './dep.cjs';" } else { "" },
        if require { "const required=require('./dep.cjs');" } else { "" },
        if import && require {
          "value===required,required.answer"
        } else if require {
          "required.answer"
        } else {
          "value.answer"
        },
      );
      fixture.write("main.js", &source)?;
      fixture.write(
        "reference.js",
        &format!("import{{createRequire}}from'node:module';const require=createRequire(import.meta.url);{source}"),
      )?;
      let expected = fixture.execute("./reference.js")?;
      let scan = if round == 0 { ScanMode::Full } else { partial_scan(&fixture, "main.js") };
      let output = bundler.incremental_generate(scan).await?;
      publish(&fixture, &output, &bundler)?;
      assert_eq!(fixture.execute("./observe.js")?, expected, "round={round}, omit={omit}");
    }
    bundler.close().await?;
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

#[tokio::test]
async fn namespace_of_commonjs_reexport_takes_no_star_record_from_required_module() -> Result<()> {
  let main = "import * as ns from './r.cjs';events.push('main');\
              events.push(Object.keys(ns).sort(),ns.a.from);";
  let past_the_end = [
    ("r.cjs", "module.exports = require('./e.js');"),
    ("e.js", "import './p.js'; import './q.js'; export * from './x.js';"),
    ("p.js", "events.push('p');"),
    ("q.js", "events.push('q');"),
    ("x.js", "events.push('x'); export const a = { from: 'x' };"),
  ];
  let another_record = [
    ("r.cjs", "require('./s.js'); module.exports = require('./e.js');"),
    ("s.js", "events.push('s'); export * from './y.js';"),
    ("y.js", "events.push('y'); export const a = { from: 'y' };"),
    ("e.js", "events.push('e'); export * from './x.js';"),
    ("x.js", "events.push('x'); export const a = { from: 'x' };"),
  ];
  for (files, expected_events) in [
    (&past_the_end[..], json!(["p", "q", "x", "main", ["a", "default"], "x"])),
    (&another_record[..], json!(["y", "s", "x", "e", "main", ["a", "default"], "x"])),
  ] {
    for strict_only in [false, true] {
      let mut entries = vec![("main.js", main), ("observe.js", "await import('./dist/main.js');")];
      entries.extend_from_slice(files);
      let fixture = Fixture::new(&entries)?;
      let mut opts = fixture.options();
      opts.strict_execution_order = Some(true);
      opts.chunk_filenames = Some("[name].js".to_owned().into());
      if !strict_only {
        opts.experimental =
          Some(ExperimentalOptions { on_demand_wrapping: Some(true), ..Default::default() });
      }
      let mut bundler = Bundler::new(opts)?;
      let output = bundler.generate().await?;
      fixture.output(&output)?;
      assert_eq!(fixture.execute("./observe.js")?, expected_events);
      bundler.close().await?;
    }
  }
  Ok(())
}

#[tokio::test]
async fn commonjs_reexport_self_referential_cycle_terminates_and_evaluates() -> Result<()> {
  let cases = [
    (
      "import * as ns from './m1.cjs';events.push(typeof ns.a,ns.b);",
      "import { a } from './m1.cjs'; export { a }; export const b = 'b';",
      json!(["undefined", "b"]),
    ),
    (
      "import { a, b } from './m1.cjs';events.push(typeof a,b);",
      "import { a } from './m1.cjs'; export { a }; export * from './m1.cjs'; export const b = 'b';",
      json!(["undefined", "b"]),
    ),
    (
      "const ns = await import('./m1.cjs');events.push(typeof ns.a,ns.b);",
      "import { a } from './m1.cjs'; export { a }; export const b = 'b';",
      json!(["undefined", "b"]),
    ),
  ];
  for (main, m2, expected) in cases {
    let fixture = Fixture::new(&[
      ("main.js", main),
      ("m1.cjs", "module.exports = require('./m2.mjs');"),
      ("m2.mjs", m2),
      ("observe.js", "await import('./dist/main.js');"),
    ])?;
    let mut opts = fixture.options();
    opts.strict_execution_order = Some(true);
    opts.chunk_filenames = Some("[name].js".to_owned().into());
    opts.experimental =
      Some(ExperimentalOptions { on_demand_wrapping: Some(true), ..Default::default() });
    let mut bundler = Bundler::new(opts)?;
    let output = tokio::time::timeout(std::time::Duration::from_secs(30), bundler.generate())
      .await
      .context("CJS re-export cycle link timed out")??;
    fixture.output(&output)?;
    assert_eq!(fixture.execute("./observe.js")?, expected);
    bundler.close().await?;
  }
  Ok(())
}
