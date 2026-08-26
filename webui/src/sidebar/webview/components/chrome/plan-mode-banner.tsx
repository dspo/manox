// Plan-mode banner: a prominent, always-visible strip while the session
// plans. The working tree is read-only during planning, so the state must
// never be silent — and the Exit action is the escape hatch when a review
// card is missed or the model stalls in research.

import { LayoutDashboard, X } from 'lucide-react';

import { ThreadApi } from '../../api/client';
import { t } from '../../lib/i18n';

export const PlanModeBanner = ({ sessionId }: { sessionId: string }) => (
  <div className="flex items-center gap-2 border-b border-warning/40 bg-warning/10 px-3 py-1.5">
    <LayoutDashboard className="text-warning size-4 shrink-0" />
    <div className="min-w-0 flex-1">
      <span className="text-warning text-xs font-semibold">{t('plan_mode_banner')}</span>
      <span className="text-muted-foreground ml-2 text-xs">{t('plan_mode_banner_desc')}</span>
    </div>
    <button
      className="text-warning hover:bg-warning/20 flex shrink-0 cursor-pointer items-center gap-1 rounded px-2 py-0.5 text-xs font-medium transition-colors"
      onClick={() => new ThreadApi(sessionId).setPlanMode(false)}
      title={t('plan_mode_exit')}
      type="button"
    >
      <X className="size-3" />
      {t('plan_mode_exit')}
    </button>
  </div>
);
