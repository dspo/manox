import { memo } from 'react';
import ReactMarkdown from 'react-markdown';
import rehypeSanitize from 'rehype-sanitize';
import remarkGfm from 'remark-gfm';

import { cn } from '../../lib/utils';

export interface MarkdownContentProps {
  content: string;
  className?: string;
}

// Sanitized markdown for streamed assistant/thinking text; rendered as React
// elements, never innerHTML.
export const MarkdownContent = memo(function MarkdownContent({
  content,
  className,
}: MarkdownContentProps) {
  return (
    <div
      className={cn(
        'font-transcript text-sm [&>*:first-child]:mt-0 [&>*:last-child]:mb-0',
        className,
      )}
    >
      <ReactMarkdown
        rehypePlugins={[rehypeSanitize]}
        remarkPlugins={[remarkGfm]}
        components={{
          a: ({ className: c, ...props }) => (
            <a
              className={cn('text-primary underline underline-offset-2', c)}
              rel="noreferrer"
              target="_blank"
              {...props}
            />
          ),
          code: ({ className: c, ...props }) => (
            <code
              className={cn(
                'font-code rounded bg-muted px-1 py-0.5 text-[0.9em]',
                '[pre_&]:rounded-none [pre_&]:bg-transparent [pre_&]:p-0',
                c,
              )}
              {...props}
            />
          ),
          pre: ({ className: c, ...props }) => (
            <pre
              className={cn(
                'font-code max-h-[180px] overflow-auto rounded-md border bg-muted/50 p-3 text-xs',
                c,
              )}
              {...props}
            />
          ),
          ul: ({ className: c, ...props }) => (
            <ul className={cn('list-disc pl-5', c)} {...props} />
          ),
          ol: ({ className: c, ...props }) => (
            <ol className={cn('list-decimal pl-5', c)} {...props} />
          ),
        }}
      >
        {content}
      </ReactMarkdown>
    </div>
  );
});
