// Temporary verification script: drives installed Chrome against the dev
// server to confirm the disk setup page reports unreachable nodes instead of
// showing mock data.
import { chromium } from 'playwright-core';

const URL = process.env.VERIFY_URL || 'http://localhost:5199/';

const browser = await chromium.launch({ channel: 'chrome', headless: true });
const page = await browser.newPage({ viewport: { width: 1600, height: 1200 } });

const consoleLines = [];
page.on('console', msg => consoleLines.push(`[${msg.type()}] ${msg.text()}`));

await page.goto(URL, { waitUntil: 'load' });

// Sign in with default credentials
await page.getByLabel(/username/i).or(page.locator('input[type="text"], input[name="username"]')).first().fill('admin');
await page.locator('input[type="password"]').fill('spdk-admin-2025');
await page.getByRole('button', { name: /sign in/i }).click();

// Navigate to the Disk Setup tab
await page.getByText('Disk Setup', { exact: false }).first().click();

// Wait for per-node disk fetches (incl. the 503 for the failed node) to settle
await page.waitForTimeout(8000);

const body = await page.textContent('body');

const checks = {
  'banner shown': body.includes('Disk information unavailable'),
  'failed node named': body.includes('testa-aws-1781218303'),
  'backend error surfaced': /Failed to connect|Node agent unavailable/.test(body),
  'no mock Samsung disk': !body.includes('Samsung SSD 980 PRO'),
  'no mock WD disk': !body.includes('WD Black'),
  'no mock Micron disk': !body.includes('Micron 7450'),
  'real disks shown (URING bdev)': body.includes('URING bdev'),
};

for (const [name, ok] of Object.entries(checks)) {
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}`);
}

await page.screenshot({ path: '/tmp/disk-setup-verify.png', fullPage: true });
console.log('screenshot: /tmp/disk-setup-verify.png');

const interesting = consoleLines.filter(l =>
  /mock|Failed to refresh|503|unavailable/i.test(l)
);
console.log('--- relevant console lines ---');
interesting.slice(0, 15).forEach(l => console.log(l));

await browser.close();
process.exit(Object.values(checks).every(Boolean) ? 0 : 1);
