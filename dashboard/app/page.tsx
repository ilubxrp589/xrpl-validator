'use client';

import { ValidatorProvider } from '@/hooks/use-validator-data';
import { ChaosProvider } from '@/hooks/use-chaos-mode';
import { Navbar } from '@/components/navbar';
import { HeroBanner } from '@/components/hero-banner';
import { NodeResources } from '@/components/node-resources';
import { MetricsGrid } from '@/components/metrics-grid';
import { Speedometer } from '@/components/speedometer';
import { DisagreementHistory } from '@/components/disagreement-history';
import { TransactionFlow } from '@/components/transaction-flow';
import { ConsensusGrid } from '@/components/consensus-grid';
import { IdentityCard } from '@/components/identity-card';
import { TrendCharts } from '@/components/trend-charts';
import { Footer } from '@/components/footer';

export default function Home() {
  return (
    <ValidatorProvider>
      <ChaosProvider>
        <div className="flex flex-col min-h-screen">
          <Navbar />

          <main className="flex-1 max-w-7xl mx-auto w-full px-4 sm:px-6 py-6 space-y-4">
            <HeroBanner />
            <NodeResources />
            <MetricsGrid />

            {/* Speedometer + Disagreements side by side, compact */}
            <div className="grid grid-cols-1 sm:grid-cols-2 gap-4">
              <Speedometer />
              <DisagreementHistory />
            </div>

            <TransactionFlow />
            <ConsensusGrid />
            <IdentityCard />
            <TrendCharts />
          </main>

          <Footer />
        </div>
      </ChaosProvider>
    </ValidatorProvider>
  );
}
