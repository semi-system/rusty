import { defineConfig } from 'vitest/config';
import solid from 'vite-plugin-solid';

// The build is the shipped artifact: no source maps, content-hashed file names, nothing inlined (the page's CSP forbids inline script and style).
export default defineConfig({
  plugins: [solid()],
  build: {
    target: 'es2022',
    sourcemap: false,
    assetsInlineLimit: 0,
    cssCodeSplit: false,
    modulePreload: { polyfill: false },
  },
  server: { proxy: { '/api': 'http://127.0.0.1:8090' } },
  resolve: { conditions: ['development', 'browser'] },
  test: {
    environment: 'jsdom',
    include: ['src/**/*.test.ts', 'src/**/*.test.tsx'],
    testTimeout: 30000,
    globals: false,
    server: { deps: { inline: [/solid-js/, /@solidjs\/testing-library/] } },
  },
});
