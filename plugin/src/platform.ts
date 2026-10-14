import '@vaadin/vaadin-context-menu'
import { Injectable } from '@angular/core'
import { invoke } from '@tauri-apps/api/core'
import { NgbModal } from '@ng-bootstrap/ng-bootstrap'
import { PlatformService, ClipboardContent, MenuItemOptions, MessageBoxOptions, MessageBoxResult, FileUpload, FileUploadOptions, FileDownload, DirectoryDownload, HTMLFileUpload, DirectoryUpload } from 'tabby-core'

// eslint-disable-next-line no-duplicate-imports
import type { ContextMenuElement, ContextMenuItem } from '@vaadin/vaadin-context-menu'

import { MessageBoxModalComponent } from './components/messageBoxModal.component'
import { syncCall } from './sync'
import './styles.scss'


@Injectable()
export class TauriPlatformService extends PlatformService {
    private menu: ContextMenuElement
    private contextMenuHandlers = new Map<ContextMenuItem, () => void>()
    private fileSelector: HTMLInputElement

    constructor (
        private ngbModal: NgbModal,
    ) {
        super()
        this.menu = window.document.createElement('vaadin-context-menu')
        this.menu.addEventListener('item-selected', e => {
            this.contextMenuHandlers.get(e.detail.value)?.()
        })
        document.body.appendChild(this.menu)

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
        return []
    }

    popupContextMenu (menu: MenuItemOptions[], event?: MouseEvent): void {
        this.contextMenuHandlers.clear()
        this.menu.items = menu
            .filter(x => x.type !== 'separator')
            .map(x => this.remapMenuItem(x))
        setTimeout(() => {
            this.menu.open(event)
        }, 10)
    }

    private remapMenuItem (item: MenuItemOptions): ContextMenuItem {
        const cmi = {
            text: item.label,
            disabled: !(item.enabled ?? true),
            checked: item.checked,
            children: item.submenu?.map(i => this.remapMenuItem(i)),
        }
        if (item.click) {
            this.contextMenuHandlers.set(cmi, item.click)
        }
        return cmi
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
        window.close()
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
