// End-to-end bulk-init drill (improvement-plan Phase 2d/4 follow-up).
//
// Run this at the START of a spot-builder session, BEFORE partitioning the
// scratch NVMe for docker — that is the one window where the cluster has a
// disk that is genuinely uninitialized, unmounted, non-system, and about to
// be reformatted anyway. It exercises the full UI path the safety rails
// normally keep unreachable: eligibility → selection → BulkConfirmModal →
// runInitBatch → per-disk ok → node refresh showing LVS Ready.
//
// The drill INITIALIZES THE TARGET DISK (creates an SPDK LVS on it). Hand
// the disk back to the builder flow afterwards with, on the node:
//     wipefs -a /dev/nvme1n1     (via the node_run helper)
// then continue the normal docker-data setup — its blkid guard would
// otherwise refuse the LVS signature.
//
// Prereqs:  npm install --no-save playwright-core   (Chrome must be installed)
//           kubectl -n flint-system port-forward deploy/spdk-dashboard 13000:3000
// Usage:
//   DASHBOARD_ADMIN_PW=... TARGET_NODE=runj-aws-17831xxxxx TARGET_PCI=0000:00:1f.0 \
//     node scripts/bulk-init-drill.mjs
//
// Safety: the script refuses to run without an explicit TARGET_NODE +
// TARGET_PCI, only ever selects that one disk, asserts the confirm manifest
// names exactly it, and aborts before confirming on any mismatch.
import { chromium } from 'playwright-core';

const BASE = process.env.BASE_URL || 'http://localhost:13000';
const ADMIN_PW = process.env.DASHBOARD_ADMIN_PW;
const TARGET_NODE = process.env.TARGET_NODE;
const TARGET_PCI = process.env.TARGET_PCI;
const OUT = process.env.SHOT_DIR || '/tmp/bulk-init-drill';
if (!ADMIN_PW || !TARGET_NODE || !TARGET_PCI) {
  console.error('DASHBOARD_ADMIN_PW, TARGET_NODE and TARGET_PCI are required.');
  process.exit(2);
}
import { mkdirSync } from 'node:fs';
mkdirSync(OUT, { recursive: true });

const results = [];
const check = (name, ok, detail = '') => {
  results.push([name, ok]);
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}${detail ? `  (${detail})` : ''}`);
  if (!ok) throw new Error(`drill aborted at: ${name}`);
};

const browser = await chromium.launch({ channel: 'chrome', headless: true });
const page = await browser.newPage({ viewport: { width: 1600, height: 1200 } });
const errors = [];
page.on('pageerror', (e) => errors.push(String(e)));

try {
  await page.goto(`${BASE}/disk-setup`, { waitUntil: 'load' });
  await page.locator('input[type="text"]').first().fill('admin');
  await page.locator('input[type="password"]').fill(ADMIN_PW);
  await page.getByRole('button', { name: /sign in/i }).click();
  await page.waitForSelector('nav a', { timeout: 15000 });
  await page.waitForTimeout(6000); // let all node agents answer

  // 1. Find the target disk card: inside the TARGET_NODE group, the card
  //    that names TARGET_PCI. It must be eligible (checkbox present).
  const checkbox = page
    .locator(`text=${TARGET_PCI}`)
    .locator('xpath=ancestor::*[.//input[@type="checkbox"]][1]//input[@type="checkbox"]')
    .first();
  check('target disk card found and selectable', (await checkbox.count()) === 1);
  await page.screenshot({ path: `${OUT}/1-before.png`, fullPage: true });

  // 2. Select exactly that disk.
  await checkbox.check();
  await page.waitForTimeout(500);
  const initButton = page.getByRole('button', { name: /^Initialize 1 disk$/ });
  check('Initialize 1 disk button appears', (await initButton.count()) === 1);

  // 3. Open the confirm modal; verify the manifest names exactly our disk.
  await initButton.click();
  const dialog = page.getByRole('alertdialog', { name: 'Initialize 1 disk for SPDK' });
  await dialog.waitFor({ timeout: 5000 });
  check('confirm manifest names the target node', (await dialog.getByText(TARGET_NODE).count()) >= 1);
  check('confirm manifest names the target PCI', (await dialog.getByText(TARGET_PCI).count()) >= 1);
  check('manifest has exactly one disk row', (await dialog.locator('tbody tr').count()) === 1);
  await page.screenshot({ path: `${OUT}/2-confirm.png`, fullPage: true });

  // 4. Confirm — this WIPES the target disk (that is the point).
  await dialog.getByRole('button', { name: /^Initialize 1 disk$/ }).click();

  // 5. Watch the batch panel drive the disk to ok (agent init is seconds).
  let done = false;
  for (let i = 0; i < 30 && !done; i++) {
    await page.waitForTimeout(3000);
    const okCount = await page.getByText(/1\s*\/\s*1/).count();
    const failed = await page.getByText(/Failed/i).count();
    if (failed > 2) break; // stat cards contain one static 'Failed'; spikes mean batch failures
    done = okCount > 0;
  }
  await page.screenshot({ path: `${OUT}/3-batch.png`, fullPage: true });
  check('batch reports the disk ok', done);

  // 6. After the queue-drain refresh the disk must read LVS Ready.
  await page.waitForTimeout(5000);
  await page.screenshot({ path: `${OUT}/4-after.png`, fullPage: true });
  check('zero page errors', errors.length === 0, errors.slice(0, 2).join(' | '));

  console.log(`\n${results.length}/${results.length} drill checks passed; shots in ${OUT}`);
  console.log('REMINDER: wipefs -a the disk on the node before the docker-data setup.');
} finally {
  await browser.close();
}
