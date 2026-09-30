'use strict';

const assert = require('node:assert/strict');
const { createResolver, readSetting, writeSetting } = require('../web/device-id.js');

function memoryStorage() {
  const values = new Map();
  return {
    getItem(key) { return values.has(key) ? values.get(key) : null; },
    setItem(key, value) { values.set(key, String(value)); },
  };
}

let generated = 0;
const createId = () => `00000000-0000-4000-8000-${String(++generated).padStart(12, '0')}`;

const firstProfileStorage = memoryStorage();
const firstProfile = createResolver([() => firstProfileStorage], createId);
const firstId = firstProfile();
assert.match(firstId, /^[0-9a-f-]{36}$/i);
assert.equal(firstProfile(), firstId, 'one browser profile must reuse its saved device ID');
assert.equal(createResolver([() => firstProfileStorage], createId)(), firstId, 'saved IDs must survive page reloads');

const secondProfile = createResolver([() => memoryStorage()], createId);
const secondId = secondProfile();
assert.notEqual(secondId, firstId, 'separate browser profiles must receive different device IDs');

const blockedStorage = { getItem() { throw new Error('storage blocked'); }, setItem() { throw new Error('storage blocked'); } };
const fallbackStorage = memoryStorage();
const fallbackProfile = createResolver([() => blockedStorage, () => fallbackStorage], createId);
const fallbackId = fallbackProfile();
assert.equal(fallbackProfile(), fallbackId, 'session storage fallback must remain stable during the browser session');
assert.equal(createResolver([() => blockedStorage, () => fallbackStorage], createId)(), fallbackId,
  'session storage fallback must be reused after page reloads in the same session');

const memoryOnly = createResolver([() => { throw new Error('storage disabled'); }], createId);
assert.equal(memoryOnly(), memoryOnly(), 'fully blocked storage must use a stable per-page memory identity');

const preferences = memoryStorage();
assert.equal(readSetting(() => preferences, 'theme', 'dark'), 'dark', 'missing preference must use its fallback');
writeSetting(() => preferences, 'theme', 'light');
assert.equal(readSetting(() => preferences, 'theme', 'dark'), 'light', 'available preferences must persist');
const unavailable = { getItem() { throw new Error('storage blocked'); }, setItem() { throw new Error('storage blocked'); } };
assert.equal(readSetting(() => unavailable, 'theme', 'dark'), 'dark', 'blocked preferences must not prevent startup');
assert.doesNotThrow(() => writeSetting(() => unavailable, 'theme', 'light'), 'blocked preferences must not break rendering');

process.stdout.write('Browser device identity storage tests passed.\n');
