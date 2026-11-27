import adapter from '@sveltejs/adapter-static';
import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

export default defineConfig({
  // Static output: Tauri serves the built pages; there is no server.
  plugins: [sveltekit({ adapter: adapter() })],
  clearScreen: false,
  server: { port: 5173, strictPort: true },
  build: { target: 'es2022' }
});
