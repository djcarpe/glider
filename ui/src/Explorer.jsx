import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import GraphView, { RENDER_CAP } from './GraphView'
import Inspector from './Inspector'
import { expandNode, fetchEdges, fetchNodes } from './api'
import { labelColor, captionOf } from './entities'
import * as edit from './edit'

const PAGE = 50

/**
 * Browse a graph without writing queries.
 *
 * Three columns: a searchable, infinitely scrolling list of what exists; a
 * canvas you build up by clicking and expanding; and an inspector that edits
 * whatever is selected. The list is the only thing that touches the whole
 * graph, and it does so a page at a time through an id cursor — so a graph
 * with millions of nodes costs the same to open as one with ten. The canvas
 * never holds more than you have asked for.
 */
export default function Explorer({ schema, onSchemaChange }) {
  // ---- what the list shows
  const [kind, setKind] = useState('nodes') // 'nodes' | 'rels'
  const [q, setQ] = useState('')
  const [dq, setDq] = useState('') // debounced
  const [filter, setFilter] = useState(null) // a label, or a rel type
  const [list, setList] = useState({ items: [], next: 0, total: null, loading: false, error: null })

  // ---- what the canvas holds, keyed by id
  const [canvas, setCanvas] = useState({ nodes: {}, edges: {} })
  const [sel, setSel] = useState(null) // {kind:'node'|'rel', id} | {kind:'new'} | null
  const [linking, setLinking] = useState(null) // {from, to} while making a relationship
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState(null)

  // Nodes seen anywhere — list pages, edge endpoints, expansions — so a
  // relationship row can name its ends without another request.
  const cache = useRef(new Map())
  const remember = useCallback((nodes) => {
    for (const n of nodes || []) cache.current.set(n.id, n)
  }, [])
  const nodeById = useCallback((id) => canvas.nodes[id] || cache.current.get(id) || null, [canvas.nodes])

  // ---- search: debounce keystrokes, reset the list on any change of scope
  useEffect(() => {
    const t = setTimeout(() => setDq(q.trim()), 200)
    return () => clearTimeout(t)
  }, [q])

  const gen = useRef(0)
  useEffect(() => {
    gen.current++
    setList({ items: [], next: 0, total: null, loading: false, error: null })
    // Back to the top, or a list left scrolled to its end would keep the
    // sentinel in view and pull page after page of the new scope.
    if (listRef.current) listRef.current.scrollTop = 0
  }, [kind, dq, filter])

  const loadMore = useCallback(async () => {
    if (list.loading || list.next === null) return
    const g = gen.current
    setList((l) => ({ ...l, loading: true }))
    try {
      const page =
        kind === 'nodes'
          ? await fetchNodes({ label: filter, q: dq, from: list.next, limit: PAGE })
          : await fetchEdges({ type: filter, q: dq, from: list.next, limit: PAGE })
      if (g !== gen.current) return // scope changed while in flight
      remember(page.nodes)
      const items = kind === 'nodes' ? page.nodes : page.edges
      setList((l) => ({ items: [...l.items, ...items], next: page.next, total: page.total, loading: false, error: null }))
    } catch (e) {
      if (g === gen.current) setList((l) => ({ ...l, loading: false, error: String(e.message || e) }))
    }
  }, [kind, dq, filter, list.loading, list.next, remember])

  // Lazy loading: a sentinel at the foot of the list asks for the next page
  // whenever it scrolls into view. Re-observing after every page means a list
  // too short to fill the panel keeps loading until it does.
  const listRef = useRef(null)
  const sentinelRef = useRef(null)
  useEffect(() => {
    const el = sentinelRef.current
    if (!el || typeof IntersectionObserver === 'undefined') return
    const io = new IntersectionObserver(
      (entries) => {
        if (entries.some((e) => e.isIntersecting)) loadMore()
      },
      { root: listRef.current, rootMargin: '200px' },
    )
    io.observe(el)
    return () => io.disconnect()
  }, [loadMore])

  // ---- canvas bookkeeping
  const addToCanvas = useCallback(
    (nodes = [], edges = []) => {
      remember(nodes)
      setCanvas((c) => {
        const n = { ...c.nodes }
        const e = { ...c.edges }
        for (const x of nodes) n[x.id] = x
        for (const x of edges) e[x.id] = x
        return { nodes: n, edges: e }
      })
    },
    [remember],
  )

  /** A fresh copy of an entity from the engine: swap it in everywhere. */
  const refresh = useCallback(
    (ent) => {
      if (!ent) return
      if (ent._e === 'node') remember([ent])
      setCanvas((c) => {
        const bucket = ent._e === 'node' ? 'nodes' : 'edges'
        if (!c[bucket][ent.id]) return c
        return { ...c, [bucket]: { ...c[bucket], [ent.id]: ent } }
      })
      setList((l) => ({ ...l, items: l.items.map((it) => (it._e === ent._e && it.id === ent.id ? ent : it)) }))
    },
    [remember],
  )

  const dropNode = useCallback((id) => {
    setCanvas((c) => {
      const nodes = { ...c.nodes }
      delete nodes[id]
      const edges = {}
      for (const e of Object.values(c.edges)) if (e.from !== id && e.to !== id) edges[e.id] = e
      return { nodes, edges }
    })
  }, [])

  const dropEdge = useCallback((id) => {
    setCanvas((c) => {
      const edges = { ...c.edges }
      delete edges[id]
      return { ...c, edges }
    })
  }, [])

  const graph = useMemo(() => ({ nodes: Object.values(canvas.nodes), edges: Object.values(canvas.edges) }), [canvas])

  // ---- selection
  const selectNode = useCallback(
    async (id) => {
      let n = nodeById(id)
      if (!n) n = await edit.fetchNode(id)
      if (!n) return
      addToCanvas([n])
      setSel({ kind: 'node', id })
    },
    [nodeById, addToCanvas],
  )

  const onCanvasSelect = useCallback(
    (node) => {
      if (linking) {
        if (node) setLinking((l) => ({ ...l, to: node }))
        return
      }
      setSel(node ? { kind: 'node', id: node.id } : null)
    },
    [linking],
  )

  const onListClick = useCallback(
    (item) => {
      if (item._e === 'node') {
        if (linking) {
          addToCanvas([item])
          setLinking((l) => ({ ...l, to: item }))
          return
        }
        addToCanvas([item])
        setSel({ kind: 'node', id: item.id })
      } else {
        const ends = [nodeById(item.from), nodeById(item.to)].filter(Boolean)
        addToCanvas(ends, [item])
        setSel({ kind: 'rel', id: item.id })
      }
    },
    [linking, addToCanvas, nodeById],
  )

  const target = useMemo(() => {
    if (!sel) return null
    if (sel.kind === 'new') return sel
    if (sel.kind === 'node') {
      const node = nodeById(sel.id)
      return node ? { kind: 'node', node } : null
    }
    const edge = canvas.edges[sel.id] || list.items.find((it) => it._e === 'rel' && it.id === sel.id)
    return edge ? { kind: 'rel', edge } : null
  }, [sel, nodeById, canvas.edges, list.items])

  // ---- writes: every edit runs under one guard so the panel shows one error
  const run = useCallback(async (fn) => {
    setBusy(true)
    setError(null)
    try {
      return await fn()
    } catch (e) {
      setError(String(e.message || e))
      return undefined
    } finally {
      setBusy(false)
    }
  }, [])

  const expand = useCallback(
    (id) =>
      run(async () => {
        const r = await expandNode(id, 60)
        addToCanvas(r.graph?.nodes || [], r.graph?.edges || [])
      }),
    [run, addToCanvas],
  )

  /** Re-read the nodes touching an edge: their degrees just changed. */
  const refreshEnds = useCallback(
    async (edge) => {
      for (const id of [edge.from, edge.to]) refresh(await edit.fetchNode(id))
    },
    [refresh],
  )

  const actions = useMemo(
    () => ({
      newNode: () => {
        setLinking(null)
        setSel({ kind: 'new' })
      },
      cancelNew: () => setSel(null),
      selectNode,
      expand,
      remove: (ent) => {
        if (ent._e === 'node') dropNode(ent.id)
        else dropEdge(ent.id)
        setSel(null)
      },
      setProp: (ent, key, value) => run(async () => refresh(await edit.setProp(ent, key, value))),
      removeProp: (ent, key) => run(async () => refresh(await edit.removeProp(ent, key))),
      addLabel: (node, label) =>
        run(async () => {
          refresh(await edit.addLabel(node, label))
          onSchemaChange?.()
        }),
      removeLabel: (node, label) =>
        run(async () => {
          refresh(await edit.removeLabel(node, label))
          onSchemaChange?.()
        }),
      del: (ent) =>
        run(async () => {
          if (ent._e === 'node') {
            // Neighbours on the canvas lose a relationship each; re-read them.
            const touched = Object.values(canvas.edges)
              .filter((e) => e.from === ent.id || e.to === ent.id)
              .flatMap((e) => [e.from, e.to])
              .filter((id) => id !== ent.id)
            await edit.deleteNode(ent.id)
            dropNode(ent.id)
            cache.current.delete(ent.id)
            for (const id of new Set(touched)) refresh(await edit.fetchNode(id))
          } else {
            await edit.deleteEdge(ent)
            dropEdge(ent.id)
            await refreshEnds(ent)
          }
          setList((l) => ({ ...l, items: l.items.filter((it) => !(it._e === ent._e && it.id === ent.id)) }))
          setSel(null)
          onSchemaChange?.()
        }),
      createNode: (labels, props) =>
        run(async () => {
          const node = await edit.createNode(labels, props)
          addToCanvas([node])
          if (kind === 'nodes') setList((l) => ({ ...l, items: [node, ...l.items] }))
          setSel({ kind: 'node', id: node.id })
          onSchemaChange?.()
        }),
      startLink: (node) => setLinking({ from: node, to: null }),
      cancelLink: () => setLinking(null),
      createEdge: (type, dir) =>
        run(async () => {
          const [a, b] = dir === 'out' ? [linking.from, linking.to] : [linking.to, linking.from]
          const e = await edit.createEdge(a.id, b.id, type, {})
          if (!e) throw new Error('the relationship was created but could not be read back')
          addToCanvas([a, b], [e])
          await refreshEnds(e)
          if (kind === 'rels') setList((l) => ({ ...l, items: [e, ...l.items] }))
          setLinking(null)
          setSel({ kind: 'rel', id: e.id })
          onSchemaChange?.()
        }),
    }),
    [run, refresh, refreshEnds, dropNode, dropEdge, addToCanvas, selectNode, expand, canvas.edges, kind, linking, onSchemaChange],
  )

  // ---- render
  const chips = kind === 'nodes' ? schema?.labels : schema?.edge_types
  const total = kind === 'nodes' ? schema?.nodes : schema?.edges
  const canvasN = graph.nodes.length

  return (
    <div className="explore">
      <datalist id="glider-labels">
        {schema?.labels?.map((l) => (
          <option key={l.name} value={l.name} />
        ))}
      </datalist>
      <datalist id="glider-types">
        {schema?.edge_types?.map((t) => (
          <option key={t.name} value={t.name} />
        ))}
      </datalist>

      <aside className="ex-side">
        <SearchBox
          q={q}
          setQ={setQ}
          kind={kind}
          schema={schema}
          onScope={(k, name) => {
            setKind(k)
            setFilter(name)
            setQ('')
          }}
        />
        <div className="ex-kind">
          <button className={`tab${kind === 'nodes' ? ' on' : ''}`} onClick={() => { setKind('nodes'); setFilter(null) }}>
            Nodes {schema && <span className="n">{fmt(schema.nodes)}</span>}
          </button>
          <button className={`tab${kind === 'rels' ? ' on' : ''}`} onClick={() => { setKind('rels'); setFilter(null) }}>
            Relationships {schema && <span className="n">{fmt(schema.edges)}</span>}
          </button>
        </div>
        {chips?.length > 0 && (
          <div className="chips ex-filters">
            <button className={`chip${filter === null ? ' on' : ''}`} onClick={() => setFilter(null)}>all</button>
            {chips.map((c) => (
              <button
                key={c.name}
                className={`chip${kind === 'rels' ? ' rel' : ''}${filter === c.name ? ' on' : ''}`}
                style={kind === 'nodes' && filter === c.name ? { background: labelColor(c.name), color: '#0b0f14' } : undefined}
                onClick={() => setFilter(filter === c.name ? null : c.name)}
              >
                {kind === 'nodes' && <span className="ins-dot" style={{ background: labelColor(c.name) }} />}
                {c.name} <span className="n">{fmt(c.count)}</span>
              </button>
            ))}
          </div>
        )}

        <div className="ex-list" ref={listRef}>
          {list.items.map((it) =>
            it._e === 'node' ? (
              <NodeItem key={`n${it.id}`} node={it} onCanvas={!!canvas.nodes[it.id]} selected={sel?.kind === 'node' && sel.id === it.id} onClick={() => onListClick(it)} />
            ) : (
              <RelItem key={`r${it.id}`} edge={it} nodeById={nodeById} onCanvas={!!canvas.edges[it.id]} selected={sel?.kind === 'rel' && sel.id === it.id} onClick={() => onListClick(it)} />
            ),
          )}
          <div ref={sentinelRef} className="ex-sentinel" />
          <div className="ex-status">
            {list.error ? (
              <span className="err">{list.error}</span>
            ) : list.loading ? (
              <>
                <span className="spinner" /> loading…
              </>
            ) : list.next === null ? (
              list.items.length ? `${list.items.length} shown · end of list` : dq || filter ? 'no matches' : `nothing here yet`
            ) : (
              `${list.items.length} of ${dq ? '…' : fmt(list.total ?? total)}`
            )}
          </div>
        </div>

        <div className="ex-side-foot">
          <button className="btn primary" onClick={actions.newNode} disabled={busy}>+ New node</button>
          {linking && <span className="ex-linking">pick the other end…</span>}
        </div>
      </aside>

      <div className={`ex-canvas${linking ? ' linking' : ''}`}>
        {canvasN ? (
          <GraphView
            graph={graph}
            selectedId={sel?.kind === 'node' ? sel.id : null}
            onSelect={onCanvasSelect}
            selectedEdgeId={sel?.kind === 'rel' ? sel.id : null}
            onSelectEdge={(l) => !linking && setSel({ kind: 'rel', id: l.id })}
            onExpand={expand}
            inspector={false}
            fill
          />
        ) : (
          <div className="ex-empty">
            <h2>Nothing on the canvas</h2>
            <p>Search on the left and click a result to add it. Double-click a node to pull in its neighbours.</p>
            <p>Or <button className="link" onClick={actions.newNode}>create a node</button> to start a graph from scratch.</p>
          </div>
        )}
        {canvasN > 0 && (
          <div className="ex-canvas-bar">
            <span>{canvasN} node{canvasN === 1 ? '' : 's'} · {graph.edges.length} rel{graph.edges.length === 1 ? '' : 's'} on canvas</span>
            {canvasN > RENDER_CAP && <span className="warn">drawing the first {RENDER_CAP}</span>}
            <button className="btn small" onClick={() => { setCanvas({ nodes: {}, edges: {} }); setSel(null); setLinking(null) }}>
              Clear
            </button>
          </div>
        )}
      </div>

      <aside className="ex-inspector" key={selKey(sel)}>
        {error && (
          <div className="ex-error">
            {error}
            <button className="icon-btn" onClick={() => setError(null)}>✕</button>
          </div>
        )}
        <Inspector target={target} schema={schema} nodeById={nodeById} busy={busy} linking={linking} actions={actions} />
      </aside>
    </div>
  )
}

function selKey(sel) {
  return sel ? `${sel.kind}:${sel.id ?? ''}` : 'none'
}

function NodeItem({ node, onCanvas, selected, onClick }) {
  const props = Object.entries(node.props || {})
  return (
    <button className={`ex-item${selected ? ' sel' : ''}${onCanvas ? ' on-canvas' : ''}`} onClick={onClick} title={`#${node.id}`}>
      <span className="ins-dot" style={{ background: labelColor(node.labels?.[0]) }} />
      <span className="ex-item-main">
        <span className="ex-item-title">{captionOf(node)}</span>
        <span className="ex-item-sub">
          {node.labels?.length ? node.labels.join(' · ') : 'no label'}
          {props.length > 0 && ` · ${props.length} prop${props.length === 1 ? '' : 's'}`}
          {node.degree > 0 && ` · ${node.degree} rel${node.degree === 1 ? '' : 's'}`}
        </span>
      </span>
      {onCanvas && <span className="ex-item-mark" title="on canvas">●</span>}
    </button>
  )
}

function RelItem({ edge, nodeById, onCanvas, selected, onClick }) {
  const a = nodeById(edge.from)
  const b = nodeById(edge.to)
  return (
    <button className={`ex-item${selected ? ' sel' : ''}${onCanvas ? ' on-canvas' : ''}`} onClick={onClick} title={`#${edge.id}`}>
      <span className="ex-item-main">
        <span className="ex-item-title mono">{edge.type}</span>
        <span className="ex-item-sub">
          <span className="ins-dot" style={{ background: labelColor(a?.labels?.[0]) }} /> {a ? captionOf(a) : `#${edge.from}`}
          {' → '}
          <span className="ins-dot" style={{ background: labelColor(b?.labels?.[0]) }} /> {b ? captionOf(b) : `#${edge.to}`}
        </span>
      </span>
      {onCanvas && <span className="ex-item-mark" title="on canvas">●</span>}
    </button>
  )
}

/**
 * The search input, with type-ahead for scope: typing part of a label or
 * relationship type offers to filter to it, which is both faster than a
 * text match and how a newcomer learns the filters exist. Plain Enter still
 * just searches.
 */
function SearchBox({ q, setQ, kind, schema, onScope }) {
  const [focus, setFocus] = useState(false)
  const [active, setActive] = useState(-1)
  const [shut, setShut] = useState(false)

  const matches = useMemo(() => {
    const w = q.trim().toLowerCase()
    if (!w) return []
    const hit = (name) => name.toLowerCase().includes(w)
    const labels = (schema?.labels ?? []).filter((l) => hit(l.name)).map((l) => ({ kind: 'nodes', name: l.name, count: l.count }))
    const types = (schema?.edge_types ?? []).filter((t) => hit(t.name)).map((t) => ({ kind: 'rels', name: t.name, count: t.count }))
    // The current list's kind first; prefix hits before substring ones.
    const all = kind === 'nodes' ? [...labels, ...types] : [...types, ...labels]
    const pre = (m) => (m.name.toLowerCase().startsWith(w) ? 0 : 1)
    return all.sort((a, b) => pre(a) - pre(b)).slice(0, 8)
  }, [q, schema, kind])

  useEffect(() => {
    setActive(-1)
    setShut(false)
  }, [q])

  const open = focus && !shut && matches.length > 0
  const pick = (m) => {
    onScope(m.kind, m.name)
    setShut(true)
  }

  return (
    <div className="ex-search">
      <input
        className="ins-input"
        type="search"
        placeholder={kind === 'nodes' ? 'Search nodes — any label, property or id' : 'Search relationships — type, property or id'}
        value={q}
        autoFocus
        role="combobox"
        aria-expanded={open}
        aria-autocomplete="list"
        onChange={(e) => setQ(e.target.value)}
        onFocus={() => setFocus(true)}
        onBlur={() => setFocus(false)}
        onKeyDown={(e) => {
          if (!open) return
          if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
            e.preventDefault()
            const n = matches.length
            // -1 is "just search the text", above the first suggestion.
            setActive((a) => ((a + 1 + (e.key === 'ArrowDown' ? 1 : -1) + n + 1) % (n + 1)) - 1)
          } else if (e.key === 'Enter') {
            e.preventDefault()
            if (active >= 0) pick(matches[active])
            else setShut(true)
          } else if (e.key === 'Escape') {
            setShut(true)
          }
        }}
      />
      {open && (
        <div className="sug ex-sug" role="listbox">
          <div className="sug-list">
            <div className={`sug-item${active === -1 ? ' on' : ''}`} onMouseDown={(e) => { e.preventDefault(); setShut(true) }}>
              <span className="sug-kind">⌕</span>
              <span className="sug-label">search every property for “{q.trim()}”</span>
            </div>
            {matches.map((m, i) => (
              <div
                key={`${m.kind}:${m.name}`}
                role="option"
                aria-selected={i === active}
                className={`sug-item${i === active ? ' on' : ''}`}
                onMouseDown={(e) => {
                  e.preventDefault()
                  pick(m)
                }}
                onMouseEnter={() => setActive(i)}
              >
                {m.kind === 'nodes' ? (
                  <span className="sug-kind"><span className="ins-dot" style={{ background: labelColor(m.name) }} /></span>
                ) : (
                  <span className="sug-kind k-type">→</span>
                )}
                <span className="sug-label">
                  {m.kind === 'nodes' ? `only :${m.name} nodes` : `only ${m.name} relationships`}
                </span>
                <span className="sug-count">{fmt(m.count)}</span>
              </div>
            ))}
          </div>
          <div className="sug-detail sug-keys">
            <kbd>↓</kbd> pick a filter · <kbd>Enter</kbd> apply · <kbd>Esc</kbd> close
          </div>
        </div>
      )}
    </div>
  )
}

function fmt(n) {
  if (n == null) return ''
  return Number(n).toLocaleString()
}
