// Pure session-list shaping shared by the home view and its tests.

import type { ThreadListItem } from '../../../protocol';

/** One row in the list tree: the session plus its nested team members. */
export interface SessionTreeNode {
  item: ThreadListItem;
  children: SessionTreeNode[];
}

/** Active rows (pinned first, newest activity first) and archived rows
 * (newest first) partitioned for the list and its "more" section, each
 * shaped as a forest whose team members nest under their leader. Children
 * keep the partition's sort order relative to each other; an orphan (leader
 * missing from the list) stays top-level. */
export function partitionSessions(threads: ThreadListItem[]): {
  active: SessionTreeNode[];
  archived: SessionTreeNode[];
} {
  const byRecent = (a: ThreadListItem, b: ThreadListItem) => b.updated_at - a.updated_at;
  const sort = (rows: ThreadListItem[]) =>
    rows.sort((a, b) => (a.pinned !== b.pinned ? (a.pinned ? -1 : 1) : byRecent(a, b)));
  const forest = (rows: ThreadListItem[]): SessionTreeNode[] => {
    const nodes = new Map(
      rows.map((item) => [item.id, { item, children: [] as SessionTreeNode[] }]),
    );
    const roots: SessionTreeNode[] = [];
    for (const item of rows) {
      const node = nodes.get(item.id)!;
      // The store zeroes `depth` for orphans and cycles, so nesting only
      // rows with depth > 0 keeps the tree acyclic even on corrupt wire data.
      const parent = item.depth > 0 && item.parent_id ? nodes.get(item.parent_id) : undefined;
      if (parent) {
        parent.children.push(node);
      } else {
        roots.push(node);
      }
    }
    return roots;
  };
  return {
    active: forest(sort(threads.filter((item) => !item.archived))),
    archived: forest(threads.filter((item) => item.archived).sort(byRecent)),
  };
}
