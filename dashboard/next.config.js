/** @type {import('next').NextConfig} */
const nextConfig = {
  output: 'standalone',
  // Proxy API requests to the validator backend during development.
  // In production, Caddy/Nginx handles the reverse proxy.
  async rewrites() {
    // Defaults point at the REAL backends so the dashboard works with no env vars
    // set. (A missing VALIDATOR_API previously sent /api/* to a dead localhost:3777.)
    const backend = process.env.VALIDATOR_API || 'http://10.0.0.97:3777';
    const metrics = process.env.METRICS_API || 'http://10.0.0.97:3779';
    const copilot = process.env.COPILOT_API || 'http://localhost:3780';
    return [
      { source: '/api/:path*', destination: `${backend}/api/:path*` },
      { source: '/ws', destination: `${backend}/ws` },
      { source: '/system/metrics', destination: `${metrics}/metrics` },
      { source: '/copilot', destination: `${copilot}/copilot` },
      { source: '/copilot/health', destination: `${copilot}/health` },
      // NOTE: /copilot/investigate is a Route Handler (app/copilot/investigate/route.ts),
      // NOT a rewrite — investigate runs ~1-2 min and the rewrite proxy resets the socket.
    ];
  },
};

module.exports = nextConfig;
