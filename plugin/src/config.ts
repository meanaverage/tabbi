import { ConfigProvider, Platform } from 'tabby-core'

/** @hidden Electron Tabby's defaults for the hotkeys this plugin handles (tabby-electron/src/config.ts). */
export class TauriConfigProvider extends ConfigProvider {
    platformDefaults = {
        [Platform.macOS]: {
            hotkeys: {
                'toggle-window': ['Ctrl-Space'],
            },
        },
    }

    defaults = {}
}
