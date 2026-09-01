// Enter-key decision for the composer. Chinese IMEs commit a candidate with
// Enter; that keydown arrives with `isComposing` true (WebKit also reports
// keyCode 229), and WKWebView fires one trailing Enter after `compositionend`.
// The composer defers those keys to the IME's default commit instead of
// sending the message.

export interface EnterKeyInfo {
  key: string;
  shiftKey: boolean;
  isComposing: boolean;
  keyCode: number;
}

/** `submit` when the key is a real send-Enter; `defer` for shift+Enter
 * (newline), any composition key, and the trailing key after a composition
 * ended. */
export function enterAction(e: EnterKeyInfo, suppressNextEnter: boolean): 'submit' | 'defer' {
  if (e.key !== 'Enter' || e.shiftKey) return 'defer';
  if (e.isComposing || e.keyCode === 229) return 'defer';
  if (suppressNextEnter) return 'defer';
  return 'submit';
}
