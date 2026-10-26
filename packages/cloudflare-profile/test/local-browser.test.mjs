// Opt-in: AGENT_BROWSER_TEST_CHROME=/absolute/path/to/chrome node --test test/local-browser.test.mjs
// This validates local multi-client and Playwright storage behavior only; it is
// not a Cloudflare compatibility proof and must not enable shared writes.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { createServer } from 'node:http';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { setTimeout as delay } from 'node:timers/promises';
import { Companion } from '../companion.mjs';

const chromePath = process.env.AGENT_BROWSER_TEST_CHROME;
test('local CDP clients share a Playwright context and restore IndexedDB', { skip: !chromePath, timeout: 90000 }, async () => {
  const { chromium } = await import('playwright');
  const root = mkdtempSync(join(tmpdir(), 'ab-cf-live-'));
  const server = createServer((_req, res) => { res.writeHead(200, { 'content-type': 'text/html' }); res.end('<!doctype html><title>profile test</title>'); });
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  const origin = `http://127.0.0.1:${server.address().port}`;
  const chrome = spawn(chromePath, ['--headless=new', '--no-first-run', '--no-default-browser-check', '--remote-debugging-port=0', `--user-data-dir=${join(root, 'chrome')}`, 'about:blank'], { stdio: 'ignore' });
  let second, companion, restored;
  try {
    let portFile;
    for (let i = 0; i < 300; i++) {
      try { portFile = readFileSync(join(root, 'chrome', 'DevToolsActivePort'), 'utf8'); break; }
      catch {
        if (chrome.exitCode !== null || chrome.signalCode !== null) throw new Error(`Chrome exited before CDP startup (code: ${chrome.exitCode}, signal: ${chrome.signalCode})`);
        await delay(100);
      }
    }
    assert.ok(portFile, 'Chrome did not start debugging');
    const [port, path] = portFile.trim().split('\n');
    const ws = `ws://127.0.0.1:${port}${path}`;
    const adapter = { async connectOverCDP() { return chromium.connectOverCDP(ws); } };
    const args = { wsUrl: 'wss://api.cloudflare.com/client/v4/accounts/a/browser-run/devtools/browser/s', token: 'test', sessionId: 's', home: root, mode: 'write', compatibilityVerified: true };
    companion = new Companion(adapter);
    const opened = await companion.open(args);
    const first = companion.context.pages()[0];
    await first.goto(origin);
    await first.evaluate(async () => {
      localStorage.setItem('local', 'value');
      document.cookie = 'profile_cookie=yes; path=/';
      const db = await new Promise((resolve, reject) => { const request = indexedDB.open('profile-db', 1); request.onupgradeneeded = () => request.result.createObjectStore('records'); request.onsuccess = () => resolve(request.result); request.onerror = () => reject(request.error); });
      await new Promise((resolve, reject) => { const tx = db.transaction('records', 'readwrite'); tx.objectStore('records').put({ nested: ['a', 2] }, 'key'); tx.oncomplete = resolve; tx.onerror = () => reject(tx.error); });
      db.close();
    });
    second = await chromium.connectOverCDP(ws);
    const cdp = await second.newBrowserCDPSession();
    const nextPage = companion.context.waitForEvent('page');
    const created = await cdp.send('Target.createTarget', { url: origin, browserContextId: opened.contextId });
    const page2 = await nextPage;
    assert.ok(created.targetId);
    await page2.waitForLoadState();
    assert.equal(await page2.evaluate(() => document.cookie), 'profile_cookie=yes');
    await companion.save();
    const state = JSON.parse(readFileSync(join(root, 'profiles', 'main', 'state.json'), 'utf8'));
    assert.ok(state.origins.some(entry => entry.indexedDB?.length), 'IndexedDB must be captured');
    await companion.closeContext(); companion.release(); companion = undefined;
    restored = new Companion(adapter);
    await restored.open(args);
    const restoredPage = restored.context.pages()[0];
    await restoredPage.goto(origin);
    assert.equal(await restoredPage.evaluate(() => localStorage.getItem('local')), 'value');
    assert.equal(await restoredPage.evaluate(async () => { const db = await new Promise(resolve => { const request = indexedDB.open('profile-db', 1); request.onsuccess = () => resolve(request.result); }); return await new Promise(resolve => { const request = db.transaction('records').objectStore('records').get('key'); request.onsuccess = () => resolve(request.result.nested[0]); }); }), 'a');
    await restored.closeContext(); restored.release(); restored = undefined;
  } finally {
    if (companion) await companion.abort();
    if (restored) await restored.abort();
    if (second) await second.close();
    if (chrome.exitCode === null && chrome.signalCode === null) {
      chrome.kill('SIGTERM');
      await Promise.race([once(chrome, 'exit'), delay(3000)]);
    }
    await new Promise(resolve => server.close(resolve));
    rmSync(root, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 });
  }
});
