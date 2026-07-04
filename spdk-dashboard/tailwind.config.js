/** @type {import('tailwindcss').Config} */
import colors from 'tailwindcss/colors';

export default {
  content: [
    "./index.html",
    "./src/**/*.{js,ts,jsx,tsx}",
  ],
  theme: {
    extend: {
      // Semantic status palette (design system): components say what a color
      // MEANS (bg-healthy-100), not which hue it is. Aliased onto the stock
      // palette so the Phase 0-3 visuals are unchanged; status.ts is the one
      // place that maps engine states onto these.
      colors: {
        healthy: colors.green,
        degraded: colors.yellow,
        failed: colors.red,
        rebuilding: colors.orange,
        stale: colors.amber,
        standby: colors.blue,
        insync: colors.green,
        rejoining: colors.purple,
        brand: colors.blue,
      },
    },
  },
  plugins: [],
}
