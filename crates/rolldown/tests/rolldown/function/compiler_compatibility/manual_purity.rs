use anyhow::Result;
use rolldown::{Bundler, BundlerOptions};
use rolldown_common::{
  CodeSplittingMode, InlineCommonChunksOptions, InnerOptions, InputItem,
  ManualCodeSplittingOptions, PreserveEntrySignatures, RawMinifyOptions, TreeshakeOptions,
};
use serde_json::{Value, json};

use super::Fixture;

fn options(fixture: &Fixture, inline: bool, minify: RawMinifyOptions) -> BundlerOptions {
  let mut options = fixture.options();
  options.input = Some(
    ["a", "b"]
      .into_iter()
      .map(|name| InputItem { name: Some(name.into()), import: format!("./{name}.js") })
      .collect(),
  );
  options.strict_execution_order = Some(true);
  options.preserve_entry_signatures = Some(PreserveEntrySignatures::False);
  options.minify = Some(minify);
  options.treeshake = TreeshakeOptions::Option(InnerOptions {
    manual_pure_functions: Some(std::iter::once("f".to_string()).collect()),
    ..Default::default()
  });
  options.code_splitting = Some(CodeSplittingMode::Advanced(ManualCodeSplittingOptions {
    experimental_inline_common_chunks: Some(InlineCommonChunksOptions {
      max_size: Some(if inline { 4096.0 } else { 0.0 }),
      exclude: None,
    }),
    ..Default::default()
  }));
  options
}

fn minify_modes() -> [RawMinifyOptions; 3] {
  [
    RawMinifyOptions::Bool(false),
    RawMinifyOptions::DeadCodeEliminationOnly,
    RawMinifyOptions::Bool(true),
  ]
}

async fn execute(fixture: &Fixture, options: BundlerOptions) -> Result<Value> {
  let mut bundler = Bundler::new(options)?;
  fixture.output(&bundler.generate().await?)?;
  bundler.close().await?;
  fixture.execute("./observe.js")
}

async fn check_cyclic_elision(operation: &str, inline: bool) -> Result<()> {
  for initialized in [false, true] {
    let fixture = Fixture::new(&[
      ("a.js", "import{f}from'./shared.js';events.push(['entry-a',typeof f]);"),
      ("b.js", "import{f}from'./shared.js';events.push(['entry-b',typeof f]);"),
      (
        "source-observe.js",
        "try{await import('./a.js')}catch(error){events.push(['error',error.name])}",
      ),
      (
        "observe.js",
        "try{await import('./dist/a.js')}catch(error){events.push(['error',error.name])}",
      ),
    ])?;
    fixture.write(
      "caller.js",
      &format!(
        "import{{f}}from'./shared.js';export function run(){{\
        events.push('before');{operation};events.push('after');}}"
      ),
    )?;
    let body =
      if initialized { "export const f=()=>{};run();" } else { "run();export const f=()=>{};" };
    fixture.write(
      "shared.js",
      &format!("import{{run}}from'./caller.js';{body}events.push('initialized');"),
    )?;
    let elided = json!(["before", "after", "initialized", ["entry-a", "function"]]);
    assert_eq!(
      fixture.execute("./source-observe.js")?,
      if initialized { elided.clone() } else { json!(["before", ["error", "ReferenceError"]]) },
      "direct Node reference, operation={operation}, initialized={initialized}"
    );
    for minify in minify_modes() {
      let compressed = matches!(minify, RawMinifyOptions::Bool(true));
      let mut bundler = Bundler::new(options(&fixture, inline, minify.clone()))?;
      let output = bundler.generate().await?;
      if !compressed {
        assert_eq!(
          output.assets.iter().any(|asset| {
            String::from_utf8_lossy(asset.content_as_bytes()).contains("share_shared")
          }),
          inline,
          "execute an actual shared record, inline={inline}"
        );
      }
      fixture.output(&output)?;
      bundler.close().await?;
      // Explicit manual purity preserves the established elision contract, not Node's TDZ.
      assert_eq!(
        fixture.execute("./observe.js")?,
        elided,
        "operation={operation}, initialized={initialized}, inline={inline}, minify={minify:?}"
      );
    }
  }
  Ok(())
}

#[tokio::test]
async fn manual_pure_cyclic_call_elides_without_compression() -> Result<()> {
  check_cyclic_elision("f()", false).await
}

#[tokio::test]
async fn manual_pure_cyclic_call_elides_in_shared_records() -> Result<()> {
  check_cyclic_elision("f()", true).await
}

#[tokio::test]
async fn manual_pure_cyclic_tag_elides_without_compression() -> Result<()> {
  check_cyclic_elision("f`tag`", false).await
}

#[tokio::test]
async fn manual_pure_cyclic_tag_elides_in_shared_records() -> Result<()> {
  check_cyclic_elision("f`tag`", true).await
}

#[tokio::test]
async fn manual_purity_keeps_unmarked_and_argument_temporal_reads() -> Result<()> {
  for source in [
    "try{unmarked()}catch(error){events.push(error.name)}\
      const unmarked=()=>{events.push('called')};unmarked();",
    "try{unmarked`tag`}catch(error){events.push(error.name)}\
      const unmarked=()=>{events.push('called')};unmarked`tag`;",
    "function f(){}try{f(later)}catch(error){events.push(error.name)}\
      const later=1;f(later);events.push('called');",
    "function f(){}try{f?.(later)}catch(error){events.push(error.name)}\
      const later=1;f?.(later);events.push('called');",
    "function f(){}try{f`${later}`}catch(error){events.push(error.name)}\
      const later=1;f`${later}`;events.push('called');",
  ] {
    let fixture =
      Fixture::new(&[("main.cjs", source), ("observe.js", "await import('./dist/main.js');")])?;
    let expected = fixture.execute("./main.cjs")?;
    assert_eq!(expected, json!(["ReferenceError", "called"]), "{source}");
    for minify in minify_modes() {
      let mut options = options(&fixture, false, minify.clone());
      options.input =
        Some(vec![InputItem { name: Some("main".into()), import: "./main.cjs".into() }]);
      assert_eq!(execute(&fixture, options).await?, expected, "source={source}, minify={minify:?}");
    }
  }
  Ok(())
}

#[tokio::test]
async fn manual_purity_keeps_authored_callee_arguments_keys_and_iterations() -> Result<()> {
  for operation in [
    "f(events.push('argument'));",
    "f[events.push('key'),'method'](events.push('outer'));",
    "f`${events.push('substitution')}`;",
    "const values={[Symbol.iterator](){events.push('iterate');return [1,2][Symbol.iterator]()}};\
      f(...values);",
    "function fail(){events.push('argument');throw Error('failure')}\
      try{f(fail())}catch(error){events.push(error.message)}",
    "g(events.push('argument')).method();",
  ] {
    let fixture = Fixture::new(&[
      (
        "shared.js",
        "events.push('owner');export const f=Object.assign(()=>({method(){}}),{method(){}});\
        export const g=()=>{events.push('unmarked-body');\
          return{method(){events.push('unmarked-method')}}};",
      ),
      ("source-observe.js", "await import('./a.js');"),
      ("observe.js", "await import('./dist/a.js');"),
    ])?;
    for entry in ["a", "b"] {
      fixture
        .write(&format!("{entry}.js"), &format!("import{{f,g}}from'./shared.js';{operation}"))?;
    }
    let expected = fixture.execute("./source-observe.js")?;
    for inline in [false, true] {
      for minify in minify_modes() {
        assert_eq!(
          execute(&fixture, options(&fixture, inline, minify.clone())).await?,
          expected,
          "operation={operation}, inline={inline}, minify={minify:?}"
        );
      }
    }
  }
  Ok(())
}

#[tokio::test]
async fn manual_purity_does_not_hide_arguments_inside_a_callee_chain() -> Result<()> {
  for operation in [
    "f(events.push('inner')).method(events.push('outer'));",
    "const values={[Symbol.iterator](){events.push('iterate');return[1,2][Symbol.iterator]()}};\
      f(...values).method(events.push('outer'));",
  ] {
    let fixture = Fixture::new(&[
      ("shared.js", "events.push('owner');export const f=()=>({method(){}});"),
      ("source-observe.js", "await import('./a.js');"),
      ("observe.js", "await import('./dist/a.js');"),
    ])?;
    for entry in ["a", "b"] {
      fixture
        .write(&format!("{entry}.js"), &format!("import{{f}}from'./shared.js';{operation}"))?;
    }
    let expected = fixture.execute("./source-observe.js")?;
    for inline in [false, true] {
      assert_eq!(
        execute(&fixture, options(&fixture, inline, RawMinifyOptions::Bool(false))).await?,
        expected,
        "operation={operation}, inline={inline}"
      );
    }
  }
  Ok(())
}
