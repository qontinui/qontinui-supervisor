/// <reference types="vitest" />
import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import path from 'path';

export default defineConfig({
  plugins: [react()],
  test: {
    globals: true,
    environment: 'jsdom',
    setupFiles: ['./src/test-setup.ts'],
  },
  resolve: {
    alias: {
      '@': path.resolve(__dirname, './src'),
    },
  },
  build: {
    outDir: '../dist',
    emptyOutDir: true,
    rollupOptions: {
      output: {
        manualChunks: {
          recharts: ['recharts'],
          react: ['react', 'react-dom', 'react-router-dom'],
        },
      },
    },
  },
  server: {
    port: 5174,
    proxy: {
      '/health': 'http://localhost:9875',
      '/runner': 'http://localhost:9875',
      '/logs': 'http://localhost:9875',
      '/ai': 'http://localhost:9875',
      '/dev-start': 'http://localhost:9875',
      '/velocity': 'http://localhost:9875',
      '/workflow-loop': 'http://localhost:9875',
      '/diagnostics': 'http://localhost:9875',
      '/ui-bridge': 'http://localhost:9875',
      '/runner-api': 'http://localhost:9875',
      '/expo': 'http://localhost:9875',
      '/eval': 'http://localhost:9875',
      '/velocity-tests': 'http://localhost:9875',
      '/velocity-improvement': 'http://localhost:9875',
      '/ws': { target: 'ws://localhost:9875', ws: true },
    },
  },
});
