import { createRoot } from 'react-dom/client';

import { App } from './app';
import { ErrorBoundary } from './components/error-boundary';

createRoot(document.getElementById('root')!).render(
  <ErrorBoundary>
    <App />
  </ErrorBoundary>,
);
