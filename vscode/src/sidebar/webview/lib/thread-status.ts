// Thread-row status machine, mirroring the gpui host's sidebar ship-wheel
// states: errored wins as the danger triangle, then the waiting-user states
// (blue static), then the self-advancing states (green spin), then the
// unread blue static, and finally the plain idle wheel. Waiting outranks
// running so a thread parked on an authorization keeps the blue static wheel
// instead of spinning.

import type { ThreadListItem } from '../../../protocol';

export type ThreadRowState = 'errored' | 'waiting' | 'autonomous' | 'unread' | 'idle';

export function threadRowState(item: ThreadListItem): ThreadRowState {
  if (item.errored) return 'errored';
  if (item.pending_auth || item.pending_plan) return 'waiting';
  if (item.running || item.background_work) return 'autonomous';
  if (item.unread) return 'unread';
  return 'idle';
}
