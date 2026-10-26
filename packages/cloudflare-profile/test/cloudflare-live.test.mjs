// This test creates a billable Cloudflare browser. It never uses the user's profiles.
// See LIVE_TESTS.md before enabling it. Do not turn this result into a production
// shared-write override: lock safety also needs independent cross-tool verification.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { chmodSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { Companion } from '../companion.mjs';

const enabled = process.env.AGENT_BROWSER_CLOUDFLARE_LIVE_TEST === '1';
const origin = 'https://example.com';

async function request(url, token, method) {
  return fetch(url, {
    method,
    headers: { Authorization: `Bearer ${token}` },
    redirect: 'error',
    signal: AbortSignal.timeout(30000),
  });
}

async function deleteAndConfirm(endpoint, token, sessionId) {
  const url = `${endpoint}/${encodeURIComponent(sessionId)}`;
  try {
    const deleted = await request(url, token, 'DELETE');
    if (deleted.status === 404) return true;
    if (!deleted.ok) return false;
    // A successful delete can report "closing". Confirm target-list 404.
    const deadline = Date.now() + 10000;
    while (Date.now() < deadline) {
      const status = await request(`${url}/json/list`, token, 'GET');
      if (status.status === 404) return true;
      if (!status.ok) return false;
      await delay(250);
    }
  } catch {
    // Never include network errors, response bodies, headers or URLs in logs.
  }
  return false;
}

test('isolated Cloudflare CDP context and profile round trip', { skip: !enabled, timeout: 240000 }, async () => {
  // These values are read only after an explicit opt-in.
  const account = process.env.CLOUDFLARE_ACCOUNT_ID;
  const token = process.env.CLOUDFLARE_API_TOKEN;
  if (!account || !/^[a-zA-Z0-9-]+$/.test(account) || !token || /[\r\n]/.test(token)) throw new Error('Set valid CLOUDFLARE_ACCOUNT_ID and CLOUDFLARE_API_TOKEN before the opt-in test.');
  const { chromium } = await import('playwright');
  const home = mkdtempSync(join(tmpdir(), 'agent-browser-cf-test-'));
  chmodSync(home, 0o700);
  const endpoint = `https://api.cloudflare.com/client/v4/accounts/${account}/browser-run/devtools/browser`;
  let sessionId, companion, restored, second;
  let stage = 'session creation';
  let testError;
  let closed = false;
  try {
    let response;
    try { response = await request(`${endpoint}?keep_alive=120000`, token, 'POST'); }
    catch { throw new Error('Session creation outcome is unknown; inspect the Cloudflare dashboard for a possibly open session.'); }
    if (!response.ok) throw new Error(`Session creation failed (HTTP ${response.status}).`);
    let payload;
    try { payload = await response.json(); }
    catch { throw new Error('Session creation response was unreadable; inspect the Cloudflare dashboard for a possibly open session.'); }
    // Retain the ID before checking the connection URL, so malformed responses
    // still trigger a compensating DELETE.
    if (typeof payload.sessionId === 'string' && /^[a-zA-Z0-9-]+$/.test(payload.sessionId)) sessionId = payload.sessionId;
    if (!sessionId) throw new Error('Session ID was missing or invalid; inspect the Cloudflare dashboard for a possibly open session.');
    if (typeof payload.webSocketDebuggerUrl !== 'string') throw new Error('Cloudflare returned no CDP URL.');
    const wsUrl = payload.webSocketDebuggerUrl;
    const args = { wsUrl, token, sessionId, home, profile: 'isolated-test', mode: 'write', compatibilityVerified: true };
    stage = 'profile context creation';
    companion = new Companion(chromium);
    const opened = await companion.open(args);
    const first = companion.context.pages()[0];
    stage = 'fixture setup';
    await first.goto(origin, { waitUntil: 'domcontentloaded', timeout: 30000 });
    if (new URL(first.url()).origin !== origin) throw new Error('Fixture origin redirected; cannot safely run storage assertions.');
    await first.evaluate(async () => {
      localStorage.setItem('agent-browser-test', 'local-value');
      document.cookie = 'agent_browser_test=fixture; path=/; Secure; SameSite=Lax';
      const db = await new Promise((resolve, reject) => {
        const open = indexedDB.open('agent-browser-test', 1);
        open.onupgradeneeded = () => open.result.createObjectStore('records');
        open.onsuccess = () => resolve(open.result);
        open.onerror = () => reject(open.error);
      });
      await new Promise((resolve, reject) => {
        const tx = db.transaction('records', 'readwrite');
        tx.objectStore('records').put({ nested: ['indexed-value', 42] }, 'fixture-key');
        tx.oncomplete = resolve;
        tx.onerror = () => reject(tx.error);
      });
      db.close();
    });
    stage = 'second CDP client and tab';
    second = await chromium.connectOverCDP(wsUrl, { headers: { Authorization: `Bearer ${token}` }, timeout: 30000 });
    const cdp = await second.newBrowserCDPSession();
    const nextPage = companion.context.waitForEvent('page', { timeout: 30000 });
    const target = await cdp.send('Target.createTarget', { url: origin, browserContextId: opened.contextId });
    assert.ok(target.targetId, 'second client did not create a target');
    const secondPage = await nextPage;
    await secondPage.waitForLoadState('domcontentloaded', { timeout: 30000 });
    assert.ok((await secondPage.evaluate(() => document.cookie)).includes('agent_browser_test=fixture'), 'second tab did not share the context cookie');
    stage = 'profile capture';
    await companion.save();
    await companion.closeContext();
    companion.release();
    companion = undefined;
    stage = 'profile restoration';
    restored = new Companion(chromium);
    await restored.open(args);
    const page = restored.context.pages()[0];
    await page.goto(origin, { waitUntil: 'domcontentloaded', timeout: 30000 });
    assert.equal(await page.evaluate(() => localStorage.getItem('agent-browser-test')), 'local-value');
    assert.ok((await page.evaluate(() => document.cookie)).includes('agent_browser_test=fixture'), 'cookie was not restored');
    const indexedValue = await page.evaluate(async () => {
      const db = await new Promise((resolve, reject) => {
        const open = indexedDB.open('agent-browser-test', 1);
        open.onsuccess = () => resolve(open.result);
        open.onerror = () => reject(open.error);
      });
      return await new Promise((resolve, reject) => {
        const get = db.transaction('records').objectStore('records').get('fixture-key');
        get.onsuccess = () => resolve(get.result?.nested?.[0]);
        get.onerror = () => reject(get.error);
      });
    });
    assert.equal(indexedValue, 'indexed-value');
  } catch (error) {
    // Error strings from browser libraries can include bearer values or browser
    // URLs. Restrict output to the stage and controlled messages above.
    const known = error instanceof Error && /^(Session creation|Session ID|Cloudflare returned no CDP URL|Fixture origin redirected)/.test(error.message);
    testError = new Error(known ? error.message : `Cloudflare compatibility test failed during ${stage}; details redacted.`);
  } finally {
    // Make the billable remote cleanup attempt first: a hung local Playwright
    // disconnect must not prevent DELETE from being sent.
    if (sessionId) {
      closed = await deleteAndConfirm(endpoint, token, sessionId);
      if (!closed) console.error(`Cloudflare test session ${sessionId}: remote deletion is unconfirmed. Check and close it in the Cloudflare dashboard.`);
    }
    try { if (companion) await companion.abort(); } catch {}
    try { if (restored) await restored.abort(); } catch {}
    try { if (second) await second.close(); } catch {}
    rmSync(home, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 });
  }
  if (testError) throw testError;
  if (!closed) throw new Error('Cloudflare test session deletion was not confirmed.');
});
