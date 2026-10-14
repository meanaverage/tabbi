// Node's `net` for the page, over the Rust sockets (src-tauri/src/net.rs): Socket (a Duplex stream: connect, write,
// end, destroy, timeouts, addresses) and Server (listen, close, address, 'connection'), as tabby-ssh's port forwards
// and X11 and tabby-telnet use them.
//
// Input comes as frames on a Tauri channel (`[0][bytes]` data, `[1][JSON]` end or error) and is acknowledged once
// the stream has taken it, so a reader that stops reading holds the peer up.
import { Channel, invoke } from '@tauri-apps/api/core'
import { EventEmitter } from 'events'
import { Duplex } from 'stream-browserify'

const ACK_EVERY = 128 * 1024
const ACK_DELAY = 20

function netError (error: any): Error {
    if (error instanceof Error) {
        return error
    }
    const e = new Error(error?.message ?? String(error)) as any
    if (error?.code) {
        e.code = error.code
        e.syscall = String(error.message ?? '').split(' ')[0] || undefined
    }
    return e
}

// What the page before this one had is gone with it.
invoke('net_reset').catch(() => null)

interface SocketInfo {
    id: number
    localAddress: string|null
    localPort: number|null
    remoteAddress: string|null
    remotePort: number|null
    remoteFamily: string|null
}

type Ready = { id: number } | { error: Error }

export class Socket extends Duplex {
    remoteAddress?: string
    remotePort?: number
    remoteFamily?: string
    localAddress?: string
    localPort?: number
    connecting = false
    bytesRead = 0
    bytesWritten = 0

    private id: number|null = null
    private ready: Promise<Ready>
    private setReady: (r: Ready) => void
    private unacked = 0
    private ackTimer: any = null
    private readBlocked = false
    private timeout = 0
    private timeoutTimer: any = null
    private decoder = new TextDecoder()

    constructor (options?: { allowHalfOpen?: boolean }) {
        super({ allowHalfOpen: options?.allowHalfOpen ?? false, autoDestroy: true, emitClose: true } as any)
        this.ready = new Promise(resolve => this.setReady = resolve)
    }

    /** A connection a Server accepted. */
    static accepted (info: SocketInfo): Socket {
        const socket = new Socket()
        socket.adopt(info)
        invoke('net_attach', { id: info.id, data: socket.channel() }).catch(e => socket.destroy(netError(e)))
        return socket
    }

    connect (...args: any[]): this {
        let options: any = {}
        let callback: (() => void)|undefined
        if (typeof args[args.length - 1] === 'function') {
            callback = args.pop()
        }
        if (typeof args[0] === 'object' && args[0] !== null) {
            options = args[0]
        } else if (typeof args[0] === 'string' && isNaN(Number(args[0]))) {
            options = { path: args[0] }
        } else {
            options = { port: Number(args[0]), host: args[1] }
        }
        if (callback) {
            this.once('connect', callback)
        }
        this.connecting = true
        invoke<SocketInfo>('net_connect', {
            host: options.path ? null : options.host ?? 'localhost',
            port: options.path ? null : Number(options.port),
            path: options.path ?? null,
            data: this.channel(),
        }).then(info => {
            if (this.destroyed) {
                invoke('net_destroy', { id: info.id }).catch(() => null)
                return
            }
            this.adopt(info)
            this.connecting = false
            this.touch()
            this.emit('connect')
            this.emit('ready')
        }, error => {
            this.connecting = false
            const e = netError(error)
            this.setReady({ error: e })
            this.destroy(e)
        })
        return this
    }

    private adopt (info: SocketInfo): void {
        this.id = info.id
        this.remoteAddress = info.remoteAddress ?? undefined
        this.remotePort = info.remotePort ?? undefined
        this.remoteFamily = info.remoteFamily ?? undefined
        this.localAddress = info.localAddress ?? undefined
        this.localPort = info.localPort ?? undefined
        this.setReady({ id: info.id })
    }

    private channel (): Channel<ArrayBuffer> {
        const channel = new Channel<ArrayBuffer>()
        channel.onmessage = buffer => {
            const frame = new Uint8Array(buffer)
            if (frame[0] === 0) {
                const data = Buffer.from(frame.buffer, frame.byteOffset + 1, frame.byteLength - 1)
                this.bytesRead += data.length
                this.touch()
                if (!this.push(data)) {
                    this.readBlocked = true
                }
                this.ack(data.length)
            } else {
                const e = JSON.parse(this.decoder.decode(frame.subarray(1)))
                if (e.event === 'end') {
                    this.push(null)
                } else if (e.event === 'error') {
                    this.destroy(netError(e))
                }
            }
        }
        return channel
    }

    private ack (length: number): void {
        this.unacked += length
        if (this.readBlocked) {
            return // until the reader wants more (_read)
        }
        if (this.unacked >= ACK_EVERY) {
            this.flushAck()
        } else {
            this.ackTimer ??= setTimeout(() => this.flushAck(), ACK_DELAY)
        }
    }

    private flushAck (): void {
        clearTimeout(this.ackTimer)
        this.ackTimer = null
        if (this.id !== null && this.unacked > 0) {
            invoke('net_ack', { id: this.id, length: this.unacked }).catch(() => null)
            this.unacked = 0
        }
    }

    _read (): void {
        if (this.readBlocked) {
            this.readBlocked = false
            this.flushAck()
        }
    }

    _write (chunk: any, encoding: string, callback: (error?: Error|null) => void): void {
        const data: Uint8Array = typeof chunk === 'string' ? Buffer.from(chunk, encoding as BufferEncoding) : chunk
        this.ready.then(async r => {
            if ('error' in r) {
                throw r.error
            }
            const bytes = data.constructor === Uint8Array ? data : new Uint8Array(data.buffer, data.byteOffset, data.byteLength)
            await invoke('net_write', bytes, { headers: { 'net-id': String(r.id) } })
            this.bytesWritten += data.length
            this.touch()
        }).then(() => callback(), e => callback(netError(e)))
    }

    _final (callback: (error?: Error|null) => void): void {
        this.ready.then(async r => {
            if ('id' in r) {
                await invoke('net_end', { id: r.id })
            }
        }).then(() => callback(), e => callback(netError(e)))
    }

    _destroy (error: Error|null, callback: (error: Error|null) => void): void {
        clearTimeout(this.timeoutTimer)
        clearTimeout(this.ackTimer)
        if (this.id !== null) {
            invoke('net_destroy', { id: this.id }).catch(() => null)
        }
        callback(error)
    }

    setTimeout (ms: number, callback?: () => void): this {
        this.timeout = ms
        if (callback) {
            this.once('timeout', callback)
        }
        this.touch()
        return this
    }

    private touch (): void {
        clearTimeout(this.timeoutTimer)
        if (this.timeout > 0 && !this.destroyed) {
            this.timeoutTimer = setTimeout(() => this.emit('timeout'), this.timeout)
        }
    }

    setNoDelay (noDelay = true): this {
        this.ready.then(r => 'id' in r && invoke('net_set_no_delay', { id: r.id, noDelay })).catch(() => null)
        return this
    }

    setKeepAlive (): this { return this }
    ref (): this { return this }
    unref (): this { return this }

    address (): { address?: string, port?: number, family?: string } {
        return { address: this.localAddress, port: this.localPort, family: this.remoteFamily }
    }

    get pending (): boolean {
        return this.id === null
    }

    get readyState (): string {
        if (this.connecting) {
            return 'opening'
        }
        return this.readable && this.writable ? 'open' : this.readable ? 'readOnly' : this.writable ? 'writeOnly' : 'closed'
    }
}

interface ServerInfo {
    id: number
    address: string
    port: number
    family: string
}

export class Server extends EventEmitter {
    listening = false
    maxConnections = Infinity
    private id: number|null = null
    private info: ServerInfo|null = null
    private connections = new Set<Socket>()
    private generation = 0
    private pending: Promise<void>|null = null
    private listenCallback?: () => void

    constructor (options?: any, listener?: (socket: Socket) => void) {
        super()
        if (typeof options === 'function') {
            listener = options
        }
        if (listener) {
            this.on('connection', listener)
        }
    }

    listen (...args: any[]): this {
        if (this.listening || this.pending) {
            throw Object.assign(new Error('Server is already listening.'), { code: 'ERR_SERVER_ALREADY_LISTEN' })
        }
        const generation = ++this.generation
        let callback: (() => void)|undefined
        if (typeof args[args.length - 1] === 'function') {
            callback = args.pop()
        }
        let port = 0
        let host: string|undefined
        if (typeof args[0] === 'object' && args[0] !== null) {
            port = Number(args[0].port ?? 0)
            host = args[0].host
        } else {
            port = Number(args[0] ?? 0)
            host = typeof args[1] === 'string' ? args[1] : undefined
        }
        const onListening = callback ? () => callback?.call(this) : undefined
        this.listenCallback = onListening
        if (onListening) {
            this.once('listening', onListening)
        }
        const events = new Channel<any>()
        events.onmessage = e => {
            if (generation !== this.generation) {
                if (e.event === 'connection') {
                    invoke('net_destroy', { id: e.socket.id }).catch(() => null)
                }
                return
            }
            if (e.event === 'connection') {
                const socket = Socket.accepted(e.socket)
                if (this.connections.size >= this.maxConnections) {
                    socket.destroy()
                    return
                }
                this.connections.add(socket)
                socket.once('close', () => this.connections.delete(socket))
                this.emit('connection', socket)
            } else if (e.event === 'error') {
                this.emit('error', netError(e))
            }
        }
        this.pending = invoke<ServerInfo>('net_listen', { host: host ?? null, port, events }).then(async info => {
            if (generation !== this.generation) {
                await invoke('net_close_server', { id: info.id })
                return
            }
            this.pending = null
            this.id = info.id
            this.info = info
            this.listening = true
            this.emit('listening')
        }, error => {
            if (generation === this.generation) {
                this.pending = null
                this.emit('error', netError(error))
            }
        }).finally(() => {
            if (onListening) this.removeListener('listening', onListening)
        })
        return this
    }

    close (callback?: (error?: Error) => void): this {
        ++this.generation
        if (this.listenCallback) this.removeListener('listening', this.listenCallback)
        this.listenCallback = undefined
        const wasListening = this.listening || this.pending !== null
        const closing = this.id !== null ? invoke('net_close_server', { id: this.id }) : this.pending ?? Promise.resolve()
        this.pending = null
        this.id = null
        this.info = null
        this.listening = false
        closing.then(() => {
            callback?.(wasListening ? undefined : Object.assign(new Error('Server is not running.'), { code: 'ERR_SERVER_NOT_RUNNING' }))
            if (wasListening) {
                this.emit('close')
            }
        }, error => callback?.(netError(error)))
        return this
    }

    address (): { address: string, port: number, family: string }|null {
        return this.info && this.listening ? { address: this.info.address, port: this.info.port, family: this.info.family } : null
    }

    getConnections (callback: (error: Error|null, count: number) => void): void {
        queueMicrotask(() => callback(null, this.connections.size))
    }

    ref (): this { return this }
    unref (): this { return this }
}

export function createServer (options?: any, listener?: (socket: Socket) => void): Server {
    return new Server(options, listener)
}

export function connect (...args: any[]): Socket {
    const options = typeof args[0] === 'object' && args[0] !== null ? args[0] : {}
    return new Socket(options).connect(...args)
}

export const createConnection = connect

export function isIPv4 (s: string): boolean {
    return /^(25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)(\.(25[0-5]|2[0-4]\d|1\d\d|[1-9]?\d)){3}$/.test(s)
}

export function isIPv6 (s: string): boolean {
    return s.includes(':') && /^[0-9a-fA-F:.]+(%.+)?$/.test(s)
}

export function isIP (s: string): number {
    return isIPv4(s) ? 4 : isIPv6(s) ? 6 : 0
}
