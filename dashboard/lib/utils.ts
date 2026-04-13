import { clsx, type ClassValue } from 'clsx';
import { twMerge } from 'tailwind-merge';

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs));
}

/** Format a number with locale-aware thousands separators. */
export function fmt(n: number): string {
  return n.toLocaleString();
}

/** Truncate a hex hash for display: first 8 + "..." */
export function truncHash(hash: string, len = 16): string {
  if (!hash || hash.length <= len) return hash || '—';
  return hash.slice(0, len) + '...';
}
