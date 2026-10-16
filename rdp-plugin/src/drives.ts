import { invoke } from '@tauri-apps/api/core'
import * as path from 'path'
import type { ServerFlow } from './rdcleanpath'

/** A local folder shared with remote desktops as a drive (`remoteDesktop.sharedFolders`). */
export interface SharedFolder {
    /** The folder on this computer, absolute. */
    path: string
    /** The drive's name on the remote desktop (`\\tsclient\<name>`, "<name> on <this computer>" in Explorer). */
    name: string
    /** The remote can read but not change, add or remove anything. */
    readOnly: boolean
}

/** The folders configured, tidied: strings and booleans where expected, no duplicate paths, each with a name. */
export function sharedFolders (store: any): SharedFolder[] {
    const list = Array.isArray(store?.sharedFolders) ? store.sharedFolders : []
    const folders: SharedFolder[] = []
    for (const entry of list) {
        const folder = typeof entry?.path === 'string' ? entry.path.trim() : ''
        if (!folder || folders.some(f => f.path === folder)) {
            continue
        }
        folders.push({
            path: folder,
            name: driveName(typeof entry.name === 'string' && entry.name.trim() ? entry.name : path.basename(folder) || folder, folders),
            readOnly: entry.readOnly === true,
        })
    }
    return folders
}

/**
 * A drive name the remote can show: no characters a Windows file name can't have, not empty, and not another
 * shared folder's name (a number is added).
 */
export function driveName (wanted: string, others: { name: string }[]): string {
    const base = wanted.replace(/[\\/:*?"<>|\u0000-\u001f]/g, ' ').replace(/\s+/g, ' ').trim().slice(0, 64) || 'Folder'
    let name = base
    for (let n = 2; others.some(o => o.name.toLowerCase() === name.toLowerCase()); n++) {
        name = `${base} ${n}`
    }
    return name
}

/** What a shared drive's file operations see and return (iron-remote-desktop-rdp's `DriveFs`). */
interface DriveStat {
    dir: boolean
    size: number
    created: number
    accessed: number
    modified: number
    readonly: boolean
}

interface DriveOpenMode {
    write: boolean
    create: boolean
    exclusive: boolean
    truncate: boolean
}

/**
 * The native side's errors come as `CODE: message` (Node's errno names); IronRDP maps the `code` to the NT status the
 * server gets.
 */
function nodeError (e: unknown): Error {
    const message = String((e as any)?.message ?? e)
    const m = /^([A-Z][A-Z0-9]+): ([^]*)$/.exec(message)
    const error = new Error(m ? m[2] : message) as Error & { code?: string }
    error.code = m?.[1]
    return error
}

/**
 * The shared folders as the remote desktop's file system (MS-RDPEFS drive redirection). The file operations are
 * native (src-tauri/src/rdp/drives.rs): off the page's thread, one request at a time per drive, each path resolved
 * inside its folder (`..` and links that lead out of it don't resolve; only regular files and folders are served),
 * and read-only folders refused anything that writes, there rather than here. This object only forwards IronRDP's
 * calls; file contents cross once, as bytes (IronRDP itself runs in the page).
 */
export class SharedDrives {
    readonly drives: { id: number, name: string }[]
    private session: Promise<number>
    private closed = false

    constructor (folders: SharedFolder[], private log: (message: string) => void = () => null, private flow?: ServerFlow) {
        // RDPDR device ids are the session's: the audio device, when there is one, takes 0 in IronRDP.
        this.drives = folders.map((folder, i) => ({ id: i + 1, name: folder.name }))
        this.session = invoke<number>('rdp_drives_open', { folders: folders.map((f, i) => ({ id: i + 1, path: f.path, name: f.name, readOnly: f.readOnly })) })
        this.session.catch(e => this.log(`drives: ${e?.message ?? e}`))
    }

    /** Closes whatever the server left open (the connection ended). */
    dispose (): void {
        if (this.closed) {
            return
        }
        this.closed = true
        this.session.then(drives => invoke('rdp_drives_close', { drives })).catch(() => null)
    }

    private async call<T> (command: string, args: Record<string, unknown>): Promise<T> {
        const drives = await this.session
        try {
            return await invoke<T>(command, { drives, ...args })
        } catch (e) {
            throw nodeError(e)
        }
    }

    stat (deviceId: number, remote: string): Promise<DriveStat | null> {
        return this.call('rdp_drive_stat', { device: deviceId, path: remote })
    }

    list (deviceId: number, remote: string): Promise<string[]> {
        return this.call('rdp_drive_list', { device: deviceId, path: remote })
    }

    open (deviceId: number, remote: string, mode: DriveOpenMode): Promise<number> {
        return this.call('rdp_drive_open', { device: deviceId, path: remote, mode })
    }

    async read (handle: number, offset: number, length: number): Promise<Uint8Array> {
        while (this.flow && !this.flow.room()) { await this.flow.whenRoom() }
        if (!Number.isSafeInteger(length) || length < 0 || length > 16 * 1024 * 1024) { throw new Error('Invalid shared-drive read size') }
        const reserved = Math.max(length, 64 * 1024)
        this.flow?.spend(reserved)
        try {
            const bytes = new Uint8Array(await this.call<ArrayBuffer>('rdp_drive_read', { handle, offset, length }))
            this.flow?.refund(reserved - bytes.length)
            return bytes
        } catch (error) {
            this.flow?.refund(reserved)
            throw error
        }
    }

    async write (handle: number, offset: number, data: Uint8Array): Promise<number> {
        const drives = await this.session
        try {
            // The bytes as the request's body, the rest in headers: no JSON for the data.
            return await invoke<number>('rdp_drive_write', data, { headers: { drives: String(drives), handle: String(handle), offset: String(offset) } })
        } catch (e) {
            throw nodeError(e)
        }
    }

    truncate (handle: number, size: number): Promise<void> {
        return this.call('rdp_drive_truncate', { handle, size })
    }

    close (handle: number): Promise<void> {
        return this.call('rdp_drive_close', { handle })
    }

    mkdir (deviceId: number, remote: string): Promise<void> {
        return this.call('rdp_drive_mkdir', { device: deviceId, path: remote })
    }

    remove (deviceId: number, remote: string): Promise<void> {
        return this.call('rdp_drive_remove', { device: deviceId, path: remote })
    }

    rename (deviceId: number, from: string, to: string, replace: boolean): Promise<void> {
        return this.call('rdp_drive_rename', { device: deviceId, from, to, replace })
    }

    async volume (deviceId: number): Promise<{ total: number, free: number, label: string }> {
        try {
            return await this.call('rdp_drive_volume', { device: deviceId })
        } catch (e: any) {
            const name = this.drives.find(d => d.id === deviceId)?.name ?? String(deviceId)
            this.log(`drives: ${name}: no volume information: ${e?.message ?? e}`)
            return { total: 0, free: 0, label: name }
        }
    }
}
