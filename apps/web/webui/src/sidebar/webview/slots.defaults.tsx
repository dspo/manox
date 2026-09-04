// Default slot occupants (§G "行为不变，装配方式改变"): the built-in webui
// surfaces that ship in the first-batch slots. Each owner renders its slot
// outlet (see `slots.outlet.tsx`); the occupant is contributed here through
// `inject` + `register` and reads shared data only through the standard
// store hooks — a slot component never imports another component and never
// reaches into store internals. The conversation-info plugin contributes its
// own occupant separately (see `plugins/conversation-info/client.tsx`).
//
// Importing this module has the side effect of loading `slots.registry`
// (which declares the first-batch slots at runtime), so registration always
// finds a live target.

import { Search, Settings } from 'lucide-react';

import { register } from './state/slots';
import './slots.registry';
import { t } from './lib/i18n';
import { toggleOverlay, useOverlayOpen } from './lib/ui-overlays';
import { useModels, useReasoningEffort } from './state/hooks';
import { SettingsDialog } from './components/settings-view';
import { ModelPicker } from './components/chrome/model-picker';

// ── sidebar.workspaces.footer.action ───────────────────────────────────────
// A "Settings" trigger beside the sessions header. It flips the `settings`
// overlay flag; the shell overlay outlet (app.tsx) renders the dialog, so no
// component import crosses the boundary.
register(
  { name: 'sidebar.workspaces.footer.action', id: 'settings', order: 10, registrant: 'shell' },
  () => (
    <button
      className="text-muted-foreground hover:bg-muted hover:text-foreground flex size-6 cursor-pointer items-center justify-center rounded transition-colors"
      onClick={() => toggleOverlay('settings')}
      title={t('settings')}
      type="button"
    >
      <Settings className="size-4" />
    </button>
  ),
);

// ── conversation.session.header.utilities ──────────────────────────────────
// The turn-navigator search chip (the header's existing Search button, now
// contributed through the slot). It toggles the `turn-navigator` overlay
// flag; the conversation view (the navigator's data owner) reads it and draws
// the overlay — the chip never touches transcript data.
register(
  { name: 'conversation.session.header.utilities', id: 'turn-navigator', order: 10, registrant: 'shell' },
  () => (
    <button
      className="text-muted-foreground hover:bg-accent flex size-6 cursor-pointer items-center justify-center rounded transition-colors"
      onClick={() => toggleOverlay('turn-navigator')}
      title={t('turn_navigator_title')}
      type="button"
    >
      <Search className="size-4" />
    </button>
  ),
);

// ── conversation.composer.dock ─────────────────────────────────────────────
// The model picker chip. Shared data (the model registry and the thread's
// reasoning-effort projection) is read through the store hooks; the owner
// props carry only what the render site alone knows (the current display ref
// the parent is showing, the disabled gate, the draft-mode select callback).
register(
  { name: 'conversation.composer.dock', id: 'model-picker', order: 10, registrant: 'shell' },
  ({ sessionId, currentModelRef, disabled, onModelChange }) => {
    const models = useModels();
    const reasoningEffort = useReasoningEffort(sessionId);
    return (
      <ModelPicker
        currentModelRef={currentModelRef}
        disabled={disabled}
        models={models}
        onSelect={onModelChange}
        reasoningEffort={reasoningEffort ?? 'high'}
        sessionId={sessionId}
      />
    );
  },
);

// ── settings.section ───────────────────────────────────────────────────────
// The built-in "General" settings section: a read-only model-registry summary
// to seed the overlay. Real settings land here as more sections register.
register(
  { name: 'settings.section', id: 'general', order: 10, registrant: 'shell' },
  () => {
    const models = useModels();
    return (
      <section className="space-y-1.5">
        <h3 className="font-medium text-[11px] tracking-wide text-muted-foreground">
          {t('settings_general')}
        </h3>
        <p className="text-muted-foreground text-xs">{t('settings_general_desc')}</p>
        <p className="font-code text-xs">{t('settings_models', models.length)}</p>
      </section>
    );
  },
);

// ── shell.overlay ──────────────────────────────────────────────────────────
// The settings modal, mounted through the app-level overlay slot: the
// Settings trigger (footer action) flips the `settings` overlay flag and this
// occupant reads it. Plugins add their own overlays by registering here too —
// the shell renders one outlet and imports none of them.
register(
  { name: 'shell.overlay', id: 'settings', order: 10, registrant: 'shell' },
  () => {
    const open = useOverlayOpen('settings');
    if (!open) return null;
    return <SettingsDialog onClose={() => toggleOverlay('settings')} />;
  },
);
