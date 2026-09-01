import { ArrowDown } from 'lucide-react';
import type { ComponentProps } from 'react';
import { useCallback } from 'react';
import { StickToBottom, useStickToBottomContext } from 'use-stick-to-bottom';

import { cn } from '../../lib/utils';
import { Button } from '../ui/button';

export type ConversationProps = ComponentProps<typeof StickToBottom>;

export const Conversation = ({ className, ...props }: ConversationProps) => (
  <StickToBottom
    className={cn('relative flex-1 overflow-y-hidden', className)}
    initial="smooth"
    resize="smooth"
    role="log"
    {...props}
  />
);

export type ConversationContentProps = ComponentProps<typeof StickToBottom.Content>;

export const ConversationContent = ({ className, ...props }: ConversationContentProps) => (
  <StickToBottom.Content
    className={cn('flex flex-col gap-4 p-4', className)}
    {...props}
  />
);

export type ConversationEmptyStateProps = ComponentProps<'div'> & {
  title?: string;
  description?: string;
  icon?: React.ReactNode;
};

export const ConversationEmptyState = ({
  className,
  title = 'No messages yet',
  description = 'Start a conversation to see messages here',
  icon,
  children,
  ...props
}: ConversationEmptyStateProps) => (
  <div
    className={cn(
      'flex size-full flex-col items-center justify-center gap-3 p-8 text-center',
      className,
    )}
    {...props}
  >
    {children ?? (
      <>
        {icon && <div className="text-muted-foreground">{icon}</div>}
        <div className="space-y-1">
          <h3 className="font-medium text-sm">{title}</h3>
          {description && <p className="text-muted-foreground text-sm">{description}</p>}
        </div>
      </>
    )}
  </div>
);

export type ConversationScrollButtonProps = ComponentProps<typeof Button> & {
  /** Right inset of the scrollable content (floating info card gutter);
   * keeps the button centered on the messages rather than the container. */
  rightInsetPx?: number;
};

export const ConversationScrollButton = ({
  className,
  rightInsetPx,
  ...props
}: ConversationScrollButtonProps) => {
  const { isAtBottom, scrollToBottom } = useStickToBottomContext();

  const handleScrollToBottom = useCallback(() => {
    scrollToBottom();
  }, [scrollToBottom]);

  return (
    !isAtBottom && (
      <Button
        {...props}
        className={cn(
          'absolute bottom-4 left-[50%] translate-x-[-50%] rounded-full bg-background hover:bg-muted',
          className,
        )}
        style={rightInsetPx ? { left: `calc(50% - ${rightInsetPx / 2}px)` } : undefined}
        onClick={handleScrollToBottom}
        size="icon"
        type="button"
        variant="outline"
      >
        <ArrowDown className="size-4" />
      </Button>
    )
  );
};
