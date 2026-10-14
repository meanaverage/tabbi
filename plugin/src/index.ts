import { NgModule, NgZone } from '@angular/core'
import { CommonModule } from '@angular/common'
import { invoke } from '@tauri-apps/api/core'
import { getCurrentWindow, ProgressBarStatus } from '@tauri-apps/api/window'
import { AppService, ConfigProvider, ConfigService, DockingService, FileProvider, HostAppService, HostWindowService, HotkeyProvider, LogService, Platform, PlatformService, Theme, ThemesService, UpdaterService } from 'tabby-core'
import { TerminalContextMenuItemProvider, TerminalDecorator } from 'tabby-terminal'
import { auditTime, debounceTime } from 'rxjs'

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
import { TauriConfigProvider } from './config'
import { TauriHotkeyProvider } from './hotkeys'
import { PathDropDecorator } from './pathDrop'
import { EchoNowDecorator } from './echoNow'
import { ExportTerminalContextMenu } from './terminalContextMenu'
import { TauriDockingService } from './services/docking.service'
import { DockMenuService } from './services/dockMenu.service'
import { TauriDragDropService } from './services/dragDrop.service'

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
        { provide: DockingService, useExisting: TauriDockingService },
        { provide: ConfigProvider, useClass: TauriConfigProvider, multi: true },
        { provide: HotkeyProvider, useClass: TauriHotkeyProvider, multi: true },
        { provide: TerminalDecorator, useClass: PathDropDecorator, multi: true },
        { provide: TerminalDecorator, useClass: EchoNowDecorator, multi: true },
        { provide: TerminalContextMenuItemProvider, useClass: ExportTerminalContextMenu, multi: true },
        { provide: ShellProvider, useClass: MacOSDefaultShellProvider, multi: true },
        { provide: ShellProvider, useClass: POSIXShellsProvider, multi: true },
    ],
    declarations: [
        MessageBoxModalComponent,
    ],
})
export default class TauriModule {
    private registeredHotkeys: string|null = null

    constructor (
        hostApp: HostAppService,
        themes: ThemesService,
        private config: ConfigService,
        app: AppService,
        docking: TauriDockingService,
        dockMenu: DockMenuService,
        dragDrop: TauriDragDropService,
        zone: NgZone,
    ) {
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

        // What the rest of tabby-electron's module does at startup and on settings changes.
        dragDrop.start()
        config.ready$.toPromise().then(() => {
            this.updateWindow()
            this.registerGlobalHotkey()
            dockMenu.update()
            docking.dock()
        })
        config.changed$.subscribe(() => {
            this.updateWindow()
            this.registerGlobalHotkey()
        })
        getCurrentWindow().listen('host:toggle-window', () => zone.run(() => docking.toggleWindow()))

        // WebKit collects garbage only under allocation pressure, so closed tabs' terminals (their scrollback) would stay
        // in memory for a long while: ask it to once tabs stop closing (src-tauri/src/window.rs).
        app.tabClosed$.pipe(debounceTime(2000)).subscribe(() => invoke('webview_collect_garbage'))

        // A tab's progress (a transfer, a long command reporting it) on the Dock icon.
        let lastProgress: number|null = null
        app.tabOpened$.subscribe(tab => {
            tab.progress$.pipe(auditTime(250)).subscribe(progress => {
                if (lastProgress === progress) {
                    return
                }
                lastProgress = progress
                getCurrentWindow().setProgressBar(progress !== null
                    ? { status: ProgressBarStatus.Normal, progress: Math.round(progress) }
                    : { status: ProgressBarStatus.None })
            })
        })
    }

    /** Vibrancy, opacity and dark mode from Settings → Appearance (src-tauri/src/window.rs). */
    private updateWindow () {
        const appearance = this.config.store.appearance
        invoke('window_set_vibrancy', { enabled: !!appearance.vibrancy, kind: appearance.vibrancyType ?? null })
        invoke('window_set_opacity', { opacity: appearance.opacity ?? 1 })
        invoke('window_set_dark_mode', { mode: appearance.colorSchemeMode ?? 'auto' })
    }

    /** The toggle-window hotkey, system-wide (src-tauri/src/hotkey.rs), from Tabby's notation to Tauri's. */
    private registerGlobalHotkey () {
        let value = this.config.store.hotkeys['toggle-window'] || []
        if (typeof value === 'string') {
            value = [value]
        }
        const specs = value
            .map((item: string|string[]) => typeof item === 'string' ? item : item[0])
            .filter(Boolean)
            .map((spec: string) => spec.replaceAll('⌘', 'Command').replaceAll('⌥', 'Alt').replaceAll('Meta', 'Super').replaceAll('-', '+'))
        const key = specs.join('|')
        if (key === this.registeredHotkeys) {
            return
        }
        this.registeredHotkeys = key
        invoke('global_hotkey_set', { specs }).catch(error => console.error('Could not register the global hotkey:', error))
    }
}

export { startsWindowDrag } from './windowDrag'
export { ExportTerminalContextMenu } from './terminalContextMenu'
