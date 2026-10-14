import { Injectable } from '@angular/core'
import { TerminalDecorator, BaseTerminalTabComponent, BaseTerminalProfile } from 'tabby-terminal'

/** @hidden A short reply to a keystroke (usually its echo) is drawn at once instead of at the next animation frame,
 * which in WebKit can be up to a frame away (8 ms at 120 Hz). xterm already reads that reply right away; only the
 * drawing waited. Floods and queued output keep xterm's usual schedule. From tabbz (there in Tabby's xtermFrontend.ts);
 * it uses xterm's internals (6.0) and does nothing where they differ. */
@Injectable()
export class EchoNowDecorator extends TerminalDecorator {
    private originalWrites = new WeakMap<BaseTerminalTabComponent<any>, (data: string|Uint8Array, callback?: () => void) => void>()

    attach (terminal: BaseTerminalTabComponent<BaseTerminalProfile>): void {
        const xterm = (terminal.frontend as any)?.xterm
        const core = xterm?._core
        if (!core?._writeBuffer || !core._renderService || this.originalWrites.has(terminal)) {
            return
        }
        const write = xterm.write.bind(xterm)
        this.originalWrites.set(terminal, xterm.write)
        xterm.write = (data: string|Uint8Array, callback?: () => void) => {
            // (xterm's WriteBuffer.write: the first write after a keystroke, into an empty queue, is parsed at once)
            const buffer = core._writeBuffer
            const reply = data.length <= 256 && buffer._didUserInput && !buffer._writeBuffer.length
            return write(data, reply ? () => { drawNow(core); callback?.() } : callback)
        }
    }

    detach (terminal: BaseTerminalTabComponent<BaseTerminalProfile>): void {
        const write = this.originalWrites.get(terminal)
        const xterm = (terminal.frontend as any)?.xterm
        if (write && xterm) {
            xterm.write = write
        }
        this.originalWrites.delete(terminal)
        super.detach(terminal)
    }
}

/** What xterm's render debouncer does in its animation frame, now: draw the rows the parser marked. */
function drawNow (core: any): void {
    const service = core._renderService
    const debouncer = service._renderDebouncer
    if (service._isPaused || service._coreService?.decPrivateModes?.synchronizedOutput) {
        return
    }
    if (debouncer?._rowStart === undefined || typeof debouncer._innerRefresh !== 'function') {
        return
    }
    try {
        debouncer.dispose()
        debouncer._innerRefresh()
    } catch (error) {
        console.warn('Could not draw the reply at once:', error)
    }
}
