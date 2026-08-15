// User message rendered as a gate frame: a rounded border tinted by the
// approval mode, opened at bottom center by a background-colored strip.

import type { ApprovalMode } from '../../../../protocol';
import { t } from '../../lib/i18n';
import { cn } from '../../lib/utils';
import type { TranscriptItem } from '../../state/store';
import { CopyOnHover } from './copy-on-hover';

const timeFormat = new Intl.DateTimeFormat(undefined, { hour: '2-digit', minute: '2-digit' });

export type UserMessageProps = {
  item: Extract<TranscriptItem, { kind: 'user' }>;
  approvalMode: ApprovalMode;
};

export const UserMessage = ({ item, approvalMode }: UserMessageProps) => {
  const time = item.timestamp ? timeFormat.format(item.timestamp * 1000) : null;
  const visibleImages = item.images?.filter((img) => img.data) ?? [];
  return (
    <div
      className={cn(
        'group relative rounded-xl border-2 px-4 pt-2 pb-3',
        approvalMode === 'danger' ? 'border-danger' : 'border-info',
      )}
    >
      <div className="bg-background pointer-events-none absolute bottom-[-2px] left-1/2 h-[2px] w-2/5 -translate-x-1/2" />
      <div className="text-muted-foreground mb-1 flex items-center gap-1.5 text-xs">
        <span className="text-foreground font-medium">{t('you')}</span>
        {time && (
          <>
            <span>›</span>
            <span>{time}</span>
          </>
        )}
        {item.modelId && (
          <>
            <span>›</span>
            <span className="truncate">{item.modelId}</span>
          </>
        )}
        <CopyOnHover className="ml-auto" text={item.text} />
      </div>
      <p className="whitespace-pre-wrap text-sm">{item.displayText ?? item.text}</p>
      {visibleImages.length > 0 && (
        <div className="mt-2 flex flex-wrap gap-2">
          {visibleImages.map((img, index) => (
            <img
              alt={`attachment ${index + 1}`}
              className="h-16 w-16 rounded-md border object-cover"
              key={index}
              src={img.data ?? undefined}
            />
          ))}
        </div>
      )}
    </div>
  );
};
