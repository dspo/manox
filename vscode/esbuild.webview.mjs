// Bundle the sidebar webview frontend into dist/webview/bundle.js, loaded
// by sidebarProvider behind a CSP nonce. The companion bundle.css is
// produced by the Tailwind CLI from styles/tokens.css. The host extension
// is compiled separately by tsc; this bundle is webview-only code.
import * as esbuild from 'esbuild';
import { fileURLToPath } from 'node:url';
import * as path from 'node:path';

const root = path.dirname(fileURLToPath(import.meta.url));
const watch = process.argv.includes('--watch');

/** @type {import('esbuild').BuildOptions} */
const options = {
  entryPoints: [path.join(root, 'src', 'sidebar', 'webview', 'main.tsx')],
  bundle: true,
  outfile: path.join(root, 'dist', 'webview', 'bundle.js'),
  format: 'iife',
  target: 'es2022',
  // Match tsconfig.webview.json's react-jsx; the classic transform would need
  // a React import in every module.
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
