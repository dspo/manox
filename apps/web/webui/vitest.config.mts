import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    environment: 'node',
    // `.test.ts` files are pure-logic/node suites (store, journal, i18n,
    // manifest scan). `.test.tsx` files render React (slot outlets, the
    // standard hooks, the conversation-info plugin) and opt into jsdom with
    // the `// @vitest-environment jsdom` docblock pragma on each such file.
    include: ['src/**/*.test.{ts,tsx}'],
  },
});
