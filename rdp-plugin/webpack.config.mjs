import * as url from 'url'
import config from '../webpack.plugin.config.mjs'
const dirname = url.fileURLToPath(new URL('.', import.meta.url))
export default () => {
    const c = config({ name: 'rdp', dirname, externals: [/^@tauri-apps\//], rules: [{ test: /\.py$/, type: 'asset/source' }] })
    c.optimization.minimize = false
    return c
}
