//! Resonant Dust client-side wasm crate.
//!
//! Compiled to a browser wasm bundle (see the `wasm` service in `compose.yml`)
//! and imported by the pixijs client. The server does NOT consume this crate —
//! it links `resonantdust-data` directly as an rlib. The bindings live only
//! here, where they're needed.
//!
//! The substance ([`Content`] and its methods) is plain Rust — feature-
//! independent, so `cargo check`/`test` exercise it natively without the wasm
//! toolchain. The browser surface is a thin `#[wasm_bindgen]` layer, gated on
//! `js`, that marshals JSON in/out and delegates to the plain methods.

use resonantdust_data::bridge::{card_view, operating_set, Card};
use resonantdust_data::loader::{load, Bundle};
use resonantdust_data::vm::{match_recipe as vm_match, plan_recipe as vm_plan, Cell, Plan};

#[cfg(feature = "js")]
use wasm_bindgen::prelude::*;

/// A loaded content runtime: the [`Bundle`] plus the operations the client
/// calls (card render-view, client-side recipe match/plan). Opaque to JS —
/// constructed once from the `.rd` sources, then queried.
#[derive(Debug)]
#[cfg_attr(feature = "js", wasm_bindgen)]
pub struct Content {
    bundle: Bundle,
}

impl Content {
    /// Load `(name, source)` `.rd` pairs into a runtime handle. Returns the
    /// corpus problems (parse/validate/resolve) as a message on failure.
    pub fn load(sources: Vec<(String, String)>) -> Result<Content, String> {
        load(&sources).map(|bundle| Content { bundle }).map_err(|errs| {
            let mut msg = format!("{} load problem(s):", errs.len());
            for e in errs.iter().take(8) {
                msg.push_str(&format!("\n  {}: {}", e.file, e.message));
            }
            msg
        })
    }

    /// Packed `def_id` for a card name (`None` if unknown).
    pub fn card_def_id(&self, name: &str) -> Option<u16> {
        self.bundle.card_def_id(name)
    }

    /// The VM view of a stored card — `def_id` + folded aspects (what a recipe
    /// reads, and what the renderer walks for objects).
    pub fn card_view(&self, card: &Card) -> Cell {
        card_view(&self.bundle, card)
    }

    /// Match a recipe's `@input` against an operating-set frame of
    /// `(slot path, card)` placements — the client-side matcher.
    pub fn match_recipe(&self, placed: &[(String, Card)], recipe: &str) -> Option<Plan> {
        self.run_recipe(placed, recipe, true)
    }

    /// Run a (matched) recipe's `@output` tape against the frame.
    pub fn plan_recipe(&self, placed: &[(String, Card)], recipe: &str) -> Option<Plan> {
        self.run_recipe(placed, recipe, false)
    }

    fn run_recipe(&self, placed: &[(String, Card)], recipe: &str, input: bool) -> Option<Plan> {
        let refs: Vec<(&str, &Card)> = placed.iter().map(|(p, c)| (p.as_str(), c)).collect();
        let mut frame = operating_set(&self.bundle, &refs);
        let hook = if input { "input" } else { "output" };
        let body = &self.bundle.recipe(recipe)?.hook(hook)?.body;
        let run = if input { vm_match } else { vm_plan };
        run(body, &mut frame, &self.bundle.catalog, &self.bundle.functions).ok()
    }
}

// ---------- Browser surface (js feature) ----------

#[cfg(feature = "js")]
fn jserr<E: std::fmt::Display>(e: E) -> JsValue {
    JsValue::from_str(&e.to_string())
}

#[cfg(feature = "js")]
#[wasm_bindgen]
impl Content {
    /// `new Content(sourcesJson)` — `sourcesJson` is `[[name, text], …]`.
    #[wasm_bindgen(constructor)]
    pub fn new(sources_json: &str) -> Result<Content, JsValue> {
        let sources: Vec<(String, String)> = serde_json::from_str(sources_json).map_err(jserr)?;
        Content::load(sources).map_err(|e| JsValue::from_str(&e))
    }

    #[wasm_bindgen(js_name = cardDefId)]
    pub fn card_def_id_js(&self, name: &str) -> Option<u16> {
        self.card_def_id(name)
    }

    /// `cardView(cardJson)` → the view cell as JSON. `cardJson` = `{def_id, stock}`.
    #[wasm_bindgen(js_name = cardView)]
    pub fn card_view_js(&self, card_json: &str) -> Result<String, JsValue> {
        let card: Card = serde_json::from_str(card_json).map_err(jserr)?;
        serde_json::to_string(&self.card_view(&card)).map_err(jserr)
    }

    /// `matchRecipe(placedJson, recipe)` → the `Plan` as JSON (`null` if no
    /// such recipe). `placedJson` = `[[slotPath, {def_id, stock}], …]`.
    #[wasm_bindgen(js_name = matchRecipe)]
    pub fn match_recipe_js(&self, placed_json: &str, recipe: &str) -> Result<String, JsValue> {
        let placed: Vec<(String, Card)> = serde_json::from_str(placed_json).map_err(jserr)?;
        serde_json::to_string(&self.match_recipe(&placed, recipe)).map_err(jserr)
    }

    #[wasm_bindgen(js_name = planRecipe)]
    pub fn plan_recipe_js(&self, placed_json: &str, recipe: &str) -> Result<String, JsValue> {
        let placed: Vec<(String, Card)> = serde_json::from_str(placed_json).map_err(jserr)?;
        serde_json::to_string(&self.plan_recipe(&placed, recipe)).map_err(jserr)
    }
}

// ---------- Tests ----------

#[cfg(test)]
mod tests {
    use super::*;

    fn content() -> Content {
        let aspects = "<aspect>\n  ::type>\n    @define>\n      traits &section set\n";
        let cards = "<card>\n  ::corpus>\n    :data>\n      @define>\n        faculty &aspect.type set\n";
        let recipes = "<recipe>\n  ::use_corpus>\n    @input>\n      $card::corpus *slot.1.0.def_id eq if &slot.1.0 use\n    @output>\n      10 &sys.duration set\n      &slot.1.0 destroy\n";
        Content::load(vec![
            ("a.rd".into(), aspects.into()),
            ("c.rd".into(), cards.into()),
            ("r.rd".into(), recipes.into()),
        ])
        .expect("load")
    }

    #[test]
    fn loads_and_matches_a_recipe() {
        let c = content();
        let id = c.card_def_id("corpus").expect("corpus id");
        let plan = c
            .match_recipe(&[("slot.1.0".into(), Card { def_id: id, stock: vec![] })], "use_corpus")
            .expect("matched");
        assert!(plan.matched);
        let out = c
            .plan_recipe(&[("slot.1.0".into(), Card { def_id: id, stock: vec![] })], "use_corpus")
            .expect("planned");
        assert_eq!(out.duration, 10);
    }

    #[test]
    fn bad_content_reports_problems() {
        let err = Content::load(vec![("b.rd".into(), "<functions:f>\n  2 &aspect.ghost set\n".into())]).unwrap_err();
        assert!(err.contains("ghost"), "{err}");
    }
}
