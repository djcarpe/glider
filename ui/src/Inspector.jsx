import { useState } from 'react'
import { labelColor, captionOf } from './entities'
import { parseTyped, typeOf, toText } from './edit'

const TYPES = ['auto', 'text', 'int', 'float', 'bool', 'null', 'list']

/**
 * The explorer's right-hand panel: everything about the selected node or
 * relationship, editable in place. Also hosts the two creation forms — a new
 * node, and a new relationship from the selected node to one picked by
 * clicking.
 *
 * Purely presentational: every change goes out through `actions`, and the
 * explorer hands back a fresh entity when the engine confirms it. The panel
 * is keyed on the selection by its parent, so local editing state resets when
 * the selection changes.
 */
export default function Inspector({ target, schema, nodeById, busy, linking, actions }) {
  if (!target) {
    return (
      <div className="ins-empty">
        <p>Select a node or relationship to inspect and edit it.</p>
        <p>
          <button className="btn" onClick={actions.newNode}>+ New node</button>
        </p>
      </div>
    )
  }
  if (target.kind === 'new') return <NewNodeForm schema={schema} busy={busy} actions={actions} />
  if (target.kind === 'rel') return <RelPanel edge={target.edge} nodeById={nodeById} busy={busy} actions={actions} />
  return <NodePanel node={target.node} schema={schema} busy={busy} linking={linking} actions={actions} />
}

// ------------------------------------------------------------------- node

function NodePanel({ node, schema, busy, linking, actions }) {
  const [newLabel, setNewLabel] = useState('')
  const [confirm, setConfirm] = useState(false)
  const isLinkSource = linking && linking.from.id === node.id

  const addLabel = () => {
    const l = newLabel.trim()
    if (!l) return
    actions.addLabel(node, l)
    setNewLabel('')
  }

  return (
    <div className="ins">
      <div className="ins-head">
        <span className="ins-dot" style={{ background: labelColor(node.labels?.[0]) }} />
        <span className="ins-title">{captionOf(node)}</span>
        <span className="ins-id">#{node.id}</span>
      </div>
      <div className="ins-meta">
        {node.degree ?? 0} relationship{node.degree === 1 ? '' : 's'}
      </div>

      <Section title="Labels">
        <div className="chips">
          {(node.labels || []).map((l) => (
            <span key={l} className="chip" style={{ background: labelColor(l), color: '#0b0f14' }}>
              {l}
              <button className="chip-x" title={`Remove label ${l}`} disabled={busy} onClick={() => actions.removeLabel(node, l)}>
                ×
              </button>
            </span>
          ))}
          {!node.labels?.length && <span className="side-empty" style={{ padding: 0 }}>none</span>}
        </div>
        <div className="ins-row">
          <input
            className="ins-input"
            list="glider-labels"
            placeholder="add label…"
            value={newLabel}
            disabled={busy}
            onChange={(e) => setNewLabel(e.target.value)}
            onKeyDown={(e) => e.key === 'Enter' && addLabel()}
          />
          <button className="btn" disabled={busy || !newLabel.trim()} onClick={addLabel}>
            Add
          </button>
        </div>
      </Section>

      <Section title="Properties">
        <PropsEditor ent={node} busy={busy} actions={actions} />
      </Section>

      <Section title="Relationships">
        {isLinkSource ? (
          <LinkForm linking={linking} schema={schema} busy={busy} actions={actions} />
        ) : (
          <div className="ins-actions">
            <button className="btn" disabled={busy} onClick={() => actions.expand(node.id)}>
              Expand neighbours
            </button>
            <button className="btn" disabled={busy || !!linking} onClick={() => actions.startLink(node)}>
              New relationship…
            </button>
          </div>
        )}
      </Section>

      <Section title="Node">
        <div className="ins-actions">
          <button className="btn" disabled={busy} onClick={() => actions.remove(node)}>
            Remove from canvas
          </button>
          {confirm ? (
            <div className="ins-row">
              <button className="btn danger" disabled={busy} onClick={() => actions.del(node)}>
                Confirm delete
              </button>
              <button className="btn" onClick={() => setConfirm(false)}>Cancel</button>
            </div>
          ) : (
            <button className="btn danger-soft" disabled={busy} onClick={() => setConfirm(true)}>
              Delete node…
            </button>
          )}
        </div>
        <div className="ins-note">Deleting a node also deletes its relationships.</div>
      </Section>
    </div>
  )
}

// ------------------------------------------------------------ relationship

function RelPanel({ edge, nodeById, busy, actions }) {
  const [confirm, setConfirm] = useState(false)
  const from = nodeById(edge.from)
  const to = nodeById(edge.to)
  const end = (n, id) => (
    <button className="ins-endpoint" onClick={() => actions.selectNode(id)} title={`Select node #${id}`}>
      <span className="ins-dot" style={{ background: labelColor(n?.labels?.[0]) }} />
      {n ? captionOf(n) : `#${id}`}
    </button>
  )

  return (
    <div className="ins">
      <div className="ins-head">
        <span className="ins-title mono">{edge.type}</span>
        <span className="ins-id">#{edge.id}</span>
      </div>
      <div className="ins-endpoints">
        {end(from, edge.from)}
        <span className="ins-arrow">→</span>
        {end(to, edge.to)}
      </div>

      <Section title="Properties">
        <PropsEditor ent={edge} busy={busy} actions={actions} />
      </Section>

      <Section title="Relationship">
        <div className="ins-actions">
          <button className="btn" disabled={busy} onClick={() => actions.remove(edge)}>
            Remove from canvas
          </button>
          {confirm ? (
            <div className="ins-row">
              <button className="btn danger" disabled={busy} onClick={() => actions.del(edge)}>
                Confirm delete
              </button>
              <button className="btn" onClick={() => setConfirm(false)}>Cancel</button>
            </div>
          ) : (
            <button className="btn danger-soft" disabled={busy} onClick={() => setConfirm(true)}>
              Delete relationship…
            </button>
          )}
        </div>
      </Section>
    </div>
  )
}

// -------------------------------------------------------------- properties

function PropsEditor({ ent, busy, actions }) {
  const entries = Object.entries(ent.props || {})
  return (
    <div className="props">
      {entries.map(([k, v]) => (
        <PropRow key={k} name={k} value={v} busy={busy} onSave={(val) => actions.setProp(ent, k, val)} onRemove={() => actions.removeProp(ent, k)} />
      ))}
      {!entries.length && <div className="ins-note">no properties</div>}
      <NewPropRow busy={busy} existing={ent.props || {}} onAdd={(k, val) => actions.setProp(ent, k, val)} />
    </div>
  )
}

/**
 * One property. Edits are local until Enter or blur; Escape puts the stored
 * value back. The type chip shows how the text will be read, and can be
 * overridden for the cases 'auto' gets wrong.
 */
function PropRow({ name, value, busy, onSave, onRemove }) {
  const [text, setText] = useState(() => toText(value))
  const [type, setType] = useState('auto')
  const [error, setError] = useState(null)
  const dirty = text !== toText(value) || type !== 'auto'

  let preview
  try {
    preview = typeOf(parseTyped(text, type))
  } catch {
    preview = '?'
  }

  const save = () => {
    if (!dirty) return
    try {
      onSave(parseTyped(text, type))
      setError(null)
      setType('auto')
    } catch (e) {
      setError(String(e.message || e))
    }
  }
  const reset = () => {
    setText(toText(value))
    setType('auto')
    setError(null)
  }

  return (
    <div className={`prop-row${dirty ? ' dirty' : ''}`}>
      <div className="prop-key" title={name}>{name}</div>
      <input
        className="ins-input"
        value={text}
        disabled={busy}
        onChange={(e) => setText(e.target.value)}
        onBlur={save}
        onKeyDown={(e) => {
          if (e.key === 'Enter') save()
          if (e.key === 'Escape') reset()
        }}
      />
      <select className="prop-type" value={type} disabled={busy} title={`read as ${preview}`} onChange={(e) => setType(e.target.value)}>
        {TYPES.map((t) => (
          <option key={t} value={t}>{t === 'auto' ? `auto·${preview}` : t}</option>
        ))}
      </select>
      <button className="icon-btn" title="Remove property" disabled={busy} onClick={onRemove}>✕</button>
      {error && <div className="prop-err">{error}</div>}
    </div>
  )
}

function NewPropRow({ busy, existing, onAdd }) {
  const [key, setKey] = useState('')
  const [text, setText] = useState('')
  const [type, setType] = useState('auto')
  const [error, setError] = useState(null)
  const k = key.trim()

  let preview
  try {
    preview = typeOf(parseTyped(text, type))
  } catch {
    preview = '?'
  }

  const add = () => {
    if (!k) return
    if (k in existing) {
      setError(`"${k}" already exists — edit it above`)
      return
    }
    try {
      onAdd(k, parseTyped(text, type))
      setKey('')
      setText('')
      setType('auto')
      setError(null)
    } catch (e) {
      setError(String(e.message || e))
    }
  }

  return (
    <div className="prop-row new">
      <input className="ins-input" placeholder="key" value={key} disabled={busy} onChange={(e) => setKey(e.target.value)} onKeyDown={(e) => e.key === 'Enter' && add()} />
      <input className="ins-input" placeholder="value" value={text} disabled={busy} onChange={(e) => setText(e.target.value)} onKeyDown={(e) => e.key === 'Enter' && add()} />
      <select className="prop-type" value={type} disabled={busy} onChange={(e) => setType(e.target.value)}>
        {TYPES.map((t) => (
          <option key={t} value={t}>{t === 'auto' ? `auto·${preview}` : t}</option>
        ))}
      </select>
      <button className="icon-btn add" title="Add property" disabled={busy || !k} onClick={add}>+</button>
      {error && <div className="prop-err">{error}</div>}
    </div>
  )
}

// ------------------------------------------------------------- creation

/** New relationship from `linking.from`; the target is chosen by clicking. */
function LinkForm({ linking, schema, busy, actions }) {
  const [type, setType] = useState(schema?.edge_types?.[0]?.name || '')
  const [dir, setDir] = useState('out')
  const ready = linking.to && type.trim()
  const [a, b] = dir === 'out' ? [linking.from, linking.to] : [linking.to, linking.from]

  return (
    <div className="link-form">
      <div className="ins-note">New relationship — click a node on the canvas or in the list to pick the other end.</div>
      <div className="ins-row">
        <input
          className="ins-input mono"
          list="glider-types"
          placeholder="TYPE"
          value={type}
          disabled={busy}
          onChange={(e) => setType(e.target.value)}
        />
        <button className="btn" title="Flip direction" onClick={() => setDir((d) => (d === 'out' ? 'in' : 'out'))}>
          {dir === 'out' ? 'outgoing →' : '← incoming'}
        </button>
      </div>
      <div className="link-preview">
        <span className="ent" style={{ background: labelColor(a?.labels?.[0]), color: '#0b0f14' }}>{a ? captionOf(a) : '?'}</span>
        <span className="mono"> —[:{type.trim() || '?'}]→ </span>
        <span className="ent" style={{ background: b ? labelColor(b.labels?.[0]) : '#1d2431', color: b ? '#0b0f14' : 'var(--text-faint)' }}>
          {b ? captionOf(b) : 'pick a node…'}
        </span>
      </div>
      <div className="ins-row">
        <button className="btn primary" disabled={busy || !ready} onClick={() => actions.createEdge(type.trim(), dir)}>
          Create
        </button>
        <button className="btn" disabled={busy} onClick={actions.cancelLink}>Cancel</button>
      </div>
    </div>
  )
}

function NewNodeForm({ schema, busy, actions }) {
  const [labels, setLabels] = useState('')
  const [props, setProps] = useState([])
  const [key, setKey] = useState('')
  const [text, setText] = useState('')
  const [type, setType] = useState('auto')
  const [error, setError] = useState(null)

  const labelList = labels.split(/[\s,:]+/).map((s) => s.trim()).filter(Boolean)

  const addProp = () => {
    const k = key.trim()
    if (!k) return
    if (props.some((p) => p.key === k)) {
      setError(`"${k}" is already listed`)
      return
    }
    try {
      setProps((p) => [...p, { key: k, value: parseTyped(text, type) }])
      setKey('')
      setText('')
      setType('auto')
      setError(null)
    } catch (e) {
      setError(String(e.message || e))
    }
  }

  const create = () => {
    const obj = {}
    for (const p of props) obj[p.key] = p.value
    actions.createNode(labelList, obj)
  }

  return (
    <div className="ins">
      <div className="ins-head">
        <span className="ins-title">New node</span>
      </div>

      <Section title="Labels">
        <input
          className="ins-input"
          list="glider-labels"
          placeholder="Person Employee  (space or comma separated)"
          value={labels}
          disabled={busy}
          onChange={(e) => setLabels(e.target.value)}
        />
        {labelList.length > 0 && (
          <div className="chips" style={{ marginTop: 6 }}>
            {labelList.map((l) => (
              <span key={l} className="chip" style={{ background: labelColor(l), color: '#0b0f14' }}>{l}</span>
            ))}
          </div>
        )}
      </Section>

      <Section title="Properties">
        <div className="props">
          {props.map((p) => (
            <div className="prop-row" key={p.key}>
              <div className="prop-key">{p.key}</div>
              <div className="prop-val">{toText(p.value) || <span className="v-null">null</span>}</div>
              <span className="prop-type static">{typeOf(p.value)}</span>
              <button className="icon-btn" title="Remove" onClick={() => setProps((ps) => ps.filter((x) => x.key !== p.key))}>✕</button>
            </div>
          ))}
          <div className="prop-row new">
            <input className="ins-input" placeholder="key" value={key} disabled={busy} onChange={(e) => setKey(e.target.value)} onKeyDown={(e) => e.key === 'Enter' && addProp()} />
            <input className="ins-input" placeholder="value" value={text} disabled={busy} onChange={(e) => setText(e.target.value)} onKeyDown={(e) => e.key === 'Enter' && addProp()} />
            <select className="prop-type" value={type} disabled={busy} onChange={(e) => setType(e.target.value)}>
              {TYPES.map((t) => (
                <option key={t} value={t}>{t}</option>
              ))}
            </select>
            <button className="icon-btn add" title="Add property" disabled={busy || !key.trim()} onClick={addProp}>+</button>
            {error && <div className="prop-err">{error}</div>}
          </div>
        </div>
      </Section>

      <div className="ins-row">
        <button className="btn primary" disabled={busy} onClick={create}>Create node</button>
        <button className="btn" disabled={busy} onClick={actions.cancelNew}>Cancel</button>
      </div>
    </div>
  )
}

function Section({ title, children }) {
  return (
    <div className="ins-section">
      <div className="side-title">{title}</div>
      {children}
    </div>
  )
}
