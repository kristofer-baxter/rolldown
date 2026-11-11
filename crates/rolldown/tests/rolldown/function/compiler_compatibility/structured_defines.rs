use anyhow::Result;
use rolldown::{Bundler, BundlerOptions};
use rolldown_common::{ExperimentalOptions, RawMinifyOptions};
use serde_json::{Value, json};

use super::Fixture;

const VALUE: &str = "({used:(events.push('value'),1),\
  unused:(()=>{throw Error('unused')})(),\
  sub:{used:2,unused:(()=>{throw Error('sub-unused')})()},\
  then:resolve=>{events.push('own-then');resolve({used:9})}})";

fn options(fixture: &Fixture, root: &str, value: &str, marked: bool) -> BundlerOptions {
  let mut options = fixture.options();
  options.define = Some([(root.to_owned(), value.to_owned())].into_iter().collect());
  options.experimental = Some(ExperimentalOptions {
    define_object_destructuring: marked.then(|| vec![root.to_owned()]),
    ..Default::default()
  });
  options
}

async fn run(fixture: &Fixture, options: BundlerOptions) -> Result<Value> {
  let mut bundler = Bundler::new(options)?;
  fixture.output(&bundler.generate().await?)?;
  bundler.close().await?;
  fixture.execute("./dist/main.js")
}

fn cases_source(cases: &[&str], target: &str) -> String {
  cases
    .iter()
    .map(|case| {
      format!(
        "try{{{}}}catch(error){{events.push(error.message)}};",
        case.replace("TARGET", target)
      )
    })
    .collect()
}

#[tokio::test]
async fn import_meta_parent_destructuring_preserves_await_and_complete_child_values() -> Result<()>
{
  let cases = [
    "const{env}=TARGET;events.push(env.A,env.B,env.C,env.unused)",
    "const{env}=await TARGET;events.push(env.A,env.B,env.C,env.unused)",
    "let env;({env}=TARGET);events.push(env.A,env.B,env.C,env.unused)",
    "let env;({env}=await TARGET);events.push(env.A,env.B,env.C,env.unused)",
    "const{[`e\\u006ev`]:env}=await TARGET;events.push(env.A,env.B,env.C,env.unused)",
    "const{env:{A}}=await TARGET;events.push(A)",
    "const{env,NOPE}=await TARGET;events.push(env.A,NOPE)",
    "const{env,NOPE}=TARGET;events.push(env.A,NOPE)",
    "const{env,url}=await TARGET;events.push(env.A,typeof url)",
  ];
  let fixture = Fixture::new(&[("main.js", &cases_source(&cases, "import.meta"))])?;
  for marked in [false, true, false] {
    for minify in [false, true] {
      let mut options = options(
        &fixture,
        "import.meta.env",
        "({A:'first',B:2,C:true,unused:(events.push('env-value'),4)})",
        marked,
      );
      options.define.as_mut().expect("define map").extend([
        ("import.meta.env.A".to_owned(), "'first'".to_owned()),
        ("import.meta.env.B".to_owned(), "2".to_owned()),
        ("import.meta.env.C".to_owned(), "true".to_owned()),
        ("import.meta.other".to_owned(), "(()=>{throw Error('other materialized')})()".to_owned()),
      ]);
      options.minify = Some(RawMinifyOptions::Bool(minify));
      assert_eq!(
        run(&fixture, options.clone()).await?,
        json!([
          "env-value",
          "first",
          2,
          true,
          4,
          "env-value",
          "first",
          2,
          true,
          4,
          "env-value",
          "first",
          2,
          true,
          4,
          "env-value",
          "first",
          2,
          true,
          4,
          "env-value",
          "first",
          2,
          true,
          4,
          "env-value",
          "first",
          "env-value",
          "first",
          null,
          "env-value",
          "first",
          null,
          "env-value",
          "first",
          "string"
        ]),
        "marked={marked}, minify={minify}"
      );
      options.define.as_mut().expect("define map").insert(
        "import.meta.env".to_owned(),
        "({A:'first',B:2,C:true,unused:(()=>{throw Error('env-expression')})()})".to_owned(),
      );
      assert_eq!(
        run(&fixture, options).await?,
        json!(vec!["env-expression"; cases.len()]),
        "parent selection must not prune a selected child: marked={marked}, minify={minify}"
      );
    }
    for minify in [false, true] {
      let mut options = options(
        &fixture,
        "import.meta",
        "({env:{A:'first',B:2,C:true,unused:4},other:(()=>{throw Error('parent-expression')})()})",
        false,
      );
      options
        .define
        .as_mut()
        .expect("define map")
        .insert("import.meta.env.A".to_owned(), "'first'".to_owned());
      options.minify = Some(RawMinifyOptions::Bool(minify));
      assert_eq!(
        run(&fixture, options).await?,
        json!(vec!["parent-expression"; cases.len()]),
        "an explicit unmarked import.meta expression must stay complete: minify={minify}"
      );
    }
  }
  Ok(())
}

#[tokio::test]
async fn structured_defines_select_properties_only_with_explicit_metadata() -> Result<()> {
  let cases = [
    "const{used}=TARGET;events.push(used)",
    "const{['used']:a,[`u\\u0073ed`]:b,NOPE}=TARGET;events.push(a,b,NOPE)",
    "const{}=TARGET;events.push('empty')",
    "const{NOPE=4}=TARGET;events.push(NOPE)",
    "let used;({used}=TARGET);events.push(used)",
    "let used,NOPE;({used}=({NOPE}=TARGET));events.push(used,NOPE)",
    "const{used}=await TARGET;events.push(used)",
    "let used;({used}=await TARGET);events.push(used)",
  ];
  // Observed with webpack 5.112.0's actual structured DefinePlugin values, not expression strings.
  let selected = json!([
    "value", 1, "value", 1, 1, null, "empty", 4, "value", 1, "value", 1, null, "value", 1, "value",
    1
  ]);
  let ordinary = json!([
    "value", "unused", "value", "unused", "value", "unused", "value", "unused", "value", "unused",
    "value", "unused", "value", "unused", "value", "unused"
  ]);
  for target in ["CONFIG", "globalThis.CONFIG", "import.meta.env", "import.meta"] {
    let fixture = Fixture::new(&[("main.js", &cases_source(&cases, target))])?;
    // Alternate build-local metadata on clones; neither replacement state nor marks may escape.
    for marked in [false, true, false, true] {
      for minify in [false, true] {
        let mut options = options(&fixture, target, VALUE, marked);
        options.minify = Some(RawMinifyOptions::Bool(minify));
        assert_eq!(
          &run(&fixture, options.clone()).await?,
          if marked { &selected } else { &ordinary },
          "target={target}, marked={marked}, minify={minify}"
        );
      }
    }
  }
  Ok(())
}

#[tokio::test]
async fn structured_defines_preserve_full_materialization_contexts() -> Result<()> {
  let cases = [
    "events.push(TARGET)",
    "const{used,...rest}=TARGET;events.push(rest)",
    "let used;events.push(({used}=TARGET),used)",
    "let used;const all=({used}=TARGET);events.push(all,used)",
    "let used,saved;({used}=saved=TARGET);events.push(saved,used)",
    "let used;events.push(({used}=await TARGET),used)",
    "const{used}=await await TARGET;events.push(used)",
    "const{used}=await Promise.resolve(TARGET);events.push(used)",
    "await Promise.resolve(TARGET).then(({used})=>events.push(used))",
    "const{used}=globalThis.flag?TARGET:TARGET;events.push(used)",
    "const{used}=TARGET||{};events.push(used)",
    "const{used}=(events.push('sequence'),TARGET);events.push(used)",
    "const{[globalThis.key||'used']:used}=TARGET;events.push(used)",
    "const{NOPE=TARGET}=TARGET;events.push(NOPE)",
    "const{sub:{used}}=TARGET;events.push(used)",
    "const{sub}=TARGET;events.push(sub)",
  ];
  let mut expected = Vec::new();
  for (index, _) in cases.iter().enumerate() {
    if index == 11 {
      expected.push(json!("sequence"));
    }
    if index >= 14 {
      expected.push(json!("sub-unused"));
    } else {
      expected.extend([json!("value"), json!("unused")]);
    }
  }
  // The missing-key/default case first selects {}, then materializes TARGET in the default.
  for target in ["CONFIG", "globalThis.CONFIG", "import.meta.env"] {
    let fixture = Fixture::new(&[("main.js", &cases_source(&cases, target))])?;
    for minify in [false, true] {
      let mut options = options(&fixture, target, VALUE, true);
      options.minify = Some(RawMinifyOptions::Bool(minify));
      assert_eq!(
        run(&fixture, options).await?,
        json!(expected),
        "target={target}, minify={minify}"
      );
    }
  }
  Ok(())
}

#[tokio::test]
async fn structured_defines_match_exact_nested_static_prefixes_without_context_leaks() -> Result<()>
{
  let cases = [
    "const{used}=TARGET.sub;events.push(used)",
    "const{used}=TARGET['sub'];events.push(used)",
    "const{used}=TARGET[`s\\u0075b`];events.push(used)",
    "const{NOPE=(()=>{const{used}=TARGET.sub;return used})()}=TARGET;events.push(NOPE)",
    "function f(CONFIG){const{used}=CONFIG;events.push(used)}f({used:4})",
    "const{used}=TARGET;events.push(used);events.push(TARGET)",
  ];
  for target in ["CONFIG", "globalThis.CONFIG", "import.meta.env"] {
    let fixture = Fixture::new(&[("main.js", &cases_source(&cases, target))])?;
    for mark_child in [false, true] {
      let mut options = options(&fixture, target, VALUE, true);
      options.define.as_mut().expect("define map").insert(
        format!("{target}.sub"),
        "({used:2,unused:(()=>{throw Error('sub-unused')})()})".to_owned(),
      );
      if mark_child {
        options
          .experimental
          .as_mut()
          .expect("experimental")
          .define_object_destructuring
          .as_mut()
          .expect("marks")
          .push(format!("{target}.sub"));
      }
      assert_eq!(
        run(&fixture, options).await?,
        if mark_child {
          json!([2, 2, 2, 2, 4, "value", 1, "value", "unused"])
        } else {
          json!([
            "sub-unused",
            "sub-unused",
            "sub-unused",
            "sub-unused",
            4,
            "value",
            1,
            "value",
            "unused"
          ])
        },
        "target={target}, mark_child={mark_child}"
      );
    }
  }
  Ok(())
}

#[tokio::test]
async fn structured_defines_keep_await_assimilation_of_the_selected_object() -> Result<()> {
  let source = "Object.defineProperty(Object.prototype,'then',{configurable:true,\
    get(){events.push(this.unused);return undefined}});\
    try{const{used}=await CONFIG;events.push(used)}finally{delete Object.prototype.then}";
  let fixture = Fixture::new(&[("main.js", source)])?;
  for marked in [false, true] {
    assert_eq!(
      run(&fixture, options(&fixture, "CONFIG", "({used:1,unused:2})", marked)).await?,
      if marked { json!([null, 1]) } else { json!([2, 1]) }
    );
  }
  Ok(())
}

#[tokio::test]
async fn structured_defines_select_merged_children_and_preserve_own_proto_keys() -> Result<()> {
  let source = "const{used}=CONFIG;events.push(used);\
    try{events.push(CONFIG)}catch(error){events.push(error.message)};\
    const{['__proto__']:own,used:other}=PROTO;events.push(own,other);\
    const{second,first}=ORDER;events.push(second,first)";
  let fixture = Fixture::new(&[("main.js", source)])?;
  for minify in [false, true] {
    let mut options = options(&fixture, "CONFIG", "({used:1})", true);
    options.define.as_mut().expect("define map").extend([
      ("CONFIG.unused".to_owned(), "(()=>{throw Error('merged-child')})()".to_owned()),
      ("PROTO".to_owned(), "({['__proto__']:7,used:3,unused:missingValue})".to_owned()),
      (
        "ORDER".to_owned(),
        "({first:(events.push('first'),1),\
        unused:missingValue,second:(events.push('second'),2)})"
          .to_owned(),
      ),
    ]);
    // Exercise the same deserializable schema used by standalone Rust/JSON consumers.
    options.experimental = Some(serde_json::from_value(json!({
      "defineObjectDestructuring": ["CONFIG", "PROTO", "ORDER"]
    }))?);
    options.minify = Some(RawMinifyOptions::Bool(minify));
    assert_eq!(
      run(&fixture, options).await?,
      json!([1, "merged-child", 7, 3, "first", "second", 2, 1])
    );
  }
  Ok(())
}

#[tokio::test]
async fn structured_defines_reject_invalid_metadata_without_poisoning_later_builds() -> Result<()> {
  let fixture = Fixture::new(&[("main.js", "const{used}=CONFIG;events.push(used)")])?;
  for (key, value, marks) in [
    ("CONFIG", "({used:1})", vec!["MISSING"]),
    ("CONFIG", "({used:1})", vec!["typeof CONFIG"]),
    ("import.meta.*", "({used:1})", vec!["import.meta.*"]),
    ("CONFIG", "1", vec!["CONFIG"]),
    ("CONFIG", "({get used(){return 1}})", vec!["CONFIG"]),
    ("CONFIG", "({used(){}})", vec!["CONFIG"]),
    ("CONFIG", "({...other,used:1})", vec!["CONFIG"]),
    ("CONFIG", "({[other]:1})", vec!["CONFIG"]),
    ("CONFIG", "({__proto__:null,used:1})", vec!["CONFIG"]),
    ("CONFIG", "({used:0,used:1})", vec!["CONFIG"]),
    ("CONFIG", "({used:1})", vec!["CONFIG", "CONFIG"]),
  ] {
    let mut options = options(&fixture, key, value, false);
    options.experimental.as_mut().expect("experimental").define_object_destructuring =
      Some(marks.iter().map(|key| (*key).to_owned()).collect());
    let mut bundler = Bundler::new(options)?;
    let result = bundler.generate().await;
    assert!(
      result.is_err(),
      "invalid metadata was accepted: key={key}, value={value}, marks={marks:?}"
    );
    let error = format!("{:?}", result.err().expect("invalid metadata"));
    assert!(error.contains("define_object_destructuring"), "wrong diagnostic: {error}");
    bundler.close().await?;
  }
  let mut missing = fixture.options();
  missing.experimental = Some(ExperimentalOptions {
    define_object_destructuring: Some(vec!["CONFIG".to_owned()]),
    ..Default::default()
  });
  let mut bundler = Bundler::new(missing)?;
  assert!(bundler.generate().await.is_err(), "metadata with no defines was ignored");
  bundler.close().await?;
  assert_eq!(run(&fixture, options(&fixture, "CONFIG", VALUE, true)).await?, json!(["value", 1]));
  Ok(())
}
