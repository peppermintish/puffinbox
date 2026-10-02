#!/usr/bin/env node
'use strict';

const assert = require('node:assert/strict');
const { randomUUID } = require('node:crypto');
const { spawn, spawnSync } = require('node:child_process');
const { once } = require('node:events');
const fs = require('node:fs');
const http = require('node:http');
const net = require('node:net');
const os = require('node:os');
const path = require('node:path');

const ROOT = path.resolve(__dirname, '..');
const BODY_DELAY_MS = Number(process.env.PUFFINBOX_BROWSER_BODY_DELAY_MS || 0);
assert.ok(Number.isInteger(BODY_DELAY_MS) && BODY_DELAY_MS >= 0 && BODY_DELAY_MS <= 1000,
  'synthetic response body delay must be between 0 and 1000 milliseconds');
const USER_ID = '7c9e6679-7425-40de-944b-e07fc1f90ae7';
const AUDIO_ITEMS = [
  { Id: 'b6d9a7e1-c3c9-43cb-9e55-2dd19619fd01', Name: 'Track One', Type: 'Audio', MediaType: 'Audio', Container: 'wav' },
  { Id: 'b6d9a7e1-c3c9-43cb-9e55-2dd19619fd02', Name: 'Track Two', Type: 'Audio', MediaType: 'Audio', Container: 'wav' },
  { Id: 'b6d9a7e1-c3c9-43cb-9e55-2dd19619fd03', Name: 'Track Three', Type: 'Audio', MediaType: 'Audio', Container: 'wav' },
];

function makeWave() {
  const seconds = 8;
  const sampleRate = 8000;
  const bytes = Buffer.alloc(44 + seconds * sampleRate * 2);
  bytes.write('RIFF', 0); bytes.writeUInt32LE(bytes.length - 8, 4); bytes.write('WAVE', 8);
  bytes.write('fmt ', 12); bytes.writeUInt32LE(16, 16); bytes.writeUInt16LE(1, 20);
  bytes.writeUInt16LE(1, 22); bytes.writeUInt32LE(sampleRate, 24); bytes.writeUInt32LE(sampleRate * 2, 28);
  bytes.writeUInt16LE(2, 32); bytes.writeUInt16LE(16, 34); bytes.write('data', 36); bytes.writeUInt32LE(bytes.length - 44, 40);
  for (let offset = 44; offset < bytes.length; offset += 2) bytes.writeInt16LE(0, offset);
  return bytes;
}

const wave = makeWave();
const playlists = [];
const entriesByPlaylist = new Map();
const apiRequests = [];
const playbackItems = [];
let failPlaylistItemGetFor = null;
let delayedMutation = null;
let bodyProbeSent = false;

function holdNextMutation(playlistId, method, pathnamePart) {
  let startedResolve;
  let releaseResolve;
  const started = new Promise((resolve) => { startedResolve = resolve; });
  const pending = new Promise((resolve) => { releaseResolve = resolve; });
  delayedMutation = { playlistId, method, pathnamePart, startedResolve, pending };
  return { started, release: releaseResolve };
}

async function delayMutationResponse(playlistId, method, pathname) {
  const gate = delayedMutation;
  if (!gate || gate.playlistId !== playlistId || gate.method !== method || !pathname.includes(gate.pathnamePart)) return;
  delayedMutation = null;
  gate.startedResolve();
  await gate.pending;
}

function jsonResponse(response, status, value, onBodySent) {
  const body = Buffer.from(JSON.stringify(value));
  response.writeHead(status, { 'Content-Type': 'application/json; charset=utf-8', 'Content-Length': body.length, 'Cache-Control': 'no-store' });
  const sendBody = () => { response.end(body); onBodySent?.(); };
  if (BODY_DELAY_MS) {
    response.flushHeaders();
    setTimeout(sendBody, BODY_DELAY_MS);
    return;
  }
  sendBody();
}

function emptyResponse(response, status = 204) {
  response.writeHead(status, { 'Cache-Control': 'no-store' });
  response.end();
}

function readJson(request) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    request.on('data', (chunk) => chunks.push(chunk));
    request.on('end', () => {
      try { resolve(chunks.length ? JSON.parse(Buffer.concat(chunks).toString('utf8')) : {}); }
      catch (error) { reject(error); }
    });
    request.on('error', reject);
  });
}

function serveFile(response, pathname) {
  const absolute = path.resolve(ROOT, `.${decodeURIComponent(pathname)}`);
  if (!absolute.startsWith(`${ROOT}${path.sep}`) || !fs.existsSync(absolute) || !fs.statSync(absolute).isFile()) {
    response.writeHead(404); response.end(); return;
  }
  const contentType = ({ '.html': 'text/html; charset=utf-8', '.css': 'text/css; charset=utf-8', '.js': 'application/javascript; charset=utf-8' })[path.extname(absolute)] || 'application/octet-stream';
  const body = fs.readFileSync(absolute);
  response.writeHead(200, { 'Content-Type': contentType, 'Content-Length': body.length, 'Cache-Control': 'no-store' });
  response.end(body);
}

function createTestServer() {
  const user = { Id: USER_ID, Name: 'Playlist browser test', IsAdministrator: true, Policy: { EnableContentDownloading: true, EnableMediaPlayback: true, EnableRemoteAccess: true, EnableAllFolders: true, IsAdministrator: true, BlockUnratedItems: [] } };
  return http.createServer(async (request, response) => {
    const url = new URL(request.url, 'http://127.0.0.1');
    apiRequests.push({ method: request.method, pathname: url.pathname, search: url.search });
    if (url.pathname === '/web/') { serveFile(response, '/web/index.html'); return; }
    if (url.pathname.startsWith('/web/')) { serveFile(response, url.pathname); return; }
    if (url.pathname === '/Startup/Configuration') { jsonResponse(response, 200, { IsStartupWizardCompleted: true }); return; }
    if (url.pathname === '/Users/Me') { jsonResponse(response, 200, user); return; }
    if (url.pathname === '/Users' && request.method === 'GET') { jsonResponse(response, 200, [user]); return; }
    if (url.pathname === '/System/Info') { jsonResponse(response, 200, { ServerName: 'Playlist test server' }); return; }
    if (url.pathname === '/UserViews') { jsonResponse(response, 200, { Items: [] }); return; }
    if (url.pathname === '/Library/VirtualFolders' || url.pathname === '/Localization/ParentalRatings') { jsonResponse(response, 200, []); return; }
    if (url.pathname === '/Items' && request.method === 'GET') {
      const audioOnly = String(url.searchParams.get('IncludeItemTypes') || '').split(',').includes('Audio');
      const start = Number(url.searchParams.get('StartIndex') || 0);
      const limit = Math.min(100, Number(url.searchParams.get('Limit') || 100));
      const rows = AUDIO_ITEMS.slice(start, start + limit);
      jsonResponse(response, 200, { Items: audioOnly ? rows : [...rows], TotalRecordCount: AUDIO_ITEMS.length, StartIndex: start });
      return;
    }
    if (url.pathname === '/Playlists' && request.method === 'GET') {
      const start = Number(url.searchParams.get('StartIndex') || 0);
      const limit = Math.min(100, Number(url.searchParams.get('Limit') || 100));
      jsonResponse(response, 200, { Items: playlists.slice(start, start + limit), TotalRecordCount: playlists.length, StartIndex: start },
        url.searchParams.get('BodyProbe') === 'true' ? () => { bodyProbeSent = true; } : undefined);
      return;
    }
    if (url.pathname === '/Playlists' && request.method === 'POST') {
      const body = await readJson(request);
      const playlist = { Id: randomUUID(), Name: String(body.Name || ''), MediaType: 'Audio', Type: 'Playlist' };
      playlists.push(playlist);
      entriesByPlaylist.set(playlist.Id, []);
      jsonResponse(response, 200, { Id: playlist.Id });
      return;
    }
    const move = /^\/Playlists\/([^/]+)\/Items\/([^/]+)\/Move\/(\d+)$/.exec(url.pathname);
    if (move && request.method === 'POST') {
      const entries = entriesByPlaylist.get(move[1]);
      const index = entries?.findIndex((entry) => entry.PlaylistItemId === move[2]) ?? -1;
      const target = Number(move[3]);
      if (index < 0 || target >= entries.length) { response.writeHead(404); response.end(); return; }
      entries.splice(target, 0, ...entries.splice(index, 1));
      await delayMutationResponse(move[1], request.method, url.pathname);
      emptyResponse(response);
      return;
    }
    const playlistItems = /^\/Playlists\/([^/]+)\/Items$/.exec(url.pathname);
    if (playlistItems) {
      const entries = entriesByPlaylist.get(playlistItems[1]);
      if (!entries) { response.writeHead(404); response.end(); return; }
      if (request.method === 'GET') {
        if (failPlaylistItemGetFor === playlistItems[1]) {
          failPlaylistItemGetFor = null;
          jsonResponse(response, 503, { Message: 'Injected playlist read failure' });
          return;
        }
        const start = Number(url.searchParams.get('StartIndex') || 0);
        const limit = Math.min(100, Number(url.searchParams.get('Limit') || 100));
        const items = entries.slice(start, start + limit).map((entry) => ({
          ...AUDIO_ITEMS.find((item) => item.Id === entry.Id), PlaylistItemId: entry.PlaylistItemId,
          PlayAccess: 'Full', UserData: { ItemId: entry.Id },
        }));
        jsonResponse(response, 200, { Items: items, TotalRecordCount: entries.length, StartIndex: start });
        return;
      }
      if (request.method === 'POST') {
        for (const id of url.searchParams.getAll('Ids').flatMap((value) => value.split(','))) {
          if (AUDIO_ITEMS.some((item) => item.Id === id)) entries.push({ Id: id, PlaylistItemId: randomUUID() });
        }
        await delayMutationResponse(playlistItems[1], request.method, url.pathname);
        emptyResponse(response);
        return;
      }
      if (request.method === 'DELETE') {
        const remove = new Set(url.searchParams.getAll('EntryIds').flatMap((value) => value.split(',')));
        entriesByPlaylist.set(playlistItems[1], entries.filter((entry) => !remove.has(entry.PlaylistItemId)));
        await delayMutationResponse(playlistItems[1], request.method, url.pathname);
        emptyResponse(response);
        return;
      }
    }
    const playbackInfo = /^\/Items\/([^/]+)\/PlaybackInfo$/.exec(url.pathname);
    if (playbackInfo && request.method === 'POST') {
      playbackItems.push(playbackInfo[1]);
      jsonResponse(response, 200, {
        PlaySessionId: randomUUID(),
        MediaSources: [{ SupportsDirectPlay: true, DirectStreamUrl: `/Audio/${playbackInfo[1]}/stream`, MediaStreams: [] }],
      });
      return;
    }
    if (/^\/Items\/[^/]+\/UserData$/.test(url.pathname)) { jsonResponse(response, 200, { Played: true, PlaybackPositionTicks: 0 }); return; }
    if (/^\/Audio\/[^/]+\/stream$/.test(url.pathname)) {
      response.writeHead(200, { 'Content-Type': 'audio/wav', 'Content-Length': wave.length, 'Cache-Control': 'no-store' });
      response.end(wave);
      return;
    }
    if (url.pathname === '/Sessions/Playing' || url.pathname === '/Sessions/Playing/Progress' || url.pathname === '/Sessions/Playing/Stopped') { emptyResponse(response); return; }
    response.writeHead(404, { 'Content-Type': 'text/plain; charset=utf-8' }); response.end('test server route not found');
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
  throw new Error('A Chrome or Chromium executable is required for playlist browser checks. Set CHROME_BIN to its path.');
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
    socket.addEventListener('message', (event) => {
      const message = JSON.parse(event.data);
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
  async evaluate(sessionId, expression) {
    const response = await this.send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true }, sessionId);
    if (response.exceptionDetails) throw new Error(response.exceptionDetails.exception?.description || response.exceptionDetails.text || 'Browser evaluation failed.');
    return response.result?.value;
  }
  async openPage(url, script) {
    const { targetId } = await this.send('Target.createTarget', { url: 'about:blank' });
    const { sessionId } = await this.send('Target.attachToTarget', { targetId, flatten: true });
    await this.send('Page.enable', {}, sessionId);
    await this.send('Runtime.enable', {}, sessionId);
    if (script) await this.send('Page.addScriptToEvaluateOnNewDocument', { source: script }, sessionId);
    await this.send('Page.navigate', { url }, sessionId);
    return { targetId, sessionId };
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

async function main() {
  const server = createTestServer();
  let profilePath;
  let chrome;
  let chromeOutput = '';
  let launchError;
  let socket;
  try {
    server.listen(0, '127.0.0.1');
    await once(server, 'listening');
    const serverPort = server.address().port;
    const debugPort = await freePort();
    profilePath = fs.mkdtempSync(path.join(os.tmpdir(), 'puffinbox-playlists-browser-'));
    chrome = spawn(findChrome(), [
      '--headless=new', '--disable-gpu', '--no-first-run', '--no-default-browser-check', '--disable-background-networking',
      '--autoplay-policy=no-user-gesture-required', '--mute-audio', `--remote-debugging-port=${debugPort}`,
      '--remote-allow-origins=*', `--user-data-dir=${profilePath}`,
      ...(process.platform === 'win32' ? [] : ['--no-sandbox']), 'about:blank',
    ], { stdio: ['ignore', 'ignore', 'pipe'], windowsHide: true });
    chrome.stderr.on('data', (chunk) => { chromeOutput += chunk.toString(); });
    chrome.on('error', (error) => { launchError = error; });
    let devtoolsUrl = null;
    for (let attempt = 0; attempt < 150; attempt += 1) {
      if (launchError) throw launchError;
      if (chrome.exitCode != null) throw new Error(`Chrome exited before its debugging endpoint was ready: ${chromeOutput}`);
      try {
        const response = await fetch(`http://127.0.0.1:${debugPort}/json/version`);
        if (response.ok) { devtoolsUrl = (await response.json()).webSocketDebuggerUrl; break; }
      } catch (_) { /* Chrome is still starting. */ }
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    if (!devtoolsUrl) throw new Error(`Chrome did not expose its debugging endpoint: ${chromeOutput}`);
    socket = new WebSocket(devtoolsUrl);
    await once(socket, 'open');
    const cdp = new DevTools(socket);
    const page = await cdp.openPage(`http://127.0.0.1:${serverPort}/web/`, `
      HTMLMediaElement.prototype.play = function() { setTimeout(() => this.dispatchEvent(new Event('playing')), 0); return Promise.resolve(); };
      window.__playlistPending = 0;
      const originalFetch = window.fetch.bind(window);
      window.fetch = (input, init) => {
        const url = new URL(typeof input === 'string' ? input : input.url, location.href);
        const tracked = url.pathname.startsWith('/Playlists');
        if (tracked) window.__playlistPending += 1;
        const result = originalFetch(input, init);
        return tracked ? result.then(async (response) => { await response.clone().arrayBuffer(); return response; }).finally(() => { window.__playlistPending -= 1; }) : result;
      };
    `);
    await cdp.waitFor(page, 'document.querySelector("[data-screen=music]") !== null', Boolean, 'the signed-in music browser');
    await cdp.evaluate(page.sessionId, 'fetch("/Playlists?BodyProbe=true").then((response) => response.json()); true');
    await cdp.waitFor(page, 'window.__playlistPending === 0', Boolean, 'the complete playlist response body');
    assert.equal(bodyProbeSent, true, 'playlist request tracking completed before the response body was sent');
    await cdp.evaluate(page.sessionId, 'document.querySelector("[data-screen=users]").click(); true');
    await cdp.waitFor(page, 'document.querySelector("#user-form") !== null', Boolean, 'the People account form');
    assert.equal(await cdp.evaluate(page.sessionId, 'document.querySelector("#user-form [name=EnableRemoteAccess]").checked'), false, 'new accounts should have remote access unchecked by default');
    await cdp.evaluate(page.sessionId, `document.querySelector('[data-edit-user="${USER_ID}"]').click(); true`);
    assert.equal(await cdp.evaluate(page.sessionId, 'document.querySelector("#user-form [name=EnableRemoteAccess]").checked'), true, 'editing an existing remotely enabled account should keep its checkbox checked');
    await cdp.evaluate(page.sessionId, 'document.querySelector("[data-screen=music]").click(); true');
    await cdp.waitFor(page, 'document.querySelector("#playlist-create-form") !== null', Boolean, 'the playlist controls');
    await cdp.evaluate(page.sessionId, `(() => { const input = document.querySelector('#playlist-create-form [name="Name"]'); input.value = 'Road trip'; document.querySelector('#playlist-create-form').requestSubmit(); return true; })()`);
    await cdp.waitFor(page, 'document.querySelector("[data-select-playlist]")?.textContent.trim()', (value) => value === 'Road trip', 'the newly created playlist');
    const playlistId = await cdp.evaluate(page.sessionId, 'document.querySelector("[data-select-playlist]")?.dataset.selectPlaylist');
    assert.ok(playlistId, 'the created playlist did not have an identifier');

    for (let index = 0; index < AUDIO_ITEMS.length; index += 1) {
      const item = AUDIO_ITEMS[index];
      await cdp.evaluate(page.sessionId, `document.querySelector('[data-add-playlist-item="${item.Id}"]').click(); true`);
      await cdp.waitFor(page, 'document.querySelectorAll(".playlist-track-row").length', (count) => count === index + 1, `track ${index + 1} to be added`);
    }
    const initialOrder = await cdp.evaluate(page.sessionId, 'Array.from(document.querySelectorAll(".playlist-track-row .data-row-main strong"), (node) => node.textContent)');
    assert.deepEqual(initialOrder, ['Track One', 'Track Two', 'Track Three'], 'visible audio items were not added in order');
    await cdp.evaluate(page.sessionId, `document.querySelectorAll('.playlist-track-row')[2].querySelector('[data-move-playlist-entry="up"]').click(); true`);
    await cdp.waitFor(page, 'Array.from(document.querySelectorAll(".playlist-track-row .data-row-main strong"), (node) => node.textContent).join(",")', (value) => value === 'Track One,Track Three,Track Two', 'the reordered playlist');
    await cdp.evaluate(page.sessionId, `Array.from(document.querySelectorAll('.playlist-track-row')).find((row) => row.querySelector('strong')?.textContent === 'Track Two').querySelector('[data-remove-playlist-entry]').click(); true`);
    await cdp.waitFor(page, 'Array.from(document.querySelectorAll(".playlist-track-row .data-row-main strong"), (node) => node.textContent).join(",")', (value) => value === 'Track One,Track Three', 'the item removal');

    await cdp.evaluate(page.sessionId, `(() => { const input = document.querySelector('#playlist-create-form [name="Name"]'); input.value = 'Quiet'; document.querySelector('#playlist-create-form').requestSubmit(); return true; })()`);
    await cdp.waitFor(page, 'Array.from(document.querySelectorAll("[data-select-playlist]"), (button) => button.textContent.trim()).includes("Quiet")', Boolean, 'the second playlist');
    const quietId = await cdp.evaluate(page.sessionId, 'Array.from(document.querySelectorAll("[data-select-playlist]")).find((button) => button.textContent.trim() === "Quiet")?.dataset.selectPlaylist');
    assert.ok(quietId, 'the second playlist did not have an identifier');
    await cdp.evaluate(page.sessionId, `Array.from(document.querySelectorAll('[data-select-playlist]')).find((button) => button.dataset.selectPlaylist === '${playlistId}').click(); true`);
    await cdp.waitFor(page, 'Array.from(document.querySelectorAll(".playlist-track-row .data-row-main strong"), (node) => node.textContent).join(",")', (value) => value === 'Track One,Track Three', 'the first playlist before the failed selection');
    failPlaylistItemGetFor = quietId;
    await cdp.evaluate(page.sessionId, `Array.from(document.querySelectorAll('[data-select-playlist]')).find((button) => button.dataset.selectPlaylist === '${quietId}').click(); true`);
    await cdp.waitFor(page, 'document.querySelector(".playlist-panel .empty-state strong")?.textContent', (value) => value === 'Playlist could not be loaded', 'the failed playlist read state');
    assert.equal(await cdp.evaluate(page.sessionId, 'document.querySelectorAll(".playlist-track-row").length'), 0, 'tracks from the previously selected playlist remained visible after the new playlist failed to load');
    await cdp.evaluate(page.sessionId, `Array.from(document.querySelectorAll('[data-select-playlist]')).find((button) => button.dataset.selectPlaylist === '${quietId}').click(); true`);
    await cdp.waitFor(page, 'document.querySelector(".playlist-panel .empty-state strong")?.textContent', (value) => value === 'This playlist is empty', 'the retry of the empty playlist');
    await cdp.evaluate(page.sessionId, `document.querySelector('[data-add-playlist-item="${AUDIO_ITEMS[1].Id}"]').click(); true`);
    await cdp.waitFor(page, 'Array.from(document.querySelectorAll(".playlist-track-row .data-row-main strong"), (node) => node.textContent).join(",")', (value) => value === 'Track Two', 'the second playlist item');

    async function verifySelectionSurvivesMutation(playlistId, method, pathPart, trigger, label) {
      const gate = holdNextMutation(playlistId, method, pathPart);
      try {
        await trigger();
        await gate.started;
        await cdp.evaluate(page.sessionId, `Array.from(document.querySelectorAll('[data-select-playlist]')).find((button) => button.dataset.selectPlaylist === '${quietId}').click(); true`);
        await cdp.waitFor(page, 'Array.from(document.querySelectorAll(".playlist-track-row .data-row-main strong"), (node) => node.textContent).join(",")', (value) => value === 'Track Two', `${label}: select the other playlist`);
        const quietReadsBefore = apiRequests.filter((item) => item.method === 'GET' && item.pathname === `/Playlists/${quietId}/Items`).length;
        gate.release();
        await cdp.waitFor(page, 'window.__playlistPending === 0', Boolean, `${label}: request completion`);
        await cdp.evaluate(page.sessionId, 'new Promise((resolve) => setTimeout(() => resolve(true), 100))');
        const quietReadsAfter = apiRequests.filter((item) => item.method === 'GET' && item.pathname === `/Playlists/${quietId}/Items`).length;
        assert.equal(quietReadsAfter, quietReadsBefore, `${label} completion refreshed the playlist selected after the request began`);
        assert.equal(await cdp.evaluate(page.sessionId, 'Array.from(document.querySelectorAll(".playlist-track-row .data-row-main strong"), (node) => node.textContent).join(",")'), 'Track Two', `${label} completion changed the newly selected playlist`);
      } finally { gate.release(); }
    }

    await cdp.evaluate(page.sessionId, `Array.from(document.querySelectorAll('[data-select-playlist]')).find((button) => button.dataset.selectPlaylist === '${playlistId}').click(); true`);
    await cdp.waitFor(page, 'Array.from(document.querySelectorAll(".playlist-track-row .data-row-main strong"), (node) => node.textContent).join(",")', (value) => value === 'Track One,Track Three', 'select the first playlist for the delayed add');
    await verifySelectionSurvivesMutation(playlistId, 'POST', '/Items', () => cdp.evaluate(page.sessionId, `document.querySelector('[data-add-playlist-item="${AUDIO_ITEMS[1].Id}"]').click(); true`), 'add');

    await cdp.evaluate(page.sessionId, `Array.from(document.querySelectorAll('[data-select-playlist]')).find((button) => button.dataset.selectPlaylist === '${playlistId}').click(); true`);
    await cdp.waitFor(page, 'Array.from(document.querySelectorAll(".playlist-track-row .data-row-main strong"), (node) => node.textContent).join(",")', (value) => value === 'Track One,Track Three,Track Two', 'select the first playlist for the delayed move');
    await verifySelectionSurvivesMutation(playlistId, 'POST', '/Move/1', () => cdp.evaluate(page.sessionId, `document.querySelectorAll('.playlist-track-row')[2].querySelector('[data-move-playlist-entry="up"]').click(); true`), 'move');

    await cdp.evaluate(page.sessionId, `Array.from(document.querySelectorAll('[data-select-playlist]')).find((button) => button.dataset.selectPlaylist === '${playlistId}').click(); true`);
    await cdp.waitFor(page, 'Array.from(document.querySelectorAll(".playlist-track-row .data-row-main strong"), (node) => node.textContent).join(",")', (value) => value === 'Track One,Track Two,Track Three', 'select the first playlist for the delayed remove');
    await verifySelectionSurvivesMutation(playlistId, 'DELETE', '/Items', () => cdp.evaluate(page.sessionId, `Array.from(document.querySelectorAll('.playlist-track-row')).find((row) => row.querySelector('strong')?.textContent === 'Track Two').querySelector('[data-remove-playlist-entry]').click(); true`), 'remove');

    await cdp.evaluate(page.sessionId, `Array.from(document.querySelectorAll('[data-select-playlist]')).find((button) => button.dataset.selectPlaylist === '${playlistId}').click(); true`);
    await cdp.waitFor(page, 'Array.from(document.querySelectorAll(".playlist-track-row .data-row-main strong"), (node) => node.textContent).join(",")', (value) => value === 'Track One,Track Three', 'the first playlist after delayed mutations');

    await cdp.evaluate(page.sessionId, 'document.querySelector("#play-playlist").click(); true');
    await cdp.waitFor(page, 'document.querySelector("#player-title")?.textContent', (value) => value === 'Track One', 'the first playlist track');
    await cdp.waitFor(page, 'window.__playlistTestReady = Boolean(document.querySelector("#player-stage audio")); window.__playlistTestReady', Boolean, 'the first audio player');
    await cdp.evaluate(page.sessionId, 'document.querySelector("#player-stage audio").dispatchEvent(new Event("ended")); true');
    await cdp.waitFor(page, 'document.querySelector("#player-title")?.textContent', (value) => value === 'Track Three', 'the queued next track');
    assert.deepEqual(playbackItems, [AUDIO_ITEMS[0].Id, AUDIO_ITEMS[2].Id], 'playlist playback did not follow the edited order');
    assert.ok(apiRequests.some((item) => item.pathname === '/Playlists' && item.method === 'POST'), 'playlist creation did not use the collection API');
    assert.ok(apiRequests.some((item) => item.pathname === `/Playlists/${playlistId}/Items` && item.method === 'POST'), 'adding an item did not use the playlist item API');
    assert.ok(apiRequests.some((item) => item.pathname.endsWith('/Move/1') && item.method === 'POST'), 'reordering did not use the item move route');
    assert.ok(apiRequests.some((item) => item.pathname === `/Playlists/${playlistId}/Items` && item.method === 'DELETE'), 'removing an item did not use the item delete route');
    const playlistRequests = apiRequests.filter((item) => item.pathname.startsWith('/Playlists'));
    assert.ok(playlistRequests.every((item) => !item.search.includes('UserId=')), 'playlist controls attempted to select a user other than the signed-in account');
    process.stdout.write('Playlist browser checks passed: remote-access create and edit defaults, user-scoped playlist create, visible audio add, reorder, remove, failed-load isolation, stale mutation isolation, and sequential playback through the existing player.\n');
    await cdp.send('Target.closeTarget', { targetId: page.targetId });
  } catch (error) {
    const routes = apiRequests.map((item) => `${item.method} ${item.pathname}${item.search}`).join('\n');
    const details = `${error.message}\nAPI requests:\n${routes}\nChrome output:\n${chromeOutput.trim().slice(-2000)}`;
    throw new Error(details, { cause: error });
  } finally {
    socket?.close();
    if (chrome) {
      chrome.kill();
      await Promise.race([once(chrome, 'exit').catch(() => {}), new Promise((resolve) => setTimeout(resolve, 3000))]);
    }
    const closed = new Promise((resolve) => server.close(() => resolve()));
    server.closeAllConnections();
    await closed;
    for (let attempt = 0; profilePath && attempt < 10; attempt += 1) {
      try {
        if (!fs.existsSync(profilePath)) break;
        const absolute = fs.realpathSync(profilePath);
        const normalize = (value) => process.platform === 'win32' ? value.toLowerCase() : value;
        assert.equal(normalize(path.dirname(absolute)), normalize(fs.realpathSync(os.tmpdir())));
        assert.ok(path.basename(absolute).startsWith('puffinbox-playlists-browser-'));
        fs.rmSync(absolute, { recursive: true, force: true });
        break;
      }
      catch (error) { if (error.code !== 'EPERM' || attempt === 9) break; await new Promise((resolve) => setTimeout(resolve, 150)); }
    }
  }
}

main().catch((error) => { console.error(error); process.exitCode = 1; });
