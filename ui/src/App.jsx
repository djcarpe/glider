import { useCallback, useEffect, useRef, useState } from 'react'
import Frame from './Frame'
import { fetchSchema, runQuery, transportKind, transportLabel } from './api'
import { labelColor } from './entities'

const EXAMPLES = [
  'MATCH (n)-[r]->(m) RETURN n, r, m LIMIT 25',
  'MATCH (n) RETURN labels(n) AS label, count(n) AS n ORDER BY n DESC',
  'CALL pagerank(iterations: 20, top: 10)',
  'SCHEMA',
  'STATS',
]

const HISTORY_KEY = 'glider.history'
const MAX_HISTORY = 40

export default function App() {
  const [text, setText] = useState('')
  const [frames, setFrames] = useState([])
  const [schema, setSchema] = useState(null)
  const [online, setOnline] = useState(null)
  const [history, setHistory] = useState(() => load(HISTORY_KEY, []))
  const [sidebar, setSidebar] = useState(true)
  const taRef = useRef(null)
  const nextId = useRef(1)
  // Position in the history when arrowing up through it; null when typing.
  const histPos = useRef(null)

  const refreshSchema = useCallback(async () => {
    try {
      const s = await fetchSchema()
      setSchema(s)
      setOnline(true)
    } catch {
      setOnline(false)
    }
  }, [])

  useEffect(() => {
    refreshSchema()
  }, [refreshSchema])

  const execute = useCallback(
    async (q) => {
      const query = (q ?? '').trim()
      if (!query) return

      const id = nextId.current++
      setFrames((f) => [{ id, query, pending: true }, ...f])

      setHistory((h) => {
        const next = [query, ...h.filter((x) => x !== query)].slice(0, MAX_HISTORY)
        save(HISTORY_KEY, next)
        return next
      })
      histPos.current = null

      try {
        const result = await runQuery(query)
        setFrames((f) => f.map((fr) => (fr.id === id ? { ...fr, pending: false, result } : fr)))
        setOnline(true)
        // A write may have introduced a new label or relationship type.
        if (result.touched > 0) refreshSchema()
      } catch (e) {
        setFrames((f) =>
          f.map((fr) => (fr.id === id ? { ...fr, pending: false, error: String(e.message || e) } : fr)),
        )
        if (String(e).includes('Failed to fetch')) setOnline(false)
      }
    },
    [refreshSchema],
  )

  const submit = useCallback(() => {
    execute(text)
    setText('')
  }, [execute, text])

  const onKeyDown = useCallback(
    (e) => {
      // Ctrl/Cmd+Enter runs; plain Enter inserts a newline, because these
      // queries are routinely multi-line.
      if ((e.ctrlKey || e.metaKey) && e.key === 'Enter') {
        e.preventDefault()
        submit()
        return
      }
      // Arrow through history, but only from the edges of the text so that
      // normal cursor movement inside a multi-line query still works.
      const ta = e.currentTarget
      if (e.key === 'ArrowUp' && ta.selectionStart === 0 && history.length) {
        e.preventDefault()
        const pos = histPos.current == null ? 0 : Math.min(histPos.current + 1, history.length - 1)
        histPos.current = pos
        setText(history[pos])
      } else if (e.key === 'ArrowDown' && ta.selectionStart === ta.value.length && histPos.current != null) {
        e.preventDefault()
        const pos = histPos.current - 1
        if (pos < 0) {
          histPos.current = null
          setText('')
        } else {
          histPos.current = pos
          setText(history[pos])
        }
      }
    },
    [history, submit],
  )

  const insert = useCallback((q) => {
    setText(q)
    taRef.current?.focus()
  }, [])

  return (
    <div className="app">
      <header className="topbar">
        <button className="icon-btn" title="Toggle sidebar" onClick={() => setSidebar((s) => !s)}>
          ☰
        </button>
        <div className="brand">
          <Logo />
          glider <span className="ver">browser</span>
        </div>
        <div className="spacer" />
        <div className="conn" title={transportKind() === 'wasm' ? 'glider is running in this tab as WebAssembly' : 'talking to a glider server'}>
          <span className={`dot ${online === null ? '' : online ? 'up' : 'down'}`} />
          {online === null ? 'starting' : online ? transportLabel() : 'offline'}
        </div>
      </header>

      <div className="body">
        <aside className={`sidebar${sidebar ? '' : ' collapsed'}`}>
          <Section title="Node labels">
            {schema?.labels?.length ? (
              <div className="chips">
                {schema.labels.map((l) => (
                  <button
                    key={l.name}
                    className="chip"
                    style={{ background: labelColor(l.name), color: '#0b0f14' }}
                    onClick={() => insert(`MATCH (n:${l.name}) RETURN n LIMIT 25`)}
                  >
                    {l.name} <span className="n" style={{ color: '#0b0f1499' }}>{l.count}</span>
                  </button>
                ))}
              </div>
            ) : (
              <div className="side-empty">none</div>
            )}
          </Section>

          <Section title="Relationship types">
            {schema?.edge_types?.length ? (
              <div className="chips">
                {schema.edge_types.map((t) => (
                  <button
                    key={t.name}
                    className="chip rel"
                    onClick={() => insert(`MATCH (a)-[r:${t.name}]->(b) RETURN a, r, b LIMIT 25`)}
                  >
                    {t.name} <span className="n">{t.count}</span>
                  </button>
                ))}
              </div>
            ) : (
              <div className="side-empty">none</div>
            )}
          </Section>

          <Section title="Examples">
            {EXAMPLES.map((q) => (
              <button key={q} className="side-item" title={q} onClick={() => insert(q)}>
                {q}
              </button>
            ))}
          </Section>

          <Section title="History">
            {history.length ? (
              history.slice(0, 15).map((q, i) => (
                <button key={i} className="side-item" title={q} onClick={() => insert(q)}>
                  {q}
                </button>
              ))
            ) : (
              <div className="side-empty">nothing yet</div>
            )}
          </Section>
        </aside>

        <main className="main">
          <div className="editor-wrap">
            <div className="editor">
              <span className="prompt">»</span>
              <textarea
                ref={taRef}
                value={text}
                spellCheck={false}
                autoFocus
                rows={Math.min(10, Math.max(1, text.split('\n').length))}
                placeholder="MATCH (n)-[r]->(m) RETURN n, r, m LIMIT 25"
                onChange={(e) => {
                  setText(e.target.value)
                  histPos.current = null
                }}
                onKeyDown={onKeyDown}
              />
              <button className="run" onClick={submit} disabled={!text.trim()}>
                ▶ Run
              </button>
            </div>
            <div className="hint">
              <kbd>Ctrl</kbd>+<kbd>Enter</kbd> run · <kbd>↑</kbd> history · double-click a node to expand
            </div>
          </div>

          <div className="stream">
            {frames.length === 0 && (
              <div className="welcome">
                <h2>An embeddable property-graph database</h2>
                <p>Run a query to begin. Results that contain nodes or relationships are drawn as a graph.</p>
                <div className="examples">
                  {EXAMPLES.map((q) => (
                    <button key={q} onClick={() => execute(q)}>
                      {q}
                    </button>
                  ))}
                </div>
              </div>
            )}
            {frames.map((f) => (
              <Frame
                key={f.id}
                frame={f}
                onClose={(id) => setFrames((fs) => fs.filter((x) => x.id !== id))}
                onRunQuery={execute}
              />
            ))}
          </div>
        </main>
      </div>
    </div>
  )
}

function Section({ title, children }) {
  return (
    <div className="side-section">
      <div className="side-title">{title}</div>
      {children}
    </div>
  )
}

function Logo() {
  // A glider from Conway's Life — the shape the project is named for.
  const cells = [
    [1, 0], [2, 1], [0, 2], [1, 2], [2, 2],
  ]
  return (
    <svg width="15" height="15" viewBox="0 0 3 3" aria-hidden="true">
      {cells.map(([x, y]) => (
        <rect key={`${x}-${y}`} x={x} y={y} width="0.86" height="0.86" rx="0.12" fill="var(--accent)" />
      ))}
    </svg>
  )
}

function load(key, fallback) {
  try {
    const v = JSON.parse(localStorage.getItem(key))
    return Array.isArray(v) ? v : fallback
  } catch {
    // Private windows and blocked site data both throw here; a console that
    // works without history is better than one that fails to start.
    return fallback
  }
}

function save(key, value) {
  try {
    localStorage.setItem(key, JSON.stringify(value))
  } catch {
    /* non-fatal */
  }
}
