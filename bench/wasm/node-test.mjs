globalThis.self = globalThis
import { readFileSync } from 'node:fs'
self.testHooks = { glider: readFileSync(new URL('./glider/glider.wasm', import.meta.url)),
                   sqlite: { wasmBinary: readFileSync(new URL('./sqlite/sqlite3.wasm', import.meta.url)) } }
const results = []
let done
const finished = new Promise(r => { done = r })
let list = [], i = 0
globalThis.postMessage = (m) => {
  if (m.op === 'result') console.log(`${m.id.padEnd(9)} glider ${m.glider_ms.toFixed(3).padStart(9)} ms  sqlite ${m.sqlite_ms.toFixed(3).padStart(9)} ms  match=${m.match}`, m.match === false ? [m.glider_answer, m.sqlite_answer] : '')
  else console.log(JSON.stringify(m).slice(0, 240))
  if (m.op === 'ready') globalThis.self.onmessage({ data: { op: 'build', people: 5000 } })
  else if (m.op === 'built') { list = m.list; globalThis.self.onmessage({ data: { op: 'run', id: list[i++].id } }) }
  else if (m.op === 'result' || m.op === 'error') { if (i < list.length) globalThis.self.onmessage({ data: { op: 'run', id: list[i++].id } }); else done() }
}
await import('./bench-worker.mjs')
await globalThis.self.onmessage({ data: { op: 'init' } })
await finished
