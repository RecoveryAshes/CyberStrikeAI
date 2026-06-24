import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";
import path from "node:path";

const apiProxyTarget = process.env.VITE_API_BASE_URL || "http://192.168.64.2:51282";
const apiProxy = {
  "/api": {
    target: apiProxyTarget,
    changeOrigin: true,
    secure: false
  }
};

export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src")
    }
  },
  server: {
    port: 5177,
    strictPort: false,
    proxy: apiProxy
  },
  preview: {
    host: "0.0.0.0",
    port: 4177,
    strictPort: false,
    proxy: apiProxy
  }
});
