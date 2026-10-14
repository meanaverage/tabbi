import { Injectable } from '@angular/core'
import { invoke } from '@tauri-apps/api/core'
import { HostAppService, Platform, TranslateService } from 'tabby-core'

import { ShellProvider, Shell } from 'tabby-local'

/** @hidden */
@Injectable()
export class MacOSDefaultShellProvider extends ShellProvider {
    private cachedShell?: Promise<string>

    constructor (
        private hostApp: HostAppService,
        private translate: TranslateService,
    ) {
        super()
    }

    async provide (): Promise<Shell[]> {
        if (this.hostApp.platform !== Platform.macOS) {
            return []
        }
        this.cachedShell ??= invoke<string>('default_shell')
        return [{
            id: 'default',
            name: this.translate.instant('OS default'),
            command: await this.cachedShell,
            args: ['--login'],
            hidden: true,
            env: {},
            shellType: 'unix',
        }]
    }
}
