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

          <main className="flex-1 max-w-7xl mx-auto w-full px-4 sm:px-6 py-6 space-y-6">
            <HeroBanner />
            <NodeResources />

            {/* Metrics + Speedometer side by side */}
            <div className="grid grid-cols-1 md:grid-cols-[1fr_200px] gap-4">
              <MetricsGrid />
              <div className="flex flex-col gap-4">
                <Speedometer />
                <DisagreementHistory />
              </div>
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
