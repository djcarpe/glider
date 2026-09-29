import { test, before, describe } from 'node:test'
import assert from 'node:assert/strict'
import { loadGlider, isNode, isRel, GliderError } from '../dist/index.js'

let glider

before(async () => {
  glider = await loadGlider()
})

/** Node count via STATS, which reports 0 rather than returning no rows. */
function nodeCount(db) {
  const row = db.stats().rows.find((r) => r[0] === 'nodes')
  return row[1]
}

function seeded() {
  const db = glider.open()
  db.run(`CREATE (a:Person {name:"Ada", age:36})-[:KNOWS {since:2019}]->(b:Person {name:"Bob", age:41})`)
  db.run(`MATCH (a:Person {name:"Ada"}) CREATE (a)-[:LIVES_IN]->(:City {name:"London"})`)
  return db
}

describe('module', () => {
  test('reports a version', () => {
    assert.match(glider.version, /^\d+\.\d+\.\d+$/)
  })

  test('the wasm module needs no imports at all', async () => {
    const { readFile } = await import('node:fs/promises')
    const bytes = await readFile(new URL('../dist/glider.wasm', import.meta.url))
    const mod = await WebAssembly.compile(bytes)
    // This is the portability claim, so assert it rather than trusting it:
    // no WASI, no bindgen glue, runs in any host that speaks plain wasm.
    assert.deepEqual(WebAssembly.Module.imports(mod), [])
  })

  test('graphs are isolated from one another', () => {
    const a = glider.open()
    const b = glider.open()
    a.run('CREATE (:Only {in:"a"})')
    // Note: count() over zero matches yields zero ROWS in glider, not one row
    // holding 0 as Cypher would. Counting via stats sidesteps that.
    assert.equal(nodeCount(a), 1)
    assert.equal(nodeCount(b), 0)
    a.close()
    b.close()
  })
})

describe('queries', () => {
  test('entities come back typed, not as JSON strings', () => {
    const db = seeded()
    const r = db.query('MATCH (a)-[r:KNOWS]->(b) RETURN a, r, b')
    const [a, rel, b] = r.rows[0]

    assert.ok(isNode(a), 'first cell should be a node')
    assert.ok(isRel(rel), 'second cell should be a relationship')
    assert.ok(isNode(b))
    assert.equal(a.props.name, 'Ada')
    assert.equal(a.props.age, 36)
    assert.deepEqual(a.labels, ['Person'])
    assert.equal(rel.type, 'KNOWS')
    assert.equal(rel.props.since, 2019)
    assert.equal(rel.from, a.id)
    assert.equal(rel.to, b.id)
    db.close()
  })

  test('scalars stay scalars', () => {
    const db = seeded()
    const r = db.query('MATCH (p:Person) RETURN p.name, p.age ORDER BY p.age')
    assert.deepEqual(r.columns, ['p.name', 'p.age'])
    assert.deepEqual(r.rows, [['Ada', 36], ['Bob', 41]])
    db.close()
  })

  test('the graph payload is deduplicated and closed over endpoints', () => {
    const db = seeded()
    // Returns only the relationship; endpoints must still be drawable.
    const g = db.graph('MATCH ()-[r:KNOWS]->() RETURN r')
    assert.equal(g.edges.length, 1)
    assert.equal(g.nodes.length, 2)

    const all = db.graph('MATCH (a)-[r]->(b) RETURN a, r, b')
    assert.equal(all.nodes.length, 3, 'Ada, Bob, London — Ada appears twice but counts once')
    assert.equal(all.edges.length, 2)
    db.close()
  })

  test('a text property that mimics a node is not turned into one', () => {
    const db = glider.open()
    db.run(`CREATE (:Decoy {trap:"{\\"id\\":0,\\"labels\\":[\\"Person\\"],\\"props\\":{}}"})`)
    const r = db.query('MATCH (d:Decoy) RETURN d.trap')
    assert.equal(typeof r.rows[0][0], 'string', 'decoy should stay a string')
    assert.equal(r.graph.nodes.length, 0, 'decoy must not be drawn')
    db.close()
  })

  test('write statements report what they touched', () => {
    const db = glider.open()
    assert.equal(db.run('CREATE (:A)-[:R]->(:B)'), 3)
    db.close()
  })
})

describe('schema and expand', () => {
  test('schema lists labels and relationship types with counts', () => {
    const db = seeded()
    const s = db.schema()
    assert.deepEqual(
      s.labels.map((l) => [l.name, l.count]).sort(),
      [['City', 1], ['Person', 2]],
    )
    assert.deepEqual(
      s.edge_types.map((t) => [t.name, t.count]).sort(),
      [['KNOWS', 1], ['LIVES_IN', 1]],
    )
    db.close()
  })

  test('expand walks both directions', () => {
    const db = seeded()
    const ada = db.query('MATCH (p:Person {name:"Ada"}) RETURN p').rows[0][0]
    const outward = db.expand(ada.id)
    // Ada knows Bob and lives in London.
    assert.equal(outward.edges.length, 2)
    assert.equal(outward.nodes.length, 3)

    const bob = db.query('MATCH (p:Person {name:"Bob"}) RETURN p').rows[0][0]
    // Bob has only the inbound KNOWS, which expand must still find.
    assert.equal(db.expand(bob.id).edges.length, 1)
    db.close()
  })

  test('expanding an unknown node is an error, not a crash', () => {
    const db = seeded()
    assert.throws(() => db.expand(9_999_999), GliderError)
    db.close()
  })
})

describe('jsonl round trip', () => {
  test('export then import reproduces the graph', () => {
    const src = seeded()
    const dump = src.exportJsonl()
    assert.ok(dump.includes('"Ada"'))

    const dst = glider.open()
    dst.importJsonl(dump)
    assert.equal(nodeCount(dst), nodeCount(src))
    assert.deepEqual(
      dst.query('MATCH ()-[r]->() RETURN count(r)').rows[0][0],
      src.query('MATCH ()-[r]->() RETURN count(r)').rows[0][0],
    )
    src.close()
    dst.close()
  })

  test('a large import survives wasm memory growth', () => {
    // Allocating well past the initial linear memory forces a memory.grow,
    // which detaches every existing JS view. If any helper cached one, this
    // is where it corrupts.
    const db = glider.open()
    const lines = []
    for (let i = 0; i < 20_000; i++) {
      lines.push(JSON.stringify({ type: 'node', id: i, labels: ['N'], props: { i, pad: 'x'.repeat(64) } }))
    }
    db.importJsonl(lines.join('\n'))
    assert.equal(db.query('MATCH (n:N) RETURN count(n)').rows[0][0], 20_000)
    // And the engine still answers correctly afterwards.
    assert.equal(db.query('MATCH (n:N) WHERE n.i = 19999 RETURN n.i').rows[0][0], 19_999)
    db.close()
  })
})

describe('algorithms', () => {
  test('pagerank runs and writes back', () => {
    const db = seeded()
    db.query('CALL pagerank(iterations: 10, write: "rank")')
    const r = db.query('MATCH (p:Person) RETURN p.name, p.rank ORDER BY p.rank DESC')
    assert.equal(r.rows.length, 2)
    assert.equal(typeof r.rows[0][1], 'number')
    db.close()
  })
})

describe('errors', () => {
  test('a syntax error throws GliderError with the engine message', () => {
    const db = glider.open()
    assert.throws(
      () => db.query('MATCH (((('),
      (e) => e instanceof GliderError && /expected/.test(e.message),
    )
    // The handle is still usable afterwards.
    db.run('CREATE (:Fine)')
    assert.equal(db.query('MATCH (n:Fine) RETURN count(n)').rows[0][0], 1)
    db.close()
  })

  test('using a closed graph throws rather than trapping', () => {
    const db = glider.open()
    db.close()
    assert.throws(() => db.query('MATCH (n) RETURN n'), /closed/)
  })

  test('close is idempotent', () => {
    const db = glider.open()
    db.close()
    db.close()
  })
})

describe('paging', () => {
  test('nodes page by id cursor and filter by label and text', () => {
    const db = seeded()
    const first = db.nodes({ limit: 2 })
    assert.equal(first.nodes.length, 2)
    assert.equal(first.total, 3)
    assert.ok(first.next !== null)
    const rest = db.nodes({ from: first.next, limit: 2 })
    assert.equal(rest.nodes.length, 1)
    assert.equal(rest.next, null)

    assert.deepEqual(db.nodes({ label: 'City' }).nodes.map((n) => n.props.name), ['London'])
    // Case-insensitive, against any property value.
    assert.deepEqual(db.nodes({ q: 'ADA' }).nodes.map((n) => n.props.name), ['Ada'])
    assert.equal(db.nodes({ q: 'nobody' }).nodes.length, 0)
    // Degree rides along so a list can show connectivity.
    assert.equal(db.nodes({ q: 'Ada' }).nodes[0].degree, 2)
    db.close()
  })

  test('edges page with their endpoints', () => {
    const db = seeded()
    const page = db.edges({ type: 'KNOWS' })
    assert.equal(page.edges.length, 1)
    assert.equal(page.edges[0].type, 'KNOWS')
    assert.equal(page.nodes.length, 2)
    assert.equal(db.edges({ q: 'lives' }).edges.length, 1)
    assert.equal(db.edges().total, 2)
    db.close()
  })

  test('schema carries true node and edge totals', () => {
    const db = seeded()
    const s = db.schema()
    assert.equal(s.nodes, 3)
    assert.equal(s.edges, 2)
    assert.deepEqual(s.node_keys.Person, ['age', 'name'])
    assert.deepEqual(s.edge_keys.KNOWS, ['since'])
    db.close()
  })
})

describe('opening a database file', () => {
  const fixture = new URL('./fixtures/people.gldb', import.meta.url)

  test('a .gldb opens from its bytes', async () => {
    const { readFile } = await import('node:fs/promises')
    const db = glider.openBytes(await readFile(fixture))
    assert.equal(nodeCount(db), 3)
    const r = db.query('MATCH (a:Person)-[r:KNOWS]->(b) RETURN a.name, r.since, b.name')
    assert.deepEqual(r.rows, [['Ada', 2019, 'Bob']])
    // Writable, in memory only; ids carry on past the file's.
    db.run('CREATE (:Person {name:"Cai"})')
    assert.equal(nodeCount(db), 4)
    db.close()
  })

  test('something that is not a database is an error', () => {
    assert.throws(() => glider.openBytes(new TextEncoder().encode('{"hello":1}')), GliderError)
    assert.throws(() => glider.openBytes(new Uint8Array(0)), /too short/)
  })
})

describe('telemetry', () => {
  /** A tracer and meter with the @opentelemetry/api shape, recording calls. */
  function fakes() {
    const spans = []
    const tracer = {
      startSpan(name, options = {}) {
        const s = { name, kind: options.kind, attributes: { ...options.attributes }, status: null, exceptions: [], ended: false }
        spans.push(s)
        return {
          updateName: (n) => (s.name = n),
          setAttributes: (a) => Object.assign(s.attributes, a),
          setStatus: (st) => (s.status = st),
          recordException: (e) => s.exceptions.push(e),
          end: () => (s.ended = true),
        }
      },
    }
    const recorded = []
    const callbacks = new Map()
    const observable = (name) => ({ addCallback: (cb) => callbacks.set(name, cb) })
    const meter = {
      createHistogram: (name) => ({ record: (v, a) => recorded.push({ name, v, a }) }),
      createObservableCounter: observable,
      createObservableGauge: observable,
    }
    /** Run one instrument's callback, as a metric reader would. */
    const collect = (name) => {
      const out = []
      callbacks.get(name)({ observe: (v, a) => out.push({ v, a }) })
      return out
    }
    return { spans, tracer, meter, recorded, collect }
  }

  test('each statement is a span carrying the engine report', async () => {
    const f = fakes()
    const g = await loadGlider(undefined, { telemetry: { tracer: f.tracer, meter: f.meter } })
    const db = g.open()
    db.run('CREATE (:Person {name:"Ada"})-[:KNOWS]->(:Person {name:"Bob"})')
    const r = db.query('MATCH (p:Person) RETURN p.name')
    assert.ok(r.ms >= 0)
    assert.equal(r.op.op, 'MATCH')
    assert.equal(r.op.rows, 2)
    const span = f.spans.at(-1)
    assert.equal(span.name, 'glider MATCH')
    assert.equal(span.kind, 2)
    assert.ok(span.ended)
    assert.equal(span.attributes['db.system.name'], 'glider')
    assert.equal(span.attributes['db.operation.name'], 'MATCH')
    assert.equal(span.attributes['db.response.returned_rows'], 2)
    assert.equal(span.attributes['db.query.text'], 'MATCH (p:Person) RETURN p.name')
    assert.equal(f.spans[0].attributes['glider.touched'], 3)
    assert.equal(f.recorded.at(-1).name, 'glider.query.duration')
    assert.equal(f.recorded.at(-1).a['db.operation.name'], 'MATCH')

    assert.throws(() => db.query('MATCH (n RETURN n'), GliderError)
    const bad = f.spans.at(-1)
    assert.equal(bad.status.code, 2)
    assert.equal(bad.attributes['db.operation.name'], 'INVALID')
    assert.equal(bad.exceptions.length, 1)
    assert.equal(f.recorded.at(-1).a['glider.outcome'], 'error')

    const queries = f.collect('glider.queries')
    assert.ok(queries.some((q) => q.a['db.operation.name'] === 'MATCH' && q.a['glider.outcome'] === 'ok' && q.v >= 1))
    const nodes = f.collect('glider.db.nodes')
    assert.deepEqual(nodes.map((n) => n.v), [2])
    assert.match(nodes[0].a['glider.db'], /^:memory:\d+$/)
    db.close()
    assert.deepEqual(f.collect('glider.db.nodes'), [])
  })

  test('the engine exposes its counters, reports and OTLP metrics without an SDK', async () => {
    const g = await loadGlider()
    const db = g.open()
    db.run('CREATE (:City {name:"Oslo"})')
    const last = g.lastOp()
    assert.equal(last.op, 'CREATE')
    assert.equal(last.touched, 1)
    assert.equal(last.duration_ns, undefined, 'no clock inside wasm')
    const snap = g.telemetry()
    assert.ok(snap.duration.count >= 1, 'the wrapper feeds host timings into the histogram')
    assert.equal(db.metrics().nodes, 1)

    const otlp = JSON.parse(g.otlpMetrics('browser-app'))
    const res = otlp.resourceMetrics[0]
    assert.deepEqual(res.resource.attributes[0], { key: 'service.name', value: { stringValue: 'browser-app' } })
    const names = res.scopeMetrics[0].metrics.map((m) => m.name)
    for (const n of ['glider.queries', 'glider.db.nodes', 'glider.query.duration']) assert.ok(names.includes(n), n)
    const t = Number(res.scopeMetrics[0].metrics[0].sum.dataPoints[0].timeUnixNano)
    assert.ok(Math.abs(t / 1e6 - Date.now()) < 60_000, 'timestamps come from the host clock')
    assert.match(g.prometheus(), /glider_db_nodes\{glider_db=":memory:\d+"\} 1/)

    const posts = []
    await g.exportOtlp({
      endpoint: 'http://collector:4318/',
      headers: { authorization: 'Bearer t' },
      fetch: async (url, init) => {
        posts.push({ url, init })
        return { ok: true, status: 200 }
      },
    })
    assert.equal(posts[0].url, 'http://collector:4318/v1/metrics')
    assert.equal(posts[0].init.headers.authorization, 'Bearer t')
    assert.ok(JSON.parse(posts[0].init.body).resourceMetrics)
    db.close()
  })
})
