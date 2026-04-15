/** @type {import('next').NextConfig} */
const nextConfig = {
  output: 'standalone',
  // Proxy API requests to the validator backend during development.
  // In production, Caddy/Nginx handles the reverse proxy.
  async rewrites() {
    const backend = process.env.VALIDATOR_API || 'http://localhost:3777';
    const metrics = process.env.METRICS_API || 'http://localhost:3779';
    return [
      { source: '/api/:path*', destination: `${backend}/api/:path*` },
      { source: '/ws', destination: `${backend}/ws` },
      { source: '/system/metrics', destination: `${metrics}/metrics` },
    ];
  },
};

module.exports = nextConfig;
