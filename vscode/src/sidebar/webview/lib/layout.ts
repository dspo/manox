// Breakpoint layout for the conversation view: the info card joins the
// conversation column as the container widens, then the session list.
// The card breakpoint is the message-content floor plus the card gutter,
// so the content never drops below the floor in any layout.

// The widths the breakpoints derive from; declared first so the derived
// constants below can reference them.
export const INFO_CARD_WIDTH_PX = 260;
// Right gutter the transcript reserves so messages clear the floating card
// and its shadow.
export const INFO_CARD_GUTTER_PX = INFO_CARD_WIDTH_PX + 36;
export const CONVERSATION_MIN_PX = 480;
// Sash width the session list leaves beside the conversation column
// (SidebarSash's w-1).
export const SASH_PX = 4;

export const INFO_CARD_BREAKPOINT_PX = CONVERSATION_MIN_PX + INFO_CARD_GUTTER_PX;
export const SESSION_LIST_BREAKPOINT_PX = 1120;

export type ChatLayout = 'conversation' | 'conversation-info' | 'list-conversation-info';

export function chatLayoutForWidth(width: number): ChatLayout {
  if (width >= SESSION_LIST_BREAKPOINT_PX) return 'list-conversation-info';
  if (width >= INFO_CARD_BREAKPOINT_PX) return 'conversation-info';
  return 'conversation';
}

/** Widest the session list may grow while the message content keeps at
 * least CONVERSATION_MIN_PX beside the card gutter and the sash. */
export function maxSessionListWidth(width: number): number {
  return Math.max(0, width - CONVERSATION_MIN_PX - INFO_CARD_GUTTER_PX - SASH_PX);
}
