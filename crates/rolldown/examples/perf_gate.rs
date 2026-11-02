use std::{fs, path::PathBuf, time::Instant};

use rolldown::{
  Bundler, BundlerOptions, ExperimentalOptions, InputItem, RawMinifyOptions, SourceMapType,
  TreeshakeOptions,
};
use rolldown_common::ScanMode;

struct TempFixture(PathBuf);

impl Drop for TempFixture {
  fn drop(&mut self) {
    let _ = fs::remove_dir_all(&self.0);
  }
}

fn generate_fixture_dir(tag: &str, with_dynamic_imports: bool) -> TempFixture {
  let root = std::env::temp_dir().join(format!(
    "rolldown-perf-gate-{}-{tag}",
    std::process::id()
  ));
  let _ = fs::remove_dir_all(&root);
  fs::create_dir_all(&root).expect("create temp fixture dir");
  let root = fs::canonicalize(&root).unwrap_or(root);

  let mut barrel = String::new();
  let mut entry = String::from("import { m0, m1, m2, m3, m4 } from './barrel.js';\n");
  for i in 0..120 {
    let mut helpers = String::new();
    for h in 0..24 {
      helpers.push_str(&format!(
        "function helper_{i}_{h}(x) {{ const a = x + {h}; const b = a * 2; return b - {i}; }}\n"
      ));
    }
    if i == 0 {
      fs::write(
        root.join("mod_0.js"),
        format!("{helpers}export function m0() {{ return helper_0_0(1) + helper_0_23(2); }}\nexport function unused_0() {{ return 0; }}\n"),
      )
      .expect("write mod_0");
    } else {
      let prev = i - 1;
      let full = format!(
        "import {{ m{prev} }} from './mod_{prev}.js';\n{helpers}export function m{i}() {{ return helper_{i}_0({i}) + helper_{i}_23(1) + m{prev}(); }}\nexport function unused_{i}() {{ return {i} * 10; }}\n"
      );
      fs::write(root.join(format!("mod_{i}.js")), full).expect("write chained mod");
    }
    barrel.push_str(&format!("export {{ m{i}, unused_{i} }} from './mod_{i}.js';\n"));
  }
  fs::write(root.join("barrel.js"), barrel).expect("write barrel");

  if with_dynamic_imports {
    for d in 0..8 {
      let target = d * 10 + 5;
      let dyn_code = format!(
        "import {{ m{target} }} from './barrel.js';\nexport const dynVal{d} = () => m{target}();\n"
      );
      fs::write(root.join(format!("dyn_{d}.js")), dyn_code).expect("write dyn");
      entry.push_str(&format!("export const load_{d} = () => import('./dyn_{d}.js');\n"));
    }
    entry.push_str("export const total = () => m0() + m1() + m2() + m3() + m4();\n");
  } else {
    entry.push_str("import { leafVal } from './leaf_edit.js';\n");
    entry.push_str("export const total = () => m0() + m1() + m2() + m3() + m4() + leafVal();\n");
    fs::write(
      root.join("leaf_edit.js"),
      "export function leafVal() { return 100; }\n",
    )
    .expect("write leaf_edit");
  }
  fs::write(root.join("entry.js"), entry).expect("write entry");
  TempFixture(root)
}

async fn bench_case(name: &str, rounds: usize, strict_and_lazy: bool) -> (u128, usize) {
  assert!(rounds >= 1, "ROUNDS must be >= 1");
  let dir = generate_fixture_dir(name, true);
  let cwd = dir.0.clone();
  let mut best_ns = u128::MAX;
  let mut asset_count = 0usize;

  for _ in 0..rounds {
    let mut bundler = Bundler::new(BundlerOptions {
      input: Some(vec![InputItem {
        name: Some("entry".to_string()),
        import: "./entry.js".to_string(),
      }]),
      cwd: Some(cwd.clone()),
      sourcemap: Some(SourceMapType::File),
      treeshake: TreeshakeOptions::Boolean(true),
      strict_execution_order: Some(strict_and_lazy),
      experimental: Some(ExperimentalOptions {
        lazy_barrel: Some(strict_and_lazy),
        ..Default::default()
      }),
      ..Default::default()
    })
    .expect("create bundler");

    let start = Instant::now();
    let out = bundler.generate().await.expect("generate bundle");
    let elapsed = start.elapsed().as_nanos();
    asset_count = out.assets.len();
    assert!(asset_count > 0, "expected generated assets for {name}");
    best_ns = best_ns.min(elapsed);
  }
  println!("RESULT|{name}|{asset_count}|{best_ns}");
  (best_ns, asset_count)
}

async fn bench_incremental_preserve_modules(name: &str, rounds: usize) -> (u128, usize) {
  assert!(rounds >= 1, "ROUNDS must be >= 1");
  let dir = generate_fixture_dir(name, false);
  let cwd = dir.0.clone();
  let mut bundler = Bundler::new(BundlerOptions {
    input: Some(vec![InputItem {
      name: Some("entry".to_string()),
      import: "./entry.js".to_string(),
    }]),
    cwd: Some(cwd.clone()),
    entry_filenames: Some("[name].js".to_string().into()),
    chunk_filenames: Some("[name].js".to_string().into()),
    preserve_modules: Some(true),
    sourcemap: Some(SourceMapType::File),
    minify: Some(RawMinifyOptions::Bool(false)),
    treeshake: TreeshakeOptions::Boolean(false),
    experimental: Some(ExperimentalOptions {
      incremental_build: Some(true),
      dev_mode: Some(Default::default()),
      ..Default::default()
    }),
    ..Default::default()
  })
  .expect("create incremental bundler");
  bundler.set_omit_reused_chunk_bytes(true);

  let cold_out = bundler
    .incremental_generate(ScanMode::Full)
    .await
    .expect("initial full incremental generate");
  let asset_count = cold_out.assets.len();
  assert!(asset_count > 0, "expected generated assets for {name}");

  let target_path = cwd.join("leaf_edit.js");
  let target_id = target_path.to_string_lossy().into_owned();
  let mut best_ns = u128::MAX;

  for round in 0..rounds {
    let updated = format!(
      "export function leafVal() {{ return {}; }}\n",
      100 + round + 1
    );
    fs::write(&target_path, updated).expect("write updated module");

    let start = Instant::now();
    let _out = bundler
      .incremental_generate(ScanMode::Partial(vec![target_id.as_str().into()]))
      .await
      .expect("incremental partial generate");
    let elapsed = start.elapsed().as_nanos();
    best_ns = best_ns.min(elapsed);
  }
  assert!(
    bundler.last_fast_native_relink() && bundler.last_reused_chunk_filenames().len() >= 120,
    "expected fast_native_relink and >= 120 reused chunks on incremental single-module edit, got {} (fast_relink={}, fallback={:?})",
    bundler.last_reused_chunk_filenames().len(),
    bundler.last_fast_native_relink(),
    bundler.last_native_relink_fallback(),
  );
  println!("RESULT|{name}|{asset_count}|{best_ns}");
  (best_ns, asset_count)
}

async fn bench_incremental_chunked(name: &str, rounds: usize) -> (u128, usize) {
  assert!(rounds >= 1, "ROUNDS must be >= 1");
  let dir = generate_fixture_dir(name, true);
  let cwd = dir.0.clone();
  let mut bundler = Bundler::new(BundlerOptions {
    input: Some(vec![InputItem {
      name: Some("entry".to_string()),
      import: "./entry.js".to_string(),
    }]),
    cwd: Some(cwd.clone()),
    sourcemap: Some(SourceMapType::File),
    treeshake: TreeshakeOptions::Boolean(true),
    experimental: Some(ExperimentalOptions {
      incremental_build: Some(true),
      ..Default::default()
    }),
    ..Default::default()
  })
  .expect("create chunked incremental bundler");

  let cold_out = bundler
    .incremental_generate(ScanMode::Full)
    .await
    .expect("initial full chunked incremental generate");
  let asset_count = cold_out.assets.len();
  assert!(asset_count > 0, "expected generated assets for {name}");

  let target_path = cwd.join("mod_3.js");
  let target_id = target_path.to_string_lossy().into_owned();
  let mut best_ns = u128::MAX;

  for round in 0..rounds {
    let mut helpers = String::new();
    for h in 0..24 {
      helpers.push_str(&format!(
        "function helper_3_{h}(x) {{ const a = x + {h}; const b = a * 2; return b - 3; }}\n"
      ));
    }
    let updated = format!(
      "import {{ m2 }} from './mod_2.js';\n{helpers}export function m3() {{ return helper_3_0({}) + helper_3_23(1) + m2(); }}\nexport function unused_3() {{ return 30; }}\n",
      3 + round + 1
    );
    fs::write(&target_path, updated).expect("write updated mod_3");

    let start = Instant::now();
    let _out = bundler
      .incremental_generate(ScanMode::Partial(vec![target_id.as_str().into()]))
      .await
      .expect("incremental chunked partial generate");
    let elapsed = start.elapsed().as_nanos();
    best_ns = best_ns.min(elapsed);
  }
  println!("RESULT|{name}|{asset_count}|{best_ns}");
  (best_ns, asset_count)
}

#[tokio::main]
async fn main() {
  let rounds: usize = std::env::var("ROUNDS")
    .ok()
    .map(|s| s.parse::<usize>().expect("ROUNDS must be a positive integer"))
    .unwrap_or(5);
  assert!(rounds >= 1, "ROUNDS must be >= 1");

  bench_case("bundle_eager_120", rounds, false).await;
  bench_case("bundle_strict_lazy_barrel_120", rounds, true).await;
  bench_incremental_chunked("bundle_chunked_incremental_edit_120", rounds).await;
  bench_incremental_preserve_modules("bundle_preserve_incremental_edit_120", rounds).await;
}
