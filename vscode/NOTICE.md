# Third-party notice

The webview vendors UI components from two MIT-licensed projects. Vendored
copies live under `src/sidebar/webview/components/` and are adapted to the
local wire protocol (`src/protocol.ts`); no upstream runtime package is
installed.

## Vercel AI Elements

- Source: https://github.com/vercel/ai-elements (`packages/elements/src`)
- License: MIT
- Vendored at commit `0c1f5e8c75273f0e95c8faa031544a8aa2bb1a5b`
- Adapted files: `components/ai/` (conversation, message, reasoning, tool,
  confirmation, model selector patterns)

## shadcn/ui

- Source: https://github.com/shadcn-ui/ui
- License: MIT
- Adapted files: `components/ui/` (button, badge, card, textarea, alert,
  collapsible, dropdown-menu primitives)

Adaptations remove the upstream `ai` SDK runtime dependency, replace
streamdown/shiki rendering with react-markdown, and re-theme every component
onto VS Code `--vscode-*` color tokens.
