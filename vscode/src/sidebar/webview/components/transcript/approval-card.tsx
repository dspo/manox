import { api } from '../../api/client';
import { store } from '../../state/bridge';
import type { TranscriptItem } from '../../state/store';
import {
  Confirmation,
  ConfirmationAccepted,
  ConfirmationAction,
  ConfirmationActions,
  ConfirmationRejected,
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

const decide = (id: string, allow: boolean) => {
  api.approve(id, allow);
  store.decideApproval(id, allow);
};

export type ApprovalCardProps = {
  item: ApprovalItem;
};

export const ApprovalCard = ({ item }: ApprovalCardProps) => {
  const inputPreview = formatInput(item.input);

  return (
    <Confirmation
      approval={{ id: item.id, approved: item.decided === 'approved' }}
      state={item.decided ? 'approval-responded' : 'approval-requested'}
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
      <ConfirmationAccepted>
        <div className="text-muted-foreground text-sm">Approved</div>
      </ConfirmationAccepted>
      <ConfirmationRejected>
        <div className="text-muted-foreground text-sm">Denied</div>
      </ConfirmationRejected>
      <ConfirmationActions>
        <ConfirmationAction onClick={() => decide(item.id, false)} variant="outline">
          Deny
        </ConfirmationAction>
        <ConfirmationAction onClick={() => decide(item.id, true)}>Approve</ConfirmationAction>
      </ConfirmationActions>
    </Confirmation>
  );
};
