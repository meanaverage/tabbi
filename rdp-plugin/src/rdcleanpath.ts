// RDCleanPath proxy for the IronRDP web client. The WASM client talks WebSocket to the proxy; the proxy asks
// openUpstream where each connection goes, opens that, forwards the X.224 request, terminates TLS, has checkCertificate decide on the server's certificate,
// hands the cert chain back to the client (for CredSSP), then relays. The proxy itself is in the app's Rust side
// (src-tauri/src/rdp.rs), so the RDP stream goes from the socket (or ssh) to the WebSocket without passing through the
// page on the way. So does a desktop behind an RD Gateway (src-tauri/src/rdp/gateway.rs): the page only says where
// the gateway is and with which account.
import { Channel, invoke } from '@tauri-apps/api/core'
import type { CertificateDetails } from './authorities'

/** An RD Gateway's TLS certificate, before the sign-in goes to it: throws to refuse it (see openThroughGateway). */
export type GatewayCertificateCheck = (fingerprint: string, valid: boolean, reason: string, details?: CertificateDetails) => void | Promise<void>

/** Where a proxy connection goes. The proxy opens it; SSH channels are opened on the page's SSH client natively. */
export type Upstream =
    /** A plain socket from this computer (LAN, VPN). */
    | { kind: 'tcp', host: string, port: number }
    /** A program whose stdin and stdout are the stream: `ssh -W host:port …`. */
    | { kind: 'process', program: string, args: string[] }
    /** host:port as the SSH server sees it (direct-tcpip), over a russh client (its native handle). */
    | { kind: 'sshTcp', client: number, host: string, port: number }
    /** A command on the SSH server (e.g. its own `ssh -W`), its stdin and stdout the stream. */
    | { kind: 'sshExec', client: number, command: string }
    | GatewayUpstream

/**
 * `target` through an RD Gateway ([MS-TSGU] over its WebSocket transport): `via` reaches the gateway (as the SSH host,
 * or this computer, does), the proxy signs in there with NTLM bound to its TLS certificate, and opens a channel to
 * `target` as the gateway sees it. The functions stay in the page (JSON leaves them out): the proxy asks
 * `checkCertificate` about the gateway's certificate before the account is used, and says when it signed in or was
 * refused.
 */
export interface GatewayUpstream {
    kind: 'gateway'
    via: Upstream
    gateway: { host: string, port: number }
    /** `user`, `DOMAIN\user` or `user@domain`. */
    account: { username: string, domain?: string, password: string }
    target: { host: string, port: number }
    /** A name for this computer, as the gateway's logs will have it. */
    clientName: string
    checkCertificate: GatewayCertificateCheck
    signedIn?: () => void
    /** The gateway refused the account (its sign-in, or its connection authorization policy). */
    refused?: (message: string) => void
}

/** Where the next connection goes, asked for each one (the SSH connection may have been reopened since). */
export type UpstreamFactory = (destination: string, signal?: AbortSignal) => Upstream | Promise<Upstream>

/**
 * Decides on the server's certificate, given its SHA-256 fingerprint (`AB:CD:…`); throws to refuse it. Upstream TLS
 * accepts any certificate (RDP servers are self-signed), so this is what stands between the client and an impostor.
 * It runs before the client hears back from the proxy, and so before CredSSP: no credentials have been sent yet.
 */
export type CertificateCheck = (fingerprint: string, valid: boolean, reason: string, details?: CertificateDetails) => void | Promise<void>

/** A SHA-256 fingerprint as `AB:CD:…` (openssl and Node print it so), from any hex spelling; '' if it isn't one. */
export function normalizeFingerprint (value: string): string {
    const hex = String(value ?? '').replace(/[\s:]/g, '').toUpperCase()
    return /^[0-9A-F]{64}$/.test(hex) ? hex.match(/../g)!.join(':') : ''
}

export interface ServerFlow {
    room (): boolean
    spend (bytes: number): void
    refund (bytes: number): void
    whenRoom (): Promise<void>
}
export interface RDCleanPathProxy extends ServerFlow {
    url: string
    token: string
    /**
     * Why the last connection failed before the relay was up (the server didn't answer, refused, offered no TLS, sent
     * no certificate): the client only sees a generic RDCleanPath error then.
     */
    failure: string | null
    /**
     * RDP bytes relayed since the proxy started (after TLS, before SSH): from the server, and to it. Read as of the
     * last read (each read asks for newer numbers), which is what a once-a-second display needs.
     */
    stats: { bytesIn: number, bytesOut: number }
    close (): void
}

export interface RDCleanPathOptions {
    /** Set INFO_AUTOLOGON in the client's Client Info PDU (xrdp). */
    autologon?: boolean
    /**
     * Called when the server chose TLS without Network Level Authentication, before anything more is sent to it: the
     * client would go on to send the password as it is. Throws to refuse. Without it, the proxy goes on.
     */
    withoutNla: () => void | Promise<void>
    withoutNlaCertificate?: (fingerprint: string) => void | Promise<void>
    serverName?: string
}

type ProxyEvent =
    | { event: 'log', message: string }
    | { event: 'drained', bytes: number }
    | { event: 'upstream', request: number }
    | { event: 'certificate', request: number, fingerprint: string, valid: boolean, reason: string, withoutNla: boolean }
    | { event: 'withoutNla', request: number }
    | { event: 'gatewayCertificate', request: number, upstream: number, fingerprint: string, valid: boolean }
    | { event: 'gatewaySignedIn', upstream: number }
    | { event: 'gatewayRefused', upstream: number, message: string }
    | { event: 'failure', message: string | null }

export async function startRDCleanPathProxy (openUpstream: UpstreamFactory, checkCertificate: CertificateCheck, log: (msg: string) => void, options: RDCleanPathOptions): Promise<RDCleanPathProxy> {
    if (typeof options.withoutNla !== 'function') { throw new Error('Missing non-NLA consent check') }
    const abort = new AbortController()
    let spent = 0
    let failure: string | null = null
    const stats = { bytesIn: 0, bytesOut: 0 }
    let reading = false
    let closed = false
    // Events can only come once a client connects, which needs the URL: `id` is set by then.
    let id = 0
    const answer = (request: number, ask: () => unknown) => {
        Promise.resolve()
            .then(ask)
            .then(value => ({ value: value ?? null, error: null }), (error: any) => ({ value: null, error: String(error?.message ?? error) || 'refused' }))
            .then(({ value, error }) => invoke('rdp_proxy_answer', { id, request, value, error }))
            .catch(() => null)
    }
    // A gateway connection's page-side callbacks, by the upstream request that described it (at most a few at a time).
    const gateways = new Map<number, GatewayUpstream>()
    const upstreamFor = (request: number) => async () => {
        const upstream = await openUpstream('', abort.signal)
        if (upstream.kind === 'gateway') {
            gateways.set(request, upstream)
            while (gateways.size > 8) {
                gateways.delete(gateways.keys().next().value!)
            }
        }
        return upstream
    }
    const events = new Channel<ProxyEvent>()
    events.onmessage = e => {
        switch (e.event) {
            case 'drained':
                spent = Math.max(0, spent - e.bytes)
                break
            case 'log':
                log(e.message)
                break
            case 'failure':
                failure = e.message
                break
            case 'upstream':
                answer(e.request, upstreamFor(e.request))
                break
            case 'certificate':
                answer(e.request, async () => { await checkCertificate(e.fingerprint, e.valid, e.reason); if (e.withoutNla) { await options.withoutNlaCertificate?.(e.fingerprint) } })
                break
            case 'withoutNla':
                answer(e.request, async () => { await options.withoutNla() })
                break
            case 'gatewayCertificate': {
                const gateway = gateways.get(e.upstream)
                answer(e.request, async () => {
                    if (!gateway) {
                        throw new Error('the gateway connection is gone')
                    }
                    await gateway.checkCertificate(e.fingerprint, e.valid, e.valid ? '' : 'Not verified by this computer’s certificate authorities')
                })
                break
            }
            case 'gatewaySignedIn':
                gateways.get(e.upstream)?.signedIn?.()
                gateways.delete(e.upstream)
                break
            case 'gatewayRefused':
                gateways.get(e.upstream)?.refused?.(e.message)
                gateways.delete(e.upstream)
                break
        }
    }
    const started = await invoke<{ id: number, url: string, token: string }>('rdp_proxy_start', { autologon: !!options.autologon, serverName: options.serverName ?? null, events })
    id = started.id
    const refresh = async () => {
        if (closed) { throw new Error('RDP proxy is closed') }
        Object.assign(stats, await invoke('rdp_proxy_stats', { id }))
    }
    return {
        room: () => !closed && spent < 64 * 1024 * 1024,
        spend: bytes => { spent += bytes },
        refund: bytes => { spent = Math.max(0, spent - bytes) },
        whenRoom: async () => {
            while (spent >= 64 * 1024 * 1024) {
                await refresh()
                if (spent >= 64 * 1024 * 1024) { await new Promise(resolve => setTimeout(resolve, 25)) }
            }
            if (closed) { throw new Error('RDP proxy is closed') }
        },
        url: started.url,
        token: started.token,
        get failure () { return failure },
        get stats () {
            if (!reading && !closed) {
                reading = true
                invoke<{ bytesIn: number, bytesOut: number }>('rdp_proxy_stats', { id })
                    .then(now => Object.assign(stats, now), () => null)
                    .finally(() => { reading = false })
            }
            return stats
        },
        close: () => {
            closed = true
            abort.abort()
            gateways.clear()
            invoke('rdp_proxy_close', { id }).catch(() => null)
        },
    }
}

export function withoutToken (text: string, proxy: { token: string } | null | undefined): string {
    return proxy?.token ? text.split(proxy.token).join('…') : text
}
