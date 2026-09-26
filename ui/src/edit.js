// Writes for the explorer, expressed as queries.
//
// Every edit the inspector makes — a property, a label, a new node, a new
// relationship, a delete — goes through the same query language a user would
// type. That is deliberate: it means the edit is logged, indexed and committed
// exactly like a typed statement, and works unchanged under both transports.
// The only job here is rendering JavaScript values as literals the lexer will
// read back the same way.

import { runQuery } from './api'

// ------------------------------------------------------------- rendering

/** A float that happens to be integral. `5` would be read back as an Int. */
export class FloatNum {
  constructor(v) {
    this.value = v
  }
}

const IDENT = /^[A-Za-z_][A-Za-z0-9_]*$/

/** A label, relationship type or property key, backtick-quoted if needed. */
export function ident(name) {
  const s = String(name)
  if (!s) throw new Error('a name cannot be empty')
  if (IDENT.test(s)) return s
  if (s.includes('`')) throw new Error('a name cannot contain a backtick')
  return '`' + s + '`'
}

function str(s) {
  // The lexer understands \\ \" \n \r \t. JSON.stringify would emit \uXXXX
  // for control characters, which it does not.
  return (
    '"' +
    String(s)
      .replace(/\\/g, '\\\\')
      .replace(/"/g, '\\"')
      .replace(/\n/g, '\\n')
      .replace(/\r/g, '\\r')
      .replace(/\t/g, '\\t') +
    '"'
  )
}

function num(v) {
  if (!Number.isFinite(v)) throw new Error('numbers must be finite')
  const s = String(v)
  if (/e/i.test(s)) throw new Error('number is out of the range the query language accepts')
  return s
}

/** Render a value as a literal. */
export function lit(v) {
  if (v === null || v === undefined) return 'null'
  if (v instanceof FloatNum) {
    const s = num(v.value)
    return s.includes('.') ? s : s + '.0'
  }
  if (typeof v === 'boolean') return String(v)
  if (typeof v === 'number') return num(v)
  if (Array.isArray(v)) return '[' + v.map(lit).join(', ') + ']'
  return str(v)
}

/** Render `{k: v, ...}`. Empty maps render as nothing at all. */
export function propMap(props) {
  const entries = Object.entries(props || {})
  if (!entries.length) return ''
  return ' {' + entries.map(([k, v]) => `${ident(k)}: ${lit(v)}`).join(', ') + '}'
}

// ------------------------------------------------------------ typed input

/**
 * Turn what someone typed into a value. With `type` 'auto' the text is read
 * the way a literal would be: `42` is an int, `4.2` a float, `true` a bool,
 * `null` null, `[..]` a list, anything else text. The explicit types exist for
 * the cases auto gets wrong — a postcode that must stay text, a `5` that must
 * be a float.
 */
export function parseTyped(text, type = 'auto') {
  const t = String(text)
  switch (type) {
    case 'text':
      return t
    case 'int': {
      if (!/^-?\d+$/.test(t.trim())) throw new Error(`"${t}" is not an integer`)
      return parseInt(t, 10)
    }
    case 'float': {
      const f = Number(t)
      if (t.trim() === '' || !Number.isFinite(f)) throw new Error(`"${t}" is not a number`)
      return Number.isInteger(f) ? new FloatNum(f) : f
    }
    case 'bool':
      return /^(true|t|yes|y|1)$/i.test(t.trim())
    case 'null':
      return null
    case 'list': {
      let v
      try {
        v = JSON.parse(t)
      } catch {
        throw new Error('a list is written like JSON: ["a", 2, true]')
      }
      if (!Array.isArray(v)) throw new Error('a list is written like JSON: ["a", 2, true]')
      return v
    }
    default: {
      const s = t.trim()
      if (s === 'null') return null
      if (s === 'true') return true
      if (s === 'false') return false
      if (/^-?\d+$/.test(s) && Math.abs(Number(s)) <= Number.MAX_SAFE_INTEGER) return parseInt(s, 10)
      if (/^-?(\d+\.\d*|\.\d+)$/.test(s)) {
        const f = Number(s)
        return Number.isInteger(f) ? new FloatNum(f) : f
      }
      if (s.startsWith('[') && s.endsWith(']')) {
        try {
          const v = JSON.parse(s)
          if (Array.isArray(v)) return v
        } catch {
          /* fall through to text */
        }
      }
      return t
    }
  }
}

/** The type name of a value, as the engine would report it. */
export function typeOf(v) {
  if (v === null || v === undefined) return 'null'
  if (v instanceof FloatNum) return 'float'
  if (typeof v === 'boolean') return 'bool'
  if (typeof v === 'number') return Number.isInteger(v) ? 'int' : 'float'
  if (Array.isArray(v)) return 'list'
  return 'text'
}

/** Text to put back in an input for an existing value. */
export function toText(v) {
  if (v === null || v === undefined) return ''
  if (Array.isArray(v)) return JSON.stringify(v)
  return String(v)
}

// ----------------------------------------------------------------- reads

function firstCell(r) {
  return r.rows?.[0]?.[0] ?? null
}

/** One node by id, or null if it no longer exists. */
export async function fetchNode(id) {
  return firstCell(await runQuery(`MATCH (n) WHERE id(n) = ${Number(id)} RETURN n`))
}

/**
 * One relationship. Anchored on its source node so the lookup is O(degree)
 * rather than a scan of every edge — id(r) alone is not indexed.
 */
export async function fetchEdge(edge) {
  const q = `MATCH (a)-[r]->() WHERE id(a) = ${Number(edge.from)} AND id(r) = ${Number(edge.id)} RETURN r`
  return firstCell(await runQuery(q))
}

// ---------------------------------------------------------------- writes

const nodeMatch = (id) => `MATCH (n) WHERE id(n) = ${Number(id)}`
const edgeMatch = (e) => `MATCH (a)-[r]->() WHERE id(a) = ${Number(e.from)} AND id(r) = ${Number(e.id)}`

/** Set one property on a node or relationship; resolves to the refreshed entity. */
export async function setProp(ent, key, value) {
  if (ent._e === 'node') {
    await runQuery(`${nodeMatch(ent.id)} SET n.${ident(key)} = ${lit(value)}`)
    return fetchNode(ent.id)
  }
  await runQuery(`${edgeMatch(ent)} SET r.${ident(key)} = ${lit(value)}`)
  return fetchEdge(ent)
}

export async function removeProp(ent, key) {
  if (ent._e === 'node') {
    await runQuery(`${nodeMatch(ent.id)} REMOVE n.${ident(key)}`)
    return fetchNode(ent.id)
  }
  await runQuery(`${edgeMatch(ent)} REMOVE r.${ident(key)}`)
  return fetchEdge(ent)
}

export async function addLabel(node, label) {
  await runQuery(`${nodeMatch(node.id)} SET n:${ident(label)}`)
  return fetchNode(node.id)
}

export async function removeLabel(node, label) {
  await runQuery(`${nodeMatch(node.id)} REMOVE n:${ident(label)}`)
  return fetchNode(node.id)
}

/** Create a node; resolves to it. */
export async function createNode(labels, props) {
  const ls = (labels || []).map((l) => ':' + ident(l)).join('')
  const r = await runQuery(`CREATE (n${ls}${propMap(props)})`)
  // CREATE reports the ids of the nodes it made as rows.
  const id = firstCell(r)
  if (id === null) throw new Error('create returned no id')
  return fetchNode(id)
}

/** Create a relationship between two existing nodes; resolves to it. */
export async function createEdge(fromId, toId, type, props) {
  const f = Number(fromId)
  const t = Number(toId)
  await runQuery(
    `MATCH (a), (b) WHERE id(a) = ${f} AND id(b) = ${t} CREATE (a)-[:${ident(type)}${propMap(props)}]->(b)`,
  )
  // The tail form of CREATE reports only a count, so pick up the newest edge
  // of that type between the pair — which is the one just made. ORDER BY
  // must name a returned column, hence the aliased id.
  const r = await runQuery(
    `MATCH (a)-[r:${ident(type)}]->(b) WHERE id(a) = ${f} AND id(b) = ${t} RETURN r, id(r) AS rid ORDER BY rid DESC LIMIT 1`,
  )
  return firstCell(r)
}

export async function deleteNode(id) {
  await runQuery(`${nodeMatch(id)} DETACH DELETE n`)
}

export async function deleteEdge(edge) {
  await runQuery(`${edgeMatch(edge)} DELETE r`)
}
