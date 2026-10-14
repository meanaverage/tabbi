import * as fs from 'fs/promises'
import { Injectable } from '@angular/core'
import { invoke } from '@tauri-apps/api/core'
import { FileProvider } from 'tabby-core'

/** Files Tabby refers to as `file://…` (SSH keys, for one), as tabby-electron's ElectronFileProvider does. */
@Injectable()
export class TauriFileProvider extends FileProvider {
    name = 'Filesystem'

    async selectAndStoreFile (description: string): Promise<string> {
        const path = await invoke<string|null>('plugin:dialog|open', {
            options: { title: `Select ${description}`, multiple: false, directory: false },
        })
        if (!path) {
            throw new Error('canceled')
        }
        return `file://${path}`
    }

    async retrieveFile (key: string): Promise<Buffer> {
        if (key.startsWith('file://')) {
            key = key.substring('file://'.length)
        } else if (key.includes('://')) {
            throw new Error('Incorrect type')
        }
        return Buffer.from(await fs.readFile(key))
    }
}
