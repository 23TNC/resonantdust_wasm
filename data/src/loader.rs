//! Content loader — parse a set of `.rd` sources into one ready-to-run [`Bundle`].
//!
//! This is the entry point both the gate (rlib) and the client (wasm) use to
//! turn the on-disk content into something executable: a [`SymbolTable`] for
//! resolution, a [`Catalog`] (assets / manifests / aspects, with their `@define`
//! records built), the global [`Functions`], and the card / recipe defs indexed
//! by id so a consumer can reach any card's `:data @init` or a recipe's
//! `@input`/`@output`. Content is read at *runtime* — no `include_str!`, so
//! editing a `.rd` never recompiles the crate.
//!
//! [`load`] also runs the corpus acceptance pass (validate + resolve every file
//! against the whole-corpus symbol table) and returns *all* problems on failure,
//! so a bad edit fails loading loudly rather than half-building.

use crate::parser::{parse, Header, Node};
use crate::resolve::{unresolved, SymbolTable};
use crate::validate::validate;
use crate::vm::{Catalog, Functions};
use std::collections::HashMap;

/// A single load-time problem (parse error, or a validate/resolve diagnostic),
/// tagged with the file it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadError {
  pub file: String,
  pub message: String,
}

/// Everything the runtime needs to execute content, built once at load.
#[derive(Default, Debug)]
pub struct Bundle {
  /// Whole-corpus symbol table (resolution + the aspect-member registry).
  pub table: SymbolTable,
  /// Asset / manifest / aspect records (their `@define` hooks run into cells).
  pub catalog: Catalog,
  /// Global `<functions:x>` bodies, callable via `$functions:x call`.
  pub functions: Functions,
  /// Card defs by name (the `::id` node — navigate its `:data`/`:visuals` facets).
  pub cards: HashMap<String, Node>,
  /// Recipe defs by name (the `::id` node — navigate its `@input`/`@output`).
  pub recipes: HashMap<String, Node>,
  /// Card names ordered by def_id: a card's packed `def_id` is `index + 1`
  /// (1-based, 0 = none). Sorted, so ids are stable + content-derived — no
  /// `id.json`. Renumbers are free pre-release ([[project_data_drop_policy]]).
  pub card_ids: Vec<String>,
  /// Recipe names ordered by id (1-based), same scheme — for packing a card's
  /// bound `magnetic.recipe` and naming recipes over the wire.
  pub recipe_ids: Vec<String>,
}

impl Bundle {
  pub fn card(&self, name: &str) -> Option<&Node> {
    self.cards.get(name)
  }
  pub fn recipe(&self, name: &str) -> Option<&Node> {
    self.recipes.get(name)
  }
  /// Packed `def_id` for a card name (1-based; `None` if unknown).
  pub fn card_def_id(&self, name: &str) -> Option<u16> {
    self.card_ids.iter().position(|n| n == name).map(|i| i as u16 + 1)
  }
  /// Card name for a packed `def_id` (`def_id == 0` is the none sentinel).
  pub fn card_name(&self, def_id: u16) -> Option<&str> {
    (def_id != 0).then(|| self.card_ids.get(def_id as usize - 1)).flatten().map(String::as_str)
  }
  pub fn recipe_def_id(&self, name: &str) -> Option<u16> {
    self.recipe_ids.iter().position(|n| n == name).map(|i| i as u16 + 1)
  }
  pub fn recipe_name(&self, id: u16) -> Option<&str> {
    (id != 0).then(|| self.recipe_ids.get(id as usize - 1)).flatten().map(String::as_str)
  }
}

/// Parse every `(name, source)` into a [`Bundle`]. Collects the symbol table and
/// catalog across *all* files first (so cross-file `$` refs resolve), then
/// validates + resolves each. Returns the bundle only if the whole corpus is
/// clean; otherwise every problem found.
pub fn load(sources: &[(String, String)]) -> Result<Bundle, Vec<LoadError>> {
  let mut errors = Vec::new();
  let mut parsed: Vec<(&str, Node)> = Vec::new();
  for (name, text) in sources {
    match parse(text) {
      Ok(node) => parsed.push((name, node)),
      Err(e) => errors.push(LoadError { file: name.clone(), message: format!("parse: {e}") }),
    }
  }

  // Build the corpus view from every successfully-parsed file.
  let mut b = Bundle::default();
  for (_, node) in &parsed {
    b.table.collect(node);
    b.catalog.add_assets(node);
    b.catalog.add_manifest(node);
    b.catalog.add_aspects(node);
    b.functions.add(node);
    index_defs(node, &mut b.cards, &mut b.recipes);
  }

  // Stable, content-derived ids: sort names so def_id is deterministic across
  // gate + client (both load the same content → agree). No id.json.
  b.card_ids = b.cards.keys().cloned().collect();
  b.card_ids.sort();
  b.recipe_ids = b.recipes.keys().cloned().collect();
  b.recipe_ids.sort();

  // Acceptance: validate (per-file) + resolve (whole-corpus) every file.
  for (name, node) in &parsed {
    for d in validate(node) {
      errors.push(LoadError { file: name.to_string(), message: format!("[{}] {}", d.path, d.message) });
    }
    for d in unresolved(node, &b.table) {
      errors.push(LoadError { file: name.to_string(), message: format!("[{}] {}", d.path, d.message) });
    }
  }

  if errors.is_empty() {
    Ok(b)
  } else {
    Err(errors)
  }
}

/// Index `<card>` / `<recipe>` defs by id (inline facet stripped: `::a:visuals` → `a`).
fn index_defs(node: &Node, cards: &mut HashMap<String, Node>, recipes: &mut HashMap<String, Node>) {
  for bucket in &node.children {
    let target = match &bucket.header {
      Header::Bucket(n) if n == "card" => &mut *cards,
      Header::Bucket(n) if n == "recipe" => &mut *recipes,
      _ => continue,
    };
    for d in &bucket.children {
      if let Header::Def(id) = &d.header {
        let key = id.split(':').next().unwrap_or(id).to_string();
        target.insert(key, d.clone());
      }
    }
  }
}

// ---------- Tests ----------

#[cfg(test)]
mod tests {
  use super::*;

  fn src(name: &str, text: &str) -> (String, String) {
    (name.to_string(), text.to_string())
  }

  #[test]
  fn loads_a_clean_corpus() {
    let srcs = vec![
      src("aspects.rd", "<aspect>\n  ::type>\n    @define>\n      traits &section set\n"),
      src("cards.rd", "<card>\n  ::forest>\n    :data>\n      @define>\n        tile &aspect.type set\n    :visuals>\n      @update>\n        $functions:ring call drop\n"),
      src("recipes.rd", "<recipe>\n  ::r>\n    @output>\n      10 &sys.duration set\n"),
      src("fns.rd", "<functions:ring>\n  0 ret\n"),
    ];
    let b = load(&srcs).expect("clean load");
    // defs indexed + navigable
    assert!(b.card("forest").is_some());
    assert!(b.recipe("r").is_some());
    assert!(b.card("forest").unwrap().facet("data").unwrap().hook("define").is_some());
    assert!(b.card("forest").unwrap().facet("visuals").unwrap().hook("update").is_some());
    // symbol table + functions populated
    assert!(b.table.aspects.contains("type"));
    assert!(b.table.functions.contains("ring"));
  }

  #[test]
  fn derives_stable_def_ids_from_content() {
    let srcs = vec![
      src("aspects.rd", "<aspect>\n  ::type>\n    @define>\n      traits &section set\n"),
      src("cards.rd", "<card>\n  ::forest>\n    :data>\n      @define>\n        tile &aspect.type set\n  ::desert>\n    :data>\n      @define>\n        tile &aspect.type set\n"),
      src("recipes.rd", "<recipe>\n  ::cut>\n    @output>\n      10 &sys.duration set\n  ::burn>\n    @output>\n      10 &sys.duration set\n"),
    ];
    let b = load(&srcs).unwrap();
    // ids are 1-based and assigned by sorted name (desert < forest)
    assert_eq!(b.card_def_id("desert"), Some(1));
    assert_eq!(b.card_def_id("forest"), Some(2));
    assert_eq!(b.card_name(1), Some("desert"));
    assert_eq!(b.card_name(2), Some("forest"));
    // round-trip + sentinels
    assert_eq!(b.card_name(b.card_def_id("forest").unwrap()), Some("forest"));
    assert_eq!(b.card_name(0), None); // 0 = none
    assert_eq!(b.card_def_id("ghost"), None);
    // recipes get their own id space (burn < cut)
    assert_eq!(b.recipe_def_id("burn"), Some(1));
    assert_eq!(b.recipe_def_id("cut"), Some(2));
    assert_eq!(b.recipe_name(2), Some("cut"));
  }

  #[test]
  fn surfaces_resolve_errors() {
    // `aspect.ghost` member is not in any <aspect> registry -> a load error
    let errs = load(&[src("bad.rd", "<functions:f>\n  2 &aspect.ghost set\n")]).unwrap_err();
    assert!(errs.iter().any(|e| e.file == "bad.rd" && e.message.contains("ghost")), "{errs:?}");
  }

  #[test]
  fn surfaces_parse_errors() {
    // an instruction at structural level is a parse error
    let errs = load(&[src("p.rd", "<card>\n  ::x>\n  10 &aspect.cost set\n")]).unwrap_err();
    assert!(errs.iter().any(|e| e.file == "p.rd" && e.message.starts_with("parse:")), "{errs:?}");
  }
}
