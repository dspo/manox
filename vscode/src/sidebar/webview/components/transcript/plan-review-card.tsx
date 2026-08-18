// Plan review card: the model submitted a plan for approval. The four
// verdicts mirror the gpui host's review card (ExecuteFresh / ExecuteCompact
// / ExecuteKeep / Refine); every verdict clears the local card.

import { ThreadApi } from '../../api/client';
import { t } from '../../lib/i18n';
import { store } from '../../state/bridge';
import type { TranscriptItem } from '../../state/store';
import {
  Confirmation,
  ConfirmationAction,
  ConfirmationActions,
  ConfirmationRequest,
  ConfirmationTitle,
} from '../ai/confirmation';
import { MarkdownContent } from '../ai/markdown-content';

export type PlanReviewItem = Extract<TranscriptItem, { kind: 'plan_review' }>;

export type PlanReviewCardProps = {
  item: PlanReviewItem;
  sessionId: string;
  cwd: string;
};

export const PlanReviewCard = ({ item, sessionId, cwd }: PlanReviewCardProps) => {
  const api = new ThreadApi(sessionId);
  const verdict = (choice: 'execute_keep' | 'execute_compact' | 'refine') => {
    api.planVerdict(choice);
    store.clearPlanReview(sessionId);
  };
  const executeFresh = () => {
    api.planExecuteFresh(item.planFile, cwd);
    store.clearPlanReview(sessionId);
  };

  return (
    <Confirmation
      approval={{ id: item.id, approved: false }}
      state="approval-requested"
      variant="default"
    >
      <ConfirmationTitle>{item.title}</ConfirmationTitle>
      <ConfirmationRequest>
        {item.content ? (
          <MarkdownContent content={item.content} />
        ) : (
          <div className="text-sm">{t('plan_review_empty')}</div>
        )}
      </ConfirmationRequest>
      <ConfirmationActions>
        <ConfirmationAction onClick={() => verdict('refine')} variant="outline">
          {t('plan_refine')}
        </ConfirmationAction>
        <ConfirmationAction onClick={executeFresh} variant="outline">
          {t('plan_execute_fresh')}
        </ConfirmationAction>
        <ConfirmationAction onClick={() => verdict('execute_keep')}>
          {t('plan_execute')}
        </ConfirmationAction>
        <ConfirmationAction onClick={() => verdict('execute_compact')}>
          {t('plan_execute_compact')}
        </ConfirmationAction>
      </ConfirmationActions>
    </Confirmation>
  );
};
