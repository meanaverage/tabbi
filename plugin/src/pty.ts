import { Channel, invoke } from '@tauri-apps/api/core'
import { ChildProcess, PTYInterface, PTYProxy } from 'tabby-local'

type PtyEvent = { event: 'exit', code: number } | { event: 'close' }

interface Attachment {
    data: Channel<ArrayBuffer>
    events: Channel<PtyEvent>
}

function channels (proxy: () => TauriPTYProxy|undefined): Attachment {
    const data = new Channel<ArrayBuffer>()
    data.onmessage = buffer => proxy()?.emit('data', new Uint8Array(buffer))
    const events = new Channel<PtyEvent>()
    events.onmessage = e => {
        if (e.event === 'exit') {
            proxy()?.emit('exit', e.code)
        } else {
            proxy()?.emit('close')
        }
    }
    return { data, events }
}

export class TauriPTYInterface extends PTYInterface {
    async spawn (command: string, args: string[], options: any): Promise<PTYProxy> {
        let proxy: TauriPTYProxy|undefined
        const { data, events } = channels(() => proxy)
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
            data,
            events,
        })
        proxy = new TauriPTYProxy(id, pid)
        return proxy
    }

    async restore (id: string): Promise<PTYProxy|null> {
        let proxy: TauriPTYProxy|undefined
        const { data, events } = channels(() => proxy)
        const pid = await invoke<number|null>('pty_attach', { id, data, events })
        if (pid === null) {
            return null
        }
        proxy = new TauriPTYProxy(id, pid)
        return proxy
    }
}

export class TauriPTYProxy extends PTYProxy {
    private handlers = new Map<string, ((...args: any[]) => void)[]>()
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
