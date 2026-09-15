import { defineConfig } from "vite";

// Tauri 前端构建配置：固定端口供 tauri dev 连接
export default defineConfig({
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
  },
  build: {
    target: "es2021",
  },
});
