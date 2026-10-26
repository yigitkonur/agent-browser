// The file layout and cross-process lock protocol are compatible with
// yigitkonur/plugin-cf-browser, plugins/cf-browser/server/src/profiles.ts.
import { closeSync, existsSync, fsyncSync, mkdirSync, openSync, readFileSync, renameSync, rmdirSync, statSync, unlinkSync, writeSync } from 'node:fs';
import { randomBytes } from 'node:crypto';
import { join } from 'node:path';

const PROFILE_NAME = /^[a-z0-9][a-z0-9._-]{0,63}$/;
const STALE_MS = 120000;

export class ProfileError extends Error {
  constructor(code, message) { super(message); this.code = code; }
}

export function profileDirectory(home, name) {
  if (!PROFILE_NAME.test(name)) throw new ProfileError('BAD_PROFILE_NAME', 'Profile name must start with a lowercase letter or digit and contain only lowercase letters, digits, dot, underscore, and hyphen.');
  return join(home, 'profiles', name);
}

function ensureDir(path) { mkdirSync(path, { recursive: true, mode: 0o700 }); }

function readJson(path, allowMissing = false) {
  try { return JSON.parse(readFileSync(path, 'utf8')); }
  catch (error) {
    if (allowMissing && error.code === 'ENOENT') return undefined;
    throw new ProfileError('PROFILE_READ_FAILED', 'Profile data is unreadable or invalid.');
  }
}

function readLock(path) {
  try { return JSON.parse(readFileSync(path, 'utf8')); } catch { return undefined; }
}

function atomicWrite(path, value) {
  const tmp = `${path}.tmp-${process.pid}-${randomBytes(4).toString('hex')}`;
  const fd = openSync(tmp, 'wx', 0o600);
  try { writeSync(fd, value); fsyncSync(fd); } catch (error) { try { unlinkSync(tmp); } catch {} throw error; } finally { closeSync(fd); }
  try { renameSync(tmp, path); } catch (error) { try { unlinkSync(tmp); } catch {} throw error; }
}

function pidAlive(pid) {
  if (!Number.isInteger(pid) || pid <= 0) return false;
  try { process.kill(pid, 0); return true; } catch (error) { return error.code === 'EPERM'; }
}
function isStale(lock) { return !pidAlive(lock.serverPid) || Date.now() - Date.parse(lock.updatedAt) > STALE_MS; }

function summarize(state) {
  const sites = new Set();
  for (const cookie of state.cookies || []) if (cookie.domain) sites.add(String(cookie.domain).replace(/^\./, ''));
  for (const origin of state.origins || []) { try { sites.add(new URL(origin.origin).hostname); } catch {} }
  const expires = (state.cookies || []).map(cookie => cookie.expires).filter(value => typeof value === 'number' && value > 0).sort((a, b) => a - b)[0];
  return { sites: [...sites].sort(), cookieCount: (state.cookies || []).length, ...(expires ? { earliestExpiry: new Date(expires * 1000).toISOString() } : {}) };
}

export class ProfileStore {
  constructor(home, name, sessionId, label = 'agent-browser') {
    this.dir = profileDirectory(home, name);
    this.name = name;
    this.sessionId = sessionId;
    this.label = label;
    this.nonce = randomBytes(6).toString('hex');
    this.ownsLock = false;
    this.firstSave = true;
  }

  load() {
    const state = readJson(join(this.dir, 'state.json'), true);
    if (state !== undefined && (!Array.isArray(state.cookies) || !Array.isArray(state.origins))) throw new ProfileError('PROFILE_READ_FAILED', 'Profile state has an invalid structure.');
    return state;
  }

  acquire() {
    ensureDir(this.dir);
    const path = join(this.dir, 'lock.json');
    const now = new Date().toISOString();
    const info = { serverPid: process.pid, nonce: this.nonce, claudePid: process.ppid, sessionId: this.sessionId, label: this.label, since: now, updatedAt: now };
    for (let attempt = 0; attempt < 5; attempt++) {
      try {
        const fd = openSync(path, 'wx', 0o600);
        try { writeSync(fd, JSON.stringify(info)); fsyncSync(fd); } finally { closeSync(fd); }
        this.ownsLock = true;
        return;
      } catch (error) {
        if (error.code !== 'EEXIST') throw error;
        const holder = readLock(path);
        if (holder && !isStale(holder)) throw new ProfileError('PROFILE_IN_USE', 'Profile is in use; clone it, open it read-only, or retry later.');
        const guard = join(this.dir, 'lock.takeover');
        try { mkdirSync(guard, { mode: 0o700 }); } catch {
          try { if (Date.now() - statSync(guard).mtimeMs > 10000) rmdirSync(guard); } catch {}
          Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 100);
          continue;
        }
        try {
          const again = readLock(path);
          if (again && !isStale(again)) throw new ProfileError('PROFILE_IN_USE', 'Profile is in use; clone it, open it read-only, or retry later.');
          if (JSON.stringify(again) === JSON.stringify(holder)) try { unlinkSync(path); } catch {}
        } finally { try { rmdirSync(guard); } catch {} }
      }
    }
    throw new ProfileError('PROFILE_IN_USE', 'Could not acquire the profile lock.');
  }

  assertOwned() {
    const lock = readLock(join(this.dir, 'lock.json'));
    if (!this.ownsLock || !lock || lock.nonce !== this.nonce || lock.sessionId !== this.sessionId) throw new ProfileError('PROFILE_LOCK_LOST', 'Profile lock was lost; refusing to save.');
    return lock;
  }

  heartbeat() {
    const lock = this.assertOwned();
    atomicWrite(join(this.dir, 'lock.json'), JSON.stringify({ ...lock, updatedAt: new Date().toISOString() }));
  }

  save(state) {
    this.assertOwned();
    if (!state || !Array.isArray(state.cookies) || !Array.isArray(state.origins)) throw new ProfileError('PROFILE_SAVE_FAILED', 'Refusing to save incomplete profile state.');
    const path = join(this.dir, 'state.json');
    const existing = readJson(join(this.dir, 'meta.json'), true) || {};
    // Preserve the previous state before replacing the current file. A failed
    // replacement never removes the current state.
    if (this.firstSave && existsSync(path)) {
      const previous = readFileSync(path, 'utf8');
      atomicWrite(join(this.dir, 'state.prev.json'), previous);
    }
    atomicWrite(path, JSON.stringify(state));
    const metadata = { name: this.name, createdAt: existing.createdAt || new Date().toISOString(), updatedAt: new Date().toISOString(), savedFromSession: this.sessionId, ...summarize(state) };
    atomicWrite(join(this.dir, 'meta.json'), JSON.stringify(metadata, null, 2));
    this.firstSave = false;
  }

  release() {
    if (!this.ownsLock) return;
    const path = join(this.dir, 'lock.json');
    const lock = readLock(path);
    if (lock && lock.nonce === this.nonce && lock.sessionId === this.sessionId) unlinkSync(path);
    this.ownsLock = false;
  }
}

export function cloneProfile(home, from, to) {
  const source = profileDirectory(home, from);
  const destination = profileDirectory(home, to);
  if (!existsSync(join(source, 'state.json')) && !existsSync(join(source, 'meta.json'))) throw new ProfileError('PROFILE_NOT_FOUND', 'Source profile does not exist.');
  if (existsSync(destination)) throw new ProfileError('PROFILE_EXISTS', 'Destination profile already exists.');
  const state = readJson(join(source, 'state.json'), true);
  const metadata = readJson(join(source, 'meta.json'), true) || {};
  ensureDir(join(home, 'profiles'));
  try { mkdirSync(destination, { mode: 0o700 }); } catch (error) { if (error.code === 'EEXIST') throw new ProfileError('PROFILE_EXISTS', 'Destination profile already exists.'); throw error; }
  if (state) atomicWrite(join(destination, 'state.json'), JSON.stringify(state));
  atomicWrite(join(destination, 'meta.json'), JSON.stringify({ ...metadata, name: to, createdAt: new Date().toISOString(), updatedAt: new Date().toISOString(), savedFromSession: `clone of ${from}` }, null, 2));
}
