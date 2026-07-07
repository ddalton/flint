/** @type {import('tailwindcss').Config} */
import colors from 'tailwindcss/colors';

export default {
  content: [
    "./index.html",
    "./src/**/*.{js,ts,jsx,tsx}",
  ],
  theme: {
    extend: {
      // Semantic type ramp (design system): components say what a text role
      // IS (text-page-title), not which size it wants. Body copy stays on
      // the stock text-sm/text-xs scale; these cover the roles that had
      // drifted apart (stat tiles were 20/24/30px depending on the tab).
      fontSize: {
        // Tab/page titles ("Disk Setup for SPDK", header wordmark).
        'page-title': ['1.5rem', { lineHeight: '2rem', fontWeight: '700' }],
        // Section/panel/chart/modal headers — one size whether hand-rolled
        // or rendered through the Card primitive.
        'section': ['1.125rem', { lineHeight: '1.75rem', fontWeight: '600' }],
        // The big number on a stat/summary tile.
        'stat': ['1.5rem', { lineHeight: '2rem', fontWeight: '700' }],
      },
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
        // Non-sync amber: node warnings, caution banners, the zoom chip.
        // Distinct from `stale` (also amber) so a future re-hue of either
        // meaning never drags the other along.
        warning: colors.amber,
      },
    },
  },
  plugins: [],
}
