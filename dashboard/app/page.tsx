'use client';

import { ValidatorProvider } from '@/hooks/use-validator-data';
import { Navbar } from '@/components/navbar';
import { HeroBanner } from '@/components/hero-banner';
import { MetricsGrid } from '@/components/metrics-grid';
import { TransactionFlow } from '@/components/transaction-flow';
import { ConsensusGrid } from '@/components/consensus-grid';
import { IdentityCard } from '@/components/identity-card';
import { TrendCharts } from '@/components/trend-charts';
import { Footer } from '@/components/footer';

export default function Home() {
  return (
    <ValidatorProvider>
      <div className="flex flex-col min-h-screen">
        <Navbar />

        <main className="flex-1 max-w-7xl mx-auto w-full px-4 sm:px-6 py-6 space-y-6">
          <HeroBanner />
          <MetricsGrid />
          <TransactionFlow />
          <ConsensusGrid />
          <IdentityCard />
          <TrendCharts />
        </main>

        <Footer />
      </div>
    </ValidatorProvider>
  );
}
