// Settings overlay: a modal dialog whose body is the `settings.section` slot
// outlet (§G). Sections contribute through registration only; the dialog
// itself knows nothing about its occupants.

import { useEffect, type ReactNode } from 'react';

import { t } from '../lib/i18n';
import { Slot } from '../slots.outlet';

export type SettingsDialogProps = {
  onClose: () => void;
};

export const SettingsDialog = ({ onClose }: SettingsDialogProps) => {
  useEffect(() => {
    const onKey = (event: KeyboardEvent): void => {
      if (event.key === 'Escape') onClose();
    };
    document.addEventListener('keydown', onKey);
    return () => document.removeEventListener('keydown', onKey);
  }, [onClose]);

  return (
    <div
      className="bg-background/70 fixed inset-0 z-40 flex items-center justify-center p-6"
      onClick={onClose}
      role="presentation"
    >
      <div
        aria-modal="true"
        className="font-chrome bg-card flex max-h-full w-full max-w-md flex-col gap-3 overflow-hidden rounded-lg border border-border p-4 text-sm shadow-lg"
        onClick={(event) => event.stopPropagation()}
        role="dialog"
      >
        <div className="flex items-center">
          <h2 className="font-medium">{t('settings')}</h2>
          <button
            className="text-muted-foreground hover:text-foreground ml-auto cursor-pointer rounded px-2 text-xs"
            onClick={onClose}
            type="button"
          >
            {t('close')}
          </button>
        </div>
        <div className="min-h-0 flex-1 space-y-4 overflow-y-auto">
          <SectionOutlet />
        </div>
      </div>
    </div>
  );
};

/** The `settings.section` outlet — the dialog contributes no sections of
 * its own; the default `general` section is registered in
 * `slots.defaults.tsx`. */
const SectionOutlet = (): ReactNode => <Slot name="settings.section" owner={{}} />;
