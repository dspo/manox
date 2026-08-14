import type { ElementType } from 'react';
import { memo } from 'react';

import { cn } from '../../lib/utils';

export interface TextShimmerProps {
  children: string;
  as?: ElementType;
  className?: string;
  duration?: number;
}

// The webview CSP forbids inline style attributes, so durations map to
// pre-generated utility classes; unmapped values keep the 2s theme default
// (--animate-shimmer in tokens.css).
const durationClasses: Record<number, string> = {
  1: '[animation-duration:1s]',
  2: '[animation-duration:2s]',
};

const ShimmerComponent = ({
  children,
  as: Component = 'p',
  className,
  duration = 2,
}: TextShimmerProps) => (
  <Component
    className={cn(
      'animate-shimmer inline-block bg-[linear-gradient(90deg,var(--color-muted-foreground)_40%,var(--color-foreground)_50%,var(--color-muted-foreground)_60%)] bg-[length:200%_100%] bg-clip-text text-transparent',
      durationClasses[duration],
      className,
    )}
  >
    {children}
  </Component>
);

export const Shimmer = memo(ShimmerComponent);
