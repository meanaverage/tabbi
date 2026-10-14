// keytar (saved passwords) over the Rust keychain commands (src-tauri/src/keychain.rs).
import { invoke } from '@tauri-apps/api/core'

async function call<T> (cmd: string, args: any): Promise<T> {
    try {
        return await invoke<T>(cmd, args)
    } catch (error) {
        throw error instanceof Error ? error : new Error(String(error))
    }
}

export async function getPassword (service: string, account: string): Promise<string|null> {
    return call<string|null>('keychain_get', { service, account })
}

export async function setPassword (service: string, account: string, password: string): Promise<void> {
    await call('keychain_set', { service, account, password })
}

export async function deletePassword (service: string, account: string): Promise<boolean> {
    return call<boolean>('keychain_delete', { service, account })
}

export async function findCredentials (service: string): Promise<{ account: string, password: string }[]> {
    return call('keychain_find', { service })
}

export async function findPassword (service: string): Promise<string|null> {
    return (await findCredentials(service))[0]?.password ?? null
}
