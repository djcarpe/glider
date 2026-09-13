// Standalone console: glider runs in this tab as WebAssembly, with no server.
//
// Same React app as the embedded build; only the transport differs. Because
// the wasm module has no filesystem, the graph starts empty and lives for the
// life of the page — seed it with CREATE, or load JSONL.

import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { loadGlider } from '@glider/wasm'
import App from './App'
import { setTransport } from './api'
import { wasmTransport } from './transport'
import './styles.css'

const root = createRoot(document.getElementById('root'))

function Fatal({ error }) {
  return (
    <div style={{ padding: 32, fontFamily: 'var(--mono)', color: '#f85149' }}>
      <h2>Could not start glider</h2>
      <pre style={{ whiteSpace: 'pre-wrap' }}>{String(error)}</pre>
    </div>
  )
}

try {
  const glider = await loadGlider(new URL('./glider.wasm', import.meta.url))
  const db = glider.open()

  // A graph with nothing in it makes for a bleak first impression, so seed a
  // small example. Replace it with CLEAR, or your own data.
  db.run(`CREATE (ada:Person {name:"Ada", city:"London"})`)
  db.run(`CREATE (bob:Person {name:"Bob", city:"Paris"})`)
  db.run(`CREATE (cai:Person {name:"Cai", city:"Lisbon"})`)
  db.run(`MATCH (a:Person {name:"Ada"}), (b:Person {name:"Bob"}) CREATE (a)-[:KNOWS {since:2019}]->(b)`)
  db.run(`MATCH (a:Person {name:"Bob"}), (b:Person {name:"Cai"}) CREATE (a)-[:KNOWS {since:2021}]->(b)`)
  db.run(`MATCH (a:Person {name:"Cai"}), (b:Person {name:"Ada"}) CREATE (a)-[:KNOWS {since:2024}]->(b)`)

  // Handy at the console: `window.glider.exportJsonl()` to get your data out.
  window.glider = db

  setTransport(wasmTransport(db))
  root.render(
    <StrictMode>
      <App />
    </StrictMode>,
  )
} catch (e) {
  root.render(<Fatal error={e} />)
}
