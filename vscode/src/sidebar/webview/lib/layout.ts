// Breakpoint layout for the conversation view: columns join as the
// container widens so each column keeps a non-cramped width.
export const TWO_COL_BREAKPOINT_PX = 760;
export const THREE_COL_BREAKPOINT_PX = 1120;
export const INFO_PANEL_WIDTH_PX = 260;
export const CONVERSATION_MIN_PX = 480;

export type ChatLayout = 'conversation' | 'conversation-info' | 'list-conversation-info';

export function chatLayoutForWidth(width: number): ChatLayout {
  if (width >= THREE_COL_BREAKPOINT_PX) return 'list-conversation-info';
  if (width >= TWO_COL_BREAKPOINT_PX) return 'conversation-info';
  return 'conversation';
}
