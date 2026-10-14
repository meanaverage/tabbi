import { Injectable, Injector } from '@angular/core'
import { HostAppService, Platform } from 'tabby-core'

@Injectable()
export class TauriHostApp extends HostAppService {
    get platform (): Platform {
        return { darwin: Platform.macOS, win32: Platform.Windows, linux: Platform.Linux }[process.platform] ?? Platform.Linux
    }

    get configPlatform (): Platform {
        return this.platform
    }

    // Needed for injector metadata
    // eslint-disable-next-line @typescript-eslint/no-useless-constructor
    constructor (
        injector: Injector,
    ) {
        super(injector)
    }

    newWindow (): void {
        throw new Error('Not implemented')
    }

    relaunch (): void {
        location.reload()
    }

    quit (): void {
        window.close()
    }
}
