(() => {
  'use strict';

  const DB_NAME = 'puffinbox-offline-cache';
  const DB_VERSION = 2;
  const PACKAGE_STORE = 'packages';
  const CHUNK_STORE = 'chunks';
  const SETTINGS_STORE = 'settings';
  const CHUNK_SIZE = 1024 * 1024;
  const MAX_ITEM_SIZE = 8 * 1024 * 1024 * 1024;
  const SHA256_K = new Uint32Array([
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
  ]);

  class Sha256 {
    constructor() {
      this.state = new Uint32Array([0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19]);
      this.buffer = new Uint8Array(64);
      this.words = new Uint32Array(64);
      this.buffered = 0;
      this.byteLength = 0;
      this.finished = false;
    }

    update(input) {
      if (this.finished) throw new Error('The digest is already finalized.');
      const bytes = input instanceof Uint8Array ? input : new Uint8Array(input);
      this.byteLength += bytes.byteLength;
      let offset = 0;
      if (this.buffered) {
        const count = Math.min(64 - this.buffered, bytes.length);
        this.buffer.set(bytes.subarray(0, count), this.buffered);
        this.buffered += count;
        offset += count;
        if (this.buffered === 64) {
          this.compress(this.buffer);
          this.buffered = 0;
        }
      }
      while (offset + 64 <= bytes.length) {
        this.compress(bytes.subarray(offset, offset + 64));
        offset += 64;
      }
      if (offset < bytes.length) {
        this.buffer.set(bytes.subarray(offset), 0);
        this.buffered = bytes.length - offset;
      }
      return this;
    }

    compress(block) {
      const words = this.words;
      for (let index = 0; index < 16; index += 1) {
        const offset = index * 4;
        words[index] = (((block[offset] << 24) | (block[offset + 1] << 16) | (block[offset + 2] << 8) | block[offset + 3]) >>> 0);
      }
      for (let index = 16; index < 64; index += 1) {
        const x = words[index - 15];
        const y = words[index - 2];
        const sigma0 = ((x >>> 7) | (x << 25)) ^ ((x >>> 18) | (x << 14)) ^ (x >>> 3);
        const sigma1 = ((y >>> 17) | (y << 15)) ^ ((y >>> 19) | (y << 13)) ^ (y >>> 10);
        words[index] = (words[index - 16] + sigma0 + words[index - 7] + sigma1) >>> 0;
      }
      let [a, b, c, d, e, f, g, h] = this.state;
      for (let index = 0; index < 64; index += 1) {
        const sum1 = ((e >>> 6) | (e << 26)) ^ ((e >>> 11) | (e << 21)) ^ ((e >>> 25) | (e << 7));
        const choose = (e & f) ^ (~e & g);
        const temp1 = (h + sum1 + choose + SHA256_K[index] + words[index]) >>> 0;
        const sum0 = ((a >>> 2) | (a << 30)) ^ ((a >>> 13) | (a << 19)) ^ ((a >>> 22) | (a << 10));
        const majority = (a & b) ^ (a & c) ^ (b & c);
        const temp2 = (sum0 + majority) >>> 0;
        h = g; g = f; f = e; e = (d + temp1) >>> 0;
        d = c; c = b; b = a; a = (temp1 + temp2) >>> 0;
      }
      this.state[0] = (this.state[0] + a) >>> 0;
      this.state[1] = (this.state[1] + b) >>> 0;
      this.state[2] = (this.state[2] + c) >>> 0;
      this.state[3] = (this.state[3] + d) >>> 0;
      this.state[4] = (this.state[4] + e) >>> 0;
      this.state[5] = (this.state[5] + f) >>> 0;
      this.state[6] = (this.state[6] + g) >>> 0;
      this.state[7] = (this.state[7] + h) >>> 0;
    }

    digestHex() {
      if (!this.finished) {
        const bitLength = this.byteLength * 8;
        this.buffer[this.buffered] = 0x80;
        this.buffer.fill(0, this.buffered + 1);
        if (this.buffered >= 56) {
          this.compress(this.buffer);
          this.buffer.fill(0);
        }
        const high = Math.floor(bitLength / 0x100000000);
        const low = bitLength >>> 0;
        const offset = 56;
        this.buffer[offset] = (high >>> 24) & 0xff;
        this.buffer[offset + 1] = (high >>> 16) & 0xff;
        this.buffer[offset + 2] = (high >>> 8) & 0xff;
        this.buffer[offset + 3] = high & 0xff;
        this.buffer[offset + 4] = (low >>> 24) & 0xff;
        this.buffer[offset + 5] = (low >>> 16) & 0xff;
        this.buffer[offset + 6] = (low >>> 8) & 0xff;
        this.buffer[offset + 7] = low & 0xff;
        this.compress(this.buffer);
        this.finished = true;
      }
      return Array.from(this.state, (word) => word.toString(16).padStart(8, '0')).join('');
    }
  }

  function requestResult(request) {
    return new Promise((resolve, reject) => {
      request.onsuccess = () => resolve(request.result);
      request.onerror = () => reject(request.error || new Error('Local storage request failed.'));
    });
  }

  function transactionDone(transaction) {
    return new Promise((resolve, reject) => {
      transaction.oncomplete = resolve;
      transaction.onabort = () => reject(transaction.error || new Error('Local storage transaction was cancelled.'));
      transaction.onerror = () => reject(transaction.error || new Error('Local storage transaction failed.'));
    });
  }

  function openDatabase() {
    if (!globalThis.indexedDB) return Promise.reject(new Error('This browser does not provide local offline storage.'));
    return new Promise((resolve, reject) => {
      const request = indexedDB.open(DB_NAME, DB_VERSION);
      request.onupgradeneeded = () => {
        const db = request.result;
        if (!db.objectStoreNames.contains(PACKAGE_STORE)) {
          const store = db.createObjectStore(PACKAGE_STORE, { keyPath: 'key' });
          store.createIndex('accountId', 'accountId', { unique: false });
          store.createIndex('urlToken', 'urlToken', { unique: true });
        }
        if (!db.objectStoreNames.contains(CHUNK_STORE)) {
          const store = db.createObjectStore(CHUNK_STORE, { keyPath: 'key' });
          store.createIndex('accountId', 'accountId', { unique: false });
          store.createIndex('packageKey', 'packageKey', { unique: false });
        }
        if (!db.objectStoreNames.contains(SETTINGS_STORE)) db.createObjectStore(SETTINGS_STORE, { keyPath: 'key' });
      };
      request.onsuccess = () => resolve(request.result);
      request.onerror = () => reject(request.error || new Error('Offline storage could not be opened.'));
      request.onblocked = () => reject(new Error('Close other Puffinbox tabs to upgrade offline storage.'));
    });
  }

  function accountPackageKey(accountId, packageId) {
    return `${String(accountId)}:${String(packageId)}`;
  }

  function packageEpochKey(accountId, packageId) {
    return `package-epoch:${accountPackageKey(accountId, packageId)}`;
  }

  function activeAccountMatches(active, accountId, generation) {
    return active?.accountId === String(accountId)
      && typeof generation === 'string'
      && generation.length > 0
      && active.generation === generation;
  }

  async function requireActiveAccount(transaction, accountId, generation) {
    const active = await requestResult(transaction.objectStore(SETTINGS_STORE).get('active-account'));
    if (!activeAccountMatches(active, accountId, generation)) {
      try { transaction.abort(); } catch (_) { /* It may already be aborting. */ }
      throw new Error('The active offline account changed.');
    }
    return active;
  }

  function randomToken() {
    const bytes = new Uint8Array(16);
    if (!globalThis.crypto?.getRandomValues) throw new Error('Secure local IDs are unavailable in this browser.');
    crypto.getRandomValues(bytes);
    return Array.from(bytes, (byte) => byte.toString(16).padStart(2, '0')).join('');
  }

  function nextActiveAccountRecord(prior, accountId, expectedGeneration, createToken = randomToken) {
    if (expectedGeneration !== undefined) {
      const expected = expectedGeneration || null;
      const actual = prior?.generation || null;
      const sameNewAccount = expected === null && String(accountId || '') !== ''
        && prior?.accountId === String(accountId);
      if (actual !== expected && !sameNewAccount) {
        throw new Error('The offline account changed while this page was opening. Reload before saving local copies.');
      }
    }
    if (accountId == null || String(accountId) === '') {
      return { key: 'active-account', accountId: null, generation: createToken() };
    }
    const account = String(accountId);
    const generation = prior?.accountId === account && typeof prior.generation === 'string'
      ? prior.generation
      : createToken();
    return { key: 'active-account', accountId: account, generation };
  }

  function packageTransferMatches(active, packageRow, accountId, generation, transferToken) {
    const account = String(accountId);
    return activeAccountMatches(active, account, generation)
      && packageRow?.accountId === account
      && typeof transferToken === 'string'
      && transferToken.length > 0
      && packageRow.transferToken === transferToken;
  }

  function packageTransferCanBeRemoved(packageRow, expectedTransferToken) {
    return typeof expectedTransferToken === 'string'
      && expectedTransferToken.length > 0
      && packageRow?.transferToken === expectedTransferToken;
  }

  function nextPackageEpoch(current, expectedEpoch, createToken = randomToken) {
    const expected = expectedEpoch || null;
    const actual = current?.value || null;
    if (actual !== expected) throw new Error('This offline package was removed or replaced. Refresh before saving it again.');
    return { value: createToken() };
  }

  async function settleLogout(revokeServerSession, clearLocalCopies) {
    let serverRevoked = false;
    let localCopiesCleared = false;
    let serverError = null;
    let localError = null;
    try { await revokeServerSession(); serverRevoked = true; }
    catch (error) { serverError = error; }
    try { await clearLocalCopies(); localCopiesCleared = true; }
    catch (error) { localError = error; }
    return { serverRevoked, localCopiesCleared, serverError, localError };
  }

  function localSignOutApplies(savedMarker, activeSession) {
    if (savedMarker === '1') return true;
    if (!savedMarker) return false;
    let marker;
    try { marker = JSON.parse(savedMarker); } catch (_) { return false; }
    if (!marker || typeof marker.accountId !== 'string' || typeof marker.generation !== 'string') return false;
    return !activeSession
      || (activeSession.accountId === marker.accountId && activeSession.generation === marker.generation);
  }

  function offlineStorageErrorCode(error) {
    const name = String(error?.name || '').toLowerCase();
    const code = Number(error?.code);
    const message = String(error?.message || error || '').toLowerCase();
    if (name === 'quotaexceedederror' || code === 22 || code === 1014
        || /quota|storage (?:is )?(?:full|exceeded)|(?:disk|device) (?:is )?full/.test(`${name} ${message}`)) {
      return 'storage-quota';
    }
    if (name === 'securityerror' || name === 'invalidstateerror'
        || /storage (?:is )?(?:blocked|disabled|unavailable)|indexeddb.*(?:blocked|disabled|unavailable)/.test(`${name} ${message}`)) {
      return 'storage-unavailable';
    }
    return null;
  }

  async function listPackages(accountId) {
    const db = await openDatabase();
    try {
      const transaction = db.transaction(PACKAGE_STORE, 'readonly');
      const done = transactionDone(transaction);
      const store = transaction.objectStore(PACKAGE_STORE);
      const rows = await requestResult(store.index('accountId').getAll(String(accountId)));
      await done;
      return rows.sort((left, right) => String(left.itemName).localeCompare(String(right.itemName)));
    } finally { db.close(); }
  }

  async function getPackage(accountId, packageId) {
    const db = await openDatabase();
    try {
      const transaction = db.transaction(PACKAGE_STORE, 'readonly');
      const done = transactionDone(transaction);
      const row = await requestResult(transaction.objectStore(PACKAGE_STORE).get(accountPackageKey(accountId, packageId)));
      await done;
      return row || null;
    } finally { db.close(); }
  }

  async function getPackageByToken(accountId, token) {
    const db = await openDatabase();
    try {
      const transaction = db.transaction([PACKAGE_STORE, SETTINGS_STORE], 'readonly');
      const done = transactionDone(transaction);
      const [row, active] = await Promise.all([
        requestResult(transaction.objectStore(PACKAGE_STORE).index('urlToken').get(String(token))),
        requestResult(transaction.objectStore(SETTINGS_STORE).get('active-account')),
      ]);
      await done;
      return active?.accountId === String(accountId) && row?.accountId === active.accountId
        && typeof row.transferToken === 'string' && row.transferToken.length > 0 && row.status === 'complete'
        ? { ...row, accountGeneration: active.generation }
        : null;
    } finally { db.close(); }
  }

  async function getPackageGeneration(accountId, packageId, generation) {
    const db = await openDatabase();
    try {
      const transaction = db.transaction(SETTINGS_STORE, 'readonly');
      const done = transactionDone(transaction);
      const [active, packageEpoch] = await Promise.all([
        requestResult(transaction.objectStore(SETTINGS_STORE).get('active-account')),
        requestResult(transaction.objectStore(SETTINGS_STORE).get(packageEpochKey(accountId, packageId))),
      ]);
      await done;
      if (!activeAccountMatches(active, accountId, generation)) throw new Error('The active offline account changed.');
      return packageEpoch?.value || null;
    } finally { db.close(); }
  }

  async function startPackageTransfer(accountId, packageId, values, generation, expectedPackageEpoch) {
    const account = String(accountId);
    const db = await openDatabase();
    try {
      const transaction = db.transaction([PACKAGE_STORE, SETTINGS_STORE], 'readwrite');
      const done = transactionDone(transaction);
      try {
        await requireActiveAccount(transaction, account, generation);
        const store = transaction.objectStore(PACKAGE_STORE);
        const settings = transaction.objectStore(SETTINGS_STORE);
        const key = accountPackageKey(account, packageId);
        const prior = await requestResult(store.get(key));
        const epochKey = packageEpochKey(account, packageId);
        const currentEpoch = await requestResult(settings.get(epochKey));
        let nextEpoch;
        try { nextEpoch = nextPackageEpoch(currentEpoch, expectedPackageEpoch); }
        catch (error) {
          try { transaction.abort(); } catch (_) { /* It may already be aborting. */ }
          throw error;
        }
        const transferToken = nextEpoch.value;
        settings.put({ key: epochKey, value: transferToken });
        const row = {
          ...(prior || {}), ...values,
          key, accountId: account, packageId: String(packageId),
          urlToken: prior?.urlToken || randomToken(),
          transferToken,
          updatedAt: new Date().toISOString(),
        };
        store.put(row);
        await done;
        return transferToken;
      } catch (error) {
        await done.catch(() => {});
        throw error;
      }
    } finally { db.close(); }
  }

  async function updatePackageTransfer(accountId, packageId, values, generation, transferToken) {
    const account = String(accountId);
    const db = await openDatabase();
    try {
      const transaction = db.transaction([PACKAGE_STORE, SETTINGS_STORE], 'readwrite');
      const done = transactionDone(transaction);
      try {
        await requireActiveAccount(transaction, account, generation);
        const store = transaction.objectStore(PACKAGE_STORE);
        const key = accountPackageKey(account, packageId);
        const prior = await requestResult(store.get(key));
        const active = await requestResult(transaction.objectStore(SETTINGS_STORE).get('active-account'));
        if (!packageTransferMatches(active, prior, account, generation, transferToken)) {
          try { transaction.abort(); } catch (_) { /* It may already be aborting. */ }
          throw new Error('This offline transfer was removed or replaced.');
        }
        const row = { ...prior, ...values, key, accountId: account, packageId: String(packageId), updatedAt: new Date().toISOString() };
        store.put(row);
        await done;
        return row;
      } catch (error) {
        await done.catch(() => {});
        throw error;
      }
    } finally { db.close(); }
  }

  async function putChunk(accountId, packageId, index, bytes, digest, generation, transferToken) {
    const account = String(accountId);
    const packageKey = accountPackageKey(account, packageId);
    const data = bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes);
    const actualDigest = new Sha256().update(data).digestHex();
    if (actualDigest !== String(digest).toLowerCase()) throw new Error('A downloaded chunk failed its SHA-256 check.');
    const db = await openDatabase();
    try {
      const transaction = db.transaction([CHUNK_STORE, PACKAGE_STORE, SETTINGS_STORE], 'readwrite');
      const done = transactionDone(transaction);
      try {
        await requireActiveAccount(transaction, account, generation);
        const packageRow = await requestResult(transaction.objectStore(PACKAGE_STORE).get(packageKey));
        const active = await requestResult(transaction.objectStore(SETTINGS_STORE).get('active-account'));
        if (!packageTransferMatches(active, packageRow, account, generation, transferToken)) {
          try { transaction.abort(); } catch (_) { /* It may already be aborting. */ }
          throw new Error('This offline transfer was removed or replaced.');
        }
        transaction.objectStore(CHUNK_STORE).put({
          key: `${packageKey}:${index}`, packageKey, accountId: account,
          packageId: String(packageId), index, digest: actualDigest,
          bytes: data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength),
        });
        await done;
      } catch (error) {
        await done.catch(() => {});
        throw error;
      }
    } finally { db.close(); }
  }

  async function getChunk(accountId, packageId, index, generation, transferToken) {
    return getChunkForActiveAccount(accountId, packageId, index, generation, transferToken);
  }

  async function getChunkForActiveAccount(accountId, packageId, index, generation, transferToken) {
    const account = String(accountId);
    const db = await openDatabase();
    try {
      const transaction = db.transaction([CHUNK_STORE, PACKAGE_STORE, SETTINGS_STORE], 'readonly');
      const done = transactionDone(transaction);
      const [row, packageRow, active] = await Promise.all([
        requestResult(transaction.objectStore(CHUNK_STORE).get(`${accountPackageKey(account, packageId)}:${index}`)),
        requestResult(transaction.objectStore(PACKAGE_STORE).get(accountPackageKey(account, packageId))),
        requestResult(transaction.objectStore(SETTINGS_STORE).get('active-account')),
      ]);
      await done;
      return packageTransferMatches(active, packageRow, account, generation, transferToken)
        && row?.accountId === account
        ? row
        : null;
    } finally { db.close(); }
  }

  async function chunkIndexes(accountId, packageId, generation, transferToken) {
    const packageKey = accountPackageKey(accountId, packageId);
    const db = await openDatabase();
    try {
      const transaction = db.transaction([CHUNK_STORE, PACKAGE_STORE, SETTINGS_STORE], 'readonly');
      const done = transactionDone(transaction);
      const request = transaction.objectStore(CHUNK_STORE).index('packageKey').openCursor(IDBKeyRange.only(packageKey));
      const indexResult = new Promise((resolve, reject) => {
        const result = [];
        request.onerror = () => reject(request.error || new Error('Offline storage could not be read.'));
        request.onsuccess = () => {
          const cursor = request.result;
          if (!cursor) { resolve(result); return; }
          result.push(cursor.value.index);
          cursor.continue();
        };
      });
      const activeResult = requestResult(transaction.objectStore(SETTINGS_STORE).get('active-account'));
      const packageResult = requestResult(transaction.objectStore(PACKAGE_STORE).get(packageKey));
      const [indexes, active, packageRow] = await Promise.all([indexResult, activeResult, packageResult]);
      await done;
      if (!packageTransferMatches(active, packageRow, accountId, generation, transferToken)) return [];
      return indexes.sort((left, right) => left - right);
    } finally { db.close(); }
  }

  async function removePackage(accountId, packageId, generation, expectedTransferToken) {
    const packageKey = accountPackageKey(accountId, packageId);
    const db = await openDatabase();
    try {
      const transaction = db.transaction([PACKAGE_STORE, CHUNK_STORE, SETTINGS_STORE], 'readwrite');
      const done = transactionDone(transaction);
      try {
        await requireActiveAccount(transaction, accountId, generation);
        const packageStore = transaction.objectStore(PACKAGE_STORE);
        const packageRow = await requestResult(packageStore.get(packageKey));
        if (!packageTransferCanBeRemoved(packageRow, expectedTransferToken)) {
          try { transaction.abort(); } catch (_) { /* It may already be aborting. */ }
          throw new Error('This offline package changed in another tab. Refresh before removing it.');
        }
        packageStore.delete(packageKey);
        const nextEpoch = nextPackageEpoch(
          await requestResult(transaction.objectStore(SETTINGS_STORE).get(packageEpochKey(accountId, packageId))),
          packageRow.transferToken,
        );
        transaction.objectStore(SETTINGS_STORE).put({ key: packageEpochKey(accountId, packageId), ...nextEpoch });
        const request = transaction.objectStore(CHUNK_STORE).index('packageKey').openCursor(IDBKeyRange.only(packageKey));
        request.onsuccess = () => {
          const cursor = request.result;
          if (cursor) { cursor.delete(); cursor.continue(); }
        };
        await done;
        return nextEpoch.value;
      } catch (error) {
        await done.catch(() => {});
        throw error;
      }
    } finally { db.close(); }
  }

  async function deactivateAccount(accountId, generation) {
    const account = String(accountId);
    const db = await openDatabase();
    try {
      const transaction = db.transaction(SETTINGS_STORE, 'readwrite');
      const done = transactionDone(transaction);
      try {
        const active = await requestResult(transaction.objectStore(SETTINGS_STORE).get('active-account'));
        if (!activeAccountMatches(active, account, generation)) {
          try { transaction.abort(); } catch (_) { /* It may already be aborting. */ }
          throw new Error('The active offline account changed; local copies were left untouched.');
        }
        const logoutGeneration = randomToken();
        transaction.objectStore(SETTINGS_STORE).put({ key: 'active-account', accountId: null, generation: logoutGeneration });
        await done;
        return logoutGeneration;
      } catch (error) {
        await done.catch(() => {});
        throw error;
      }
    } finally { db.close(); }
  }

  async function forgetAccount(accountId, generation, logoutGeneration) {
    const account = String(accountId);
    const tombstone = logoutGeneration || await deactivateAccount(account, generation);
    const db = await openDatabase();
    try {
      const transaction = db.transaction([PACKAGE_STORE, CHUNK_STORE, SETTINGS_STORE], 'readwrite');
      const done = transactionDone(transaction);
      try {
        const settings = transaction.objectStore(SETTINGS_STORE);
        const active = await requestResult(settings.get('active-account'));
        if (active?.accountId !== null || active?.generation !== tombstone) {
          try { transaction.abort(); } catch (_) { /* It may already be aborting. */ }
          throw new Error('The active offline account changed; local copies were left untouched.');
        }
        const settingsCursor = transaction.objectStore(SETTINGS_STORE).openCursor();
        settingsCursor.onsuccess = () => {
          const cursor = settingsCursor.result;
          if (!cursor) return;
          if (typeof cursor.key === 'string' && cursor.key.startsWith(`package-epoch:${account}:`)) cursor.delete();
          cursor.continue();
        };
        for (const storeName of [PACKAGE_STORE, CHUNK_STORE]) {
          const request = transaction.objectStore(storeName).index('accountId').openCursor(IDBKeyRange.only(account));
          request.onsuccess = () => {
            const cursor = request.result;
            if (cursor) { cursor.delete(); cursor.continue(); }
          };
        }
        await done;
      } catch (error) {
        await done.catch(() => {});
        throw error;
      }
    } finally { db.close(); }
  }

  async function setActiveAccount(accountId, expectedGeneration) {
    const db = await openDatabase();
    try {
      const transaction = db.transaction(SETTINGS_STORE, 'readwrite');
      const done = transactionDone(transaction);
      const store = transaction.objectStore(SETTINGS_STORE);
      try {
        const prior = await requestResult(store.get('active-account'));
        let next;
        try { next = nextActiveAccountRecord(prior, accountId, expectedGeneration); }
        catch (error) {
          try { transaction.abort(); } catch (_) { /* It may already be aborting. */ }
          throw error;
        }
        store.put(next);
        await done;
        return next.generation;
      } catch (error) {
        await done.catch(() => {});
        throw error;
      }
    } finally { db.close(); }
  }

  async function getActiveSession() {
    const db = await openDatabase();
    try {
      const transaction = db.transaction(SETTINGS_STORE, 'readonly');
      const done = transactionDone(transaction);
      const row = await requestResult(transaction.objectStore(SETTINGS_STORE).get('active-account'));
      await done;
      return row || null;
    } finally { db.close(); }
  }

  async function getActiveAccount() {
    const db = await openDatabase();
    try {
      const transaction = db.transaction(SETTINGS_STORE, 'readonly');
      const done = transactionDone(transaction);
      const row = await requestResult(transaction.objectStore(SETTINGS_STORE).get('active-account'));
      await done;
      return row?.accountId || null;
    } finally { db.close(); }
  }

  async function verifyAndComplete(accountId, packageId, totalSize, expectedDigest, chunkSize, generation, transferToken) {
    const actualChunkSize = chunkSize || CHUNK_SIZE;
    if (!Number.isSafeInteger(totalSize) || totalSize < 0 || totalSize > MAX_ITEM_SIZE) throw new Error('The offline item exceeds the 8 GiB per-item limit.');
    if (!Number.isSafeInteger(actualChunkSize) || actualChunkSize < 1 || actualChunkSize > CHUNK_SIZE) throw new Error('The server returned an unsupported offline chunk size.');
    const chunkCount = Math.ceil(totalSize / actualChunkSize);
    const indexes = await chunkIndexes(accountId, packageId, generation, transferToken);
    if (indexes.length !== chunkCount || indexes.some((index, position) => index !== position)) return false;
    const hasher = new Sha256();
    let verifiedSize = 0;
    for (let index = 0; index < chunkCount; index += 1) {
      const row = await getChunkForActiveAccount(accountId, packageId, index, generation, transferToken);
      if (!row) return false;
      const bytes = new Uint8Array(row.bytes);
      const expectedLength = Math.min(actualChunkSize, totalSize - verifiedSize);
      if (bytes.byteLength !== expectedLength || new Sha256().update(bytes).digestHex() !== row.digest) {
        throw new Error(`Offline chunk ${index + 1} failed its local integrity check.`);
      }
      hasher.update(bytes);
      verifiedSize += bytes.byteLength;
    }
    if (verifiedSize !== totalSize || hasher.digestHex() !== String(expectedDigest).toLowerCase()) {
      throw new Error('The cached file did not match the server’s complete SHA-256 digest. Remove it and download it again.');
    }
    await updatePackageTransfer(accountId, packageId, { status: 'complete', sourceSize: totalSize, sha256: String(expectedDigest).toLowerCase(), chunkSize: actualChunkSize, chunkCount, completedAt: new Date().toISOString(), errorCode: null }, generation, transferToken);
    return true;
  }

  async function readSetting(key) {
    try { return globalThis.localStorage.getItem(key); } catch (_) { return null; }
  }

  async function writeSetting(key, value) {
    try { globalThis.localStorage.setItem(key, String(value)); return true; } catch (_) { return false; }
  }

  const api = {
    CHUNK_SIZE, MAX_ITEM_SIZE, Sha256, openDatabase, listPackages, getPackage,
    getPackageByToken, getPackageGeneration, startPackageTransfer, updatePackageTransfer, putChunk, getChunk, getChunkForActiveAccount, chunkIndexes,
    removePackage, deactivateAccount, forgetAccount, verifyAndComplete, readSetting, writeSetting,
    setActiveAccount, getActiveAccount, getActiveSession, activeAccountMatches,
    nextActiveAccountRecord, packageTransferMatches, packageTransferCanBeRemoved, nextPackageEpoch, settleLogout, localSignOutApplies,
    accountPackageKey, packageEpochKey, randomToken,
    offlineStorageErrorCode,
  };
  globalThis.PuffinboxOfflineCache = api;
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
})();
