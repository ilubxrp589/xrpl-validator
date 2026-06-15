#!/usr/bin/env node
// Watch for the STABLE xrpld/rippled 3.2.0 release and Telegram-ping ONCE when it lands.
// Read-only. Reuses the copilot's Telegram creds (../copilot/config.local.json).
// Cron (every 30 min):
//   */30 * * * * /ABS/PATH/node /home/localai/Desktop/xrpl-validator/scripts/watch-320-release.mjs >> .../scripts/.320-watch.log 2>&1
// Stop notifying: it self-disables after the first hit via a marker file.

import { readFileSync, existsSync, writeFileSync } from 'node:fs';

const MARKER = new URL('./.320-notified', import.meta.url);

// stable 3.2.0+ means a plain x.y.z tag (no -b/-rc suffix) with maj>3 or (maj=3 and min>=2)
function isStable320Plus(tag) {
  const m = /^v?(\d+)\.(\d+)\.(\d+)$/.exec(String(tag).trim());
  if (!m) return false;
  const maj = +m[1], min = +m[2];
  return maj > 3 || (maj === 3 && min >= 2);
}

function telegram() {
  try { return JSON.parse(readFileSync(new URL('../copilot/config.local.json', import.meta.url), 'utf8')).alerts?.telegram || null; }
  catch { return null; }
}
async function send(text) {
  const t = telegram();
  if (!t?.token || !t?.chatId) { console.log('no telegram creds — skipping send'); return; }
  try {
    await fetch(`https://api.telegram.org/bot${t.token}/sendMessage`, {
      method: 'POST', headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ chat_id: t.chatId, text, disable_web_page_preview: true }),
    });
  } catch (e) { console.log('telegram send failed:', e?.message || e); }
}

(async () => {
  if (existsSync(MARKER)) return;                       // already notified — done
  let found = null;
  try {
    const r = await fetch('https://api.github.com/repos/XRPLF/rippled/releases?per_page=12', { headers: { 'User-Agent': 'xrpld-320-watch' } });
    const rel = await r.json();
    if (Array.isArray(rel)) {
      const hit = rel.find((x) => !x.prerelease && !x.draft && isStable320Plus(x.tag_name));
      if (hit) found = `GitHub release ${hit.tag_name} (${hit.published_at})`;
    }
  } catch { /* network — try apt below */ }
  if (!found) {
    try {
      const html = await (await fetch('https://repos.ripple.com/repos/api/rippled-deb/pool/stable/r/rippled/')).text();
      const m = html.match(/rippled_3\.2\.[0-9]+[^"]*\.deb/);
      if (m) found = `apt stable deb ${m[0]}`;
    } catch { /* leave found null */ }
  }
  if (found) {
    await send(`🚀 xrpld 3.2.0 STABLE is out — ${found}.\nExecute the port (see PORT-3.2.0.md): build on m3060 → hash-match a multi-hundred-ledger window → roll, keep .39 alive. The 3.1.3 validator stays safe until the new amendments activate (~2 weeks after majority).`);
    try { writeFileSync(MARKER, `${new Date().toISOString()} ${found}\n`); } catch { /* ignore */ }
    console.log('NOTIFIED:', found);
  } else {
    console.log(`not yet (${new Date().toISOString()})`);
  }
})();
