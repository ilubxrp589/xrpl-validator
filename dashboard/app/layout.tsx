import type { Metadata } from 'next';
import './globals.css';

export const metadata: Metadata = {
  title: 'Halcyon XRPL Validator',
  description: 'Real-time status dashboard for the Halcyon XRPL mainnet validator.',
};

export default function RootLayout({ children }: { children: React.ReactNode }) {
  return (
    <html lang="en" className="dark">
      <body className="min-h-screen bg-halcyon-bg text-white antialiased">
        {children}
      </body>
    </html>
  );
}
