/** Shapes returned by the validator's /api/* endpoints. */

export interface FfiVerifier {
  libxrpl_version: string;
  ledgers_applied: number;
  live_apply_attempted: number;
  live_apply_ok: number;
  live_apply_claimed: number;
  live_apply_diverged: number;
  live_apply_last_ter: string;
  live_apply_last_mutations: number;
  live_apply_last_ms: number;
  live_apply_ter_counts: Record<string, number>;
  live_diverged_by_type: Record<string, number>;
  diverged_tx_samples: string[];
  apply_by_type: Record<string, number>;
  apply_duration_buckets_ms: number[];
  apply_duration_count: number;
  apply_duration_sum_ms: number;
  round_tx_types: Record<string, number>;
  round_tx_count: number;
  round_ledger_seq: number;
  round_fees_drops: number;
  total_fees_burned_drops: number;
  shadow_hash_attempted: number;
  shadow_hash_matched: number;
  shadow_hash_mismatched: number;
  shadow_hash_last: string;
  shadow_hash_last_network: string;
  shadow_hash_last_matched: boolean;
  db_hits: number;
  db_rpc_fallbacks: number;
  health_checks: number;
  health_passed: number;
  /** True when XRPL_FFI_STAGE3=1 was read at start_ws_sync entry. */
  stage3_enabled?: boolean;
  /** Rolling buffer of the last 50 txs applied by the FFI engine.
   *  Format: "L{seq} {tx_type}/{ter_name} {short_hash} {ms}ms mut={N}" */
  recent_tx_samples?: string[];
}

export interface LiveEngine {
  active: boolean;
  entries: number;
  total_coins: number;
  total_applied: number;
  total_failed: number;
  verified_match: number;
  verified_mismatch: number;
  verified_total: number;
  verify_pct: number;
}

export interface EngineData {
  ledger_seq: number;
  ledgers_processed: number;
  lifetime_burned: number;
  network_total_coins: number;
  round_fees: number;
  round_proposals: number;
  round_supported: number;
  round_tx_count: number;
  round_tx_types: Record<string, number>;
  round_unsupported: number;
  round_validations: number;
  sig_fail: number;
  sig_ok: number;
  start_time_ms: number;
  total_fees_burned: number;
  total_messages: number;
  total_proposals: number;
  total_txs: number;
  total_validations: number;
  ffi_verifier: FfiVerifier;
  live_engine: LiveEngine;
}

export interface ConsensusMonitor {
  total_proposals: number;
  agreement: {
    count: number;
    hash: string;
    pct: number;
  };
}

export interface ConsensusData {
  phase: string;
  unl_size: number;
  monitor: ConsensusMonitor;
  establish_rounds?: number;
  candidate_set_size?: number;
  mempool_size?: number;
  ledger_seq?: number;
  peer_proposals?: number;
}

export interface StateHashStatus {
  computed_hash: string;
  network_hash: string;
  consecutive_matches: number;
  total_matches: number;
  total_mismatches: number;
  compute_time_secs: number;
  computing: boolean;
  ready_to_sign: boolean;
  db_entries: number;
  wallet_count: number;
  /** Raw shape from the API is `[seq, count][]`, not objects. */
  wallet_history: Array<[number, number]>;
  bulk_sync: {
    running: boolean;
    objects_synced: number;
    rate: number;
    verified: boolean;
  };
  sync_log: Array<{
    seq: number;
    matched: boolean;
    txs: number;
    objs: number;
    time_secs: number;
  }>;
}

/** Combined snapshot of all live data — this is what components consume. */
export interface ValidatorSnapshot {
  engine: EngineData;
  consensus: ConsensusData;
  stateHash: StateHashStatus;
  /** Number of connected peers from /api/peers. */
  peers: number;
  /** Milliseconds since epoch when this snapshot was captured. */
  updatedAt: number;
}

/** Percentile extractor for the cumulative histogram buckets. */
export function histPercentile(
  buckets: number[],
  count: number,
  pct: number,
): string {
  const bounds = [1, 2, 5, 10, 25, 50, 100, 250, 500, 1000];
  if (count === 0) return '—';
  const target = Math.ceil(count * pct);
  for (let i = 0; i < bounds.length && i < buckets.length; i++) {
    if (buckets[i] >= target) return `${bounds[i]}ms`;
  }
  return '>1s';
}
