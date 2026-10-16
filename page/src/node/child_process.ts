// Node's `child_process` for the page, over src-tauri/src/process.rs: spawn (with piped stdio), execFile and exec
// (buffered, with timeout and maxBuffer). Programs run directly, not through a shell, except for exec.
import { Channel, invoke } from '@tauri-apps/api/core'
import { EventEmitter } from 'events'
import { Readable, Writable } from 'stream-browserify'

// What each message on the output channel is (its first byte; see src-tauri/src/process.rs).
const STDOUT = 1
const STDERR = 2
const EXIT = 3

function nodeError (error: any, program: string): Error {
    const message = String(error?.message ?? error)
    const e = new Error(message) as any
    const code = /^(E[A-Z]+):/.exec(message)?.[1]
    if (code) {
        e.code = code
        e.errno = code
        e.syscall = `spawn ${program}`
        e.path = program
    }
    return e
}

export class ChildProcess extends EventEmitter {
    pid?: number
    exitCode: number|null = null
    signalCode: string|null = null
    killed = false
    stdin: Writable|null
    stdout: Readable
    stderr: Readable
    private id?: number
    private pendingWrites: Promise<void> = Promise.resolve()

    constructor (program: string, args: string[], options: any = {}) {
        super()
        const stdio = Array.isArray(options.stdio) ? options.stdio : [options.stdio ?? 'pipe']
        const wantStdin = (stdio[0] ?? 'pipe') === 'pipe'
        this.stdout = new Readable({ read () { } })
        this.stderr = new Readable({ read () { } })
        this.stdin = wantStdin ? new Writable({
            write: (chunk: Buffer, _encoding: string, callback: (e?: Error|null) => void) => {
                const bytes = typeof chunk === 'string' ? Buffer.from(chunk) : chunk
                this.pendingWrites = this.pendingWrites
                    .then(() => this.started)
                    .then(() => invoke('proc_write', new Uint8Array(bytes), { headers: { process: String(this.id) } }))
                this.pendingWrites.then(() => callback(), e => callback(nodeError(e, program)))
            },
            final: (callback: (e?: Error|null) => void) => {
                this.pendingWrites.then(() => this.started).then(() => invoke('proc_end_stdin', { id: this.id })).then(() => callback(), () => callback())
            },
        }) : null

        const output = new Channel<ArrayBuffer>()
        output.onmessage = buffer => {
            const bytes = new Uint8Array(buffer)
            const data = Buffer.from(bytes.buffer, bytes.byteOffset + 1, bytes.length - 1)
            if (bytes[0] === STDOUT || bytes[0] === STDERR) {
                ;(bytes[0] === STDOUT ? this.stdout : this.stderr).push(data)
            } else if (bytes[0] === EXIT) {
                const { code, signal } = JSON.parse(data.toString())
                this.exitCode = code
                this.signalCode = signal
                this.emit('exit', code, signal)
            } else {
                this.stdout.push(null)
                this.stderr.push(null)
                this.emit('close', this.exitCode, this.signalCode)
                this.started.then(() => invoke('proc_release', { id: this.id })).catch(() => null)
            }
        }

        let env: Record<string, string>|undefined
        if (options.env) {
            env = {}
            for (const [k, v] of Object.entries(options.env)) {
                if (v !== undefined && v !== null) {
                    env[k] = String(v)
                }
            }
        }
        this.started = invoke<{ id: number, pid: number }>('proc_spawn', {
            program, args: args.map(String), cwd: options.cwd ?? null, env: env ?? null, stdin: wantStdin, output,
        }).then(({ id, pid }) => {
            this.id = id
            this.pid = pid
            this.emit('spawn')
        }, error => {
            const e = nodeError(error, program)
            queueMicrotask(() => {
                this.emit('error', e)
                this.emit('close', -2, null)
            })
            throw e
        })
        this.started.catch(() => null)
    }

    private started: Promise<void>

    kill (signal: string = 'SIGTERM'): boolean {
        this.killed = true
        this.started.then(() => invoke('proc_kill', { id: this.id, signal })).catch(() => null)
        return true
    }

    ref (): this { return this }
    unref (): this { return this }
}

export function spawn (program: string, args?: any, options?: any): ChildProcess {
    if (!Array.isArray(args)) {
        options = args
        args = []
    }
    return new ChildProcess(program, args, options)
}

type ExecCallback = (error: any, stdout: any, stderr: any) => void

export function execFile (file: string, args?: any, options?: any, callback?: ExecCallback): ChildProcess {
    if (typeof args === 'function') {
        callback = args
        args = []
        options = {}
    } else if (typeof options === 'function') {
        callback = options
        options = Array.isArray(args) ? {} : args
        args = Array.isArray(args) ? args : []
    }
    options ??= {}
    const maxBuffer = options.maxBuffer ?? 1024 * 1024
    const encoding = options.encoding === 'buffer' ? null : options.encoding ?? 'utf8'
    const child = spawn(file, args, { ...options, stdio: ['pipe', 'pipe', 'pipe'] })
    const out: Buffer[] = []
    const err: Buffer[] = []
    let size = 0
    let failure: any = null
    let timer: any
    const finish = (code: number|null, signal: string|null) => {
        clearTimeout(timer)
        const stdout = Buffer.concat(out)
        const stderr = Buffer.concat(err)
        const decode = (b: Buffer) => encoding ? b.toString(encoding) : b
        if (!failure && (code !== 0 || signal)) {
            failure = new Error(`Command failed: ${[file, ...args].join(' ')}\n${stderr.toString()}`)
            failure.code = code
            failure.signal = signal
            failure.killed = child.killed
        }
        callback?.(failure, decode(stdout), decode(stderr))
    }
    const collect = (list: Buffer[]) => (data: Buffer) => {
        if (failure) {
            return
        }
        size += data.length
        if (size > maxBuffer) {
            failure = new RangeError('stdout maxBuffer length exceeded')
            failure.code = 'ERR_CHILD_PROCESS_STDIO_MAXBUFFER'
            child.kill('SIGKILL')
            return
        }
        list.push(data)
    }
    child.stdout.on('data', collect(out))
    child.stderr.on('data', collect(err))
    child.on('error', e => {
        failure = e
    })
    child.on('close', finish)
    child.stdin?.end(options.input ?? undefined)
    if (options.timeout) {
        timer = setTimeout(() => {
            failure = Object.assign(new Error(`Command timed out: ${file}`), { killed: true, signal: options.killSignal ?? 'SIGTERM' })
            child.kill(options.killSignal ?? 'SIGTERM')
        }, options.timeout)
    }
    return child
}

export function exec (command: string, options?: any, callback?: ExecCallback): ChildProcess {
    if (typeof options === 'function') {
        callback = options
        options = {}
    }
    return execFile(options?.shell ?? '/bin/sh', ['-c', command], options, callback)
}
