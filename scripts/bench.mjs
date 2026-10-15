#!/usr/bin/env node
// Measures the built app's terminal (out/tabbi.app) as a background test instance: how long a flood of output takes
// to get through (`seq`, about 15 MB), and how long a typed character takes to come back (the shell's echo): until
// the terminal (xterm's parser) has it, and until the page draws the frame that shows it. Run it when nothing else
// keeps the Mac busy.
//   node scripts/bench.mjs [rounds] [path to another tabbi.app to measure]
import * as fs from 'node:fs'
import * as net from 'node:net'
import * as os from 'node:os'
import * as path from 'node:path'
import * as url from 'node:url'
import { spawn } from 'node:child_process'

const root = path.resolve(url.fileURLToPath(new URL('.', import.meta.url)), '..')
const rounds = Number(process.argv[2] ?? 3)
const bundle = path.resolve(process.argv[3] ?? path.join(root, 'out/tabbi.app'))
const port = 9100 + Math.floor(Math.random() * 400)
const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), 'tabbi-bench-'))
const log = path.join(dataDir, 'app.log')
const sleep = ms => new Promise(r => setTimeout(r, ms))

const app = spawn(path.join(bundle, 'Contents/MacOS/tabbi'), [], {
    env: { ...process.env, TABBI_DEBUG_PORT: String(port), TABBI_DATA_DIR: dataDir, SSH_AUTH_SOCK: '' },
    stdio: ['ignore', fs.openSync(log, 'a'), fs.openSync(log, 'a')],
})

function evaluate (code) {
    const token = fs.readFileSync(path.join(dataDir, 'debug-token'), 'utf-8').trim()
    return new Promise((resolve, reject) => {
        const socket = net.connect(port, '127.0.0.1', () => socket.write(`${token}\n${JSON.stringify(code)}\n`))
        let buffer = ''
        socket.on('data', chunk => {
            buffer += chunk
            if (buffer.includes('\n')) {
                socket.end()
                const reply = JSON.parse(buffer.slice(0, buffer.indexOf('\n')))
                'error' in reply ? reject(new Error(reply.error)) : resolve(reply.ok)
            }
        })
        socket.on('error', reject)
    })
}

const median = values => [...values].sort((a, b) => a - b)[Math.floor(values.length / 2)]

try {
    let up = false
    for (let i = 0; i < 300 && !up; i++) {
        await sleep(100)
        up = fs.existsSync(path.join(dataDir, 'debug-token')) && await evaluate('return !!window.tabby').catch(() => false)
    }
    if (!up) {
        throw new Error('the app did not start')
    }
    // The terminal, and a helper that resolves when its parsed output shows a marker, kept on the page.
    await evaluate(`
        const sleep = ms => new Promise(r => setTimeout(r, ms))
        const config = window.tabby.get(window.require('tabby-core').ConfigService)
        for (let i = 0; i < 100 && !config.store?.profiles; i++) await sleep(100)
        const t = await window.tabby.get(window.require('tabby-local').TerminalService).openTab()
        for (let i = 0; i < 100 && !(t.frontend?.xterm && t.session?.open); i++) await sleep(100)
        await sleep(1500)
        const xterm = t.frontend.xterm
        window.bench = {
            t, xterm, sleep,
            seen: marker => new Promise(resolve => {
                const listener = xterm.onWriteParsed(() => {
                    const b = xterm.buffer.active
                    for (let i = Math.max(0, b.length - 4); i < b.length; i++) {
                        if (b.getLine(i)?.translateToString(true).includes(marker)) {
                            listener.dispose()
                            resolve(performance.now())
                            return
                        }
                    }
                })
            }),
        }
        return true`)
    const results = { flood: [], echo: [], drawn: [] }
    for (let round = 0; round < rounds; round++) {
        // (the typed command reads flood-don''e-N, so only the output matches the marker)
        results.flood.push(await evaluate(`
            const { t, seen, sleep } = window.bench
            const done = seen('flood-done-${round}')
            const start = performance.now()
            t.sendInput("seq 1 2000000; echo flood-don''e-${round}\\r")
            const ms = (await done) - start
            await sleep(500)
            return ms`))
    }
    for (let round = 0; round < rounds; round++) {
        // One character typed, until the shell's echo of it is parsed, and until the frame that draws it (the next
        // animation frame: xterm draws in one asked for while parsing); a different letter each time.
        const { echo, drawn } = await evaluate(`
            const { t, xterm, sleep } = window.bench
            const echo = [], drawn = []
            for (let i = 0; i < 40; i++) {
                const before = xterm.buffer.active.cursorX
                const parsed = new Promise(resolve => {
                    const l = xterm.onWriteParsed(() => {
                        if (xterm.buffer.active.cursorX !== before) {
                            l.dispose()
                            const at = performance.now()
                            requestAnimationFrame(() => resolve([at, performance.now()]))
                        }
                    })
                })
                const start = performance.now()
                t.sendInput(String.fromCharCode(97 + (i % 26)))
                const [at, frame] = await parsed
                echo.push(at - start)
                drawn.push(frame - start)
                await sleep(30)
            }
            t.sendInput('\\x15')
            await sleep(300)
            return { echo, drawn }`)
        results.echo.push(...echo)
        results.drawn.push(...drawn)
    }
    console.log(`flood (seq 1 2000000, ~15 MB): median ${median(results.flood).toFixed(0)} ms  [${results.flood.map(v => v.toFixed(0)).join(', ')}]`)
    for (const [name, values] of [['echo parsed', results.echo], ['echo drawn', results.drawn]]) {
        const sorted = [...values].sort((a, b) => a - b)
        // (the page's clock counts whole milliseconds; over many keys the mean still shows smaller differences)
        const mean = values.reduce((a, b) => a + b, 0) / values.length
        console.log(`${name} (${values.length} keys): mean ${mean.toFixed(2)} ms, median ${median(values).toFixed(0)} ms, 90th percentile ${sorted[Math.floor(sorted.length * 0.9)].toFixed(0)} ms`)
    }
} catch (error) {
    console.log(`FAIL ${error.message.split('\n')[0]}`)
    process.exitCode = 1
} finally {
    app.kill('SIGKILL')
}
for (const [, store] of fs.readFileSync(log, 'utf-8').matchAll(/web storage ([0-9A-F-]{36})/g)) {
    fs.rmSync(path.join(os.homedir(), 'Library/WebKit/io.github.meanaverage.tabbi/WebsiteDataStore', store.toLowerCase()), { recursive: true, force: true })
}
fs.rmSync(dataDir, { recursive: true, force: true })
