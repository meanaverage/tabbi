// Synchronous answers from the Rust side (src-tauri/src/sync.rs). Tabby's code calls a few things that must answer
// immediately (fs.statSync, the clipboard for a paste, process.kill(pid, 0)); a synchronous request to the
// `tabbysync:` scheme waits for Rust, so that code works unchanged.

export function nodeError (message: string): Error {
    const error = new Error(message) as any
    const code = /^(E[A-Z]+):/.exec(message)?.[1]
    if (code) {
        error.code = code
    }
    return error
}

export function syncCall<T> (call: string, args: Record<string, any> = {}): T {
    const request = new XMLHttpRequest()
    request.open('POST', `tabbysync://localhost/${call}`, false)
    request.setRequestHeader('Content-Type', 'text/plain')
    request.send(JSON.stringify(args))
    if (request.status !== 200) {
        throw nodeError(request.responseText || `${call} failed (${request.status})`)
    }
    return JSON.parse(request.responseText)
}
