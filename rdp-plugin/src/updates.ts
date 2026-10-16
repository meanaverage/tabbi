// This plugin is bundled with the host. The Electron/npm updater cannot replace a Tauri build.
import { Injectable } from '@angular/core'
import { Subject } from 'rxjs'
import { PlatformService } from 'tabby-core'
export function installedVersion (): string { return require('../package.json').version }
@Injectable({ providedIn: 'root' })
export class UpdateCheck {
    readonly available: string | null = null
    readonly changed$ = new Subject<void>()
    constructor (private platform: PlatformService) { }
    start (): void { }
    refresh (): void { }
    upgrade (): void { }
    whatsNew (): void { this.platform.openExternal('https://github.com/meanaverage/tabby-rdp/releases') }
}
