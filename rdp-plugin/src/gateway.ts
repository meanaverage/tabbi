import { domainToASCII } from 'url'
export interface Gateway {
    host: string
    port: number
}

export interface GatewayAccount {
    /** `user`, `DOMAIN\user` or `user@domain`. */
    username: string
    domain?: string
    password: string
}

/**
 * A gateway's host in one canonical form, so its trust key, its credential scope, its SNI and the way it is shown all
 * agree on it and an alternate spelling of the same gateway isn't taken for another one: an IPv6 literal compressed
 * (so `[0:0::1]` and `[::1]` are one), any other host as the name it resolves to (lower case, international letters as
 * punycode, one trailing dot dropped). Null for what names no host: an IPv6 literal that isn't one, an empty label
 * (`gw..example`, or a second trailing dot), a label over 63 characters or a name over 253, and a name with a
 * character no host name has (`#`, `?`, `@`, `%`, quotes, …), which the URL parser behind domainToASCII would otherwise
 * read as the end of the name or decode (`gw.example#x` as `gw.example`, `a%41` as `aa`) rather than refuse.
 */
function canonicalGatewayHost (bracketed: string | undefined, bare: string | undefined): string | null {
    if (bracketed !== undefined) {
        try {
            return new URL(`http://[${bracketed}]`).hostname.replace(/^\[|\]$/g, '')
        } catch {
            return null
        }
    }
    // Letters of any script go to IDNA, which refuses what isn't a name; of ASCII, only what a host name is made of.
    if (/[^A-Za-z0-9._-]/.test((bare ?? '').replace(/[^\x00-\x7f]/g, ''))) {
        return null
    }
    const host = domainToASCII((bare ?? '').replace(/\.$/, ''))
    return host && host.length <= 253 && host.split('.').every(label => label.length > 0 && label.length <= 63) ? host : null
}

/** A `host` or `host:port` setting as a gateway (443 by default), or null if it isn't one. The host is canonical. */
export function parseGateway (value: unknown): Gateway | null {
    // Text only, as specOf reads a gateway: String() of an object from the config runs its own conversions, which can throw.
    const m = /^\s*(?:\[([0-9a-f:.]+)\]|([^\s:/]+))(?::(\d{1,5}))?\s*$/i.exec(typeof value === 'string' ? value : '')
    const port = Number(m?.[3] ?? 443)
    const host = m && canonicalGatewayHost(m[1], m[2])
    return host && port > 0 && port < 65536 ? { host, port } : null
}

/** A gateway as a setting writes it: `host`, or `host:port` when that isn't 443 (an IPv6 address in brackets). */
export function formatGateway (gateway: Gateway): string {
    const host = gateway.host.includes(':') ? `[${gateway.host}]` : gateway.host
    return gateway.port === 443 ? host : `${host}:${gateway.port}`
}

/** The gateway refused the account (its HTTP sign-in, or its connection authorization policy). */
export class GatewaySignInError extends Error { }
