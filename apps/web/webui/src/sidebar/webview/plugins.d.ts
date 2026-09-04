// Ambient declaration for the build-time plugin virtual module (§H): esbuild's
// `manox-plugins` plugin serves `manox:plugins` as one side-effect import per
// discovered plugin manifest (see `build/plugin-manifests.mjs`). It has no
// exports — importing it runs the plugins' slot registrations at bundle load.
declare module 'manox:plugins' {}
