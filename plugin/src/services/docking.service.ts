import { Inject, Injectable, NgZone } from '@angular/core'
import { invoke } from '@tauri-apps/api/core'
import { PhysicalPosition, PhysicalSize } from '@tauri-apps/api/dpi'
import { availableMonitors, cursorPosition, getCurrentWindow, monitorFromPoint, primaryMonitor, type Monitor } from '@tauri-apps/api/window'
import { BOOTSTRAP_DATA, BootstrapData, ConfigService, DockingService, HostWindowService, PlatformService, Screen } from 'tabby-core'

interface TauriScreen extends Screen {
    monitor: Monitor
}

const host = (): any => window['tabbyTauriHost'] ?? {}

/**
 * Docking the window to a screen edge (Settings → Window → Dock), as Electron Tabby's ElectronDockingService: the
 * edge, how much of the screen it fills, and whether it stays on top. Also the "Toggle terminal window" hotkey and
 * hiding a docked window when it loses focus, which the Electron build does in its main process.
 */
@Injectable({ providedIn: 'root' })
export class TauriDockingService extends DockingService {
    private screens: TauriScreen[] = []
    private pending = Promise.resolve()
    private ready = false

    constructor (
        private config: ConfigService,
        private hostWindow: HostWindowService,
        platform: PlatformService,
        private zone: NgZone,
        @Inject(BOOTSTRAP_DATA) private bootstrapData: BootstrapData,
    ) {
        super()
        void this.refreshScreens()
        platform.displayMetricsChanged$.subscribe(() => {
            void this.refreshScreens().then(() => this.applyDock()).then(() => this.repositionWindow())
        })
        this.hostWindow.windowMoved$.subscribe(() => void this.refreshScreens())
        config.ready$.subscribe(() => {
            this.ready = true
        })
        config.changed$.subscribe(() => this.dock())
        // Tauri reports scale changes but no screens being connected or disconnected.
        this.zone.runOutsideAngular(() => setInterval(() => {
            void this.refreshScreens().then(changed => {
                if (changed) {
                    void this.applyDock().catch(error => console.error('Could not dock the window:', error)).then(() => this.repositionWindow())
                }
            })
        }, 3000))
        getCurrentWindow().onFocusChanged(({ payload: focused }) => {
            if (focused) {
                this.dock()
            } else {
                void this.hideOnBlur()
            }
        })
    }

    dock (): void {
        void this.applyDock().catch(error => console.error('Could not dock the window:', error))
    }

    /** Also for tests: resolves once the window is where the settings put it. */
    applyDock (): Promise<void> {
        this.pending = this.pending.catch(() => null).then(() => this.updateDock())
        return this.pending
    }

    getScreens (): Screen[] {
        return this.screens.map(({ id, name }) => ({ id, name }))
    }

    /** The toggle hotkey: shows and focuses the window, or hides it when it's already in front. */
    async toggleWindow (): Promise<void> {
        const window = getCurrentWindow()
        const appearance = this.config.store.appearance
        const dockedOnTop = this.bootstrapData.isMainWindow && (appearance.dock ?? 'off') !== 'off' && (appearance.dockAlwaysOnTop ?? true)
        if (await window.isVisible() && (dockedOnTop || await window.isFocused())) {
            if (dockedOnTop) {
                await window.setAlwaysOnTop(false)
            }
            await window.hide()
        } else {
            await this.applyDock()
            await window.show()
            await window.unminimize()
            if (!host().background) {
                await window.setFocus()
            }
        }
    }

    private async updateDock (): Promise<void> {
        if (!this.ready || !this.bootstrapData.isMainWindow) {
            return
        }
        const window = getCurrentWindow()
        const appearance = this.config.store.appearance
        const side = appearance.dock ?? 'off'
        if (side === 'off') {
            await invoke('window_set_docked', { enabled: false })
            return
        }

        await this.refreshScreens()
        let screen = this.screens.find(item => item.id === Number(appearance.dockScreen))
        if (!screen) {
            const cursor = await cursorPosition()
            const current = await monitorFromPoint(cursor.x, cursor.y)
            screen = this.screens.find(item => current && this.sameMonitor(item.monitor, current)) ?? this.screens[0]
        }
        if (!screen) {
            return
        }

        const area = screen.monitor.workArea
        const fill = Math.max(0.05, Math.min(Number(appearance.dockFill ?? 0.5), 1))
        const space = Math.max(0.2, Math.min(Number(appearance.dockSpace ?? 1), 1))
        const minWidth = Math.round(400 * screen.monitor.scaleFactor)
        const minHeight = Math.round(300 * screen.monitor.scaleFactor)
        let width: number
        let height: number
        if (side === 'left' || side === 'right') {
            width = Math.max(minWidth, Math.round(fill * area.size.width))
            height = Math.max(minHeight, Math.round(area.size.height * space))
        } else {
            width = Math.max(minWidth, Math.round(area.size.width * space))
            height = Math.max(minHeight, Math.round(fill * area.size.height))
        }
        const x = side === 'right' ? area.position.x + area.size.width - width
            : side === 'left' ? area.position.x
                : area.position.x + Math.round((area.size.width - width) / 2)
        const y = side === 'bottom' ? area.position.y + area.size.height - height
            : side === 'top' ? area.position.y
                : area.position.y + Math.round((area.size.height - height) / 2)

        if (await window.isFullscreen()) {
            await window.setFullscreen(false)
        }
        if (await window.isMaximized()) {
            await window.unmaximize()
        }
        await invoke('window_set_docked', { enabled: appearance.dockAlwaysOnTop ?? true })
        await window.setSize(new PhysicalSize(width, height))
        await window.setPosition(new PhysicalPosition(x, y))
    }

    private async refreshScreens (): Promise<boolean> {
        const [monitors, primary] = await Promise.all([availableMonitors(), primaryMonitor()])
        monitors.sort((a, b) => a.position.x === b.position.x ? a.position.y - b.position.y : a.position.x - b.position.x)
        const next = monitors.map((monitor, index): TauriScreen => ({
            id: index,
            name: primary && this.sameMonitor(monitor, primary) ? 'Primary Display' : monitor.name ?? `Display ${index + 1}`,
            monitor,
        }))
        const describe = (screens: TauriScreen[]) => JSON.stringify(screens.map(s => [s.id, s.name, s.monitor.position, s.monitor.size, s.monitor.workArea, s.monitor.scaleFactor]))
        const changed = describe(this.screens) !== describe(next)
        this.screens = next
        if (changed) {
            this.zone.run(() => this.screensChanged.next())
        }
        return changed
    }

    /** A window left off every screen (one was disconnected) comes back to the primary one. */
    private async repositionWindow (): Promise<void> {
        const window = getCurrentWindow()
        const position = await window.outerPosition()
        if (this.screens.some(({ monitor }) => position.x >= monitor.position.x && position.x <= monitor.position.x + monitor.size.width
            && position.y >= monitor.position.y && position.y <= monitor.position.y + monitor.size.height)) {
            return
        }
        const primary = await primaryMonitor()
        if (primary) {
            await window.setPosition(primary.workArea.position)
        }
    }

    private async hideOnBlur (): Promise<void> {
        const appearance = this.config.store.appearance
        if (!this.ready || (appearance.dock ?? 'off') === 'off' || !appearance.dockHideOnBlur || !this.bootstrapData.isMainWindow) {
            return
        }
        const window = getCurrentWindow()
        await window.setAlwaysOnTop(false)
        await window.hide()
    }

    private sameMonitor (a: Monitor, b: Monitor): boolean {
        return a.position.x === b.position.x && a.position.y === b.position.y && a.size.width === b.size.width && a.size.height === b.size.height
    }
}
