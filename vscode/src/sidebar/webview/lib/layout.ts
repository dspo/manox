// Breakpoint layout for the conversation view: columns join as the
// container widens so each column keeps a non-cramped width.
export const TWO_COL_BREAKPOINT_PX = 760;
export const THREE_COL_BREAKPOINT_PX = 1120;
export const INFO_PANEL_WIDTH_PX = 260;
export const CONVERSATION_MIN_PX = 480;
// Chrome beside the conversation column the sash clamp must reserve so the
// conversation never drops below CONVERSATION_MIN_PX: sash 4px + info column
// border 1px + info wrapper horizontal padding 16px.
export const NON_FLEX_OVERHEAD_PX = 21;

export type ChatLayout = 'conversation' | 'conversation-info' | 'list-conversation-info';

export function chatLayoutForWidth(width: number): ChatLayout {
  if (width >= THREE_COL_BREAKPOINT_PX) return 'list-conversation-info';
  if (width >= TWO_COL_BREAKPOINT_PX) return 'conversation-info';
  return 'conversation';
}

/** Widest the session list may grow while the conversation column keeps at
 * least CONVERSATION_MIN_PX in the three-column layout. */
export function maxSessionListWidth(width: number): number {
  return Math.max(0, width - CONVERSATION_MIN_PX - INFO_PANEL_WIDTH_PX - NON_FLEX_OVERHEAD_PX);
}
