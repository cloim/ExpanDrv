import tailwindcss from '@tailwindcss/vite';
import react from '@vitejs/plugin-react';
import { defineConfig } from 'vite';

// Tauri 창이 쓰는 고정 포트(devUrl)와 맞춥니다.
export default defineConfig({
  plugins: [react(), tailwindcss()],
  clearScreen: false,
  server: { port: 5173, strictPort: true },
  build: {
    target: 'es2022',
    // 엄격한 CSP에서 동작하도록 자산을 data: URL로 인라인하지 않습니다.
    assetsInlineLimit: 0,
    chunkSizeWarningLimit: 2500,
  },
});
