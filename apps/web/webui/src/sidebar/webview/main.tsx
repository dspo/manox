import { createRoot } from 'react-dom/client';

import { App } from './app';
import { ErrorBoundary } from './components/error-boundary';
// Side-effect imports: the first-batch slot declarations + the built-in
// occupants (§G), then the extension plugins. `manox:plugins` is a build-time
// virtual module that esbuild fills from the plugin manifest scan (T8 §H,
// see `build/plugin-manifests.mjs`) — dropping a
// `plugins/<name>/{manifest.json,client.tsx}` folder registers it, with no
// hand-maintained import list here.
import './slots.defaults';
import 'manox:plugins';

createRoot(document.getElementById('root')!).render(
  <ErrorBoundary>
    <App />
  </ErrorBoundary>,
);
