// Bundle the webview frontend into dist/webview/bundle.js. The companion
// bundle.css is produced by the Tailwind CLI from styles/tokens.css. This
// bundle is shared verbatim by both hosts: VS Code loads it behind a CSP
// nonce, the browser host embeds it in the served index.html.
//
// Extension plugins (T8 §H): `build/plugin-manifests.mjs` scans
// `src/sidebar/webview/plugins/*/manifest.json` at build time and the
// `manox-plugins` esbuild plugin serves a `manox:plugins` virtual module that
// side-effect-imports every discovered client entry (so each plugin's
// slot-registration runs at bundle load). `main.tsx` imports that virtual
// module exactly once. Adding a plugin = dropping a folder; no hand-maintained
// import list.
import * as esbuild from 'esbuild';
import { fileURLToPath } from 'node:url';
import * as path from 'node:path';

import { buildPluginsModuleSource, scanPluginManifests } from './build/plugin-manifests.mjs';

const root = path.dirname(fileURLToPath(import.meta.url));
const watch = process.argv.includes('--watch');

const pluginsDir = path.join(root, 'src', 'sidebar', 'webview', 'plugins');
const manifests = scanPluginManifests(pluginsDir);

/** @type {import('esbuild').Plugin} */
const manoxPlugins = {
  name: 'manox-plugins',
  setup(build) {
    build.onResolve({ filter: /^manox:plugins$/ }, () => ({
      path: 'manox:plugins',
      namespace: 'manox-plugins',
    }));
    build.onLoad({ filter: /.*/, namespace: 'manox-plugins' }, () => ({
      contents: buildPluginsModuleSource(manifests),
      loader: 'js',
      resolveDir: pluginsDir,
    }));
  },
};

/** @type {import('esbuild').BuildOptions} */
const options = {
  entryPoints: [path.join(root, 'src', 'sidebar', 'webview', 'main.tsx')],
  bundle: true,
  plugins: [manoxPlugins],
  outfile: path.join(root, 'dist', 'webview', 'bundle.js'),
  format: 'iife',
  target: 'es2022',
  // Match tsconfig.json's react-jsx; the classic transform would need a
  // React import in every module.
  jsx: 'automatic',
  minify: !watch,
  sourcemap: watch ? 'inline' : false,
  logLevel: 'info',
};

if (watch) {
  const ctx = await esbuild.context(options);
  await ctx.watch();
} else {
  await esbuild.build(options);
}
