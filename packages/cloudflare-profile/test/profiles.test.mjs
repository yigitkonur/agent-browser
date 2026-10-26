import { test } from 'node:test';
import assert from 'node:assert/strict';
import { copyFileSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { ProfileStore, cloneProfile } from '../profiles.mjs';
import { Companion } from '../companion.mjs';

const fixture = () => ({ cookies: [{ name: 'sid', value: 'secret', domain: 'example.test', path: '/', expires: -1, httpOnly: true, secure: true, sameSite: 'Lax' }], origins: [{ origin: 'https://example.test', localStorage: [{ name: 'key', value: 'value' }], indexedDB: [{ name: 'data', version: 1, stores: [{ name: 'items', records: [{ key: 'a', value: { nested: [1, 2, 3] } }] }] }] }] });
function withHome(fn) { const home = mkdtempSync(join(tmpdir(), 'ab-cf-profile-')); return Promise.resolve().then(() => fn(home)).finally(() => rmSync(home, { recursive: true, force: true })); }
const path = home => join(home, 'profiles', 'main', 'state.json');

test('default opens the existing main profile without saving changes', () => withHome(async home => {
  const store = new ProfileStore(home, 'main', 'seed');
  store.acquire(); store.save(fixture()); store.release();
  const before = readFileSync(path(home), 'utf8');
  let restoredState;
  const context = {
    async newPage() { return {}; },
    async newCDPSession() { return { async send() { return { targetInfo: { targetId: 't', browserContextId: 'c' } }; }, async detach() {} }; },
    async storageState() { return fixture(); },
    async close() {}
  };
  const chromium = { async connectOverCDP() { return { async newContext(options) { restoredState = options.storageState; return context; }, async close() {} }; } };
  const companion = new Companion(chromium);
  const opened = await companion.open({ wsUrl: 'wss://api.cloudflare.com/client/v4/accounts/a/browser-run/devtools/browser/s', token: 'token', sessionId: 's', home });
  assert.equal(opened.profile, 'main');
  assert.equal(opened.mode, 'read');
  assert.deepEqual(restoredState, fixture());
  await assert.rejects(companion.save(), error => error.code === 'PROFILE_SAVE_FAILED');
  await companion.closeContext(); companion.release();
  assert.equal(readFileSync(path(home), 'utf8'), before);
}));

test('default refuses to open a missing main profile instead of starting empty', () => withHome(async home => {
  let connected = false;
  const companion = new Companion({ async connectOverCDP() { connected = true; throw new Error('unexpected connection'); } });
  await assert.rejects(companion.open({ wsUrl: 'wss://api.cloudflare.com/client/v4/accounts/a/browser-run/devtools/browser/s', token: 'token', sessionId: 's', home }), error => error.code === 'PROFILE_NOT_FOUND');
  assert.equal(connected, false);
}));

test('plugin-compatible profile lock, metadata, backup and clone', () => withHome(home => {
  const first = new ProfileStore(home, 'main', 'session-1');
  first.acquire();
  first.save(fixture());
  const other = new ProfileStore(home, 'main', 'session-2');
  assert.throws(() => other.acquire(), error => error.code === 'PROFILE_IN_USE');
  cloneProfile(home, 'main', 'copy');
  assert.deepEqual(JSON.parse(readFileSync(join(home, 'profiles', 'copy', 'state.json'), 'utf8')), fixture());
  first.release();
  const second = new ProfileStore(home, 'main', 'session-2');
  second.acquire();
  const changed = fixture(); changed.cookies[0].value = 'new';
  second.save(changed);
  assert.deepEqual(JSON.parse(readFileSync(join(home, 'profiles', 'main', 'state.prev.json'), 'utf8')), fixture());
  second.release();
}));

test('invalid state and lost lock never overwrite a profile', () => withHome(home => {
  const store = new ProfileStore(home, 'main', 'one'); store.acquire(); store.save(fixture());
  assert.throws(() => store.save({ cookies: [] }), error => error.code === 'PROFILE_SAVE_FAILED');
  writeFileSync(join(store.dir, 'lock.json'), JSON.stringify({ nonce: 'other', sessionId: 'other' }));
  assert.throws(() => store.save({ cookies: [], origins: [] }), error => error.code === 'PROFILE_LOCK_LOST');
  assert.deepEqual(JSON.parse(readFileSync(path(home), 'utf8')), fixture());
  store.release();
}));

test('companion fails closed until compatibility is verified and on IndexedDB capture error', () => withHome(async home => {
  const store = new ProfileStore(home, 'main', 'seed'); store.acquire(); store.save(fixture()); store.release();
  let captureFails = false;
  const context = {
    async newPage() { return {}; },
    async newCDPSession() { return { async send() { return { targetInfo: { targetId: 'target', browserContextId: 'context' } }; }, async detach() {} }; },
    async storageState(options) { assert.deepEqual(options, { indexedDB: true }); if (captureFails) throw new Error('capture failed'); return fixture(); },
    async close() {}
  };
  const chromium = { async connectOverCDP() { return { async newContext() { return context; }, async close() {} }; } };
  const companion = new Companion(chromium);
  const args = { wsUrl: 'wss://api.cloudflare.com/client/v4/accounts/account/browser-run/devtools/browser/session', token: 'token', sessionId: 'session', home, mode: 'write' };
  await assert.rejects(companion.open(args), error => error.code === 'PROFILE_WRITE_GATE');
  const opened = await companion.open({ ...args, compatibilityVerified: true });
  assert.equal(opened.contextId, 'context');
  captureFails = true;
  await assert.rejects(companion.save());
  assert.deepEqual(JSON.parse(readFileSync(path(home), 'utf8')), fixture());
  await companion.closeContext();
  companion.release();
}));

test('companion rejects an incomplete restoration and retains original state', () => withHome(async home => {
  const store = new ProfileStore(home, 'main', 'seed'); store.acquire(); store.save(fixture()); store.release();
  const context = { async newPage() { return {}; }, async newCDPSession() { return { async send() { return { targetInfo: { targetId: 't', browserContextId: 'c' } }; }, async detach() {} }; }, async storageState() { return { cookies: [], origins: [] }; }, async close() {} };
  const companion = new Companion({ async connectOverCDP() { return { async newContext() { return context; }, async close() {} }; } });
  await assert.rejects(companion.open({ wsUrl: 'wss://api.cloudflare.com/client/v4/accounts/a/browser-run/devtools/browser/s', token: 'token', sessionId: 's', home, mode: 'write', compatibilityVerified: true }), error => error.code === 'PROFILE_RESTORE_INCOMPLETE');
  assert.deepEqual(JSON.parse(readFileSync(path(home), 'utf8')), fixture());
}));


test('missing optional Playwright returns an actionable redacted error', () => withHome(home => {
  const packageDir = fileURLToPath(new URL('..', import.meta.url));
  copyFileSync(join(packageDir, 'companion.mjs'), join(home, 'companion.mjs'));
  copyFileSync(join(packageDir, 'profiles.mjs'), join(home, 'profiles.mjs'));
  const request = { id: 1, method: 'open', params: { token: 'private-test-token' } };
  const child = spawnSync(process.execPath, [join(home, 'companion.mjs')], { input: JSON.stringify(request) + '\n', encoding: 'utf8' });
  assert.equal(child.status, 0);
  const result = JSON.parse(child.stdout);
  assert.equal(result.error.code, 'PLAYWRIGHT_MISSING');
  assert.doesNotMatch(child.stdout + child.stderr, /private-test-token/);
}));


test('read-only profile rejects incomplete restoration without touching saved state', () => withHome(async home => {
  const store = new ProfileStore(home, 'main', 'seed'); store.acquire(); store.save(fixture()); store.release();
  const context = { async newPage() { return {}; }, async newCDPSession() { return { async send() { return { targetInfo: { targetId: 't', browserContextId: 'c' } }; }, async detach() {} }; }, async storageState(options) { assert.deepEqual(options, { indexedDB: true }); return { cookies: [], origins: [] }; }, async close() {} };
  const companion = new Companion({ async connectOverCDP() { return { async newContext() { return context; }, async close() {} }; } });
  await assert.rejects(companion.open({ wsUrl: 'wss://api.cloudflare.com/client/v4/accounts/a/browser-run/devtools/browser/s', token: 'token', sessionId: 's', home, mode: 'read' }), error => error.code === 'PROFILE_RESTORE_INCOMPLETE');
  assert.deepEqual(JSON.parse(readFileSync(path(home), 'utf8')), fixture());
}));


test('restore IPC remains disabled and never changes saved profiles', () => withHome(home => {
  const store = new ProfileStore(home, 'main', 'seed'); store.acquire(); store.save(fixture()); store.release();
  const companion = fileURLToPath(new URL('../companion.mjs', import.meta.url));
  const child = spawnSync(process.execPath, [companion], { input: JSON.stringify({ id: 1, method: 'restore', params: { profile: 'main' } }) + '\n', encoding: 'utf8', env: { ...process.env, CFB_HOME: home } });
  assert.equal(child.status, 0);
  assert.equal(JSON.parse(child.stdout).error.code, 'PROFILE_RESTORE_DISABLED');
  assert.deepEqual(JSON.parse(readFileSync(path(home), 'utf8')), fixture());
}));
