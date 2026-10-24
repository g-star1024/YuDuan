import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri 前端构建配置（对齐官方模板口径）
export default defineConfig({
  plugins: [react()],
  // Rust core 调试信息与终端输出不被清除
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
  envPrefix: ["VITE_", "TAURI_"],
  build: {
    target: "es2021",
    outDir: "dist",
    minify: "esbuild",
    sourcemap: false,
  },
});
