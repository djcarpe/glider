import { useCallback, useEffect, useRef, useState } from 'react'
import Editor from './Editor'
import Explorer from './Explorer'
import Frame from './Frame'
import { canOpenFiles, exportJsonl, fetchSchema, openFile, runQuery, sourceName, transportKind, transportLabel } from './api'
import { labelColor } from './entities'

const EXAMPLES = [
  'MATCH (n)-[r]->(m) RETURN n, r, m LIMIT 25',
  'MATCH (n) RETURN labels(n) AS label, count(n) AS n ORDER BY n DESC',
  'CALL pagerank(iterations: 20, top: 10)',
  'SCHEMA',
  'STATS',
]

const HISTORY_KEY = 'glider.history'
const MODE_KEY = 'glider.mode'
const MAX_HISTORY = 40

export default function App() {
  const [text, setText] = useState('')
  const [frames, setFrames] = useState([])
  const [schema, setSchema] = useState(null)
  const [online, setOnline] = useState(null)
  const [history, setHistory] = useState(() => load(HISTORY_KEY, []))
  const [sidebar, setSidebar] = useState(true)
  // 'console' writes queries; 'explore' browses without them. Both stay
  // mounted so switching back finds frames and the canvas as they were.
  const [mode, setMode] = useState(() => (loadStr(MODE_KEY) === 'explore' ? 'explore' : 'console'))
  const editorRef = useRef(null)
  const nextId = useRef(1)
  // Bumped whenever a file replaces the graph, to remount the explorer so it
  // holds nothing from the graph that was there before.
  const [epoch, setEpoch] = useState(0)
  const [fileNote, setFileNote] = useState(null) // {error?, text}
  const [dragging, setDragging] = useState(false)
  const fileRef = useRef(null)

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

  const insert = useCallback((q) => {
    editorRef.current?.set(q)
  }, [])

  // ---- opening a file (wasm build only: the server's graph is its own file)
  const open = useCallback(
    async (file) => {
      if (!file) return
      setFileNote({ text: `opening ${file.name}…` })
      try {
        const t0 = performance.now()
        await openFile(file)
        const s = await fetchSchema()
        setSchema(s)
        setOnline(true)
        setFrames([])
        setEpoch((n) => n + 1)
        const secs = ((performance.now() - t0) / 1000).toFixed(2)
        setFileNote({ text: `${file.name}: ${s.nodes.toLocaleString()} nodes, ${s.edges.toLocaleString()} relationships in ${secs}s` })
      } catch (e) {
        setFileNote({ error: true, text: `could not open ${file.name}: ${e.message || e}` })
      }
    },
    [],
  )

  const download = useCallback(async () => {
    try {
      const text = await exportJsonl()
      const url = URL.createObjectURL(new Blob([text], { type: 'application/x-ndjson' }))
      const a = document.createElement('a')
      a.href = url
      a.download = (sourceName() ?? 'graph').replace(/\.[^.]*$/, '') + '.jsonl'
      a.click()
      setTimeout(() => URL.revokeObjectURL(url), 1000)
    } catch (e) {
      setFileNote({ error: true, text: `export failed: ${e.message || e}` })
    }
  }, [])

  const canOpen = canOpenFiles()
  const dropProps = canOpen
    ? {
        onDragOver: (e) => {
          if (![...e.dataTransfer.types].includes('Files')) return
          e.preventDefault()
          setDragging(true)
        },
        onDragLeave: (e) => {
          if (e.currentTarget === e.target || !e.currentTarget.contains(e.relatedTarget)) setDragging(false)
        },
        onDrop: (e) => {
          e.preventDefault()
          setDragging(false)
          open(e.dataTransfer.files?.[0])
        },
      }
    : {}

  return (
    <div className="app" {...dropProps}>
      {dragging && (
        <div className="drop-veil">
          <div>Drop a <code>.gldb</code> or <code>.jsonl</code> file to explore it</div>
        </div>
      )}
      <header className="topbar">
        <button className="icon-btn" title="Toggle sidebar" onClick={() => setSidebar((s) => !s)}>
          ☰
        </button>
        <div className="brand">
          <Logo />
          glider <span className="ver">browser</span>
        </div>
        <div className="mode" role="tablist">
          {['console', 'explore'].map((m) => (
            <button
              key={m}
              role="tab"
              aria-selected={mode === m}
              className={`tab${mode === m ? ' on' : ''}`}
              onClick={() => {
                setMode(m)
                saveStr(MODE_KEY, m)
              }}
            >
              {m === 'console' ? 'Console' : 'Explore'}
            </button>
          ))}
        </div>
        <div className="spacer" />
        {canOpen && (
          <>
            <input
              ref={fileRef}
              type="file"
              accept=".gldb,.jsonl,.ndjson,.json"
              hidden
              onChange={(e) => {
                open(e.target.files?.[0])
                e.target.value = ''
              }}
            />
            <button className="top-btn" title="Open a .gldb database or JSON Lines file in this tab" onClick={() => fileRef.current?.click()}>
              Open file…
            </button>
            <button className="top-btn" title="Download this graph as JSON Lines — edits here are not written back to the file" onClick={download}>
              Export
            </button>
          </>
        )}
        <div className="conn" title={transportKind() === 'wasm' ? 'glider is running in this tab as WebAssembly' : 'talking to a glider server'}>
          <span className={`dot ${online === null ? '' : online ? 'up' : 'down'}`} />
          {online === null ? 'starting' : online ? transportLabel() : 'offline'}
        </div>
      </header>

      {fileNote && (
        <div className={`file-note${fileNote.error ? ' err' : ''}`} role="status">
          {fileNote.text}
          <button className="icon-btn" title="Dismiss" onClick={() => setFileNote(null)}>
            ×
          </button>
        </div>
      )}

      <div className="body" hidden={mode !== 'console'}>
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
              <Editor
                ref={editorRef}
                value={text}
                onChange={setText}
                onRun={submit}
                history={history}
                schema={schema}
              />
              <button className="run" onClick={submit} disabled={!text.trim()}>
                ▶ Run
              </button>
            </div>
            <div className="hint">
              <kbd>Ctrl</kbd>+<kbd>Enter</kbd> run · <kbd>Ctrl</kbd>+<kbd>Space</kbd> suggest · <kbd>Tab</kbd> accept · <kbd>↑</kbd> history · double-click a node to expand
            </div>
          </div>

          <div className="stream">
            {frames.length === 0 && (
              <div className="welcome">
                <h2>An embeddable property-graph database</h2>
                <p>
                  Run a query to begin — start typing and suggestions will show what fits, or press{' '}
                  <kbd>Ctrl</kbd>+<kbd>Space</kbd>. Results that contain nodes or relationships are drawn as a graph.
                  {canOpen && (
                    <>
                      <br />
                      To explore your own data, <button className="linkish" onClick={() => fileRef.current?.click()}>open a .gldb file</button> or drop one here.
                    </>
                  )}
                </p>
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

      <div className="body" hidden={mode !== 'explore'}>
        <Explorer key={epoch} schema={schema} onSchemaChange={refreshSchema} />
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

function loadStr(key) {
  try {
    return localStorage.getItem(key)
  } catch {
    return null
  }
}

function saveStr(key, value) {
  try {
    localStorage.setItem(key, value)
  } catch {
    /* non-fatal */
  }
}
