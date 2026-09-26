// Type-ahead for the query editor.
//
// One pure function, `complete(text, cursor, schema)`, reads the text before
// the cursor and decides what could come next: a statement to start with, a
// label after `(n:`, a relationship type after `[:`, a property key after
// `n.` or inside `{...}`, a procedure after CALL and its arguments, or the
// keywords, variables and functions that fit the clause being written.
//
// It is a heuristic over the raw text rather than a parser: it has to work
// on half-written queries, which is exactly when a real parser gives up. The
// vocabulary here mirrors query.rs (see HELP there); keep the two in step.
//
// An item's `insert` may carry `${text}` — that text is selected after
// insertion, so a template's placeholder can be typed over — or `$0`, where
// the cursor lands. Without either the cursor goes to the end.

/** Whole statements, offered on an empty line. The main teaching surface. */
const STARTERS = [
  { label: 'MATCH … RETURN', insert: 'MATCH (n${})\nRETURN n LIMIT 25', detail: 'find nodes; type : after n to pick a label' },
  { label: 'MATCH (a)-[r]->(b)', insert: 'MATCH (a)-[r${}]->(b)\nRETURN a, r, b LIMIT 25', detail: 'find relationships and both ends' },
  { label: 'MATCH … WHERE', insert: 'MATCH (n$0)\nWHERE n.${name} = ""\nRETURN n', detail: 'filter on a property' },
  { label: 'MATCH paths 1..3 hops', insert: 'MATCH (a$0)-[*1..3]->(b)\nRETURN DISTINCT b LIMIT 50', detail: 'variable-length paths' },
  { label: 'count by label', insert: 'MATCH (n) RETURN labels(n) AS label, count(n) AS n ORDER BY n DESC', detail: 'what is in this graph?' },
  { label: 'CREATE node', insert: 'CREATE (n:${Label} {name: ""})', detail: 'add a node with a label and properties' },
  { label: 'CREATE relationship', insert: 'MATCH (a), (b)\nWHERE id(a) = ${1} AND id(b) = 2\nCREATE (a)-[:RELATED]->(b)', detail: 'connect two existing nodes' },
  { label: 'SET property', insert: 'MATCH (n) WHERE id(n) = ${1}\nSET n.key = "value"', detail: 'change a property' },
  { label: 'DETACH DELETE', insert: 'MATCH (n) WHERE id(n) = ${1}\nDETACH DELETE n', detail: 'delete a node and its relationships' },
  { label: 'CALL algorithm', insert: 'CALL ${}', detail: 'pagerank, shortestpath, communities, …' },
  { label: 'INDEX ON', insert: 'INDEX ON :${Label}(key)', detail: 'speed up lookups by a property' },
  { label: 'DROP INDEX ON', insert: 'DROP INDEX ON :${Label}(key)', detail: 'remove an index' },
  { label: 'EXPLAIN', insert: 'EXPLAIN MATCH ', detail: 'show the plan instead of running' },
  { label: 'SCHEMA', insert: 'SCHEMA', detail: 'labels, relationship types and indexes' },
  { label: 'STATS', insert: 'STATS', detail: 'node, edge and index counts' },
  { label: 'HELP', insert: 'HELP', detail: 'the query language on one page' },
  { label: 'BEGIN', insert: 'BEGIN', detail: 'start a transaction' },
  { label: 'COMMIT', insert: 'COMMIT', detail: 'commit the open transaction' },
  { label: 'COMPACT', insert: 'COMPACT', detail: 'rewrite the file to its minimal form' },
  { label: 'CLEAR', insert: 'CLEAR', detail: 'delete everything — careful' },
]

const FUNCTIONS = [
  ['id', 'id(n)', 'the id of a node or relationship'],
  ['labels', 'labels(n)', 'the labels of a node'],
  ['type', 'type(r)', 'the type of a relationship'],
  ['keys', 'keys(n)', 'property names'],
  ['degree', 'degree(n)', 'number of relationships'],
  ['indegree', 'indegree(n)', 'incoming relationships'],
  ['outdegree', 'outdegree(n)', 'outgoing relationships'],
  ['length', 'length(x)', 'length of a string or list'],
  ['size', 'size(x)', 'length of a string or list'],
  ['lower', 'lower(s)', 'lower-case'],
  ['upper', 'upper(s)', 'upper-case'],
  ['trim', 'trim(s)', 'strip surrounding whitespace'],
  ['abs', 'abs(x)', 'absolute value'],
  ['round', 'round(x)', 'nearest integer'],
  ['floor', 'floor(x)', 'round down'],
  ['ceil', 'ceil(x)', 'round up'],
  ['sqrt', 'sqrt(x)', 'square root'],
  ['toInt', 'toInt(x)', 'convert to an integer'],
  ['toFloat', 'toFloat(x)', 'convert to a float'],
  ['toString', 'toString(x)', 'convert to text'],
  ['coalesce', 'coalesce(a, b, …)', 'first non-null argument'],
]

const AGGREGATES = [
  ['count', 'count(x)', 'number of rows; count(*) counts all'],
  ['sum', 'sum(x)', 'total'],
  ['avg', 'avg(x)', 'mean'],
  ['min', 'min(x)', 'smallest'],
  ['max', 'max(x)', 'largest'],
  ['collect', 'collect(x)', 'gather into a list'],
]

const COMMON = ['dir', 'type', 'top']
/** CALL procedures: name, arguments, what it does. From query.rs HELP. */
const PROCEDURES = [
  ['pagerank', ['damping', 'iterations', 'tolerance', 'dir', 'type', 'weight', 'top', 'write'], 'importance by incoming links', 'pagerank(iterations: 20, top: ${10})'],
  ['betweenness', ['dir', 'type', 'samples', 'top', 'write'], 'nodes that sit on many shortest paths', 'betweenness(top: ${10})'],
  ['closeness', ['dir', 'type', 'weighted', 'top', 'write'], 'how near a node is to everything else', 'closeness(top: ${10})'],
  ['degree', ['dir', 'type', 'top', 'write'], 'most-connected nodes', 'degree(dir: "both", top: ${10})'],
  ['triangles', ['type', 'top', 'write'], 'triangles each node is part of', 'triangles(top: ${10})'],
  ['clustering', ['type', 'top', 'write'], 'local clustering coefficient', 'clustering(top: ${10})'],
  ['kcore', ['type', 'top', 'write'], 'k-core number of each node', 'kcore(top: ${10})'],
  ['components', ['type', 'top', 'write'], 'weakly connected components', 'components(top: ${10})'],
  ['scc', ['type', 'top', 'write'], 'strongly connected components', 'scc(top: ${10})'],
  ['communities', ['type', 'iterations', 'top', 'write'], 'label-propagation communities', 'communities(top: ${10})'],
  ['shortestpath', ['from', 'to', 'dir', 'type', 'weight'], 'shortest path between two node ids', 'shortestpath(from: ${1}, to: 2)'],
  ['sssp', ['from', 'dir', 'type', 'weight', 'top'], 'distances from one node to all others', 'sssp(from: ${1}, top: 10)'],
  ['bfs', ['from', 'depth', 'dir', 'type', 'top'], 'breadth-first walk from a node', 'bfs(from: ${1}, depth: 2)'],
  ['dfs', ['from', 'depth', 'dir', 'type', 'top'], 'depth-first walk from a node', 'dfs(from: ${1}, depth: 2)'],
  ['neighbors', ['from', 'dir', 'type'], 'direct neighbours of a node', 'neighbors(from: ${1})'],
  ['subgraph', ['from', 'depth', 'dir', 'type'], 'everything within depth hops, drawn', 'subgraph(from: ${1}, depth: 2)'],
  ['toposort', ['type', 'top'], 'topological order (DAGs only)', 'toposort(top: ${50})'],
  ['cycle', ['type'], 'find a cycle, if there is one', 'cycle()$0'],
  ['mst', ['type', 'weight'], 'minimum spanning tree', 'mst(weight: "${cost}")'],
]

const ARG_HELP = {
  dir: '"out" | "in" | "both"',
  type: 'only follow this relationship type',
  weight: 'property holding each relationship’s cost',
  top: 'rows returned',
  write: 'store the result on each node under this key',
  from: 'start node id',
  to: 'end node id',
  depth: 'hops to walk',
  iterations: 'rounds to run',
  damping: 'probability of following a link (0.85)',
  tolerance: 'stop when scores change less than this',
  samples: 'estimate from this many sources',
  weighted: 'use the weight property',
}

// Clause keywords, each with what may follow it, so suggestions fit the
// clause being written rather than listing the whole language.
const AFTER_PATTERN = ['WHERE', 'RETURN', 'SET', 'REMOVE', 'DELETE', 'DETACH DELETE', 'CREATE']
const IN_WHERE = ['AND', 'OR', 'NOT', 'IS NULL', 'IS NOT NULL', 'IN', 'CONTAINS', 'STARTS WITH', 'ENDS WITH', 'true', 'false', 'null', 'RETURN', 'SET', 'REMOVE', 'DELETE', 'DETACH DELETE', 'CREATE']
const IN_RETURN = ['DISTINCT', 'AS', 'ORDER BY', 'SKIP', 'LIMIT']
const IN_ORDER = ['ASC', 'DESC', 'SKIP', 'LIMIT']
const IN_WRITE = ['RETURN', 'SET', 'REMOVE', 'DELETE', 'DETACH DELETE', 'CREATE']

const KW_HELP = {
  WHERE: 'filter the matches',
  RETURN: 'choose what to show',
  SET: 'change properties or add labels',
  REMOVE: 'drop a property or label',
  DELETE: 'delete (fails if relationships remain)',
  'DETACH DELETE': 'delete with its relationships',
  CREATE: 'create nodes or relationships',
  DISTINCT: 'drop duplicate rows',
  AS: 'name a column',
  'ORDER BY': 'sort rows',
  SKIP: 'skip the first n rows',
  LIMIT: 'return at most n rows',
  'IS NULL': 'property is missing',
  'IS NOT NULL': 'property is present',
  IN: 'x IN [1, 2, 3]',
  CONTAINS: 'substring match',
  'STARTS WITH': 'prefix match',
  'ENDS WITH': 'suffix match',
}

const CLAUSES = /\b(MATCH|WHERE|RETURN|ORDER\s+BY|SKIP|LIMIT|CREATE|SET|REMOVE|DETACH\s+DELETE|DELETE|CALL|INDEX|EXPLAIN)\b/gi

/**
 * Suggestions at `cursor`: `{from, to, items}` where `from..to` is the span
 * an accepted item replaces, or null when there is nothing useful to offer.
 * `force` (Ctrl+Space) offers even with nothing typed.
 */
export function complete(text, cursor, schema, { force = false } = {}) {
  const before = text.slice(0, cursor)
  const code = blankStrings(before)
  if (code === null) return null // inside a string literal

  const word = /[A-Za-z_][A-Za-z0-9_]*$/.exec(code)?.[0] ?? ''
  const from = cursor - word.length
  // Extend over the rest of the word under the cursor, so accepting in the
  // middle of an identifier replaces it rather than splitting it.
  const to = cursor + (/^[A-Za-z0-9_]*/.exec(text.slice(cursor))?.[0].length ?? 0)
  const head = code.slice(0, code.length - word.length)
  const prev = head.replace(/\s+$/, '').slice(-1)
  const trigger = head.slice(-1)

  const items = suggest(head, prev, trigger, word, text, schema, force)
  if (!items) return null

  const ranked = rank(items, word)
  if (!ranked.length) return null
  return { from, to, items: ranked.slice(0, 60) }
}

function suggest(head, prev, trigger, word, text, schema, force) {
  const labels = schema?.labels ?? []
  const types = schema?.edge_types ?? []

  // Empty line: the menu of things you can do at all.
  if (!head.trim()) {
    if (!word && !force) return null
    return STARTERS.map((s) => ({ ...s, kind: 'snippet' }))
  }

  // CALL name — the procedures; inside its (...) — its arguments.
  if (/\bCALL\s+$/i.test(head)) {
    return PROCEDURES.map(([name, args, detail, snippet]) => ({
      label: name,
      insert: snippet,
      kind: 'proc',
      detail: `${detail} — ${args.join(', ')}`,
    }))
  }
  const call = inCall(head)
  if (call) {
    const proc = PROCEDURES.find((p) => p[0] === call.name.toLowerCase())
    // After `type:` or `dir:` offer values; after `(` or `,` offer names.
    const arg = /([A-Za-z_]\w*)\s*:\s*$/.exec(head)?.[1]?.toLowerCase()
    if (arg === 'type') return types.map((t) => ({ label: `"${t.name}"`, insert: `"${t.name}"`, kind: 'type', detail: `${t.count} relationships` }))
    if (arg === 'dir') return ['out', 'in', 'both'].map((d) => ({ label: `"${d}"`, insert: `"${d}"`, kind: 'value' }))
    if (prev === '(' || prev === ',') {
      const used = new Set([...call.body.matchAll(/([A-Za-z_]\w*)\s*:/g)].map((m) => m[1].toLowerCase()))
      const names = (proc?.[1] ?? COMMON).filter((a) => !used.has(a))
      return names.map((a) => ({ label: a, insert: `${a}: `, kind: 'arg', detail: ARG_HELP[a] }))
    }
    return null
  }

  const pat = openPattern(head)
  const labelList = () => labels.map((l) => ({ label: l.name, insert: l.name, kind: 'label', detail: `${l.count} nodes` }))
  const typeList = () => types.map((t) => ({ label: t.name, insert: t.name, kind: 'type', detail: `${t.count} relationships` }))

  // `(n:` → a label; `[r:`, `[:` or `[:A|` → a type. Inside `{...}` a colon
  // separates a key from its value, so nothing is offered there.
  if (trigger === ':' || (trigger === '|' && pat?.bracket === '[')) {
    if (pat && !pat.brace) return pat.bracket === '[' ? typeList() : labelList()
    if (/\bON\s*:$/i.test(head)) return labelList() // INDEX ON :Label
    if (/^(SET|REMOVE)$/.test(lastClause(head) ?? '') && /\w:$/.test(head)) return labelList() // SET n:Label
    return null
  }

  // `n.` → that variable's property keys.
  const dot = /([A-Za-z_]\w*)\.$/.exec(head)
  if (dot) return keysFor(dot[1], text, schema).map((k) => ({ label: k, insert: k, kind: 'key' }))

  // `INDEX ON :Label(` → that label's keys.
  const idx = /\bON\s*:\s*([A-Za-z_]\w*)\s*\(\s*$/i.exec(head)
  if (idx) return (schema?.node_keys?.[idx[1]] ?? []).map((k) => ({ label: k, insert: k, kind: 'key' }))

  // Inside a pattern's `{...}`: that label's keys, as `key: `.
  if (pat?.brace && (prev === '{' || prev === ',')) {
    const pool = pat.bracket === '[' ? schema?.edge_keys : schema?.node_keys
    const keys = pat.name ? pool?.[pat.name] : null
    return (keys ?? unionKeys(pool)).map((k) => ({ label: k, insert: `${k}: `, kind: 'key' }))
  }
  // Naming a variable inside a pattern: nothing sensible to suggest.
  if (pat && !pat.brace) return null

  if (!word && !force) return null

  // Otherwise: whatever fits the clause being written.
  const clause = lastClause(head)
  const vars = variables(text, head.length).map((v) => ({ label: v, insert: v, kind: 'var' }))
  const kw = (list) => list.map((k) => ({ label: k, insert: k + ' ', kind: 'keyword', detail: KW_HELP[k] }))
  const fns = (list) => list.map(([name, sig, detail]) => ({ label: sig, insert: `${name}($0)`, kind: 'fn', detail, match: name }))

  switch (clause) {
    case 'MATCH':
    case 'CREATE':
      return kw(AFTER_PATTERN)
    case 'WHERE':
      return [...vars, ...fns(FUNCTIONS), ...kw(IN_WHERE)]
    case 'RETURN':
      return [...vars, ...fns(AGGREGATES), ...fns(FUNCTIONS), ...kw(IN_RETURN)]
    case 'ORDER BY':
      return [...vars, ...kw(IN_ORDER)]
    case 'SKIP':
      return kw(['LIMIT'])
    case 'SET':
    case 'REMOVE':
    case 'DELETE':
    case 'DETACH DELETE':
      return [...vars, ...kw(IN_WRITE)]
    case 'EXPLAIN':
      return kw(['MATCH'])
    default:
      return null
  }
}

/**
 * Prefix matches first, then substring; keep the source order within each.
 * Substring matches wait for two characters, or a single letter would match
 * half the list. What is already typed in full is not offered back.
 */
function rank(items, word) {
  if (!word) return items
  const w = word.toLowerCase()
  const pre = []
  const sub = []
  for (const it of items) {
    const name = (it.match ?? it.label).toLowerCase()
    if (it.insert === word) continue
    if (name.startsWith(w)) pre.push(it)
    else if (w.length > 1 && name.includes(w)) sub.push(it)
  }
  return [...pre, ...sub]
}

/**
 * Blank out the contents of string literals so brackets and keywords inside
 * them are ignored. Null if the text ends inside an unterminated string.
 */
function blankStrings(s) {
  let out = ''
  let quote = null
  for (let i = 0; i < s.length; i++) {
    const c = s[i]
    if (quote) {
      if (c === '\\') {
        out += '  '
        i++
        continue
      }
      if (c === quote) {
        quote = null
        out += c
      } else out += ' '
    } else {
      if (c === '"' || c === "'" || c === '`') quote = c
      out += c
    }
  }
  return quote ? null : out
}

/**
 * The innermost unclosed `(` or `[` of a MATCH/CREATE pattern, if the cursor
 * is inside one: `{bracket, name, brace}` where `name` is the label or type
 * already written there and `brace` says we are inside its `{...}`.
 * Parentheses that belong to a function call or CALL are not patterns.
 */
function openPattern(head) {
  const stack = []
  for (let i = 0; i < head.length; i++) {
    const c = head[i]
    if (c === '(' || c === '[' || c === '{') stack.push(i)
    else if (c === ')' || c === ']' || c === '}') stack.pop()
  }
  if (!stack.length) return null
  let top = stack[stack.length - 1]
  let brace = false
  if (head[top] === '{') {
    brace = true
    top = stack[stack.length - 2]
    if (top === undefined) return null
  }
  const bracket = head[top]
  if (bracket === '[' && !/[-<]\s*$/.test(head.slice(0, top))) return null // a list literal
  if (bracket === '(') {
    // A pattern paren follows a clause keyword, a comma, or an arrow; a
    // function call's paren follows its name.
    const lead = head.slice(0, top)
    if (!/(\b(MATCH|CREATE)\s*|,\s*|[->]\s*)$/i.test(lead)) return null
    if (!/^(MATCH|CREATE)$/.test(lastClause(lead) ?? '')) return null
  }
  const inner = head.slice(top + 1, brace ? stack[stack.length - 1] : undefined)
  const name = /:\s*([A-Za-z_]\w*)/.exec(inner)?.[1] ?? null
  return { bracket, name, brace }
}

/** Inside `CALL name(...)`: `{name, body}`; else null. */
function inCall(head) {
  const m = /\bCALL\s+([A-Za-z_]\w*)\s*\(([^)]*)$/i.exec(head)
  return m ? { name: m[1], body: m[2] } : null
}

function lastClause(head) {
  let last = null
  for (const m of head.matchAll(CLAUSES)) last = m[1].toUpperCase().replace(/\s+/g, ' ')
  if (last === 'LIMIT') return null // nothing follows LIMIT
  return last
}

/** Variables bound anywhere in the query, excluding the word being typed. */
function variables(text, at) {
  const code = blankStrings(text) ?? text
  const seen = new Set()
  const re = /[([]\s*([A-Za-z_]\w*)|\bAS\s+([A-Za-z_]\w*)/gi
  for (const m of code.matchAll(re)) {
    const v = m[1] ?? m[2]
    // Skip the identifier the cursor is in, and function names like `count(`.
    if (m.index <= at && at <= m.index + m[0].length) continue
    if (m[1] && /\w/.test(code[m.index - 1] ?? '')) continue
    seen.add(v)
  }
  return [...seen]
}

/** Property keys for a variable, from the label or type it is bound with. */
function keysFor(v, text, schema) {
  const code = blankStrings(text) ?? text
  const esc = v.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
  const node = new RegExp(`\\(\\s*${esc}\\s*:\\s*([A-Za-z_]\\w*)`).exec(code)
  if (node) return schema?.node_keys?.[node[1]] ?? unionKeys(schema?.node_keys)
  const rel = new RegExp(`\\[\\s*${esc}\\s*:\\s*([A-Za-z_]\\w*)`).exec(code)
  if (rel) return schema?.edge_keys?.[rel[1]] ?? unionKeys(schema?.edge_keys)
  if (new RegExp(`\\[\\s*${esc}\\b`).test(code)) return unionKeys(schema?.edge_keys)
  return unionKeys(schema?.node_keys)
}

function unionKeys(pool) {
  const all = new Set()
  for (const keys of Object.values(pool ?? {})) for (const k of keys) all.add(k)
  return [...all].sort()
}

/**
 * Apply an item: returns `{text, selStart, selEnd}` with the replacement
 * spliced in and the selection set from its `${...}` or `$0` marker.
 */
export function applyItem(text, from, to, item) {
  let ins = item.insert
  let a = null
  let b = null
  const ph = /\$\{([^}]*)\}/.exec(ins)
  if (ph) {
    a = ph.index
    b = a + ph[1].length
    ins = ins.slice(0, ph.index) + ph[1] + ins.slice(ph.index + ph[0].length)
  }
  ins = ins.replace(/\$\{([^}]*)\}/g, '$1')
  const zero = ins.indexOf('$0')
  if (zero >= 0) {
    ins = ins.slice(0, zero) + ins.slice(zero + 2)
    if (a === null) a = b = zero
    else if (zero < a) {
      a -= 2
      b -= 2
    }
  }
  if (a === null) a = b = ins.length
  return { text: text.slice(0, from) + ins + text.slice(to), selStart: from + a, selEnd: from + b }
}

/** Starter templates, for the welcome screen. */
export const starters = STARTERS
