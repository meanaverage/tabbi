import * as fs from 'fs'
import { Injectable } from '@angular/core'
import { invoke } from '@tauri-apps/api/core'
import { MenuItemOptions, NotificationsService, TranslateService } from 'tabby-core'
import { BaseTerminalTabComponent, TerminalContextMenuItemProvider } from 'tabby-terminal'

/** @hidden The terminal's "Export to file", as tabby-electron/src/terminalContextMenu.ts: everything in the terminal,
 * its scrollback included, saved as text where the user picks. */
@Injectable()
export class ExportTerminalContextMenu extends TerminalContextMenuItemProvider {
    weight = 0

    constructor (
        private notifications: NotificationsService,
        private translate: TranslateService,
    ) {
        super()
    }

    async getItems (tab: BaseTerminalTabComponent<any>): Promise<MenuItemOptions[]> {
        return [
            {
                label: this.translate.instant('Export to file'),
                click: async () => {
                    const path = await invoke<string|null>('plugin:dialog|save', { options: { defaultPath: 'terminal.txt' } })
                    if (path) {
                        await this.exportTo(tab, path)
                    }
                },
            },
        ]
    }

    /** Also for tests: writes the terminal's text to this file. */
    async exportTo (tab: BaseTerminalTabComponent<any>, path: string): Promise<void> {
        const frontend = tab.frontend
        if (!frontend) {
            return
        }
        frontend.selectAll()
        const content = frontend.getSelection()
        frontend.clearSelection()
        await fs.promises.writeFile(path, content)
        this.notifications.info(this.translate.instant('Saved to {path}', { path }))
    }
}
