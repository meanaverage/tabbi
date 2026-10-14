import { Channel, invoke } from '@tauri-apps/api/core'
import { ChildProcess, PTYInterface, PTYProxy } from 'tabby-local'

// Each message from src-tauri/src/pty.rs starts with one of these, then the output, the exit code (u32,
// little-endian), or nothing.
const DATA = 1
const EXIT = 2
const CLOSE = 3

type PtyEvent = ['data', Uint8Array] | ['exit', number] | ['close']

/** The channel a PTY's output comes on, and a function that hands it to the proxy once there is one: until then
 * (output can come before the spawn call returns), what comes waits. */
function outputChannel (): [Channel<ArrayBuffer>, (proxy: TauriPTYProxy) => void] {
    let target: TauriPTYProxy|undefined
    const early: PtyEvent[] = []
    const channel = new Channel<ArrayBuffer>()
    channel.onmessage = buffer => {
        const bytes = new Uint8Array(buffer)
        let event: PtyEvent
        if (bytes[0] === DATA) {
            event = ['data', bytes.subarray(1)]
        } else if (bytes[0] === EXIT) {
            event = ['exit', new DataView(buffer).getUint32(1, true)]
        } else if (bytes[0] === CLOSE) {
            event = ['close']
        } else {
            return
        }
        target ? target.deliver(event) : early.push(event)
    }
    return [channel, proxy => {
        target = proxy
        for (const event of early.splice(0)) {
            proxy.deliver(event)
        }
    }]
}

export class TauriPTYInterface extends PTYInterface {
    async spawn (command: string, args: string[], options: any): Promise<PTYProxy> {
        const [output, connect] = outputChannel()
        const env: Record<string, string> = {}
        for (const [k, v] of Object.entries(options.env ?? {})) {
            if (v !== undefined && v !== null) {
                env[k] = String(v)
            }
        }
        const { id, pid } = await invoke<{ id: string, pid: number }>('pty_spawn', {
            command,
            args,
            cwd: options.cwd ?? null,
            env,
            cols: options.cols ?? 80,
            rows: options.rows ?? 30,
            output,
        })
        const proxy = new TauriPTYProxy(id, pid)
        connect(proxy)
        return proxy
    }

    async restore (id: string): Promise<PTYProxy|null> {
        const [output, connect] = outputChannel()
        const pid = await invoke<number|null>('pty_attach', { id, output })
        if (pid === null) {
            return null
        }
        const proxy = new TauriPTYProxy(id, pid)
        connect(proxy)
        return proxy
    }
}

export class TauriPTYProxy extends PTYProxy {
    private handlers = new Map<string, ((...args: any[]) => void)[]>()
    /** What came before anyone subscribed. */
    private waiting: PtyEvent[]|null = []
    private truePID: Promise<number>

    constructor (private id: string, private pid: number) {
        super()
        // The shell's own program when the shell only ran one (`exec`), as the Electron build does.
        this.truePID = (async () => {
            let pid = this.pid
            await new Promise(r => setTimeout(r, 2000))
            let processes = await this.getChildProcessesInternal(pid)
            while (pid && processes.length === 1 && processes[0].pid) {
                pid = processes[0].pid
                processes = await this.getChildProcessesInternal(pid)
            }
            return pid
        })().catch(() => this.pid)
    }

    deliver (event: PtyEvent): void {
        if (this.waiting) {
            this.waiting.push(event)
        } else {
            const [name, ...args] = event
            this.emit(name, ...args)
        }
    }

    emit (event: string, ...args: any[]): void {
        for (const handler of this.handlers.get(event) ?? []) {
            handler(...args)
        }
        if (event === 'close') {
            invoke('pty_release', { id: this.id })
        }
    }

    getID (): string {
        return this.id
    }

    async getPID (): Promise<number> {
        return this.pid
    }

    getTruePID (): Promise<number> {
        return this.truePID
    }

    subscribe (event: string, handler: (..._: any[]) => void): void {
        this.handlers.set(event, [...this.handlers.get(event) ?? [], handler])
        // Tabby subscribes to data, exit and close one after another: what waited goes out once it has.
        queueMicrotask(() => {
            const waiting = this.waiting
            this.waiting = null
            for (const [name, ...args] of waiting ?? []) {
                this.emit(name, ...args)
            }
        })
    }

    unsubscribeAll (): void {
        this.handlers.clear()
    }

    ackData (length: number): void {
        invoke('pty_ack', { id: this.id, length })
    }

    async resize (columns: number, rows: number): Promise<void> {
        await invoke('pty_resize', { id: this.id, cols: columns, rows })
    }

    async write (data: Buffer): Promise<void> {
        await invoke('pty_write', new Uint8Array(data), { headers: { 'pty-id': this.id } })
    }

    async kill (signal?: string): Promise<void> {
        await invoke('pty_kill', { id: this.id, signal: signal ?? null })
    }

    async getChildProcesses (): Promise<ChildProcess[]> {
        return this.getChildProcessesInternal(await this.getTruePID())
    }

    private async getChildProcessesInternal (pid: number): Promise<ChildProcess[]> {
        return invoke<ChildProcess[]>('process_children', { pid })
    }

    async getWorkingDirectory (): Promise<string|null> {
        return invoke<string|null>('process_cwd', { pid: await this.getTruePID() })
    }
}
