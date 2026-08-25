import { ThreadApi } from '../../api/client';
import { store } from '../../state/bridge';
import type { TranscriptItem } from '../../state/store';
import {
  Confirmation,
  ConfirmationAction,
  ConfirmationActions,
  ConfirmationRequest,
  ConfirmationTitle,
} from '../ai/confirmation';

export type ApprovalItem = Extract<TranscriptItem, { kind: 'approval' }>;

const INPUT_CAP = 4_096;

function formatInput(input: unknown): string | null {
  if (input === undefined || input === null) {
    return null;
  }
  let text: string;
  try {
    text = typeof input === 'string' ? input : JSON.stringify(input, null, 2);
  } catch {
    return null;
  }
  if (!text) {
    return null;
  }
  return text.length > INPUT_CAP ? `${text.slice(0, INPUT_CAP)}…` : text;
}

export type ApprovalCardProps = {
  item: ApprovalItem;
  sessionId: string;
};

// Deciding removes the card from the transcript; the resulting tool events
// render as ordinary tool items.
export const ApprovalCard = ({ item, sessionId }: ApprovalCardProps) => {
  const inputPreview = formatInput(item.input);
  const decide = (allow: boolean) => {
    new ThreadApi(sessionId).approve(item.id, allow);
    store.decideApproval(sessionId, item.id);
  };

  return (
    <Confirmation
      approval={{ id: item.id, approved: false }}
      state="approval-requested"
      variant="default"
    >
      <ConfirmationTitle>{item.toolName}</ConfirmationTitle>
      <ConfirmationRequest>
        <div className="text-sm">{item.summary}</div>
        {inputPreview && (
          <pre className="font-code max-h-[180px] overflow-auto rounded-md bg-muted/50 p-2 text-xs">
            {inputPreview}
          </pre>
        )}
      </ConfirmationRequest>
      <ConfirmationActions>
        <ConfirmationAction onClick={() => decide(false)} variant="outline">
          Deny
        </ConfirmationAction>
        <ConfirmationAction onClick={() => decide(true)}>Approve</ConfirmationAction>
      </ConfirmationActions>
    </Confirmation>
  );
};
