import { Injectable, NgZone } from '@angular/core'
import { getCurrentWebview } from '@tauri-apps/api/webview'

/**
 * WebKit doesn't tell the page where a dropped file lives (Electron's webUtils.getPathForFile does), and Tabby needs
 * paths to type into a shell. The window's native drag and drop events have them: this replays them as DOM drag
 * events on the element under the pointer, with the paths as `event.tauriPaths`.
 */
@Injectable({ providedIn: 'root' })
export class TauriDragDropService {
    private target: Element|null = null

    constructor (private zone: NgZone) { }

    start (): void {
        getCurrentWebview().onDragDropEvent(({ payload }) => this.zone.run(() => {
            if (payload.type === 'leave') {
                this.dispatch(this.target, 'dragleave', [])
                this.target = null
                return
            }
            const scale = window.devicePixelRatio || 1
            const x = payload.position.x / scale
            const y = payload.position.y / scale
            const element = document.elementFromPoint(x, y)
            const paths = payload.type === 'drop' || payload.type === 'enter' ? payload.paths : []
            if (element !== this.target) {
                this.dispatch(this.target, 'dragleave', [], x, y)
                this.target = element
                this.dispatch(element, 'dragenter', paths, x, y)
            }
            if (payload.type === 'drop') {
                this.dispatch(element, 'drop', payload.paths, x, y)
                this.target = null
            } else {
                this.dispatch(element, 'dragover', paths, x, y)
            }
        }))
    }

    /** Also for tests: a drop of these paths on this element. */
    dispatch (element: Element|null, type: string, paths: string[], x = 0, y = 0): void {
        if (!element) {
            return
        }
        const event = new DragEvent(type, { bubbles: true, cancelable: true, clientX: x, clientY: y, dataTransfer: new DataTransfer() })
        event['tauriPaths'] = paths
        element.dispatchEvent(event)
    }
}
