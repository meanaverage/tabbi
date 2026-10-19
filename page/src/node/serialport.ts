// `@serialport/bindings-cpp` for the page, over the Rust ports (src-tauri/src/serial.rs): what tabby-serial asks of
// it through `@serialport/stream` (its `@serialport/bindings-interface`): `autoDetect()` gives a binding that lists
// ports and opens one; an open port reads, writes, changes its baud rate and control lines, flushes and drains.
//
// Input comes as frames on a Tauri channel (`[0][bytes]` data, `[1][JSON]` end or error) and is acknowledged once a
// read has taken it, so a reader that stops reading holds the port up.
import { Channel, invoke } from '@tauri-apps/api/core'

export interface PortInfo {
    path: string
    manufacturer?: string
    serialNumber?: string
    pnpId?: string
    locationId?: string
    productId?: string
    vendorId?: string
}

export interface OpenOptions {
    path: string
    baudRate: number
    dataBits?: 5 | 6 | 7 | 8
    lock?: boolean
    stopBits?: 1 | 1.5 | 2
    parity?: string
    rtscts?: boolean
    xon?: boolean
    xoff?: boolean
    xany?: boolean
    hupcl?: boolean
}

export interface SetOptions { brk?: boolean, cts?: boolean, dsr?: boolean, dtr?: boolean, rts?: boolean }
export interface PortStatus { cts: boolean, dsr: boolean, dcd: boolean }

function serialError (error: any): Error & { code?: string, canceled?: boolean } {
    if (error instanceof Error) {
        return error
    }
    const e = new Error(error?.message ?? String(error)) as Error & { code?: string }
    if (error?.code) {
        e.code = error.code
    }
    return e
}

/** What a read that was waiting gets when the port is closed under it: the stream expects `canceled`. */
function canceled (): Error & { canceled: boolean } {
    return Object.assign(new Error('Port is not open'), { canceled: true })
}

// What the page before this one had is gone with it.
invoke('serial_reset').catch(() => null)

export class TauriPort {
    isOpen = false
    readonly openOptions: Required<OpenOptions>
    private id = 0
    private chunks: Uint8Array[] = []
    private ended = false
    private failure: Error|null = null
    private wake: (() => void)|null = null
    private decoder = new TextDecoder()

    private constructor (options: OpenOptions) {
        this.openOptions = {
            dataBits: 8, lock: true, stopBits: 1, parity: 'none', rtscts: false, xon: false, xoff: false, xany: false, hupcl: true,
            ...options,
        }
    }

    static async open (options: OpenOptions): Promise<TauriPort> {
        const port = new TauriPort(options)
        const channel = new Channel<ArrayBuffer>()
        channel.onmessage = buffer => port.onFrame(new Uint8Array(buffer))
        try {
            const opened = await invoke<{ id: number, baudRate: number }>('serial_open', {
                options: {
                    path: port.openOptions.path,
                    baudRate: port.openOptions.baudRate,
                    dataBits: port.openOptions.dataBits,
                    stopBits: port.openOptions.stopBits,
                    parity: port.openOptions.parity,
                    rtscts: port.openOptions.rtscts,
                    xon: port.openOptions.xon,
                    xoff: port.openOptions.xoff,
                    xany: port.openOptions.xany,
                    lock: port.openOptions.lock,
                },
                data: channel,
            })
            port.id = opened.id
            port.isOpen = true
            return port
        } catch (error) {
            throw serialError(error)
        }
    }

    private onFrame (frame: Uint8Array): void {
        if (frame[0] === 0) {
            if (frame.length > 1) {
                this.chunks.push(frame.subarray(1))
            }
        } else {
            const e = JSON.parse(this.decoder.decode(frame.subarray(1)))
            if (e.event === 'end') {
                this.ended = true
            } else if (e.event === 'error') {
                this.failure = serialError(e)
            }
        }
        this.wake?.()
    }

    /** At least one byte, as the interface promises, unless the input ended (then none). */
    async read (buffer: Buffer, offset: number, length: number): Promise<{ buffer: Buffer, bytesRead: number }> {
        while (!this.chunks.length) {
            if (!this.isOpen) {
                throw canceled()
            }
            if (this.failure) {
                throw this.failure
            }
            if (this.ended) {
                return { buffer, bytesRead: 0 }
            }
            await new Promise<void>(resolve => { this.wake = resolve })
            this.wake = null
        }
        let bytesRead = 0
        while (this.chunks.length && bytesRead < length) {
            const chunk = this.chunks[0]
            const take = Math.min(chunk.length, length - bytesRead)
            buffer.set(chunk.subarray(0, take), offset + bytesRead)
            bytesRead += take
            if (take === chunk.length) {
                this.chunks.shift()
            } else {
                this.chunks[0] = chunk.subarray(take)
            }
        }
        invoke('serial_ack', { id: this.id, length: bytesRead }).catch(() => null)
        return { buffer, bytesRead }
    }

    async write (buffer: Buffer): Promise<void> {
        if (!this.isOpen) {
            throw canceled()
        }
        const bytes = buffer.constructor === Uint8Array ? buffer : new Uint8Array(buffer.buffer, buffer.byteOffset, buffer.byteLength)
        try {
            await invoke('serial_write', bytes, { headers: { 'serial-id': String(this.id) } })
        } catch (error) {
            throw this.isOpen ? serialError(error) : canceled()
        }
    }

    async close (): Promise<void> {
        if (!this.isOpen) {
            throw new Error('Port is not open')
        }
        this.isOpen = false
        this.chunks = []
        this.wake?.()
        await invoke('serial_close', { id: this.id }).catch(() => null)
    }

    async update (options: { baudRate: number }): Promise<void> {
        await this.call('serial_update', { baudRate: options.baudRate })
        this.openOptions.baudRate = options.baudRate
    }

    async set (options: SetOptions): Promise<void> {
        await this.call('serial_set', { options })
    }

    get (): Promise<PortStatus> {
        return this.call<PortStatus>('serial_get')
    }

    getBaudRate (): Promise<{ baudRate: number }> {
        return this.call<{ baudRate: number }>('serial_get_baud_rate')
    }

    async flush (): Promise<void> {
        this.chunks = []
        await this.call('serial_flush')
    }

    drain (): Promise<void> {
        return this.call('serial_drain')
    }

    private async call<T = void> (command: string, args: Record<string, unknown> = {}): Promise<T> {
        if (!this.isOpen) {
            throw new Error('Port is not open')
        }
        try {
            return await invoke<T>(command, { id: this.id, ...args })
        } catch (error) {
            throw serialError(error)
        }
    }
}

export const TauriBinding = {
    list: (): Promise<PortInfo[]> => invoke<PortInfo[]>('serial_list').catch(error => { throw serialError(error) }),
    open: (options: OpenOptions): Promise<TauriPort> => TauriPort.open(options),
}

/** As `@serialport/bindings-cpp` exports it: the binding for this platform. */
export function autoDetect (): typeof TauriBinding {
    return TauriBinding
}
