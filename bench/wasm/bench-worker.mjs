// glider vs SQLite, both compiled to WebAssembly, in this browser tab.
//
// Runs in a Web Worker so the page stays responsive. Both engines get the
// same generated graph: people in communities with power-law hubs (KNOWS),
// products in a category tree, orders placing and containing products.
// glider stores it as a property graph; SQLite as a table per label and per
// relationship type with foreign keys and both-way relationship indexes, as
// in the native benchmarks. Every benchmark checks both engines' answers.

import { loadGlider } from './glider/index.js'
import sqlite3InitModule from './sqlite/index.mjs'

let glider, sqlite3, gdb, sdb, meta, writeRun = 0

const post = (m) => self.postMessage(m)

// ------------------------------------------------------------------ graph

function rng(seed) {
  let s = BigInt(seed) || 1n
  return () => {
    s ^= s << 13n; s &= (1n << 64n) - 1n
    s ^= s >> 7n
    s ^= s << 17n; s &= (1n << 64n) - 1n
    return Number(s % 1000000007n) / 1000000007
  }
}

const COUNTRIES = ['US', 'GB', 'DE', 'FR', 'JP', 'IN', 'BR', 'NZ', 'CA', 'ES']
const STATUSES = ['placed', 'paid', 'packed', 'shipped', 'delivered', 'cancelled']

function generate(people) {
  const r = rng(42)
  const products = Math.max(20, Math.floor(people / 10))
  const orders = Math.floor(people / 2)
  const categories = 50
  const nodes = [], edges = []
  let id = 0
  const person = (i) => 1 + i
  for (let i = 0; i < people; i++) {
    nodes.push({ id: ++id, label: 'Person', props: {
      email: `user${i}@example.org`, name: `Person ${i}`, age: 18 + Math.floor(r() * 70),
      country: COUNTRIES[Math.floor(r() * r() * 10)], active: r() < 0.7 } })
  }
  const catBase = id
  for (let i = 0; i < categories; i++) nodes.push({ id: ++id, label: 'Category', props: { name: `Category ${i}`, depth: i === 0 ? 0 : 1 + Math.floor(Math.log2(i + 1)) } })
  const prodBase = id
  for (let i = 0; i < products; i++) nodes.push({ id: ++id, label: 'Product', props: { sku: `SKU-${i}`, price: Math.round(r() * 50000) / 100 } })
  const orderBase = id
  for (let i = 0; i < orders; i++) nodes.push({ id: ++id, label: 'Order', props: { ref: `ORD-${i}`, total: Math.round(r() * 200000) / 100, status: STATUSES[Math.floor(r() * 6)] } })

  let eid = 0
  const edge = (type, from, to, props = {}) => edges.push({ id: ++eid, type, from, to, props })
  for (let i = 0; i < people; i++) {
    const community = Math.floor(i / 100) * 100
    for (let k = 0; k < 6; k++) {
      const j = community + Math.floor(r() * 100)
      if (j < people && j !== i) edge('KNOWS', person(i), person(j), { since: 2000 + Math.floor(r() * 26), weight: Math.round(r() * 1000) / 1000 })
    }
    const hub = Math.floor(people * r() * r() * r())       // power law: low ids are hubs
    if (hub !== i) edge('KNOWS', person(i), person(hub), { since: 2020, weight: 0.5 })
    const any = Math.floor(r() * people)
    if (any !== i) edge('KNOWS', person(i), person(any), { since: 2024, weight: 0.1 })
  }
  for (let i = 1; i < categories; i++) edge('PARENT_OF', catBase + 1 + Math.floor((i - 1) / 3), catBase + 1 + i)
  for (let i = 0; i < products; i++) edge('IN_CATEGORY', prodBase + 1 + i, catBase + 1 + Math.floor(categories / 2 + r() * categories / 2))
  for (let i = 0; i < orders; i++) {
    const buyer = Math.floor(people * r() * r())
    edge('PLACED', person(buyer), orderBase + 1 + i)
    const lines = 1 + Math.floor(r() * 4)
    for (let l = 0; l < lines; l++) edge('CONTAINS', orderBase + 1 + i, prodBase + 1 + Math.floor(products * r() * r()), { qty: 1 + Math.floor(r() * 3) })
  }
  return { nodes, edges, people, products, orders, categories }
}

// ------------------------------------------------------------------ loading

const EDGE_TABLES = {
  KNOWS: ['knows', 'person', 'person', ['since', 'weight']],
  PARENT_OF: ['parent_of', 'category', 'category', []],
  IN_CATEGORY: ['in_category', 'product', 'category', []],
  PLACED: ['placed', 'person', 'orders', []],
  CONTAINS: ['contains', 'orders', 'product', ['qty']],
}
const NODE_TABLES = {
  Person: ['person', ['email', 'name', 'age', 'country', 'active']],
  Category: ['category', ['name', 'depth']],
  Product: ['product', ['sku', 'price']],
  Order: ['orders', ['ref', 'total', 'status']],
}

function loadSqlite(g) {
  sdb = new sqlite3.oo1.DB(':memory:')
  sdb.exec('PRAGMA foreign_keys = ON')
  const ddl = []
  for (const [t, cols] of Object.values(NODE_TABLES)) ddl.push(`CREATE TABLE ${t} (id INTEGER PRIMARY KEY, ${cols.join(', ')})`)
  for (const [t, s, d, cols] of Object.values(EDGE_TABLES)) {
    ddl.push(`CREATE TABLE ${t} (id INTEGER PRIMARY KEY, src INTEGER NOT NULL REFERENCES ${s}(id), dst INTEGER NOT NULL REFERENCES ${d}(id)${cols.map(c => ', ' + c).join('')})`)
  }
  sdb.exec(ddl.join(';'))
  const t0 = performance.now()
  sdb.exec('BEGIN')
  const ins = {}
  for (const [label, [t, cols]] of Object.entries(NODE_TABLES)) ins[label] = sdb.prepare(`INSERT INTO ${t} (id, ${cols.join(', ')}) VALUES (?${', ?'.repeat(cols.length)})`)
  for (const n of g.nodes) {
    const cols = NODE_TABLES[n.label][1]
    ins[n.label].bind([n.id, ...cols.map(c => typeof n.props[c] === 'boolean' ? Number(n.props[c]) : n.props[c])]).stepReset()
  }
  const eins = {}
  for (const [type, [t, , , cols]] of Object.entries(EDGE_TABLES)) eins[type] = sdb.prepare(`INSERT INTO ${t} (id, src, dst${cols.map(c => ', ' + c).join('')}) VALUES (?, ?, ?${', ?'.repeat(cols.length)})`)
  for (const e of g.edges) eins[e.type].bind([e.id, e.from, e.to, ...EDGE_TABLES[e.type][3].map(c => e.props[c] ?? null)]).stepReset()
  for (const s of [...Object.values(ins), ...Object.values(eins)]) s.finalize()
  const idx = ['CREATE INDEX person_email ON person(email)', 'CREATE INDEX person_country ON person(country)',
    'CREATE INDEX orders_ref ON orders(ref)', 'CREATE INDEX product_sku ON product(sku)']
  for (const [t] of Object.values(EDGE_TABLES)) idx.push(`CREATE INDEX ${t}_src ON ${t}(src, dst)`, `CREATE INDEX ${t}_dst ON ${t}(dst, src)`)
  sdb.exec(idx.join(';'))
  sdb.exec('COMMIT')
  sdb.exec('ANALYZE')
  return performance.now() - t0
}

function loadGliderGraph(g) {
  gdb = glider.open()
  const lines = []
  for (const n of g.nodes) lines.push(JSON.stringify({ type: 'node', id: n.id, labels: [n.label], props: n.props }))
  for (const e of g.edges) lines.push(JSON.stringify({ type: 'edge', label: e.type, from: e.from, to: e.to, props: e.props }))
  const text = lines.join('\n')
  const t0 = performance.now()
  for (const [l, k] of [['Person', 'email'], ['Person', 'country'], ['Order', 'ref'], ['Product', 'sku']]) gdb.run(`INDEX ON :${l}(${k})`)
  gdb.importJsonl(text)
  return performance.now() - t0
}

// ------------------------------------------------------------------ benchmarks

const gq = (q) => gdb.query(q)
const gv = (q) => gq(q).rows
const sv = (sql, bind) => sdb.exec({ sql, bind, returnValue: 'resultRows' })
const one = (rows) => rows.length ? rows[0][0] : 0
const num = (x) => typeof x === 'number' ? Math.round(x * 1e6) / 1e6 : x

function gid(email) { return one(gv(`MATCH (p:Person {email:"${email}"}) RETURN id(p)`)) }

function pagerankSqlite() {
  const n = one(sv('SELECT (SELECT count(*) FROM person) + (SELECT count(*) FROM category) + (SELECT count(*) FROM product) + (SELECT count(*) FROM orders)'))
  const persons = one(sv('SELECT count(*) FROM person'))
  const d = 0.85
  sdb.exec('DROP TABLE IF EXISTS pr; DROP TABLE IF EXISTS deg; DROP TABLE IF EXISTS nx')
  sdb.exec('CREATE TEMP TABLE deg (id INTEGER PRIMARY KEY, n INTEGER); INSERT INTO deg SELECT src, count(*) FROM knows GROUP BY src')
  sdb.exec('CREATE TEMP TABLE pr (id INTEGER PRIMARY KEY, r REAL)')
  sv('INSERT INTO pr SELECT id, ? FROM person', [1 / n])
  let ro = 1 / n
  for (let it = 0; it < 5; it++) {
    const dangling = one(sv('SELECT coalesce(sum(r), 0) FROM pr WHERE id NOT IN (SELECT id FROM deg)')) + (n - persons) * ro
    const base = (1 - d) / n + d * dangling / n
    sdb.exec('CREATE TEMP TABLE nx (id INTEGER PRIMARY KEY, r REAL)')
    sv(`INSERT INTO nx SELECT p.id, ? + ? * coalesce(s.x, 0) FROM pr p LEFT JOIN (
          SELECT k.dst AS id, sum(pr.r / deg.n) AS x FROM knows k JOIN pr ON pr.id = k.src JOIN deg ON deg.id = k.src GROUP BY k.dst
        ) s ON s.id = p.id`, [base, d])
    sdb.exec('DROP TABLE pr; ALTER TABLE nx RENAME TO pr')
    ro = base
  }
  const top = sv('SELECT p.email FROM pr JOIN person p USING (id) ORDER BY pr.r DESC, pr.id LIMIT 5').map(r => r[0])
  sdb.exec('DROP TABLE pr; DROP TABLE deg')
  return top
}

function wccSqlite() {
  const n = one(sv('SELECT (SELECT count(*) FROM person) + (SELECT count(*) FROM category) + (SELECT count(*) FROM product) + (SELECT count(*) FROM orders)'))
  const persons = one(sv('SELECT count(*) FROM person'))
  sdb.exec('DROP TABLE IF EXISTS comp; DROP TABLE IF EXISTS nx; CREATE TEMP TABLE comp (id INTEGER PRIMARY KEY, c INTEGER); INSERT INTO comp SELECT id, id FROM person')
  for (let i = 0; i < 100; i++) {
    sdb.exec(`CREATE TEMP TABLE nx (id INTEGER PRIMARY KEY, c INTEGER);
      INSERT INTO nx SELECT id, min(c) FROM (SELECT id, c FROM comp
        UNION ALL SELECT k.dst, c.c FROM knows k JOIN comp c ON c.id = k.src
        UNION ALL SELECT k.src, c.c FROM knows k JOIN comp c ON c.id = k.dst) GROUP BY id`)
    const changed = one(sv('SELECT count(*) FROM nx JOIN comp USING (id) WHERE nx.c <> comp.c'))
    sdb.exec('DROP TABLE comp; ALTER TABLE nx RENAME TO comp')
    if (!changed) break
  }
  const k = one(sv('SELECT count(DISTINCT c) FROM comp')) + (n - persons)
  sdb.exec('DROP TABLE comp')
  return k
}

function benchmarks() {
  const P = meta.people
  const mid = `user${Math.floor(P / 2)}@example.org`, hub = 'user0@example.org', far = `user${Math.floor(P * 0.9)}@example.org`
  const buyer = 'user1@example.org', midId = meta.ids.mid, farId = meta.ids.far
  return [
    { id: 'point', group: 'Reads', name: 'Point lookup by indexed email',
      cypher: `MATCH (p:Person {email:"${mid}"}) RETURN p.name, p.age`,
      sql: 'SELECT name, age FROM person WHERE email = ?', bind: [mid] },
    { id: 'hop1', group: 'Reads', name: '1 hop: friends, count and average age',
      cypher: `MATCH (p:Person {email:"${mid}"})-[:KNOWS]->(f) RETURN count(f), avg(f.age)`,
      sql: 'SELECT count(*), avg(f.age) FROM person p JOIN knows k ON k.src = p.id JOIN person f ON f.id = k.dst WHERE p.email = ?', bind: [mid] },
    { id: 'hub', group: 'Reads', name: '1 hop in: a power-law hub\'s followers',
      cypher: `MATCH (h:Person {email:"${hub}"})<-[:KNOWS]-(f) RETURN count(f)`,
      sql: 'SELECT count(*) FROM person h JOIN knows k ON k.dst = h.id WHERE h.email = ?', bind: [hub] },
    { id: 'hop2', group: 'Reads', name: '2 hops: friends of friends',
      cypher: `MATCH (p:Person {email:"${mid}"})-[:KNOWS]->()-[:KNOWS]->(x) RETURN count(x)`,
      sql: 'SELECT count(*) FROM person p JOIN knows a ON a.src = p.id JOIN knows b ON b.src = a.dst WHERE p.email = ? AND b.id <> a.id', bind: [mid] },
    { id: 'hop3', group: 'Reads', name: '3 hops out',
      cypher: `MATCH (p:Person {email:"${mid}"})-[:KNOWS]->()-[:KNOWS]->()-[:KNOWS]->(x) RETURN count(x)`,
      sql: 'SELECT count(*) FROM person p JOIN knows a ON a.src = p.id JOIN knows b ON b.src = a.dst JOIN knows c ON c.src = b.dst WHERE p.email = ? AND b.id <> a.id AND c.id <> a.id AND c.id <> b.id', bind: [mid] },
    { id: 'bip', group: 'Reads', name: 'Bipartite: buyer → orders → products',
      cypher: `MATCH (p:Person {email:"${buyer}"})-[:PLACED]->(o)-[:CONTAINS]->(pr) RETURN count(pr)`,
      sql: 'SELECT count(*) FROM person p JOIN placed pl ON pl.src = p.id JOIN contains c ON c.src = pl.dst WHERE p.email = ?', bind: [buyer] },
    { id: 'bfs', group: 'Reads', name: 'BFS: everyone within 3 KNOWS hops',
      cypher: `CALL bfs(from: ${midId}, depth: 3, dir: "out", type: "KNOWS")`, answer: 'rowcount',
      sql: 'WITH RECURSIVE r(id, d) AS (SELECT ?, 0 UNION SELECT k.dst, r.d + 1 FROM r JOIN knows k ON k.src = r.id WHERE r.d < 3) SELECT count(DISTINCT id) FROM r', bind: [meta.sids.mid] },
    { id: 'path', group: 'Reads', name: 'Shortest path between two people',
      cypher: `CALL shortestpath(from: ${midId}, to: ${farId}, dir: "out", type: "KNOWS")`, answer: 'hops',
      sql: 'WITH RECURSIVE r(id, d) AS (SELECT ?, 0 UNION SELECT k.dst, r.d + 1 FROM r JOIN knows k ON k.src = r.id WHERE r.d < 10 AND r.id <> ?) SELECT min(d) FROM r WHERE id = ?', bind: [meta.sids.mid, meta.sids.far, meta.sids.far] },
    { id: 'scan', group: 'Reads', name: 'Label scan with a property filter',
      cypher: 'MATCH (o:Order) WHERE o.total > 1900 RETURN count(o)',
      sql: 'SELECT count(*) FROM orders WHERE total > 1900' },
    { id: 'group', group: 'Reads', name: 'Aggregate: orders by status',
      cypher: 'MATCH (o:Order) RETURN o.status, count(o), avg(o.total) ORDER BY o.status',
      sql: 'SELECT status, count(*), avg(total) FROM orders GROUP BY status ORDER BY status' },
    { id: 'topk', group: 'Reads', name: 'Top 5 products by price',
      cypher: 'MATCH (p:Product) RETURN p.sku, p.price ORDER BY p.price DESC, p.sku LIMIT 5',
      sql: 'SELECT sku, price FROM product ORDER BY price DESC, sku LIMIT 5' },
    { id: 'degree', group: 'Reads', name: 'Degree: top 5 by KNOWS in-degree',
      cypher: 'MATCH (p:Person)<-[:KNOWS]-() RETURN p.email, count(*) AS d ORDER BY d DESC, p.email LIMIT 5',
      sql: 'SELECT p.email, count(*) AS d FROM knows k JOIN person p ON p.id = k.dst GROUP BY k.dst ORDER BY d DESC, p.email LIMIT 5' },
    { id: 'count', group: 'Reads', name: 'Count every node',
      cypher: 'MATCH (n) RETURN count(n)',
      sql: 'SELECT (SELECT count(*) FROM person) + (SELECT count(*) FROM category) + (SELECT count(*) FROM product) + (SELECT count(*) FROM orders)' },
    { id: 'pagerank', group: 'Algorithms', name: 'PageRank: 5 iterations over KNOWS', answer: 'custom',
      cypher: 'CALL pagerank(type: "KNOWS", iterations: 5, tolerance: 0, top: 5)',
      gfn: () => {
        const ids = gv('CALL pagerank(type: "KNOWS", iterations: 5, tolerance: 0, top: 5)').map(r => r[0])
        return ids.map(i => one(gv(`MATCH (p) WHERE id(p) = ${i} RETURN p.email`)))
      },
      sql: 'iterated SQL (see the native suite)', sfn: pagerankSqlite },
    { id: 'wcc', group: 'Algorithms', name: 'Connected components over KNOWS', answer: 'custom',
      cypher: 'CALL wcc(type: "KNOWS", top: 0)',
      gfn: () => parseInt(gq('CALL wcc(type: "KNOWS", top: 0)').message, 10),
      sql: 'iterated minimum-label propagation', sfn: wccSqlite },
    { id: 'insert', group: 'Writes', name: 'Insert 1,000 people, one transaction', answer: 'none', write: true,
      cypher: 'BEGIN; CREATE (:Person {email: …}) ×1000; COMMIT',
      gfn: () => { const r = ++writeRun; gdb.run('BEGIN'); for (let i = 0; i < 1000; i++) gdb.run(`CREATE (:Person {email:"w${r}-${i}@x.org", name:"W ${i}", age: 30, country:"NZ"})`); gdb.run('COMMIT') },
      sql: 'BEGIN; INSERT INTO person … ×1000; COMMIT',
      sfn: () => { const r = writeRun; const st = sdb.prepare("INSERT INTO person (email, name, age, country, active) VALUES (?, ?, 30, 'NZ', 1)"); sdb.exec('BEGIN'); for (let i = 0; i < 1000; i++) st.bind([`w${r}-${i}@x.org`, `W ${i}`]).stepReset(); sdb.exec('COMMIT'); st.finalize() } },
    { id: 'update', group: 'Writes', name: 'Update 1,000 people by indexed email, one transaction', answer: 'none', write: true,
      cypher: 'BEGIN; MATCH (p:Person {email: …}) SET p.age = … ×1000; COMMIT',
      gfn: () => { gdb.run('BEGIN'); for (let i = 0; i < 1000; i++) gdb.run(`MATCH (p:Person {email:"user${(i * 7919) % P}@example.org"}) SET p.age = ${20 + i % 50}`); gdb.run('COMMIT') },
      sql: 'BEGIN; UPDATE person SET age = ? WHERE email = ? ×1000; COMMIT',
      sfn: () => { const st = sdb.prepare('UPDATE person SET age = ? WHERE email = ?'); sdb.exec('BEGIN'); for (let i = 0; i < 1000; i++) st.bind([20 + i % 50, `user${(i * 7919) % P}@example.org`]).stepReset(); sdb.exec('COMMIT'); st.finalize() } },
  ]
}

function answerOf(b, engine, rows, value) {
  if (b.answer === 'none') return null
  if (b.answer === 'custom') return value
  if (b.answer === 'rowcount') return engine === 'glider' ? rows.length : rows[0][0]
  if (b.answer === 'hops') return engine === 'glider' ? (rows.length ? rows.length - 1 : null) : rows[0][0]
  return JSON.stringify(rows.map(r => r.map(num)))
}

function measure(fn, budgetMs, maxRuns) {
  const times = []
  let out
  const t0 = performance.now()
  do {
    const t = performance.now()
    out = fn()
    times.push(performance.now() - t)
  } while (performance.now() - t0 < budgetMs && times.length < maxRuns)
  times.sort((a, b) => a - b)
  return { first: times.length ? null : null, median: times[Math.floor(times.length / 2)], runs: times.length, out }
}

function run(b) {
  const budget = b.write ? 0 : 400, maxRuns = b.write ? 1 : 200
  const g = measure(b.gfn ?? (() => gv(b.cypher)), budget, maxRuns)
  const s = measure(b.sfn ?? (() => sv(b.sql, b.bind)), budget, maxRuns)
  const ga = answerOf(b, 'glider', g.out, g.out), sa = answerOf(b, 'sqlite', s.out, s.out)
  return {
    id: b.id, glider_ms: g.median, sqlite_ms: s.median, glider_runs: g.runs, sqlite_runs: s.runs,
    match: b.answer === 'none' ? null : JSON.stringify(ga) === JSON.stringify(sa),
    glider_answer: ga, sqlite_answer: sa,
  }
}

self.onmessage = async (e) => {
  const m = e.data
  try {
    if (m.op === 'init') {
      const t0 = performance.now()
      // self.testHooks lets a Node test hand over the wasm bytes (Node's
      // fetch cannot read file: URLs); browsers load the files beside this one.
      const hooks = self.testHooks || {}
      glider = await loadGlider(hooks.glider ?? new URL('./glider/glider.wasm', import.meta.url))
      const t1 = performance.now()
      sqlite3 = await sqlite3InitModule({ print: () => {}, printErr: () => {}, ...(hooks.sqlite || {}) })
      post({ op: 'ready', glider_version: glider.version, sqlite_version: sqlite3.version.libVersion,
             glider_init_ms: t1 - t0, sqlite_init_ms: performance.now() - t1 })
    } else if (m.op === 'build') {
      if (gdb) gdb.close()
      if (sdb) sdb.close()
      const t0 = performance.now()
      const g = generate(m.people)
      const gen_ms = performance.now() - t0
      const sqlite_load_ms = loadSqlite(g)
      const glider_load_ms = loadGliderGraph(g)
      meta = { people: g.people, nodes: g.nodes.length, edges: g.edges.length }
      meta.ids = { mid: gid(`user${Math.floor(g.people / 2)}@example.org`), far: gid(`user${Math.floor(g.people * 0.9)}@example.org`) }
      meta.sids = { mid: 1 + Math.floor(g.people / 2), far: 1 + Math.floor(g.people * 0.9) }
      writeRun = 0
      post({ op: 'built', ...meta, gen_ms, glider_load_ms, sqlite_load_ms,
             list: benchmarks().map(({ id, group, name, cypher, sql }) => ({ id, group, name, cypher, sql })) })
    } else if (m.op === 'run') {
      const b = benchmarks().find(x => x.id === m.id)
      post({ op: 'result', ...run(b) })
    }
  } catch (err) {
    post({ op: 'error', id: m.id, message: String(err && err.message || err) })
  }
}
