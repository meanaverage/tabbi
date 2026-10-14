import { invoke } from '@tauri-apps/api/core'
import { Injectable, NgZone } from '@angular/core'
import { getCurrentWindow, Window } from '@tauri-apps/api/window'
import { HostWindowService } from 'tabby-core'
import { followTabbyDragRegions } from '../windowDrag'

@Injectable({ providedIn: 'root' })
export class TauriHostWindow extends HostWindowService {
    private window: Window = getCurrentWindow()
    private fullscreen = false
    private maximized = false

    get isFullscreen (): boolean { return this.fullscreen }

    constructor (zone: NgZone) {
        super()
        this.windowShown.next()
        this.window.isFocused().then(focused => focused && zone.run(() => this.windowFocused.next()))
        this.window.onFocusChanged(({ payload: focused }) => zone.run(() => {
            if (focused) {
                this.windowFocused.next()
            }
        }))
        this.window.onResized(async () => {
            this.fullscreen = await this.window.isFullscreen()
            this.maximized = await this.window.isMaximized()
            zone.run(() => this.windowMoved.next())
        })
        this.window.onMoved(() => zone.run(() => this.windowMoved.next()))
        this.window.onCloseRequested(event => {
            event.preventDefault()
            zone.run(() => this.windowCloseRequest.next())
        })
        zone.runOutsideAngular(() => followTabbyDragRegions(() => this.window.startDragging()))
    }

    reload (): void {
        location.reload()
    }

    setTitle (title?: string): void {
        // (through Rust, which puts the window buttons back where macOS moves them on a title change)
        invoke('window_set_title', { title: title ?? 'tabbi' })
    }

    toggleFullscreen (): void {
        this.fullscreen = !this.fullscreen
        this.window.setFullscreen(this.fullscreen)
    }

    minimize (): void {
        this.window.minimize()
    }

    isMaximized (): boolean {
        return this.maximized
    }

    toggleMaximize (): void {
        this.window.toggleMaximize()
    }

    close (): void {
        this.window.destroy()
    }
}
