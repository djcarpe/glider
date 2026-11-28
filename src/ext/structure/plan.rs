//! The plan: whether writing a part of the graph with templates and chain
//! patterns is shorter than writing it plainly, decided by writing it.
//!
//! Every text here is made of per-node blocks (see [`super::notation`]),
//! whose lengths add up exactly, so a candidate is priced on the blocks it
//! changes: the cost of defining it once, the cost of each use, and the
//! cost of the plain blocks a use replaces. The choice, deterministic:
//!
//! 1. Templates, largest first (then by estimated saving, then id): a
//!    template is chosen when the uses that are shorter than what they
//!    replace, among its instances not already inside a chosen instance,
//!    save more than its definition costs. Larger first means a chosen
//!    instance never has a chosen instance inside it.
//! 2. Nesting, smallest first: a template is used inside a chosen one's
//!    definition (`.k=Tm`) when every use of the outer one has an instance
//!    of it there and the outer definition and uses together get shorter,
//!    by more than the inner definition costs when it is not yet chosen.
//! 3. Further uses: an instance of a chosen template that no chosen
//!    instance covers is used when its use is shorter than its plain text.
//! 4. Chains, pattern by pattern: a chain with property-less edges whose
//!    nodes are written plainly is written as one hop when that shortens
//!    the blocks it touches, and a pattern is kept when its chains save
//!    more than its definition costs.
//!
//! Then both texts are written in full and measured. The saving reported
//! is plain minus compact, exactly; when it is not positive nothing is
//! chosen and the compact text is the plain text. No candidate here leaves
//! anything out, so `lambda:` (the weight of what a lossy candidate would
//! omit) is recorded and has nothing to weigh.

use std::collections::{HashMap, HashSet};

use super::notation::{self, Cost, Names, Sink, NOTATION};
use super::paths::{chain_at, Chain};
use super::templates::{self as tp, Forest, Found, Hier, Prop, SlotRef, TEdge, Template, To};
use super::{begin, fresh_arg, is_fresh, limit_arg, seal, sip128, step, store_name, Fresh, NS};
use crate::codec::{self, Reader};
use crate::ext::Args;
use crate::graph::{Dir, Error, Graph, Result};
use crate::ooc::StateVec;
use crate::query::QueryResult;
use crate::value::Value;

/// The version of the code that writes dictionaries, kept with each (D3).
pub const CODE: &str = "structure-templates/1";

/// What a reader needs to read a dictionary, written at its head.
pub const LEGEND: &str = "# .k: a template's k-th node in preorder, .1 its root; $: a slot, filled in order by each use; ~\"x\": the root's value of the same key, then x; ~N\"x\": its first N bytes, then x; .k=Tm: template Tm from .k on; a -Pm-> b [x y]: the run a -> x -> y -> b along pattern Pm\n";

const BODY: &str = "# body\n";
const CHUNK: usize = 60_000;

/// A chain pattern and the chains written with it.
#[derive(Clone, Debug)]
pub struct Pattern {
    pub id: String,
    pub text: String,
    pub chains: Vec<Chain>,
}

/// A dictionary: templates in order (`T1`, `T2`, ...; nested ones refer
/// to their place in this list) and chain patterns (`P1`, ...).
#[derive(Clone, Debug, Default)]
pub struct Dict {
    pub templates: Vec<Template>,
    pub patterns: Vec<(String, String)>,
}

impl Dict {
    /// The dictionary's text: the legend, each definition, each pattern.
    pub fn text(&self, g: &Graph, names: &Names) -> String {
        let mut s = String::from(LEGEND);
        for (i, t) in self.templates.iter().enumerate() {
            let short = |u: usize| format!("T{}", u + 1);
            s.push_str(
                &tp::definition(g, names, t, &self.templates, &format!("T{}", i + 1), &short).0,
            );
        }
        for (i, (id, text)) in self.patterns.iter().enumerate() {
            s.push_str(&format!("P{} #{id} {text}\n", i + 1));
        }
        s
    }
}

pub fn hash_hex(text: &str) -> String {
    format!("{:032x}", sip128(text.as_bytes()))
}

/// What the planner decides by.
fn by_tokens(a: &Args) -> Result<bool> {
    match a.str("by").map(|s| s.to_ascii_lowercase()).as_deref() {
        None | Some("tokens") => Ok(true),
        Some("bytes") => Ok(false),
        Some(o) => Err(Error::Msg(format!(
            "by: '{o}' is not a measure; use \"tokens\" (as context budgets) or \"bytes\""
        ))),
    }
}

fn lambda(a: &Args) -> Result<f64> {
    match a.get("lambda") {
        None | Some(Value::Null) => Ok(1.0),
        Some(v) => match v.as_f64() {
            Some(x) if x >= 0.0 && x.is_finite() => Ok(x),
            _ => Err(Error::Msg(format!(
                "lambda: {v} is not a weight; give a number 0 or more"
            ))),
        },
    }
}

struct Planner<'a> {
    g: &'a Graph,
    h: Hier,
    tokens: bool,
    /// Definitions come from a kept dictionary: they cost this text nothing
    /// and are not changed.
    cached: bool,
    found: Found,
    part: Option<HashSet<u64>>,
    covered: StateVec<bool>,
    /// Per node position: 1 + the template it is a used root of.
    root_of: StateVec<u64>,
    /// Per entry of `found.roots`: used.
    used: StateVec<bool>,
    in_dict: Vec<bool>,
    /// Per template: what nesting it saved other definitions and uses, as
    /// estimated when it was nested.
    nested_save: Vec<(i64, i64)>,
    pats: Vec<Pattern>,
    /// Kept pattern ids, when cached: only these may be used.
    kept_pats: Vec<String>,
    omit: HashSet<u64>,
    extra: HashMap<u64, Vec<(usize, Chain)>>,
}

impl<'a> Planner<'a> {
    fn by(&self, c: Cost) -> i64 {
        c.by(self.tokens)
    }

    fn pair_by(&self, c: (u64, u64)) -> i64 {
        if self.tokens {
            c.1 as i64
        } else {
            c.0 as i64
        }
    }

    fn pos(&mut self, id: u64) -> usize {
        self.found.forest.pos(id).expect("a node of the forest")
    }

    fn in_part(&self, id: u64) -> bool {
        self.part.as_ref().map_or(true, |p| p.contains(&id))
    }

    fn members(&mut self, root: u64) -> Result<Vec<u64>> {
        Ok(tp::order(self.g, &mut self.found.forest, &self.h, root)?.0)
    }

    /// Whether every node under `root` is in the part and written plainly.
    fn free(&mut self, root: u64) -> Result<Option<Vec<u64>>> {
        let ids = self.members(root)?;
        for &id in &ids {
            if !self.in_part(id) {
                return Ok(None);
            }
            let p = self.pos(id);
            if self.covered.get(p) {
                return Ok(None);
            }
        }
        Ok(Some(ids))
    }

    fn take(&mut self, t: usize, r: usize, ids: &[u64]) {
        self.used.set(r, true);
        for &id in ids {
            let p = self.pos(id);
            self.covered.set(p, true);
        }
        let p = self.pos(ids[0]);
        self.root_of.set(p, t as u64 + 1);
    }

    fn def_cost(&self, t: usize) -> i64 {
        if self.cached {
            0
        } else {
            self.by(self.found.def_cost[t])
        }
    }

    fn select(&mut self) -> Result<()> {
        let n = self.found.templates.len();
        let mut order: Vec<(usize, usize, i64)> = Vec::with_capacity(n);
        for i in 0..n {
            let s = tp::saving(&mut self.found, i);
            order.push((
                i,
                self.found.templates[i].nodes(),
                if self.tokens { s.1 } else { s.0 },
            ));
        }
        order.sort_by(|a, b| {
            (b.1, b.2).cmp(&(a.1, a.2)).then_with(|| {
                self.found.templates[a.0]
                    .id
                    .cmp(&self.found.templates[b.0].id)
            })
        });
        for (i, _, _) in order {
            let (start, count) = (self.found.templates[i].start, self.found.templates[i].count);
            let mut gain = 0i64;
            let mut picks: Vec<(usize, Vec<u64>)> = Vec::new();
            for r in start..start + count {
                step(1)?;
                let root = self.found.roots.get(r);
                let p = self.pos(root);
                if self.covered.get(p) || !self.in_part(root) {
                    continue;
                }
                let (pc, uc) = (self.found.plain_cost.get(r), self.found.use_cost.get(r));
                let gi = self.pair_by(pc) - self.pair_by(uc);
                if gi <= 0 {
                    continue;
                }
                let Some(ids) = self.free(root)? else {
                    continue;
                };
                gain += gi;
                picks.push((r, ids));
            }
            if !picks.is_empty() && gain > self.def_cost(i) {
                self.in_dict[i] = true;
                for (r, ids) in picks {
                    self.take(i, r, &ids);
                }
            }
        }
        Ok(())
    }

    fn used_roots(&mut self, t: usize) -> Vec<(usize, u64)> {
        let (start, count) = (self.found.templates[t].start, self.found.templates[t].count);
        let mut v = Vec::new();
        for r in start..start + count {
            if self.used.get(r) {
                v.push((r, self.found.roots.get(r)));
            }
        }
        v
    }

    /// The template each root belongs to.
    fn root_index(&mut self) -> HashMap<u64, usize> {
        let mut m = HashMap::new();
        for (i, t) in self.found.templates.iter().enumerate() {
            for r in t.start..t.start + t.count {
                m.insert(self.found.roots.get(r), i);
            }
        }
        m
    }

    /// The change nesting `u` at `p` in `t` makes to `t`'s definition and
    /// its uses, and the template and use costs it would leave.
    fn try_nest(
        &mut self,
        t: usize,
        p: u32,
        u: usize,
    ) -> Result<(i64, Template, Cost, Vec<(usize, (u64, u64))>)> {
        let g = self.g;
        let t2 = tp::nest(
            g,
            &self.h.names,
            &self.found.templates[t],
            p,
            u,
            &self.found.templates,
        );
        let def2 = Cost::of(
            &tp::definition(g, &self.h.names, &t2, &self.found.templates, "T", &|_| {
                "T".into()
            })
            .0,
        );
        let mut delta = self.by(def2) - self.by(self.found.def_cost[t]);
        let mut costs = Vec::new();
        // A use is its name, its ids and one value per slot, apart: when
        // the slots are the same ones, every use costs what it did.
        let key = |x: &SlotRef| (x.pos, x.edge, x.key);
        let mut before: Vec<(u32, u32, u32)> =
            self.found.templates[t].slots.iter().map(key).collect();
        let mut after: Vec<(u32, u32, u32)> = t2.slots.iter().map(key).collect();
        before.sort_unstable();
        after.sort_unstable();
        if before == after {
            return Ok((delta, t2, def2, costs));
        }
        for (r, root) in self.used_roots(t) {
            step(1)?;
            let inst = tp::walk(g, &mut self.found.forest, &self.h, root)?;
            let c = Cost::of(&tp::use_text(g, &self.h.names, &t2, &inst, "T"));
            let old = self.found.use_cost.get(r);
            delta += self.by(c) - self.pair_by(old);
            costs.push((r, (c.bytes, c.tokens)));
        }
        Ok((delta, t2, def2, costs))
    }

    fn nesting(&mut self) -> Result<()> {
        let by_root = self.root_index();
        let mut order: Vec<usize> = (0..self.found.templates.len()).collect();
        order.sort_by(|&a, &b| {
            let (x, y) = (&self.found.templates[a], &self.found.templates[b]);
            (x.nodes(), &x.id).cmp(&(y.nodes(), &y.id))
        });
        // Where each template could go: in a chosen one, at a position
        // where every use of the chosen one has an instance of it.
        let mut places: HashMap<usize, Vec<(usize, u32)>> = HashMap::new();
        for t in 0..self.found.templates.len() {
            if !self.in_dict[t] {
                continue;
            }
            let roots = self.used_roots(t);
            if roots.is_empty() {
                continue;
            }
            let tn = self.found.templates[t].nodes();
            let ids0 = self.members(roots[0].1)?;
            let mut cands: Vec<(u32, usize)> = (1..ids0.len())
                .filter_map(|p| by_root.get(&ids0[p]).map(|&u| (p as u32, u)))
                .filter(|&(_, u)| self.found.templates[u].nodes() < tn)
                .collect();
            for &(_, root) in &roots[1..] {
                if cands.is_empty() {
                    break;
                }
                step(1)?;
                let ids = self.members(root)?;
                cands.retain(|&(p, u)| by_root.get(&ids[p as usize]) == Some(&u));
            }
            for (p, u) in cands {
                places.entry(u).or_default().push((t, p));
            }
        }
        for &u in &order {
            let un = self.found.templates[u].nodes();
            let mut opps: Vec<(usize, u32)> = Vec::new();
            let mut last: Option<(usize, u32)> = None;
            for &(t, p) in places.get(&u).map(|v| v.as_slice()).unwrap_or(&[]) {
                let ranges = tp::nested_ranges(&self.found.templates[t], &self.found.templates);
                let blocked = ranges.iter().any(|&(a, b, v)| {
                    (p > a && p < b) || (p == a && self.found.templates[v].nodes() >= un)
                });
                // Nested parts may not overlap: keep the first of any two.
                let overlaps = matches!(last, Some((lt, end)) if lt == t && p < end);
                if !blocked && !overlaps {
                    opps.push((t, p));
                    last = Some((t, p + un as u32));
                }
            }
            if opps.is_empty() {
                continue;
            }
            let mut undo: Vec<(usize, Template, Cost, Vec<(usize, (u64, u64))>)> = Vec::new();
            let mut sum = 0i64;
            for (t, p) in opps {
                let (delta, t2, def2, costs) = self.try_nest(t, p, u)?;
                if delta >= 0 {
                    continue;
                }
                let old_costs: Vec<(usize, (u64, u64))> = costs
                    .iter()
                    .map(|&(r, _)| (r, self.found.use_cost.get(r)))
                    .collect();
                undo.push((
                    t,
                    self.found.templates[t].clone(),
                    self.found.def_cost[t],
                    old_costs,
                ));
                self.found.templates[t] = t2;
                self.found.def_cost[t] = def2;
                for (r, c) in costs {
                    self.found.use_cost.set(r, c);
                }
                sum += delta;
            }
            if undo.is_empty() {
                continue;
            }
            if self.in_dict[u] || sum + self.def_cost(u) < 0 {
                self.in_dict[u] = true;
                let save = -sum;
                let ns = &mut self.nested_save[u];
                if self.tokens {
                    ns.1 += save;
                } else {
                    ns.0 += save;
                }
            } else {
                for (t, old, def, costs) in undo.into_iter().rev() {
                    self.found.templates[t] = old;
                    self.found.def_cost[t] = def;
                    for (r, c) in costs {
                        self.found.use_cost.set(r, c);
                    }
                }
            }
        }
        Ok(())
    }

    fn more_uses(&mut self) -> Result<()> {
        let mut order: Vec<usize> = (0..self.found.templates.len())
            .filter(|&i| self.in_dict[i])
            .collect();
        order.sort_by(|&a, &b| {
            let (x, y) = (&self.found.templates[a], &self.found.templates[b]);
            (y.nodes(), &x.id).cmp(&(x.nodes(), &y.id))
        });
        for t in order {
            let (start, count) = (self.found.templates[t].start, self.found.templates[t].count);
            for r in start..start + count {
                step(1)?;
                if self.used.get(r) {
                    continue;
                }
                let root = self.found.roots.get(r);
                if !self.in_part(root) {
                    continue;
                }
                let Some(ids) = self.free(root)? else {
                    continue;
                };
                let g = self.g;
                let inst = tp::walk(g, &mut self.found.forest, &self.h, root)?;
                let u = Cost::of(&tp::use_text(
                    g,
                    &self.h.names,
                    &self.found.templates[t],
                    &inst,
                    "T",
                ));
                let p = Cost::of(&tp::plain_text(g, &self.h.names, &inst));
                self.found.use_cost.set(r, (u.bytes, u.tokens));
                self.found.plain_cost.set(r, (p.bytes, p.tokens));
                if self.by(p) > self.by(u) {
                    self.take(t, r, &ids);
                }
            }
        }
        Ok(())
    }

    /// A node's block as the compact text writes it when it is not in a
    /// used instance: plain, less the edges chains carry, with the chains
    /// that start there.
    fn block(&self, v: u64, pnames: &dyn Fn(usize) -> String) -> String {
        let g = self.g;
        let names = &self.h.names;
        let mut s = String::new();
        notation::plain_block(g, names, v, &|e| self.omit.contains(&e), &mut s);
        if let Some(list) = self.extra.get(&v) {
            for (pi, c) in list {
                s.push_str(&chain_use(g, names, c, &pnames(*pi)));
            }
        }
        s
    }

    fn chains(&mut self) -> Result<()> {
        let g = self.g;
        let mut by: HashMap<String, Pattern> = HashMap::new();
        let mut heads: Vec<u64> = Vec::new();
        match &self.part {
            Some(p) => {
                heads.extend(p.iter().copied());
                heads.sort_unstable();
            }
            None => {
                g.scan_nodes(None, 0, usize::MAX, &mut |id| {
                    heads.push(id);
                    true
                });
            }
        }
        for head in heads {
            step(1)?;
            let Some(c) = chain_at(g, &self.h, head)? else {
                continue;
            };
            let mut ok = self.in_part(c.from) && c.props_free(g);
            for &x in std::iter::once(&c.from).chain(c.interior.iter()) {
                if !ok {
                    break;
                }
                ok = self.in_part(x) && {
                    let p = self.pos(x);
                    !self.covered.get(p)
                };
            }
            if !ok {
                continue;
            }
            let id = c.pattern_id(self.h.dir);
            if self.cached && !self.kept_pats.contains(&id) {
                continue;
            }
            let text = c.pattern(self.h.dir).trim().to_string();
            by.entry(id.clone())
                .or_insert_with(|| Pattern {
                    id,
                    text,
                    chains: Vec::new(),
                })
                .chains
                .push(c);
        }
        let mut pats: Vec<Pattern> = by.into_values().collect();
        pats.sort_by(|a, b| {
            b.chains
                .len()
                .cmp(&a.chains.len())
                .then_with(|| a.id.cmp(&b.id))
        });
        let provisional = |_: usize| "P0".to_string();
        for pat in pats {
            let pi = self.pats.len();
            let def = if self.cached {
                0
            } else {
                self.by(Cost::of(&format!("P0 #{} {}\n", pat.id, pat.text)))
            };
            let mut sum = 0i64;
            let mut applied: Vec<Chain> = Vec::new();
            for c in &pat.chains {
                step(1)?;
                let nodes: Vec<u64> = std::iter::once(c.from)
                    .chain(c.interior.iter().copied())
                    .collect();
                let before: i64 = nodes
                    .iter()
                    .map(|&v| self.by(Cost::of(&self.block(v, &provisional))))
                    .sum();
                for &e in &c.edges {
                    self.omit.insert(e);
                }
                self.extra.entry(c.from).or_default().push((pi, c.clone()));
                let after: i64 = nodes
                    .iter()
                    .map(|&v| self.by(Cost::of(&self.block(v, &provisional))))
                    .sum();
                if after < before {
                    sum += after - before;
                    applied.push(c.clone());
                } else {
                    self.unapply(c);
                }
            }
            if !applied.is_empty() && sum + def < 0 {
                self.pats.push(Pattern {
                    id: pat.id,
                    text: pat.text,
                    chains: applied,
                });
            } else {
                for c in &applied {
                    self.unapply(c);
                }
            }
        }
        Ok(())
    }

    fn unapply(&mut self, c: &Chain) {
        for e in &c.edges {
            self.omit.remove(e);
        }
        if let Some(list) = self.extra.get_mut(&c.from) {
            if let Some(i) = list
                .iter()
                .rposition(|(_, x)| x.interior[0] == c.interior[0])
            {
                list.remove(i);
            }
            if list.is_empty() {
                self.extra.remove(&c.from);
            }
        }
    }
}

/// `a -P3-> b [x y]`.
pub fn chain_use(g: &Graph, names: &Names, c: &Chain, name: &str) -> String {
    let mut s = names.handle(g, c.from);
    s.push_str(" -");
    s.push_str(name);
    s.push_str("-> ");
    s.push_str(&names.handle(g, c.to));
    s.push_str(" [");
    for (i, &x) in c.interior.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(&names.handle(g, x));
    }
    s.push_str("]\n");
    s
}

/// The nodes of the part in the order texts write them: by id, or by
/// handle under `identity:` so the order depends on content only.
fn node_order(g: &Graph, names: &Names, part: &Option<HashSet<u64>>) -> Result<Vec<u64>> {
    let mut ids: Vec<u64> = match part {
        Some(p) => p
            .iter()
            .copied()
            .filter(|&id| g.node_shared(id).is_some())
            .collect(),
        None => {
            let mut v = Vec::new();
            g.scan_nodes(None, 0, usize::MAX, &mut |id| {
                v.push(id);
                true
            });
            v
        }
    };
    if names.identity.is_empty() {
        ids.sort_unstable();
        return Ok(ids);
    }
    let mut s = super::sorter(g, "structure-plan-order")?;
    for id in ids {
        step(1)?;
        let mut k = names.handle(g, id).into_bytes();
        k.push(0);
        k.extend_from_slice(&id.to_be_bytes());
        s.push(&k, &id.to_be_bytes()).map_err(Error::Io)?;
    }
    let mut out = Vec::new();
    let mut sorted = s.finish().map_err(Error::Io)?;
    while let Some((_, v)) = sorted.next().map_err(Error::Io)? {
        out.push(u64::from_be_bytes(v[..8].try_into().unwrap()));
    }
    Ok(out)
}

/// Per chosen template: uses, their cost, the plain text they replace.
#[derive(Clone, Copy, Default)]
struct Tally {
    uses: u64,
    used: Cost,
    plain: Cost,
}

struct Written {
    plain: Sink,
    compact: Sink,
    dict: Option<(Dict, String, String)>,
    tally: Vec<Tally>,
    pat_tally: Vec<(Cost, Cost)>,
}

impl Planner<'_> {
    /// Write the part plainly and with what was chosen, measuring both.
    fn write(&mut self, keep: bool, kept: Option<(&Dict, &str)>) -> Result<Written> {
        let g = self.g;
        // Dictionary order: smaller templates first, so a nested one is
        // defined before the one that uses it.
        let mut chosen: Vec<usize> = (0..self.found.templates.len())
            .filter(|&i| self.in_dict[i])
            .collect();
        chosen.sort_by(|&a, &b| {
            let (x, y) = (&self.found.templates[a], &self.found.templates[b]);
            (x.nodes(), &x.id).cmp(&(y.nodes(), &y.id))
        });
        let mut slot_of: HashMap<usize, usize> = HashMap::new();
        for (k, &i) in chosen.iter().enumerate() {
            slot_of.insert(i, k);
        }
        let mut pat_order: Vec<usize> = (0..self.pats.len()).collect();
        pat_order.sort_by(|&a, &b| self.pats[a].id.cmp(&self.pats[b].id));
        let mut pat_slot = vec![0usize; self.pats.len()];
        for (k, &i) in pat_order.iter().enumerate() {
            pat_slot[i] = k;
        }
        let tnames: Vec<String> = (0..self.found.templates.len())
            .map(|i| match kept {
                Some((d, _)) => {
                    let id = &self.found.templates[i].id;
                    format!(
                        "T{}",
                        d.templates.iter().position(|t| &t.id == id).unwrap_or(0) + 1
                    )
                }
                None => slot_of
                    .get(&i)
                    .map(|k| format!("T{}", k + 1))
                    .unwrap_or_default(),
            })
            .collect();
        let pnames: Vec<String> = (0..self.pats.len())
            .map(|i| match kept {
                Some((d, _)) => format!(
                    "P{}",
                    d.patterns
                        .iter()
                        .position(|(id, _)| *id == self.pats[i].id)
                        .unwrap_or(0)
                        + 1
                ),
                None => format!("P{}", pat_slot[i] + 1),
            })
            .collect();
        let pname = |i: usize| pnames[i].clone();
        let mut plain = Sink::new(keep);
        let mut compact = Sink::new(keep);
        plain.put(&format!("# {NOTATION}\n"));
        let mut dict_out: Option<(Dict, String, String)> = None;
        let anything = !chosen.is_empty() || !self.pats.is_empty();
        if let Some((_, hash)) = kept {
            compact.put(&format!("# {NOTATION} dictionary {hash}\n"));
        } else if anything {
            let mut d = Dict::default();
            for &i in &chosen {
                let mut t = self.found.templates[i].clone();
                t.nested = t.nested.iter().map(|&(p, u)| (p, slot_of[&u])).collect();
                d.templates.push(t);
            }
            for &i in &pat_order {
                d.patterns
                    .push((self.pats[i].id.clone(), self.pats[i].text.clone()));
            }
            let text = d.text(g, &self.h.names);
            let hash = hash_hex(&text);
            compact.put(&format!("# {NOTATION} dictionary {hash}\n"));
            compact.put(&text);
            compact.put(BODY);
            dict_out = Some((d, text, hash));
        } else {
            compact.put(&format!("# {NOTATION}\n"));
        }
        let mut tally = vec![Tally::default(); self.found.templates.len()];
        let mut pat_tally = vec![(Cost::default(), Cost::default()); self.pats.len()];
        let mut chain_owner: HashMap<u64, usize> = HashMap::new();
        for (pi, p) in self.pats.iter().enumerate() {
            for c in &p.chains {
                chain_owner.entry(c.from).or_insert(pi);
                for &x in &c.interior {
                    chain_owner.entry(x).or_insert(pi);
                }
            }
        }
        for v in node_order(g, &self.h.names, &self.part)? {
            step(1)?;
            let mut pb = String::new();
            notation::plain_block(g, &self.h.names, v, &|_| false, &mut pb);
            plain.put(&pb);
            let p = self.pos(v);
            let r = self.root_of.get(p);
            if r > 0 {
                let t = r as usize - 1;
                let inst = tp::walk(g, &mut self.found.forest, &self.h, v)?;
                let use_line = tp::use_text(
                    g,
                    &self.h.names,
                    &self.found.templates[t],
                    &inst,
                    &tnames[t],
                );
                compact.put(&use_line);
                let tl = &mut tally[t];
                tl.uses += 1;
                tl.used.add(Cost::of(&use_line));
                tl.plain
                    .add(Cost::of(&tp::plain_text(g, &self.h.names, &inst)));
            } else if !self.covered.get(p) {
                let b = self.block(v, &pname);
                if let Some(&pi) = chain_owner.get(&v) {
                    pat_tally[pi].0.add(Cost::of(&pb));
                    pat_tally[pi].1.add(Cost::of(&b));
                }
                compact.put(&b);
            }
        }
        Ok(Written {
            plain,
            compact,
            dict: dict_out,
            tally,
            pat_tally,
        })
    }
}

// --------------------------------------------------------------- the call

pub fn plan(g: &mut Graph, a: &Args) -> Result<QueryResult> {
    let tokens = by_tokens(a)?;
    let lam = lambda(a)?;
    let store = store_name(a, "store")?;
    let text = a.str("text").map(|t| t.to_ascii_lowercase());
    if let Some(t) = &text {
        if !["compact", "plain", "dictionary"].contains(&t.as_str()) {
            return Err(Error::Msg(format!(
                "text: '{t}' is not a text this writes; use \"compact\", \"plain\" or \"dictionary\""
            )));
        }
    }
    let limit = limit_arg(a)?;
    let after = a.usize("after", 0);
    let part: Option<HashSet<u64>> = match a.get("nodes") {
        None | Some(Value::Null) => None,
        Some(_) => Some(a.ids("nodes")?.into_iter().collect()),
    };
    // With a kept dictionary and a selection, the plan reads the selection
    // and the templates it can use, never the whole graph; `find: "graph"`
    // finds the selection's instances over the whole graph instead (the
    // same text, for checking). Without a selection, or without a kept
    // dictionary, it is a whole-graph computation.
    let whole = a
        .str("find")
        .is_some_and(|f| f.eq_ignore_ascii_case("graph"));
    let want_text = text.as_deref() == Some("dictionary");
    let mut local: Option<Forest> = None;
    let kept = match store_name(a, "dictionary")? {
        Some(name) => {
            if store.is_some() {
                return Err(Error::Msg(
                    "store: and dictionary: together would keep a dictionary that was given; plan without dictionary: to keep a new one".into(),
                ));
            }
            let (meta, ok) = open_dict(g, &name, fresh_arg(a)?)?;
            let k = match &part {
                Some(p) if !whole => {
                    let h = meta.hier.rebind(g);
                    let mut sel: Vec<u64> = p
                        .iter()
                        .copied()
                        .filter(|&id| g.node_shared(id).is_some())
                        .collect();
                    sel.sort_unstable();
                    let mut f = Forest::part(g, &h, &sel)?;
                    let mut shapes = HashSet::new();
                    for v in 0..f.len {
                        let size = f.size.get(v) as usize;
                        if size >= h.min_nodes && size <= h.max_nodes {
                            let id = f.ids.get(v);
                            shapes.insert(f.shape_of(id).unwrap_or_default());
                        }
                    }
                    let k = load_with(g, &name, meta, ok, Some(&shapes), want_text)?;
                    local = Some(f);
                    k
                }
                _ => load_with(g, &name, meta, ok, None, want_text)?,
            };
            Some((name.clone(), k))
        }
        None => None,
    };
    let at = g.log_end();
    let (h, found, cached) = match &kept {
        Some((_, k)) => {
            let h = k.hier(g);
            let found = match local.take() {
                Some(f) => find_kept_in(g, &h, &k.dict, f)?,
                None => find_kept(g, &h, &k.dict)?,
            };
            (h, found, true)
        }
        None => {
            let h = Hier::from(g, a)?;
            let found = tp::find(g, &h)?;
            (h, found, false)
        }
    };
    let len = found.forest.len;
    let n = found.templates.len();
    let roots = found.roots.len();
    let mut pl = Planner {
        g,
        h,
        tokens,
        cached,
        found,
        part,
        covered: StateVec::new(len, false),
        root_of: StateVec::new(len, 0u64),
        used: StateVec::new(roots, false),
        in_dict: vec![false; n],
        nested_save: vec![(0, 0); n],
        pats: Vec::new(),
        kept_pats: kept
            .as_ref()
            .map(|(_, k)| k.dict.patterns.iter().map(|(id, _)| id.clone()).collect())
            .unwrap_or_default(),
        omit: HashSet::new(),
        extra: HashMap::new(),
    };
    pl.select()?;
    if !cached {
        pl.nesting()?;
        pl.more_uses()?;
    }
    pl.chains()?;
    let keep_text = text.is_some();
    let kept_ref = kept.as_ref().map(|(_, k)| (&k.dict, k.meta.hash.as_str()));
    let mut w = pl.write(keep_text, kept_ref)?;
    // Chosen only when the whole text is shorter, measured.
    let shorter = w.compact.cost.by(tokens) < w.plain.cost.by(tokens);
    if !shorter {
        pl.in_dict.iter_mut().for_each(|x| *x = false);
        for r in 0..roots {
            pl.used.set(r, false);
        }
        for p in 0..len {
            pl.covered.set(p, false);
            pl.root_of.set(p, 0);
        }
        pl.pats.clear();
        pl.omit.clear();
        pl.extra.clear();
        w = pl.write(keep_text, None)?;
    }
    let gr = pl.g;
    let names = pl.h.names.clone();

    // Rows: the total, then each candidate, chosen first.
    let mut rows: Vec<Vec<Value>> = Vec::new();
    let int = |x: i64| Value::Int(x);
    let dict_cost = match (&kept, &w.dict) {
        (Some((_, k)), _) => Cost {
            bytes: k.meta.bytes,
            tokens: k.meta.tokens,
        },
        (None, Some((_, t, _))) => Cost::of(t),
        _ => Cost::default(),
    };
    let hash = match (&kept, &w.dict) {
        (Some((_, k)), _) => Value::Text(k.meta.hash.clone()),
        (None, Some((_, _, h))) => Value::Text(h.clone()),
        _ => Value::Null,
    };
    let uses: u64 = w.tally.iter().map(|t| t.uses).sum::<u64>()
        + pl.pats.iter().map(|p| p.chains.len() as u64).sum::<u64>();
    let text_value = match text.as_deref() {
        Some("plain") => Value::Text(w.plain.keep.take().unwrap_or_default()),
        Some("compact") => Value::Text(w.compact.keep.take().unwrap_or_default()),
        Some("dictionary") => Value::Text(match (&kept, &w.dict) {
            (Some((_, k)), _) => k.text.clone(),
            (None, Some((_, t, _))) => t.clone(),
            _ => String::new(),
        }),
        _ => Value::Null,
    };
    let (pc, cc) = (w.plain.cost, w.compact.cost);
    rows.push(vec![
        Value::Text("total".into()),
        hash.clone(),
        Value::Text(NOTATION.into()),
        Value::Bool(shorter),
        int(uses as i64),
        Value::Null,
        int(dict_cost.bytes as i64),
        int(dict_cost.tokens as i64),
        int(cc.bytes as i64),
        int(cc.tokens as i64),
        int(pc.bytes as i64),
        int(pc.tokens as i64),
        int(pc.bytes as i64 - cc.bytes as i64),
        int(pc.tokens as i64 - cc.tokens as i64),
        text_value,
    ]);
    let dict_names: Vec<usize> = match &w.dict {
        Some((d, _, _)) => d
            .templates
            .iter()
            .map(|t| {
                pl.found
                    .templates
                    .iter()
                    .position(|x| x.id == t.id)
                    .unwrap()
            })
            .collect(),
        None => match &kept {
            Some((_, k)) => k
                .dict
                .templates
                .iter()
                .filter_map(|t| pl.found.templates.iter().position(|x| x.id == t.id))
                .filter(|&i| pl.in_dict[i])
                .collect(),
            None => Vec::new(),
        },
    };
    let mut nested_in = vec![0i64; n];
    for &i in &dict_names {
        for &(_, u) in &pl.found.templates[i].nested {
            nested_in[u] += 1;
        }
    }
    let mut cand: Vec<(bool, i64, String, Vec<Value>)> = Vec::new();
    for i in 0..n {
        let t = &pl.found.templates[i];
        let chosen = pl.in_dict[i];
        let def = if cached {
            Cost::default()
        } else {
            Cost::of(&tp::definition(gr, &names, t, &pl.found.templates, "T", &|_| "T".into()).0)
        };
        let (count, used, plain) = if chosen {
            let tl = w.tally[i];
            (tl.uses, tl.used, tl.plain)
        } else {
            let mut u = Cost::default();
            let mut p = Cost::default();
            for r in t.start..t.start + t.count {
                let x = pl.found.use_cost.get(r);
                let y = pl.found.plain_cost.get(r);
                u.add(Cost {
                    bytes: x.0,
                    tokens: x.1,
                });
                p.add(Cost {
                    bytes: y.0,
                    tokens: y.1,
                });
            }
            (t.count as u64, u, p)
        };
        let ns = pl.nested_save[i];
        let sb = plain.bytes as i64 - used.bytes as i64 - def.bytes as i64 + ns.0;
        let st = plain.tokens as i64 - used.tokens as i64 - def.tokens as i64 + ns.1;
        let name = if chosen {
            match dict_names.iter().position(|&x| x == i) {
                Some(k) => match &kept {
                    Some((_, kd)) => Value::Text(format!(
                        "T{}",
                        kd.dict
                            .templates
                            .iter()
                            .position(|x| x.id == t.id)
                            .unwrap_or(0)
                            + 1
                    )),
                    None => Value::Text(format!("T{}", k + 1)),
                },
                None => Value::Null,
            }
        } else {
            Value::Null
        };
        cand.push((
            chosen,
            if tokens { st } else { sb },
            t.id.clone(),
            vec![
                Value::Text("template".into()),
                Value::Text(t.id.clone()),
                name,
                Value::Bool(chosen),
                int(count as i64),
                int(nested_in[i]),
                int(def.bytes as i64),
                int(def.tokens as i64),
                int(used.bytes as i64),
                int(used.tokens as i64),
                int(plain.bytes as i64),
                int(plain.tokens as i64),
                int(sb),
                int(st),
                Value::Null,
            ],
        ));
    }
    for (pi, p) in pl.pats.iter().enumerate() {
        let def = if cached {
            Cost::default()
        } else {
            Cost::of(&format!("P{} #{} {}\n", pi + 1, p.id, p.text))
        };
        let (before, after_) = w.pat_tally[pi];
        let sb = before.bytes as i64 - after_.bytes as i64 - def.bytes as i64;
        let st = before.tokens as i64 - after_.tokens as i64 - def.tokens as i64;
        cand.push((
            true,
            if tokens { st } else { sb },
            p.id.clone(),
            vec![
                Value::Text("pattern".into()),
                Value::Text(p.id.clone()),
                Value::Text(p.text.clone()),
                Value::Bool(true),
                int(p.chains.len() as i64),
                Value::Null,
                int(def.bytes as i64),
                int(def.tokens as i64),
                int(after_.bytes as i64),
                int(after_.tokens as i64),
                int(before.bytes as i64),
                int(before.tokens as i64),
                int(sb),
                int(st),
                Value::Null,
            ],
        ));
    }
    cand.sort_by(|a, b| (b.0, b.1).cmp(&(a.0, a.1)).then_with(|| a.2.cmp(&b.2)));
    let total_cands = cand.len();
    let take = if after == 0 {
        limit.saturating_sub(1)
    } else {
        limit
    };
    let skip = after.saturating_sub(1);
    let mut shown = 0;
    if after > 0 {
        rows.clear();
    }
    for (_, _, _, row) in cand.into_iter().skip(skip).take(take) {
        rows.push(row);
        shown += 1;
    }
    let mut touched = 0usize;
    if let Some(name) = &store {
        let d = w
            .dict
            .as_ref()
            .map(|(d, t, h)| (d.clone(), t.clone(), h.clone()));
        let (d, t, hsh) = d.unwrap_or_else(|| {
            let d = Dict::default();
            let t = d.text(gr, &names);
            let h = hash_hex(&t);
            (d, t, h)
        });
        let meta = DictMeta {
            at,
            code: CODE.into(),
            args: format!(
                "{{\"hierarchy\":{},\"by\":\"{}\",\"lambda\":{lam},\"nodes\":{}}}",
                pl.h.json(),
                if tokens { "tokens" } else { "bytes" },
                pl.part.as_ref().map_or(0, |p| p.len())
            ),
            hash: hsh,
            templates: d.templates.len() as u64,
            patterns: d.patterns.len() as u64,
            bytes: t.len() as u64,
            tokens: Cost::of(&t).tokens,
            hier: pl.h.clone(),
        };
        drop(pl);
        touched = keep(g, name, &meta, &d, &t)?;
    }
    let columns: Vec<String> = [
        "kind",
        "id",
        "name",
        "chosen",
        "uses",
        "nested_in",
        "define_bytes",
        "define_tokens",
        "use_bytes",
        "use_tokens",
        "plain_bytes",
        "plain_tokens",
        "saving_bytes",
        "saving_tokens",
        "text",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let mut msg = format!(
        "{} as {NOTATION}: plain {} bytes, ~{} tokens; with {} {} bytes, ~{} tokens; saving {} bytes, ~{} tokens, measured by writing both{}; lambda: {lam} (no candidate here leaves anything out)",
        if a.get("nodes").is_some() { "the nodes given" } else { "the whole graph" },
        pc.bytes,
        pc.tokens,
        match &kept {
            Some((name, _)) => format!("the kept dictionary '{name}' (not repeated)"),
            None if shorter => "its dictionary".to_string(),
            None => "nothing chosen (nothing pays for itself)".to_string(),
        },
        cc.bytes,
        cc.tokens,
        pc.bytes as i64 - cc.bytes as i64,
        pc.tokens as i64 - cc.tokens as i64,
        if shorter { "" } else { "; nothing is chosen, so the compact text is the plain text" }
    );
    if let Some((name, k)) = &kept {
        msg.push_str(&format!(
            "; dictionary '{name}' computed at revision {}, {}",
            k.meta.at,
            if k.fresh {
                "fresh"
            } else {
                "stale: the database has changed since (every use was checked against the graph)"
            }
        ));
    }
    if let Some(name) = &store {
        msg.push_str(&format!("; kept as '{name}'"));
    }
    let next = if after == 0 { shown + 1 } else { after + shown };
    if next <= total_cands {
        msg.push_str(&format!(
            "; more candidates: CALL structure.plan again with after: {next} and the same arguments"
        ));
    }
    let mut r = QueryResult::table(columns, rows);
    r.message = Some(msg);
    r.touched = touched;
    Ok(r)
}

// --------------------------------------------------- kept dictionaries

/// What is kept about a dictionary beside its text and definitions.
#[derive(Clone, Debug)]
pub struct DictMeta {
    pub at: u64,
    pub code: String,
    pub args: String,
    pub hash: String,
    pub templates: u64,
    pub patterns: u64,
    pub bytes: u64,
    pub tokens: u64,
    pub hier: Hier,
}

pub struct Kept {
    pub meta: DictMeta,
    pub dict: Dict,
    pub text: String,
    pub fresh: bool,
}

impl Kept {
    fn hier(&self, g: &Graph) -> Hier {
        self.meta.hier.rebind(g)
    }
}

fn key(prefix: u8, name: &str) -> Vec<u8> {
    let mut k = vec![prefix];
    k.extend_from_slice(name.as_bytes());
    k
}

fn item_key(prefix: u8, name: &str, i: u32) -> Vec<u8> {
    let mut k = key(prefix, name);
    k.push(0);
    k.extend_from_slice(&i.to_be_bytes());
    k
}

fn put_list(out: &mut Vec<u8>, v: &[String]) {
    codec::put_u32(out, v.len() as u32);
    for s in v {
        codec::put_str(out, s);
    }
}

fn get_list(r: &mut Reader<'_>) -> std::result::Result<Vec<String>, String> {
    let n = r.u32()? as usize;
    let mut v = Vec::with_capacity(n.min(1 << 16));
    for _ in 0..n {
        v.push(r.string()?);
    }
    Ok(v)
}

impl DictMeta {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = vec![1u8];
        codec::put_varint(&mut b, self.at);
        codec::put_str(&mut b, &self.code);
        codec::put_str(&mut b, &self.args);
        codec::put_str(&mut b, &self.hash);
        for x in [self.templates, self.patterns, self.bytes, self.tokens] {
            codec::put_varint(&mut b, x);
        }
        let h = &self.hier;
        put_list(&mut b, &h.types);
        b.push(match h.dir {
            Dir::In => 1,
            _ => 0,
        });
        put_list(&mut b, &h.labels);
        for x in [h.min_nodes, h.min_count, h.max_nodes] {
            codec::put_varint(&mut b, x as u64);
        }
        put_list(&mut b, &h.names.identity);
        b
    }

    pub fn decode(b: &[u8]) -> Option<DictMeta> {
        let mut r = Reader::new(b);
        let go = |r: &mut Reader<'_>| -> std::result::Result<DictMeta, String> {
            if r.u8()? != 1 {
                return Err("version".into());
            }
            let at = r.varint()?;
            let code = r.string()?;
            let args = r.string()?;
            let hash = r.string()?;
            let templates = r.varint()?;
            let patterns = r.varint()?;
            let bytes = r.varint()?;
            let tokens = r.varint()?;
            let types = get_list(r)?;
            let dir = if r.u8()? == 1 { Dir::In } else { Dir::Out };
            let labels = get_list(r)?;
            let min_nodes = r.varint()? as usize;
            let min_count = r.varint()? as usize;
            let max_nodes = r.varint()? as usize;
            let identity = get_list(r)?;
            Ok(DictMeta {
                at,
                code,
                args,
                hash,
                templates,
                patterns,
                bytes,
                tokens,
                hier: Hier::parts(
                    types, dir, labels, min_nodes, min_count, max_nodes, identity,
                ),
            })
        };
        let m = go(&mut r).ok()?;
        (r.remaining() == 0).then_some(m)
    }
}

pub fn encode_template(t: &Template) -> Vec<u8> {
    let mut b = vec![1u8];
    codec::put_str(&mut b, &t.id);
    b.extend_from_slice(&t.group.to_be_bytes());
    b.extend_from_slice(&t.shape.to_be_bytes());
    codec::put_bytes(&mut b, &t.structure);
    codec::put_u32(&mut b, t.nodes() as u32);
    for p in 0..t.nodes() {
        codec::put_u32(&mut b, t.parent[p]);
        put_list(&mut b, &t.labels[p]);
        codec::put_u32(&mut b, t.props[p].len() as u32);
        for (k, pr) in &t.props[p] {
            codec::put_str(&mut b, k);
            put_prop(&mut b, pr);
        }
    }
    codec::put_u32(&mut b, t.edges.len() as u32);
    for e in &t.edges {
        codec::put_u32(&mut b, e.src);
        codec::put_str(&mut b, &e.etype);
        match e.to {
            To::Pos(p) => {
                b.push(0);
                codec::put_varint(&mut b, p as u64);
            }
            To::Node(n) => {
                b.push(1);
                codec::put_varint(&mut b, n);
            }
        }
        b.push(e.tree as u8);
        codec::put_u32(&mut b, e.props.len() as u32);
        for (k, pr) in &e.props {
            codec::put_str(&mut b, k);
            put_prop(&mut b, pr);
        }
    }
    codec::put_u32(&mut b, t.nested.len() as u32);
    for &(p, u) in &t.nested {
        codec::put_u32(&mut b, p);
        codec::put_u32(&mut b, u as u32);
    }
    codec::put_u32(&mut b, t.slots.len() as u32);
    for s in &t.slots {
        codec::put_u32(&mut b, s.pos);
        codec::put_u32(&mut b, s.edge);
        codec::put_u32(&mut b, s.key);
    }
    b
}

fn put_prop(b: &mut Vec<u8>, p: &Prop) {
    match p {
        Prop::Const(v) => {
            b.push(0);
            codec::put_value(b, v);
        }
        Prop::Slot => b.push(1),
        Prop::Derived {
            base,
            k,
            full,
            suffix,
        } => {
            b.push(2);
            codec::put_u32(b, *base);
            codec::put_u32(b, *k);
            b.push(*full as u8);
            codec::put_str(b, suffix);
        }
    }
}

fn get_prop(r: &mut Reader<'_>) -> std::result::Result<Prop, String> {
    Ok(match r.u8()? {
        0 => Prop::Const(r.value()?),
        1 => Prop::Slot,
        2 => Prop::Derived {
            base: r.u32()?,
            k: r.u32()?,
            full: r.u8()? == 1,
            suffix: r.string()?,
        },
        x => return Err(format!("property kind {x}")),
    })
}

pub fn decode_template(b: &[u8]) -> Option<Template> {
    let mut r = Reader::new(b);
    let go = |r: &mut Reader<'_>| -> std::result::Result<Template, String> {
        if r.u8()? != 1 {
            return Err("version".into());
        }
        let id = r.string()?;
        if r.remaining() < 16 {
            return Err("short".into());
        }
        if r.remaining() < 32 {
            return Err("short".into());
        }
        let group = u128::from_be_bytes(r.buf[r.pos..r.pos + 16].try_into().unwrap());
        let shape = u128::from_be_bytes(r.buf[r.pos + 16..r.pos + 32].try_into().unwrap());
        r.pos += 32;
        let structure = r.bytes()?;
        let n = r.u32()? as usize;
        let (mut parent, mut labels, mut props) = (Vec::new(), Vec::new(), Vec::new());
        for _ in 0..n {
            parent.push(r.u32()?);
            labels.push(get_list(r)?);
            let k = r.u32()? as usize;
            let mut ps = Vec::new();
            for _ in 0..k {
                let key = r.string()?;
                ps.push((key, get_prop(r)?));
            }
            props.push(ps);
        }
        let m = r.u32()? as usize;
        let mut edges = Vec::new();
        for _ in 0..m {
            let src = r.u32()?;
            let etype = r.string()?;
            let to = match r.u8()? {
                0 => To::Pos(r.varint()? as u32),
                _ => To::Node(r.varint()?),
            };
            let tree = r.u8()? == 1;
            let k = r.u32()? as usize;
            let mut ps = Vec::new();
            for _ in 0..k {
                let key = r.string()?;
                ps.push((key, get_prop(r)?));
            }
            edges.push(TEdge {
                src,
                etype,
                to,
                props: ps,
                tree,
            });
        }
        let k = r.u32()? as usize;
        let mut nested = Vec::new();
        for _ in 0..k {
            nested.push((r.u32()?, r.u32()? as usize));
        }
        let k = r.u32()? as usize;
        let mut slots = Vec::new();
        for _ in 0..k {
            slots.push(SlotRef {
                pos: r.u32()?,
                edge: r.u32()?,
                key: r.u32()?,
            });
        }
        Ok(Template {
            id,
            group,
            shape,
            structure,
            parent,
            labels,
            props,
            edges,
            nested,
            slots,
            start: 0,
            count: 0,
        })
    };
    let t = go(&mut r).ok()?;
    (r.remaining() == 0).then_some(t)
}

/// Delete every record of the dictionary `name`, a batch at a time.
fn forget(g: &mut Graph, name: &str) -> Result<u64> {
    let mut gone = 0;
    for p in *b"tpx" {
        let mut prefix = key(p, name);
        prefix.push(0);
        loop {
            let mut keys: Vec<Vec<u8>> = Vec::new();
            g.ext_scan(NS, &prefix, &mut |k, _| {
                keys.push(k.to_vec());
                keys.len() < 4096
            });
            if keys.is_empty() {
                break;
            }
            step(keys.len() as u64)?;
            gone += keys.len() as u64;
            g.ext_write(NS, Vec::new(), keys)?;
        }
    }
    Ok(gone)
}

fn keep(g: &mut Graph, name: &str, meta: &DictMeta, d: &Dict, text: &str) -> Result<usize> {
    let (_, from) = begin(g);
    forget(g, name)?;
    let mut puts: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut n = 0;
    for (i, t) in d.templates.iter().enumerate() {
        puts.push((item_key(b't', name, i as u32), encode_template(t)));
        if puts.len() >= 256 {
            n += puts.len();
            g.ext_write(NS, std::mem::take(&mut puts), Vec::new())?;
        }
    }
    for (i, (id, t)) in d.patterns.iter().enumerate() {
        let mut v = Vec::new();
        codec::put_str(&mut v, id);
        codec::put_str(&mut v, t);
        puts.push((item_key(b'p', name, i as u32), v));
    }
    let bytes = text.as_bytes();
    for (i, c) in bytes.chunks(CHUNK).enumerate() {
        puts.push((item_key(b'x', name, i as u32), c.to_vec()));
        if puts.len() >= 64 {
            n += puts.len();
            g.ext_write(NS, std::mem::take(&mut puts), Vec::new())?;
        }
    }
    n += puts.len();
    g.ext_write(NS, puts, Vec::new())?;
    // The index a selection is planned by: each template's root shape and
    // id, so only the templates a selection can use are read.
    let mut index = Vec::new();
    codec::put_varint(&mut index, d.templates.len() as u64);
    for t in &d.templates {
        index.extend_from_slice(&t.shape.to_be_bytes());
        codec::put_str(&mut index, &t.id);
    }
    seal(
        g,
        from,
        vec![(key(b's', name), index), (key(b'd', name), meta.encode())],
        Vec::new(),
    )?;
    Ok(n + 2)
}

pub fn find_meta(g: &Graph, name: &str) -> Result<DictMeta> {
    let v = g.ext_get(NS, &key(b'd', name)).ok_or_else(|| {
        Error::Msg(format!(
            "no dictionary is kept as '{name}'; CALL structure.dictionaries() lists them, and CALL structure.plan(store: '{name}', ...) keeps one"
        ))
    })?;
    DictMeta::decode(&v).ok_or_else(|| {
        Error::Msg(format!(
            "the dictionary kept as '{name}' does not decode; drop it and plan again"
        ))
    })
}

pub fn load(g: &Graph, name: &str, fresh: Fresh) -> Result<Kept> {
    let (meta, ok) = open_dict(g, name, fresh)?;
    load_with(g, name, meta, ok, None, true)
}

/// A kept dictionary's metadata, refusing it when stale and `fresh:
/// "require"` was asked for.
pub fn open_dict(g: &Graph, name: &str, fresh: Fresh) -> Result<(DictMeta, bool)> {
    let meta = find_meta(g, name)?;
    let ok = is_fresh(g, meta.at);
    if !ok && fresh == Fresh::Require {
        return Err(Error::Msg(format!(
            "the dictionary kept as '{name}' was made at revision {} and the database has changed since; plan again with store: '{name}', or use it with fresh: \"report\"",
            meta.at
        )));
    }
    Ok((meta, ok))
}

fn decode_index(v: &[u8]) -> Option<Vec<(u128, String)>> {
    let mut r = Reader::new(v);
    let n = r.varint().ok()? as usize;
    let mut out = Vec::with_capacity(n.min(1 << 16));
    for _ in 0..n {
        if r.remaining() < 16 {
            return None;
        }
        let shape = u128::from_be_bytes(r.buf[r.pos..r.pos + 16].try_into().ok()?);
        r.pos += 16;
        out.push((shape, r.string().ok()?));
    }
    (r.remaining() == 0).then_some(out)
}

/// A kept dictionary read for use. With `only`, just the templates whose
/// root shape is in it are read and decoded (the others stand in by id,
/// with no instances), so planning a selection reads what the selection
/// can use, not the whole dictionary; the text is read (and checked
/// against its hash) only when `text` asks for it.
pub fn load_with(
    g: &Graph,
    name: &str,
    meta: DictMeta,
    fresh: bool,
    only: Option<&HashSet<u128>>,
    text: bool,
) -> Result<Kept> {
    let bad = || {
        Error::Msg(format!(
            "the dictionary kept as '{name}' does not decode; drop it and plan again"
        ))
    };
    let mut dict = Dict::default();
    let index = g.ext_get(NS, &key(b's', name)).map(|v| decode_index(&v));
    match (only, index) {
        (Some(only), Some(Some(index))) => {
            for (i, (shape, id)) in index.into_iter().enumerate() {
                step(1)?;
                if only.contains(&shape) {
                    let v = g
                        .ext_get(NS, &item_key(b't', name, i as u32))
                        .ok_or_else(bad)?;
                    dict.templates.push(decode_template(&v).ok_or_else(bad)?);
                } else {
                    dict.templates.push(Template {
                        id,
                        group: 0,
                        shape,
                        structure: Vec::new(),
                        parent: Vec::new(),
                        labels: Vec::new(),
                        props: Vec::new(),
                        edges: Vec::new(),
                        nested: Vec::new(),
                        slots: Vec::new(),
                        start: 0,
                        count: 0,
                    });
                }
            }
        }
        _ => {
            let mut prefix = key(b't', name);
            prefix.push(0);
            let mut raw: Vec<Vec<u8>> = Vec::new();
            g.ext_scan(NS, &prefix, &mut |_, v| {
                raw.push(v.to_vec());
                true
            });
            for v in raw {
                dict.templates.push(decode_template(&v).ok_or_else(bad)?);
            }
        }
    }
    let mut prefix = key(b'p', name);
    prefix.push(0);
    let mut raw: Vec<Vec<u8>> = Vec::new();
    g.ext_scan(NS, &prefix, &mut |_, v| {
        raw.push(v.to_vec());
        true
    });
    for v in raw {
        let mut r = Reader::new(&v);
        let id = r.string().map_err(|_| bad())?;
        let t = r.string().map_err(|_| bad())?;
        dict.patterns.push((id, t));
    }
    let mut body = String::new();
    if text {
        let mut prefix = key(b'x', name);
        prefix.push(0);
        let mut raw = Vec::new();
        g.ext_scan(NS, &prefix, &mut |_, v| {
            raw.extend_from_slice(v);
            true
        });
        body = String::from_utf8(raw).map_err(|_| bad())?;
        if hash_hex(&body) != meta.hash {
            return Err(bad());
        }
    }
    Ok(Kept {
        meta,
        dict,
        text: body,
        fresh,
    })
}

/// Bytes a kept dictionary takes in the keyspace, every record counted:
/// definitions, patterns, text, the shape index and the metadata.
fn stored_bytes(g: &Graph, name: &str) -> u64 {
    let mut n = 0u64;
    for p in *b"tpx" {
        let mut prefix = key(p, name);
        prefix.push(0);
        g.ext_scan(NS, &prefix, &mut |k, v| {
            n += (k.len() + v.len()) as u64;
            true
        });
    }
    for p in *b"sd" {
        let k = key(p, name);
        if let Some(v) = g.ext_get(NS, &k) {
            n += (k.len() + v.len()) as u64;
        }
    }
    n
}

/// A kept dictionary's templates as found in the graph now: each
/// candidate root whose shape is a kept template's and that matches it in
/// full.
pub fn find_kept(g: &Graph, h: &Hier, d: &Dict) -> Result<Found> {
    find_kept_in(g, h, d, Forest::build(g, h)?)
}

/// [`find_kept`] over a forest: the whole graph's, or a selection's
/// ([`Forest::part`]), whose instances are then those wholly inside it.
pub fn find_kept_in(g: &Graph, h: &Hier, d: &Dict, mut forest: Forest) -> Result<Found> {
    let by_group: HashMap<u128, usize> = d
        .templates
        .iter()
        .enumerate()
        .filter(|(_, t)| !t.structure.is_empty())
        .map(|(i, t)| (t.group, i))
        .collect();
    let shapes: HashSet<u128> = d
        .templates
        .iter()
        .filter(|t| !t.structure.is_empty())
        .map(|t| t.shape)
        .collect();
    let label_ids: Vec<u32> = h
        .labels
        .iter()
        .filter_map(|l| g.strings.lookup(l))
        .collect();
    let mut per: Vec<Vec<u64>> = vec![Vec::new(); d.templates.len()];
    let mut candidates = 0u64;
    for v in 0..forest.len {
        step(1)?;
        let size = forest.size.get(v) as usize;
        let id = forest.ids.get(v);
        if size < h.min_nodes
            || size > h.max_nodes
            || !(label_ids.is_empty() || label_ids.iter().any(|&l| g.has_label(id, l)))
        {
            continue;
        }
        candidates += 1;
        if !shapes.contains(&forest.shape_of(id).unwrap_or_default()) {
            continue;
        }
        let inst = tp::walk(g, &mut forest, h, id)?;
        let st = tp::structure(&inst);
        if let Some(&i) = by_group.get(&sip128(&st)) {
            if tp::matches(&d.templates[i], &inst, &st).is_ok() {
                per[i].push(id);
            }
        }
    }
    let total: usize = per.iter().map(|v| v.len()).sum();
    let mut roots = StateVec::new(total, 0u64);
    let mut use_cost = StateVec::new(total, (0u64, 0u64));
    let mut plain_cost = StateVec::new(total, (0u64, 0u64));
    let mut templates = d.templates.clone();
    let mut at = 0usize;
    for (i, list) in per.into_iter().enumerate() {
        templates[i].start = at;
        templates[i].count = list.len();
        for id in list {
            let inst = tp::walk(g, &mut forest, h, id)?;
            let u = Cost::of(&tp::use_text(g, &h.names, &templates[i], &inst, "T"));
            let p = Cost::of(&tp::plain_text(g, &h.names, &inst));
            roots.set(at, id);
            use_cost.set(at, (u.bytes, u.tokens));
            plain_cost.set(at, (p.bytes, p.tokens));
            at += 1;
        }
    }
    let def_cost = vec![Cost::default(); templates.len()];
    Ok(Found {
        forest,
        templates,
        roots,
        use_cost,
        plain_cost,
        def_cost,
        candidates,
    })
}

pub fn dictionaries(g: &mut Graph) -> Result<QueryResult> {
    let mut metas: Vec<(String, Vec<u8>)> = Vec::new();
    g.ext_scan(NS, b"d", &mut |k, v| {
        metas.push((String::from_utf8_lossy(&k[1..]).into_owned(), v.to_vec()));
        true
    });
    let mut rows = Vec::new();
    for (name, v) in metas {
        let Some(m) = DictMeta::decode(&v) else {
            rows.push(vec![
                Value::Text(name),
                Value::Null,
                Value::Bool(false),
                Value::Text("unreadable".into()),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ]);
            continue;
        };
        let stored = stored_bytes(g, &name);
        rows.push(vec![
            Value::Text(name),
            Value::Int(m.at as i64),
            Value::Bool(is_fresh(g, m.at)),
            Value::Text(m.hash),
            Value::Int(m.templates as i64),
            Value::Int(m.patterns as i64),
            Value::Int(m.bytes as i64),
            Value::Int(m.tokens as i64),
            Value::Text(m.code),
            Value::Text(m.args),
            Value::Int(stored as i64),
        ]);
    }
    let n = rows.len();
    let mut r = QueryResult::table(
        [
            "name",
            "revision",
            "fresh",
            "hash",
            "templates",
            "patterns",
            "bytes",
            "tokens",
            "code",
            "args",
            "stored_bytes",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
        rows,
    );
    r.message = Some(format!(
        "{n} kept dictionaries; bytes and tokens are the dictionary's text, stored_bytes every record kept for it"
    ));
    Ok(r)
}

pub fn drop_dictionary(g: &mut Graph, name: &str) -> Result<QueryResult> {
    find_meta(g, name)?;
    let (_, from) = begin(g);
    let gone = forget(g, name)?;
    seal(g, from, Vec::new(), vec![key(b's', name), key(b'd', name)])?;
    let mut r = QueryResult::message(format!("dropped the dictionary '{name}' ({gone} records)"));
    r.touched = 1;
    Ok(r)
}

/// Whether a record of a kept dictionary decodes, for `check.run`.
pub fn sound(k: &[u8], v: &[u8]) -> bool {
    match k.first() {
        Some(b'd') => DictMeta::decode(v).is_some(),
        Some(b't') => k.len() > 6 && decode_template(v).is_some(),
        Some(b'p') => {
            let mut r = Reader::new(v);
            k.len() > 6 && r.string().is_ok() && r.string().is_ok() && r.remaining() == 0
        }
        Some(b'x') => k.len() > 6,
        Some(b's') => decode_index(v).is_some(),
        _ => false,
    }
}

/// `expand(of:, root:, dictionary:)` with a kept dictionary.
pub fn expand_kept(
    g: &Graph,
    name: &str,
    of: &str,
    roots: &[u64],
    fresh: Fresh,
) -> Result<QueryResult> {
    let k = load(g, name, fresh)?;
    let h = k.hier(g);
    let of = of.trim().trim_start_matches('#');
    let i = k
        .dict
        .templates
        .iter()
        .position(|t| t.id == of || (of.len() >= 16 && t.id.starts_with(of)))
        .or_else(|| {
            of.strip_prefix('T')
                .and_then(|n| n.parse::<usize>().ok())
                .filter(|&n| n >= 1 && n <= k.dict.templates.len())
                .map(|n| n - 1)
        })
        .ok_or_else(|| {
            Error::Msg(format!(
                "the dictionary '{name}' has no template '{of}'; give its id, the 16 digits its definition shows, or its name (T1, T2, ...)"
            ))
        })?;
    tp::expand_template(g, &h, &k.dict.templates[i], roots)
}
