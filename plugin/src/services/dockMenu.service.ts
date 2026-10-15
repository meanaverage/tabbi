import { Injectable, NgZone } from '@angular/core'
import { invoke } from '@tauri-apps/api/core'
import { getCurrentWindow } from '@tauri-apps/api/window'
import { ConfigService, HostAppService, Platform, ProfilesService } from 'tabby-core'

interface DockMenuItem {
    id: string
    label: string
}

/** The Dock icon's menu: recent profiles, then all of them, as Electron Tabby's DockMenuService (src-tauri/src/dock_menu.rs). */
@Injectable({ providedIn: 'root' })
export class DockMenuService {
    private actions = new Map<string, () => void>()
    private revision = 0

    constructor (
        private config: ConfigService,
        private hostApp: HostAppService,
        private profiles: ProfilesService,
        zone: NgZone,
    ) {
        getCurrentWindow().listen<string>('host:dock-menu', event => zone.run(() => this.actions.get(event.payload)?.()))
        this.config.changed$.subscribe(() => this.update())
    }

    async update (): Promise<void> {
        const revision = ++this.revision
        if (this.hostApp.platform !== Platform.macOS) {
            return
        }
        const shown = (profile: { id?: string }) => profile.id && !this.config.store.profileBlacklist.includes(profile.id)
        const profiles = (await this.profiles.getProfiles()).filter(shown)
        const recent = this.profiles.getRecentProfiles().filter(shown)

        const items: DockMenuItem[] = []
        const actions = new Map<string, () => void>()
        for (const [index, profile] of [...recent, ...profiles].entries()) {
            const id = index < recent.length ? `recent:${index}` : `profile:${profile.id}`
            items.push({ id, label: profile.name })
            actions.set(id, () => this.profiles.openNewTabForProfile(profile))
        }
        if (revision !== this.revision) {
            return
        }
        this.actions = actions
        await invoke('dock_menu_set', { items })
    }
}
