// Type surface for the build-time plugin manifest scanner so the vitest
// manifest-inline snapshot test (and tsc) can import the `.mjs` scanner with
// full types. The runtime lives in `plugin-manifests.mjs`.

export interface PluginManifest {
  id: string;
  name: string;
  version: string;
  entry: string;
  slots: string[];
  /** Absolute path to the resolved client entry file. */
  file: string;
}

export function scanPluginManifests(pluginsDir: string): PluginManifest[];

export function buildPluginsModuleSource(manifests: PluginManifest[]): string;
