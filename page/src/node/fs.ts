// Node's `fs`, `fs/promises` and `mz/fs` for the page, over the Rust fs commands (src-tauri/src/fs.rs). The read-only
// synchronous calls Tabby makes go through sync.ts; the other synchronous ones throw, naming the call.
import { invoke } from '@tauri-apps/api/core'
import { nodeError, syncCall } from './sync'

interface RawStat {
    size: number
    mode: number
    uid: number
    gid: number
    mtimeMs: number
    atimeMs: number
    ctimeMs: number
    birthtimeMs: number
    isFile: boolean
    isDirectory: boolean
    isSymlink: boolean
}

class Stats {
    size: number
    mode: number
    uid: number
    gid: number
    mtimeMs: number
    atimeMs: number
    ctimeMs: number
    birthtimeMs: number
    mtime: Date
    atime: Date
    ctime: Date
    birthtime: Date
    private raw: RawStat

    constructor (raw: RawStat) {
        this.raw = raw
        Object.assign(this, {
            size: raw.size, mode: raw.mode, uid: raw.uid, gid: raw.gid,
            mtimeMs: raw.mtimeMs, atimeMs: raw.atimeMs, ctimeMs: raw.ctimeMs, birthtimeMs: raw.birthtimeMs,
        })
        this.mtime = new Date(raw.mtimeMs)
        this.atime = new Date(raw.atimeMs)
        this.ctime = new Date(raw.ctimeMs)
        this.birthtime = new Date(raw.birthtimeMs)
    }

    isFile (): boolean { return this.raw.isFile }
    isDirectory (): boolean { return this.raw.isDirectory }
    isSymbolicLink (): boolean { return this.raw.isSymlink }
    // The file type bits of the mode, as Node reads them (Tabby checks that the SSH agent's path is a socket).
    isSocket (): boolean { return (this.mode & 0o170000) === 0o140000 }
    isFIFO (): boolean { return (this.mode & 0o170000) === 0o010000 }
    isBlockDevice (): boolean { return (this.mode & 0o170000) === 0o060000 }
    isCharacterDevice (): boolean { return (this.mode & 0o170000) === 0o020000 }
}

class Dirent {
    constructor (public name: string, private raw: { isFile: boolean, isDirectory: boolean, isSymlink: boolean }) { }
    isFile (): boolean { return this.raw.isFile }
    isDirectory (): boolean { return this.raw.isDirectory }
    isSymbolicLink (): boolean { return this.raw.isSymlink }
}

async function call<T> (cmd: string, args?: any, options?: any): Promise<T> {
    try {
        return await invoke<T>(cmd, args, options)
    } catch (error) {
        throw nodeError(String((error as any)?.message ?? error))
    }
}

const toPath = (p: any): string => typeof p === 'string' ? p : p instanceof URL ? decodeURIComponent(p.pathname) : String(p)
const encodingOf = (options: any): string|null => typeof options === 'string' ? options : options?.encoding ?? null

function toBytes (data: any, encoding?: string|null): Uint8Array {
    if (typeof data === 'string') {
        return Buffer.from(data, (encoding ?? 'utf-8') as BufferEncoding)
    }
    if (data instanceof Uint8Array) {
        return data
    }
    if (data instanceof ArrayBuffer) {
        return new Uint8Array(data)
    }
    return Buffer.from(String(data))
}

const promises = {
    async stat (path: any): Promise<Stats> {
        return new Stats(await call<RawStat>('fs_stat', { path: toPath(path) }))
    },
    async lstat (path: any): Promise<Stats> {
        return new Stats(await call<RawStat>('fs_stat', { path: toPath(path), lstat: true }))
    },
    async access (path: any): Promise<void> {
        await call('fs_stat', { path: toPath(path) })
    },
    async exists (path: any): Promise<boolean> {
        try {
            await call('fs_stat', { path: toPath(path) })
            return true
        } catch {
            return false
        }
    },
    async readFile (path: any, options?: any): Promise<any> {
        const bytes = Buffer.from(await call<ArrayBuffer>('fs_read', { path: toPath(path) }))
        const encoding = encodingOf(options)
        return encoding ? bytes.toString(encoding as BufferEncoding) : bytes
    },
    async writeFile (path: any, data: any, options?: any): Promise<void> {
        const headers: Record<string, string> = { path: encodeURIComponent(toPath(path)) }
        if (options?.mode) {
            headers.mode = options.mode.toString(8)
        }
        if (options?.flag === 'a') {
            headers.append = '1'
        }
        await call('fs_write', toBytes(data, encodingOf(options)), { headers })
    },
    async appendFile (path: any, data: any, options?: any): Promise<void> {
        await promises.writeFile(path, data, { ...typeof options === 'string' ? { encoding: options } : options, flag: 'a' })
    },
    async readdir (path: any, options?: any): Promise<any[]> {
        const entries = await call<any[]>('fs_readdir', { path: toPath(path) })
        return options?.withFileTypes ? entries.map(e => new Dirent(e.name, e)) : entries.map(e => e.name)
    },
    async realpath (path: any): Promise<string> {
        return call<string>('fs_realpath', { path: toPath(path) })
    },
    async mkdir (path: any, options?: any): Promise<void> {
        await call('fs_mkdir', { path: toPath(path), recursive: !!options?.recursive })
    },
    async rm (path: any, options?: any): Promise<void> {
        await call('fs_remove', { path: toPath(path), recursive: !!options?.recursive, force: !!options?.force })
    },
    async unlink (path: any): Promise<void> {
        await call('fs_remove', { path: toPath(path) })
    },
    async rmdir (path: any, options?: any): Promise<void> {
        await call('fs_remove', { path: toPath(path), recursive: !!options?.recursive })
    },
    async rename (from: any, to: any): Promise<void> {
        await call('fs_rename', { from: toPath(from), to: toPath(to) })
    },
    async chmod (path: any, mode: number|string): Promise<void> {
        await call('fs_chmod', { path: toPath(path), mode: typeof mode === 'string' ? parseInt(mode, 8) : mode })
    },
    async copyFile (from: any, to: any): Promise<void> {
        await promises.writeFile(to, await promises.readFile(from))
    },
}

// Callback style: fs.stat(path, (err, stats) => …).
function callbackStyle (fn: (...args: any[]) => Promise<any>) {
    return (...args: any[]) => {
        const cb = typeof args[args.length - 1] === 'function' ? args.pop() : null
        const p = fn(...args)
        if (cb) {
            p.then(r => cb(null, r), e => cb(e))
            return undefined
        }
        return p
    }
}

const notSync = (name: string) => () => {
    throw new Error(`fs.${name} is not available in this app (no synchronous file access); use the async version`)
}

const constants = { F_OK: 0, R_OK: 4, W_OK: 2, X_OK: 1, O_RDONLY: 0, O_WRONLY: 1, O_RDWR: 2, O_CREAT: 0o100, O_TRUNC: 0o1000, O_APPEND: 0o2000, S_IFMT: 0o170000, S_IFREG: 0o100000, S_IFDIR: 0o40000, S_IFLNK: 0o120000 }

const callbackFs: Record<string, any> = { constants, Stats, Dirent, promises }
for (const [name, fn] of Object.entries(promises)) {
    callbackFs[name] = callbackStyle(fn)
}
// Node's own fs.exists calls back with just the boolean.
callbackFs.exists = (path: any, cb?: (exists: boolean) => void) => {
    const p = promises.exists(path)
    if (cb) {
        p.then(cb)
        return undefined
    }
    return p
}
for (const name of ['writeFileSync', 'mkdirSync', 'unlinkSync', 'rmSync', 'rmdirSync', 'appendFileSync', 'chmodSync', 'renameSync', 'copyFileSync']) {
    callbackFs[name] = notSync(name)
}

function statSync (path: any, options?: any, lstat = false): Stats|undefined {
    try {
        return new Stats(syncCall<RawStat>('stat', { path: toPath(path), lstat }))
    } catch (error) {
        if (options?.throwIfNoEntry === false && (error as any).code === 'ENOENT') {
            return undefined
        }
        throw error
    }
}
callbackFs.statSync = (path: any, options?: any) => statSync(path, options)
callbackFs.lstatSync = (path: any, options?: any) => statSync(path, options, true)
callbackFs.accessSync = (path: any) => {
    statSync(path)
}
callbackFs.existsSync = (path: any) => {
    try {
        statSync(path)
        return true
    } catch {
        return false
    }
}
callbackFs.readFileSync = (path: any, options?: any) => {
    const bytes = Buffer.from(syncCall<string>('read_file', { path: toPath(path) }), 'base64')
    const encoding = encodingOf(options)
    return encoding ? bytes.toString(encoding as BufferEncoding) : bytes
}
callbackFs.readdirSync = (path: any) => syncCall<string[]>('readdir', { path: toPath(path) })
callbackFs.realpathSync = Object.assign((path: any) => syncCall<string>('realpath', { path: toPath(path) }), {
    native: (path: any) => syncCall<string>('realpath', { path: toPath(path) }),
})

// mz/fs: the same functions, returning promises when there's no callback.
export const fs = callbackFs
export const fsPromises = { ...promises, constants }
export const mzFs = callbackFs
