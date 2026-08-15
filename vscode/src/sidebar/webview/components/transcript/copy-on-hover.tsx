// Copy affordance revealed on hover of its owning message block. The
// clipboard API can be unavailable inside a webview, so a hidden textarea
// plus execCommand stands in as a silent fallback.

import { Check, Copy } from 'lucide-react';
import { useState } from 'react';

import { t } from '../../lib/i18n';
import { cn } from '../../lib/utils';

export async function copyText(text: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
    return;
  } catch {
    // Fall through to the legacy path below.
  }
  const area = document.createElement('textarea');
  area.value = text;
  area.style.position = 'fixed';
  area.style.opacity = '0';
  document.body.appendChild(area);
  area.select();
  document.execCommand('copy');
  document.body.removeChild(area);
}

export const CopyOnHover = ({ text, className }: { text: string; className?: string }) => {
  const [copied, setCopied] = useState(false);
  return (
    <button
      className={cn(
        'text-muted-foreground hover:bg-accent hover:text-foreground shrink-0 cursor-pointer rounded p-1 opacity-0 transition-opacity group-hover:opacity-100',
        copied && 'opacity-100',
        className,
      )}
      onClick={() => {
        void copyText(text).then(() => {
          setCopied(true);
          setTimeout(() => setCopied(false), 1200);
        });
      }}
      title={t('copy')}
      type="button"
    >
      {copied ? <Check className="size-3.5 text-success" /> : <Copy className="size-3.5" />}
    </button>
  );
};
