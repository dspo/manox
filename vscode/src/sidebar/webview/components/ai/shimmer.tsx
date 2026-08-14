import type { CSSProperties, ElementType } from 'react';
import { memo } from 'react';

import { cn } from '../../lib/utils';

export interface TextShimmerProps {
  children: string;
  as?: ElementType;
  className?: string;
  duration?: number;
}

const ShimmerComponent = ({
  children,
  as: Component = 'p',
  className,
  duration = 2,
}: TextShimmerProps) => (
  <Component
    className={cn(
      'animate-shimmer inline-block bg-[linear-gradient(90deg,var(--color-muted-foreground)_40%,var(--color-foreground)_50%,var(--color-muted-foreground)_60%)] bg-[length:200%_100%] bg-clip-text text-transparent',
      className,
    )}
    style={{ animationDuration: `${duration}s` } as CSSProperties}
  >
    {children}
  </Component>
);

export const Shimmer = memo(ShimmerComponent);
