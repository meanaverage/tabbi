import { NgModule } from '@angular/core'
import { CommonModule } from '@angular/common'
import { invoke } from '@tauri-apps/api/core'
import { FileProvider, HostAppService, HostWindowService, LogService, Platform, PlatformService, Theme, ThemesService, UpdaterService } from 'tabby-core'

import { TauriPlatformService } from './platform'
import { ConsoleLogService } from './services/log.service'
import { NullUpdaterService } from './services/updater.service'
import { TauriHostWindow } from './services/hostWindow.service'
import { TauriHostApp } from './services/hostApp.service'
import { MessageBoxModalComponent } from './components/messageBoxModal.component'
import { PTYInterface, ShellProvider } from 'tabby-local'
import { TauriPTYInterface } from './pty'
import { TauriFileProvider } from './fileProvider'
import { MacOSDefaultShellProvider } from './shells/macDefault'
import { POSIXShellsProvider } from './shells/posix'

import './styles.scss'

@NgModule({
    imports: [
        CommonModule,
    ],
    providers: [
        { provide: PlatformService, useClass: TauriPlatformService },
        { provide: LogService, useClass: ConsoleLogService },
        { provide: UpdaterService, useClass: NullUpdaterService },
        { provide: HostWindowService, useClass: TauriHostWindow },
        { provide: HostAppService, useClass: TauriHostApp },
        { provide: PTYInterface, useClass: TauriPTYInterface },
        { provide: FileProvider, useClass: TauriFileProvider, multi: true },
        { provide: ShellProvider, useClass: MacOSDefaultShellProvider, multi: true },
        { provide: ShellProvider, useClass: POSIXShellsProvider, multi: true },
    ],
    declarations: [
        MessageBoxModalComponent,
    ],
})
export default class TauriModule {
    constructor (hostApp: HostAppService, themes: ThemesService) {
        // The macOS window buttons where the theme wants them, as tabby-electron does (src-tauri/src/traffic_lights.rs):
        // now, for the theme Tabby applied before this module started, and whenever it changes.
        const placeButtons = (theme: Theme) => {
            if (hostApp.platform === Platform.macOS) {
                invoke('window_set_traffic_light_position', {
                    x: theme.macOSWindowButtonsInsetX ?? 14,
                    y: theme.macOSWindowButtonsInsetY ?? 11,
                })
            }
        }
        placeButtons(themes.findCurrentTheme())
        themes.themeChanged$.subscribe(placeButtons)
    }
}

export { startsWindowDrag } from './windowDrag'
