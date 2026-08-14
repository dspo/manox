// Sidebar WebviewView: the manox workbench surface — model selector, streamed
// messages, and actor state, all rendered by inline HTML/JS (no framework).

import * as vscode from 'vscode';
import { ensureSession, onEvent, sendCommand } from './core';

export function registerManoxView(context: vscode.ExtensionContext): void {
  const provider: vscode.WebviewViewProvider = {
    resolveWebviewView(webviewView, _ctx, _token) {
      webviewView.webview.options = { enableScripts: true };
      webviewView.webview.html = getHtml();

      const cwd =
        vscode.workspace.workspaceFolders?.[0]?.uri.fsPath ??
        process.env.HOME ??
        process.cwd();
      ensureSession(cwd);

      // Actor events → webview.
      const off = onEvent((ev) => {
        void webviewView.webview.postMessage({ type: 'event', event: ev });
      });
      webviewView.onDidDispose(() => off());

      // Webview → actor.
      webviewView.webview.onDidReceiveMessage((msg: Record<string, unknown>) => {
        switch (msg.type) {
          case 'submit':
            sendCommand({ cmd: 'submit', text: String(msg.text ?? '') });
            break;
          case 'set_model':
            sendCommand({ cmd: 'set_model', id: String(msg.id) });
            break;
          case 'get_models':
            sendCommand({ cmd: 'list_models' });
            break;
          case 'get_current':
            sendCommand({ cmd: 'current_model' });
            break;
          case 'get_usage':
            sendCommand({ cmd: 'get_usage' });
            break;
        }
      });

      // Prime the model list once the session is up.
      setTimeout(() => {
        sendCommand({ cmd: 'list_models' });
        sendCommand({ cmd: 'current_model' });
      }, 200);
    },
  };

  context.subscriptions.push(
    vscode.window.registerWebviewViewProvider('manox.chatView', provider, {
      webviewOptions: { retainContextWhenHidden: true },
    }),
  );
}

function getHtml(): string {
  return /* html */ `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<style>
  :root { color-scheme: light dark; }
  * { box-sizing: border-box; }
  body { font-family: var(--vscode-font-family); font-size: 13px; margin: 0; display: flex; flex-direction: column; height: 100vh; }
  #model-bar { display: flex; align-items: center; gap: 6px; padding: 8px; border-bottom: 1px solid var(--vscode-panel-border); }
  #model-bar label { font-size: 11px; opacity: .8; white-space: nowrap; }
  #model-select { flex: 1; min-width: 0; }
  #messages { flex: 1; overflow-y: auto; padding: 8px; }
  .msg { margin-bottom: 10px; }
  .msg .role { font-size: 11px; font-weight: 600; opacity: .8; margin-bottom: 2px; }
  .msg .body { white-space: pre-wrap; word-break: break-word; }
  .msg.user .body { background: var(--vscode-editor-selectionBackground, rgba(0,0,0,.08)); padding: 6px 8px; border-radius: 4px; }
  .msg.thinking .body { color: var(--vscode-descriptionForeground); font-style: italic; }
  .msg.tool .body { font-family: var(--vscode-editor-font-family); font-size: 12px; opacity: .85; }
  .msg.error .body { color: var(--vscode-errorForeground); }
  pre { background: var(--vscode-textCodeBlock-background, rgba(0,0,0,.06)); padding: 8px; border-radius: 4px; overflow-x: auto; }
  code { font-family: var(--vscode-editor-font-family); }
  #input-row { display: flex; gap: 6px; padding: 8px; border-top: 1px solid var(--vscode-panel-border); }
  #prompt { flex: 1; }
</style>
</head>
<body>
  <div id="model-bar">
    <label>model</label>
    <select id="model-select"></select>
  </div>
  <div id="messages"></div>
  <div id="input-row">
    <input id="prompt" type="text" placeholder="Ask manox…" />
    <button id="send">Send</button>
  </div>
  <script>
    const vscode = acquireVsCodeApi();
    const messagesEl = document.getElementById('messages');
    const selectEl = document.getElementById('model-select');
    const promptEl = document.getElementById('prompt');

    function cmd(obj) { vscode.postMessage(obj); }
    function append(role, text) {
      const div = document.createElement('div');
      div.className = 'msg ' + role;
      const r = document.createElement('div');
      r.className = 'role';
      r.textContent = role;
      const b = document.createElement('div');
      b.className = 'body';
      b.innerHTML = render(role === 'user' ? text : text);
      div.append(r, b);
      messagesEl.appendChild(div);
      messagesEl.scrollTop = messagesEl.scrollHeight;
    }
    function stream(role, text) {
      let last = messagesEl.lastElementChild;
      if (!last || last.className !== 'msg ' + role) { append(role, text); return; }
      const b = last.querySelector('.body');
      b.innerHTML = render(b.textContent + text);
      messagesEl.scrollTop = messagesEl.scrollHeight;
    }
    function render(t) { return renderMarkdown(t); }

    window.addEventListener('message', (e) => {
      const data = e.data;
      if (data.type === 'models') {
        selectEl.innerHTML = '';
        for (const m of data.models) {
          const opt = document.createElement('option');
          opt.value = m.id; opt.textContent = m.name;
          selectEl.appendChild(opt);
        }
      } else if (data.type === 'current_model' && data.id) {
        selectEl.value = data.id;
      } else if (data.type === 'model_set') {
        if (data.error) append('error', data.error);
      } else if (data.type === 'event') {
        const ev = data.event;
        if (ev.type === 'agent_text') stream('assistant', ev.text);
        else if (ev.type === 'agent_thinking') stream('thinking', ev.text);
        else if (ev.type === 'tool_call') append('tool', '🔧 ' + ev.name);
        else if (ev.type === 'error') append('error', ev.message);
        else if (ev.type === 'usage') {
          const u = ev.usage;
          append('tool', 'tokens in ' + u.input + ' out ' + u.output);
        }
      }
    });

    document.getElementById('send').addEventListener('click', () => {
      const text = promptEl.value.trim();
      if (!text) return;
      append('user', text);
      promptEl.value = '';
      cmd({ type: 'submit', text });
    });
    promptEl.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') document.getElementById('send').click();
    });
    selectEl.addEventListener('change', () => cmd({ type: 'set_model', id: selectEl.value }));

    function renderMarkdown(t) {
      const esc = (s) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
      const lines = esc(t).split('\\n');
      const out = []; let inCode = false;
      for (const line of lines) {
        if (line.trimStart().startsWith('\`\`\`')) {
          out.push(inCode ? '</code></pre>' : '<pre><code>');
          inCode = !inCode;
        } else {
          out.push(inCode ? line : line.replace(/\`([^\`]+)\`/g, '<code>$1</code>'));
        }
      }
      if (inCode) out.push('</code></pre>');
      return out.join('\\n');
    }

    cmd({ type: 'get_models' });
    cmd({ type: 'get_current' });
  </script>
</body>
</html>`;
}
