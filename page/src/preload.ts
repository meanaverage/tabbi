// Runs first: fonts and the loading screen's styles, then the module registry that stands in for Node's `require`
// (plugins are CommonJS bundles whose externals — Angular, rxjs, other plugins, Node modules — are `require`d).
import 'source-sans-pro/source-sans-pro.css'
import 'source-code-pro/source-code-pro.css'
import '@fortawesome/fontawesome-free/css/solid.css'
import '@fortawesome/fontawesome-free/css/brands.css'
import '@fortawesome/fontawesome-free/css/regular.css'
import '@fortawesome/fontawesome-free/css/fontawesome.css'
import '../../app/src/preload.scss'

// Required before other imports
import '../../web/polyfills.buffer'

const modules: Record<string, any> = {}

const customRequire = (name: string): any => {
    if (name in modules) {
        return modules[name]
    }
    if (name.startsWith('node:') && name.slice(5) in modules) {
        return modules[name.slice(5)]
    }
    throw new Error(`Attempted to require ${name}`)
}
customRequire['resolve'] = (() => null) as any
customRequire['main'] = { paths: [] }

async function loadScript (url: string): Promise<any> {
    const module = { exports: {} }
    window['module'] = module
    window['exports'] = module.exports
    await new Promise((resolve, reject) => {
        const e = document.createElement('script')
        e.onload = resolve
        e.onerror = () => reject(new Error(`Could not load ${url}`))
        e.src = url
        document.head.appendChild(e)
    })
    return module.exports
}

const TabbyHost = {
    registerModule: (name: string, mod: any): void => {
        modules[name] = mod
    },
    hasModule: (name: string): boolean => name in modules,
    loadPlugins: async (names: string[], progress?: (current: number, total: number) => void): Promise<any[]> => {
        const infos = await Promise.all(names.map(async name => {
            const pkg = await (await fetch(`plugins/${name}/package.json`)).json()
            return { ...pkg, url: `plugins/${name}/${pkg.main}` }
        }))
        const result: any[] = []
        for (const [i, info] of infos.entries()) {
            progress?.(i, infos.length)
            const module = await loadScript(info.url)
            TabbyHost.registerModule(info.name, module)
            TabbyHost.registerModule(`resources/builtin-plugins/${info.name}`, module)
            module.pluginName = info.name
            module.pluginInfo = info
            result.push(module)
        }
        progress?.(1, 1)
        return result
    },
}

Object.assign(window, {
    require: customRequire,
    module: { paths: [] },
    TabbyHost,
    __filename: '',
    __dirname: '',
    global: window,
    process: {
        env: {},
        argv: ['tabby'],
        platform: 'darwin',
        arch: 'arm64',
        pid: 0,
        on: () => null,
        once: () => null,
        removeListener: () => null,
        emitWarning: () => null,
        stdout: {},
        stderr: {},
        resourcesPath: 'resources',
        version: 'v20.0.0',
        versions: { modules: 0, node: '20.0.0' },
        nextTick: (f, ...args) => queueMicrotask(() => f(...args)),
        cwd: () => '/',
        hrtime: Object.assign(() => [0, 0], { bigint: () => BigInt(Math.round(performance.now() * 1e6)) }),
        umask: () => 0o22,
    },
})

// The page's warnings and errors also go to the app's stderr (src-tauri/src/debug.rs).
const forward = (level: string, args: any[]) => {
    try {
        const message = args.map(a => a instanceof Error ? `${a.name}: ${a.message}\n${a.stack ?? ""}` : typeof a === 'string' ? a : (() => { try { return JSON.stringify(a) } catch { return String(a) } })()).join(' ')
        window['__TAURI_INTERNALS__']?.invoke('console_log', { level, message: message.slice(0, 4000) })
    } catch { }
}
for (const level of ['warn', 'error']) {
    const original = console[level].bind(console)
    console[level] = (...args: any[]) => {
        original(...args)
        forward(level, args)
    }
}
window.addEventListener('error', e => forward('uncaught', [e.error ?? e.message]))
window.addEventListener('unhandledrejection', e => forward('unhandled rejection', [e.reason]))

// Node's setImmediate: the next macrotask, without setTimeout's clamping.
{
    const queue = new Map<number, () => void>()
    let next = 1
    const channel = new MessageChannel()
    channel.port1.onmessage = e => {
        const f = queue.get(e.data)
        queue.delete(e.data)
        f?.()
    }
    window['setImmediate'] = (f: (...a: any[]) => void, ...args: any[]) => {
        const id = next++
        queue.set(id, () => f(...args))
        channel.port2.postMessage(id)
        return id
    }
    window['clearImmediate'] = (id: number) => queue.delete(id)
}
