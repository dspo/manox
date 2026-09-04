import { createRoot } from 'react-dom/client';

import { App } from './app';
import { ErrorBoundary } from './components/error-boundary';
// Side-effect imports: the first-batch slot declarations + the built-in
// occupants (§G). The conversation-info extension plugin registers in step 4.
import './slots.defaults';

createRoot(document.getElementById('root')!).render(
  <ErrorBoundary>
    <App />
  </ErrorBoundary>,
);
