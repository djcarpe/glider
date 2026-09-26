import { Fragment, useEffect, useMemo, useRef, useState, useCallback } from 'react'
import {
  forceSimulation,
  forceLink,
  forceManyBody,
  forceCenter,
  forceCollide,
} from 'd3-force'
import { expandNode } from './api'
import { labelColor, captionOf, LABEL_PALETTE } from './entities'

// How many nodes we are willing to put in a force simulation before it stops
// being a picture and starts being a hairball. Neo4j Browser draws 300 by
// default for the same reason; past that the layout tells you nothing and the
// frame rate goes with it.
const RENDER_CAP = 300

const RADIUS = 19

/**
 * Force-directed graph over an SVG, with drag, zoom/pan, selection and
 * click-to-expand.
 *
 * The simulation mutates node objects in place (d3 convention), so the React
 * state here holds a *version counter* rather than the positions themselves —
 * re-rendering 300 nodes on every tick through React's reconciler is far more
 * expensive than letting d3 own the vectors and asking React to redraw.
 *
 * Two modes. On its own (a result frame) it owns selection, expansion and an
 * inspector. Controlled — the explorer — the parent owns all three through
 * `selectedId`/`onSelect`, `onExpand` and `inspector={false}`, and grows the
 * `graph` prop itself. Either way the graph the parent passes is the truth;
 * only positions live here.
 */
export default function GraphView({
  graph,
  onRunQuery,
  selectedId,
  onSelect,
  selectedEdgeId,
  onSelectEdge,
  onExpand,
  inspector = true,
  fill = false,
}) {
  const svgRef = useRef(null)
  const simRef = useRef(null)
  const nodesRef = useRef([])
  const linksRef = useRef([])
  const dragRef = useRef(null)
  const panRef = useRef(null)

  const [, bump] = useState(0)
  const [ownSelected, setOwnSelected] = useState(null)
  const [view, setView] = useState({ x: 0, y: 0, k: 1 })
  const [extra, setExtra] = useState({ nodes: [], edges: [] })
  const [expanding, setExpanding] = useState(false)

  const controlled = onSelect !== undefined
  const selected = controlled ? selectedId ?? null : ownSelected
  const select = useCallback(
    (node) => {
      if (controlled) onSelect(node)
      else setOwnSelected(node ? node.id : null)
    },
    [controlled, onSelect],
  )

  // Merge the query's graph with anything pulled in by expansion, de-duped by
  // id so expanding a node twice does not double it.
  const merged = useMemo(() => {
    const nodeMap = new Map()
    for (const n of graph.nodes || []) nodeMap.set(n.id, n)
    for (const n of extra.nodes) if (!nodeMap.has(n.id)) nodeMap.set(n.id, n)

    const edgeMap = new Map()
    for (const e of graph.edges || []) edgeMap.set(e.id, e)
    for (const e of extra.edges) if (!edgeMap.has(e.id)) edgeMap.set(e.id, e)

    let nodes = [...nodeMap.values()]
    const truncated = nodes.length > RENDER_CAP
    if (truncated) nodes = nodes.slice(0, RENDER_CAP)

    const keep = new Set(nodes.map((n) => n.id))
    const edges = [...edgeMap.values()].filter((e) => keep.has(e.from) && keep.has(e.to))
    return { nodes, edges, truncated, total: nodeMap.size }
  }, [graph, extra])

  const labels = useMemo(() => {
    const seen = new Map()
    for (const n of merged.nodes) {
      const l = n.labels?.[0] || '(none)'
      seen.set(l, (seen.get(l) || 0) + 1)
    }
    return [...seen.entries()].sort((a, b) => b[1] - a[1])
  }, [merged.nodes])

  // The simulation is rebuilt only when *membership* changes. An edit to a
  // property produces a new graph object but the same shape, and must not
  // re-heat the layout and send every node drifting.
  const shape = useMemo(
    () => merged.nodes.map((n) => n.id).join(',') + '|' + merged.edges.map((e) => e.id).join(','),
    [merged],
  )
  const mergedRef = useRef(merged)
  mergedRef.current = merged

  // ---- build / rebuild the simulation when the node or edge set changes
  useEffect(() => {
    const { nodes: mnodes, edges: medges } = mergedRef.current
    const box = svgRef.current?.getBoundingClientRect()
    const w = box?.width || 800
    const h = box?.height || 460

    // Carry positions across rebuilds so an expand animates outward from where
    // the node already is rather than teleporting the whole layout.
    const prev = new Map(nodesRef.current.map((n) => [n.id, n]))
    const nodes = mnodes.map((n) => {
      const p = prev.get(n.id)
      return p ? Object.assign(p, n) : { ...n, x: w / 2 + (Math.random() - 0.5) * 240, y: h / 2 + (Math.random() - 0.5) * 240 }
    })
    const byId = new Map(nodes.map((n) => [n.id, n]))
    const links = medges
      .map((e) => ({ ...e, source: byId.get(e.from), target: byId.get(e.to) }))
      .filter((l) => l.source && l.target)

    nodesRef.current = nodes
    linksRef.current = links

    simRef.current?.stop()
    const sim = forceSimulation(nodes)
      .force('link', forceLink(links).id((d) => d.id).distance(78).strength(0.35))
      .force('charge', forceManyBody().strength(-320).distanceMax(420))
      .force('center', forceCenter(w / 2, h / 2).strength(0.06))
      .force('collide', forceCollide(RADIUS + 5).strength(0.85))
      .alpha(0.9)
      .alphaDecay(0.035)
      .on('tick', () => bump((v) => v + 1))

    simRef.current = sim
    return () => sim.stop()
  }, [shape])

  // ---- same shape, new data: copy labels/props into the live objects
  useEffect(() => {
    const live = new Map(nodesRef.current.map((n) => [n.id, n]))
    for (const n of merged.nodes) {
      const l = live.get(n.id)
      if (l && l !== n) Object.assign(l, n)
    }
    const liveE = new Map(linksRef.current.map((l) => [l.id, l]))
    for (const e of merged.edges) {
      const l = liveE.get(e.id)
      if (l) {
        l.props = e.props
        l.type = e.type
      }
    }
    bump((v) => v + 1)
  }, [merged])

  // ---- pointer: drag a node, or pan the canvas
  const onPointerDown = useCallback(
    (e, node) => {
      e.stopPropagation()
      svgRef.current?.setPointerCapture?.(e.pointerId)
      if (node) {
        select(node)
        node.fx = node.x
        node.fy = node.y
        dragRef.current = { node, id: e.pointerId }
        simRef.current?.alphaTarget(0.25).restart()
      } else {
        panRef.current = { x: e.clientX, y: e.clientY, ox: view.x, oy: view.y, id: e.pointerId, moved: false }
      }
    },
    [view.x, view.y, select],
  )

  const onPointerMove = useCallback((e) => {
    const d = dragRef.current
    if (d) {
      const pt = toWorld(svgRef.current, e, viewRef.current)
      d.node.fx = pt.x
      d.node.fy = pt.y
      bump((v) => v + 1)
      return
    }
    const p = panRef.current
    if (p) {
      if (Math.abs(e.clientX - p.x) > 3 || Math.abs(e.clientY - p.y) > 3) p.moved = true
      setView((v) => ({ ...v, x: p.ox + (e.clientX - p.x), y: p.oy + (e.clientY - p.y) }))
    }
  }, [])

  const onPointerUp = useCallback(() => {
    const d = dragRef.current
    if (d) {
      // Release the pin so the node settles back into the layout.
      d.node.fx = null
      d.node.fy = null
      simRef.current?.alphaTarget(0)
      dragRef.current = null
    }
    const p = panRef.current
    if (p) {
      // A click on empty canvas — a pan that never moved — clears the selection.
      if (!p.moved) select(null)
      panRef.current = null
    }
  }, [select])

  // Keep a ref of the view for the coordinate transform, which runs inside a
  // pointer handler that must not re-subscribe on every pan frame.
  const viewRef = useRef(view)
  useEffect(() => {
    viewRef.current = view
  }, [view])

  const onWheel = useCallback((e) => {
    e.preventDefault()
    const box = svgRef.current.getBoundingClientRect()
    const mx = e.clientX - box.left
    const my = e.clientY - box.top
    setView((v) => {
      const k = Math.min(4, Math.max(0.15, v.k * (e.deltaY < 0 ? 1.12 : 1 / 1.12)))
      // Zoom about the cursor: keep the world point under the pointer fixed.
      return { k, x: mx - ((mx - v.x) / v.k) * k, y: my - ((my - v.y) / v.k) * k }
    })
  }, [])

  const doExpand = useCallback(
    async (id) => {
      if (onExpand) {
        onExpand(id)
        return
      }
      setExpanding(true)
      try {
        const r = await expandNode(id, 40)
        setExtra((prev) => ({
          nodes: [...prev.nodes, ...(r.graph?.nodes || [])],
          edges: [...prev.edges, ...(r.graph?.edges || [])],
        }))
      } catch (err) {
        console.error('expand failed', err)
      } finally {
        setExpanding(false)
      }
    },
    [onExpand],
  )

  const fit = useCallback(() => setView({ x: 0, y: 0, k: 1 }), [])

  const sel = merged.nodes.find((n) => n.id === selected)
  const selNeighbours = selected == null ? new Set() : new Set(
    linksRef.current.filter((l) => l.from === selected || l.to === selected).map((l) => l.id),
  )

  if (!merged.nodes.length) {
    return (
      <div className="msg-box">
        No nodes in this result. Return a node or relationship — e.g.{' '}
        <code>MATCH (n)-[r]-&gt;(m) RETURN n, r, m LIMIT 25</code> — to draw a graph.
      </div>
    )
  }

  return (
    <div className={`graph-wrap${fill ? ' fill' : ''}`}>
      <svg
        ref={svgRef}
        className={dragRef.current || panRef.current ? 'dragging' : ''}
        onPointerDown={(e) => onPointerDown(e, null)}
        onPointerMove={onPointerMove}
        onPointerUp={onPointerUp}
        onPointerLeave={onPointerUp}
        onWheel={onWheel}
      >
        <defs>
          <marker
            id="arrow"
            viewBox="0 -5 10 10"
            refX={RADIUS + 9}
            refY="0"
            markerWidth="5.5"
            markerHeight="5.5"
            orient="auto"
          >
            <path d="M0,-4L9,0L0,4" fill="#43505f" />
          </marker>
        </defs>

        <g transform={`translate(${view.x},${view.y}) scale(${view.k})`}>
          {linksRef.current.map((l) => {
            if (!l.source || !l.target) return null
            const hl = selNeighbours.has(l.id)
            const isSel = l.id === selectedEdgeId
            const mx = (l.source.x + l.target.x) / 2
            const my = (l.source.y + l.target.y) / 2
            return (
              <g key={l.id} className={onSelectEdge ? 'g-link' : ''}>
                {onSelectEdge && (
                  // A wide, invisible twin of the line so a 1px edge is clickable.
                  <line
                    className="g-edge-hit"
                    x1={l.source.x}
                    y1={l.source.y}
                    x2={l.target.x}
                    y2={l.target.y}
                    onPointerDown={(e) => {
                      e.stopPropagation()
                      onSelectEdge(l)
                    }}
                  />
                )}
                <line
                  className={`g-edge${hl ? ' hl' : ''}${isSel ? ' sel' : ''}`}
                  x1={l.source.x}
                  y1={l.source.y}
                  x2={l.target.x}
                  y2={l.target.y}
                  markerEnd="url(#arrow)"
                />
                {view.k > 0.75 && (
                  <text className="g-elabel" x={mx} y={my - 3} textAnchor="middle">
                    {l.type}
                  </text>
                )}
              </g>
            )
          })}

          {nodesRef.current.map((n) => (
            <g
              key={n.id}
              className={`g-node${n.id === selected ? ' sel' : ''}`}
              transform={`translate(${n.x},${n.y})`}
              onPointerDown={(e) => onPointerDown(e, n)}
              onDoubleClick={(e) => {
                e.stopPropagation()
                doExpand(n.id)
              }}
            >
              <circle r={RADIUS} fill={labelColor(n.labels?.[0])} />
              <text textAnchor="middle" dy="3.4">
                {truncate(captionOf(n), 10)}
              </text>
            </g>
          ))}
        </g>
      </svg>

      <div className="g-tools">
        <button title="Zoom in" onClick={() => setView((v) => ({ ...v, k: Math.min(4, v.k * 1.25) }))}>+</button>
        <button title="Zoom out" onClick={() => setView((v) => ({ ...v, k: Math.max(0.15, v.k / 1.25) }))}>−</button>
        <button title="Reset view" onClick={fit}>⌂</button>
        <button title="Re-run layout" onClick={() => simRef.current?.alpha(0.9).restart()}>↻</button>
      </div>

      {labels.length > 0 && (
        <div className="g-legend">
          {labels.map(([l, n]) => (
            <span className="chip" key={l} style={{ background: labelColor(l), color: '#0b0f14' }}>
              {l} <span className="n" style={{ color: '#0b0f1499' }}>{n}</span>
            </span>
          ))}
        </div>
      )}

      <div className="g-note">
        {merged.nodes.length} nodes · {merged.edges.length} rels
        {merged.truncated && ` · showing first ${RENDER_CAP} of ${merged.total}`}
        {expanding && ' · expanding…'}
      </div>

      {inspector && sel && (
        <div className="inspector">
          <h4>
            <span
              style={{
                width: 9,
                height: 9,
                borderRadius: '50%',
                background: labelColor(sel.labels?.[0]),
                display: 'inline-block',
              }}
            />
            {sel.labels?.join(':') || 'Node'}{' '}
            <span style={{ color: 'var(--text-faint)', fontFamily: 'var(--mono)' }}>#{sel.id}</span>
          </h4>
          <dl>
            {Object.entries(sel.props || {}).map(([k, v]) => (
              <Fragment key={k}>
                <dt>{k}</dt>
                <dd>{formatProp(v)}</dd>
              </Fragment>
            ))}
          </dl>
          <button className="expand" onClick={() => doExpand(sel.id)} disabled={expanding}>
            {expanding ? 'expanding…' : 'Expand neighbours'}
          </button>
          {onRunQuery && (
            <button
              className="expand"
              style={{ marginTop: 5 }}
              onClick={() => onRunQuery(`MATCH (n)-[r]-(m) WHERE id(n) = ${sel.id} RETURN n, r, m LIMIT 50`)}
            >
              Query from here
            </button>
          )}
        </div>
      )}
    </div>
  )
}

// Convert client coordinates to simulation (world) coordinates, undoing the
// current pan/zoom transform.
function toWorld(svg, e, view) {
  const box = svg.getBoundingClientRect()
  return {
    x: (e.clientX - box.left - view.x) / view.k,
    y: (e.clientY - box.top - view.y) / view.k,
  }
}

function truncate(s, n) {
  s = String(s ?? '')
  return s.length > n ? s.slice(0, n - 1) + '…' : s
}

function formatProp(v) {
  if (v === null || v === undefined) return 'null'
  if (Array.isArray(v)) return `[${v.join(', ')}]`
  return String(v)
}

export { LABEL_PALETTE, RENDER_CAP }
