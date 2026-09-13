import { useState } from 'react'
import GraphView from './GraphView'
import { labelColor, captionOf, isEntity } from './entities'

/**
 * One result frame. Frames stack newest-first and are independently
 * collapsible, pinnable and closable — the interaction model Neo4j Browser
 * uses, which suits an exploratory session where you want the last few results
 * visible side by side rather than a single replaced pane.
 */
export default function Frame({ frame, onClose, onRunQuery }) {
  const hasGraph = (frame.result?.graph?.nodes?.length || 0) > 0
  const hasRows = (frame.result?.rows?.length || 0) > 0
  const [tab, setTab] = useState(hasGraph ? 'graph' : 'table')
  const [collapsed, setCollapsed] = useState(false)

  // A frame created before the response landed picks its tab once the result
  // arrives; without this the first render locks it to 'table'.
  const [settled, setSettled] = useState(false)
  if (frame.result && !settled) {
    setSettled(true)
    setTab(hasGraph ? 'graph' : 'table')
  }

  return (
    <div className="frame">
      <div className="frame-head">
        <div className={`frame-q${frame.error ? ' err' : ''}`} title={frame.query}>
          {frame.query}
        </div>

        {frame.result && !frame.error && (
          <div className="tabs">
            {hasGraph && (
              <button className={`tab${tab === 'graph' ? ' on' : ''}`} onClick={() => setTab('graph')}>
                Graph
              </button>
            )}
            <button className={`tab${tab === 'table' ? ' on' : ''}`} onClick={() => setTab('table')}>
              Table
            </button>
            <button className={`tab${tab === 'json' ? ' on' : ''}`} onClick={() => setTab('json')}>
              JSON
            </button>
          </div>
        )}

        <button className="icon-btn" title={collapsed ? 'Expand' : 'Collapse'} onClick={() => setCollapsed((c) => !c)}>
          {collapsed ? '▸' : '▾'}
        </button>
        <button className="icon-btn" title="Re-run" onClick={() => onRunQuery(frame.query)}>↻</button>
        <button className="icon-btn" title="Close" onClick={() => onClose(frame.id)}>✕</button>
      </div>

      <div className={`frame-body${collapsed ? ' collapsed' : ''}`}>
        {frame.pending && (
          <div className="msg-box" style={{ display: 'flex', alignItems: 'center', gap: 9 }}>
            <span className="spinner" /> running…
          </div>
        )}

        {frame.error && <div className="err-box">{frame.error}</div>}

        {frame.result && !frame.error && (
          <>
            {tab === 'graph' && hasGraph && (
              <GraphView graph={frame.result.graph} onRunQuery={onRunQuery} />
            )}
            {tab === 'table' &&
              (hasRows ? (
                <TableView result={frame.result} />
              ) : (
                <div className="msg-box">{frame.result.message || 'No rows.'}</div>
              ))}
            {tab === 'json' && (
              <pre className="json-box">{JSON.stringify(frame.result, null, 2)}</pre>
            )}
          </>
        )}
      </div>

      {frame.result && !frame.error && !collapsed && (
        <div className="frame-foot">
          <span>{frame.result.rows?.length ?? 0} rows</span>
          {hasGraph && (
            <span>
              {frame.result.graph.nodes.length} nodes · {frame.result.graph.edges.length} rels
            </span>
          )}
          {frame.result.touched > 0 && <span>{frame.result.touched} touched</span>}
          <span className="spacer" />
          <span>{fmtMs(frame.result.ms)}</span>
        </div>
      )}
    </div>
  )
}

function fmtMs(ms) {
  if (ms == null) return ''
  if (ms < 1) return `${(ms * 1000).toFixed(0)} µs`
  if (ms < 1000) return `${ms.toFixed(1)} ms`
  return `${(ms / 1000).toFixed(2)} s`
}

// ------------------------------------------------------------------- table

function TableView({ result }) {
  const { columns, rows } = result
  // Render a bounded window. A RETURN with a million rows should not lock the
  // tab up; the JSON view and an explicit LIMIT are the ways to see more.
  const cap = 1000
  const shown = rows.slice(0, cap)

  return (
    <>
      <div className="table-scroll">
        <table className="grid">
          <thead>
            <tr>
              <th className="idx" />
              {columns.map((c) => (
                <th key={c}>{c}</th>
              ))}
            </tr>
          </thead>
          <tbody>
            {shown.map((row, i) => (
              <tr key={i}>
                <td className="idx">{i + 1}</td>
                {row.map((cell, j) => (
                  <td key={j} title={plain(cell)}>
                    <Cell value={cell} />
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {rows.length > cap && (
        <div className="msg-box" style={{ paddingTop: 8, paddingBottom: 8 }}>
          showing first {cap} of {rows.length} rows
        </div>
      )}
    </>
  )
}

function Cell({ value }) {
  if (value === null || value === undefined) return <span className="v-null">null</span>

  if (isEntity(value)) {
    if (value._e === 'node') {
      return (
        <span className="ent" style={{ background: labelColor(value.labels?.[0]), color: '#0b0f14' }}>
          <span className="k">{value.labels?.join(':') || 'Node'}</span>
          <span className="p" style={{ color: '#0b0f14bb' }}>{captionOf(value)}</span>
        </span>
      )
    }
    return (
      <span className="ent rel">
        <span className="k" style={{ color: 'var(--text-dim)' }}>{value.type}</span>
        <span className="p">
          {value.from}→{value.to}
        </span>
      </span>
    )
  }

  if (typeof value === 'number') return <span className="v-num">{value}</span>
  if (typeof value === 'boolean') return <span className="v-bool">{String(value)}</span>
  if (Array.isArray(value)) return <span className="v-str">[{value.map(plain).join(', ')}]</span>
  return <span className="v-str">{String(value)}</span>
}

function plain(v) {
  if (v === null || v === undefined) return 'null'
  if (isEntity(v)) return v._e === 'node' ? `(${v.labels?.join(':')} ${captionOf(v)})` : `[:${v.type}]`
  if (typeof v === 'object') return JSON.stringify(v)
  return String(v)
}
