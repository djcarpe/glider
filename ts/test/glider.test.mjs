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
