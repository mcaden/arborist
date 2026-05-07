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
      },
    },
  },
  plugins: [],
};
