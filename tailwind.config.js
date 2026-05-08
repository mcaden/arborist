import defaultTheme from 'tailwindcss/defaultTheme';

/** @type {import('tailwindcss').Config} */
export default {
  content: ['./index.html', './src/**/*.{ts,tsx}'],
  darkMode: 'class',
  theme: {
    extend: {
      fontFamily: {
        sans: ["'CaskaydiaCove NF'", ...defaultTheme.fontFamily.sans],
        mono: ["'CaskaydiaCove NF Mono'", ...defaultTheme.fontFamily.mono],
        // Dedicated stack for Nerd Font icon glyphs (e.g. `StatusIcon`).
        // Pinned to CaskaydiaCove NF only — no generic fallback — so the
        // icon column either renders the codicon or shows tofu rather
        // than silently falling back to a system font that lacks the
        // PUA codepoints. Decoupled from `sans` so reorganising the
        // body-font stack can't accidentally break icon rendering.
        icon: ["'CaskaydiaCove NF'"],
      },
    },
  },
  plugins: [],
};
