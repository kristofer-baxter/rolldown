use super::*;

fn locale_fixture() -> Result<Fixture> {
  Fixture::new(&[
    (
      "main.js",
      "import './register.js';import{tokens}from'./tokens.js';\
       import{makeStyles,hydrateShell}from'./styles.js';\
       import{verifyLocales}from'./context-registry.js';\
       globalThis.order.push('main');\
       export const boot=`${hydrateShell()}:${verifyLocales()}:${tokens.brand}`;\
       export const load=()=>import('./mail.js');\
       export const loadLocale=()=>import('./en-US.js');",
    ),
    ("register.js", "globalThis.order.push('register');export const CONTRACT='example.di';"),
    ("tokens.js", "export const tokens=Object.freeze({brand:'#0f6cbd'});"),
    (
      "styles.js",
      "import{tokens}from'./tokens.js';\
       export function makeStyles(rules){return()=>`${Object.keys(rules).join(',')}:${tokens.brand}`}\
       export const useShellStyles=makeStyles({root:true});\
       export function hydrateShell(){globalThis.order.push('styles');return useShellStyles()}",
    ),
    (
      "en-US.js",
      "export const localeStrings=Object.freeze({locale:'en-US',strings:{title:'Workspace'}});\
       export default localeStrings;",
    ),
    (
      "context-registry.js",
      "export function verifyLocales(){const mod=require('./en-US.js');return mod.localeStrings.locale}",
    ),
    (
      "mail.js",
      "import{tokens}from'./tokens.js';import{makeStyles}from'./styles.js';\
       export const useMailStyles=makeStyles({mail:tokens.brand});\
       export function activateMail(){globalThis.order.push('mail');return useMailStyles()}",
    ),
  ])
}

fn sole_root_options(fixture: &Fixture, minify: bool, on_demand: bool) -> BundlerOptions {
  let mut options = fixture.options();
  options.strict_execution_order = Some(true);
  options.preserve_entry_signatures = Some(PreserveEntrySignatures::AllowExtension);
  options.minify = Some(RawMinifyOptions::Bool(minify));
  options.experimental = Some(ExperimentalOptions {
    chunk_optimization: Some(ChunkOptimizationOption::Bool(true)),
    on_demand_wrapping: Some(on_demand),
    ..Default::default()
  });
  options
}

#[tokio::test]
async fn sole_root_colocated_required_locale_keeps_deferred_modules_unwrapped() -> Result<()> {
  let fixture = locale_fixture()?;
  let observe = |entry: &str| {
    format!(
      "globalThis.require=(await import('node:module')).createRequire(import.meta.url);\
       globalThis.order=[];const main=await import({entry:?});\
       events.push([...order]);const mail=await main.load();\
       const locale=await main.loadLocale();\
       events.push({{boot:main.boot,mail:mail.activateMail(),locale:locale.default.locale,order}});"
    )
  };
  fixture.write("observe-reference.js", &observe("./main.js"))?;
  fixture.write("observe-bundle.js", &observe("./dist/main.js"))?;
  let reference = fixture.execute("./observe-reference.js")?;
  assert_eq!(
    reference,
    serde_json::json!([
      ["register", "main", "styles"],
      {
        "boot": "root:#0f6cbd:en-US:#0f6cbd",
        "mail": "mail:#0f6cbd",
        "locale": "en-US",
        "order": ["register", "main", "styles", "mail"]
      }
    ])
  );
  for minify in [true, false] {
    for on_demand in [true, false] {
      let mut bundler = Bundler::new(sole_root_options(&fixture, minify, on_demand))?;
      let output = bundler.generate().await?;
      bundler.close().await?;
      fixture.output(&output)?;
      assert_eq!(
        fixture.execute("./observe-bundle.js")?,
        reference,
        "minify={minify}, on_demand={on_demand}"
      );
      let mail = output
        .assets
        .iter()
        .find_map(|asset| match asset {
          Output::Chunk(chunk) if chunk.filename.contains("mail") => Some(chunk),
          _ => None,
        })
        .context("deferred mail chunk")?;
      let wrapped = mail.code.contains("_esm") || mail.code.contains("(()=>");
      assert_eq!(
        wrapped, !on_demand,
        "deferred-module wrapper, minify={minify}, on_demand={on_demand}:\n{}",
        mail.code
      );
    }
  }
  Ok(())
}

#[tokio::test]
async fn sole_root_colocated_call_time_require_retains_its_lazy_wrapper() -> Result<()> {
  let fixture = locale_fixture()?;
  fixture.write(
    "main.js",
    "import './register.js';import{tokens}from'./tokens.js';\
     import{hydrateShell}from'./styles.js';\
     export{verifyLocales}from'./context-registry.js';\
     globalThis.order.push('main');\
     export const boot=`${hydrateShell()}:${tokens.brand}`;\
     export const load=()=>import('./mail.js');\
     export const loadLocale=()=>import('./en-US.js');",
  )?;
  fixture
    .write("locale-data.js", "globalThis.order.push('locale-data');export const locale='en-US';")?;
  fixture.write(
    "en-US.js",
    "import{locale}from'./locale-data.js';globalThis.order.push('locale');\
     export const localeStrings=Object.freeze({locale});export default localeStrings;",
  )?;
  let observe = |entry: &str| {
    format!(
      "globalThis.require=(await import('node:module')).createRequire(import.meta.url);\
       globalThis.order=[];const main=await import({entry:?});\
       events.push([...order]);const mail=await main.load();\
       events.push(mail.activateMail(),[...order]);\
       events.push(main.verifyLocales(),[...order]);const locale=await main.loadLocale();\
       events.push(locale.default.locale,main.verifyLocales(),[...order]);"
    )
  };
  fixture.write("observe-reference.js", &observe("./main.js"))?;
  fixture.write("observe-bundle.js", &observe("./dist/main.js"))?;
  let reference = fixture.execute("./observe-reference.js")?;
  assert_eq!(
    reference,
    serde_json::json!([
      ["register", "main", "styles"],
      "mail:#0f6cbd",
      ["register", "main", "styles", "mail"],
      "en-US",
      ["register", "main", "styles", "mail", "locale-data", "locale"],
      "en-US",
      "en-US",
      ["register", "main", "styles", "mail", "locale-data", "locale"]
    ])
  );
  for native in [false, true] {
    for minify in [false, true] {
      for on_demand in [false, true] {
        let mut options = sole_root_options(&fixture, minify, on_demand);
        options.experimental.as_mut().expect("experimental options").resolve_new_url_to_asset =
          Some(native);
        let mut bundler = Bundler::new(options)?;
        fixture.output(&bundler.generate().await?)?;
        bundler.close().await?;
        assert_eq!(
          fixture.execute("./observe-bundle.js")?,
          reference,
          "native={native}, minify={minify}, on_demand={on_demand}"
        );
      }
    }
  }
  Ok(())
}
