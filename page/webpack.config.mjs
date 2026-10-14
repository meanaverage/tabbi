// The page's own two scripts: preload.js (Node stand-ins, fonts) and bundle.js (starts Tabby). This folder sits next
// to Tabby's plugins in the build's copy of Tabby, so `..` is Tabby.
import * as path from 'path'
import * as url from 'url'
import webpack from 'webpack'
const __dirname = url.fileURLToPath(new URL('.', import.meta.url))
const web = path.join(__dirname, '../web/node_modules')

export default () => ({
    name: 'tabbi-page',
    target: 'web',
    entry: {
        preload: path.resolve(__dirname, 'src/preload.ts'),
        bundle: path.resolve(__dirname, 'src/entry.ts'),
    },
    mode: process.env.TABBY_DEV ? 'development' : 'production',
    optimization: {
        minimize: false,
    },
    context: __dirname,
    devtool: 'source-map',
    output: {
        path: path.join(__dirname, 'dist'),
        pathinfo: true,
        filename: '[name].js',
        publicPath: 'auto',
    },
    resolve: {
        // (then the usual lookup up the tree, for bundled packages' own node_modules)
        modules: [...['../app/node_modules', 'node_modules', '../web/node_modules', '../node_modules', '../app/assets/'].map(x => path.join(__dirname, x)), 'node_modules'],
        extensions: ['.ts', '.js'],
        alias: {
            net: path.join(__dirname, 'src/node/net.ts'),
            dns: false,
        },
        fallback: {
            stream: path.join(web, 'stream-browserify/index.js'),
            assert: path.join(web, 'assert/assert.js'),
            constants: path.join(web, 'constants-browserify/constants.json'),
            util: path.join(web, 'util/util.js'),
            crypto: false,
            fs: false,
            path: false,
            os: false,
            vm: false,
        },
    },
    plugins: [
        // russh's own wrapper (its lib/*.js, what Tabby's SSH plugin talks to) gets our stand-in for its native part.
        new webpack.NormalModuleReplacementPlugin(/^\.\/native$/, resource => {
            if (/[\\/]russh[\\/]lib$/.test(resource.context)) {
                resource.request = path.join(__dirname, 'src/node/russh.native.ts')
            }
        }),
    ],
    module: {
        rules: [
            {
                test: /\.ts$/,
                use: {
                    loader: 'ts-loader',
                    options: {
                        configFile: path.resolve(__dirname, 'tsconfig.json'),
                        transpileOnly: true,
                    },
                },
            },
            { test: /\.scss$/, use: ['style-loader', 'css-loader', 'sass-loader'] },
            { test: /\.css$/, use: ['style-loader', 'css-loader'] },
            {
                test: /\.(png|svg|ttf|eot|otf|woff|woff2)(\?v=[0-9]\.[0-9]\.[0-9])?$/,
                type: 'asset',
            },
        ],
    },
})
