// Shared page-level checks for the bundled Tauri Remote Desktop plugin. No remote credentials or user files.
export default [
    ['RDP: bundled profiles, settings, and isolated IronRDP instances load', `
        const rd = await until(() => window.__remoteDesktop)
        const provider = window.tabby.get(core.ProfileProvider).find(p => p.id === 'rdp')
        if (!provider) throw new Error('RDP profile provider is missing')
        const templates = await provider.getBuiltinProfiles()
        if (!templates.some(p => p.isTemplate && p.options.port === 3389)) return false
        const one = await rd.desktop.ironrdp.create('warn')
        const two = await rd.desktop.ironrdp.create('warn')
        if (one === two || !one.Backend || !two.Backend) throw new Error('RDP instances were not isolated')
        await zone.run(() => rd.help.openSettings('remote-desktop'))
        await until(() => document.querySelector('remote-desktop-settings'))
        const root = document.querySelector('remote-desktop-settings')
        const updates = root.querySelector('[data-tab="updates"]')
        updates.click()
        await until(() => root.textContent.includes('included with this app'))
        await zone.run(() => app.closeTab(app.activeTab, false))
        return true
    `],
    ['RDP: native file handles preserve exclusive writes, identity, and read bounds', `
        const fs = require('fs'), path = require('path')
        const dir = await fs.promises.mkdtemp(path.join(require('os').tmpdir(), 'rdp-files-'))
        try {
            const file = path.join(dir, 'download')
            const out = await fs.promises.open(file, 'wx', 0o600)
            await out.appendFile(Buffer.from('remote desktop'))
            const stat = await out.stat()
            await out.close()
            if (!stat.isFile() || !stat.ino || stat.size !== 14) throw new Error('Missing file identity')
            let refused = false
            try { await fs.promises.open(file, 'wx') } catch (e) { refused = e.code === 'EEXIST' }
            if (!refused) throw new Error('Exclusive write replaced an existing file')
            const input = await fs.promises.open(file, fs.constants.O_RDONLY | fs.constants.O_NOFOLLOW | fs.constants.O_NONBLOCK)
            try {
                const buf = Buffer.alloc(7)
                const { bytesRead } = await input.read(buf, 0, 7, 7)
                if (bytesRead !== 7 || buf.toString() !== 'desktop') return false
            } finally { await input.close() }
            await fs.promises.copyFile(file, path.join(dir, 'copy'), fs.constants.COPYFILE_EXCL)
            return (await fs.promises.readFile(path.join(dir, 'copy'), 'utf8')) === 'remote desktop'
        } finally { await fs.promises.rm(dir, { recursive: true, force: true }) }
    `],
    ['RDP: private browser clipboard carries text, HTML, and pictures', `
        const canvas = document.createElement('canvas'); canvas.width = canvas.height = 2
        canvas.getContext('2d').fillRect(0, 0, 2, 2)
        const png = await new Promise(resolve => canvas.toBlob(resolve, 'image/png'))
        await navigator.clipboard.write([new ClipboardItem({
            'text/plain': new Blob(['RDP test'], {type:'text/plain'}),
            'text/html': new Blob(['<b>RDP test</b>'], {type:'text/html'}),
            'image/png': png,
        })])
        const [item] = await navigator.clipboard.read()
        if (await (await item.getType('text/plain')).text() !== 'RDP test') return false
        if (await (await item.getType('text/html')).text() !== '<b>RDP test</b>') return false
        return (await item.getType('image/png')).size === png.size
    `],
    ['RDP: subprocess bridge preserves input, errors, and timeout cleanup', `
        const cp = require('child_process')
        const executeChild = (program, args, options) => new Promise(resolve => cp.execFile(program, args, options, (error, stdout, stderr) => resolve({error, stdout, stderr})))
        const echoed = await executeChild('/bin/cat', [], {input:'desktop bridge', timeout:3000})
        if (echoed.error || echoed.stdout !== 'desktop bridge' || echoed.stderr !== '') throw new Error('process input/output failed')
        const missing = await executeChild('/nonexistent-rdp-test-program', [], {})
        if (missing.error?.code !== 'ENOENT') throw new Error('missing program error lost')
        const timed = await executeChild('/bin/sleep', ['10'], {timeout:100})
        if (!timed.error?.killed) throw new Error('timeout did not stop child')
        return true
    `],
    ['RDP: only authenticated local clients reach the proxy and close releases it', `
        const { invoke, Channel } = require('@tauri-apps/api/core')
        const events = new Channel(); const seen = []; events.onmessage = e => seen.push(e)
        const proxy = await invoke('rdp_proxy_start', { autologon: false, serverName: null, events })
        const connects = url => new Promise(resolve => {
            const ws = new WebSocket(url)
            ws.onopen = () => { ws.close(); resolve(true) }
            ws.onerror = () => resolve(false)
        })
        try {
            if (await connects(proxy.url.replace(proxy.token, 'wrong-token'))) return false
            if (!await connects(proxy.url)) return false
            if (seen.some(e => e.event === 'upstream')) return false
        } finally { await invoke('rdp_proxy_close', { id: proxy.id }) }
        return !await connects(proxy.url)
    `],
]
