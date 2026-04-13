import type { Config } from 'tailwindcss';

const config: Config = {
  content: [
    './app/**/*.{ts,tsx}',
    './components/**/*.{ts,tsx}',
  ],
  darkMode: 'class',
  theme: {
    extend: {
      colors: {
        halcyon: {
          bg: '#0A0A0A',
          card: '#111111',
          border: '#1A1A1A',
          accent: '#00FF9F',
          'accent-dim': '#00CC7F',
          danger: '#FF3366',
          muted: '#666666',
          text: '#CCCCCC',
        },
      },
      fontFamily: {
        mono: ['IBM Plex Mono', 'Menlo', 'monospace'],
        sans: ['Inter', 'system-ui', 'sans-serif'],
        headline: ['Space Grotesk', 'sans-serif'],
      },
      animation: {
        'pulse-glow': 'pulse-glow 3s ease-in-out infinite',
        'fade-in': 'fade-in 0.3s ease-out',
      },
      keyframes: {
        'pulse-glow': {
          '0%, 100%': { opacity: '1', filter: 'drop-shadow(0 0 8px #00FF9F40)' },
          '50%': { opacity: '0.92', filter: 'drop-shadow(0 0 20px #00FF9F80)' },
        },
        'fade-in': {
          '0%': { opacity: '0', transform: 'translateY(4px)' },
          '100%': { opacity: '1', transform: 'translateY(0)' },
        },
      },
    },
  },
  plugins: [],
};

export default config;
