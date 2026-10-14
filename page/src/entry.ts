// Starts Tabby in the Tauri window: what Tabby's Electron and web builds do at startup, with the host's answers
// coming from the Rust side.
import './polyfills'

import 'zone.js'
import 'core-js/proposals/reflect-metadata'
import 'core-js/features/array/flat'
import 'rxjs'

import '../../app/src/global.scss'
import '../../app/src/toastr.scss'

import { enableProdMode } from '@angular/core'
import { platformBrowserDynamic } from '@angular/platform-browser-dynamic'
import { invoke } from '@tauri-apps/api/core'
import { getCurrentWindow } from '@tauri-apps/api/window'
import * as yaml from 'js-yaml'

import { getRootModule } from '../../app/src/app.module'
import { BootstrapData, BOOTSTRAP_DATA, PluginInfo } from '../../tabby-core/src/api/mainProcess'

interface HostInfo {
    env: Record<string, string>
    platform: string
    arch: string
    release: string
    hostname: string
    version: string
    executable: string
    debug: boolean
    background: boolean
}

// Always land on the start view (as Tabby's app/src/entry.ts does)
location.hash = ''

async function start (): Promise<void> {
    const info = await invoke<HostInfo>('host_info')
    const label = getCurrentWindow().label
    window['tabbyTauriHost'] = info
    Object.assign(process, { env: info.env, platform: info.platform, arch: info.arch })
    if (!info.debug) {
        enableProdMode()
    }

    let config = {}
    try {
        config = yaml.load(await invoke<string>('config_read')) as any ?? {}
    } catch (error) {
        console.error('Could not read the config:', error)
    }

    // The build writes the plugin list (scripts/build.mjs), in load order.
    const names: string[] = await (await fetch('plugins.json')).json()
    const modules = await window['TabbyHost'].loadPlugins(names, (current: number, total: number) => {
        const bar = document.querySelector('.progress .bar') as HTMLElement|null
        if (bar) {
            bar.style.width = `${100 * current / total}%`
        }
    })
    const installedPlugins: PluginInfo[] = modules.map(m => ({
        name: m.pluginInfo.name.replace(/^tabby-/, ''),
        packageName: m.pluginInfo.name,
        description: m.pluginInfo.description ?? '',
        isBuiltin: true,
        isLegacy: false,
        version: m.pluginInfo.version,
        author: m.pluginInfo.author ?? '',
    }))
    const bootstrapData: BootstrapData = {
        config,
        executable: info.executable,
        // The first window is "main", the others "main-2", "main-3"… (src-tauri/src/windows.rs)
        isMainWindow: label === 'main',
        windowID: label === 'main' ? 1 : Number(label.split('-')[1]),
        installedPlugins,
        userPluginsPath: '',
    }

    const pluginModules: any[] = []
    for (const m of modules) {
        if (!m.default) {
            continue
        }
        const pluginModule = m.default.forRoot ? m.default.forRoot() : m.default
        pluginModule.pluginName = m.pluginName
        pluginModule.bootstrap = m.bootstrap
        pluginModules.push(pluginModule)
    }
    // Tabby's ConfigService looks the plugin modules up here, as the Electron build leaves them.
    window['pluginModules'] = pluginModules
    // For tests (scripts/eval.mjs): services are `tabby.get(require('tabby-core').AppService)`.
    const moduleRef = await platformBrowserDynamic([
        { provide: BOOTSTRAP_DATA, useValue: bootstrapData },
    ]).bootstrapModule(getRootModule(pluginModules))
    window['tabby'] = moduleRef.injector
}

start().catch(error => {
    console.error('Startup failed:', error)
    document.body.insertAdjacentHTML('beforeend', `<pre style="position:fixed;bottom:0;color:#f66;white-space:pre-wrap">${String(error?.stack ?? error)}</pre>`)
})
