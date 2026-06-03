//! Phase 3 — the VM.
//!
//! Executes a code body against an in-memory [`Store`] over an abstract value
//! model (so semantics are pinned before any bit-packing). Slots form a tree of
//! [`Cell`]s; `get` pulls host inputs; a [`Catalog`] holds the asset/manifest
//! definitions so a reference stored in a slot can be *followed* mid-path.
//!
//! COVERED: literals, `set`, arithmetic, comparisons, `and`/`or`/`not`,
//! `within`, `if`/`!if`, `goto`, `call`/`ret`/`drop` (functions + `^system`
//! calls, returning one value), `inc`/`dec`, `count`,
//! `array`, `range`, `normalize`, `stock`, `random`; interpolated path segments
//! (`objects.*var.0`, `*aspect.*var.2`, `:*faction`); **ref-following derefs**
//! through the catalog — a slot whose value is a stored `$asset::x` ref follows
//! into the catalog when the path continues (`asset.0.object`). Only genuinely
//! stored refs follow; a plain scalar (`aspect.pine` = a magnitude) does not
//! shadow into a registry — its definition is reached via `$aspect::pine`;
//! cross-function
//! `$functions:x call`; and **recipe execution** — [`match_recipe`] runs an
//! `@input` conjunction over a positioned operating-set frame and emits the
//! [`Hold`]s, and [`plan_recipe`] runs the `@output` tape into a [`Plan`] of
//! duration + styles + ordered [`Effect`]s (the gate's IO contract).
//!
//! BOUNDARY: the VM evaluates *one positioned frame* and treats every aspect as
//! a plain key. The gate builds the operating-set frame: it applies the
//! frame-base `N`, bakes owner re-anchors into the tree, and **folds the aspect
//! hierarchy up** — a forest tile stocks `pine`, so the gate writes the rolled-up
//! `aspect.wood` (= the sum of pine and any other wood-descendants) into the
//! frame before calling, since `cut_tree` reads `aspect.wood`. The pine→wood tree
//! is still a JSON registry the gate consults; it never enters the VM. The gate
//! slides the frame by re-calling `match_recipe` at successive positions.

use crate::parser::{Header, Node, Stmt, Token};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A stored slot. Maps are insertion-ordered so `aspect`/`asset` index
/// positionally (declaration order).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Cell {
  Int(i64),
  Sym(String),
  Ranged { min: i64, max: i64, val: i64 },
  Map(Vec<(String, Cell)>),
  Arr(Vec<Cell>),
}

impl Cell {
  pub fn as_int(&self) -> i64 {
    match self {
      Cell::Int(n) => *n,
      Cell::Ranged { val, .. } => *val,
      _ => 0,
    }
  }
  fn len(&self) -> i64 {
    match self {
      Cell::Map(m) => m.len() as i64,
      Cell::Arr(v) => v.len() as i64,
      _ => 0,
    }
  }
}

/// The asset/manifest definitions, keyed by id. `assets[pine]` is the store from
/// running that pack's `@define` (it has `object`, `size`, …); `manifests[conifer]`
/// is `{faction → {texture: [...]}}`. Used to follow a stored `$asset::x` ref.
#[derive(Default, Debug)]
pub struct Catalog {
  assets: HashMap<String, Cell>,
  manifests: HashMap<String, Cell>,
  /// `<aspect>` records (satisfies/section/icon/color), keyed by id — the
  /// registry the gate consults to fold a card's per-instance aspect scalars up
  /// the satisfies LUT. Cards do NOT store these; an aspect's definition is a
  /// `$aspect::id` lookup here, distinct from the card's `aspect.id` magnitude.
  aspects: HashMap<String, Cell>,
}

impl Catalog {
  /// The `<aspect>` record for `id` (its `@define` cell — satisfies/section/
  /// icon/color/art). For the bridge's satisfies fold + render lookups.
  pub fn aspect(&self, id: &str) -> Option<&Cell> {
    self.aspects.get(id)
  }
  /// Resolve a `$`-ref symbol (`asset::pine`, `manifest::conifer`) to its def.
  fn deref(&self, sym: &str) -> Option<&Cell> {
    let (root, id) = sym.split_once("::")?;
    match root {
      "asset" => self.assets.get(id),
      "manifest" => self.manifests.get(id),
      "aspect" => self.aspects.get(id),
      _ => None,
    }
  }
  /// Load all `<asset>` packs from a parsed file: run each `::id @define`.
  pub fn add_assets(&mut self, root: &Node) {
    for b in &root.children {
      if b.header == Header::Bucket("asset".into()) {
        for d in &b.children {
          if let Header::Def(id) = &d.header {
            if let Some(h) = d.hook("define") {
              let mut s = Store::default();
              let _ = run(&h.body, &mut s, &[], &Catalog::default(), &Functions::default());
              self.assets.insert(id.clone(), s.root);
            }
          }
        }
      }
    }
  }
  /// Load all `<manifest>` objects: each `::id` becomes `{faction → facet data}`.
  pub fn add_manifest(&mut self, root: &Node) {
    for b in &root.children {
      if b.header == Header::Bucket("manifest".into()) {
        for d in &b.children {
          if let Header::Def(id) = &d.header {
            let mut factions = Vec::new();
            for f in &d.children {
              if let Header::Facet(fname) = &f.header {
                if let Some(h) = f.hook("define") {
                  let mut s = Store::default();
                  let _ = run(&h.body, &mut s, &[], &Catalog::default(), &Functions::default());
                  factions.push((fname.clone(), s.root));
                }
              }
            }
            self.manifests.insert(id.clone(), Cell::Map(factions));
          }
        }
      }
    }
  }
  /// Load all `<aspect>` records: run each `::id @define` into a record cell.
  pub fn add_aspects(&mut self, root: &Node) {
    for b in &root.children {
      if b.header == Header::Bucket("aspect".into()) {
        for d in &b.children {
          if let Header::Def(id) = &d.header {
            if let Some(h) = d.hook("define") {
              let mut s = Store::default();
              let _ = run(&h.body, &mut s, &[], &Catalog::default(), &Functions::default());
              self.aspects.insert(id.clone(), s.root);
            }
          }
        }
      }
    }
  }
}

/// A resolved path step.
enum Seg {
  Lit(String),  // literal name (Arr index if numeric, else Map key)
  Idx(usize),   // interpolation that read an Int — index / positional
  Key(String),  // interpolation that read a Sym — map key (e.g. faction)
}

fn step<'a>(cur: &'a Cell, seg: &Seg) -> Option<&'a Cell> {
  match (cur, seg) {
    (Cell::Arr(v), Seg::Idx(i)) => v.get(*i),
    (Cell::Map(m), Seg::Idx(i)) => m.get(*i).map(|(_, c)| c),
    (Cell::Arr(v), Seg::Lit(s)) => s.parse::<usize>().ok().and_then(|i| v.get(i)),
    (Cell::Map(m), Seg::Lit(s)) => m.iter().find(|(k, _)| k == s).map(|(_, c)| c),
    (Cell::Map(m), Seg::Key(k)) => m.iter().find(|(k2, _)| k2 == k).map(|(_, c)| c),
    _ => None,
  }
}

fn walk_read<'a>(cur: &'a Cell, segs: &[Seg]) -> Option<&'a Cell> {
  let Some((head, rest)) = segs.split_first() else { return Some(cur) };
  step(cur, head).and_then(|c| walk_read(c, rest))
}

/// Deref-aware read: walks the instance store, and when a path continues past an
/// *explicitly stored* `Sym` ref, follows it into the catalog and continues
/// there. Only a real stored ref derefs — a slot whose value is `$asset::pine`
/// (e.g. `objects.0`), where `*objects.0.object` walks the ref that's actually
/// in the data. A plain scalar does NOT shadow into a registry: `aspect.pine`
/// (a per-card magnitude) behaves like any other local key, and an aspect's
/// definition is reached with an explicit `$aspect::pine`, never by walking a
/// card's value. Instance (`*`) and definition (`$`) stay distinct.
fn resolve(store: &Store, cat: &Catalog, path: &str) -> Option<Cell> {
  let segs = store.parse(path);
  let mut cur = store.root.clone();
  for seg in &segs {
    cur = match step(&cur, seg) {
      Some(c) => c.clone(),
      None => match &cur {
        Cell::Sym(s) => step(cat.deref(s)?, seg)?.clone(),
        _ => return None,
      },
    };
  }
  Some(cur)
}

fn walk_write(cur: &mut Cell, segs: &[Seg], val: Cell) {
  let Some((head, rest)) = segs.split_first() else {
    *cur = val;
    return;
  };
  match head {
    Seg::Idx(i) => {
      if !matches!(cur, Cell::Arr(_) | Cell::Map(_)) {
        *cur = Cell::Arr(Vec::new());
      }
      match cur {
        Cell::Arr(v) => {
          if *i >= v.len() {
            v.resize(i + 1, Cell::Int(0));
          }
          walk_write(&mut v[*i], rest, val);
        }
        Cell::Map(m) if *i < m.len() => walk_write(&mut m[*i].1, rest, val),
        _ => {}
      }
    }
    Seg::Lit(s) if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) && matches!(cur, Cell::Arr(_)) => {
      if let Cell::Arr(v) = cur {
        let i: usize = s.parse().unwrap();
        if i >= v.len() {
          v.resize(i + 1, Cell::Int(0));
        }
        walk_write(&mut v[i], rest, val);
      }
    }
    Seg::Lit(s) | Seg::Key(s) => {
      if !matches!(cur, Cell::Map(_)) {
        *cur = Cell::Map(Vec::new());
      }
      if let Cell::Map(m) = cur {
        match m.iter().position(|(k, _)| k == s) {
          Some(idx) => walk_write(&mut m[idx].1, rest, val),
          None => {
            m.push((s.clone(), Cell::Int(0)));
            let last = m.len() - 1;
            walk_write(&mut m[last].1, rest, val);
          }
        }
      }
    }
  }
}

#[derive(Debug)]
pub struct Store {
  root: Cell,
}
impl Default for Store {
  fn default() -> Self {
    Store { root: Cell::Map(Vec::new()) }
  }
}
impl Store {
  /// Wrap a prebuilt cell as a store's root (e.g. a `card_view` result, or an
  /// operating-set frame assembled by the gate).
  pub fn with_root(root: Cell) -> Store {
    Store { root }
  }
  /// Consume the store, yielding its root cell.
  pub fn into_root(self) -> Cell {
    self.root
  }
  /// Parse a path into steps, resolving `*`-interpolations against the local
  /// store (Int → index, Sym → map key — the latter is how `:*faction` works).
  fn parse(&self, path: &str) -> Vec<Seg> {
    let raw: Vec<&str> = path.split([':', '.']).filter(|s| !s.is_empty()).collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < raw.len() {
      if let Some(name) = raw[i].strip_prefix('*') {
        let mut sub = name.to_string();
        i += 1;
        while i < raw.len() && !raw[i].is_empty() && raw[i].bytes().all(|b| b.is_ascii_digit()) {
          sub.push('.');
          sub.push_str(raw[i]);
          i += 1;
        }
        match self.read_local(&sub) {
          Some(Cell::Sym(s)) => out.push(Seg::Key(s.clone())),
          Some(c) => out.push(Seg::Idx(c.as_int().max(0) as usize)),
          None => out.push(Seg::Idx(0)),
        }
      } else {
        out.push(Seg::Lit(raw[i].to_string()));
        i += 1;
      }
    }
    out
  }
  /// Local read (no catalog deref) — used for interpolation subpaths and
  /// simple in-place reads (`inc`, `normalize`, `&x count`).
  fn read_local(&self, path: &str) -> Option<&Cell> {
    walk_read(&self.root, &self.parse(path))
  }
  fn read_int(&self, path: &str) -> i64 {
    self.read_local(path).map(Cell::as_int).unwrap_or(0)
  }
  pub fn read(&self, path: &str) -> Option<&Cell> {
    self.read_local(path)
  }
  pub fn write(&mut self, path: &str, val: Cell) {
    let segs = self.parse(path);
    walk_write(&mut self.root, &segs, val);
  }
}

#[derive(Clone, Debug)]
enum Item {
  Val(i64),
  Sym(String),
  Addr(String),
  Label(String),
  Cell(Cell),
  /// `^name` — a pending system call (resolved by `call` against the host).
  Sys(String),
}
impl Item {
  fn int(&self) -> i64 {
    match self {
      Item::Val(n) => *n,
      Item::Cell(c) => c.as_int(),
      _ => 0,
    }
  }
  fn addr(&self) -> &str {
    match self {
      Item::Addr(a) => a,
      _ => "",
    }
  }
}

fn hash(x: i64) -> i64 {
  let mut z = (x as u64).wrapping_add(0x9E37_79B9_7F4A_7C15);
  z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
  z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
  z ^= z >> 31;
  (z >> 1) as i64
}

const STEP_CAP: u32 = 1_000_000;

/// Functions callable via `$functions:name call`.
#[derive(Default, Debug)]
pub struct Functions {
  map: HashMap<String, Vec<Stmt>>,
}
impl Functions {
  /// Register every `<functions:name>` code body from a parsed file.
  pub fn add(&mut self, root: &Node) {
    for b in &root.children {
      if let Header::Bucket(name) = &b.header {
        if name.starts_with("functions:") && !b.body.is_empty() {
          self.map.insert(name.clone(), b.body.clone());
        }
      }
    }
  }
  fn get(&self, name: &str) -> Option<&Vec<Stmt>> {
    self.map.get(name)
  }
}

/// A hold a recipe `@input` line acquires on a bound slot. The four verbs encode
/// the old `(slot_hold, position_hold)` policy (cf. recipe_tape): `use` =
/// exclusive but movable (the player's trigger card), `claim` = exclusive +
/// position-pinned (the default), `share` = non-exclusive + pinned, `borrow` =
/// existence-only (no exclusivity, no pin). The gate maps these to the cards-DB
/// acquire reducers; the VM only records *which* hold each slot needs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Hold {
  Use,
  Claim,
  Share,
  Borrow,
}
impl Hold {
  fn from_word(w: &str) -> Option<Hold> {
    match w {
      "use" => Some(Hold::Use),
      "claim" => Some(Hold::Claim),
      "share" => Some(Hold::Share),
      "borrow" => Some(Hold::Borrow),
      _ => None,
    }
  }
}

/// A server-side mutation a recipe `@output` emits, in source order. Slot/target
/// fields are unresolved path strings (`slot.1.0`, `slot.1.0.owner.inventory`) —
/// the gate resolves them against the operating set to concrete card ids, then
/// decomposes each into a cards/regions-DB reducer call at completion time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Effect {
  /// `&slot destroy` — mark the bound card dead.
  Destroy { slot: String },
  /// `$card::x &target create` — spawn `def` into `target` (`…owner.inventory`).
  Create { def: String, target: String },
  /// `&slot.aspect.x dec`/`inc`/`set` — per-row tile-stock mutation. `delta` is
  /// the signed change for inc/dec; `set` carries the absolute value with abs=true.
  Stock { slot: String, aspect: String, delta: i64, abs: bool },
  /// `$blueprint::x &…owner.blueprint set` — FLAGGED: blueprint unlock has no
  /// dedicated op yet (content guesses a slot `set`); recorded for the gate to
  /// resolve once the unlock semantics land. cf. project_definition_language.
  Blueprint { def: String, target: String },
}

/// What `exec` is running, so the recipe verbs know how to behave. `Data` is the
/// card/function hooks (verbs unused); `Input` records holds + tracks match;
/// `Output` records effects, styles, and duration.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
  Data,
  Input,
  Output,
}

/// The result of evaluating a recipe against one positioned operating-set frame
/// — the gate's IO contract (mirrors the old `ActionPlan`). `matched` is the
/// `@input` conjunction verdict; the rest is the `@output` tape.
#[derive(Default, Debug, PartialEq, Serialize, Deserialize)]
pub struct Plan {
  pub matched: bool,
  pub holds: Vec<(String, Hold)>,
  pub duration: i64,
  /// `(slot, style)` — progress-bar style (`rtl`/`ltr`) per bound card.
  pub styles: Vec<(String, String)>,
  pub effects: Vec<Effect>,
}

/// Run a hook/function body against `store`. `host` is the system-call table
/// (`^name call` looks up `name` here — deterministic engine-provided values like
/// biome/seed); `cat` is for ref-following derefs; `funcs` for `$functions:x call`.
pub fn run(body: &[Stmt], store: &mut Store, host: &[(String, Cell)], cat: &Catalog, funcs: &Functions) -> Result<(), String> {
  let mut plan = Plan::default();
  exec(body, store, host, cat, funcs, &mut plan, Mode::Data, 0).map(|_| ())
}

/// Evaluate a recipe's `@input` against a positioned operating-set `store`.
/// Every `@input` line is a `<predicate> if &slot <verb>` conjunct: a false
/// predicate skips its verb, so the recipe matches iff every line acquired its
/// hold. The gate applies frame-base `N` and owner re-anchors when it builds the
/// store, and slides the frame by re-calling with successive positions.
pub fn match_recipe(input: &[Stmt], store: &mut Store, cat: &Catalog, funcs: &Functions) -> Result<Plan, String> {
  let mut plan = Plan::default();
  exec(input, store, &[], cat, funcs, &mut plan, Mode::Input, 0)?;
  let expected = input.iter().filter(|s| matches!(s, Stmt::Instr(_))).count();
  plan.matched = plan.holds.len() == expected && expected > 0;
  Ok(plan)
}

/// Run a recipe's `@output` tape against the (matched) operating-set `store`,
/// collecting duration, per-card styles, and the ordered effect list.
pub fn plan_recipe(output: &[Stmt], store: &mut Store, cat: &Catalog, funcs: &Functions) -> Result<Plan, String> {
  let mut plan = Plan::default();
  plan.matched = true;
  exec(output, store, &[], cat, funcs, &mut plan, Mode::Output, 0)?;
  Ok(plan)
}

/// Returns the body's return value (`<val> ret`, or `Val(0)` on fall-through).
fn exec(body: &[Stmt], store: &mut Store, host: &[(String, Cell)], cat: &Catalog, funcs: &Functions, plan: &mut Plan, mode: Mode, depth: u32) -> Result<Item, String> {
  if depth > 64 {
    return Err("call depth exceeded".into());
  }
  let mut labels: Vec<(&str, usize)> = Vec::new();
  for (i, s) in body.iter().enumerate() {
    if let Stmt::LabelDef(n) = s {
      labels.push((n, i));
    }
  }
  let label = |n: &str| labels.iter().find(|(k, _)| *k == n).map(|(_, i)| *i);

  let mut call: Vec<usize> = Vec::new();
  let mut ip = 0usize;
  let mut steps = 0u32;

  while ip < body.len() {
    steps += 1;
    if steps > STEP_CAP {
      return Err("step cap exceeded".into());
    }
    let toks = match &body[ip] {
      Stmt::LabelDef(_) => {
        ip += 1;
        continue;
      }
      Stmt::Instr(t) => t,
    };

    let mut st: Vec<Item> = Vec::new();
    let mut next = ip + 1;
    'line: for tok in toks {
      match tok {
        Token::Number(n) => st.push(Item::Val(*n)),
        Token::Const(s) => st.push(Item::Sym(s.clone())),
        Token::System(s) => st.push(Item::Sys(s.clone())),
        Token::Slot(s) => st.push(Item::Addr(s.clone())),
        Token::Value(s) => match resolve(store, cat, s) {
          Some(Cell::Sym(sym)) => st.push(Item::Sym(sym)),
          Some(c @ (Cell::Arr(_) | Cell::Map(_))) => st.push(Item::Cell(c)),
          Some(c) => st.push(Item::Val(c.as_int())),
          None => st.push(Item::Val(0)),
        },
        Token::Label(s) => st.push(Item::Label(s.clone())),
        Token::Color(s) => {
          st.push(Item::Val(i64::from_str_radix(s.trim_start_matches('#'), 16).unwrap_or(0)));
        }
        Token::Word(w) => match w.as_str() {
          "set" => {
            let a = st.pop().unwrap();
            let val = st.pop().unwrap();
            let addr = a.addr().to_string();
            // In `@output`, `set` is path-dispatched the way the old engine's
            // `resolve_target` keyed on the path tail: a `.style` write is a
            // progress style, `sys.duration` is the action window, `.aspect.`
            // is a tile-stock op, `.blueprint` is a (flagged) unlock.
            if mode == Mode::Output {
              if addr == "sys.duration" {
                plan.duration = val.int();
              } else if let Some(slot) = addr.strip_suffix(".style") {
                let style = match &val { Item::Sym(s) => s.clone(), _ => String::new() };
                plan.styles.push((slot.to_string(), style));
              } else if let Some(i) = addr.find(".aspect.") {
                plan.effects.push(Effect::Stock {
                  slot: addr[..i].to_string(),
                  aspect: addr[i + ".aspect.".len()..].to_string(),
                  delta: val.int(),
                  abs: true,
                });
              } else if let Some(slot) = addr.strip_suffix(".blueprint") {
                let def = match &val { Item::Sym(s) => s.clone(), _ => String::new() };
                plan.effects.push(Effect::Blueprint { def, target: slot.to_string() });
              }
            }
            let cell = match val {
              Item::Sym(s) => Cell::Sym(s),
              Item::Cell(c) => c,
              other => Cell::Int(other.int()),
            };
            store.write(&addr, cell);
          }
          "drop" => {
            st.pop();
          }
          "add" | "sub" | "mul" | "div" | "mod" => {
            let b = st.pop().unwrap().int();
            let a = st.pop().unwrap().int();
            st.push(Item::Val(match w.as_str() {
              "add" => a + b,
              "sub" => a - b,
              "mul" => a * b,
              "div" => if b == 0 { 0 } else { a / b },
              _ => if b == 0 { 0 } else { a % b },
            }));
          }
          // equality is symbol-aware (so `$card::corpus *slot.1.0.def_id eq`
          // compares the ref ids, not 0==0); ordering stays numeric.
          "eq" | "ne" => {
            let b = st.pop().unwrap();
            let a = st.pop().unwrap();
            let equal = match (&a, &b) {
              (Item::Sym(x), Item::Sym(y)) => x == y,
              _ => a.int() == b.int(),
            };
            st.push(Item::Val((if w == "eq" { equal } else { !equal }) as i64));
          }
          "gt" | "ge" | "lt" | "le" => {
            let b = st.pop().unwrap().int();
            let a = st.pop().unwrap().int();
            st.push(Item::Val(match w.as_str() {
              "gt" => a > b,
              "ge" => a >= b,
              "lt" => a < b,
              _ => a <= b,
            } as i64));
          }
          "and" | "or" => {
            let b = st.pop().unwrap().int();
            let a = st.pop().unwrap().int();
            let r = if w == "and" { a != 0 && b != 0 } else { a != 0 || b != 0 };
            st.push(Item::Val(r as i64));
          }
          "not" => {
            let a = st.pop().unwrap().int();
            st.push(Item::Val((a == 0) as i64));
          }
          "within" => {
            let max = st.pop().unwrap().int();
            let min = st.pop().unwrap().int();
            let v = st.pop().unwrap().int();
            st.push(Item::Val((v >= min && v <= max) as i64));
          }
          "if" | "!if" => {
            let c = st.pop().unwrap().int() != 0;
            if !(if w == "if" { c } else { !c }) {
              break 'line;
            }
          }
          "goto" => {
            if let Some(Item::Label(n)) = st.pop() {
              next = label(&n).ok_or(format!("goto :{n} unresolved"))?;
            }
            break 'line;
          }
          "call" => match st.pop() {
            // local subroutine — jump within this body (no value)
            Some(Item::Label(n)) => {
              call.push(ip + 1);
              next = label(&n).ok_or(format!("call :{n} unresolved"))?;
              break 'line;
            }
            // global function — run inline over the same store; push its return
            Some(Item::Sym(s)) => {
              let r = match funcs.get(&s) {
                Some(fbody) => exec(fbody, store, host, cat, funcs, plan, mode, depth + 1)?,
                None => Item::Val(0),
              };
              st.push(r);
            }
            // system call — fetch the host-provided value and push it
            Some(Item::Sys(name)) => {
              let c = host.iter().find(|(k, _)| *k == name).map(|(_, c)| c.clone()).unwrap_or(Cell::Int(0));
              st.push(match c {
                Cell::Sym(s) => Item::Sym(s),
                c @ (Cell::Arr(_) | Cell::Map(_)) => Item::Cell(c),
                c => Item::Val(c.as_int()),
              });
            }
            _ => {}
          },
          "ret" => {
            // `<val> ret` returns one value (`0 ret` for void). If inside a local
            // subroutine, jump back (the value is unused); else exit the function.
            let val = st.pop().unwrap_or(Item::Val(0));
            match call.pop() {
              Some(addr) => {
                next = addr;
                break 'line;
              }
              None => return Ok(val),
            }
          }
          "inc" | "dec" => {
            let p = st.pop().unwrap();
            let addr = p.addr().to_string();
            let delta = if w == "inc" { 1 } else { -1 };
            // `&slot.aspect.x dec` in `@output` is a per-row tile-stock change,
            // not just a scratch counter (`&var.0 inc`) — emit a stock effect.
            if mode == Mode::Output {
              if let Some(i) = addr.find(".aspect.") {
                plan.effects.push(Effect::Stock {
                  slot: addr[..i].to_string(),
                  aspect: addr[i + ".aspect.".len()..].to_string(),
                  delta,
                  abs: false,
                });
              }
            }
            store.write(&addr, Cell::Int(store.read_int(&addr) + delta));
          }
          "count" => {
            let n = match st.pop().unwrap() {
              Item::Cell(c) => c.len(),
              Item::Addr(a) => store.read_local(&a).map(Cell::len).unwrap_or(0),
              _ => 0,
            };
            st.push(Item::Val(n));
          }
          "key" => {
            // `&map *i key` — the key (name Sym) at positional index i of a map.
            // The complement of positional value reads (`*map.*i`), so a keyed
            // map can be iterated by index and the name recovered at each step.
            let i = st.pop().unwrap().int().max(0) as usize;
            let name = match st.pop().unwrap() {
              Item::Addr(a) => match store.read_local(&a) {
                Some(Cell::Map(m)) => m.get(i).map(|(k, _)| k.clone()),
                _ => None,
              },
              Item::Cell(Cell::Map(m)) => m.get(i).map(|(k, _)| k.clone()),
              _ => None,
            };
            st.push(name.map(Item::Sym).unwrap_or(Item::Val(0)));
          }
          "recall" => {
            // `<id-sym> <ns> recall` — the catalog record for `<ns>::<id>`
            // (e.g. `*name aspect recall` → the `<aspect>` record), or 0 if none.
            // The explicit, non-shadow registry lookup: id + namespace on the
            // stack, never synthesized from an instance path.
            let ns = match st.pop().unwrap() { Item::Sym(s) => s, _ => String::new() };
            let id = match st.pop().unwrap() { Item::Sym(s) => s, _ => String::new() };
            let rec = cat.deref(&format!("{ns}::{id}")).cloned().unwrap_or(Cell::Int(0));
            st.push(match rec {
              Cell::Sym(s) => Item::Sym(s),
              c @ (Cell::Arr(_) | Cell::Map(_)) => Item::Cell(c),
              c => Item::Val(c.as_int()),
            });
          }
          "array" => {
            let a = st.pop().unwrap();
            let n = st.pop().unwrap().int().max(0) as usize;
            store.write(a.addr(), Cell::Arr(vec![Cell::Int(0); n]));
          }
          "range" => {
            let a = st.pop().unwrap();
            let max = st.pop().unwrap().int();
            let min = st.pop().unwrap().int();
            store.write(a.addr(), Cell::Ranged { min, max, val: 0 });
          }
          "vec2" => {
            // `<x> <y> &addr vec2` — set a 2-component vector as `{x, y}`
            // (anchor, and later object positions). Read via `*addr.x`/`*addr.y`.
            let a = st.pop().unwrap();
            let y = st.pop().unwrap().int();
            let x = st.pop().unwrap().int();
            store.write(a.addr(), Cell::Map(vec![("x".into(), Cell::Int(x)), ("y".into(), Cell::Int(y))]));
          }
          "normalize" => {
            let input = st.pop().unwrap().int();
            let a = st.pop().unwrap();
            let (min, max) = match store.read_local(a.addr()) {
              Some(Cell::Ranged { min, max, .. }) => (*min, *max),
              _ => (0, 0),
            };
            let val = min + (input.clamp(0, 100) * (max - min)) / 100;
            store.write(a.addr(), Cell::Ranged { min, max, val });
          }
          "stock" => {
            let a = st.pop().unwrap();
            let _bits = st.pop().unwrap().int();
            store.write(a.addr(), Cell::Int(0));
          }
          "random" => {
            let seed = st.pop().unwrap().int();
            st.push(Item::Val(hash(seed)));
          }
          // --- recipe @input: predicate passed, acquire this slot's hold ---
          "use" | "claim" | "share" | "borrow" if mode == Mode::Input => {
            let a = st.pop().unwrap();
            if let Some(h) = Hold::from_word(w) {
              plan.holds.push((a.addr().to_string(), h));
            }
          }
          // --- recipe @output: server mutations ---
          "destroy" if mode == Mode::Output => {
            let a = st.pop().unwrap();
            plan.effects.push(Effect::Destroy { slot: a.addr().to_string() });
          }
          "create" if mode == Mode::Output => {
            let target = st.pop().unwrap().addr().to_string();
            let def = match st.pop().unwrap() {
              Item::Sym(s) => s,
              _ => String::new(),
            };
            // a blueprint def created into `.blueprint` is an unlock (flagged);
            // everything else lands as a card in the target container.
            if let Some(slot) = target.strip_suffix(".blueprint") {
              plan.effects.push(Effect::Blueprint { def, target: slot.to_string() });
            } else {
              plan.effects.push(Effect::Create { def, target });
            }
          }
          "rtl" | "ltr" => st.push(Item::Sym(w.clone())),
          other => st.push(Item::Sym(other.to_string())),
        },
      }
    }
    ip = next;
  }
  Ok(Item::Val(0)) // fell off the end — void return
}

// ---------- Tests ----------

#[cfg(test)]
mod tests {
  use super::*;
  use crate::parser::parse;

  fn find<'a>(n: &'a Node, hook: &str) -> Option<&'a [Stmt]> {
    let hit = match &n.header {
      Header::Hook(h) => h == hook,
      Header::Bucket(b) => b == hook || b.ends_with(&format!(":{hook}")),
      _ => false,
    };
    if hit && !n.body.is_empty() {
      return Some(&n.body);
    }
    n.children.iter().find_map(|c| find(c, hook))
  }
  fn run_hook(src: &str, hook: &str, host: Vec<(String, Cell)>) -> Store {
    let root = parse(src).unwrap();
    let mut store = Store::default();
    run(find(&root, hook).expect("hook"), &mut store, &host, &Catalog::default(), &Functions::default()).unwrap();
    store
  }
  fn biome(rarity: i64, humidity: i64, elevation: i64) -> Vec<(String, Cell)> {
    vec![
      ("biome".into(), Cell::Map(vec![
        ("rarity".into(), Cell::Int(rarity)),
        ("humidity".into(), Cell::Int(humidity)),
        ("elevation".into(), Cell::Int(elevation)),
        ("temperature".into(), Cell::Int(50)),
      ])),
      ("seed".into(), Cell::Int(99)),
    ]
  }

  #[test]
  fn set_and_arithmetic() {
    assert_eq!(run_hook("<functions:f>\n  @define>\n    2 3 add &x set\n", "define", vec![]).read("x"), Some(&Cell::Int(5)));
  }
  #[test]
  fn vec2_sets_xy_pair() {
    let s = run_hook("<functions:f>\n  @define>\n    50 75 &anchor vec2\n", "define", vec![]);
    assert_eq!(s.read("anchor.x"), Some(&Cell::Int(50)));
    assert_eq!(s.read("anchor.y"), Some(&Cell::Int(75)));
  }
  #[test]
  fn sym_equality() {
    // def_id-style matching: same ref → true, different → false (not 0==0).
    let s = run_hook("<functions:f>\n  $card::corpus $card::corpus eq &a set\n  $card::corpus $card::dread eq &b set\n", "f", vec![]);
    assert_eq!(s.read("a"), Some(&Cell::Int(1)));
    assert_eq!(s.read("b"), Some(&Cell::Int(0)));
  }
  #[test]
  fn range_then_normalize() {
    let s = run_hook("<functions:f>\n  @define>\n    ^biome call &biome set\n    0 3 &aspect.pine range\n    &aspect.pine *biome.humidity normalize\n", "define", biome(7, 70, 40));
    assert_eq!(s.read("aspect.pine"), Some(&Cell::Ranged { min: 0, max: 3, val: 2 }));
  }
  #[test]
  fn within_and_goto_bucket() {
    let src = "<functions:f>\n  @define>\n    ^biome call &biome set\n    *biome.rarity 0 10 within !if :r10 goto\n    0 1 &aspect.pine range\n    :norm goto\n    :r10>\n    0 3 &aspect.stone range\n    :norm>\n    1 &done set\n";
    let s = run_hook(src, "define", biome(5, 70, 40));
    assert!(matches!(s.read("aspect.pine"), Some(Cell::Ranged { .. })));
    assert_eq!(s.read("aspect.stone"), None);
  }
  #[test]
  fn interpolated_index_read_write() {
    let s = run_hook("<functions:f>\n  @define>\n    5 &objects array\n    2 &var.0 set\n    9 &objects.*var.0 set\n    *objects.*var.0 &out set\n", "define", vec![]);
    assert_eq!(s.read("objects.2"), Some(&Cell::Int(9)));
    assert_eq!(s.read("out"), Some(&Cell::Int(9)));
  }

  // --- Part B: catalog + deref ---

  const ASSETS: &str = "\
<asset>
  ::pine>
    @define>
      $manifest::conifer &object set
      256 &size set
";
  const MANIFEST: &str = "\
<manifest>
  ::conifer>
    :neutral>
      @define>
        3 &texture array
        1.png &texture.0 set
        2.png &texture.1 set
        3.png &texture.2 set
";
  fn catalog() -> Catalog {
    let mut c = Catalog::default();
    c.add_assets(&parse(ASSETS).unwrap());
    c.add_manifest(&parse(MANIFEST).unwrap());
    c
  }

  #[test]
  fn catalog_loads() {
    let c = catalog();
    assert_eq!(c.assets["pine"], Cell::Map(vec![
      ("object".into(), Cell::Sym("manifest::conifer".into())),
      ("size".into(), Cell::Int(256)),
    ]));
    assert!(c.manifests.contains_key("conifer"));
  }

  #[test]
  fn instance_aspect_is_a_plain_scalar_no_shadow() {
    // A card holds an aspect as a plain scalar — the path does NOT shadow into
    // the registry. `aspect.pine` behaves exactly like any other local key; the
    // definition is reached via `$aspect::pine`, never by walking a card value.
    // (Contrast `deref_asset_to_manifest_texture`: an asset slot stores a real
    // `$asset::x` Sym, so following it is honest, not a synthesized shadow.)
    let aspects = "<aspect>\n  ::pine>\n    @define>\n      1 &satisfies array\n      $aspect::wood &satisfies.0 set\n";
    let mut cat = Catalog::default();
    cat.add_aspects(&parse(aspects).unwrap());
    let mut s = Store::default();
    s.write("slot.0.0.aspect.pine", Cell::Int(2));
    // the scalar reads back...
    assert_eq!(resolve(&s, &cat, "slot.0.0.aspect.pine"), Some(Cell::Int(2)));
    // ...but navigating past it does NOT reach the registry (no shadow)
    assert_eq!(resolve(&s, &cat, "slot.0.0.aspect.pine.satisfies.0"), None);
    assert_eq!(resolve(&s, &cat, "slot.0.0.aspect.pine.satisfies"), None);
  }

  #[test]
  fn deref_asset_to_manifest_texture() {
    // store: asset[0] = $asset::pine, faction = neutral, var.3 = 1
    let mut s = Store::default();
    s.write("asset", Cell::Arr(vec![Cell::Sym("asset::pine".into())]));
    s.write("faction", Cell::Sym("neutral".into()));
    s.write("var.3", Cell::Int(1));
    let body = parse("<functions:f>\n  @define>\n    *asset.0.object:*faction.texture.*var.3 &out set\n    *asset.0.object:*faction.texture count &n set\n").unwrap();
    run(find(&body, "define").unwrap(), &mut s, &[], &catalog(), &Functions::default()).unwrap();
    assert_eq!(s.read("out"), Some(&Cell::Sym("2.png".into()))); // texture[1]
    assert_eq!(s.read("n"), Some(&Cell::Int(3)));         // 3 textures
  }

  #[test]
  fn cross_function_call() {
    // caller invokes helper via `$functions:helper call`; they share the store.
    let src = "<functions:helper>\n  9 &shared set\n<functions:caller>\n  $functions:helper call drop\n  *shared 1 add &out set\n";
    let root = parse(src).unwrap();
    let mut funcs = Functions::default();
    funcs.add(&root);
    let mut s = Store::default();
    run(find(&root, "caller").unwrap(), &mut s, &[], &Catalog::default(), &funcs).unwrap();
    assert_eq!(s.read("shared"), Some(&Cell::Int(9)));
    assert_eq!(s.read("out"), Some(&Cell::Int(10)));
  }

  #[test]
  fn key_returns_name_at_index() {
    let s = run_hook("<functions:f>\n  @define>\n    2 &aspect.pine set\n    1 &aspect.flora set\n    &aspect 0 key &a set\n    &aspect 1 key &b set\n", "define", vec![]);
    assert_eq!(s.read("a"), Some(&Cell::Sym("pine".into())));
    assert_eq!(s.read("b"), Some(&Cell::Sym("flora".into())));
  }

  #[test]
  fn recall_looks_up_catalog_record_by_id() {
    let aspects = "<aspect>\n  ::pine>\n    @define>\n      aspects &section set\n      $asset::p &art set\n";
    let mut cat = Catalog::default();
    cat.add_aspects(&parse(aspects).unwrap());
    let body = parse("<functions:f>\n  pine aspect recall &rec set\n  *rec.art &got set\n  ghost aspect recall &none set\n").unwrap();
    let mut s = Store::default();
    run(find(&body, "f").unwrap(), &mut s, &[], &cat, &Functions::default()).unwrap();
    assert_eq!(s.read("got"), Some(&Cell::Sym("asset::p".into()))); // pine's art ref
    assert_eq!(s.read("none"), Some(&Cell::Int(0)));                 // missing -> 0
  }

  // The aspect-driven render: ring_objects pulls each stock aspect's sprite from
  // the <aspect> registry (no per-card asset list), and skips aspects with no art.
  const RING_OBJECTS: &str = "\
<functions:ring_objects>
  7 &objects array
  0 &var.0 set
  0 &var.2 set
  ^seed call &seed set
  :aspect>
    *var.2 &aspect count ge if 0 ret
    *var.0 &objects count ge if 0 ret
    &aspect *var.2 key &name set
    *name aspect recall &rec set
    *rec.art.object:*faction.texture count &var.4 set
    *var.4 0 eq if :next goto
    0 &var.1 set
    :place>
      *var.1 *aspect.*var.2 ge if :next goto
      *var.0 &objects count ge if 0 ret
      *seed *var.0 add random *var.4 mod &var.3 set
      *rec.art.object:*faction.texture.*var.3 &objects.*var.0 set
      &var.0 inc
      &var.1 inc
      :place goto
    :next>
    &var.2 inc
    :aspect goto
";

  #[test]
  fn ring_objects_renders_from_aspect_art() {
    let aspects = "<aspect>\n  ::pine>\n    @define>\n      aspects &section set\n      $asset::p &art set\n  ::cost>\n    @define>\n      traits &section set\n";
    let assets = "<asset>\n  ::p>\n    @define>\n      $manifest::m &object set\n";
    let manifest = "<manifest>\n  ::m>\n    :neutral>\n      @define>\n        2 &texture array\n        a.png &texture.0 set\n        b.png &texture.1 set\n";
    let mut cat = Catalog::default();
    cat.add_aspects(&parse(aspects).unwrap());
    cat.add_assets(&parse(assets).unwrap());
    cat.add_manifest(&parse(manifest).unwrap());

    let ro = parse(RING_OBJECTS).unwrap();
    let mut s = Store::default();
    s.write("aspect.pine", Cell::Int(2));   // 2 pines → 2 objects
    s.write("aspect.cost", Cell::Int(30));  // no art → skipped (not 30 garbage objects)
    s.write("faction", Cell::Sym("neutral".into()));
    let host = vec![("seed".to_string(), Cell::Int(7))];
    run(find(&ro, "ring_objects").unwrap(), &mut s, &host, &cat, &Functions::default()).unwrap();

    // exactly two objects placed (pine), both real textures; the rest stay 0
    assert!(matches!(s.read("objects.0"), Some(Cell::Sym(_))), "{:?}", s.read("objects.0"));
    assert!(matches!(s.read("objects.1"), Some(Cell::Sym(_))), "{:?}", s.read("objects.1"));
    assert_eq!(s.read("objects.2"), Some(&Cell::Int(0))); // cost contributed nothing
  }

  // --- Part C: recipe execution (@input match + @output plan) ---

  /// Find a `::def` record by id, then one of its hooks (`input`/`output`).
  fn recipe_hook<'a>(root: &'a Node, def: &str, hook: &str) -> &'a [Stmt] {
    fn find_def<'a>(n: &'a Node, def: &str) -> Option<&'a Node> {
      if matches!(&n.header, Header::Def(d) if d == def) {
        return Some(n);
      }
      n.children.iter().find_map(|c| find_def(c, def))
    }
    find(find_def(root, def).expect("def"), hook).expect("hook")
  }
  fn cat_funcs() -> (Catalog, Functions) {
    (Catalog::default(), Functions::default())
  }

  const TRIPLE: &str = "\
<recipe>
  ::triple_corpus>
    @input>
      $card::corpus *slot.1.0.def_id eq if &slot.1.0 use
      $card::corpus *slot.1.1.def_id eq if &slot.1.1 claim
      $card::corpus *slot.1.2.def_id eq if &slot.1.2 claim
    @output>
      10 &sys.duration set
      rtl &slot.1.0.style set
      &slot.1.0 destroy
";
  fn corpus3() -> Store {
    let mut s = Store::default();
    for b in 0..3 {
      s.write(&format!("slot.1.{b}.def_id"), Cell::Sym("card::corpus".into()));
    }
    s
  }

  #[test]
  fn recipe_input_matches_conjunction() {
    let root = parse(TRIPLE).unwrap();
    let (c, f) = cat_funcs();
    let plan = match_recipe(recipe_hook(&root, "triple_corpus", "input"), &mut corpus3(), &c, &f).unwrap();
    assert!(plan.matched);
    assert_eq!(plan.holds, vec![
      ("slot.1.0".into(), Hold::Use),
      ("slot.1.1".into(), Hold::Claim),
      ("slot.1.2".into(), Hold::Claim),
    ]);
  }

  #[test]
  fn recipe_input_fails_when_a_predicate_fails() {
    let root = parse(TRIPLE).unwrap();
    let (c, f) = cat_funcs();
    let mut s = corpus3();
    s.write("slot.1.2.def_id", Cell::Sym("card::dread".into())); // last slot wrong
    let plan = match_recipe(recipe_hook(&root, "triple_corpus", "input"), &mut s, &c, &f).unwrap();
    assert!(!plan.matched);
    assert_eq!(plan.holds.len(), 2); // first two fired, third predicate skipped its hold
  }

  #[test]
  fn recipe_output_tape() {
    let root = parse(TRIPLE).unwrap();
    let (c, f) = cat_funcs();
    let plan = plan_recipe(recipe_hook(&root, "triple_corpus", "output"), &mut corpus3(), &c, &f).unwrap();
    assert_eq!(plan.duration, 10);
    assert_eq!(plan.styles, vec![("slot.1.0".into(), "rtl".into())]);
    assert_eq!(plan.effects, vec![Effect::Destroy { slot: "slot.1.0".into() }]);
  }

  const FLEETING: &str = "\
<recipe>
  ::fleeting>
    @input>
      *root.aspect.fleeting 1 ge if &root borrow
    @output>
      *root.aspect.fleeting &var.0 set
      5 &sys.duration set
      *var.0 2 ge if 10 &sys.duration set
      *var.0 3 ge if 15 &sys.duration set
      *var.0 4 ge if 20 &sys.duration set
      rtl &root.style set
      &root destroy
";

  #[test]
  fn recipe_fleeting_scales_duration() {
    let root = parse(FLEETING).unwrap();
    let (c, f) = cat_funcs();
    let mut s = Store::default();
    s.write("root.aspect.fleeting", Cell::Int(3)); // fleeting=3 -> ge2, ge3, !ge4
    let mp = match_recipe(recipe_hook(&root, "fleeting", "input"), &mut s, &c, &f).unwrap();
    assert!(mp.matched);
    assert_eq!(mp.holds, vec![("root".into(), Hold::Borrow)]);
    let pp = plan_recipe(recipe_hook(&root, "fleeting", "output"), &mut s, &c, &f).unwrap();
    assert_eq!(pp.duration, 15);
    assert_eq!(pp.effects, vec![Effect::Destroy { slot: "root".into() }]);
  }

  const CUT_TREE: &str = "\
<recipe>
  ::cut_tree>
    @input>
      *slot.0.0.aspect.wood 1 ge if &slot.0.0 use
      *slot.1.0.aspect.corpus_lit 1 ge if &slot.1.0 claim
      $card::axe *slot.1.0.owner.slot.1.0.def_id eq if &slot.1.0.owner.slot.1.0 share
    @output>
      10 &sys.duration set
      ltr &slot.1.0.style set
      &slot.1.0 destroy
      &slot.0.0.aspect.wood dec
      $card::corpus_dim &slot.1.0.owner.inventory create
      $card::blueprint_nd_furnace &slot.1.0.owner.blueprint set
";

  #[test]
  fn recipe_cut_tree_aspect_owner_stock_create() {
    let root = parse(CUT_TREE).unwrap();
    let (c, f) = cat_funcs();
    let mut s = Store::default();
    s.write("slot.0.0.aspect.wood", Cell::Int(3));          // tile-stock total (host pre-summed)
    s.write("slot.1.0.aspect.corpus_lit", Cell::Int(1));
    s.write("slot.1.0.owner.slot.1.0.def_id", Cell::Sym("card::axe".into())); // owner re-anchor baked in
    let mp = match_recipe(recipe_hook(&root, "cut_tree", "input"), &mut s, &c, &f).unwrap();
    assert!(mp.matched);
    assert_eq!(mp.holds, vec![
      ("slot.0.0".into(), Hold::Use),
      ("slot.1.0".into(), Hold::Claim),
      ("slot.1.0.owner.slot.1.0".into(), Hold::Share),
    ]);
    let pp = plan_recipe(recipe_hook(&root, "cut_tree", "output"), &mut s, &c, &f).unwrap();
    assert_eq!(pp.duration, 10);
    assert_eq!(pp.styles, vec![("slot.1.0".into(), "ltr".into())]);
    assert_eq!(pp.effects, vec![
      Effect::Destroy { slot: "slot.1.0".into() },
      Effect::Stock { slot: "slot.0.0".into(), aspect: "wood".into(), delta: -1, abs: false },
      Effect::Create { def: "card::corpus_dim".into(), target: "slot.1.0.owner.inventory".into() },
      Effect::Blueprint { def: "card::blueprint_nd_furnace".into(), target: "slot.1.0.owner".into() },
    ]);
  }
}
