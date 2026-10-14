#!/usr/bin/env node
// Runs JavaScript in a test instance's page and prints the result (see src-tauri/src/debug.rs).
// TABBI_DEBUG_PORT and TABBI_DATA_DIR as the instance was started with; code from the arguments or stdin.
import * as fs from 'node:fs'
import * as net from 'node:net'
import * as path from 'node:path'

const port = Number(process.env.TABBI_DEBUG_PORT ?? 9555)
const dataDir = process.env.TABBI_DATA_DIR
if (!dataDir) {
    console.error('set TABBI_DATA_DIR to the test instance\'s data folder')
    process.exit(2)
}
const code = process.argv.slice(2).join(' ') || fs.readFileSync(0, 'utf-8')
const token = fs.readFileSync(path.join(dataDir, 'debug-token'), 'utf-8').trim()

const socket = net.connect(port, '127.0.0.1', () => {
    socket.write(`${token}\n${JSON.stringify(code)}\n`)
})
let buffer = ''
socket.on('data', chunk => {
    buffer += chunk
    if (buffer.includes('\n')) {
        const reply = JSON.parse(buffer.slice(0, buffer.indexOf('\n')))
        if ('error' in reply) {
            console.error(reply.error)
            process.exitCode = 1
        } else {
            console.log(typeof reply.ok === 'string' ? reply.ok : JSON.stringify(reply.ok, null, 2))
        }
        socket.end()
    }
})
socket.on('error', error => {
    console.error(`could not reach the app on ${port}: ${error.message}`)
    process.exit(1)
})
