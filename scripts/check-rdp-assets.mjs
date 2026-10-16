#!/usr/bin/env node
// Verify the shipped IronRDP assets against the pinned release's manifest (which also lists upstream build patches).
import { createHash } from 'node:crypto'
import * as fs from 'node:fs'
import * as path from 'node:path'
import { fileURLToPath } from 'node:url'
const root = fileURLToPath(new URL('../rdp-plugin/', import.meta.url))
let checked = 0
for (const line of fs.readFileSync(path.join(root, 'vendor/SHA256SUMS'), 'utf8').split('\n')) {
    const match = /^([a-f0-9]{64})\s+(vendor\/[^\s]+)$/.exec(line)
    if (!match) continue
    const actual = createHash('sha256').update(fs.readFileSync(path.join(root, match[2]))).digest('hex')
    if (actual !== match[1]) throw new Error(`RDP vendor asset differs from its release: ${match[2]}`)
    checked++
}
if (!checked) throw new Error('The RDP asset manifest lists no vendor assets')
console.log(`Verified ${checked} RDP vendor assets`)
