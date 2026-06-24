import type { Config } from "tailwindcss";

export default {
  darkMode: ["class"],
  content: ["./index.html", "./src/**/*.{ts,tsx}"],
  theme: {
    extend: {
      fontFamily: {
        sans: [
          "-apple-system",
          "BlinkMacSystemFont",
          "SF Pro Display",
          "SF Pro Text",
          "Inter",
          "system-ui",
          "sans-serif"
        ]
      },
      colors: {
        glass: {
          base: "rgba(16, 17, 21, 0.72)",
          soft: "rgba(255, 255, 255, 0.08)",
          line: "rgba(255, 255, 255, 0.14)"
        }
      },
      boxShadow: {
        glass: "0 24px 80px rgba(0, 0, 0, 0.35), inset 0 1px 0 rgba(255, 255, 255, 0.12)",
        glow: "0 0 0 1px rgba(255,255,255,0.08), 0 18px 60px rgba(0,0,0,0.38)"
      }
    }
  },
  plugins: []
} satisfies Config;
