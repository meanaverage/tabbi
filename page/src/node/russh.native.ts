// The native half of the `russh` npm package (its lib/native.d.ts), over the Rust port in src-tauri/src/ssh. The
// package's own JS wrapper (lib/index.js) is bundled with its `./native` pointing here (see webpack.config.mjs).
//
// Native objects are handles (ids) here, released when their JS object is garbage collected, or at once when they're
// consumed or closed. A client's callbacks come as frames on one channel (see src-tauri/src/ssh/outbox.rs); its data
// is acknowledged so Rust knows the page keeps up. Events other than data run in the zone the client was connected
// in, as a click would, so what they open (the host key prompt) gets rendered; data doesn't, for speed.
import { Channel, invoke } from '@tauri-apps/api/core'
import { createHash } from 'crypto-browserify'

/* eslint-disable @typescript-eslint/no-extraneous-class */

function toError (error: any): Error {
    if (error instanceof Error) {
        return error
    }
    return new Error(typeof error === 'string' ? error : JSON.stringify(error))
}

async function call<T> (cmd: string, args?: any, options?: any): Promise<T> {
    try {
        return await invoke<T>(cmd, args, options)
    } catch (error) {
        throw toError(error)
    }
}

function asBytes (data: Uint8Array): Uint8Array {
    return data.constructor === Uint8Array ? data : new Uint8Array(data.buffer, data.byteOffset, data.byteLength)
}

async function send (cmd: string, id: number, data: Uint8Array): Promise<void> {
    await call(cmd, asBytes(data), { headers: { 'ssh-id': String(id) } })
}

/** Writes one at a time, in order: concurrent commands could run in any order in Rust. */
class Writer {
    private last: Promise<void> = Promise.resolve()

    constructor (private cmd: string) { }

    write (id: number, data: Uint8Array): Promise<void> {
        const p = this.last.then(() => send(this.cmd, id, data))
        this.last = p.catch(() => null)
        return p
    }
}

const released = new FinalizationRegistry<number>(id => {
    invoke('ssh_release', { id }).catch(() => null)
})

class Handle {
    constructor (readonly handleId: number) {
        released.register(this, handleId, this)
    }

    /** Rust has let go of it already (consumed or closed). */
    protected forget (): void {
        released.unregister(this)
    }
}

// What the page before this one had is gone with it.
invoke('ssh_reset').catch(() => null)

export const AgentConnectionKind = { Pageant: 0, Pipe: 1, Unix: 2 } as const
export const HashAlgorithm = { Sha1: 0, Sha256: 1, Sha512: 2 } as const
export const SftpFileType = { Directory: 0, File: 1, Symlink: 2, Other: 3 } as const
export const OPEN_READ = 0x01
export const OPEN_WRITE = 0x02
export const OPEN_APPEND = 0x04
export const OPEN_CREATE = 0x08
export const OPEN_TRUNCATE = 0x10

// What russh 0.63.3 supports (`ssh_supported_algorithms`). tabby-ssh reads these while it loads, before a command
// could answer, so they're here; the check below says when Rust disagrees.
const SUPPORTED = {
    ciphers: ['clear', 'none', 'aes128-ctr', 'aes192-ctr', 'aes256-ctr', 'aes128-gcm@openssh.com', 'aes256-gcm@openssh.com', 'aes128-cbc', 'aes192-cbc', 'aes256-cbc', 'chacha20-poly1305@openssh.com'],
    kex: ['mlkem768x25519-sha256', 'curve25519-sha256', 'curve25519-sha256@libssh.org', 'diffie-hellman-group-exchange-sha1', 'diffie-hellman-group-exchange-sha256', 'diffie-hellman-group1-sha1', 'diffie-hellman-group14-sha1', 'diffie-hellman-group14-sha256', 'diffie-hellman-group15-sha512', 'diffie-hellman-group16-sha512', 'diffie-hellman-group17-sha512', 'diffie-hellman-group18-sha512', 'ecdh-sha2-nistp256', 'ecdh-sha2-nistp384', 'ecdh-sha2-nistp521', 'none'],
    macs: ['none', 'hmac-sha1', 'hmac-sha2-256', 'hmac-sha2-512', 'hmac-sha1-etm@openssh.com', 'hmac-sha2-256-etm@openssh.com', 'hmac-sha2-512-etm@openssh.com'],
    compression: ['none', 'zlib', 'zlib@openssh.com'],
    keys: ['ssh-dss', 'ecdsa-sha2-nistp256', 'ecdsa-sha2-nistp384', 'ecdsa-sha2-nistp521', 'ssh-ed25519', 'ssh-rsa', 'rsa-sha2-256', 'rsa-sha2-512', 'sk-ecdsa-sha2-nistp256@openssh.com', 'sk-ssh-ed25519@openssh.com'],
}
invoke<typeof SUPPORTED>('ssh_supported_algorithms').then(actual => {
    for (const [k, v] of Object.entries(actual)) {
        if (JSON.stringify(SUPPORTED[k]) !== JSON.stringify(v)) {
            console.warn(`russh: the page's list of supported ${k} differs from Rust's; update page/src/node/russh.native.ts:`, v)
        }
    }
}).catch(() => null)

export function supportedCiphers (): string[] { return [...SUPPORTED.ciphers] }
export function supportedKexAlgorithms (): string[] { return [...SUPPORTED.kex] }
export function supportedMacs (): string[] { return [...SUPPORTED.macs] }
export function supportedCompressionAlgorithms (): string[] { return [...SUPPORTED.compression] }
export function supportedKeyTypes (): string[] { return [...SUPPORTED.keys] }

export function isPageantRunning (): boolean {
    return false
}

interface PublicKeyInfo {
    algorithm: string
    fingerprint: string
    base64: string
}

export class SshPublicKey {
    constructor (private info: PublicKeyInfo) { }
    algorithm (): string { return this.info.algorithm }
    fingerprint (): string { return this.info.fingerprint }
    base64 (): string { return this.info.base64 }
    bytes (): Uint8Array { return Buffer.from(this.info.base64, 'base64') }
    /** As Rust takes it back. */
    toOpenSSH (): string { return `${this.info.algorithm} ${this.info.base64}` }
}

const PUBLIC_KEY_TYPES = ['ssh-ed25519', 'ssh-rsa', 'ssh-dss', 'ecdsa-sha2-nistp256', 'ecdsa-sha2-nistp384', 'ecdsa-sha2-nistp521', 'sk-ssh-ed25519@openssh.com', 'sk-ecdsa-sha2-nistp256@openssh.com']

/** An OpenSSH public key line (`ssh-ed25519 AAAA… comment`). Synchronous, as tabby-ssh calls it, so parsed here. */
export function parsePublicKey (data: string): SshPublicKey {
    const [type, base64] = data.trim().split(/\s+/)
    if (!PUBLIC_KEY_TYPES.includes(type) || !base64) {
        throw new Error('Keys(SshKey(AlgorithmUnknown))')
    }
    const blob = Buffer.from(base64, 'base64')
    const nameLength = blob.length >= 4 ? blob.readUInt32BE(0) : -1
    if (nameLength < 0 || blob.length < 4 + nameLength || blob.subarray(4, 4 + nameLength).toString() !== type) {
        throw new Error('Keys(SshKey(Encoding))')
    }
    const fingerprint = 'SHA256:' + createHash('sha256').update(blob).digest('base64').replace(/=+$/, '')
    return new SshPublicKey({ algorithm: type, fingerprint, base64: blob.toString('base64') })
}

export class SshKeyPair extends Handle {
    constructor (id: number, private publicKeyInfo: PublicKeyInfo) {
        super(id)
    }

    publicKey (): SshPublicKey {
        return new SshPublicKey(this.publicKeyInfo)
    }
}

export async function parseKey (data: string, password?: string|null): Promise<SshKeyPair> {
    const { id, publicKey } = await call<{ id: number, publicKey: PublicKeyInfo }>('ssh_parse_key', { data, password: password ?? null })
    return new SshKeyPair(id, publicKey)
}

export class AgentConnection {
    constructor (public kind: number, public path?: string) { }
    static new (kind: number, path?: string|null): AgentConnection {
        return new AgentConnection(kind, path ?? undefined)
    }
}

export class SshAgentStream extends Handle {
    private writer = new Writer('ssh_agent_write')

    write (data: Uint8Array): Promise<void> {
        return this.writer.write(this.handleId, data)
    }

    async close (): Promise<void> {
        this.forget()
        await call('ssh_agent_close', { id: this.handleId })
    }
}

export async function connectAgent (connection: AgentConnection, callback: (err: Error|null, data: Uint8Array) => any): Promise<SshAgentStream> {
    const data = new Channel<ArrayBuffer>()
    data.onmessage = buffer => callback(null, new Uint8Array(buffer))
    const id = await call<number>('ssh_agent_connect', { connection: { kind: connection.kind, path: connection.path ?? null }, data })
    return new SshAgentStream(id)
}

export class SftpFileMetadata {
    size: string
    uid?: number
    user?: string
    gid?: number
    group?: string
    permissions?: number
    atime?: number
    mtime?: number
}

interface RawMetadata {
    size: string
    uid: number|null
    user: string|null
    gid: number|null
    group: string|null
    permissions: number|null
    atime: number|null
    mtime: number|null
    type: number
}

function metadata (raw: RawMetadata): SftpFileMetadata {
    const md = new SftpFileMetadata()
    for (const k of ['size', 'uid', 'user', 'gid', 'group', 'permissions', 'atime', 'mtime']) {
        md[k] = raw[k] ?? undefined
    }
    // A method, as napi's; not an own enumerable property, so spreading the object leaves it out.
    Object.defineProperty(md, 'type', { value: () => raw.type, enumerable: false })
    return md
}

export class SftpDirEntry {
    constructor (public name: string, public type: number, private md: RawMetadata) { }
    metadata (): SftpFileMetadata { return metadata(this.md) }
}

export class SftpFile extends Handle {
    async read (n: number): Promise<Uint8Array> {
        return new Uint8Array(await call<ArrayBuffer>('ssh_sftp_file_read', { id: this.handleId, n }))
    }

    async writeAll (data: Uint8Array): Promise<void> {
        await send('ssh_sftp_file_write', this.handleId, data)
    }

    async flush (): Promise<void> {
        await call('ssh_sftp_file_flush', { id: this.handleId })
    }

    async shutdown (): Promise<void> {
        this.forget()
        await call('ssh_sftp_file_shutdown', { id: this.handleId })
    }
}

export class SftpChannel extends Handle {
    constructor (id: number, public channelId: number) {
        super(id)
    }

    private op<T> (op: Record<string, any>): Promise<T> {
        return call<T>('ssh_sftp', { id: this.handleId, op })
    }

    async readDir (path: string): Promise<SftpDirEntry[]> {
        const entries = await this.op<{ name: string, type: number, metadata: RawMetadata }[]>({ op: 'readDir', path })
        return entries.map(e => new SftpDirEntry(e.name, e.type, e.metadata))
    }

    async stat (path: string): Promise<SftpFileMetadata> {
        return metadata(await this.op<RawMetadata>({ op: 'stat', path }))
    }

    async createDir (path: string): Promise<void> { await this.op({ op: 'createDir', path }) }
    async readlink (path: string): Promise<string> { return this.op<string>({ op: 'readlink', path }) }
    async rename (src: string, dst: string): Promise<void> { await this.op({ op: 'rename', src, dst }) }
    async chmod (path: string, mode: number): Promise<void> { await this.op({ op: 'chmod', path, mode }) }
    async removeDir (path: string): Promise<void> { await this.op({ op: 'removeDir', path }) }
    async removeFile (path: string): Promise<void> { await this.op({ op: 'removeFile', path }) }

    async close (): Promise<void> {
        this.forget()
        await this.op({ op: 'close' })
    }

    async open (path: string, mode: number): Promise<SftpFile> {
        return new SftpFile(await this.op<number>({ op: 'open', path, mode }))
    }
}

export class SshChannel extends Handle {
    private writer = new Writer('ssh_channel_data')

    constructor (id: number, private channelId: number) {
        super(id)
    }

    async id (): Promise<number> {
        return this.channelId
    }

    private request (request: Record<string, any>): Promise<void> {
        return call('ssh_channel_request', { id: this.handleId, request })
    }

    requestPty (term: string, colWidth: number, rowHeight: number, pixWidth: number, pixHeight: number): Promise<void> {
        return this.request({ type: 'pty', term, colWidth, rowHeight, pixWidth, pixHeight })
    }

    requestShell (): Promise<void> { return this.request({ type: 'shell' }) }
    requestExec (command: string): Promise<void> { return this.request({ type: 'exec', command }) }

    requestX11Forwarding (singleConnection: boolean, x11Protocol: string, x11Cookie: string, screen: number): Promise<void> {
        return this.request({ type: 'x11', singleConnection, x11Protocol, x11Cookie, screen })
    }

    requestAgentForwarding (): Promise<void> { return this.request({ type: 'agentForwarding' }) }

    windowChange (colWidth: number, rowHeight: number, pixWidth: number, pixHeight: number): Promise<void> {
        return this.request({ type: 'windowChange', colWidth, rowHeight, pixWidth, pixHeight })
    }

    data (data: Uint8Array): Promise<void> {
        return this.writer.write(this.handleId, data)
    }

    eof (): Promise<void> { return this.request({ type: 'eof' }) }
    close (): Promise<void> { return this.request({ type: 'close' }) }
}

export class NewSshChannel extends Handle {
    async activate (): Promise<SshChannel> {
        const { id, channelId } = await call<{ id: number, channelId: number }>('ssh_channel_activate', { id: this.handleId })
        this.forget()
        return new SshChannel(id, channelId)
    }

    async activateSftp (): Promise<SftpChannel> {
        const { id, channelId } = await call<{ id: number, channelId: number }>('ssh_channel_activate_sftp', { id: this.handleId })
        this.forget()
        return new SftpChannel(id, channelId)
    }

    consumed (): void {
        this.forget()
    }
}

export class SshTransport extends Handle {
    private static async create (spec: Record<string, any>): Promise<SshTransport> {
        return new SshTransport(await call<number>('ssh_transport_new', { spec }))
    }

    static newSocket (address: string): Promise<SshTransport> {
        return SshTransport.create({ kind: 'socket', address })
    }

    static newCommand (command: string, args: string[]): Promise<SshTransport> {
        return SshTransport.create({ kind: 'command', command, args })
    }

    static async newSshChannel (channel: NewSshChannel): Promise<SshTransport> {
        const transport = await SshTransport.create({ kind: 'sshChannel', channel: channel.handleId })
        channel.consumed()
        return transport
    }

    static newSocksProxy (proxyHost: string, proxyPort: number, targetHost: string, targetPort: number): Promise<SshTransport> {
        return SshTransport.create({ kind: 'socksProxy', proxyHost, proxyPort, targetHost, targetPort })
    }

    static newHttpProxy (proxyHost: string, proxyPort: number, targetHost: string, targetPort: number): Promise<SshTransport> {
        return SshTransport.create({ kind: 'httpProxy', proxyHost, proxyPort, targetHost, targetPort })
    }

    consumed (): void {
        this.forget()
    }
}

export class KeyboardInteractiveAuthenticationPrompt {
    prompt: string
    echo: boolean
}

export class KeyboardInteractiveAuthenticationState {
    state: string
    partialSuccess: boolean
    name?: string
    instructions?: string
    remainingMethods: string[]
    private promptList: KeyboardInteractiveAuthenticationPrompt[]|null

    constructor (raw: any) {
        this.state = raw.state
        this.partialSuccess = raw.partialSuccess
        this.name = raw.name ?? undefined
        this.instructions = raw.instructions ?? undefined
        this.remainingMethods = raw.remainingMethods
        this.promptList = raw.prompts
    }

    prompts (): KeyboardInteractiveAuthenticationPrompt[]|null {
        return this.promptList
    }
}

export class SshAuthResult {
    success: boolean
    partialSuccess: boolean
    remainingMethods: string[]
}

export class SshClient extends Handle {
    private auth (username: string, method: Record<string, any>): Promise<any> {
        return call('ssh_authenticate', { id: this.handleId, username, method })
    }

    authenticateNone (username: string): Promise<SshAuthResult> {
        return this.auth(username, { method: 'none' })
    }

    authenticatePassword (username: string, password: string): Promise<SshAuthResult> {
        return this.auth(username, { method: 'password', password })
    }

    authenticatePublickey (username: string, key: SshKeyPair, hashAlgorithm?: number|null): Promise<SshAuthResult> {
        return this.auth(username, { method: 'publickey', key: key.handleId, hashAlgorithm: hashAlgorithm ?? null })
    }

    async startKeyboardInteractiveAuthentication (username: string): Promise<KeyboardInteractiveAuthenticationState> {
        return new KeyboardInteractiveAuthenticationState(await this.auth(username, { method: 'keyboardInteractiveStart' }))
    }

    async respondToKeyboardInteractiveAuthentication (responses: string[]): Promise<KeyboardInteractiveAuthenticationState> {
        return new KeyboardInteractiveAuthenticationState(await this.auth('', { method: 'keyboardInteractiveRespond', responses }))
    }

    authenticateAgent (username: string, connection: AgentConnection, specificKey?: SshPublicKey|null): Promise<SshAuthResult> {
        return this.auth(username, {
            method: 'agent',
            connection: { kind: connection.kind, path: connection.path ?? null },
            specificKey: specificKey?.toOpenSSH() ?? null,
        })
    }

    private op<T> (op: Record<string, any>): Promise<T> {
        return call<T>('ssh_client', { id: this.handleId, op })
    }

    async channelOpenSession (): Promise<NewSshChannel> {
        return new NewSshChannel(await this.op<number>({ op: 'channelOpenSession' }))
    }

    tcpipForward (address: string, port: number): Promise<number> {
        return this.op<number>({ op: 'tcpipForward', address, port })
    }

    async cancelTcpipForward (address: string, port: number): Promise<void> {
        await this.op({ op: 'cancelTcpipForward', address, port })
    }

    async channelOpenDirectTcpip (address: string, port: number, originatorAddress: string, originatorPort: number): Promise<NewSshChannel> {
        return new NewSshChannel(await this.op<number>({ op: 'channelOpenDirectTcpip', address, port, originatorAddress, originatorPort }))
    }

    async disconnect (): Promise<void> {
        await this.op({ op: 'disconnect' })
    }
}

type Callback<A extends any[]> = (err: Error|null, ...args: A) => any

const ACK_EVERY = 128 * 1024
const ACK_DELAY = 20

// eslint-disable-next-line @typescript-eslint/max-params
export async function connect (
    transport: SshTransport,
    cipherAlgos: string[]|undefined|null,
    kexAlgos: string[]|undefined|null,
    keyAlgos: string[]|undefined|null,
    macAlgos: string[]|undefined|null,
    compressionAlgos: string[]|undefined|null,
    connectionTimeoutSeconds: number|undefined|null,
    keepaliveIntervalSeconds: number|undefined|null,
    keepaliveMax: number,
    serverKeyCallback: (err: Error|null, key: SshPublicKey) => Promise<boolean>,
    dataCallback: Callback<[number, Uint8Array]>,
    extendedDataCallback: Callback<[number, number, Uint8Array]>,
    eofCallback: Callback<[number]>,
    closeCallback: Callback<[number]>,
    disconnectCallback: Callback<[Error|undefined|null]>,
    x11ChannelOpenCallback: Callback<[NewSshChannel, string, number]>,
    tcpipChannelOpenCallback: Callback<[NewSshChannel, string, number, string, number]>,
    agentChannelOpenCallback: Callback<[NewSshChannel]>,
    bannerCallback: Callback<[string]>,
): Promise<SshClient> {
    const zone = window['Zone']?.current
    const inZone = (f: () => void) => zone ? zone.run(f) : f()

    let clientId: number|null = null
    let unacked = 0
    let ackTimer: any = null
    const flushAck = () => {
        ackTimer = null
        if (clientId !== null && unacked > 0) {
            invoke('ssh_ack', { id: clientId, length: unacked }).catch(() => null)
            unacked = 0
        }
    }
    const ack = (length: number) => {
        unacked += length
        if (unacked >= ACK_EVERY) {
            flushAck()
        } else {
            ackTimer ??= setTimeout(flushAck, ACK_DELAY)
        }
    }

    const decoder = new TextDecoder()
    const events = new Channel<ArrayBuffer>()
    events.onmessage = buffer => {
        const frame = new Uint8Array(buffer)
        const view = new DataView(frame.buffer, frame.byteOffset, frame.byteLength)
        if (frame[0] === 0) {
            const data = frame.subarray(5)
            ack(data.length)
            dataCallback(null, view.getUint32(1), data)
        } else if (frame[0] === 1) {
            const data = frame.subarray(9)
            ack(data.length)
            extendedDataCallback(null, view.getUint32(1), view.getUint32(5), data)
        } else {
            const e = JSON.parse(decoder.decode(frame.subarray(1)))
            inZone(() => handleEvent(e))
        }
    }

    const handleEvent = (e: any) => {
        switch (e.event) {
            case 'serverKey':
                Promise.resolve()
                    .then(() => serverKeyCallback(null, new SshPublicKey(e.key)))
                    .then(accept => !!accept, () => false)
                    .then(accept => invoke('ssh_answer_server_key', { id: e.id, accept }))
                    .catch(() => null)
                break
            case 'eof':
                eofCallback(null, e.channel)
                break
            case 'close':
                closeCallback(null, e.channel)
                break
            case 'disconnect':
                disconnectCallback(null, e.error ? new Error(e.error) : null)
                break
            case 'banner':
                bannerCallback(null, e.banner)
                break
            case 'x11Open':
                x11ChannelOpenCallback(null, new NewSshChannel(e.channel), e.originatorAddress, e.originatorPort)
                break
            case 'tcpipOpen':
                tcpipChannelOpenCallback(null, new NewSshChannel(e.channel), e.connectedAddress, e.connectedPort, e.originatorAddress, e.originatorPort)
                break
            case 'agentOpen':
                agentChannelOpenCallback(null, new NewSshChannel(e.channel))
                break
        }
    }

    clientId = await call<number>('ssh_connect', {
        transport: transport.handleId,
        options: {
            cipherAlgos: cipherAlgos ?? null,
            kexAlgos: kexAlgos ?? null,
            keyAlgos: keyAlgos ?? null,
            macAlgos: macAlgos ?? null,
            compressionAlgos: compressionAlgos ?? null,
            connectionTimeoutSeconds: connectionTimeoutSeconds ?? null,
            keepaliveIntervalSeconds: keepaliveIntervalSeconds ?? null,
            keepaliveMax,
        },
        events,
    }).finally(() => transport.consumed())
    return new SshClient(clientId)
}

// The wrapper expects napi's CommonJS default object; this shim is compiled as an ES module in the AOT page.
export default {
    AgentConnectionKind, HashAlgorithm, SftpFileType,
    OPEN_READ, OPEN_WRITE, OPEN_APPEND, OPEN_CREATE, OPEN_TRUNCATE,
    supportedCiphers, supportedKexAlgorithms, supportedMacs, supportedCompressionAlgorithms, supportedKeyTypes,
    isPageantRunning, parsePublicKey, parseKey, connectAgent, connect,
    SshPublicKey, SshKeyPair, AgentConnection, SshAgentStream,
    SftpFileMetadata, SftpDirEntry, SftpFile, SftpChannel, SshChannel, NewSshChannel, SshTransport,
    KeyboardInteractiveAuthenticationPrompt, KeyboardInteractiveAuthenticationState, SshAuthResult, SshClient,
}
