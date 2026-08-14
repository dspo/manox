// Thread list: the webview's home screen. Rows show title, relative
// last-activity time, and a status icon (spinning while running, warning
// triangle on error, blue dot when a turn finished while unfocused).

import { LoaderCircle, MessageSquare, Plus, TriangleAlert } from 'lucide-react';

import type { ThreadListItem } from '../../../protocol';
import { api, ThreadApi } from '../api/client';
import { store } from '../state/bridge';
import { Button } from './ui/button';

const rtf = new Intl.RelativeTimeFormat('en', { numeric: 'auto' });

function relativeTime(unixSeconds: number): string {
  const diff = unixSeconds - Date.now() / 1000;
  const abs = Math.abs(diff);
  if (abs < 60) return rtf.format(Math.round(diff), 'second');
  if (abs < 3_600) return rtf.format(Math.round(diff / 60), 'minute');
  if (abs < 86_400) return rtf.format(Math.round(diff / 3_600), 'hour');
  return rtf.format(Math.round(diff / 86_400), 'day');
}

const openThread = (item: ThreadListItem) => {
  // Threads with live local state switch instantly and only refocus the
  // actor; the rest go through the host's open handshake.
  if (store.get().perThread[item.id]) {
    store.openLocal(item.id);
    new ThreadApi(item.id).focus();
  } else {
    api.openThread(item.id);
  }
};

const StatusIcon = ({ item }: { item: ThreadListItem }) => {
  if (item.running) {
    return <LoaderCircle className="text-muted-foreground size-3.5 shrink-0 animate-spin" />;
  }
  if (item.errored) {
    return <TriangleAlert className="size-3.5 shrink-0 text-amber-500" />;
  }
  if (item.unread) {
    return <span className="bg-primary block size-2 shrink-0 rounded-full" />;
  }
  return <span className="block size-2 shrink-0" />;
};

export type ThreadsViewProps = {
  threads: ThreadListItem[];
};

export const ThreadsView = ({ threads }: ThreadsViewProps) => (
  <div className="font-chrome flex h-screen flex-col bg-background text-foreground">
    <div className="flex items-center gap-1 border-b px-3 py-1.5">
      <span className="min-w-0 flex-1 font-medium text-sm">Threads</span>
      <Button onClick={() => api.newSession()} size="icon-sm" title="New conversation" variant="ghost">
        <Plus className="size-4" />
      </Button>
    </div>
    {threads.length === 0 ? (
      <div className="text-muted-foreground flex flex-1 flex-col items-center justify-center gap-2 text-sm">
        <MessageSquare className="size-6" />
        <p>No conversations yet</p>
      </div>
    ) : (
      <ul className="flex-1 overflow-y-auto py-1">
        {threads.map((item) => (
          <li key={item.id}>
            <button
              className="hover:bg-muted flex w-full items-center gap-2 px-3 py-2 text-left"
              onClick={() => openThread(item)}
              type="button"
            >
              <StatusIcon item={item} />
              <span className="min-w-0 flex-1 truncate text-sm">{item.title}</span>
              <span className="text-muted-foreground shrink-0 text-xs">
                {relativeTime(item.updated_at)}
              </span>
            </button>
          </li>
        ))}
      </ul>
    )}
  </div>
);
