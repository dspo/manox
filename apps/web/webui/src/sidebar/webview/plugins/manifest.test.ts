/// <reference types="node" />
// Build-time plugin manifest scan → inline (§H). Node suite: it validates the
// scanner that `esbuild.mjs` uses to inline each plugin's client registration
// into the webview bundle, and snapshots the discovery of the real
// conversation-info plugin (so a dropped/renamed folder fails the build loudly).

import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import * as path from 'node:path';

import { describe, expect, it } from 'vitest';

import {
	buildPluginsModuleSource,
	scanPluginManifests,
} from '../../../../build/plugin-manifests.mjs';

const realPluginsDir = fileURLToPath(new URL('.', import.meta.url));

function withTempPlugins(run: (dir: string) => void): void {
	const dir = mkdtempSync(path.join(tmpdir(), 'manox-plugins-'));
	try {
		run(dir);
	} finally {
		rmSync(dir, { recursive: true, force: true });
	}
}

function addPlugin(
	dir: string,
	name: string,
	manifest: Record<string, unknown>,
	entryFile = 'client.tsx',
): void {
	const pdir = path.join(dir, name);
	mkdirSync(pdir, { recursive: true });
	writeFileSync(path.join(pdir, 'manifest.json'), JSON.stringify(manifest));
	// The scanner requires the entry file to exist on disk.
	writeFileSync(path.join(pdir, entryFile), 'export {};\n');
}

describe('scanPluginManifests (real plugins)', () => {
	it('discovers the conversation-info sample with its declared slot', () => {
		const found = scanPluginManifests(realPluginsDir);
		expect(found.map((m) => m.id)).toContain('conversation-info');
		const ci = found.find((m) => m.id === 'conversation-info')!;
		expect(ci).toMatchObject({
			id: 'conversation-info',
			entry: 'client.tsx',
			slots: ['conversation.session.header.utilities'],
		});
		// The resolved entry points at a real file.
		expect(ci.file.endsWith(path.join('conversation-info', 'client.tsx'))).toBe(true);
	});
});

describe('scanPluginManifests (synthetic fixtures)', () => {
	it('sorts by id and inlines one import per client entry', () => {
		withTempPlugins((dir) => {
			addPlugin(dir, 'zeta', { id: 'zeta', entry: 'client.tsx', slots: [] });
			addPlugin(dir, 'alpha', { id: 'alpha', entry: 'client.tsx', slots: ['shell.overlay'] });
			const found = scanPluginManifests(dir);
			expect(found.map((m) => m.id)).toEqual(['alpha', 'zeta']);
			const source = buildPluginsModuleSource(found);
			// Deterministic inline order (by id), each an absolute side-effect import.
			expect(source).toContain(`import ${JSON.stringify(found[0]!.file)};`);
			expect(source).toContain(`import ${JSON.stringify(found[1]!.file)};`);
			expect(source.trimEnd().split('\n')).toHaveLength(2);
		});
	});

	it('rejects a manifest whose id does not match its folder', () => {
		withTempPlugins((dir) => {
			addPlugin(dir, 'wrong', { id: 'other', entry: 'client.tsx', slots: [] });
			expect(() => scanPluginManifests(dir)).toThrow(/≠ folder/);
		});
	});

	it('rejects a manifest whose entry file is missing', () => {
		withTempPlugins((dir) => {
			const pdir = path.join(dir, 'ghost');
			mkdirSync(pdir, { recursive: true });
			writeFileSync(
				path.join(pdir, 'manifest.json'),
				JSON.stringify({ id: 'ghost', entry: 'nope.tsx', slots: [] }),
			);
			expect(() => scanPluginManifests(dir)).toThrow(/entry "nope.tsx" not found/);
		});
	});

	it('an empty plugins dir yields nothing', () => {
		withTempPlugins((dir) => {
			expect(scanPluginManifests(dir)).toEqual([]);
			expect(buildPluginsModuleSource([])).toBe('\n');
		});
	});
});
