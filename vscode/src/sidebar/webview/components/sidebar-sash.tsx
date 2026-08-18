// Draggable sash and the sidebar width it controls, shared by the wide
// layouts that pair a session column with a main column: the home screen
// (sessions | composer) and the three-column conversation. One localStorage
// key keeps the two views' list widths in sync.

import { useEffect, useRef, useState, type PointerEvent } from 'react';

import { cn } from '../lib/utils';

export const SIDEBAR_MIN_PX = 200;
export const SIDEBAR_DEFAULT_PX = 300;
const SIDEBAR_WIDTH_KEY = 'manox.sessions-sidebar-width';

export function useSidebarWidth(maxWidth: number): {
  width: number;
  sashActive: boolean;
  onDoubleClick: () => void;
  onPointerDown: (e: PointerEvent<HTMLDivElement>) => void;
  onPointerMove: (e: PointerEvent<HTMLDivElement>) => void;
  onPointerUp: () => void;
} {
  const [width, setWidth] = useState(() => {
    const saved = Number(localStorage.getItem(SIDEBAR_WIDTH_KEY));
    return saved >= SIDEBAR_MIN_PX ? saved : SIDEBAR_DEFAULT_PX;
  });
  const [sashActive, setSashActive] = useState(false);
  const sashDrag = useRef<{ startX: number; startWidth: number } | null>(null);

  useEffect(() => {
    localStorage.setItem(SIDEBAR_WIDTH_KEY, String(width));
  }, [width]);

  // A saved width can exceed what the current container affords; rendering
  // clamps it while the drag handlers keep writing the raw state.
  const listWidth = Math.min(width, maxWidth);

  const onPointerDown = (e: PointerEvent<HTMLDivElement>) => {
    sashDrag.current = { startX: e.clientX, startWidth: listWidth };
    setSashActive(true);
    e.currentTarget.setPointerCapture(e.pointerId);
  };
  const onPointerMove = (e: PointerEvent<HTMLDivElement>) => {
    if (!sashDrag.current) return;
    const next = sashDrag.current.startWidth + e.clientX - sashDrag.current.startX;
    setWidth(Math.max(SIDEBAR_MIN_PX, Math.min(next, maxWidth)));
  };
  const onPointerUp = () => {
    sashDrag.current = null;
    setSashActive(false);
  };
  const onDoubleClick = () => setWidth(SIDEBAR_DEFAULT_PX);

  return { width: listWidth, sashActive, onDoubleClick, onPointerDown, onPointerMove, onPointerUp };
}

export type SidebarSashProps = {
  sashActive: boolean;
  onDoubleClick: () => void;
  onPointerDown: (e: PointerEvent<HTMLDivElement>) => void;
  onPointerMove: (e: PointerEvent<HTMLDivElement>) => void;
  onPointerUp: () => void;
};

export const SidebarSash = ({
  sashActive,
  onDoubleClick,
  onPointerDown,
  onPointerMove,
  onPointerUp,
}: SidebarSashProps) => (
  <div
    aria-orientation="vertical"
    className="relative z-10 w-1 shrink-0 cursor-col-resize touch-none"
    onDoubleClick={onDoubleClick}
    onPointerDown={onPointerDown}
    onPointerMove={onPointerMove}
    onPointerUp={onPointerUp}
    role="separator"
  >
    <div
      className={cn(
        'bg-border absolute inset-y-0 left-1/2 w-px -translate-x-1/2 transition-colors',
        sashActive ? 'bg-ring' : 'hover:bg-ring',
      )}
    />
  </div>
);
