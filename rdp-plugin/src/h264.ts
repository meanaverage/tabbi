/** Active and retiring browser decoders together, within one desktop session. */
export const MAX_H264_DECODERS = 16

/** Public constructor options provided by the vendored WebCodecsH264Decoder. */
interface H264DecoderOptions {
    maxDecoders?: number
    stallMs?: number
    onFailure?: (reason: string) => void
    now?: () => number
}

/**
 * Keep the vendor instance intact: IronRDP calls its endSurfaces hook even without another video frame, and the
 * decoder owns pending-byte accounting, retiring decoders, readbacks and statistics. Capacity exhaustion follows
 * its normal failure path, which reconnects without H.264; the plugin must not evict its private surface state.
 */
export function createH264Decoder<T> (
    Decoder: new (options: H264DecoderOptions) => T,
    options: Omit<H264DecoderOptions, 'maxDecoders'> = {},
): T {
    return new Decoder({ ...options, maxDecoders: MAX_H264_DECODERS })
}
