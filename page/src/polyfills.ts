import '@angular/compiler'
// The Node modules plugins `require`, as far as the app needs them: browser ports where they exist, and the rest
// (fs, os, process…) backed by the Rust side. `window.tabbyTauriHost` is the host_info that entry.ts fetched first.
import * as tauriCore from '@tauri-apps/api/core'
import * as tauriEvent from '@tauri-apps/api/event'
import * as tauriWindow from '@tauri-apps/api/window'
import * as tauriWebviewWindow from '@tauri-apps/api/webviewWindow'
import * as tauriApp from '@tauri-apps/api/app'
import * as tauriMenu from '@tauri-apps/api/menu'
import * as tauriDpi from '@tauri-apps/api/dpi'
import * as tauriWebview from '@tauri-apps/api/webview'
import * as nodeFs from './node/fs'
import * as nodeNet from './node/net'
import * as keytar from './node/keytar'
import { syncCall } from './node/sync'

const register = (name: string, mod: any) => window['TabbyHost'].registerModule(name, mod)

register('@tauri-apps/api/core', tauriCore)
register('@tauri-apps/api/event', tauriEvent)
register('@tauri-apps/api/window', tauriWindow)
register('@tauri-apps/api/webviewWindow', tauriWebviewWindow)
register('@tauri-apps/api/app', tauriApp)
register('@tauri-apps/api/menu', tauriMenu)
register('@tauri-apps/api/dpi', tauriDpi)
register('@tauri-apps/api/webview', tauriWebview)

register('fs', nodeFs.fs)
register('fs/promises', nodeFs.fsPromises)
register('mz/fs', nodeFs.mzFs)
register('any-promise', Promise)
register('tls', {})
register('module', { globalPaths: [], prototype: { require: window['require'] } })
// Tabby's analytics (tabby-core's HomeBaseService, through the Node mixpanel library) would report this app's launches
// as Tabby's: requests go nowhere here. They have to be callable, though: with them missing, HomeBaseService fails to
// construct whenever analytics is on and the welcome tab off (any config past the first launch), and with Angular 21
// everything that needs it afterwards, Settings included, fails too.
const noRequest = () => {
    const request = { on: () => request, write: () => true, end: () => request, setTimeout: () => request, destroy: () => request }
    return request
}
register('http', { Agent: class {}, request: noRequest })
register('https', { Agent: class {}, request: noRequest })
register('querystring', {
    stringify: (params: Record<string, any>) => new URLSearchParams(Object.entries(params ?? {}).map(([k, v]) => [k, String(v)])).toString(),
    parse: (text: string) => Object.fromEntries(new URLSearchParams(text)),
})
register('tty', { isatty: () => false })
register('child_process', {})
register('readable-stream', require('stream-browserify'))
const host = (): any => window['tabbyTauriHost'] ?? {}
register('os', {
    arch: () => process.arch,
    platform: () => process.platform,
    homedir: () => process.env.HOME ?? '/',
    tmpdir: () => (process.env.TMPDIR ?? '/tmp').replace(/\/$/, ''),
    hostname: () => host().hostname ?? 'localhost',
    release: () => host().release ?? '0',
    type: () => ({ darwin: 'Darwin', linux: 'Linux', win32: 'Windows_NT' })[process.platform] ?? 'Darwin',
    userInfo: () => ({ username: process.env.USER ?? '', homedir: process.env.HOME ?? '/', shell: process.env.SHELL ?? null }),
    cpus: () => [],
    EOL: '\n',
    constants: { errno: {}, signals: {} },
})

// What the `tmp` package (bundled in Tabby's SSH plugin) touches when it loads: exit hooks and Node's constants.
process['addListener'] = () => process
process['binding'] = (name: string) => name === 'constants'
    ? { fs: require('constants-browserify'), os: { errno: { EBADF: 9, ENOENT: 2 } }, ...require('constants-browserify') }
    : {}

// process.kill: signal 0 asks whether the process exists, and has to answer right away.
process['kill'] = (pid: number, signal: string|number = 'SIGTERM'): boolean => {
    const result = syncCall<boolean>(signal === 0 ? 'process_alive' : 'process_kill', { pid, signal: String(signal) })
    if (!result) {
        const error = new Error(`kill ESRCH`) as any
        error.code = 'ESRCH'
        throw error
    }
    return true
}
register('buffer', { Buffer: window['Buffer'] })
register('crypto', {
    ...require('crypto-browserify'),
    getHashes: () => ['sha1', 'sha224', 'sha256', 'sha384', 'sha512', 'md5', 'rmd160'],
    timingSafeEqual: (a, b) => a.equals(b),
    randomUUID: () => crypto.randomUUID(),
})
register('dns', {})
const noChannel = () => ({ hasSubscribers: false, publish: () => null, subscribe: () => null, unsubscribe: () => null, bindStore: () => null, runStores: (_d, f, ...a) => f(...a) })
register('diagnostics_channel', {
    channel: noChannel,
    hasSubscribers: () => false,
    subscribe: () => null,
    unsubscribe: () => null,
    tracingChannel: () => ({ start: noChannel(), end: noChannel(), asyncStart: noChannel(), asyncEnd: noChannel(), error: noChannel(), hasSubscribers: false, traceSync: (f, _c, t, ...a) => f.apply(t, a), tracePromise: (f, _c, t, ...a) => f.apply(t, a), traceCallback: (f, _p, _c, t, ...a) => f.apply(t, a) }),
})
// Its index.js lists its lib folder at runtime; these are the parts Tabby's SSH plugin uses (SOCKS port forwards).
register('@luminati-io/socksv5', {
    ...require('../../tabby-ssh/node_modules/@luminati-io/socksv5/lib/server'),
    auth: {
        None: require('../../tabby-ssh/node_modules/@luminati-io/socksv5/lib/auth/None'),
        UserPassword: require('../../tabby-ssh/node_modules/@luminati-io/socksv5/lib/auth/UserPassword'),
    },
})
register('util', require('util/'))
register('keytar', keytar)
register('@serialport/bindings', {})
register('@serialport/bindings-cpp', {})
register('net', nodeNet)
// The npm package's own wrapper, over our stand-in for its native part (node/russh.native.ts, see webpack.config.mjs).
register('russh', require('russh'))
register('events', require('events'))
register('path', require('path-browserify'))
register('url', { ...require('url'), pathToFileURL: (x: string) => `file://${x}` })
register('zlib', { ...require('browserify-zlib'), constants: require('browserify-zlib') })
register('assert', Object.assign(require('assert'), { assertNotStrictEqual: () => true, notStrictEqual: () => true }))
register('constants', require('constants-browserify'))
register('stream', require('stream-browserify'))
register('readline', { ...require('readline-browserify'), cursorTo: () => null, clearLine: stream => stream.write('\r') })
register('electron', {})
register('@electron/remote', {})

register('@angular/core', require('@angular/core'))
// (Angular 16 and later; plugins built against them use it. Optional, so the page also builds on Tabby's Angular 15.)
try {
    register('@angular/core/rxjs-interop', require('@angular/core/rxjs-interop'))
} catch { }
register('@angular/cdk', require('@angular/cdk'))
register('@angular/cdk/clipboard', require('@angular/cdk/clipboard'))
register('@angular/cdk/drag-drop', require('@angular/cdk/drag-drop'))
register('@angular/compiler', require('@angular/compiler'))
register('@angular/common', require('@angular/common'))
register('@angular/forms', require('@angular/forms'))
register('@angular/platform-browser', require('@angular/platform-browser'))
register('@angular/platform-browser/animations', require('@angular/platform-browser/animations'))
register('@angular/platform-browser-dynamic', require('@angular/platform-browser-dynamic'))
register('@angular/animations', require('@angular/animations'))
register('@angular/localize', require('@angular/localize'))
register('@angular/localize/init', require('@angular/localize/init'))
register('@ng-bootstrap/ng-bootstrap', require('@ng-bootstrap/ng-bootstrap'))
register('ngx-toastr', require('ngx-toastr'))
register('deepmerge', require('deepmerge'))
register('rxjs', require('rxjs'))
register('rxjs/operators', require('rxjs'))
register('string_decoder', require('string_decoder'))
register('js-yaml', require('js-yaml'))
register('zone.js/dist/zone.js', require('zone.js'))
