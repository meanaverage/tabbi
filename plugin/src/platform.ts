import { Injectable, NgZone } from '@angular/core'
import { invoke } from '@tauri-apps/api/core'
import { LogicalPosition } from '@tauri-apps/api/dpi'
import { CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu } from '@tauri-apps/api/menu'
import { getCurrentWindow } from '@tauri-apps/api/window'
import { NgbModal } from '@ng-bootstrap/ng-bootstrap'
import { PlatformService, ClipboardContent, MenuItemOptions, MessageBoxOptions, MessageBoxResult, FileUpload, FileUploadOptions, FileDownload, DirectoryDownload, HTMLFileUpload, DirectoryUpload, PlatformTheme, TranslateService } from 'tabby-core'

import { MessageBoxModalComponent } from './components/messageBoxModal.component'
import { TextFieldMenus } from './textFieldMenus'
import { syncCall } from './sync'


@Injectable()
export class TauriPlatformService extends PlatformService {
    /** Also for tests. */
    readonly textFieldMenus: TextFieldMenus
    private fileSelector: HTMLInputElement
    private darkScheme = window.matchMedia('(prefers-color-scheme: dark)')

    constructor (
        private zone: NgZone,
        private ngbModal: NgbModal,
        translate: TranslateService,
    ) {
        super()
        this.textFieldMenus = new TextFieldMenus(this, translate)
        this.textFieldMenus.install(document)
        // The system's light or dark mode, which Tabby follows when its color scheme mode is "auto".
        this.darkScheme.addEventListener('change', () => this.zone.run(() => this.themeChanged.next(this.getTheme())))
        getCurrentWindow().onScaleChanged(() => this.zone.run(() => this.displayMetricsChanged.next()))

        this.fileSelector = document.createElement('input')
        this.fileSelector.type = 'file'
        this.fileSelector.style.visibility = 'hidden'
        document.body.appendChild(this.fileSelector)
    }

    readClipboard (): string {
        return syncCall<string>('clipboard_read')
    }

    setClipboard (content: ClipboardContent): void {
        invoke('clipboard_write', { text: content.text, html: content.html ?? null })
    }

    async loadConfig (): Promise<string> {
        return invoke<string>('config_read')
    }

    async saveConfig (content: string): Promise<void> {
        await invoke('config_write', { content })
    }

    getOSRelease (): string {
        return window['tabbyTauriHost']?.release ?? '0'
    }

    async openExternal (url: string): Promise<void> {
        await invoke('open_external', { url })
    }

    getAppVersion (): string {
        return window['tabbyTauriHost']?.version ?? '0.1.0'
    }

    async listFonts (): Promise<string[]> {
        return invoke<string[]>('list_fonts')
    }

    getTheme (): PlatformTheme {
        return this.darkScheme.matches ? 'dark' : 'light'
    }

    /** A native menu: at the pointer for a click, under the element for one opened from the keyboard. */
    popupContextMenu (menu: MenuItemOptions[], event?: MouseEvent): void {
        const at = event && !event.isTrusted ? new LogicalPosition(event.clientX, event.clientY) : undefined
        this.buildMenu(menu).then(m => m.popup(at))
    }

    /** Also for tests: what popupContextMenu shows. */
    async buildMenu (items: MenuItemOptions[]): Promise<Menu> {
        return Menu.new({ items: await Promise.all(items.map(i => this.buildMenuItem(i))) })
    }

    private async buildMenuItem (item: MenuItemOptions): Promise<any> {
        const action = item.click ? () => this.zone.run(() => item.click!()) : undefined
        const enabled = item.enabled ?? true
        if (item.type === 'separator') {
            return PredefinedMenuItem.new({ item: 'Separator' })
        }
        if (item.submenu) {
            return Submenu.new({ text: item.label ?? '', enabled, items: await Promise.all(item.submenu.map(i => this.buildMenuItem(i))) })
        }
        if (item.type === 'checkbox' || item.type === 'radio') {
            return CheckMenuItem.new({ text: item.label ?? '', enabled, checked: !!item.checked, action })
        }
        return MenuItem.new({ text: item.label ?? '', enabled, action })
    }

    async showMessageBox (options: MessageBoxOptions): Promise<MessageBoxResult> {
        const modal = this.ngbModal.open(MessageBoxModalComponent, {
            backdrop: 'static',
        })
        const instance: MessageBoxModalComponent = modal.componentInstance
        instance.options = options
        try {
            const response = await modal.result
            return { response }
        } catch {
            return { response: options.cancelId ?? 1 }
        }
    }

    quit (): void {
        invoke('app_quit')
    }

    async startDownload (name: string, mode: number, size: number): Promise<FileDownload|null> {
        const transfer = new HTMLFileDownload(name, mode, size)
        this.fileTransferStarted.next(transfer)
        return transfer
    }

    async startDownloadDirectory (_name: string, _estimatedSize?: number): Promise<DirectoryDownload|null> {
        throw new Error('Unsupported')
    }

    startUpload (options?: FileUploadOptions): Promise<FileUpload[]> {
        return new Promise(resolve => {
            this.fileSelector.onchange = () => {
                const transfers: FileUpload[] = []
                const fileList = this.fileSelector.files!
                // eslint-disable-next-line @typescript-eslint/no-unnecessary-condition
                for (let i = 0; i < (fileList.length ?? 0); i++) {
                    const file = fileList[i]
                    const transfer = new HTMLFileUpload(file)
                    this.fileTransferStarted.next(transfer)
                    transfers.push(transfer)
                    if (!options?.multiple) {
                        break
                    }
                }
                resolve(transfers)
            }
            this.fileSelector.click()
        })
    }

    async startUploadDirectory (_paths?: string[]): Promise<DirectoryUpload> {
        return new DirectoryUpload()
    }

    setErrorHandler (handler: (_: any) => void): void {
        window.addEventListener('error', handler)
    }

    async pickDirectory (): Promise<string|null> {
        return invoke<string|null>('plugin:dialog|open', { options: { multiple: false, directory: true } })
    }
}

class HTMLFileDownload extends FileDownload {
    private buffers: Uint8Array[] = []

    constructor (
        private name: string,
        private mode: number,
        private size: number,
    ) {
        super()
    }

    getName (): string {
        return this.name
    }

    getMode (): number {
        return this.mode
    }

    getSize (): number {
        return this.size
    }

    async write (buffer: Uint8Array): Promise<void> {
        this.buffers.push(Uint8Array.from(buffer))
        this.increaseProgress(buffer.length)
        if (this.isComplete()) {
            this.finish()
        }
    }

    finish () {
        const blob = new Blob(this.buffers, { type: 'application/octet-stream' })
        const element = window.document.createElement('a')
        element.href = window.URL.createObjectURL(blob)
        element.download = this.name
        document.body.appendChild(element)
        element.click()
        document.body.removeChild(element)
    }

    // eslint-disable-next-line @typescript-eslint/no-empty-function
    close (): void { }
}
