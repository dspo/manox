// Container-width measurement for breakpoint layouts. Sidebar width depends
// on the panel arrangement rather than the viewport, so a ResizeObserver
// stands in for container queries.

import { useEffect, useRef, useState } from 'react';

export function useContainerWidth<T extends HTMLElement = HTMLDivElement>(): {
  ref: React.RefObject<T | null>;
  width: number;
} {
  const ref = useRef<T>(null);
  const [width, setWidth] = useState(0);

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const observer = new ResizeObserver(() => setWidth(el.clientWidth));
    observer.observe(el);
    setWidth(el.clientWidth);
    return () => observer.disconnect();
  }, []);

  return { ref, width };
}
