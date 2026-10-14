#!/usr/bin/env node
// Checks the built app (out/tabbi.app) end to end: starts it as a background test instance (own data folder,
// web storage, clipboard and password store; window behind the others, never takes the keyboard), drives Tabby
// inside it, closes it. SSH goes to a throwaway OpenSSH server on 127.0.0.1 with throwaway keys (macOS's own sshd,
// run as you), stopped at the end.
//   node scripts/test.mjs
import * as fs from 'node:fs'
import * as net from 'node:net'
import * as os from 'node:os'
import * as path from 'node:path'
import * as url from 'node:url'
import { execFileSync, spawn } from 'node:child_process'

const root = path.resolve(url.fileURLToPath(new URL('.', import.meta.url)), '..')
const exe = path.join(root, 'out/tabbi.app/Contents/MacOS/tabbi')
const port = 9500 + Math.floor(Math.random() * 400)
const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), 'tabbi-test-'))
const log = path.join(dataDir, 'app.log')
const sleep = ms => new Promise(r => setTimeout(r, ms))

// The throwaway SSH server
const sshDir = path.join(dataDir, 'sshd')
const sshPort = 42000 + Math.floor(Math.random() * 5000)
fs.mkdirSync(sshDir)
for (const name of ['host', 'client']) {
    execFileSync('ssh-keygen', ['-q', '-t', 'ed25519', '-N', '', '-C', `tabbi-test-${name}`, '-f', path.join(sshDir, name)])
}
// A passphrase-protected key too (Tabby issue #11352: one loaded in the agent mustn't ask for its passphrase)
execFileSync('ssh-keygen', ['-q', '-t', 'ed25519', '-N', 'test-passphrase', '-C', 'tabbi-test-locked', '-f', path.join(sshDir, 'locked')])
fs.writeFileSync(path.join(sshDir, 'authorized_keys'), ['client.pub', 'locked.pub'].map(f => fs.readFileSync(path.join(sshDir, f), 'utf-8')).join(''))
fs.writeFileSync(path.join(sshDir, 'askpass'), '#!/bin/sh\necho test-passphrase\n', { mode: 0o700 })
fs.writeFileSync(path.join(sshDir, 'sshd_config'), [
    `Port ${sshPort}`, 'ListenAddress 127.0.0.1', `HostKey ${sshDir}/host`, `AuthorizedKeysFile ${sshDir}/authorized_keys`,
    'PasswordAuthentication no', 'KbdInteractiveAuthentication no', 'UsePAM no', 'StrictModes no',
    `PidFile ${sshDir}/sshd.pid`, 'Subsystem sftp /usr/libexec/sftp-server', 'AllowTcpForwarding yes', '',
].join('\n'))
// A throwaway SSH agent, empty to start with: the app never sees the user's own agent (SSH_AUTH_SOCK is replaced).
// (In the temp folder macOS gives each user: a socket's path must be short.)
const agentSocket = path.join(os.tmpdir(), `tta-${process.pid}.sock`)
fs.rmSync(agentSocket, { force: true })
const agent = spawn('ssh-agent', ['-D', '-a', agentSocket], { stdio: 'ignore' })
const sshAdd = (...args) => execFileSync('ssh-add', args, {
    env: { ...process.env, SSH_AUTH_SOCK: agentSocket, SSH_ASKPASS: path.join(sshDir, 'askpass'), SSH_ASKPASS_REQUIRE: 'force', DISPLAY: ':0' },
    stdio: 'ignore',
})
const sshd = spawn('/usr/sbin/sshd', ['-D', '-e', '-f', path.join(sshDir, 'sshd_config')], {
    stdio: ['ignore', 'ignore', fs.openSync(path.join(sshDir, 'sshd.log'), 'a')],
})

const app = spawn(exe, [], {
    env: { ...process.env, TABBI_DEBUG_PORT: String(port), TABBI_DATA_DIR: dataDir, SSH_AUTH_SOCK: agentSocket },
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

// Helpers defined in the page for every check.
const prelude = `
    const core = window.require('tabby-core'), local = window.require('tabby-local')
    const app = window.tabby.get(core.AppService)
    const sleep = ms => new Promise(r => setTimeout(r, ms))
    const until = async (f, ms = 10000) => { for (let t = 0; t < ms; t += 100) { const v = await f(); if (v) return v; await sleep(100) } throw new Error('timed out: ' + f) }
    const screen = t => { const b = t.frontend.xterm.buffer.active, out = []; for (let i = 0; i < b.length; i++) { const l = b.getLine(i)?.translateToString(true); if (l && l.trim()) out.push(l) } return out.join('\\n') }
    const open = async cwd => { const t = await window.tabby.get(local.TerminalService).openTab(null, cwd ?? null); return ready(t) }
    const ready = t => until(() => t.frontend?.xterm && t.session?.open && t)
    const fs_exists = p => window.require('fs').existsSync(p)
    const holder = t => app.tabs.find(x => x === t || x.getAllTabs?.().includes(t))
    const run = async (t, command, expect) => { t.sendInput(command + '\\r'); await until(() => screen(t).includes(expect)); return true }
`

const checks = [
    ['Tabby starts with every plugin', `return (window.pluginModules ?? []).length === ${JSON.parse(fs.readFileSync(path.join(root, '.build/page/plugins.json'), 'utf-8')).length}`],
    ['A terminal runs a command', `const t = await open(); return run(t, 'echo tt-$((6*7))', 'tt-42')`],
    ['A terminal opens in the folder asked for', `const t = await open('/private/tmp'); return run(t, 'pwd', '/private/tmp')`],
    ['A folder that does not exist is ignored', `const t = await open('/no/such/folder'); return run(t, 'echo started', 'started')`],
    ['Paste reads the clipboard', `
        const platform = window.tabby.get(core.PlatformService)
        platform.setClipboard({ text: 'echo pasted-$((40+2))' }); await sleep(200)
        const t = await open(); await t.paste(); t.sendInput('\\r')
        return until(() => screen(t).includes('pasted-42'))`],
    ['Closing a terminal ends its shell', `
        const t = await open(); const pid = await t.session.pty.getPID()
        if (!process.kill(pid, 0)) return false
        app.closeTab(holder(t), false)
        return until(() => { try { process.kill(pid, 0); return false } catch (e) { return e.code === 'ESRCH' } })`],
    ['Splitting starts a second shell', `
        const t = await open(); const split = holder(t)
        await split.splitTab(t, 'r')
        const second = await ready(await until(() => split.getAllTabs().find(x => x !== t)))
        return run(second, 'echo second-pane', 'second-pane')`],
    ['SSH: trust prompt, key login, a command', `
        window.tabby.get(core.ConfigService).store.ssh.verifyHostKeys = true
        const opened = await window.tabby.get(core.ProfilesService).openNewTabForProfile({ type: 'ssh', name: 'test sshd', options: {
            host: '127.0.0.1', port: ${sshPort}, user: ${JSON.stringify(os.userInfo().username)}, auth: 'publicKey',
            privateKeys: ['file://${sshDir}/client'],
            forwardedPorts: [{ type: 'Local', host: '127.0.0.1', port: ${sshPort + 1}, targetAddress: '127.0.0.1', targetPort: ${sshPort}, description: 'test' }],
        } })
        const accept = await until(() => [...document.querySelectorAll('.modal-footer button')].find(b => /once/i.test(b.textContent)))
        accept.click()
        const t = opened.getAllTabs ? opened.getAllTabs()[0] : opened
        await ready(t)
        window.sshTab = t
        return run(t, 'echo remote-$((6*7))', 'remote-42')`],
    ['SSH: the remote side sees a resize', `
        const t = window.sshTab; await sleep(1500)
        t.frontend.xterm.resize(97, 31); await sleep(500)
        return run(t, 'stty size', '31 97')`],
    ['SSH: SFTP lists, reads and deletes', `
        const t = window.sshTab
        await run(t, 'echo sftp-probe > ${sshDir}/probe; echo written-$((1+1))', 'written-2')
        const sftp = await t.sshSession.openSFTP()
        const listed = (await sftp.readdir(${JSON.stringify(sshDir)})).some(e => e.name === 'probe')
        const file = await sftp.open('${sshDir}/probe', 1)
        const content = new TextDecoder().decode(await file.read()); await file.close()
        await sftp.unlink('${sshDir}/probe')
        return listed && content.trim() === 'sftp-probe' && !fs_exists('${sshDir}/probe')`],
    ['SSH: a port forward carries data', `
        const t = window.sshTab
        await until(() => t.sshSession.forwardedPorts.length > 0)
        const net = window.require('net')
        const banner = await new Promise((resolve, reject) => {
            const s = new net.Socket(); let d = ''
            s.on('data', x => { d += x.toString(); if (d.includes('\\n')) { s.destroy(); resolve(d) } })
            s.on('error', reject); s.connect(${sshPort + 1}, '127.0.0.1')
        })
        return banner.startsWith('SSH-2.0-')`],
    ['SSH: login through the agent', `
        window.tabby.get(core.ConfigService).store.ssh.verifyHostKeys = false
        const opened = await window.tabby.get(core.ProfilesService).openNewTabForProfile({ type: 'ssh', name: 'agent', options: {
            host: '127.0.0.1', port: ${sshPort}, user: ${JSON.stringify(os.userInfo().username)}, auth: 'agent', privateKeys: [],
        } })
        const t = opened.getAllTabs ? opened.getAllTabs()[0] : opened
        await ready(t)
        return run(t, 'echo agent-$((6*7))', 'agent-42')`, () => sshAdd(path.join(sshDir, 'client'))],
    ['SSH: Auto login with a key file (and its .pub) the agent lacks: Tabby issue #11743', `
        window.tabby.get(core.ConfigService).store.ssh.verifyHostKeys = false
        const opened = await window.tabby.get(core.ProfilesService).openNewTabForProfile({ type: 'ssh', name: '11743', options: {
            host: '127.0.0.1', port: ${sshPort}, user: ${JSON.stringify(os.userInfo().username)}, privateKeys: ['file://${sshDir}/client'],
        } })
        const t = opened.getAllTabs ? opened.getAllTabs()[0] : opened
        await ready(t)
        return run(t, 'echo auto-$((6*7))', 'auto-42')`, () => sshAdd('-D')],
    ['SSH: Auto login with a passphrase-protected key that is in the agent asks for nothing: Tabby issue #11352', `
        window.tabby.get(core.ConfigService).store.ssh.verifyHostKeys = false
        const opened = await window.tabby.get(core.ProfilesService).openNewTabForProfile({ type: 'ssh', name: '11352', options: {
            host: '127.0.0.1', port: ${sshPort}, user: ${JSON.stringify(os.userInfo().username)}, privateKeys: ['file://${sshDir}/locked'],
        } })
        const t = opened.getAllTabs ? opened.getAllTabs()[0] : opened
        const asked = until(() => document.querySelector('.modal input[type=password]'), 15000).then(() => true, () => false)
        const loggedIn = ready(t).then(() => run(t, 'echo unlocked-$((6*7))', 'unlocked-42'))
        const first = await Promise.race([asked.then(a => a ? 'asked for a passphrase' : null), loggedIn.then(() => 'logged in')].filter(Boolean))
        if (first !== 'logged in') throw new Error(first)
        return true`, () => { sshAdd('-D'); sshAdd(path.join(sshDir, 'locked')) }],
    ['Saved passwords: save, read, delete', `
        const keytar = window.require('keytar')
        await keytar.setPassword('ssh@test.invalid:22', 'someone', 's3cret')
        const got = await keytar.getPassword('ssh@test.invalid:22', 'someone')
        return got === 's3cret' && await keytar.deletePassword('ssh@test.invalid:22', 'someone') && await keytar.getPassword('ssh@test.invalid:22', 'someone') === null`],
    ['The window moves by its tab bar, not by its tabs', `
        const { startsWindowDrag } = window.require('tabby-tauri')
        const middle = el => { const r = el.getBoundingClientRect(); return [r.left + r.width / 2, r.top + r.height / 2] }
        const drags = el => startsWindowDrag(document.elementFromPoint(...middle(el)), ...middle(el))
        return drags(document.querySelector('.tab-bar > .btn-space.drag')) && drags(document.querySelector('.tab-bar > .inset'))
            && !drags(document.querySelector('tab-header'))`],
    ['The window moves by its top edge while a dialog is open (Tabby PR #11766)', `
        const { startsWindowDrag } = window.require('tabby-tauri')
        window.tabby.get(core.PlatformService).showMessageBox({ type: 'info', message: 'drag test', buttons: ['OK'] })
        const button = await until(() => document.querySelector('.modal-footer button'))
        // beside the dialog box: where the box itself reaches into the strip, the box wins, as in Tabby
        const top = startsWindowDrag(document.elementFromPoint(20, 10), 20, 10)
        const box = button.getBoundingClientRect()
        const dialog = startsWindowDrag(button, box.left + 2, box.top + 2)
        const seen = { top, dialog, atTop: document.elementFromPoint(20, 10)?.outerHTML.slice(0, 160), modals: document.querySelectorAll('.modal').length }
        button.click()
        if (!(top && !dialog)) throw new Error(JSON.stringify(seen))
        return true`],
    ['The Mac window buttons sit where the theme says, after title changes and resizes too', `
        const { invoke } = window.require('@tauri-apps/api/core')
        const { getCurrentWindow, LogicalSize } = window.require('@tauri-apps/api/window')
        const at = async () => (await invoke('window_traffic_light_frame')).slice(0, 2).map(v => Math.round(v)).join(',')
        const seen = [await at()]
        window.tabby.get(core.HostWindowService).setTitle('a title'); await sleep(300); seen.push(await at())
        await getCurrentWindow().setSize(new LogicalSize(980, 640)); await sleep(400); seen.push(await at())
        await invoke('window_set_traffic_light_position', { x: 20, y: 16 }); await sleep(200); seen.push(await at())
        await invoke('window_set_traffic_light_position', { x: 14, y: 11 }); await sleep(200)
        if (seen.join(' ') !== '14,11 14,11 14,11 20,16') throw new Error(seen.join(' '))
        return true`],
    ['Settings open', `app.openNewTabRaw({ type: window.require('tabby-settings').SettingsTabComponent }); return until(() => app.activeTab?.constructor?.name === 'SettingsTabComponent')`],
    ['Settings are saved', `const config = window.tabby.get(core.ConfigService); config.store.terminal.fontSize = 17; await config.save(); return true`],
]

let failed = 0
try {
    let booted = false
    for (let i = 0; i < 300 && !booted; i++) {
        await sleep(100)
        booted = fs.existsSync(path.join(dataDir, 'debug-token')) && await evaluate('return !!window.tabby').catch(() => false)
    }
    if (!booted) {
        throw new Error('the app did not start')
    }
    for (const [name, code, before] of checks) {
        try {
            before?.()
            const ok = await evaluate(prelude + code)
            console.log(`${ok ? 'ok  ' : 'FAIL'} ${name}`)
            failed += ok ? 0 : 1
        } catch (error) {
            console.log(`FAIL ${name}: ${error.message.split('\n')[0]}`)
            failed++
        }
    }
    const saved = fs.readFileSync(path.join(dataDir, 'config.yaml'), 'utf-8').includes('fontSize: 17')
    console.log(`${saved ? 'ok  ' : 'FAIL'} The config file has the change`)
    failed += saved ? 0 : 1
} catch (error) {
    console.log(`FAIL ${error.message}`)
    failed++
} finally {
    app.kill('SIGKILL')
    sshd.kill('SIGKILL')
    agent.kill('SIGKILL')
    fs.rmSync(agentSocket, { force: true })
}
// The test's own web storage (see debug::web_store in src-tauri/src/debug.rs)
const store = /web storage ([0-9A-F-]{36})/.exec(fs.readFileSync(log, 'utf-8'))?.[1]
if (store) {
    fs.rmSync(path.join(os.homedir(), 'Library/WebKit/io.github.meanaverage.tabbi/WebsiteDataStore', store.toLowerCase()), { recursive: true, force: true })
}
const pageErrors = fs.readFileSync(log, 'utf-8').split('\n').filter(l => /^page (error|uncaught|unhandled)/.test(l))
if (pageErrors.length) {
    console.log(`\nErrors in the page:\n${pageErrors.join('\n')}`)
}
fs.rmSync(dataDir, { recursive: true, force: true })
console.log(failed ? `\n${failed} failed` : '\nall passed')
process.exit(failed ? 1 : 0)
