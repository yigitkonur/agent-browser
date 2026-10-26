import { realpathSync } from 'node:fs';
import { homedir } from 'node:os';
import { join, resolve } from 'node:path';
import { createInterface } from 'node:readline';
import { pathToFileURL } from 'node:url';
import { ProfileStore, ProfileError, cloneProfile } from './profiles.mjs';

function profileHome(value) {
  const home = value || process.env.CFB_HOME || join(homedir(), '.cf-browser');
  return resolve(home === '~' ? homedir() : home.startsWith('~/') ? join(homedir(), home.slice(2)) : home);
}

function assertAllowedUrl(value, sessionId) {
  let parsed;
  try { parsed = new URL(value); } catch { throw new ProfileError('INVALID_CDP_URL', 'Cloudflare CDP URL is invalid.'); }
  if (parsed.protocol !== 'wss:' || parsed.hostname !== 'api.cloudflare.com' || parsed.username || parsed.password || parsed.hash || parsed.port || !/^\/client\/v4\/accounts\/[^/]+\/browser-run\/devtools\/browser\/[^/]+$/.test(parsed.pathname)) throw new ProfileError('INVALID_CDP_URL', 'Cloudflare CDP URL is not an approved browser endpoint.');
  let pathSession;
  try { pathSession = decodeURIComponent(parsed.pathname.split('/').at(-1)); } catch { throw new ProfileError('INVALID_CDP_URL', 'Cloudflare CDP URL is invalid.'); }
  if (pathSession !== sessionId) throw new ProfileError('INVALID_CDP_URL', 'Cloudflare CDP URL identifies a different browser session.');
  return parsed.toString();
}

// Compare JSON with object keys normalized, while preserving array order.
function canonical(value) {
  if (Array.isArray(value)) return value.map(canonical);
  if (value && typeof value === 'object') return Object.fromEntries(Object.keys(value).sort().map(key => [key, canonical(value[key])]));
  return value;
}
function jsonEqual(a, b) { return JSON.stringify(canonical(a)) === JSON.stringify(canonical(b)); }
function verifyRestored(expected, captured) {
  if (!expected) return;
  const actualOrigins = new Map(captured.origins.map(origin => [origin.origin, origin]));
  for (const origin of expected.origins) {
    const current = actualOrigins.get(origin.origin);
    if (!current || !jsonEqual(origin.localStorage || [], current.localStorage || []) || !jsonEqual(origin.indexedDB || [], current.indexedDB || [])) throw new ProfileError('PROFILE_RESTORE_INCOMPLETE', 'Saved origin data could not be restored completely; refusing write-back.');
  }
  const key = cookie => `${cookie.name}\0${cookie.domain}\0${cookie.path}`;
  const actualCookies = new Map(captured.cookies.map(cookie => [key(cookie), cookie]));
  for (const cookie of expected.cookies) {
    const actual = actualCookies.get(key(cookie));
    if (!actual || actual.value !== cookie.value) throw new ProfileError('PROFILE_RESTORE_INCOMPLETE', 'Saved cookies could not be restored completely; refusing write-back.');
  }
}

export class Companion {
  constructor(chromium) { this.chromium = chromium; this.store = null; this.browser = null; this.context = null; this.gate = false; }

  async open(args) {
    if (this.browser) throw new ProfileError('ALREADY_OPEN', 'Companion already owns a context.');
    const { sessionId, token } = args;
    if (typeof sessionId !== 'string' || !sessionId || typeof token !== 'string' || !token || /[\r\n]/.test(token)) throw new ProfileError('INVALID_CREDENTIALS', 'Missing or invalid browser credentials.');
    const endpoint = assertAllowedUrl(args.wsUrl, sessionId);
    const mode = args.mode ?? 'write';
    if (!['write', 'read', 'none'].includes(mode)) throw new ProfileError('BAD_PROFILE_MODE', 'Profile mode must be write, read, or none.');
    // Native caller can set this only after its end-to-end compatibility gate.
    this.gate = args.compatibilityVerified === true;
    if (mode === 'write' && !this.gate) throw new ProfileError('PROFILE_WRITE_GATE', 'Shared profile writes are disabled until the live compatibility test succeeds.');
    const home = profileHome(args.home);
    let state;
    try {
      if (mode !== 'none') {
        this.store = new ProfileStore(home, args.profile || 'main', sessionId, args.label);
        if (mode === 'write') this.store.acquire();
        state = this.store.load();
        this.originalOrigins = new Set((state?.origins || []).map(origin => origin.origin));
        if (mode === 'read' && !state) throw new ProfileError('PROFILE_NOT_FOUND', 'Read-only profile has no saved state.');
      }
      this.browser = await this.chromium.connectOverCDP(endpoint, { headers: { Authorization: `Bearer ${token}` }, timeout: 30000 });
      this.context = await this.browser.newContext({ storageState: state, viewport: { width: 1280, height: 800 } });
      const page = await this.context.newPage();
      const cdp = await this.context.newCDPSession(page);
      const result = await cdp.send('Target.getTargetInfo');
      await cdp.detach();
      const target = result?.targetInfo;
      if (!target?.targetId || !target?.browserContextId) throw new ProfileError('CONTEXT_NOT_IDENTIFIED', 'Could not identify the isolated browser context.');
      if (mode !== 'none') {
        const restored = await this.context.storageState({ indexedDB: true });
        verifyRestored(state, restored);
      }
      if (mode === 'write') {
        this.heartbeat = setInterval(() => { try { this.store.heartbeat(); } catch { this.lockLost = true; } }, 30000);
        this.heartbeat.unref();
      }
      this.mode = mode;
      return { contextId: target.browserContextId, targetId: target.targetId, profile: mode === 'none' ? null : this.store.name, mode };
    } catch (error) {
      await this.abort();
      throw error;
    }
  }

  async save() {
    if (this.mode !== 'write' || !this.gate || !this.context || this.lockLost) throw new ProfileError('PROFILE_SAVE_FAILED', 'Profile is not writable or its lock was lost.');
    // Do not fall back to storageState() without IndexedDB.
    const state = await this.context.storageState({ indexedDB: true });
    if (!state || !Array.isArray(state.cookies) || !Array.isArray(state.origins)) throw new ProfileError('PROFILE_SAVE_FAILED', 'Profile capture was incomplete; existing state was preserved.');
    const capturedOrigins = new Set(state.origins.map(origin => origin.origin));
    if ([...(this.originalOrigins || [])].some(origin => !capturedOrigins.has(origin))) throw new ProfileError('PROFILE_SAVE_INCOMPLETE', 'An existing origin was not captured; existing state was preserved.');
    this.store.save(state);
    return { profile: this.store.name, cookieCount: state.cookies.length, originCount: state.origins.length };
  }

  async closeContext() {
    if (this.context) await this.context.close();
    this.context = null;
    if (this.browser) await this.browser.close();
    this.browser = null;
    // Keep the lock until the caller confirms the remote browser has closed.
    return { closed: true };
  }

  release() {
    if (this.context || this.browser) throw new ProfileError('CONTEXT_STILL_OPEN', 'Close the context before releasing the profile lock.');
    if (this.heartbeat) clearInterval(this.heartbeat);
    this.heartbeat = null;
    if (this.store?.ownsLock) this.store.release();
    return { released: true };
  }

  async abort() {
    if (this.heartbeat) clearInterval(this.heartbeat);
    this.heartbeat = null;
    try { if (this.context) await this.context.close(); } catch {}
    try { if (this.browser) await this.browser.close(); } catch {}
    this.context = null; this.browser = null;
    try { if (this.store?.ownsLock) this.store.release(); } catch {}
  }
}

async function main() {
  let companion;
  const home = profileHome();
  let chain = Promise.resolve();
  const lines = createInterface({ input: process.stdin, crlfDelay: Infinity });
  for await (const line of lines) {
    chain = chain.then(async () => {
      let req;
      try {
        req = JSON.parse(line);
        let value;
        if (req.method === 'open') {
          if (!companion) {
            let chromium;
            try { ({ chromium } = await import('playwright')); }
            catch (error) {
              if (error?.code === 'ERR_MODULE_NOT_FOUND') throw new ProfileError('PLAYWRIGHT_MISSING', 'The Cloudflare profile companion requires the optional Playwright dependency; install the package dependencies and retry.');
              throw error;
            }
            companion = new Companion(chromium);
          }
          value = await companion.open(req.params || {});
        } else if (req.method === 'clone') { cloneProfile(home, req.params.from, req.params.to); value = { cloned: true }; }
        else if (req.method === 'restore') throw new ProfileError('PROFILE_RESTORE_DISABLED', 'Profile restore is disabled until crash-safe recovery and cross-process locking are verified.');
        else if (!companion) throw new ProfileError('NOT_OPEN', 'No profile context is open.');
        else if (req.method === 'save') value = await companion.save();
        else if (req.method === 'close') value = await companion.closeContext();
        else if (req.method === 'release') value = companion.release();
        else throw new ProfileError('UNKNOWN_METHOD', 'Unknown companion operation.');
        process.stdout.write(JSON.stringify({ id: req.id, ok: true, result: value }) + '\n');
      } catch (error) {
        // External errors may contain URLs, cookies, or credentials. Do not echo them.
        const code = error instanceof ProfileError ? error.code : 'COMPANION_FAILED';
        const message = error instanceof ProfileError ? error.message : 'Cloudflare profile operation failed.';
        process.stdout.write(JSON.stringify({ id: req?.id ?? null, ok: false, error: { code, message } }) + '\n');
      }
    });
  }
  await chain;
  if (companion) await companion.abort();
}

if (process.argv[1] && import.meta.url === pathToFileURL(realpathSync(process.argv[1])).href) await main();
