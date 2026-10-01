'use strict';

const assert = require('node:assert/strict');
const { createHash } = require('node:crypto');
const { spawn, spawnSync } = require('node:child_process');
const { once } = require('node:events');
const fs = require('node:fs');
const http = require('node:http');
const net = require('node:net');
const os = require('node:os');
const path = require('node:path');

const ROOT = path.resolve(__dirname, '..');
const ACCOUNT_ID = '7c9e6679-7425-40de-944b-e07fc1f90ae7';
const OTHER_ACCOUNT_ID = '92457fa0-7b8c-4c57-adbd-8d9cfecf125d';
const PACKAGE_ID = '0e44e61c-6a2d-45df-9fd8-fad4d0ec4c6a';
const SECOND_PACKAGE_ID = '108e5e60-d797-4dc8-a0c4-1a83cf5ad697';
const ITEM_ID = 'eaf4d85d-307d-4b30-985f-32a763d6822c';
const BOOK_ITEM_ID = 'c1e2e134-2dc4-40f3-a090-d14f4883db32';
const PHOTO_ITEM_ID = '6548a8dc-219f-4d79-8da9-555268297c6a';
const NATIVE_ITEM_ID = 'bf284644-104e-4e9c-8e51-fd39e8d94481';
const PHOTO_PNG = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==', 'base64');
const PRIVATE_FAILURE_BODY = 'SENSITIVE_FIXTURE_BODY_9e43';
const CHUNK_SIZE = 1024 * 1024;

function sha256(bytes) {
  return createHash('sha256').update(bytes).digest('hex');
}

function makeWave() {
  const bytes = new Uint8Array(CHUNK_SIZE + 44);
  const view = new DataView(bytes.buffer);
  const ascii = (offset, text) => { for (let index = 0; index < text.length; index += 1) bytes[offset + index] = text.charCodeAt(index); };
  ascii(0, 'RIFF');
  view.setUint32(4, bytes.byteLength - 8, true);
  ascii(8, 'WAVE');
  ascii(12, 'fmt ');
  view.setUint32(16, 16, true);
  view.setUint16(20, 1, true);
  view.setUint16(22, 1, true);
  view.setUint32(24, 8000, true);
  view.setUint32(28, 16000, true);
  view.setUint16(32, 2, true);
  view.setUint16(34, 16, true);
  ascii(36, 'data');
  view.setUint32(40, bytes.byteLength - 44, true);
  for (let offset = 44; offset < bytes.byteLength; offset += 2) {
    const sample = Math.round(Math.sin(((offset - 44) / 2) * Math.PI * 2 * 440 / 8000) * 7000);
    view.setInt16(offset, sample, true);
  }
  return bytes;
}

const audioBytes = makeWave();
const audioHash = sha256(audioBytes);
const audioChunks = [audioBytes.subarray(0, CHUNK_SIZE), audioBytes.subarray(CHUNK_SIZE)];
const audioChunkHashes = audioChunks.map(sha256);
let interruptedTail = true;
let corruptTailDigest = true;
const contentRangeCounts = new Map();
const requestsSeen = [];
const bookReaderRequests = [];
const photoFileRequests = [];
let userMeRequestCount = 0;
let mediaAccessTokenRestoreCount = 0;
const nativePlaybackRequests = [];
const nativeSessionRequests = [];
let nativeUserData = { Played: false, PlaybackPositionTicks: 1_600_000_000 };

function nativePlayerFixture() {
  // Simulate only the documented client bridge. Decoding and mpv seeking are
  // covered separately with the installed Jellyfin Media Player.
  return `(() => {
    document.cookie = 'native-token-denied=; Max-Age=0; Path=/';
    document.cookie = 'native-hls-test=1; Path=/; SameSite=Strict';
    const signal = () => {
      const listeners = new Set();
      return { connect: (listener) => listeners.add(listener), disconnect: (listener) => listeners.delete(listener),
        emit: (value) => { for (const listener of [...listeners]) listener(value); } };
    };
    const fixture = window.nativePlaybackFixture = { calls: [], position: 0, paused: false, generation: 0 };
    const player = fixture.player = {
      playing: signal(), paused: signal(), finished: signal(), canceled: signal(), error: signal(),
      positionUpdate: signal(), updateDuration: signal(),
      load(url, options, data, audio, subtitle, callback) {
        fixture.calls.push(['load', url, options]);
        fixture.position = 0; fixture.paused = !options.autoplay;
        const generation = ++fixture.generation;
        callback(true);
        setTimeout(() => {
          if (fixture.generation !== generation) return;
          // A paused decoder may report position before duration. Neither
          // loading nor a pause signal alone establishes a playback position.
          player.positionUpdate.emit(0);
          player.updateDuration.emit(650000);
          (fixture.paused ? player.paused : player.playing).emit();
        }, 0);
      },
      seekTo(position) {
        fixture.calls.push(['seek', position]); fixture.position = position;
        const generation = fixture.generation;
        setTimeout(() => {
          if (fixture.generation === generation) player.positionUpdate.emit(position);
        }, 0);
      },
      getPosition(callback) { fixture.calls.push(['position']); callback(fixture.position / 1000); },
      pause() { fixture.paused = true; player.paused.emit(); },
      play() { fixture.paused = false; player.playing.emit(); },
      stop() { fixture.calls.push(['stop']); fixture.generation += 1; fixture.position = 0; },
    };
    window.jmpInfo = {};
    window.NativeShell = { AppHost: { getDeviceProfile: () => ({ DirectPlayProfiles: [],
      TranscodingProfiles: [{ Type: 'Audio', Container: 'ts', Protocol: 'hls', AudioCodec: 'aac' }] }) } };
    window.apiPromise = Promise.resolve({ player });
  })();`;
}

function harnessHtml() {
  return `<!doctype html><meta charset="utf-8"><title>Offline browser checks</title>
<script src="/web/offline-cache.js"></script><script>
(() => {
  const ACCOUNT = '${ACCOUNT_ID}';
  const OTHER_ACCOUNT = '${OTHER_ACCOUNT_ID}';
  const PACKAGE = '${PACKAGE_ID}';
  const CHUNK = 1024 * 1024;
  const bytes = new Uint8Array(CHUNK + 44);
  const view = new DataView(bytes.buffer);
  const ascii = (offset, text) => { for (let index = 0; index < text.length; index += 1) bytes[offset + index] = text.charCodeAt(index); };
  ascii(0, 'RIFF'); view.setUint32(4, bytes.byteLength - 8, true); ascii(8, 'WAVE'); ascii(12, 'fmt ');
  view.setUint32(16, 16, true); view.setUint16(20, 1, true); view.setUint16(22, 1, true);
  view.setUint32(24, 8000, true); view.setUint32(28, 16000, true); view.setUint16(32, 2, true); view.setUint16(34, 16, true);
  ascii(36, 'data'); view.setUint32(40, bytes.byteLength - 44, true);
  for (let offset = 44; offset < bytes.byteLength; offset += 2) {
    view.setInt16(offset, Math.round(Math.sin(((offset - 44) / 2) * Math.PI * 2 * 440 / 8000) * 7000), true);
  }
  const hash = (value) => new PuffinboxOfflineCache.Sha256().update(value).digestHex();
  const assert = (condition, message) => { if (!condition) throw new Error(message); };
  const status = (value, details = {}) => {
    document.body.dataset.status = value;
    document.body.dataset.details = JSON.stringify(details);
  };
  async function serviceWorkerReady() {
    await navigator.serviceWorker.register('/web/offline-sw.js?revision=offline-media-navigation-v2', { scope: '/web/', updateViaCache: 'none' });
    await navigator.serviceWorker.ready;
    if (!navigator.serviceWorker.controller) {
      await Promise.race([new Promise((resolve) => navigator.serviceWorker.addEventListener('controllerchange', resolve, { once: true })), new Promise((resolve) => setTimeout(resolve, 5000))]);
    }
    assert(!!navigator.serviceWorker.controller, 'service worker did not take control of the test page');
  }
  async function waitTransaction(transaction) {
    await new Promise((resolve, reject) => {
      transaction.oncomplete = resolve;
      transaction.onabort = () => reject(transaction.error || new Error('IndexedDB transaction aborted'));
      transaction.onerror = () => reject(transaction.error || new Error('IndexedDB transaction failed'));
    });
  }
  async function run() {
    await serviceWorkerReady();
    const cache = PuffinboxOfflineCache;
    if (new URL(location.href).searchParams.get('phase') === 'observer') {
      const current = await cache.getActiveSession();
      const generation = await cache.setActiveAccount(OTHER_ACCOUNT, current.generation);
      const channel = new BroadcastChannel('puffinbox-offline');
      channel.postMessage({ type: 'account-session', accountId: OTHER_ACCOUNT, generation, sender: 'browser-test-observer' });
      status('observer-done', { generation });
      return;
    }
    if (new URL(location.href).searchParams.get('phase') === 'restore') {
      const current = await cache.getActiveSession();
      const generation = await cache.setActiveAccount(ACCOUNT, current.generation);
      const channel = new BroadcastChannel('puffinbox-offline');
      channel.postMessage({ type: 'account-session', accountId: ACCOUNT, generation, sender: 'browser-test-observer' });
      status('restore-done', { generation });
      return;
    }
    const session = await cache.getActiveSession();
    assert(session?.accountId === ACCOUNT, 'the app did not establish the browser account before transfer');
    const completed = await cache.getPackage(ACCOUNT, PACKAGE);
    assert(completed?.status === 'complete' && completed.sourceSize === bytes.byteLength && completed.sha256 === hash(bytes), 'the user-facing transfer did not save a verified complete package');
    for (let index = 0; index < 2; index += 1) {
      const row = await cache.getChunk(ACCOUNT, PACKAGE, index, session.generation, completed.transferToken);
      const expected = bytes.subarray(index * CHUNK, Math.min(bytes.byteLength, (index + 1) * CHUNK));
      assert(!!row && hash(new Uint8Array(row.bytes)) === hash(expected), 'a resumed package chunk was missing or corrupt');
    }
    assert(await cache.verifyAndComplete(ACCOUNT, PACKAGE, bytes.byteLength, hash(bytes), CHUNK, session.generation, completed.transferToken), 'the complete-file verification failed in IndexedDB');
    assert((await cache.listPackages(ACCOUNT)).length === 1, 'removing a second item removed the user-downloaded package');

    const url = '/web/__offline/' + encodeURIComponent(ACCOUNT) + '/' + encodeURIComponent(completed.urlToken);
    window.offlineTestUrl = url;
    const full = await fetch(url);
    const fullBytes = new Uint8Array(await full.arrayBuffer());
    assert(full.status === 200 && hash(fullBytes) === hash(bytes), 'the service worker did not stream the complete verified file');
    const ranged = await fetch(url, { headers: { Range: 'bytes=1048568-1048600' } });
    assert(ranged.status === 206 && ranged.headers.get('content-range') === 'bytes 1048568-1048600/' + bytes.byteLength, 'the service worker returned an invalid partial range');
    const rangeBytes = new Uint8Array(await ranged.arrayBuffer());
    assert(rangeBytes.every((value, index) => value === bytes[1048568 + index]), 'the service worker returned incorrect range bytes');
    const head = await fetch(url, { method: 'HEAD' });
    assert(head.status === 200 && head.headers.get('content-type') === 'audio/wav' && Number(head.headers.get('content-length')) === bytes.byteLength, 'the service worker HEAD metadata did not match the package');
    const invalidRange = await fetch(url, { headers: { Range: 'bytes=999999999-' } });
    assert(invalidRange.status === 416, 'the service worker did not reject an unsatisfiable range');

    const db = await cache.openDatabase();
    try {
      const transaction = db.transaction(['chunks'], 'readwrite');
      const store = transaction.objectStore('chunks');
      const key = cache.accountPackageKey(ACCOUNT, PACKAGE) + ':0';
      const original = await new Promise((resolve, reject) => {
        const request = store.get(key);
        request.onsuccess = () => resolve(request.result);
        request.onerror = () => reject(request.error);
      });
      const corrupt = new Uint8Array(original.bytes);
      corrupt[0] ^= 0xff;
      store.put({ ...original, bytes: corrupt.buffer });
      await waitTransaction(transaction);
    } finally { db.close(); }
    let corruptRejected = false;
    try { await (await fetch(url, { headers: { Range: 'bytes=0-20' } })).arrayBuffer(); }
    catch (_) { corruptRejected = true; }
    assert(corruptRejected, 'the service worker played a chunk after its bytes were modified');
    const restoreDb = await cache.openDatabase();
    try {
      const transaction = restoreDb.transaction(['chunks'], 'readwrite');
      transaction.objectStore('chunks').put({ key: cache.accountPackageKey(ACCOUNT, PACKAGE) + ':0', packageKey: cache.accountPackageKey(ACCOUNT, PACKAGE), accountId: ACCOUNT, packageId: PACKAGE, index: 0, digest: hash(bytes.subarray(0, CHUNK)), bytes: bytes.slice(0, CHUNK).buffer });
      await waitTransaction(transaction);
    } finally { restoreDb.close(); }

    const removedToken = new URL(location.href).searchParams.get('removed');
    assert(!!removedToken, 'the removed package token was not provided to the service-worker check');
    const otherUrl = '/web/__offline/' + encodeURIComponent(ACCOUNT) + '/' + encodeURIComponent(removedToken);
    assert((await fetch(otherUrl)).status === 404, 'the service worker continued serving a removed package');
    const sessionBeforeSwitch = await cache.getActiveSession();
    const otherGeneration = await cache.setActiveAccount(OTHER_ACCOUNT, sessionBeforeSwitch.generation);
    assert((await fetch(url)).status === 404, 'a different active account could read this browser copy');
    const sessionAfterSwitch = await cache.getActiveSession();
    const restoredGeneration = await cache.setActiveAccount(ACCOUNT, sessionAfterSwitch.generation);
    assert((await fetch(url)).status === 200, 'the original account could not resume access after switching back');
    status('browser-checks-done', { url, generation: restoredGeneration, audioHash: hash(bytes), audioSize: bytes.byteLength, otherGeneration });
  }
  run().catch((error) => { status('failed', { message: String(error && error.stack || error) }); });
})();
</script><body data-status="starting"></body>`;
}

function staleWorkerHtml() {
  return `<!doctype html><meta charset="utf-8"><title>Stale worker migration fixture</title><body data-status="starting"><script>
    (async () => {
      await navigator.serviceWorker.register('/web/offline-sw-stale.js', { scope: '/web/' });
      await navigator.serviceWorker.ready;
      if (!navigator.serviceWorker.controller) {
        await Promise.race([
          new Promise((resolve) => navigator.serviceWorker.addEventListener('controllerchange', resolve, { once: true })),
          new Promise((resolve) => setTimeout(resolve, 5000)),
        ]);
      }
      document.body.dataset.status = navigator.serviceWorker.controller ? 'stale-worker-active' : 'failed';
      document.body.dataset.script = navigator.serviceWorker.controller?.scriptURL || '';
    })().catch((error) => { document.body.dataset.status = 'failed'; document.body.dataset.error = String(error); });
  </script></body>`;
}

function jsonResponse(response, status, value) {
  const body = Buffer.from(JSON.stringify(value));
  response.writeHead(status, { 'Content-Type': 'application/json; charset=utf-8', 'Content-Length': body.length, 'Cache-Control': 'no-store' });
  response.end(body);
}

function serveFile(response, pathname) {
  const absolute = path.resolve(ROOT, `.${pathname}`);
  if (!absolute.startsWith(`${ROOT}${path.sep}`) || !fs.existsSync(absolute) || !fs.statSync(absolute).isFile()) {
    response.writeHead(404); response.end(); return;
  }
  const contentType = ({ '.html': 'text/html; charset=utf-8', '.css': 'text/css; charset=utf-8', '.js': 'application/javascript; charset=utf-8' })[path.extname(absolute)] || 'application/octet-stream';
  const body = fs.readFileSync(absolute);
  response.writeHead(200, { 'Content-Type': contentType, 'Content-Length': body.length, 'Cache-Control': 'no-store' });
  response.end(body);
}

function createTestServer() {
  let packageQueued = false;
  const packageRow = {
    Id: PACKAGE_ID, ItemId: ITEM_ID, Status: 'ready', SourceSize: audioBytes.byteLength,
    Sha256: audioHash, ChunkCount: audioChunks.length, ItemName: 'Browser offline WAV fixture',
    FileName: 'fixture.wav', ItemType: 'Audio', ContentType: 'audio/wav', Container: 'wav',
  };
  const item = { Id: ITEM_ID, Name: 'Browser offline WAV fixture', Type: 'Audio', MediaType: 'Audio', Container: 'wav', RunTimeTicks: 6_500_000_000 };
  const bookItem = { Id: BOOK_ITEM_ID, Name: 'Reader route EPUB fixture', Type: 'EBook', MediaType: 'Book' };
  const photoItem = { Id: PHOTO_ITEM_ID, Name: 'Same-origin photo auth fixture', Type: 'Photo', MediaType: 'Photo' };
  const nativeItem = { Id: NATIVE_ITEM_ID, Name: 'Native HLS resume fixture', Type: 'Audio', MediaType: 'Audio', RunTimeTicks: 6_500_000_000 };
  const user = { Id: ACCOUNT_ID, Name: 'Browser test', Policy: { EnableContentDownloading: true, EnableMediaPlayback: true, EnableRemoteAccess: true, EnableAllFolders: true, IsAdministrator: false, BlockUnratedItems: [] } };
  return http.createServer((request, response) => {
    const url = new URL(request.url, 'http://127.0.0.1');
    requestsSeen.push(`${request.method} ${url.pathname}${url.search}`);
    if (url.pathname === '/web/__offline_test__/stale.html') {
      const body = Buffer.from(staleWorkerHtml());
      response.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8', 'Content-Length': body.length, 'Cache-Control': 'no-store' });
      response.end(body);
      return;
    }
    if (url.pathname === '/web/offline-sw-stale.js') {
      const body = Buffer.from("self.addEventListener('install', event => event.waitUntil(self.skipWaiting())); self.addEventListener('activate', event => event.waitUntil(self.clients.claim()));");
      response.writeHead(200, { 'Content-Type': 'application/javascript; charset=utf-8', 'Content-Length': body.length, 'Cache-Control': 'no-store' });
      response.end(body);
      return;
    }
    if (url.pathname === '/web/__offline_test__/harness.html') {
      const body = Buffer.from(harnessHtml());
      response.writeHead(200, { 'Content-Type': 'text/html; charset=utf-8', 'Content-Length': body.length, 'Cache-Control': 'no-store' });
      response.end(body);
      return;
    }
    if (url.pathname === '/web/') { serveFile(response, '/web/index.html'); return; }
    if (url.pathname.startsWith('/web/')) { serveFile(response, url.pathname); return; }
    if (url.pathname === '/Startup/Configuration') { jsonResponse(response, 200, { IsStartupWizardCompleted: true, ServerName: 'Offline browser test' }); return; }
    if (url.pathname === '/Users/Me') {
      userMeRequestCount += 1;
      const responseUser = /(?:^|;\s*)book-download-disabled=1(?:;|$)/.test(request.headers.cookie || '')
        ? { ...user, Policy: { ...user.Policy, EnableContentDownloading: false } }
        : user;
      const body = Buffer.from(JSON.stringify(responseUser));
      const sendResponse = () => {
        response.writeHead(200, {
          'Content-Type': 'application/json; charset=utf-8', 'Content-Length': body.length,
          'Cache-Control': 'no-store', 'Set-Cookie': 'puffinbox_session=browser-photo-session; Path=/; HttpOnly; SameSite=Strict',
        });
        response.end(body);
      };
      if (/(?:^|;\s*)slow-me=1(?:;|$)/.test(request.headers.cookie || '')) setTimeout(sendResponse, 1200);
      else sendResponse();
      return;
    }
    if (url.pathname === '/Users/Me/MediaAccessToken' && request.method === 'POST') {
      mediaAccessTokenRestoreCount += 1;
      if (/(?:^|;\s*)native-token-denied=1(?:;|$)/.test(request.headers.cookie || '')) {
        const body = Buffer.from(PRIVATE_FAILURE_BODY);
        response.writeHead(403, { 'Content-Type': 'text/plain; charset=utf-8', 'Content-Length': body.length, 'Cache-Control': 'no-store' });
        response.end(body);
        return;
      }
      jsonResponse(response, 200, {
        AccessToken: 'browser-scoped-media-token',
        ExpiresAt: new Date(Date.now() + 60 * 60 * 1000).toISOString(),
      });
      return;
    }
    if (url.pathname === '/System/Info') { jsonResponse(response, 200, { ServerName: 'Offline browser test', Version: 'test', ItemCount: 1 }); return; }
    if (url.pathname === '/UserViews') { jsonResponse(response, 200, { Items: [] }); return; }
    if (url.pathname === '/Library/VirtualFolders' || url.pathname === '/Localization/ParentalRatings') { jsonResponse(response, 200, []); return; }
    if (url.pathname === '/Items' && request.method === 'GET') {
      const items = [item, bookItem, photoItem];
      if (/(?:^|;\s*)native-hls-test=1(?:;|$)/.test(request.headers.cookie || '')) items.push(nativeItem);
      jsonResponse(response, 200, { Items: items, TotalRecordCount: items.length }); return;
    }
    if (url.pathname === `/Items/${NATIVE_ITEM_ID}` && request.method === 'GET') { jsonResponse(response, 200, nativeItem); return; }
    if (url.pathname === `/Items/${NATIVE_ITEM_ID}/UserData` && request.method === 'GET') { jsonResponse(response, 200, nativeUserData); return; }
    if (url.pathname === `/Items/${NATIVE_ITEM_ID}/PlaybackInfo` && request.method === 'POST') {
      let body = '';
      request.on('data', (chunk) => { body += chunk; });
      request.on('end', () => {
        nativePlaybackRequests.push(JSON.parse(body));
        const sessionId = `c4dc3154-42f0-4ce9-8a17-${String(nativePlaybackRequests.length).padStart(12, '0')}`;
        jsonResponse(response, 200, { PlaySessionId: sessionId, MediaSources: [{ Id: NATIVE_ITEM_ID,
          SupportsDirectPlay: false, SupportsDirectStream: false, SupportsTranscoding: true,
          TranscodingUrl: `/Audio/${NATIVE_ITEM_ID}/master.m3u8?fullTimeline=true&PlaySessionId=${sessionId}`,
          TranscodingContainer: 'ts', TranscodingSubProtocol: 'hls',
          MediaStreams: [{ Type: 'Audio', Index: 0, Codec: 'aac', IsDefault: true }],
        }] });
      });
      return;
    }
    if (url.pathname.startsWith(`/Audio/${NATIVE_ITEM_ID}/hls/`)) { response.writeHead(204); response.end(); return; }
    if (request.method === 'POST' && ['/Sessions/Playing', '/Sessions/Playing/Progress', '/Sessions/Playing/Stopped'].includes(url.pathname)) {
      let body = '';
      request.on('data', (chunk) => { body += chunk; });
      request.on('end', () => {
        const payload = JSON.parse(body);
        assert.equal(payload.ItemId, NATIVE_ITEM_ID);
        nativeSessionRequests.push({ path: url.pathname, ...payload });
        nativeUserData = { Played: payload.PlayedToCompletion === true, PlaybackPositionTicks: payload.PositionTicks };
        response.writeHead(204); response.end();
      });
      return;
    }
    if (url.pathname === `/Items/${ITEM_ID}` && request.method === 'GET') { jsonResponse(response, 200, item); return; }
    if (url.pathname === `/Items/${BOOK_ITEM_ID}` && request.method === 'GET') { jsonResponse(response, 200, bookItem); return; }
    if (url.pathname === `/Items/${PHOTO_ITEM_ID}` && request.method === 'GET') { jsonResponse(response, 200, photoItem); return; }
    if (url.pathname === `/Items/${PHOTO_ITEM_ID}/File` && request.method === 'GET') {
      photoFileRequests.push({ cookie: request.headers.cookie || '', search: url.search });
      const body = PHOTO_PNG;
      response.writeHead(200, { 'Content-Type': 'image/png', 'Content-Length': body.length, 'Cache-Control': 'no-store' });
      response.end(body);
      return;
    }
    if (url.pathname === `/Books/${BOOK_ITEM_ID}/Reader` && request.method === 'GET') {
      bookReaderRequests.push({ cookie: request.headers.cookie || '', search: url.search });
      const body = Buffer.from(`<!doctype html><title>Book reader test</title><body data-book-reader="${BOOK_ITEM_ID}"></body>`);
      response.writeHead(200, {
        'Content-Type': 'text/html; charset=utf-8', 'Content-Length': body.length, 'Cache-Control': 'no-store',
        'Set-Cookie': ['book-download-disabled=; Max-Age=0; Path=/; SameSite=Lax', 'book-reader-session=; Max-Age=0; Path=/; SameSite=Lax'],
      });
      response.end(body);
      return;
    }
    if (url.pathname === '/Puffinbox/Offline/Settings') { jsonResponse(response, 200, { UsedBytes: 0, QuotaBytes: 1024 * 1024 * 1024, ReservedBytes: 0, ChunkBytes: CHUNK_SIZE }); return; }
    if (url.pathname === '/Puffinbox/Offline/Packages' && request.method === 'GET') { jsonResponse(response, 200, packageQueued ? [packageRow] : []); return; }
    if (url.pathname === '/Puffinbox/Offline/Packages' && request.method === 'POST') { packageQueued = true; jsonResponse(response, 202, packageRow); return; }
    if (url.pathname === `/Puffinbox/Offline/Packages/${PACKAGE_ID}/Content` && request.method === 'GET') {
      const range = /^bytes=(\d+)-(\d+)$/.exec(request.headers.range || '');
      if (!range) { response.writeHead(416); response.end(); return; }
      const start = Number(range[1]);
      const end = Number(range[2]);
      const index = Math.floor(start / CHUNK_SIZE);
      const key = `${start}-${end}`;
      contentRangeCounts.set(key, (contentRangeCounts.get(key) || 0) + 1);
      let bytes = audioBytes.subarray(start, end + 1);
      let chunkHash = audioChunkHashes[index];
      if (index === 1 && interruptedTail) {
        interruptedTail = false;
        bytes = bytes.subarray(0, Math.min(8, bytes.byteLength));
      } else if (index === 1 && corruptTailDigest) {
        corruptTailDigest = false;
        chunkHash = '0'.repeat(64);
      }
      const body = Buffer.from(bytes);
      response.writeHead(206, {
        'Accept-Ranges': 'bytes', 'Content-Type': 'application/octet-stream', 'Content-Length': body.length,
        'Content-Range': `bytes ${start}-${end}/${audioBytes.byteLength}`, ETag: `"${audioHash}"`, 'X-Chunk-SHA256': chunkHash,
      });
      response.end(body);
      return;
    }
    if (url.pathname === '/Sessions/Logout' && request.method === 'POST') { response.writeHead(204); response.end(); return; }
    response.writeHead(404, { 'Content-Type': 'text/plain; charset=utf-8' });
    response.end('test server route not found');
  });
}

function findChrome() {
  const candidates = [process.env.CHROME_BIN, process.env.CHROME_PATH,
    ...(process.platform === 'win32' ? [
      'C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe',
      'C:\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe',
      'C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe',
    ] : ['/usr/bin/google-chrome', '/usr/bin/chromium', '/usr/bin/chromium-browser', '/usr/bin/microsoft-edge'])].filter(Boolean);
  for (const candidate of candidates) {
    if (fs.existsSync(candidate)) return candidate;
    const lookup = spawnSync(process.platform === 'win32' ? 'where.exe' : 'which', [candidate], { encoding: 'utf8' });
    if (lookup.status === 0 && lookup.stdout.trim()) return lookup.stdout.trim().split(/\r?\n/)[0];
  }
  throw new Error('A Chrome or Chromium executable is required for the deterministic offline browser checks. Set CHROME_BIN to its path.');
}

async function freePort() {
  const server = net.createServer();
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  const port = server.address().port;
  await new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
  return port;
}

class DevTools {
  constructor(socket) {
    this.socket = socket;
    this.nextId = 1;
    this.pending = new Map();
    this.eventHandlers = new Set();
    socket.addEventListener('message', (event) => {
      const message = JSON.parse(event.data);
      if (message.method) {
        for (const handler of this.eventHandlers) handler(message);
      }
      if (!message.id) return;
      const pending = this.pending.get(message.id);
      if (!pending) return;
      clearTimeout(pending.timeout);
      this.pending.delete(message.id);
      if (message.error) pending.reject(new Error(message.error.message));
      else pending.resolve(message.result || {});
    });
    socket.addEventListener('close', () => {
      for (const pending of this.pending.values()) {
        clearTimeout(pending.timeout);
        pending.reject(new Error('Browser connection closed before its command completed.'));
      }
      this.pending.clear();
    });
  }

  send(method, params = {}, sessionId) {
    const id = this.nextId++;
    const message = { id, method, params };
    if (sessionId) message.sessionId = sessionId;
    return new Promise((resolve, reject) => {
      const timeout = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`Browser command timed out after 30 seconds: ${method}`));
      }, 30000);
      this.pending.set(id, { resolve, reject, timeout });
      try { this.socket.send(JSON.stringify(message)); }
      catch (error) { clearTimeout(timeout); this.pending.delete(id); reject(error); }
    });
  }

  watchEvents(sessionId, method) {
    const events = [];
    const handler = (message) => {
      if (message.sessionId === sessionId && message.method === method) events.push(message.params);
    };
    this.eventHandlers.add(handler);
    return { events, stop: () => this.eventHandlers.delete(handler) };
  }

  async evaluate(sessionId, expression) {
    const response = await this.send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true }, sessionId);
    if (response.exceptionDetails) throw new Error(response.exceptionDetails.exception?.description || response.exceptionDetails.text || 'Browser evaluation failed.');
    return response.result?.value;
  }

  async openPage(url, newDocumentScript) {
    const { targetId } = await this.send('Target.createTarget', { url: 'about:blank' });
    const { sessionId } = await this.send('Target.attachToTarget', { targetId, flatten: true });
    await this.send('Page.enable', {}, sessionId);
    await this.send('Runtime.enable', {}, sessionId);
    if (newDocumentScript) await this.send('Page.addScriptToEvaluateOnNewDocument', { source: newDocumentScript }, sessionId);
    await this.send('Page.navigate', { url }, sessionId);
    return { targetId, sessionId };
  }

  async navigate(page, url) {
    await this.send('Page.navigate', { url }, page.sessionId);
  }

  async waitFor(page, expression, predicate, label, timeoutMs = 15000) {
    const deadline = Date.now() + timeoutMs;
    let last;
    while (Date.now() < deadline) {
      last = await this.evaluate(page.sessionId, expression).catch(() => undefined);
      if (predicate(last)) return last;
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    throw new Error(`Timed out waiting for ${label}; last browser value: ${JSON.stringify(last)}`);
  }
}

async function waitUntil(predicate, label, timeoutMs = 5000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
  throw new Error(`Timed out waiting for ${label}.`);
}

async function main() {
  const chromePath = findChrome();
  const server = createTestServer();
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  const serverPort = server.address().port;
  const debugPort = await freePort();
  const profilePath = fs.mkdtempSync(path.join(os.tmpdir(), 'puffinbox-offline-browser-'));
  const chromeArgs = [
    '--headless=new', '--disable-gpu', '--no-first-run', '--no-default-browser-check', '--disable-background-networking',
    '--autoplay-policy=no-user-gesture-required', '--mute-audio', `--remote-debugging-port=${debugPort}`,
    '--remote-allow-origins=*', `--user-data-dir=${profilePath}`,
    ...(process.platform === 'win32' ? [] : ['--no-sandbox']), 'about:blank',
  ];
  const browser = spawn(chromePath, chromeArgs, { stdio: ['ignore', 'ignore', 'pipe'], windowsHide: true });
  let browserOutput = '';
  browser.stderr.on('data', (chunk) => { browserOutput += chunk.toString(); });
  let devtoolsUrl = null;
  for (let attempt = 0; attempt < 150; attempt += 1) {
    if (browser.exitCode != null) throw new Error(`Chrome exited before its debugging endpoint was ready: ${browserOutput}`);
    try {
      const response = await fetch(`http://127.0.0.1:${debugPort}/json/version`, { signal: AbortSignal.timeout(1500) });
      if (response.ok) { devtoolsUrl = (await response.json()).webSocketDebuggerUrl; break; }
    } catch (_) { /* Chrome is still starting. */ }
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  if (!devtoolsUrl) throw new Error(`Chrome did not expose its debugging endpoint: ${browserOutput}`);
  const socket = new WebSocket(devtoolsUrl);
  await once(socket, 'open', { signal: AbortSignal.timeout(15000) });
  const cdp = new DevTools(socket);
  const baseUrl = `http://127.0.0.1:${serverPort}`;

  try {
    const staleWorkerPage = await cdp.openPage(`${baseUrl}/web/__offline_test__/stale.html`);
    await cdp.waitFor(staleWorkerPage, 'document.body.dataset.status', (value) => value === 'stale-worker-active', 'the previous offline worker fixture');
    assert.match(await cdp.evaluate(staleWorkerPage.sessionId, 'document.body.dataset.script'), /offline-sw-stale\.js$/, 'the migration fixture did not start under the stale worker');

    const appPage = await cdp.openPage(`${baseUrl}/web/`);
    await cdp.waitFor(appPage, 'document.querySelectorAll("[data-item-id]").length', (value) => value > 0, 'the signed-in media browser');
    await cdp.waitFor(appPage, 'navigator.serviceWorker.controller?.scriptURL', (value) => value?.includes('revision=offline-media-navigation-v2'), 'the app to replace the stale offline worker');
    await cdp.send('Target.closeTarget', { targetId: staleWorkerPage.targetId });

    const nativePhotoPage = await cdp.openPage(`${baseUrl}/web/`, 'window.jmpInfo = {};');
    await cdp.waitFor(nativePhotoPage, `document.querySelector('[data-item-id="${PHOTO_ITEM_ID}"]')`, Boolean, 'the native-mode photo fixture');
    await cdp.evaluate(nativePhotoPage.sessionId, `document.querySelector('[data-item-id="${PHOTO_ITEM_ID}"]').click(); true`);
    await cdp.waitFor(nativePhotoPage, 'document.querySelector("#photo-view img")', Boolean, 'the native-mode photo preview');
    const decodedPhoto = await cdp.evaluate(nativePhotoPage.sessionId, `(async () => {
      const image = document.querySelector('#photo-view img');
      await image.decode();
      return { width: image.naturalWidth, height: image.naturalHeight, sourceProtocol: new URL(image.currentSrc).protocol };
    })()`);
    assert.deepEqual(decodedPhoto, { width: 1, height: 1, sourceProtocol: 'blob:' }, 'the native-mode photo preview must decode the authenticated PNG response');
    assert.equal(mediaAccessTokenRestoreCount, 1, 'the native-mode test did not obtain a scoped media token');
    assert.equal(photoFileRequests.length, 1, 'the photo preview did not make exactly one file request');
    assert.match(photoFileRequests[0].cookie, /(?:^|;\s*)puffinbox_session=browser-photo-session(?:;|$)/, 'the photo request did not carry the HttpOnly same-origin session cookie');
    assert.equal(photoFileRequests[0].search, '', 'the browser photo request must not put scoped media credentials in its URL');
    await cdp.send('Target.closeTarget', { targetId: nativePhotoPage.targetId });

    const nativeAuthFailurePage = await cdp.openPage(
      `${baseUrl}/web/`,
      'document.cookie = "native-token-denied=1; Path=/; SameSite=Strict"; window.jmpInfo = {};',
    );
    await cdp.waitFor(nativeAuthFailurePage, `document.querySelector('[data-item-id="${ITEM_ID}"]')`, Boolean, 'the native auth failure fixture');
    await cdp.evaluate(nativeAuthFailurePage.sessionId, `document.querySelector('[data-item-id="${ITEM_ID}"]').click(); true`);
    await cdp.waitFor(nativeAuthFailurePage, 'document.querySelector("#item-dialog")?.open', Boolean, 'the playable item details');
    await cdp.evaluate(nativeAuthFailurePage.sessionId, `Array.from(document.querySelectorAll("#item-dialog button")).find((button) => button.textContent.trim() === "Play").click(); true`);
    const authorizationFailure = await cdp.waitFor(
      nativeAuthFailurePage,
      'document.querySelector("#player-note")?.textContent',
      (value) => value.includes('HTTP 403'),
      'the native media authorization status diagnostic',
    );
    assert.equal(
      authorizationFailure,
      'Native playback authorization is unavailable (HTTP 403). Reopen the app while connected and try again.',
      'the native error should expose only the HTTP status and safe recovery hint'
    );
    assert.equal(authorizationFailure.includes(PRIVATE_FAILURE_BODY), false, 'the native error exposed the endpoint response body');
    assert.equal(mediaAccessTokenRestoreCount, 3, 'the native failure test did not exercise startup and playback token exchanges');
    await cdp.send('Target.closeTarget', { targetId: nativeAuthFailurePage.targetId });

    const nativePlaybackPage = await cdp.openPage(`${baseUrl}/web/`, nativePlayerFixture());
    await cdp.waitFor(nativePlaybackPage, `document.querySelector('[data-item-id="${NATIVE_ITEM_ID}"]')`, Boolean, 'the native HLS fixture');
    await cdp.evaluate(nativePlaybackPage.sessionId, `document.querySelector('[data-item-id="${NATIVE_ITEM_ID}"]').click(); true`);
    await cdp.waitFor(nativePlaybackPage, 'document.querySelector("#item-dialog")?.open', Boolean, 'the native HLS details');
    await cdp.evaluate(nativePlaybackPage.sessionId, `Array.from(document.querySelectorAll('#item-dialog button')).find((button) => button.textContent.trim() === 'Play').click(); true`);
    await cdp.waitFor(nativePlaybackPage, '!document.querySelector("#player-resume-controls").hidden', Boolean, 'the native resume choice');
    await cdp.evaluate(nativePlaybackPage.sessionId, 'document.querySelector("#player-resume").click(); true');
    await cdp.waitFor(nativePlaybackPage, 'document.querySelector("#native-position")?.textContent', (value) => value === '2:40 / 10:50', 'the native source resume time');
    await waitUntil(() => nativeSessionRequests.some((row) => row.path === '/Sessions/Playing'), 'the resumed native session');
    assert.equal(nativeSessionRequests[0].PositionTicks, 1_600_000_000, 'startup at zero overwrote the saved resume position');
    assert.equal(nativePlaybackRequests.at(-1).StartTimeTicks, 1_600_000_000);
    assert.equal(await cdp.evaluate(nativePlaybackPage.sessionId, "nativePlaybackFixture.calls.find(([name]) => name === 'load')[2].startMilliseconds"), 0);

    await cdp.evaluate(nativePlaybackPage.sessionId, `(() => {
      const fixture = nativePlaybackFixture;
      fixture.position = 220000; fixture.player.positionUpdate.emit(220000);
      document.querySelector('#native-play-pause').click();
      const seek = document.querySelector('#native-seek'); seek.value = '0'; seek.dispatchEvent(new Event('change'));
      return true;
    })()`);
    await cdp.waitFor(nativePlaybackPage, 'document.querySelector("#native-position")?.textContent', (value) => value === '0:00 / 10:50', 'the paused stream reopening at zero');
    await waitUntil(() => nativeSessionRequests.filter((row) => row.path === '/Sessions/Playing').length === 2, 'the decoded paused position to establish its session');
    const stopped = () => nativeSessionRequests.filter((row) => row.path === '/Sessions/Playing/Stopped');
    assert.equal(stopped()[0].PositionTicks, 2_200_000_000, 'the replaced session lost its final source position');
    assert.equal(stopped()[0].PlayedToCompletion, false);
    assert.equal(nativeSessionRequests.filter((row) => row.path === '/Sessions/Playing')[1].PositionTicks, 0,
      'a paused decoded origin did not replace the previous resume position');
    assert.equal(await cdp.evaluate(nativePlaybackPage.sessionId, "nativePlaybackFixture.calls.filter(([name]) => name === 'load').at(-1)[2].autoplay"), false,
      'a backward seek resumed playback that the user had paused');

    const seekNative = async (position) => cdp.evaluate(nativePlaybackPage.sessionId, `(() => {
      const seek = document.querySelector('#native-seek'); seek.value = '${position}'; seek.dispatchEvent(new Event('change')); return true;
    })()`);
    await seekNative(120000);
    await cdp.waitFor(nativePlaybackPage, 'document.querySelector("#native-position")?.textContent', (value) => value === '2:00 / 10:50', 'the native forward seek');
    await seekNative(100000);
    await cdp.waitFor(nativePlaybackPage, 'document.querySelector("#native-position")?.textContent', (value) => value === '1:40 / 10:50', 'the native nonzero backward seek');
    await waitUntil(() => nativeSessionRequests.filter((row) => row.path === '/Sessions/Playing').length === 3, 'the nonzero paused native session');
    assert.equal(nativePlaybackRequests.at(-1).StartTimeTicks, 1_000_000_000);
    assert.equal(await cdp.evaluate(nativePlaybackPage.sessionId, "nativePlaybackFixture.calls.filter(([name]) => name === 'load').length"), 3,
      'backward seeks must reopen the native HLS stream; forward seeks use the existing player');

    // A decoder can report finished after a failed seek and then return zero
    // from getPosition. Preserve the last decoded position and completion flag.
    await cdp.evaluate(nativePlaybackPage.sessionId, 'nativePlaybackFixture.position = 0; nativePlaybackFixture.player.finished.emit(); true');
    await waitUntil(() => stopped().length === 3, 'the early native finish report');
    assert.equal(stopped().at(-1).PositionTicks, 1_000_000_000);
    assert.equal(stopped().at(-1).PlayedToCompletion, false, 'an early native exit marked the item completed');
    assert.equal(nativeUserData.Played, false);
    await cdp.waitFor(nativePlaybackPage, 'document.querySelector("#player-note")?.textContent', (value) => value.includes('Playback ended early.'), 'the early finish recovery message');
    await cdp.evaluate(nativePlaybackPage.sessionId, 'document.cookie = "native-hls-test=; Max-Age=0; Path=/"; true');
    await cdp.send('Target.closeTarget', { targetId: nativePlaybackPage.targetId });

    const readerPolicyPage = await cdp.openPage(`${baseUrl}/web/`);
    await cdp.waitFor(readerPolicyPage, 'document.querySelectorAll("[data-item-id]").length', (value) => value > 0, 'the second signed-in media browser');
    await cdp.evaluate(readerPolicyPage.sessionId, 'document.cookie = "book-download-disabled=1; path=/; SameSite=Lax"; true');
    await cdp.navigate(readerPolicyPage, `${baseUrl}/web/`);
    await cdp.waitFor(readerPolicyPage, `document.querySelector('[data-item-id="${BOOK_ITEM_ID}"]')`, Boolean, 'the indexed EPUB fixture');
    await cdp.evaluate(readerPolicyPage.sessionId, `document.querySelector('[data-item-id="${BOOK_ITEM_ID}"]').click(); true`);
    await cdp.waitFor(readerPolicyPage, 'document.querySelector("#item-dialog")?.open', Boolean, 'the book details dialog');
    const readerActions = await cdp.evaluate(readerPolicyPage.sessionId, `(() => {
      const read = document.querySelector('#read-book');
      const download = document.querySelector('#download-book');
      return {
        readHref: read?.getAttribute('href') || null,
        readOrigin: read ? new URL(read.href, location.href).origin : null,
        readTarget: read?.target ?? null,
        downloadHref: download?.getAttribute('href') || null,
      };
    })()`);
    assert.equal(readerActions.readHref, `/Books/${BOOK_ITEM_ID}/Reader`, 'the EBook details dialog did not expose the reader route');
    assert.equal(readerActions.readOrigin, new URL(baseUrl).origin, 'the Read action left the same-origin session');
    assert.equal(readerActions.readTarget, '', 'the Read action did not stay in the current same-origin tab');
    assert.equal(readerActions.downloadHref, null, 'the details dialog exposed a download action when downloads were disabled');
    await cdp.evaluate(readerPolicyPage.sessionId, 'document.cookie = "book-reader-session=present; path=/; SameSite=Lax"; document.querySelector("#read-book").click(); true');
    await cdp.waitFor(readerPolicyPage, 'document.body.dataset.bookReader', (value) => value === BOOK_ITEM_ID, 'the book reader route');
    assert.equal(bookReaderRequests.length, 1, 'the Read action did not request the reader route');
    assert.match(bookReaderRequests[0].cookie, /(?:^|;\s*)book-reader-session=present(?:;|$)/, 'the reader request did not carry the same-origin session cookie');
    assert.equal(bookReaderRequests[0].search, '', 'the reader route did not need a token in its URL');
    assert.equal(await cdp.evaluate(readerPolicyPage.sessionId, 'document.cookie.includes("book-download-disabled=") || document.cookie.includes("book-reader-session=")'), false, 'the reader test cookies were not cleared');
    await cdp.send('Target.closeTarget', { targetId: readerPolicyPage.targetId });

    await cdp.evaluate(appPage.sessionId, 'document.querySelector("[data-item-id]").click(); true');
    await cdp.waitFor(appPage, 'document.querySelector("#item-dialog")?.open', Boolean, 'the media details dialog');
    await cdp.evaluate(appPage.sessionId, 'Array.from(document.querySelectorAll("#item-dialog button")).find((button) => button.textContent.trim() === "Prepare offline").click(); true');
    try {
      await cdp.waitFor(appPage, `document.querySelector('[data-download-offline="${PACKAGE_ID}"]')?.textContent.trim()`, (value) => value === 'Save in this browser', 'the queued server package');
    } catch (error) {
      const details = await cdp.evaluate(appPage.sessionId, 'document.querySelector("#app")?.innerText').catch(() => 'unavailable');
      throw new Error(`${error.message}\nApp text:\n${details}\nRequests:\n${requestsSeen.join('\n')}`);
    }

    await cdp.evaluate(appPage.sessionId, `document.querySelector('[data-download-offline="${PACKAGE_ID}"]').click(); true`);
    await cdp.waitFor(appPage, `document.querySelector('[data-download-offline="${PACKAGE_ID}"]')?.textContent.trim()`, (value) => value === 'Resume in this browser', 'the interrupted first transfer');
    assert.equal(contentRangeCounts.get(`${CHUNK_SIZE}-${audioBytes.byteLength - 1}`), 1, 'the first download attempt did not stop at the injected network interruption');
    const pausedPackage = await cdp.waitFor(appPage,
      `PuffinboxOfflineCache.getPackage('${ACCOUNT_ID}', '${PACKAGE_ID}').then((row) => row.errorCode)`,
      (value) => value === 'network-interrupted', 'the interrupted transfer state');
    assert.equal(pausedPackage, 'network-interrupted');

    await cdp.evaluate(appPage.sessionId, `document.querySelector('[data-download-offline="${PACKAGE_ID}"]').click(); true`);
    const badDigestState = await cdp.waitFor(appPage,
      `PuffinboxOfflineCache.getPackage('${ACCOUNT_ID}', '${PACKAGE_ID}').then((row) => row.errorCode)`,
      (value) => value === 'integrity-failed', 'the rejected bad chunk digest');
    assert.equal(badDigestState, 'integrity-failed', 'a bad per-chunk digest was not reported as an integrity failure');

    await cdp.evaluate(appPage.sessionId, `
      window.__offlineOriginalPutChunk = PuffinboxOfflineCache.putChunk;
      PuffinboxOfflineCache.putChunk = async function(account, packageId, index, ...rest) {
        if (String(packageId) === '${PACKAGE_ID}' && Number(index) === 1) throw new DOMException('The storage quota has been exceeded.', 'QuotaExceededError');
        return window.__offlineOriginalPutChunk.call(this, account, packageId, index, ...rest);
      };
    `);
    await cdp.evaluate(appPage.sessionId, `document.querySelector('[data-download-offline="${PACKAGE_ID}"]').click(); true`);
    const quotaState = await cdp.waitFor(appPage,
      `PuffinboxOfflineCache.getPackage('${ACCOUNT_ID}', '${PACKAGE_ID}').then((row) => row.errorCode)`,
      (value) => value === 'storage-quota', 'the injected browser quota exception');
    assert.equal(quotaState, 'storage-quota', 'a quota exception was mislabeled as a network interruption');
    await cdp.waitFor(appPage, 'document.querySelector("#toast")?.textContent.includes("Browser storage for this site is full")', Boolean, 'the user-facing storage quota message');
    await cdp.evaluate(appPage.sessionId, 'PuffinboxOfflineCache.putChunk = window.__offlineOriginalPutChunk; delete window.__offlineOriginalPutChunk; true');

    await cdp.evaluate(appPage.sessionId, `document.querySelector('[data-download-offline="${PACKAGE_ID}"]').click(); true`);
    await cdp.waitFor(appPage, `document.querySelector('[data-open-offline="${PACKAGE_ID}"]')?.getAttribute('href')`, Boolean, 'the complete local package');
    assert.equal(contentRangeCounts.get(`0-${CHUNK_SIZE - 1}`), 1, 'resuming downloaded the already verified first chunk again');
    assert.equal(contentRangeCounts.get(`${CHUNK_SIZE}-${audioBytes.byteLength - 1}`), 4, 'the tail chunk was not retried after interruption, digest, and simulated quota failures');

    const offlineHref = await cdp.evaluate(appPage.sessionId, `document.querySelector('[data-open-offline="${PACKAGE_ID}"]')?.getAttribute('href')`);
    assert.ok(offlineHref, 'the verified package did not expose its offline playback link');
    assert.equal(await cdp.evaluate(appPage.sessionId, `document.querySelector('[data-open-offline="${PACKAGE_ID}"]').target`), '_blank', 'standard browser offline links should continue opening a new tab');
    const nativeOfflinePage = await cdp.openPage(`${baseUrl}/web/`, 'window.jmpInfo = {}; Object.defineProperty(navigator, "serviceWorker", { configurable: true, value: undefined });');
    await cdp.waitFor(nativeOfflinePage, 'document.querySelectorAll("[data-item-id]").length', (value) => value > 0, 'the embedded-player media browser');
    await cdp.evaluate(nativeOfflinePage.sessionId, 'document.querySelector("[data-screen=offline]").click(); true');
    await cdp.waitFor(nativeOfflinePage, `document.querySelector('[data-open-offline="${PACKAGE_ID}"]')`, Boolean, 'the embedded-player offline package');
    assert.equal(await cdp.evaluate(nativeOfflinePage.sessionId, `document.querySelector('[data-open-offline="${PACKAGE_ID}"]').target`), '_self', 'the embedded-player offline link must remain in its browser context');
    assert.equal(await cdp.evaluate(nativeOfflinePage.sessionId, 'typeof navigator.serviceWorker'), 'undefined', 'the embedded-player fixture must model a webview without the service-worker API');
    const offlineRoutePath = new URL(offlineHref, baseUrl).pathname;
    const offlineRouteRequestsBefore = requestsSeen.filter((request) => request.endsWith(` ${offlineRoutePath}`)).length;
    await cdp.evaluate(nativeOfflinePage.sessionId, `document.querySelector('[data-open-offline="${PACKAGE_ID}"]').click(); true`);
    await cdp.waitFor(nativeOfflinePage, 'document.querySelector("#player-dialog")?.open && document.querySelector("#player-stage audio")?.readyState >= 2', Boolean, 'the embedded-player Blob fallback');
    const nativeBlobPlayback = await cdp.evaluate(nativeOfflinePage.sessionId, `(async () => {
      const audio = document.querySelector('#player-stage audio');
      await audio.play();
      await new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error('the embedded-player Blob copy did not play')), 8000);
        const check = () => { if (audio.currentTime > 0.05) { clearTimeout(timer); audio.removeEventListener('timeupdate', check); resolve(); } };
        audio.addEventListener('timeupdate', check);
        check();
      });
      audio.pause();
      return { source: audio.src, currentTime: audio.currentTime };
    })()`);
    assert.match(nativeBlobPlayback.source, /^blob:/, 'the embedded player did not use the verified local Blob source');
    assert.ok(nativeBlobPlayback.currentTime > 0.05, 'the embedded-player copy did not advance playback');
    await cdp.evaluate(nativeOfflinePage.sessionId, 'document.querySelector("#player-dialog").close(); true');
    await cdp.waitFor(nativeOfflinePage, '!document.querySelector("#player-dialog").open', Boolean, 'closing the embedded-player preview');
    await cdp.evaluate(nativeOfflinePage.sessionId, `(async () => {
      const cache = PuffinboxOfflineCache;
      const db = await cache.openDatabase();
      try {
        const transaction = db.transaction('chunks', 'readwrite');
        const completed = new Promise((resolve, reject) => {
          transaction.oncomplete = resolve;
          transaction.onabort = () => reject(transaction.error || new Error('chunk transaction aborted'));
          transaction.onerror = () => reject(transaction.error || new Error('chunk transaction failed'));
        });
        const store = transaction.objectStore('chunks');
        const key = cache.accountPackageKey('${ACCOUNT_ID}', '${PACKAGE_ID}') + ':0';
        const request = store.get(key);
        request.onerror = () => { try { transaction.abort(); } catch (_) {} };
        request.onsuccess = () => {
          const original = new Uint8Array(request.result.bytes);
          window.__offlineOriginalChunk = original.slice().buffer;
          original[0] ^= 0xff;
          store.put({ ...request.result, bytes: original.buffer });
        };
        await completed;
      } finally { db.close(); }
    })()`);
    await cdp.evaluate(nativeOfflinePage.sessionId, `document.querySelector('[data-open-offline="${PACKAGE_ID}"]').click(); true`);
    const rejectedCorruptBlob = await cdp.waitFor(nativeOfflinePage,
      'document.querySelector("#player-note")?.textContent',
      (value) => value.includes('failed its local SHA-256 check'), 'rejecting a corrupt embedded-player chunk');
    assert.match(rejectedCorruptBlob, /chunk 1 failed its local SHA-256 check/);
    await cdp.evaluate(nativeOfflinePage.sessionId, `(async () => {
      const cache = PuffinboxOfflineCache;
      const db = await cache.openDatabase();
      try {
        const transaction = db.transaction('chunks', 'readwrite');
        const completed = new Promise((resolve, reject) => {
          transaction.oncomplete = resolve;
          transaction.onabort = () => reject(transaction.error || new Error('chunk restore aborted'));
          transaction.onerror = () => reject(transaction.error || new Error('chunk restore failed'));
        });
        const store = transaction.objectStore('chunks');
        const key = cache.accountPackageKey('${ACCOUNT_ID}', '${PACKAGE_ID}') + ':0';
        const request = store.get(key);
        request.onerror = () => { try { transaction.abort(); } catch (_) {} };
        request.onsuccess = () => store.put({ ...request.result, bytes: window.__offlineOriginalChunk });
        await completed;
      } finally { db.close(); }
      delete window.__offlineOriginalChunk;
    })()`);
    assert.equal(requestsSeen.filter((request) => request.endsWith(` ${offlineRoutePath}`)).length, offlineRouteRequestsBefore, 'the no-service-worker playback fallback requested the server-only virtual route');
    await cdp.send('Target.closeTarget', { targetId: nativeOfflinePage.targetId });

    const navigationPage = await cdp.openPage('about:blank');
    await cdp.send('Network.enable', {}, navigationPage.sessionId);
    const navigationResponses = cdp.watchEvents(navigationPage.sessionId, 'Network.responseReceived');
    await cdp.send('Network.emulateNetworkConditions', { offline: true, latency: 0, downloadThroughput: 0, uploadThroughput: 0, connectionType: 'none' }, navigationPage.sessionId);
    await cdp.navigate(navigationPage, `${baseUrl}${offlineHref}`);
    await waitUntil(() => navigationResponses.events.some((event) => event.response.url === `${baseUrl}${offlineHref}`), 'the top-level offline package navigation');
    const navigationResponse = navigationResponses.events.find((event) => event.response.url === `${baseUrl}${offlineHref}`).response;
    assert.equal(navigationResponse.status, 200, 'opening the offline link in a fresh tab did not serve the saved package');
    assert.equal(navigationResponse.fromServiceWorker, true, 'the top-level offline link bypassed the service worker');
    assert.equal(navigationResponse.headers['content-type'], 'audio/wav', 'the offline navigation did not return the media content type');
    navigationResponses.stop();
    await cdp.send('Network.emulateNetworkConditions', { offline: false, latency: 0, downloadThroughput: -1, uploadThroughput: -1, connectionType: 'wifi' }, navigationPage.sessionId);
    await cdp.send('Target.closeTarget', { targetId: navigationPage.targetId });

    await cdp.evaluate(appPage.sessionId, `document.querySelector('[data-screen="offline"]').click(); true`);
    await cdp.waitFor(appPage, 'document.querySelectorAll("[data-remove-local]").length', (value) => value === 1, 'the offline package list');
    await cdp.evaluate(appPage.sessionId, `(async () => {
      const cache = PuffinboxOfflineCache;
      const session = await cache.getActiveSession();
      const bytes = new Uint8Array([11, 22, 33, 44, 55]);
      const digest = new cache.Sha256().update(bytes).digestHex();
      const values = { itemName: 'Removable browser copy', fileName: 'secondary.bin', itemType: 'Video', contentType: 'application/octet-stream', sourceSize: bytes.length, sha256: digest, chunkSize: ${CHUNK_SIZE}, chunkCount: 1, status: 'downloading' };
      const token = await cache.startPackageTransfer('${ACCOUNT_ID}', '${SECOND_PACKAGE_ID}', values, session.generation, null);
      await cache.putChunk('${ACCOUNT_ID}', '${SECOND_PACKAGE_ID}', 0, bytes, digest, session.generation, token);
      try { await cache.verifyAndComplete('${ACCOUNT_ID}', '${SECOND_PACKAGE_ID}', bytes.length, '0'.repeat(64), ${CHUNK_SIZE}, session.generation, token); throw new Error('bad whole-file digest was accepted'); }
      catch (error) { if (!/complete SHA-256/i.test(error.message)) throw error; }
      await cache.verifyAndComplete('${ACCOUNT_ID}', '${SECOND_PACKAGE_ID}', bytes.length, digest, ${CHUNK_SIZE}, session.generation, token);
      const row = await cache.getPackage('${ACCOUNT_ID}', '${SECOND_PACKAGE_ID}');
      await cache.updatePackageTransfer('${ACCOUNT_ID}', '${SECOND_PACKAGE_ID}', { status: 'complete', bytesCopied: bytes.length }, session.generation, row.transferToken);
      return true;
    })()`);
    await cdp.evaluate(appPage.sessionId, 'document.querySelector("#refresh-offline").click(); true');
    await cdp.waitFor(appPage, 'document.querySelectorAll("[data-remove-local]").length', (value) => value === 2, 'the second cached package');
    const removedToken = await cdp.evaluate(appPage.sessionId, `PuffinboxOfflineCache.getPackage('${ACCOUNT_ID}', '${SECOND_PACKAGE_ID}').then((row) => row.urlToken)`);
    await cdp.evaluate(appPage.sessionId, `window.confirm = () => true; document.querySelector('[data-remove-local="${SECOND_PACKAGE_ID}"]').click(); true`);
    await cdp.waitFor(appPage, 'document.querySelectorAll("[data-remove-local]").length', (value) => value === 1, 'local package removal');
    const packageCount = await cdp.evaluate(appPage.sessionId, `PuffinboxOfflineCache.listPackages('${ACCOUNT_ID}').then((rows) => rows.length)`);
    assert.equal(packageCount, 1, 'removing one package did not preserve the other local package');

    await cdp.send('Network.enable', {}, appPage.sessionId);
    await cdp.send('Network.emulateNetworkConditions', { offline: true, latency: 0, downloadThroughput: 0, uploadThroughput: 0, connectionType: 'none' }, appPage.sessionId);
    const audioResult = await cdp.evaluate(appPage.sessionId, `(async () => {
      const link = document.querySelector('[data-open-offline="${PACKAGE_ID}"]');
      if (!link) throw new Error('the verified package has no user-facing Open offline link');
      const audio = new Audio();
      audio.preload = 'auto';
      const loaded = new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error('offline audio did not become playable')), 8000);
        audio.addEventListener('canplay', () => { clearTimeout(timer); resolve(); }, { once: true });
        audio.addEventListener('error', () => { clearTimeout(timer); reject(new Error('the offline audio source could not be decoded')); }, { once: true });
      });
      audio.src = link.href;
      audio.load();
      await loaded;
      await audio.play();
      const started = await new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error('offline audio playback did not advance')), 8000);
        const check = () => { if (audio.currentTime > 0.05) { clearTimeout(timer); audio.removeEventListener('timeupdate', check); resolve(true); } };
        audio.addEventListener('timeupdate', check);
        check();
      });
      audio.pause();
      audio.removeAttribute('src');
      audio.load();
      return { started, currentTime: audio.currentTime, source: link.href };
    })()`);
    assert.equal(audioResult.started, true, 'the browser media element did not play the cached WAV');
    await cdp.send('Network.emulateNetworkConditions', { offline: false, latency: 0, downloadThroughput: -1, uploadThroughput: -1, connectionType: 'wifi' }, appPage.sessionId);

    const offlinePage = await cdp.openPage(`${baseUrl}/web/__offline_test__/harness.html?phase=verify&removed=${encodeURIComponent(removedToken)}`);
    await cdp.waitFor(offlinePage, 'document.body.dataset.status', (value) => value === 'browser-checks-done', 'real IndexedDB and service-worker checks');
    const browserReport = await cdp.evaluate(offlinePage.sessionId, 'JSON.parse(document.body.dataset.details)');
    assert.equal(browserReport.audioSize, audioBytes.byteLength);
    assert.equal(browserReport.audioHash, audioHash);

    const observer = offlinePage;
    await cdp.navigate(observer, `${baseUrl}/web/__offline_test__/harness.html?phase=observer`);
    await cdp.waitFor(observer, 'document.body.dataset.status', (value) => value === 'observer-done', 'the second browser tab account change');
    await cdp.waitFor(appPage, 'document.querySelector("#auth-form") !== null', Boolean, 'the stale tab to return to sign-in after the other tab changed accounts');
    assert.equal(await cdp.evaluate(appPage.sessionId, `fetch('${browserReport.url}').then((response) => response.status)`), 404, 'the service worker exposed a package while another account was active');
    await cdp.navigate(observer, `${baseUrl}/web/__offline_test__/harness.html?phase=restore`);
    await cdp.waitFor(observer, 'document.body.dataset.status', (value) => value === 'restore-done', 'restoring the original browser account');

    const priorMeRequests = userMeRequestCount;
    await cdp.evaluate(appPage.sessionId, 'document.cookie = "slow-me=1; path=/"; true');
    await cdp.navigate(appPage, `${baseUrl}/web/`);
    await waitUntil(() => userMeRequestCount > priorMeRequests, 'the delayed account lookup');
    await cdp.navigate(observer, `${baseUrl}/web/__offline_test__/harness.html?phase=observer`);
    await cdp.waitFor(observer, 'document.body.dataset.status', (value) => value === 'observer-done', 'an account change during the delayed startup lookup');
    await cdp.waitFor(appPage, 'document.querySelector("#auth-form") !== null', Boolean, 'the delayed startup to reject the stale account');
    assert.equal(await cdp.evaluate(appPage.sessionId, '!!document.querySelector("#user-menu")'), false, 'a delayed API response restored the stale account after another tab switched users');
    await cdp.navigate(observer, `${baseUrl}/web/__offline_test__/harness.html?phase=restore`);
    await cdp.waitFor(observer, 'document.body.dataset.status', (value) => value === 'restore-done', 'restoring the original account after the delayed startup check');
    await cdp.evaluate(appPage.sessionId, 'document.cookie = "slow-me=; Max-Age=0; path=/"; true');
    await cdp.navigate(appPage, `${baseUrl}/web/`);
    await cdp.waitFor(appPage, 'document.querySelector("[data-screen=offline]") !== null', Boolean, 'the reconnected offline library');
    await cdp.evaluate(appPage.sessionId, `
      const cache = PuffinboxOfflineCache;
      const active = window.localStorage.getItem('puffinbox-local-signed-out');
      void active;
      cache.forgetAccount = async () => { throw new Error('injected local cleanup failure'); };
      window.confirm = () => true;
      document.querySelector('#user-menu').click();
      true
    `);
    await cdp.waitFor(appPage, 'document.querySelector("#auth-form") !== null', Boolean, 'local sign-out');
    assert.equal(
      await cdp.evaluate(appPage.sessionId, `fetch(${JSON.stringify(offlineHref)}).then((response) => response.status)`),
      404,
      'the service worker served a local media copy after account sign-out cleanup failed',
    );
    await cdp.send('Network.enable', {}, appPage.sessionId);
    await cdp.send('Network.emulateNetworkConditions', { offline: true, latency: 0, downloadThroughput: 0, uploadThroughput: 0, connectionType: 'none' }, appPage.sessionId);
    await cdp.navigate(appPage, `${baseUrl}/web/`);
    await cdp.waitFor(appPage, 'document.querySelector("#auth-form") !== null', Boolean, 'offline startup honoring the saved sign-out marker');
    assert.equal(await cdp.evaluate(appPage.sessionId, '!!document.querySelector(".offline-fallback")'), false, 'offline startup displayed cached media after local sign-out');

    const ranges = Object.fromEntries(contentRangeCounts);
    assert.deepEqual(ranges, { [`0-${CHUNK_SIZE - 1}`]: 1, [`${CHUNK_SIZE}-${audioBytes.byteLength - 1}`]: 4 });
    process.stdout.write('Browser checks passed: native HLS deferred resume, paused backward stream reopening, decoded paused progress and early-finish preservation; stale-worker upgrade, JMP offline Blob playback without service workers with chunk-integrity rejection, standard-browser new-tab behavior, native-mode photo cookie auth with PNG decoding and no URL credentials, status-only native authorization errors, same-origin EBook reader action and download-policy gating, user-facing offline queue, interrupted resume, chunk and full-file SHA-256, quota messaging, IndexedDB removal, cross-tab account isolation, service-worker ranges and integrity, fresh-tab offline media navigation, offline WAV playback, and sign-out fallback.\n');
  } catch (error) {
    const diagnostics = browserOutput.trim().slice(-3000);
    throw diagnostics ? new Error(`${error.message}\nChrome output:\n${diagnostics}`, { cause: error }) : error;
  } finally {
    socket.close();
    browser.kill();
    await Promise.race([once(browser, 'exit').catch(() => {}), new Promise((resolve) => setTimeout(resolve, 3000))]);
    const closed = new Promise((resolve) => server.close(() => resolve()));
    server.closeAllConnections();
    await closed;
    for (let attempt = 0; attempt < 10; attempt += 1) {
      try { fs.rmSync(profilePath, { recursive: true, force: true }); break; }
      catch (error) {
        if (error.code !== 'EPERM' || attempt === 9) break;
        await new Promise((resolve) => setTimeout(resolve, 150));
      }
    }
  }
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
