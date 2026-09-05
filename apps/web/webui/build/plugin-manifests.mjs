// Build-time plugin manifest scanner (T8 §H). Discovers every
// `src/sidebar/webview/plugins/<name>/manifest.json`, validates its shape, and
// yields the ordered list of client entries to inline into the webview bundle.
//
// The scanner is the single source of truth for the "manifest scan → inline
// bundle" step: `esbuild.mjs` feeds it to an esbuild plugin that serves a
// `manox:plugins` virtual module (imported once from `main.tsx`), and the
// vitest manifest-inline test snapshots the same output. Adding a plugin is
// dropping a `<name>/{manifest.json,client.tsx}` folder — no edit to any hand
// maintained import list.

import { readdirSync, readFileSync, existsSync } from 'node:fs';
import * as path from 'node:path';

/** @typedef {{ id: string; name: string; version: string; entry: string; slots: string[]; file: string }} PluginManifest */

/**
 * Scan `pluginsDir` for `<name>/manifest.json` files.
 * @param {string} pluginsDir absolute path to the webview `plugins/` folder
 * @returns {PluginManifest[]} sorted by id (deterministic inline order)
 * @throws if a manifest is malformed or its `entry` file is missing
 */
export function scanPluginManifests(pluginsDir) {
  if (!existsSync(pluginsDir)) return [];
  /** @type {PluginManifest[]} */
  const found = [];
  for (const dirName of readdirSync(pluginsDir, { withFileTypes: true })) {
    if (!dirName.isDirectory()) continue;
    const manifestPath = path.join(pluginsDir, dirName.name, 'manifest.json');
    if (!existsSync(manifestPath)) continue;
    const raw = JSON.parse(readFileSync(manifestPath, 'utf8'));
    const { id, name, version, entry, slots } = raw;
    if (typeof id !== 'string' || id.length === 0) {
      throw new Error(`plugin manifest ${manifestPath}: missing string "id"`);
    }
    if (id !== dirName.name) {
      throw new Error(`plugin manifest ${manifestPath}: id "${id}" ≠ folder "${dirName.name}"`);
    }
    if (typeof entry !== 'string' || entry.length === 0) {
      throw new Error(`plugin manifest ${manifestPath}: missing string "entry"`);
    }
    if (!Array.isArray(slots) || slots.some((s) => typeof s !== 'string')) {
      throw new Error(`plugin manifest ${manifestPath}: "slots" must be an array of strings`);
    }
    const file = path.resolve(path.dirname(manifestPath), entry);
    if (!existsSync(file)) {
      throw new Error(`plugin manifest ${manifestPath}: entry "${entry}" not found`);
    }
    found.push({
      id,
      name: typeof name === 'string' ? name : id,
      version: typeof version === 'string' ? version : '0.0.0',
      entry,
      slots,
      file,
    });
  }
  return found.sort((a, b) => a.id.localeCompare(b.id));
}

/**
 * Emit the `manox:plugins` virtual-module source: one side-effect import per
 * discovered client entry (the registration runs at bundle load).
 * @param {PluginManifest[]} manifests
 * @returns {string}
 */
export function buildPluginsModuleSource(manifests) {
  return manifests.map((m) => `import ${JSON.stringify(m.file)};`).join('\n') + '\n';
}
