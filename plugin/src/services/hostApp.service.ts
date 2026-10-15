import { Injectable, Injector, NgZone } from '@angular/core'
import { invoke } from '@tauri-apps/api/core'
import { getCurrentWindow } from '@tauri-apps/api/window'
import { HostAppService, Platform } from 'tabby-core'

@Injectable()
export class TauriHostApp extends HostAppService {
    get platform (): Platform {
        return { darwin: Platform.macOS, win32: Platform.Windows, linux: Platform.Linux }[process.platform] ?? Platform.Linux
    }

    get configPlatform (): Platform {
        return this.platform
    }

    constructor (
        injector: Injector,
        zone: NgZone,
    ) {
        super(injector)
        // The app menu's Preferences… (src-tauri/src/menu.rs)
        getCurrentWindow().listen('host:preferences-menu', () => zone.run(() => this.settingsUIRequest.next()))
        // Another window saved the settings (src-tauri/src/config.rs): Tabby loads them again.
        getCurrentWindow().listen('host:config-change', () => zone.run(() => this.configChangeBroadcast.next()))
    }

    newWindow (): void {
        invoke('window_new')
    }

    relaunch (): void {
        location.reload()
    }

    quit (): void {
        window.close()
    }
}
