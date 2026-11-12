use core::cmp::Reverse;
use std::collections::hash_map::Entry;

use oxc_str::CompactStr;
use rolldown_common::{ModuleTable, SymbolRef, SymbolRefDb};
use rolldown_utils::{concat_string, rustc_hash::FxHashMapExt};
use rustc_hash::FxHashMap;

/// Single source of the chunk `$N` conflict-suffix naming algorithm, shared by
/// chunk-internal deconfliction (`Renamer`) and cross-chunk export naming.
///
/// Owns the name -> conflict-index map. Callers store the returned name in their
/// own structure (`canonical_names`, `exports_to_other_chunks`, ...).
#[derive(Debug, Default)]
pub struct ConflictResolver {
  /// Key is a taken name; value is the conflict index used to generate the next
  /// unique `name$N`. See `renamer.rs` for the historical semantics this preserves.
  used: FxHashMap<CompactStr, u32>,
}

impl ConflictResolver {
  pub fn new(reserved: impl IntoIterator<Item = CompactStr>) -> Self {
    Self { used: reserved.into_iter().map(|name| (name, 0)).collect() }
  }

  /// Like `new` but pre-sizes the backing map and starts empty — for callers
  /// (e.g. the cross-chunk export pass) that know the name count up front and
  /// seed no reserved names.
  pub fn with_capacity(capacity: usize) -> Self {
    Self { used: FxHashMap::with_capacity(capacity) }
  }

  /// Reserve a name so it is never handed out (and seeds its conflict index at 0).
  /// Idempotent: re-reserving an existing name is a no-op that preserves its conflict index.
  pub fn reserve(&mut self, name: CompactStr) {
    self.used.entry(name).or_insert(0);
  }

  pub fn contains(&self, name: &str) -> bool {
    self.used.contains_key(name)
  }

  /// Prefer a module-scoped collision hint so unrelated graph edits do not renumber bindings.
  /// The normal resolver still handles hash collisions, reserved names and capture vetoes.
  pub fn resolve_stable(
    &mut self,
    base: CompactStr,
    module_id: &str,
    accept: impl Fn(&str, bool) -> bool,
  ) -> CompactStr {
    let base = match self.used.entry(base) {
      Entry::Vacant(slot) => {
        if accept(slot.key(), true) {
          let name = slot.key().clone();
          slot.insert(0);
          return name;
        }
        slot.into_key()
      }
      Entry::Occupied(slot) => slot.key().clone(),
    };
    let suffix = xxhash_rust::xxh3::xxh3_64(module_id.as_bytes()) & 0xffff_ffff;
    let hint = format!("{base}${suffix:08x}").into();
    self.resolve(hint, |candidate, _| accept(candidate, false))
  }

  /// Pick a unique name for `base`, trying `base` then `base$1`, `base$2`, ...
  ///
  /// `accept(candidate, is_original)` vetoes a candidate that is otherwise free
  /// (e.g. would capture a nested binding). `is_original` is `true` only on the
  /// bare-`base` fast path. Returns the chosen name and records it as used.
  #[inline]
  pub fn resolve(&mut self, base: CompactStr, accept: impl Fn(&str, bool) -> bool) -> CompactStr {
    // Fast path: probe `base`'s slot exactly once via the entry API. The previous
    // `contains_key(&base)` followed by `insert(base.clone(), 0)` hashed `base` and
    // walked the table twice on the common no-conflict path; `resolve` runs once per
    // top-level symbol during deconfliction, so the extra lookup is pure overhead.
    //
    // The match resolves the fast path inline and otherwise yields the
    // `(base, conflict_index)` seed for the suffix search below. `conflict_index`
    // jumps past occupied candidates by reading their stored counter
    // (preserves renamer.rs:217-225).
    let (base, mut conflict_index) = match self.used.entry(base) {
      Entry::Vacant(slot) => {
        if accept(slot.key().as_str(), true) {
          // Bare name is free and accepted. The map must own the key, so one clone
          // is unavoidable either way; insert writes the slot we already located.
          let name = slot.key().clone();
          slot.insert(0);
          return name;
        }
        // Free but vetoed (would capture a nested binding): leave `base` unrecorded
        // (another owner may legitimately take it) and start suffixing at 1.
        (slot.into_key(), 1)
      }
      Entry::Occupied(mut slot) => {
        // `base` is taken: jump past it using its stored counter.
        *slot.get_mut() += 1;
        (slot.key().clone(), *slot.get())
      }
    };

    loop {
      let candidate: CompactStr =
        concat_string!(base, "$", itoa::Buffer::new().format(conflict_index)).into();

      if let Some(idx) = self.used.get_mut(&candidate) {
        *idx += 1;
        conflict_index = *idx;
        continue;
      }

      if !accept(&candidate, false) {
        conflict_index += 1;
        continue;
      }

      self.used.insert(candidate.clone(), 0);
      return candidate;
    }
  }
}

/// Canonical ordering for handing out deconflicted names: the entry module gets
/// naming priority. `exec_order` is assigned ascending (dependencies first,
/// entry last / highest), so `Reverse(exec_order)` sorts the entry first — the
/// same priority as the `.rev()` pass in `deconflict_chunk_symbols`. Ties are
/// broken by symbol name ascending, then by `SymbolId` ascending (matching the
/// forward statement declaration order within a module for same-named facade
/// symbols such as `__initialized`). This is the single source of the ordering
/// invariant previously duplicated as the pinned-SHA comments in
/// `compute_cross_chunk_links.rs`.
pub fn deconflict_order_key<'a>(
  symbol_ref: SymbolRef,
  module_table: &ModuleTable,
  symbol_db: &'a SymbolRefDb,
) -> (Reverse<u32>, &'a str, oxc::semantic::SymbolId) {
  let exec_order = module_table[symbol_ref.owner].exec_order();
  (Reverse(exec_order), symbol_ref.name(symbol_db), symbol_ref.symbol)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn cs(s: &str) -> CompactStr {
    CompactStr::new(s)
  }

  #[test]
  fn bare_name_when_free() {
    let mut r = ConflictResolver::new(std::iter::empty());
    assert_eq!(r.resolve(cs("foo"), |_, _| true).as_str(), "foo");
  }

  #[test]
  fn suffix_on_collision() {
    let mut r = ConflictResolver::new(std::iter::empty());
    assert_eq!(r.resolve(cs("foo"), |_, _| true).as_str(), "foo");
    assert_eq!(r.resolve(cs("foo"), |_, _| true).as_str(), "foo$1");
    assert_eq!(r.resolve(cs("foo"), |_, _| true).as_str(), "foo$2");
  }

  #[test]
  fn reserved_names_are_avoided() {
    let mut r = ConflictResolver::new([cs("foo")]);
    assert_eq!(r.resolve(cs("foo"), |_, _| true).as_str(), "foo$1");
  }

  #[test]
  fn accept_veto_skips_candidate() {
    // Reject any name equal to "foo$1" (e.g. simulating a nested-capture veto),
    // forcing the loop to advance to "foo$2".
    let mut r = ConflictResolver::new(std::iter::empty());
    assert_eq!(r.resolve(cs("foo"), |_, _| true).as_str(), "foo"); // occupies "foo"
    assert_eq!(r.resolve(cs("foo"), |cand, _is_original| cand != "foo$1").as_str(), "foo$2");
  }

  #[test]
  fn jump_uses_stored_counter_of_occupied_candidate() {
    // Reproduces renamer.rs:221-225: when a candidate is already used, its stored
    // counter is read and the index jumps past it.
    let mut r = ConflictResolver::new(std::iter::empty());
    r.reserve(cs("foo"));
    r.reserve(cs("foo$1"));
    // base "foo" occupied -> try "foo$1" (occupied, counter 0 -> becomes 1, jump to "foo$2")
    assert_eq!(r.resolve(cs("foo"), |_, _| true).as_str(), "foo$2");
  }

  #[test]
  fn jump_walks_multi_hop_chain() {
    // Exercises a three-hop jump: foo -> foo$1 -> foo$2 -> foo$3.
    // All three candidates are pre-reserved; the index must walk the full chain.
    let mut r = ConflictResolver::new(std::iter::empty());
    r.reserve(cs("foo"));
    r.reserve(cs("foo$1"));
    r.reserve(cs("foo$2"));
    assert_eq!(r.resolve(cs("foo"), |_, _| true).as_str(), "foo$3");
  }

  #[test]
  fn contains_reports_reserved() {
    let mut r = ConflictResolver::new([cs("a")]);
    assert!(r.contains("a"));
    assert!(!r.contains("b"));
    r.reserve(cs("b"));
    assert!(r.contains("b"));
  }

  #[test]
  fn order_key_is_reverse_exec_order_then_name() {
    // The key type: the entry module has the highest exec_order (dependencies
    // execute first with lower orders, entry last). Reverse sorts the highest
    // exec_order first (entry-first), and ties break by name ascending. We
    // assert the tuple shape and comparison semantics with synthetic values.
    use core::cmp::Reverse;
    let a = (Reverse(5u32), CompactStr::new("b"));
    let b = (Reverse(5u32), CompactStr::new("a"));
    let c = (Reverse(6u32), CompactStr::new("z"));
    let mut v = vec![a.clone(), b.clone(), c.clone()];
    v.sort();
    // c has higher exec_order -> Reverse puts it first; within exec_order 5, "a" < "b".
    assert_eq!(v, vec![c, b, a]);
  }

  #[test]
  fn resolve_stable_keeps_free_base_and_uses_module_hash_on_collision() {
    let mut without_extra = ConflictResolver::new(std::iter::empty());
    assert_eq!(
      without_extra.resolve_stable(cs("value"), "entry.js", |_, _| true).as_str(),
      "value"
    );
    let target = without_extra.resolve_stable(cs("value"), "leaf.js", |_, _| true);
    let suffix = target.strip_prefix("value$").expect("module-scoped collision suffix");
    assert_eq!(suffix.len(), 8);
    assert!(suffix.bytes().all(|b| b.is_ascii_hexdigit()));

    let mut with_extra = ConflictResolver::new(std::iter::empty());
    assert_eq!(with_extra.resolve_stable(cs("value"), "entry.js", |_, _| true).as_str(), "value");
    let extra = with_extra.resolve_stable(cs("value"), "added.js", |_, _| true);
    assert_ne!(extra, target);
    assert_eq!(with_extra.resolve_stable(cs("value"), "leaf.js", |_, _| true), target);
  }

  #[test]
  fn resolve_stable_handles_hash_collisions_reserved_names_and_capture_vetoes() {
    let mut r = ConflictResolver::new([cs("Promise")]);
    let reserved = r.resolve_stable(cs("Promise"), "mod.js", |_, _| true);
    assert!(reserved.starts_with("Promise$"));
    assert_eq!(reserved.len(), "Promise$".len() + 8);
    // Same module id collides on the 8-hex hint and falls back to `$1`.
    assert_eq!(
      r.resolve_stable(cs("Promise"), "mod.js", |_, _| true).as_str(),
      format!("{reserved}$1")
    );

    // Capture veto on the original name must leave the bare name available for a later owner
    // and pass `is_original == false` for every fallback candidate.
    let mut vetoed = ConflictResolver::new(std::iter::empty());
    let first_hint_suffix =
      format!("{:08x}", xxhash_rust::xxh3::xxh3_64(b"vetoed.js") & 0xffff_ffff);
    let first_hint = format!("value${first_hint_suffix}");
    let chosen = vetoed.resolve_stable(cs("value"), "vetoed.js", |cand, is_original| {
      if is_original {
        assert_eq!(cand, "value");
        false
      } else {
        cand != first_hint
      }
    });
    assert_eq!(chosen.as_str(), format!("{first_hint}$1"));
    assert_eq!(vetoed.resolve_stable(cs("value"), "clean.js", |_, _| true).as_str(), "value");
  }
}
