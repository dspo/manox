// Pure session-list shaping shared by the home view and its tests.

import type { ThreadListItem } from '../../../protocol';

/** Active rows (pinned first, newest activity first) and archived rows
 * (newest first) partitioned for the list and its "more" section. */
export function partitionSessions(threads: ThreadListItem[]): {
  active: ThreadListItem[];
  archived: ThreadListItem[];
} {
  const byRecent = (a: ThreadListItem, b: ThreadListItem) => b.updated_at - a.updated_at;
  const active = threads
    .filter((item) => !item.archived)
    .sort((a, b) => (a.pinned !== b.pinned ? (a.pinned ? -1 : 1) : byRecent(a, b)));
  const archived = threads.filter((item) => item.archived).sort(byRecent);
  return { active, archived };
}
