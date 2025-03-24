import * as esbuild from 'esbuild'
import { nodeModulesPolyfillPlugin } from 'esbuild-plugins-node-modules-polyfill'


// CJS
await esbuild.build({
  entryPoints: ['src/index.cjs'],
  platform: 'node',
  format: 'cjs',
  bundle: true,
  outfile: 'dist/index.cjs'
})

// Browser ESM
await esbuild.build({
  entryPoints: ['src/index.cjs'],
  plugins: [
    nodeModulesPolyfillPlugin({
      modules: {
        buffer: true,
        child_process: 'empty',
        fs: 'empty',
        path: 'empty'
      }
    })
  ],
  platform: 'browser',
  format: 'esm',
  bundle: true,
  minify: true,
  outfile: 'dist/browser.js'
})
