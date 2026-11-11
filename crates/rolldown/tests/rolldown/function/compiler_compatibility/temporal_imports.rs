use anyhow::Result;
use rolldown::{Bundler, BundlerOptions};
use rolldown_common::{
  CodeSplittingMode, InlineCommonChunksOptions, InputItem, ManualCodeSplittingOptions,
  OptimizationOption, PreserveEntrySignatures, RawMinifyOptions,
};
use serde_json::json;

use super::Fixture;

fn namespace_fixture(main: &str) -> Result<Fixture> {
  Fixture::new(&[
    ("main.js", main),
    ("root.js", "export * as default from './one.js';export * as named from './one.js';"),
    ("one.js", "export * as one from './two.js';"),
    ("two.js", "export * as two from './dep.js';"),
    ("dep.js", "export const value=1;export const object={present:2,nested:{present:3}};"),
  ])
}

async fn execute(fixture: &Fixture, options: BundlerOptions) -> Result<serde_json::Value> {
  let mut bundler = Bundler::new(options)?;
  fixture.output(&bundler.generate().await?)?;
  bundler.close().await?;
  fixture.execute("./dist/main.js")
}

#[tokio::test]
async fn temporal_namespace_reads_keep_deep_default_named_and_computed_shims() -> Result<()> {
  let fixture = namespace_fixture(
    "import ns,{named}from'./root.js';import * as star from './one.js';
    events.push(ns.one.two.missing,named.one.two['computed'],star.one.two?.optional);
    events.push(ns.one.two.object.absent,ns.one.two.object.nested.absent);
    const identity=x=>x;events.push(identity(ns.one.two).callBoundary);
    events.push(Object.keys(ns.one.two).sort(),Object.keys(ns.one.two.object).sort(),
      Object.keys(ns.one.two.object.nested));",
  )?;
  for strict in [false, true] {
    for minify in [false, true] {
      let mut options = fixture.options();
      options.shim_missing_exports = Some(true);
      options.strict_execution_order = Some(strict);
      options.minify = Some(RawMinifyOptions::Bool(minify));
      assert_eq!(
        execute(&fixture, options).await?,
        json!([
          null,
          null,
          null,
          null,
          null,
          null,
          ["computed", "missing", "object", "optional", "value"],
          ["nested", "present"],
          ["present"]
        ]),
        "strict={strict}, minify={minify}"
      );
    }
  }
  Ok(())
}

#[tokio::test]
async fn temporal_namespace_object_writes_match_native_execution() -> Result<()> {
  let fixture = namespace_fixture(
    "import ns from './root.js';
    events.push(delete ns.one.two.deleted);
    ns.one.two.object.written=4;ns.one.two.object.nested.written=5;
    events.push(Object.keys(ns.one.two).sort(),ns.one.two.object.written,
      ns.one.two.object.nested.written);",
  )?;
  let expected = fixture.execute("./main.js")?;
  assert_eq!(expected, json!([true, ["object", "value"], 4, 5]));
  for strict in [false, true] {
    for minify in [false, true] {
      let mut options = fixture.options();
      options.shim_missing_exports = Some(true);
      options.strict_execution_order = Some(strict);
      options.minify = Some(RawMinifyOptions::Bool(minify));
      assert_eq!(execute(&fixture, options).await?, expected, "strict={strict}, minify={minify}");
    }
  }
  Ok(())
}

#[tokio::test]
async fn temporal_namespace_writes_keep_the_import_assignment_diagnostic() -> Result<()> {
  let fixture = namespace_fixture(
    "import ns from './root.js';ns.one.two.written=7;ns.one.two['computedWrite']=8;",
  )?;
  for strict in [false, true] {
    for minify in [false, true] {
      let mut options = fixture.options();
      options.shim_missing_exports = Some(true);
      options.strict_execution_order = Some(strict);
      options.minify = Some(RawMinifyOptions::Bool(minify));
      let mut bundler = Bundler::new(options)?;
      let error = bundler.generate().await.err().expect("cannot write a namespace export");
      bundler.close().await?;
      let message = error.to_string();
      assert!(message.contains("Cannot assign to import 'written'"), "{message}");
      assert!(message.contains("Cannot assign to import 'computedWrite'"), "{message}");
    }
  }
  Ok(())
}

#[tokio::test]
async fn temporal_import_calls_optional_calls_and_tags_keep_their_receivers_when_inlined()
-> Result<()> {
  let fixture = Fixture::new(&[
    ("main.js", "await import('./a.js');await import('./b.js');"),
    (
      "shared.js",
      "export function read(){return this===undefined}
      export const object={method(){return this===object}};",
    ),
    (
      "a.js",
      "import{read,object}from'./shared.js';
      events.push(['a',read(),read?.(),read`tag`,object.method(),(0,object.method)()]);",
    ),
    (
      "b.js",
      "import{read,object}from'./shared.js';
      events.push(['b',read(),(read)?.(),(read)`tag`,object.method(),(0,object.method)()]);",
    ),
    ("observe.js", "await import('./dist/a.js');await import('./dist/b.js');"),
  ])?;
  let expected = fixture.execute("./main.js")?;
  assert_eq!(
    expected,
    json!([["a", true, true, true, true, false], ["b", true, true, true, true, false]])
  );
  for inline in [false, true] {
    for pife in [false, true] {
      for minify in [false, true] {
        let mut options = fixture.options();
        options.input = Some(
          ["a", "b"]
            .into_iter()
            .map(|name| InputItem { name: Some(name.into()), import: format!("./{name}.js") })
            .collect(),
        );
        options.preserve_entry_signatures = Some(PreserveEntrySignatures::False);
        options.strict_execution_order = Some(true);
        options.optimization =
          Some(OptimizationOption { pife_for_module_wrappers: Some(pife), ..Default::default() });
        options.minify = Some(RawMinifyOptions::Bool(minify));
        options.code_splitting = Some(CodeSplittingMode::Advanced(ManualCodeSplittingOptions {
          experimental_inline_common_chunks: Some(InlineCommonChunksOptions {
            max_size: Some(if inline { 4096.0 } else { 0.0 }),
            exclude: None,
          }),
          ..Default::default()
        }));
        let mut bundler = Bundler::new(options)?;
        let output = bundler.generate().await?;
        if !minify {
          assert_eq!(
            output.assets.iter().any(|asset| {
              String::from_utf8_lossy(asset.content_as_bytes()).contains("share_shared")
            }),
            inline,
            "exercise the actual inline bridge, inline={inline}, pife={pife}"
          );
        }
        fixture.output(&output)?;
        bundler.close().await?;
        assert_eq!(
          fixture.execute("./observe.js")?,
          expected,
          "inline={inline}, pife={pife}, minify={minify}"
        );
      }
    }
  }
  Ok(())
}
