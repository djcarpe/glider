import { forwardRef, useCallback, useEffect, useImperativeHandle, useLayoutEffect, useRef, useState } from 'react'
import { applyItem, complete } from './complete'

const KIND_TAG = {
  snippet: '⌘',
  keyword: 'kw',
  fn: 'ƒ',
  var: 'x',
  label: ':',
  type: '→',
  key: '.',
  proc: '⚙',
  arg: '=',
  value: '"',
}

/**
 * The query editor: a textarea with type-ahead.
 *
 * Suggestions come from complete.js and follow the caret. While the list is
 * open, ↑/↓ move through it, Tab or Enter accept, Esc closes; Ctrl+Space
 * opens it on demand. With it closed the keys do what they did before —
 * Enter is a newline, ↑/↓ at the ends of the text walk the history.
 */
const Editor = forwardRef(function Editor({ value, onChange, onRun, history, schema }, ref) {
  const taRef = useRef(null)
  const listRef = useRef(null)
  const [sug, setSug] = useState(null) // {from, to, items, active, x, y}
  // Esc closes the list until the text next changes.
  const dismissed = useRef(false)
  // A pending selection to restore after a controlled update.
  const pendingSel = useRef(null)
  // Position in the history when arrowing up through it; null when typing.
  const histPos = useRef(null)

  useImperativeHandle(ref, () => ({
    focus: () => taRef.current?.focus(),
    /** Replace the text and put the cursor at the end, as a history pick does. */
    set: (text) => {
      pendingSel.current = [text.length, text.length]
      dismissed.current = true
      onChange(text)
      taRef.current?.focus()
    },
  }))

  const refresh = useCallback(
    (force = false) => {
      const ta = taRef.current
      if (!ta || document.activeElement !== ta) return setSug(null)
      if (ta.selectionStart !== ta.selectionEnd) return setSug(null)
      if (dismissed.current && !force) return setSug(null)
      const r = complete(ta.value, ta.selectionStart, schema, { force })
      if (!r) return setSug(null)
      const { x, y } = caretXY(ta, r.from)
      setSug((prev) => ({
        ...r,
        // Keep the highlighted row across keystrokes when it is still there.
        active: Math.max(0, prev ? r.items.findIndex((it) => it.label === prev.items[prev.active]?.label) : 0),
        x,
        y,
      }))
    },
    [schema],
  )

  useLayoutEffect(() => {
    const ta = taRef.current
    if (pendingSel.current && ta) {
      const [a, b] = pendingSel.current
      pendingSel.current = null
      ta.setSelectionRange(a, b)
    }
    refresh()
  }, [value, refresh])

  // Keep the active row in view as the list is walked with the keyboard.
  useEffect(() => {
    listRef.current?.querySelector('.sug-item.on')?.scrollIntoView({ block: 'nearest' })
  }, [sug?.active])

  const accept = useCallback(
    (item) => {
      if (!sug) return
      const out = applyItem(value, sug.from, sug.to, item)
      pendingSel.current = [out.selStart, out.selEnd]
      dismissed.current = false
      onChange(out.text)
      taRef.current?.focus()
    },
    [sug, value, onChange],
  )

  const onKeyDown = useCallback(
    (e) => {
      // Ctrl/Cmd+Enter runs; plain Enter inserts a newline, because these
      // queries are routinely multi-line.
      if ((e.ctrlKey || e.metaKey) && e.key === 'Enter') {
        e.preventDefault()
        setSug(null)
        onRun()
        return
      }
      if (e.ctrlKey && (e.key === ' ' || e.code === 'Space')) {
        e.preventDefault()
        dismissed.current = false
        refresh(true)
        return
      }

      if (sug) {
        const n = sug.items.length
        if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
          e.preventDefault()
          const step = e.key === 'ArrowDown' ? 1 : -1
          setSug((s) => ({ ...s, active: (s.active + step + n) % n }))
          return
        }
        if (e.key === 'Tab' || e.key === 'Enter') {
          e.preventDefault()
          accept(sug.items[sug.active])
          return
        }
        if (e.key === 'Escape') {
          e.preventDefault()
          dismissed.current = true
          setSug(null)
          return
        }
      }

      // Arrow through history, but only from the edges of the text so that
      // normal cursor movement inside a multi-line query still works.
      const ta = e.currentTarget
      if (e.key === 'ArrowUp' && ta.selectionStart === 0 && history.length) {
        e.preventDefault()
        const pos = histPos.current == null ? 0 : Math.min(histPos.current + 1, history.length - 1)
        histPos.current = pos
        dismissed.current = true
        onChange(history[pos])
      } else if (e.key === 'ArrowDown' && ta.selectionStart === ta.value.length && histPos.current != null) {
        e.preventDefault()
        const pos = histPos.current - 1
        dismissed.current = true
        if (pos < 0) {
          histPos.current = null
          onChange('')
        } else {
          histPos.current = pos
          onChange(history[pos])
        }
      }
    },
    [sug, accept, refresh, onRun, history, onChange],
  )

  const active = sug?.items[sug.active]

  return (
    <div className="editor-field">
      <textarea
        ref={taRef}
        value={value}
        spellCheck={false}
        autoFocus
        rows={Math.min(10, Math.max(1, value.split('\n').length))}
        placeholder="Start typing — MATCH, CREATE, CALL … (Ctrl+Space for ideas)"
        aria-autocomplete="list"
        aria-expanded={!!sug}
        aria-controls="glider-suggest"
        onChange={(e) => {
          histPos.current = null
          dismissed.current = false
          onChange(e.target.value)
        }}
        onKeyDown={onKeyDown}
        // Moving the caret by click changes the context; re-read it.
        onClick={() => refresh()}
        onBlur={() => setSug(null)}
      />
      {sug && (
        <div className="sug" style={{ left: sug.x, top: sug.y }} role="listbox" id="glider-suggest">
          <div className="sug-list" ref={listRef}>
            {sug.items.map((it, i) => (
              <div
                key={`${it.kind}:${it.label}`}
                role="option"
                aria-selected={i === sug.active}
                className={`sug-item${i === sug.active ? ' on' : ''}`}
                // mousedown, not click: a click would blur the textarea first
                // and the list would be gone before the click landed.
                onMouseDown={(e) => {
                  e.preventDefault()
                  accept(it)
                }}
                onMouseEnter={() => setSug((s) => ({ ...s, active: i }))}
              >
                <span className={`sug-kind k-${it.kind}`}>{KIND_TAG[it.kind] ?? ''}</span>
                <span className="sug-label">{it.label}</span>
              </div>
            ))}
          </div>
          {active && (
            <div className="sug-detail">
              {active.kind === 'snippet' ? <pre>{preview(active.insert)}</pre> : null}
              {active.detail && <div>{active.detail}</div>}
              <div className="sug-keys">
                <kbd>Tab</kbd> accept · <kbd>↑</kbd><kbd>↓</kbd> choose · <kbd>Esc</kbd> close
              </div>
            </div>
          )}
        </div>
      )}
    </div>
  )
})

export default Editor

/** A template as it will read once inserted, markers removed. */
function preview(insert) {
  return insert.replace(/\$\{([^}]*)\}/g, '$1').replace('$0', '')
}

// Mirror-div trick: lay the text up to `pos` out in an invisible div styled
// like the textarea, and measure where it ends. Coordinates are relative to
// the textarea's offset parent (.editor-field).
const MIRRORED = [
  'boxSizing', 'width', 'paddingTop', 'paddingRight', 'paddingBottom', 'paddingLeft',
  'borderTopWidth', 'borderRightWidth', 'borderBottomWidth', 'borderLeftWidth',
  'fontFamily', 'fontSize', 'fontWeight', 'fontStyle', 'letterSpacing', 'lineHeight',
  'textTransform', 'wordSpacing', 'tabSize', 'whiteSpace', 'wordWrap', 'overflowWrap',
]

function caretXY(ta, pos) {
  const cs = getComputedStyle(ta)
  const div = document.createElement('div')
  for (const p of MIRRORED) div.style[p] = cs[p]
  div.style.position = 'absolute'
  div.style.visibility = 'hidden'
  div.style.whiteSpace = 'pre-wrap'
  div.style.overflow = 'hidden'
  div.textContent = ta.value.slice(0, pos)
  const mark = document.createElement('span')
  mark.textContent = '​'
  div.appendChild(mark)
  document.body.appendChild(div)
  const lh = parseFloat(cs.lineHeight) || parseFloat(cs.fontSize) * 1.4
  const x = ta.offsetLeft + mark.offsetLeft - ta.scrollLeft
  const y = ta.offsetTop + mark.offsetTop - ta.scrollTop + lh + 2
  document.body.removeChild(div)
  // Keep the list inside the editor's width.
  return { x: Math.max(0, Math.min(x, ta.offsetLeft + ta.clientWidth - 320)), y }
}
