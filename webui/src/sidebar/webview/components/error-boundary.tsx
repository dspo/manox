// Last-resort render-error surface: a throw anywhere in the tree unmounts it
// into this card (error + stack on screen and in the console) instead of a
// blank webview, so a regression is visible and diagnosable rather than silent.

import { Component, type ErrorInfo, type ReactNode } from 'react';

import { t } from '../lib/i18n';

interface ErrorBoundaryProps {
  children: ReactNode;
}

interface ErrorBoundaryState {
  error: Error | null;
}

export class ErrorBoundary extends Component<ErrorBoundaryProps, ErrorBoundaryState> {
  state: ErrorBoundaryState = { error: null };

  static getDerivedStateFromError(error: Error): ErrorBoundaryState {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo): void {
    console.error('manox webview crashed:', error, info.componentStack);
  }

  render(): ReactNode {
    const { error } = this.state;
    if (!error) return this.props.children;
    return (
      <div className="font-chrome bg-background text-foreground flex h-screen flex-col items-center justify-center gap-3 p-6 text-center">
        <h1 className="font-medium text-sm">{t('crashed_title')}</h1>
        <pre className="text-muted-foreground font-code max-h-40 w-full overflow-auto text-left text-[11px] whitespace-pre-wrap break-all">
          {error.message}
          {error.stack ? `\n${error.stack}` : ''}
        </pre>
        <button
          className="bg-primary/20 text-primary hover:bg-primary/30 cursor-pointer rounded-full px-4 py-1 text-xs"
          onClick={() => location.reload()}
          type="button"
        >
          {t('crashed_reload')}
        </button>
      </div>
    );
  }
}
