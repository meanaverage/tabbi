#!/usr/bin/env node
// Checks the built app (out/tabbi.app) end to end: starts it as a background test instance (own data folder,
// web storage, clipboard and password store; window behind the others, never takes the keyboard), drives Tabby
// inside it, closes it. SSH goes to a throwaway OpenSSH server on 127.0.0.1 with throwaway keys (macOS's own sshd,
// run as you), stopped at the end.
//   node scripts/test.mjs [only the checks whose names match this regular expression]
import * as fs from 'node:fs'
import * as net from 'node:net'
import * as os from 'node:os'
import * as path from 'node:path'
import * as url from 'node:url'
import { execFileSync, spawn } from 'node:child_process'

const root = path.resolve(url.fileURLToPath(new URL('.', import.meta.url)), '..')
// (TABBI_APP: another built tabbi.app to check instead)
const exe = path.join(process.env.TABBI_APP ?? path.join(root, 'out/tabbi.app'), 'Contents/MacOS/tabbi')
const port = 9500 + Math.floor(Math.random() * 400)
const dataDir = fs.mkdtempSync(path.join(os.tmpdir(), 'tabbi-test-'))
const log = path.join(dataDir, 'app.log')
const sleep = ms => new Promise(r => setTimeout(r, ms))
// (macOS doesn't reopen windows or ask whether to after a crash: a test instance never shows that question)
const noRestore = ['-ApplePersistenceIgnoreState', 'YES']
const only = process.argv[2]

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

// The data folder of the app that's running (a check further down starts copies with fresh ones).
let currentDir = dataDir
const start = (dir = dataDir, env = {}) => {
    currentDir = dir
    return spawn(exe, noRestore, {
        env: { ...process.env, TABBI_DEBUG_PORT: String(port), TABBI_DATA_DIR: dir, SSH_AUTH_SOCK: agentSocket, ...env },
        stdio: ['ignore', fs.openSync(log, 'a'), fs.openSync(log, 'a')],
    })
}
let app = start()

async function booted () {
    for (let i = 0; i < 300; i++) {
        await sleep(100)
        // (up once Tabby has loaded the settings)
        const up = `return !!window.tabby?.get(window.require('tabby-core').ConfigService).store?.profiles`
        if (fs.existsSync(path.join(currentDir, 'debug-token')) && await evaluate(up).catch(() => false)) {
            return true
        }
    }
    return false
}

/** Runs code in a window of the app (the first one unless a label is given) and resolves with its result. */
function evaluate (code, window = 'main') {
    const token = fs.readFileSync(path.join(currentDir, 'debug-token'), 'utf-8').trim()
    const request = window === 'main' ? code : { window, code }
    return new Promise((resolve, reject) => {
        const socket = net.connect(port, '127.0.0.1', () => socket.write(`${token}\n${JSON.stringify(request)}\n`))
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
    // (in Angular's zone, as a click would: outside it, a new tab may wait for something else to update the page)
    const zone = window.tabby.get(window.require('@angular/core').NgZone)
    const open = async cwd => { const t = await zone.run(() => window.tabby.get(local.TerminalService).openTab(null, cwd ?? null)); return ready(t) }
    const ready = t => until(() => t.frontend?.xterm && t.session?.open && t)
    const fs_exists = p => window.require('fs').existsSync(p)
    const holder = t => app.tabs.find(x => x === t || x.getAllTabs?.().includes(t))
    const run = async (t, command, expect) => { t.sendInput(command + '\\r'); await until(() => screen(t).includes(expect)); return true }
    // A program on its own PTY, without a tab: what it printed by the time its exit came, and the exit code.
    const program = async (command, args) => {
        const pty = await window.tabby.get(local.PTYInterface).spawn(command, args, { env: { PATH: '/usr/bin:/bin', LANG: 'en_US.UTF-8' }, cols: 80, rows: 24 })
        const decoder = new TextDecoder(); let text = '', exit = null, closed = false
        pty.subscribe('data', d => { pty.ackData(d.length); text += decoder.decode(d, { stream: true }) })
        pty.subscribe('exit', code => { exit = { text, code } })
        pty.subscribe('close', () => { closed = true })
        await until(() => closed, 30000)
        return exit ?? { text, code: 'no exit' }
    }
`

const checks = [
    ['Tabby starts with every plugin', `return (window.pluginModules ?? []).length === ${JSON.parse(fs.readFileSync(path.join(root, '.build/page/plugins.json'), 'utf-8')).length}`],
    ['A terminal runs a command', `const t = await open(); return run(t, 'echo tt-$((6*7))', 'tt-42')`],
    ["A quick program's output comes before its exit", `
        const seen = []
        for (let i = 0; i < 10; i++) {
            const { text, code } = await program('/bin/echo', ['quick-' + i])
            if (!text.includes('quick-' + i) || code !== 0) seen.push({ i, text, code })
        }
        if (seen.length) throw new Error(JSON.stringify(seen))
        return true`],
    ['Output written right before exiting all comes before the exit', `
        const { text, code } = await program('/bin/sh', ['-c', 'seq 1 100000; printf tail-end; exit 3'])
        const numbers = text.split('\\r\\n').slice(0, -1)
        const whole = numbers.length === 100000 && numbers.every((n, i) => n === String(i + 1))
        if (!(whole && text.endsWith('tail-end') && code === 3)) throw new Error(JSON.stringify({ lines: numbers.length, end: text.slice(-30), code }))
        return true`],
    ['A flood of output arrives whole, its characters never cut in two', `
        const t = await open()
        let text = ''
        const subscription = t.session.output$.subscribe(s => text += s)
        t.sendInput("seq 1 100000 | sed 's/$/ é字🎉/'; echo utf-$((6*7))\\r")
        await until(() => text.includes('utf-42'), 60000)
        subscription.unsubscribe()
        const numbers = [...text.matchAll(/(\\d+) é字🎉/g)].map(m => Number(m[1]))
        const whole = numbers.length === 100000 && numbers.every((n, i) => n === i + 1)
        if (!(whole && !text.includes('\\ufffd'))) throw new Error(JSON.stringify({ lines: numbers.length, broken: text.split('\\ufffd').length - 1 }))
        return true`],
    ["A typed key's reply is drawn at once, not at the next animation frame", `
        const t = await open(), xterm = t.frontend.xterm, buffer = xterm._core._writeBuffer
        xterm.options.cursorBlink = false
        await sleep(500)
        // Drawn since the reply was handed to xterm, by the time xterm has read it?
        let drawn = null
        const write = buffer.write
        buffer.write = function (...args) { drawn = false; return write.apply(this, args) }
        const render = xterm.onRender(() => { if (drawn === false) drawn = true })
        const results = []
        for (let i = 0; i < 5; i++) {
            const parsed = new Promise(resolve => { const p = xterm.onWriteParsed(() => { p.dispose(); resolve(drawn) }) })
            const key = new KeyboardEvent('keydown', { key: 'x', code: 'KeyX', keyCode: 88, which: 88, bubbles: true, cancelable: true })
            xterm.textarea.dispatchEvent(key)
            if (!key.defaultPrevented) xterm.textarea.dispatchEvent(new KeyboardEvent('keypress', { key: 'x', code: 'KeyX', keyCode: 120, charCode: 120, which: 120, bubbles: true, cancelable: true }))
            results.push(await parsed)
            await sleep(100)
        }
        render.dispose()
        buffer.write = write
        t.sendInput('\\x15')
        if (results.filter(Boolean).length < 4) throw new Error(JSON.stringify(results))
        return true`],
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
    ['Typing exit closes the tab, also with a program left running in the background', `
        const t = await open()
        t.sendInput('(sleep 20 &); exit\\r')
        return until(() => !holder(t), 5000)`],
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
    ['The Mac window buttons stay put, macOS\'s distance apart, while the window is resized: every size is drawn with them in place', `
        const { invoke } = window.require('@tauri-apps/api/core')
        const [x, y] = [14, 11]
        const { rest, steps, native, width } = await invoke('window_traffic_light_resize_steps', { steps: 20 })
        // close, minimize and zoom's left edges and the top, where they belong
        const home = [x, x + native[0], x + native[0] + native[1], y].map(Math.round).join(',')
        const all = [rest, ...steps].map(p => p.map(Math.round).join(','))
        const off = all.filter(p => p !== home)
        if (!(native[0] > width + 2 && native[1] > width + 2)) throw new Error('buttons with no room between them: ' + JSON.stringify({ native, width }))
        if (off.length) throw new Error(off.length + ' of ' + all.length + ' drawn elsewhere than ' + home + ': ' + [...new Set(off)].join(' '))
        // (and keeping them there isn't a tug of war with macOS: nothing moves them while nothing happens)
        await sleep(500)
        const before = await invoke('window_traffic_light_moves'); await sleep(1500)
        const idle = await invoke('window_traffic_light_moves') - before
        if (idle) throw new Error('moved ' + idle + ' times while idle')
        return true`],
    ['Fonts: Settings lists the installed fonts', `
        const fonts = await window.tabby.get(core.PlatformService).listFonts()
        return fonts.length > 20 && fonts.includes('Menlo')`],
    ['Right-click menus are native menus', `
        const menu = await window.tabby.get(core.PlatformService).buildMenu([
            { label: 'One', click: () => null }, { type: 'separator' },
            { label: 'More', submenu: [{ label: 'Two' }] }, { type: 'checkbox', label: 'Three', checked: true },
        ])
        return (await menu.items()).length === 4`],
    ["A text field's menu copies its selection", `
        const platform = window.tabby.get(core.PlatformService)
        const input = document.createElement('input')
        input.value = 'hello world'
        document.body.appendChild(input)
        input.setSelectionRange(0, 5)
        const copy = platform.textFieldMenus.editItems(input).find(item => item.label === 'Copy')
        copy.click()
        input.remove()
        return copy.enabled && platform.readClipboard() === 'hello'`],
    ['Dropping a file on a terminal types its quoted path', `
        const t = await open()
        // (dropped again until Tabby's drop handling for the new terminal is in place)
        await until(async () => {
            const drop = new DragEvent('drop', { bubbles: true, cancelable: true, dataTransfer: new DataTransfer() })
            drop.tauriPaths = ["/tmp/a b/it's"]
            t.frontend.xterm.element.dispatchEvent(drop)
            await sleep(300)
            return screen(t).includes("'/tmp/a b/it'")
        })
        t.sendInput('\\x15')
        return true`],
    ['Preferences in the app menu opens Settings', `
        await window.require('@tauri-apps/api/event').emit('host:preferences-menu')
        return until(() => app.activeTab?.constructor?.name === 'SettingsTabComponent')`],
    ['The Dock menu lists the profiles', `
        const { invoke } = window.require('@tauri-apps/api/core')
        return until(async () => (await invoke('dock_menu_state')).items.length > 0)`],
    ['Docking puts the window on the right edge, half the screen wide', `
        const { getCurrentWindow, availableMonitors } = window.require('@tauri-apps/api/window')
        const config = window.tabby.get(core.ConfigService), docking = window.tabby.get(core.DockingService), w = getCurrentWindow()
        Object.assign(config.store.appearance, { dock: 'right', dockFill: 0.5, dockSpace: 1, dockAlwaysOnTop: false })
        await docking.applyDock()
        const position = await w.outerPosition(), size = await w.outerSize()
        const monitor = (await availableMonitors()).find(m => position.x >= m.position.x && position.x < m.position.x + m.size.width)
        config.store.appearance.dock = 'off'
        await docking.applyDock()
        const area = monitor.workArea
        if (Math.abs(position.x + size.width - (area.position.x + area.size.width)) > 2 || Math.abs(size.width - Math.round(area.size.width / 2)) > 2) {
            throw new Error(JSON.stringify({ position, size, area }))
        }
        return true`],
    ['The window looks apply: vibrancy, opacity, dark mode', `
        const { invoke } = window.require('@tauri-apps/api/core')
        await invoke('window_set_vibrancy', { enabled: true, kind: 'blur' })
        await invoke('window_set_opacity', { opacity: 0.9 })
        await invoke('window_set_dark_mode', { mode: 'dark' })
        await invoke('window_set_vibrancy', { enabled: false, kind: null })
        await invoke('window_set_opacity', { opacity: 1 })
        await invoke('window_set_dark_mode', { mode: 'auto' })
        return true`],
    ["The page draws at the screen's full frame rate, not WebKit's usual 60 fps", `
        const { invoke } = window.require('@tauri-apps/api/core')
        const rate = await invoke('window_frame_rate')
        const times = await new Promise(resolve => { const t = []; const frame = n => { t.push(n); t.length < 121 ? requestAnimationFrame(frame) : resolve(t) }; requestAnimationFrame(frame) })
        const gaps = times.slice(1).map((n, i) => n - times[i]).sort((a, b) => a - b)
        const fps = Math.round(1000 / gaps[gaps.length >> 1])
        if (rate.limited !== false || (rate.screen > 60 && fps < rate.screen * 0.8)) throw new Error(JSON.stringify({ ...rate, fps }))
        return true`],
    ['Export to file saves the terminal as text, scrollback included', `
        const t = await open()
        await run(t, 'seq 1 300 | sed s/^/line-/; echo export-$((6*7))', 'export-42')
        const menu = window.tabby.get(window.require('tabby-terminal').TerminalContextMenuItemProvider)
            .find(p => p.constructor.name === 'ExportTerminalContextMenu')
        const items = await menu.getItems(t)
        const file = ${JSON.stringify(path.join(dataDir, 'export.txt'))}
        await menu.exportTo(t, file)
        const text = await window.require('fs').promises.readFile(file, 'utf-8')
        return items[0].label === 'Export to file' && text.includes('line-1\\n') && text.includes('line-300') && text.includes('export-42')`],
    ['Settings open', `app.openNewTabRaw({ type: window.require('tabby-settings').SettingsTabComponent }); return until(() => app.activeTab?.constructor?.name === 'SettingsTabComponent')`],
    ['Every Settings page opens without errors', `
        // (Tabby's own Config sync page asks for the synced configs even with no sync server set, and fails:
        // configSyncSettingsTab.component.ts, ngOnInit)
        const tabbysOwn = /configSync\.host\.endsWith/
        const errors = [], consoleError = console.error
        console.error = (...args) => {
            const text = args.map(String).join(' ')
            if (!tabbysOwn.test(text)) errors.push(text.slice(0, 200))
            consoleError(...args)
        }
        try {
            app.openNewTabRaw({ type: window.require('tabby-settings').SettingsTabComponent })
            const links = await until(() => { const l = [...document.querySelectorAll('.nav-pills .nav-link')]; return l.length > 5 && l })
            for (const link of links) {
                link.click()
                await sleep(300)
            }
            if (errors.length) throw new Error(errors.join(' | '))
            return links.length
        } finally {
            console.error = consoleError
        }`],
    ['Settings are saved', `const config = window.tabby.get(core.ConfigService); config.store.terminal.fontSize = 17; await config.save(); return true`],
]

let failed = 0
try {
    if (!await booted()) {
        throw new Error('the app did not start')
    }
    for (const [name, code, before] of checks) {
        if (only && !new RegExp(only, 'i').test(name)) {
            continue
        }
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
    if (only) {
        throw null
    }
    const saved = fs.readFileSync(path.join(dataDir, 'config.yaml'), 'utf-8').includes('fontSize: 17')
    console.log(`${saved ? 'ok  ' : 'FAIL'} The config file has the change`)
    failed += saved ? 0 : 1

    // New Window: a second window with its own tabs that shares the settings, then closed again.
    let second = false
    try {
        const label = await evaluate(prelude + `
            window.tabby.get(core.HostAppService).newWindow()
            const { getAllWindows } = window.require('@tauri-apps/api/window')
            return until(async () => (await getAllWindows()).map(w => w.label).find(l => l !== 'main'))`)
        let up = false
        for (let i = 0; i < 300 && !up; i++) {
            await sleep(100)
            up = await evaluate('return !!window.tabby', label).catch(() => false)
        }
        const bootstrap = await evaluate(`const b = window.tabby.get(window.require('tabby-core').BOOTSTRAP_DATA); return [b.isMainWindow, b.windowID]`, label)
        await evaluate(prelude + `const config = window.tabby.get(core.ConfigService); config.store.terminal.fontSize = 19; await config.save(); return true`)
        const shared = await evaluate(prelude + `return until(() => window.tabby.get(core.ConfigService).store.terminal.fontSize === 19)`, label)
        const ran = await evaluate(prelude + `const t = await open(); return run(t, 'echo window-$((6*7))', 'window-42')`, label)
        const dockItem = await evaluate(`const { invoke } = window.require('@tauri-apps/api/core'); return (await invoke('dock_menu_state')).items.some(i => i.id === 'new-window')`)
        const fullRate = await evaluate(`return (await window.require('@tauri-apps/api/core').invoke('window_frame_rate')).limited === false`, label)
        await evaluate(`window.require('@tauri-apps/api/window').getCurrentWindow().destroy(); return true`, label).catch(() => null)
        await sleep(500)
        const mainAlive = await evaluate('return !!window.tabby')
        second = up && bootstrap[0] === false && bootstrap[1] > 1 && shared && ran && dockItem && fullRate && mainAlive
        if (!second) {
            console.log(`     (${JSON.stringify({ label, up, bootstrap, shared, ran, dockItem, fullRate, mainAlive })})`)
        }
    } catch (error) {
        console.log(`     (${error.message.split('\n')[0]})`)
    }
    console.log(`${second ? 'ok  ' : 'FAIL'} New Window opens a second window that shares the settings, and closes again`)
    failed += second ? 0 : 1

    // Closing tabs gives their memory back: WebKit is asked to collect garbage once tabs stop closing.
    let freed = false
    try {
        const pid = (await evaluate(`return window.require('@tauri-apps/api/core').invoke('webview_pids')`)).web
        const footprint = () => {
            const m = /Footprint: ([\d.]+) (KB|MB|GB)/.exec(execFileSync('footprint', [String(pid)], { encoding: 'utf-8' }))
            return Math.round(Number(m[1]) * { KB: 1 / 1024, MB: 1, GB: 1024 }[m[2]])
        }
        const before = footprint()
        await evaluate(prelude + `
            window.filled = []
            for (let i = 0; i < 6; i++) {
                const t = await open()
                await run(t, "seq 1 30000 | sed 's/^/scrollback line /'; echo fill-don''e-" + i, 'fill-done-' + i)
                window.filled.push(t)
            }
            return true`)
        const full = footprint()
        await evaluate(prelude + `for (const t of window.filled) await app.closeTab(holder(t), false); window.filled = null; return true`)
        await sleep(5000)
        const closed = footprint()
        freed = full - before > 150 && closed - before < (full - before) / 2
        if (!freed) {
            console.log(`     (${before} MB, ${full} MB with six full tabs, ${closed} MB 5 s after closing them)`)
        }
    } catch (error) {
        console.log(`     (${error.message.split('\n')[0]})`)
    }
    console.log(`${freed ? 'ok  ' : 'FAIL'} Closing tabs gives their memory back`)
    failed += freed ? 0 : 1

    // Reloading the page keeps the terminals: the new page takes over their shells (Tabby's tab recovery).
    let reloaded = false
    try {
        await evaluate(prelude + `
            const t = await open()
            await run(t, 'export TT_RELOAD=kept-$((6*7))', 'TT_RELOAD')
            for (const tab of app.tabs.filter(x => x !== holder(t))) {
                await app.closeTab(tab, false)
            }
            await window.tabby.get(core.TabRecoveryService).saveTabs(app.tabs)
            setTimeout(() => location.reload(), 100)
            return true`)
        await sleep(1000)
        if (!await booted()) {
            throw new Error('the page did not come back')
        }
        reloaded = await evaluate(prelude + `
            // (the split tab comes back first, its terminal a moment later)
            const tab = await until(() => app.tabs.find(x => x.getAllTabs))
            const t = await ready(await until(() => tab.getAllTabs()[0]))
            return run(t, 'echo $TT_RELOAD', 'kept-42')`)
    } catch (error) {
        console.log(`     (${error.message.split('\n')[0]})`)
    }
    console.log(`${reloaded ? 'ok  ' : 'FAIL'} A terminal keeps its shell through a page reload`)
    failed += reloaded ? 0 : 1

    // Quit goes through Tabby's close flow and the app exits; started again, the window has the size it was quit at.
    let restored = false
    try {
        await evaluate(prelude + `
            const { LogicalSize, getCurrentWindow } = window.require('@tauri-apps/api/window')
            await getCurrentWindow().setSize(new LogicalSize(910, 620))
            for (const tab of [...app.tabs]) {
                await app.closeTab(tab, false)
            }
            window.tabby.get(core.PlatformService).quit()
            return true`)
        const exited = await Promise.race([new Promise(resolve => app.once('exit', () => resolve(true))), sleep(15000).then(() => false)])
        if (!exited) {
            throw new Error('the app did not quit')
        }
        app = start()
        if (!await booted()) {
            throw new Error('the app did not start again')
        }
        const size = await evaluate(`const w = window.require('@tauri-apps/api/window').getCurrentWindow(); const s = (await w.innerSize()).toLogical(await w.scaleFactor()); return [Math.round(s.width), Math.round(s.height)]`)
        restored = size[0] === 910 && size[1] === 620
        if (!restored) {
            console.log(`     (reopened at ${size.join('x')})`)
        }
    } catch (error) {
        console.log(`     (${error.message.split('\n')[0]})`)
    }
    console.log(`${restored ? 'ok  ' : 'FAIL'} Quit closes the app through Tabby, and it reopens at the size it was quit at`)
    failed += restored ? 0 : 1

    // First launch with Tabby installed (a fake Tabby folder here): "Import from Tabby" starts from a copy of its
    // settings, "Start fresh" from Tabby's defaults; Tabby's file stays as it was.
    const tabbyDir = path.join(dataDir, 'fake-tabby')
    fs.mkdirSync(tabbyDir)
    const tabbySettings = 'version: 1\nterminal:\n  fontSize: 23\n'
    fs.writeFileSync(path.join(tabbyDir, 'config.yaml'), tabbySettings)
    for (const answer of ['import', 'fresh']) {
        let ok = false
        try {
            app.kill('SIGKILL')
            await sleep(500)
            const dir = path.join(dataDir, `first-launch-${answer}`)
            fs.mkdirSync(dir)
            app = start(dir, { TABBI_IMPORT_FROM: tabbyDir, TABBI_IMPORT_ANSWER: answer })
            if (!await booted()) {
                throw new Error('the app did not start')
            }
            const fontSize = await evaluate(prelude + `const config = window.tabby.get(core.ConfigService); await until(() => config.store?.terminal); return config.store.terminal.fontSize`)
            ok = (answer === 'import' ? fontSize === 23 : fontSize !== 23) && fs.existsSync(path.join(dir, 'config.yaml'))
                && fs.readFileSync(path.join(tabbyDir, 'config.yaml'), 'utf-8') === tabbySettings
            if (!ok) {
                console.log(`     (font size ${fontSize})`)
            }
        } catch (error) {
            console.log(`     (${error.message.split('\n')[0]})`)
        }
        console.log(`${ok ? 'ok  ' : 'FAIL'} First launch, ${answer === 'import' ? '"Import from Tabby": starts from Tabby\'s settings' : '"Start fresh": starts from the defaults'}, Tabby's file unchanged`)
        failed += ok ? 0 : 1
    }

    // A later launch: a settings file with the welcome tab done and analytics left on (Tabby's default). Tabby's
    // analytics then starts with the app; it has to start without errors (and send nothing), and Settings must open.
    let returning = false
    try {
        app.kill('SIGKILL')
        await sleep(500)
        const dir = path.join(dataDir, 'returning')
        fs.mkdirSync(dir)
        fs.writeFileSync(path.join(dir, 'config.yaml'), 'version: 8\nenableWelcomeTab: false\nenableAnalytics: true\n')
        const errorsBefore = fs.readFileSync(log, 'utf-8').split('\n').filter(l => /^page (error|uncaught|unhandled)/.test(l)).length
        app = start(dir)
        if (!await booted()) {
            throw new Error('the app did not start')
        }
        const opened = await evaluate(prelude + `
            window.tabby.get(core.HomeBaseService)
            await zone.run(() => app.openNewTabRaw({ type: window.require('tabby-settings').SettingsTabComponent }))
            return until(() => app.activeTab?.constructor?.name === 'SettingsTabComponent')`)
        const errors = fs.readFileSync(log, 'utf-8').split('\n').filter(l => /^page (error|uncaught|unhandled)/.test(l)).slice(errorsBefore)
        returning = opened && errors.length === 0
        if (!returning) {
            console.log(`     (${errors.slice(0, 2).join(' | ').slice(0, 300)})`)
        }
    } catch (error) {
        console.log(`     (${error.message.split('\n')[0]})`)
    }
    console.log(`${returning ? 'ok  ' : 'FAIL'} A later launch (welcome tab done, analytics on) starts without errors, and Settings opens`)
    failed += returning ? 0 : 1
} catch (error) {
    if (error !== null) {
        console.log(`FAIL ${error.message}`)
        failed++
    }
} finally {
    app.kill('SIGKILL')
    sshd.kill('SIGKILL')
    agent.kill('SIGKILL')
    fs.rmSync(agentSocket, { force: true })
}
// The test's own web storage (see debug::web_store in src-tauri/src/debug.rs)
for (const [, store] of fs.readFileSync(log, 'utf-8').matchAll(/web storage ([0-9A-F-]{36})/g)) {
    fs.rmSync(path.join(os.homedir(), 'Library/WebKit/io.github.meanaverage.tabbi/WebsiteDataStore', store.toLowerCase()), { recursive: true, force: true })
}
const pageErrors = fs.readFileSync(log, 'utf-8').split('\n').filter(l => /^page (error|uncaught|unhandled)/.test(l))
if (pageErrors.length) {
    console.log(`\nErrors in the page:\n${pageErrors.join('\n')}`)
}
fs.rmSync(dataDir, { recursive: true, force: true })
console.log(failed ? `\n${failed} failed` : '\nall passed')
process.exit(failed ? 1 : 0)
