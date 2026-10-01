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
const LIBRARY_ID = 'a37f75ba-64b4-4a6e-beb6-70a2c82408a1';
const CHANNEL_ID = 'afbf8ac3-64a3-4fc1-9ec9-534dcb224201';
const SECOND_CHANNEL_ID = 'afbf8ac3-64a3-4fc1-9ec9-534dcb224202';
const LIVE_PLAY_SESSION_ID = 'b0ccab98-0d93-461c-a06f-f872d2789f60';
const PROGRAM_ID = 'b0ccab98-0d93-461c-a06f-f872d2789f51';
const SECOND_PROGRAM_ID = 'b0ccab98-0d93-461c-a06f-f872d2789f52';
const SOURCE_ID = 'def95529-32ab-4bd8-b6ee-5342d6dc4b02';
const INITIAL_TIMER_ID = 'c0af0190-2ae8-4b72-9797-3108328b0001';
const ACTIVE_TIMER_ID = 'c0af0190-2ae8-4b72-9797-3108328b0002';
const INITIAL_SERIES_TIMER_ID = 'd0af0190-2ae8-4b72-9797-3108328b0001';
const CHANNEL_NAME = 'North <img id="livetv-injected" src=x>';
const PROGRAM_NAME = 'Morning <img id="guide-injected" src=x>';
const PROGRAM_DESCRIPTION = 'Guide text <script id="guide-script-injected">window.__livetvXss = true</script>';
const nextMinute = Math.ceil(Date.now() / 60_000) * 60_000;
const TIMER_START = new Date(nextMinute + 60 * 60_000).toISOString();
const TIMER_END = new Date(nextMinute + 90 * 60_000).toISOString();
const SERIES_START = new Date(nextMinute + 3 * 60 * 60_000).toISOString();
const SERIES_END = new Date(nextMinute + 4 * 60 * 60_000).toISOString();
const apiRequests = [];
const timers = [
  {
    Id: INITIAL_TIMER_ID, Name: 'Existing <img id="timer-injected" src=x>', ChannelId: CHANNEL_ID, ChannelName: CHANNEL_NAME,
    ProgramId: null, OutputLibraryId: LIBRARY_ID, PrePaddingSeconds: 120, PostPaddingSeconds: 60,
    StartDate: TIMER_START, EndDate: TIMER_END, Status: 'New',
  },
  {
    Id: ACTIVE_TIMER_ID, Name: 'Active timer', ChannelId: CHANNEL_ID, ChannelName: CHANNEL_NAME,
    ProgramId: null, OutputLibraryId: LIBRARY_ID, PrePaddingSeconds: 0, PostPaddingSeconds: 0,
    StartDate: TIMER_START, EndDate: TIMER_END, Status: 'InProgress',
  },
];
const seriesTimers = [{
  Id: INITIAL_SERIES_TIMER_ID, Name: 'Series <img id="series-injected" src=x>', ChannelId: CHANNEL_ID,
  ChannelName: CHANNEL_NAME, ProgramId: PROGRAM_ID, Days: ['Monday', 'Wednesday', 'Friday'],
  StartDate: SERIES_START, EndDate: SERIES_END, PrePaddingSeconds: 90, PostPaddingSeconds: 45,
  OutputLibraryId: LIBRARY_ID, RecordAnyTime: true, RecordAnyChannel: false,
  SkipEpisodesInLibrary: false, RecordNewOnly: false, KeepUpTo: 0, KeepUntil: 'UntilDeleted', Priority: 0,
}];
const sources = [
  { Id: SOURCE_ID, LibraryId: LIBRARY_ID, Name: 'Pinned <img id="source-injected" src=x>', Enabled: true, RefreshStatus: 'ready', LastRefreshedAt: '2026-09-29T09:00:00Z' },
];
const sourceConfigs = new Map([[SOURCE_ID, {
  PlaylistUrl: 'https://private.example/channels.m3u',
  GuideUrl: 'https://private.example/guide.xml',
  OriginPins: [{ origin: 'https://private.example/', addresses: ['203.0.113.20'] }],
}]]);
const guideGate = { channelId: null, started: null, startedResolve: null, pending: null, releaseResolve: null };
let failNextGuideFor = null;

function holdNextGuide(channelId) {
  guideGate.channelId = channelId;
  guideGate.started = new Promise((resolve) => { guideGate.startedResolve = resolve; });
  guideGate.pending = new Promise((resolve) => { guideGate.releaseResolve = resolve; });
  return { started: guideGate.started, release: guideGate.releaseResolve };
}

async function holdGuideResponse(channelId) {
  if (guideGate.channelId !== channelId) return;
  const pending = guideGate.pending;
  guideGate.channelId = null;
  guideGate.startedResolve();
  await pending;
}

function jsonResponse(response, status, value) {
  const body = Buffer.from(JSON.stringify(value));
  response.writeHead(status, { 'Content-Type': 'application/json; charset=utf-8', 'Content-Length': body.length, 'Cache-Control': 'no-store' });
  response.end(body);
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
  const absolute = path.resolve(ROOT, '.' + decodeURIComponent(pathname));
  if (!absolute.startsWith(ROOT + path.sep) || !fs.existsSync(absolute) || !fs.statSync(absolute).isFile()) {
    response.writeHead(404); response.end(); return;
  }
  const contentType = ({ '.html': 'text/html; charset=utf-8', '.css': 'text/css; charset=utf-8', '.js': 'application/javascript; charset=utf-8' })[path.extname(absolute)] || 'application/octet-stream';
  const body = fs.readFileSync(absolute);
  response.writeHead(200, { 'Content-Type': contentType, 'Content-Length': body.length, 'Cache-Control': 'no-store' });
  response.end(body);
}

function currentUser(profile) {
  const admin = profile === 'admin';
  const access = admin || profile === 'manager' || profile === 'viewer' || profile === 'empty' || profile === 'noPlayback';
  return {
    Id: '7c9e6679-7425-40de-944b-e07fc1f90ae7',
    Name: 'Synthetic Live TV test',
    IsAdministrator: admin,
    Policy: {
      IsAdministrator: admin,
      EnableLiveTvAccess: access,
      EnableLiveTvManagement: admin || profile === 'manager',
      EnableMediaPlayback: profile !== 'noPlayback',
      EnableAllFolders: true,
      BlockUnratedItems: [],
    },
  };
}

function listResult(items) {
  return { Items: items, TotalRecordCount: items.length, StartIndex: 0 };
}

function createTestServer() {
  return http.createServer(async (request, response) => {
    const url = new URL(request.url, 'http://127.0.0.1');
    const profile = String(request.headers['x-test-profile'] || 'manager');
    const body = ['POST', 'PUT', 'PATCH'].includes(request.method) ? await readJson(request) : undefined;
    const entry = { method: request.method, pathname: url.pathname, search: url.search, profile, body };
    apiRequests.push(entry);

    if (url.pathname === '/web/') { serveFile(response, '/web/index.html'); return; }
    if (url.pathname.startsWith('/web/')) { serveFile(response, url.pathname); return; }
    if (url.pathname === '/Startup/Configuration') { jsonResponse(response, 200, { IsStartupWizardCompleted: true }); return; }
    if (url.pathname === '/Users/Me') { jsonResponse(response, 200, currentUser(profile)); return; }
    if (url.pathname === '/System/Info') { jsonResponse(response, 200, { ServerName: 'Synthetic Live TV server' }); return; }
    if (url.pathname === '/UserViews') { jsonResponse(response, 200, { Items: [] }); return; }
    if (url.pathname === '/Library/VirtualFolders') {
      jsonResponse(response, 200, [{ Id: LIBRARY_ID, ItemId: LIBRARY_ID, Name: 'Synthetic recordings', CollectionType: 'tvshows', Locations: [] }]);
      return;
    }
    if (url.pathname === '/Localization/ParentalRatings') { jsonResponse(response, 200, []); return; }
    if (url.pathname === '/Items' && request.method === 'GET') { jsonResponse(response, 200, listResult([])); return; }
    if (url.pathname === '/LiveTv/Channels' && request.method === 'GET') {
      if (profile === 'empty') { jsonResponse(response, 200, listResult([])); return; }
      jsonResponse(response, 200, listResult([
        { Id: CHANNEL_ID, Name: CHANNEL_NAME, Type: 'LiveTvChannel', MediaType: 'Video', ChannelType: 'TV', IsFolder: false, Overview: 'News <svg onload=x>' },
        { Id: SECOND_CHANNEL_ID, Name: 'Second synthetic channel', Type: 'LiveTvChannel', MediaType: 'Video', ChannelType: 'TV', IsFolder: false, Overview: 'Second guide' },
      ]));
      return;
    }
    const playbackInfoChannel = /^\/Items\/([^/]+)\/PlaybackInfo$/.exec(url.pathname);
    if (playbackInfoChannel && request.method === 'POST') {
      assert.equal(playbackInfoChannel[1], CHANNEL_ID, 'the UI requested live playback for the selected channel');
      jsonResponse(response, 200, {
        PlaySessionId: LIVE_PLAY_SESSION_ID,
        MediaSources: [{
          Id: CHANNEL_ID, Name: CHANNEL_NAME, Protocol: 'Http', Type: 'Default', Container: 'm3u8',
          SupportsDirectPlay: false, SupportsDirectStream: false, SupportsTranscoding: true,
          MediaStreams: [], Formats: ['m3u8'],
          TranscodingUrl: '/LiveTv/Channels/' + CHANNEL_ID + '/master.m3u8?PlaySessionId=' + LIVE_PLAY_SESSION_ID + '&maxStreamingBitrate=2000000&maxAudioChannels=2',
        }],
      });
      return;
    }
    if (url.pathname === '/Sessions/Playing' || url.pathname === '/Sessions/Playing/Stopped') {
      emptyResponse(response);
      return;
    }
    if (url.pathname === '/LiveTv/Channels/' + CHANNEL_ID + '/master.m3u8' && request.method === 'GET') {
      assert.equal(url.searchParams.get('PlaySessionId'), LIVE_PLAY_SESSION_ID, 'the live session id was passed to the stream route');
      response.writeHead(200, { 'Content-Type': 'application/vnd.apple.mpegurl', 'Cache-Control': 'no-store' });
      response.end('#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-STREAM-INF:BANDWIDTH=2000000\n/LiveTv/Channels/' + CHANNEL_ID + '/hls/' + LIVE_PLAY_SESSION_ID + '/playlist.m3u8\n');
      return;
    }
    if (url.pathname === '/LiveTv/Channels/' + CHANNEL_ID + '/hls/' + LIVE_PLAY_SESSION_ID + '/playlist.m3u8' && request.method === 'GET') {
      response.writeHead(200, { 'Content-Type': 'application/vnd.apple.mpegurl', 'Cache-Control': 'no-store' });
      response.end('#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:3.0,\nsegment00001.ts\n');
      return;
    }
    if (url.pathname === '/LiveTv/Channels/' + CHANNEL_ID + '/hls/' + LIVE_PLAY_SESSION_ID + '/segment00001.ts' && request.method === 'GET') {
      response.writeHead(200, { 'Content-Type': 'video/mp2t', 'Cache-Control': 'no-store' });
      response.end(Buffer.alloc(16));
      return;
    }
    if (url.pathname === '/LiveTv/Channels/' + CHANNEL_ID + '/hls/' + LIVE_PLAY_SESSION_ID + '/keepalive' && request.method === 'POST') {
      emptyResponse(response);
      return;
    }
    if (url.pathname === '/LiveTv/Channels/' + CHANNEL_ID + '/hls/' + LIVE_PLAY_SESSION_ID && request.method === 'DELETE') {
      emptyResponse(response);
      return;
    }
    if (url.pathname === '/LiveTv/Programs' && request.method === 'GET') {
      const channelId = url.searchParams.get('ChannelId');
      await holdGuideResponse(channelId);
      if (failNextGuideFor === channelId) {
        failNextGuideFor = null;
        jsonResponse(response, 503, { Message: 'Guide request <img id="error-injected" src=x>' });
        return;
      }
      const program = channelId === SECOND_CHANNEL_ID
        ? { Id: SECOND_PROGRAM_ID, Name: 'Second synthetic programme', Type: 'LiveTvProgram', ChannelId: SECOND_CHANNEL_ID, StartDate: '2026-09-29T13:00:00Z', EndDate: '2026-09-29T14:00:00Z', Overview: 'Second guide entry' }
        : { Id: PROGRAM_ID, Name: PROGRAM_NAME, Type: 'LiveTvProgram', ChannelId: CHANNEL_ID, StartDate: '2026-09-29T12:00:00Z', EndDate: '2026-09-29T13:00:00Z', Overview: PROGRAM_DESCRIPTION, Category: 'News <b>category</b>', OfficialRating: 'PG <i>rating</i>' };
      jsonResponse(response, 200, listResult([program]));
      return;
    }
    if (url.pathname === '/LiveTv/Timers' && request.method === 'GET') {
      if (profile === 'empty') { jsonResponse(response, 200, listResult([])); return; }
      const results = url.searchParams.get('IsActive') === 'true'
        ? timers.filter((timer) => timer.Status === 'InProgress')
        : timers.filter((timer) => timer.Status === 'New');
      jsonResponse(response, 200, listResult(results));
      return;
    }
    if (url.pathname === '/LiveTv/Timers' && request.method === 'POST') {
      const timer = {
        Id: randomUUID(), Name: body.Name || PROGRAM_NAME, ChannelId: body.ChannelId, ProgramId: body.ProgramId || null,
        StartDate: body.StartDate || '2026-09-29T12:00:00Z', EndDate: body.EndDate || '2026-09-29T13:00:00Z',
        Status: 'New', PrePaddingSeconds: body.PrePaddingSeconds || 0, PostPaddingSeconds: body.PostPaddingSeconds || 0,
      };
      timers.push(timer);
      entry.createdId = timer.Id;
      emptyResponse(response);
      return;
    }
    const timerRoute = /^\/LiveTv\/Timers\/([^/]+)$/.exec(url.pathname);
    if (timerRoute && request.method === 'POST') {
      const timer = timers.find((item) => item.Id === timerRoute[1] && item.Status === 'New' && !item.SeriesTimerId);
      if (!timer) { jsonResponse(response, 404, { Message: 'Scheduled one-off timer not found' }); return; }
      Object.assign(timer, body);
      entry.updatedId = timer.Id;
      emptyResponse(response);
      return;
    }
    if (timerRoute && request.method === 'DELETE') {
      const index = timers.findIndex((timer) => timer.Id === timerRoute[1]);
      if (index >= 0) timers.splice(index, 1);
      emptyResponse(response);
      return;
    }
    if (url.pathname === '/LiveTv/SeriesTimers' && request.method === 'GET') {
      jsonResponse(response, 200, listResult(profile === 'empty' ? [] : seriesTimers));
      return;
    }
    if (url.pathname === '/LiveTv/SeriesTimers' && request.method === 'POST') {
      const series = {
        Id: randomUUID(), Name: PROGRAM_NAME, ChannelId: body.ChannelId, ChannelName: CHANNEL_NAME,
        ProgramId: body.ProgramId, DayPattern: body.DayPattern, StartDate: '2026-09-29T12:00:00Z',
        EndDate: '2026-09-29T13:00:00Z',
      };
      seriesTimers.push(series);
      entry.createdId = series.Id;
      emptyResponse(response);
      return;
    }
    const seriesRoute = /^\/LiveTv\/SeriesTimers\/([^/]+)$/.exec(url.pathname);
    if (seriesRoute && request.method === 'POST') {
      const series = seriesTimers.find((item) => item.Id === seriesRoute[1]);
      if (!series) { jsonResponse(response, 404, { Message: 'Series timer not found' }); return; }
      Object.assign(series, body);
      if (Array.isArray(body.Days)) delete series.DayPattern;
      else if (body.DayPattern) delete series.Days;
      entry.updatedId = series.Id;
      emptyResponse(response);
      return;
    }
    if (seriesRoute && request.method === 'DELETE') {
      const index = seriesTimers.findIndex((timer) => timer.Id === seriesRoute[1]);
      if (index >= 0) seriesTimers.splice(index, 1);
      emptyResponse(response);
      return;
    }
    if (url.pathname === '/LiveTv/Recordings' && request.method === 'GET') {
      jsonResponse(response, 200, listResult(profile === 'empty' ? [] : [
        { Id: 'eae990be-a715-4a6d-9687-4a0be1e45001', TimerId: timers[0]?.Id || 'c0af0190-2ae8-4b72-9797-3108328b0001', ChannelId: CHANNEL_ID, LibraryId: LIBRARY_ID, ChannelName: CHANNEL_NAME, Name: 'Synthetic recording', Status: 'completed', ByteCount: 2_097_152, StartDate: '2026-09-28T09:00:00Z', EndDate: '2026-09-28T09:30:00Z' },
      ]));
      return;
    }
    if (url.pathname === '/Admin/LiveTv/Sources' && request.method === 'GET') {
      if (profile !== 'admin') { jsonResponse(response, 403, { Message: 'Administrator access required' }); return; }
      jsonResponse(response, 200, sources);
      return;
    }
    if (url.pathname === '/Admin/LiveTv/Sources' && request.method === 'POST') {
      if (profile !== 'admin') { jsonResponse(response, 403, { Message: 'Administrator access required' }); return; }
      const source = { Id: randomUUID(), LibraryId: body.LibraryId, Name: body.Name, Enabled: true, RefreshStatus: 'queued', LastRefreshedAt: null, LastErrorCode: null };
      sources.push(source);
      sourceConfigs.set(source.Id, {
        PlaylistUrl: body.PlaylistUrl,
        ...(body.GuideUrl ? { GuideUrl: body.GuideUrl } : {}),
        OriginPins: body.OriginPins,
      });
      entry.createdId = source.Id;
      jsonResponse(response, 201, source);
      return;
    }
    const sourceRoute = /^\/Admin\/LiveTv\/Sources\/([^/]+)$/.exec(url.pathname);
    if (sourceRoute && request.method === 'POST') {
      if (profile !== 'admin') { jsonResponse(response, 403, { Message: 'Administrator access required' }); return; }
      const source = sources.find((item) => item.Id === sourceRoute[1]);
      if (!source) { jsonResponse(response, 404, { Message: 'Source not found' }); return; }
      Object.assign(source, Object.fromEntries(Object.entries(body).filter(([key]) => ['LibraryId', 'Name'].includes(key))));
      const config = sourceConfigs.get(source.Id) || {};
      if (Object.hasOwn(body, 'PlaylistUrl')) config.PlaylistUrl = body.PlaylistUrl;
      if (Object.hasOwn(body, 'OriginPins')) config.OriginPins = body.OriginPins;
      if (Object.hasOwn(body, 'GuideUrl')) {
        if (body.GuideUrl === null) delete config.GuideUrl;
        else config.GuideUrl = body.GuideUrl;
      }
      sourceConfigs.set(source.Id, config);
      entry.updatedId = source.Id;
      entry.configAfter = structuredClone(config);
      jsonResponse(response, 200, source);
      return;
    }
    if (sourceRoute && request.method === 'DELETE') {
      if (profile !== 'admin') { jsonResponse(response, 403, { Message: 'Administrator access required' }); return; }
      const index = sources.findIndex((item) => item.Id === sourceRoute[1]);
      if (index < 0) { jsonResponse(response, 404, { Message: 'Source not found' }); return; }
      sources.splice(index, 1);
      sourceConfigs.delete(sourceRoute[1]);
      emptyResponse(response);
      return;
    }
    const sourceRefresh = /^\/Admin\/LiveTv\/Sources\/([^/]+)\/Refresh$/.exec(url.pathname);
    if (sourceRefresh && request.method === 'POST') {
      if (profile !== 'admin') { jsonResponse(response, 403, { Message: 'Administrator access required' }); return; }
      const source = sources.find((item) => item.Id === sourceRefresh[1]);
      if (!source) { jsonResponse(response, 404, { Message: 'Source not found' }); return; }
      source.RefreshStatus = 'ready';
      source.LastRefreshedAt = new Date().toISOString();
      jsonResponse(response, 200, source);
      return;
    }
    response.writeHead(404, { 'Content-Type': 'text/plain; charset=utf-8' });
    response.end('test API route not found');
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
  throw new Error('A Chrome or Chromium executable is required. Set CHROME_BIN to its path.');
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
    if (response.exceptionDetails) {
      const detail = response.exceptionDetails.exception?.description || response.exceptionDetails.text || 'Browser evaluation failed.';
      throw new Error(detail + '\nSynthetic browser step: ' + expression.slice(0, 200));
    }
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
    throw new Error('Timed out waiting for ' + label + '; last browser value: ' + JSON.stringify(last));
  }
}

function browserTestScript() {
  return [
    'const profile = new URL(location.href).searchParams.get("profile") || "manager";',
    'const originalFetch = window.fetch.bind(window);',
    'const TestHls = class TestHls {',
    '  static Events = { ERROR: "error", MANIFEST_PARSED: "manifestParsed", MEDIA_ATTACHED: "mediaAttached" };',
    '  static isSupported() { return true; }',
    '  constructor() { this.handlers = new Map(); }',
    '  on(event, callback) { this.handlers.set(event, callback); }',
    '  emit(event, data = {}) { this.handlers.get(event)?.(event, data); }',
    '  attachMedia(media) { this.media = media; queueMicrotask(() => this.emit(TestHls.Events.MEDIA_ATTACHED)); }',
    '  loadSource(url) { fetch(url).then((response) => response.text()).then((text) => { const child = text.split(/\\r?\\n/).find((line) => line.includes("/playlist.m3u8")); return child ? fetch(new URL(child, url)).then((response) => response.text()).then((playlist) => { const segment = playlist.split(/\\r?\\n/).find((line) => line && !line.startsWith("#")); return segment ? fetch(new URL(segment, new URL(child, url))).then((response) => response.arrayBuffer()) : null; }) : null; }).then(() => this.emit(TestHls.Events.MANIFEST_PARSED)).catch((error) => this.emit(TestHls.Events.ERROR, { fatal: true, details: String(error) })); }',
    '  destroy() {}',
    '};',
    'document.addEventListener("DOMContentLoaded", () => { window.Hls = TestHls; }, { once: true });',
    'window.__testLiveMediaPlaying = false;',
    'window.__testPlaybackStartRequested = false;',
    'window.__testPlaybackStopReported = false;',
    'window.__testLiveSessionStopRequested = false;',
    'HTMLMediaElement.prototype.play = function() { setTimeout(() => { window.__testLiveMediaPlaying = true; this.dispatchEvent(new Event("playing")); }, 0); return Promise.resolve(); };',
    'window.__liveTvPending = 0;',
    'window.fetch = (input, init = {}) => {',
    '  const url = new URL(typeof input === "string" ? input : input.url, location.href);',
    '  const method = String(init.method || (typeof input === "string" ? "GET" : input.method) || "GET").toUpperCase();',
    '  if (method === "POST" && url.pathname === "/Sessions/Playing") window.__testPlaybackStartRequested = true;',
    '  if (method === "POST" && url.pathname === "/Sessions/Playing/Stopped") window.__testPlaybackStopReported = true;',
    '  if (method === "DELETE" && url.pathname.includes("/LiveTv/Channels/") && url.pathname.includes("/hls/")) window.__testLiveSessionStopRequested = true;',
    '  const tracked = url.pathname.startsWith("/LiveTv/") || url.pathname.startsWith("/Admin/LiveTv/");',
    '  if (tracked) window.__liveTvPending += 1;',
    '  const headers = new Headers(init.headers || {});',
    '  headers.set("X-Test-Profile", profile);',
    '  const result = originalFetch(input, { ...init, headers });',
    '  return tracked ? result.finally(() => { window.__liveTvPending -= 1; }) : result;',
    '};',
  ].join('\n');
}

async function main() {
  const server = createTestServer();
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  const serverPort = server.address().port;
  const debugPort = await freePort();
  const profilePath = fs.mkdtempSync(path.join(os.tmpdir(), 'puffinbox-livetv-browser-'));
  const chrome = spawn(findChrome(), [
    '--headless=new', '--disable-gpu', '--no-first-run', '--no-default-browser-check', '--disable-background-networking',
    '--autoplay-policy=no-user-gesture-required', '--mute-audio', '--remote-debugging-port=' + debugPort,
    '--remote-allow-origins=*', '--user-data-dir=' + profilePath,
    ...(process.platform === 'win32' ? [] : ['--no-sandbox']), 'about:blank',
  ], { stdio: ['ignore', 'ignore', 'pipe'], windowsHide: true });
  let chromeOutput = '';
  chrome.stderr.on('data', (chunk) => { chromeOutput += chunk.toString(); });
  let socket;
  const pages = [];
  try {
    let devtoolsUrl = null;
    for (let attempt = 0; attempt < 150; attempt += 1) {
      if (chrome.exitCode != null) throw new Error('Chrome exited before its debugging endpoint was ready: ' + chromeOutput);
      try {
        const response = await fetch('http://127.0.0.1:' + debugPort + '/json/version');
        if (response.ok) { devtoolsUrl = (await response.json()).webSocketDebuggerUrl; break; }
      } catch (_) { /* Chrome is still starting. */ }
      await new Promise((resolve) => setTimeout(resolve, 100));
    }
    if (!devtoolsUrl) throw new Error('Chrome did not expose its debugging endpoint: ' + chromeOutput);
    socket = new WebSocket(devtoolsUrl);
    await once(socket, 'open');
    const cdp = new DevTools(socket);
    const baseUrl = 'http://127.0.0.1:' + serverPort + '/web/?profile=';
    const manager = await cdp.openPage(baseUrl + 'manager', browserTestScript());
    pages.push(manager);
    await cdp.waitFor(manager, 'document.querySelector("[data-screen=\\"live-tv\\"]") !== null', Boolean, 'the permitted Live TV navigation');
    await cdp.evaluate(manager.sessionId, 'document.querySelector("[data-screen=\\"live-tv\\"]").click(); true');
    await cdp.waitFor(manager, 'document.querySelector(".live-tv-channel[data-live-tv-channel=\\"' + CHANNEL_ID + '\\"]") !== null && document.querySelector(".live-tv-program") !== null', Boolean, 'the channel list and guide');
    assert.equal(await cdp.evaluate(manager.sessionId, 'window.Hls?.name'), 'TestHls', 'the deterministic HLS adapter was not active after vendor scripts loaded');
    assert.equal(await cdp.evaluate(manager.sessionId, 'document.querySelector("[data-live-tv-play=\\"' + CHANNEL_ID + '\\"]")?.textContent'), 'Watch live', 'the playback-enabled account did not get a live channel action');
    await cdp.evaluate(manager.sessionId, 'document.querySelector("[data-live-tv-play=\\"' + CHANNEL_ID + '\\"]").click(); true');
    await cdp.waitFor(manager, '(() => ({ open: document.querySelector("#player-dialog")?.open, note: document.querySelector("#player-note")?.textContent, media: document.querySelector("#player-stage video") !== null, playing: window.__testLiveMediaPlaying }))()', (value) => value?.open && value.media && value.playing && value.note?.includes('server-converted HLS'), 'the live HLS player to enter playing');
    await cdp.waitFor(manager, 'window.__testPlaybackStartRequested && window.__liveTvPending === 0', Boolean, 'live stream and playback-session setup requests');
    const livePlaybackInfo = apiRequests.find((entry) => entry.method === 'POST' && entry.pathname === '/Items/' + CHANNEL_ID + '/PlaybackInfo');
    assert.ok(livePlaybackInfo, 'the Watch live action did not negotiate channel playback');
    assert.ok(livePlaybackInfo.body?.DeviceProfile?.TranscodingProfiles?.some((profile) => profile.Protocol === 'hls'), 'the playback request omitted the browser HLS profile');
    assert.ok(apiRequests.some((entry) => entry.profile === 'manager' && entry.method === 'GET' && entry.pathname === '/LiveTv/Channels/' + CHANNEL_ID + '/master.m3u8'), 'the player did not request the same-origin live HLS entry point');
    assert.ok(apiRequests.some((entry) => entry.profile === 'manager' && entry.method === 'POST' && entry.pathname === '/LiveTv/Channels/' + CHANNEL_ID + '/hls/' + LIVE_PLAY_SESSION_ID + '/keepalive'), 'the player did not keep the live session active');
    assert.ok(apiRequests.some((entry) => entry.profile === 'manager' && entry.method === 'GET' && entry.pathname === '/LiveTv/Channels/' + CHANNEL_ID + '/hls/' + LIVE_PLAY_SESSION_ID + '/segment00001.ts'), 'the player did not request the segment from the child playlist');
    await cdp.evaluate(manager.sessionId, 'document.querySelector("#player-dialog .player-header button").click(); true');
    await cdp.waitFor(manager, 'window.__testPlaybackStopReported && window.__testLiveSessionStopRequested && window.__liveTvPending === 0', Boolean, 'live session stop requests');
    assert.ok(apiRequests.some((entry) => entry.method === 'POST' && entry.pathname === '/Sessions/Playing'), 'the live play session was not recorded');
    assert.ok(apiRequests.some((entry) => entry.method === 'POST' && entry.pathname === '/Sessions/Playing/Stopped'), 'the live play session was not closed');
    assert.ok(apiRequests.some((entry) => entry.method === 'DELETE' && entry.pathname === '/LiveTv/Channels/' + CHANNEL_ID + '/hls/' + LIVE_PLAY_SESSION_ID), 'the live HLS session was not stopped');
    const escaped = await cdp.evaluate(manager.sessionId, '({ channel: document.querySelector(".live-tv-channel strong")?.textContent, program: document.querySelector(".live-tv-program .data-row-main strong")?.textContent, description: document.querySelector(".live-tv-program-description")?.textContent, injected: document.querySelector("#livetv-injected,#guide-injected,#guide-script-injected") !== null, scriptRan: window.__livetvXss === true })');
    assert.equal(escaped.channel, CHANNEL_NAME, 'channel/provider text should be rendered as text');
    assert.equal(escaped.program, PROGRAM_NAME, 'programme/provider text should be rendered as text');
    assert.equal(escaped.description, PROGRAM_DESCRIPTION, 'guide descriptions should be rendered as text');
    assert.equal(escaped.injected, false, 'server text inserted an HTML element');
    assert.equal(escaped.scriptRan, false, 'server text ran script in the browser');
    assert.ok(await cdp.evaluate(manager.sessionId, 'document.body.textContent.includes("Synthetic recording")'), 'recordings were not displayed');
    assert.ok(await cdp.evaluate(manager.sessionId, 'document.body.textContent.includes("2.00 MiB")'), 'recording size was not displayed');
    assert.ok(await cdp.evaluate(manager.sessionId, 'document.querySelector("[data-live-tv-cancel-timer]") !== null'), 'management user could not cancel a listed timer');
    assert.equal(await cdp.evaluate(manager.sessionId, 'document.querySelector("[data-live-tv-edit-timer=\\"' + ACTIVE_TIMER_ID + '\\"]") === null'), true, 'an active timer showed an edit control');
    assert.equal(await cdp.evaluate(manager.sessionId, 'document.querySelector("#timer-injected,#series-injected") === null'), true, 'timer provider text was inserted as markup');

    await cdp.evaluate(manager.sessionId, 'document.querySelector("[data-live-tv-edit-timer=\\"' + INITIAL_TIMER_ID + '\\"]").click(); true');
    await cdp.waitFor(manager, 'document.querySelector("[data-live-tv-edit-timer-form=\\"' + INITIAL_TIMER_ID + '\\"]") !== null', Boolean, 'the scheduled one-off timer editor');
    const initialTimerEdit = await cdp.evaluate(manager.sessionId, '(() => { const form = document.querySelector("[data-live-tv-edit-timer-form]"); return { name: form.elements.Name.value, start: form.elements.StartDate.value, end: form.elements.EndDate.value, pre: form.elements.PrePaddingSeconds.value, post: form.elements.PostPaddingSeconds.value }; })()');
    assert.equal(initialTimerEdit.name, 'Existing <img id="timer-injected" src=x>', 'one-off edit did not preserve the current name');
    assert.equal(initialTimerEdit.pre, '120', 'one-off edit did not preserve pre-padding');
    assert.equal(initialTimerEdit.post, '60', 'one-off edit did not preserve post-padding');
    const timerUpdatePath = '/LiveTv/Timers/' + INITIAL_TIMER_ID;
    const timerUpdateCount = apiRequests.filter((entry) => entry.method === 'POST' && entry.pathname === timerUpdatePath).length;
    const invalidTimerWindowError = await cdp.evaluate(manager.sessionId, '(() => { const form = document.querySelector("[data-live-tv-edit-timer-form]"); form.elements.EndDate.value = form.elements.StartDate.value; form.requestSubmit(); const error = form.querySelector("[data-live-tv-edit-error]"); return { message: error.textContent, visible: !error.hidden }; })()');
    assert.equal(invalidTimerWindowError.visible, true, 'invalid one-off window did not show an inline error');
    assert.match(invalidTimerWindowError.message, /no longer than four hours/i);
    assert.equal(apiRequests.filter((entry) => entry.method === 'POST' && entry.pathname === timerUpdatePath).length, timerUpdateCount, 'invalid one-off window posted an update');
    await cdp.evaluate(manager.sessionId, 'document.querySelector("[data-live-tv-cancel-edit-timer]").click(); true');
    assert.equal(await cdp.evaluate(manager.sessionId, 'document.querySelector("[data-live-tv-edit-timer-form]") === null'), true, 'Cancel did not close the one-off timer editor');
    assert.equal(apiRequests.filter((entry) => entry.method === 'POST' && entry.pathname === timerUpdatePath).length, timerUpdateCount, 'Cancel submitted a one-off timer update');
    await cdp.evaluate(manager.sessionId, 'document.querySelector("[data-live-tv-edit-timer=\\"' + INITIAL_TIMER_ID + '\\"]").click(); true');
    await cdp.waitFor(manager, 'document.querySelector("[data-live-tv-edit-timer-form]") !== null', Boolean, 'the one-off timer editor after cancel');
    await cdp.evaluate(manager.sessionId, '(() => { const form = document.querySelector("[data-live-tv-edit-timer-form]"); form.elements.Name.value = "Edited timer <img id=timer-edit-injected src=x>"; form.elements.PrePaddingSeconds.value = "240"; form.requestSubmit(); return true; })()');
    await cdp.waitFor(manager, 'document.querySelector("[data-live-tv-edit-timer-form]") === null && document.querySelector(".live-tv-timer-row strong")?.textContent.includes("Edited timer")', Boolean, 'the saved one-off timer and refreshed list');
    const timerUpdatePost = apiRequests.find((entry) => entry.method === 'POST' && entry.pathname === timerUpdatePath);
    assert.ok(timerUpdatePost, 'one-off timer edit did not use the item update route');
    assert.deepEqual(timerUpdatePost.body, {
      Id: INITIAL_TIMER_ID, Name: 'Edited timer <img id=timer-edit-injected src=x>', ChannelId: CHANNEL_ID,
      ProgramId: null, StartDate: TIMER_START, EndDate: TIMER_END, PrePaddingSeconds: 240,
      PostPaddingSeconds: 60, OutputLibraryId: LIBRARY_ID,
    }, 'one-off update should use PascalCase fields and preserve the existing channel, program, schedule, post-padding and destination');
    assert.equal(await cdp.evaluate(manager.sessionId, 'document.querySelector("#timer-edit-injected") === null'), true, 'updated timer text was inserted as markup');

    await cdp.evaluate(manager.sessionId, 'document.querySelector("[data-live-tv-edit-series=\\"' + INITIAL_SERIES_TIMER_ID + '\\"]").click(); true');
    await cdp.waitFor(manager, 'document.querySelector("[data-live-tv-edit-series-form=\\"' + INITIAL_SERIES_TIMER_ID + '\\"]") !== null', Boolean, 'the series timer editor');
    const initialSeriesEdit = await cdp.evaluate(manager.sessionId, '(() => { const form = document.querySelector("[data-live-tv-edit-series-form]"); return { name: form.elements.Name.value, pattern: form.elements.DayPattern.value, days: Array.from(form.querySelectorAll("input[name=Days]:checked"), (input) => input.value), pre: form.elements.PrePaddingSeconds.value, post: form.elements.PostPaddingSeconds.value }; })()');
    assert.equal(initialSeriesEdit.name, 'Series <img id="series-injected" src=x>', 'series edit did not preserve the current name');
    assert.equal(initialSeriesEdit.pattern, 'Custom', 'series custom days were not represented as a custom pattern');
    assert.deepEqual(initialSeriesEdit.days, ['Monday', 'Wednesday', 'Friday'], 'series custom days were changed in the editor');
    assert.equal(initialSeriesEdit.pre, '90', 'series edit did not preserve pre-padding');
    assert.equal(initialSeriesEdit.post, '45', 'series edit did not preserve post-padding');
    const seriesUpdatePath = '/LiveTv/SeriesTimers/' + INITIAL_SERIES_TIMER_ID;
    const seriesUpdateCount = apiRequests.filter((entry) => entry.method === 'POST' && entry.pathname === seriesUpdatePath).length;
    const emptyCustomDaysError = await cdp.evaluate(manager.sessionId, '(() => { const form = document.querySelector("[data-live-tv-edit-series-form]"); form.querySelectorAll("input[name=Days]").forEach((input) => { input.checked = false; }); form.requestSubmit(); const error = form.querySelector("[data-live-tv-edit-error]"); return { message: error.textContent, visible: !error.hidden }; })()');
    assert.equal(emptyCustomDaysError.visible, true, 'an empty custom day selection did not show an inline error');
    assert.match(emptyCustomDaysError.message, /choose at least one repeat day/i);
    assert.equal(apiRequests.filter((entry) => entry.method === 'POST' && entry.pathname === seriesUpdatePath).length, seriesUpdateCount, 'empty custom days posted a series update');
    await cdp.evaluate(manager.sessionId, 'document.querySelector("[data-live-tv-cancel-edit-series]").click(); true');
    assert.equal(await cdp.evaluate(manager.sessionId, 'document.querySelector("[data-live-tv-edit-series-form]") === null'), true, 'Cancel did not close the series timer editor');
    assert.equal(apiRequests.filter((entry) => entry.method === 'POST' && entry.pathname === seriesUpdatePath).length, seriesUpdateCount, 'Cancel submitted a series timer update');
    await cdp.evaluate(manager.sessionId, 'document.querySelector("[data-live-tv-edit-series=\\"' + INITIAL_SERIES_TIMER_ID + '\\"]").click(); true');
    await cdp.waitFor(manager, 'document.querySelector("[data-live-tv-edit-series-form]") !== null', Boolean, 'the series timer editor after cancel');
    await cdp.evaluate(manager.sessionId, 'document.querySelector("[data-live-tv-edit-series-form]").requestSubmit(); true');
    await cdp.waitFor(manager, 'document.querySelector("[data-live-tv-edit-series-form]") === null && window.__liveTvPending === 0', Boolean, 'the saved series timer and refreshed list');
    const seriesUpdatePost = apiRequests.find((entry) => entry.method === 'POST' && entry.pathname === seriesUpdatePath);
    assert.ok(seriesUpdatePost, 'series timer edit did not use the item update route');
    assert.deepEqual(seriesUpdatePost.body, {
      Id: INITIAL_SERIES_TIMER_ID, Name: 'Series <img id="series-injected" src=x>', ChannelId: CHANNEL_ID,
      ProgramId: PROGRAM_ID, StartDate: SERIES_START, EndDate: SERIES_END, PrePaddingSeconds: 90,
      PostPaddingSeconds: 45, OutputLibraryId: LIBRARY_ID, RecordAnyTime: true, RecordAnyChannel: false,
      SkipEpisodesInLibrary: false, RecordNewOnly: false, KeepUpTo: 0, KeepUntil: 'UntilDeleted', Priority: 0,
      Days: ['Monday', 'Wednesday', 'Friday'],
    }, 'series update should preserve its custom days and supported settings with PascalCase fields');
    assert.equal(await cdp.evaluate(manager.sessionId, 'document.querySelector("#series-injected") === null'), true, 'updated series timer text was inserted as markup');

    const gate = holdNextGuide(CHANNEL_ID);
    await cdp.evaluate(manager.sessionId, 'document.querySelector("[data-live-tv-channel=\\"' + SECOND_CHANNEL_ID + '\\"]").click(); true');
    await cdp.waitFor(manager, 'document.querySelector(".live-tv-program .data-row-main strong")?.textContent', (value) => value === 'Second synthetic programme', 'the second channel guide');
    await cdp.evaluate(manager.sessionId, 'document.querySelector("[data-live-tv-channel=\\"' + CHANNEL_ID + '\\"]").click(); true');
    await gate.started;
    await cdp.evaluate(manager.sessionId, 'document.querySelector("[data-live-tv-channel=\\"' + SECOND_CHANNEL_ID + '\\"]").click(); true');
    await cdp.waitFor(manager, 'document.querySelector(".live-tv-program .data-row-main strong")?.textContent', (value) => value === 'Second synthetic programme', 'the newer channel guide response');
    gate.release();
    await new Promise((resolve) => setTimeout(resolve, 150));
    assert.equal(await cdp.evaluate(manager.sessionId, 'document.querySelector(".live-tv-program .data-row-main strong")?.textContent'), 'Second synthetic programme', 'a slower earlier guide response replaced the selected channel guide');
    assert.equal(await cdp.evaluate(manager.sessionId, 'document.querySelector(".live-tv-channel.selected")?.dataset.liveTvChannel'), SECOND_CHANNEL_ID, 'the selected channel changed after an earlier guide response');

    failNextGuideFor = SECOND_CHANNEL_ID;
    await cdp.evaluate(manager.sessionId, 'document.querySelector("[data-live-tv-channel=\\"' + SECOND_CHANNEL_ID + '\\"]").click(); true');
    await cdp.waitFor(manager, 'document.querySelector(".live-tv-grid .live-tv-error") !== null', Boolean, 'the guide error state');
    assert.equal(await cdp.evaluate(manager.sessionId, 'document.querySelector("#error-injected") !== null'), false, 'guide errors inserted server text as markup');
    assert.ok(await cdp.evaluate(manager.sessionId, 'document.querySelector(".live-tv-grid").textContent.includes(\'Guide request <img id="error-injected" src=x>\')'), 'guide error details were not shown as text');
    await cdp.evaluate(manager.sessionId, 'document.querySelector(".live-tv-grid [data-live-tv-refresh]").click(); true');
    await cdp.waitFor(manager, 'window.__liveTvPending === 0 && !document.querySelector(".live-tv-refreshing") && document.querySelector("[data-live-tv-channel=\\"' + CHANNEL_ID + '\\"]") !== null && document.querySelector(".live-tv-program .data-row-main strong")?.textContent === "Second synthetic programme"', Boolean, 'the completed guide retry and refreshed channel controls');

    await cdp.evaluate(manager.sessionId, 'document.querySelector("[data-live-tv-channel=\\"' + CHANNEL_ID + '\\"]").click(); true');
    await cdp.waitFor(manager, 'document.querySelector(".live-tv-program .data-row-main strong")?.textContent', (value) => value === PROGRAM_NAME, 'the first channel guide again');
    await cdp.evaluate(manager.sessionId, 'document.querySelector("[data-live-tv-record-program=\\"' + PROGRAM_ID + '\\"]").click(); true');
    await cdp.waitFor(manager, 'document.querySelectorAll(".live-tv-timer-row").length >= 3', Boolean, 'the guide timer to appear');
    const programTimerPost = apiRequests.find((entry) => entry.method === 'POST' && entry.pathname === '/LiveTv/Timers' && entry.body?.ProgramId === PROGRAM_ID);
    assert.ok(programTimerPost, 'record once did not use the Live TV timer collection route');
    assert.deepEqual(programTimerPost.body, { ChannelId: CHANNEL_ID, ProgramId: PROGRAM_ID, PrePaddingSeconds: 0, PostPaddingSeconds: 0 }, 'programme timer payload should use the API PascalCase fields');
    await cdp.evaluate(manager.sessionId, 'document.querySelector("[data-live-tv-cancel-timer=\\"' + programTimerPost.createdId + '\\"]").click(); true');
    await cdp.waitFor(manager, 'document.querySelector("[data-live-tv-cancel-timer=\\"' + programTimerPost.createdId + '\\"]") === null', Boolean, 'the programme timer cancellation');
    assert.ok(apiRequests.some((entry) => entry.method === 'DELETE' && entry.pathname === '/LiveTv/Timers/' + programTimerPost.createdId), 'timer cancellation used an unsupported route');

    await cdp.waitFor(manager, 'window.__liveTvPending === 0 && document.querySelector("[data-live-tv-record-series=\\"' + PROGRAM_ID + '\\"]") !== null', Boolean, 'the refreshed programme controls after timer cancellation');
    await cdp.evaluate(manager.sessionId, '(() => { const row = document.querySelector("[data-live-tv-record-series=\\"' + PROGRAM_ID + '\\"]").closest(".live-tv-program"); row.querySelector("[name=DayPattern]").value = "Weekdays"; row.querySelector("[data-live-tv-record-series]").click(); return true; })()');
    await cdp.waitFor(manager, 'document.querySelectorAll(".live-tv-series-row").length === 2', Boolean, 'the series timer to appear');
    const seriesTimerPost = apiRequests.find((entry) => entry.method === 'POST' && entry.pathname === '/LiveTv/SeriesTimers');
    assert.ok(seriesTimerPost, 'record series did not use the Live TV series timer collection route');
    assert.deepEqual(seriesTimerPost.body, {
      ChannelId: CHANNEL_ID, ProgramId: PROGRAM_ID, DayPattern: 'Weekdays', PrePaddingSeconds: 0, PostPaddingSeconds: 0,
      RecordAnyTime: true, RecordAnyChannel: false, SkipEpisodesInLibrary: false, RecordNewOnly: false,
      KeepUpTo: 0, KeepUntil: 'UntilDeleted', Priority: 0,
    }, 'series timer payload should use only supported PascalCase fields and values');
    await cdp.evaluate(manager.sessionId, 'document.querySelector("[data-live-tv-cancel-series=\\"' + seriesTimerPost.createdId + '\\"]").click(); true');
    await cdp.waitFor(manager, 'document.querySelector("[data-live-tv-cancel-series=\\"' + seriesTimerPost.createdId + '\\"]") === null', Boolean, 'the series timer cancellation');
    assert.ok(apiRequests.some((entry) => entry.method === 'DELETE' && entry.pathname === '/LiveTv/SeriesTimers/' + seriesTimerPost.createdId), 'series cancellation used an unsupported route');

    await cdp.waitFor(manager, 'window.__liveTvPending === 0 && document.querySelector("#live-tv-manual-timer-form select[name=ChannelId]")?.disabled === false && Array.from(document.querySelectorAll("#live-tv-manual-timer-form select[name=ChannelId] option"), (option) => option.value).includes("' + CHANNEL_ID + '")', Boolean, 'the refreshed manual timer channels after series cancellation');
    await cdp.evaluate(manager.sessionId, '(() => { const form = document.querySelector("#live-tv-manual-timer-form"); const local = (date) => new Date(date.getTime() - date.getTimezoneOffset() * 60000).toISOString().slice(0, 16); const start = new Date(Date.now() + 2 * 60 * 60 * 1000); const end = new Date(start.getTime() + 45 * 60 * 1000); form.elements.Name.value = "Synthetic manual timer"; form.elements.ChannelId.value = "' + CHANNEL_ID + '"; form.elements.StartDate.value = local(start); form.elements.EndDate.value = local(end); form.requestSubmit(); return true; })()');
    await cdp.waitFor(manager, 'Array.from(document.querySelectorAll(".live-tv-timer-row strong"), (node) => node.textContent).includes("Synthetic manual timer")', Boolean, 'the manual timer to appear');
    const manualTimerPost = apiRequests.find((entry) => entry.method === 'POST' && entry.pathname === '/LiveTv/Timers' && entry.body?.Name === 'Synthetic manual timer');
    assert.ok(manualTimerPost, 'manual timer form did not post');
    assert.equal(manualTimerPost.body.ChannelId, CHANNEL_ID);
    assert.match(manualTimerPost.body.StartDate, /^\d{4}-\d{2}-\d{2}T/);
    assert.match(manualTimerPost.body.EndDate, /^\d{4}-\d{2}-\d{2}T/);
    await cdp.evaluate(manager.sessionId, 'document.querySelector("[data-live-tv-cancel-timer=\\"' + manualTimerPost.createdId + '\\"]").click(); true');
    await cdp.waitFor(manager, 'document.querySelector("[data-live-tv-cancel-timer=\\"' + manualTimerPost.createdId + '\\"]") === null', Boolean, 'the manual timer cancellation');

    const viewer = await cdp.openPage(baseUrl + 'viewer', browserTestScript());
    pages.push(viewer);
    await cdp.waitFor(viewer, 'document.querySelector("[data-screen=\\"live-tv\\"]") !== null', Boolean, 'the view-only Live TV navigation');
    await cdp.evaluate(viewer.sessionId, 'document.querySelector("[data-screen=\\"live-tv\\"]").click(); true');
    await cdp.waitFor(viewer, 'document.querySelector(".live-tv-program") !== null', Boolean, 'the view-only guide');
    assert.equal(await cdp.evaluate(viewer.sessionId, 'document.querySelector("[data-live-tv-record-program],[data-live-tv-record-series],#live-tv-manual-timer-form,[data-live-tv-edit-timer],[data-live-tv-edit-series]") !== null'), false, 'view-only account saw timer mutation controls');
    assert.equal(await cdp.evaluate(viewer.sessionId, 'document.querySelector("[data-live-tv-cancel-timer],[data-live-tv-cancel-series]") !== null'), false, 'view-only account saw cancellation controls');
    assert.equal(apiRequests.some((entry) => entry.profile === 'viewer' && entry.pathname.startsWith('/Admin/LiveTv/')), false, 'view-only account called an administrator route');

    const empty = await cdp.openPage(baseUrl + 'empty', browserTestScript());
    pages.push(empty);
    await cdp.waitFor(empty, 'document.querySelector("[data-screen=\\"live-tv\\"]") !== null', Boolean, 'the empty Live TV account navigation');
    await cdp.evaluate(empty.sessionId, 'document.querySelector("[data-screen=\\"live-tv\\"]").click(); true');
    await cdp.waitFor(empty, 'document.body.textContent.includes("No channels available") && document.body.textContent.includes("No upcoming timers") && document.body.textContent.includes("No recordings yet")', Boolean, 'Live TV empty states');

    const noAccess = await cdp.openPage(baseUrl + 'noaccess', browserTestScript());
    pages.push(noAccess);
    await cdp.waitFor(noAccess, 'document.querySelector("#global-search") !== null', Boolean, 'the account without Live TV access');
    assert.equal(await cdp.evaluate(noAccess.sessionId, 'document.querySelector("[data-screen=\\"live-tv\\"]") !== null'), false, 'Live TV navigation was shown without access');
    assert.equal(apiRequests.some((entry) => entry.profile === 'noaccess' && entry.pathname.startsWith('/LiveTv/')), false, 'account without access called a Live TV route');

    const admin = await cdp.openPage(baseUrl + 'admin', browserTestScript());
    pages.push(admin);
    await cdp.waitFor(admin, 'document.querySelector("[data-screen=\\"live-tv\\"]") !== null', Boolean, 'the administrator Live TV navigation');
    await cdp.evaluate(admin.sessionId, 'document.querySelector("[data-screen=\\"live-tv\\"]").click(); true');
    await cdp.waitFor(admin, 'document.querySelector("#live-tv-source-form") !== null && document.querySelector("[data-live-tv-refresh-source]") !== null', Boolean, 'administrator source controls');
    assert.equal(await cdp.evaluate(admin.sessionId, 'document.querySelector("#source-injected") !== null'), false, 'source name was inserted as markup');
    await cdp.evaluate(admin.sessionId, 'document.querySelector("[data-live-tv-refresh-source=\\"' + SOURCE_ID + '\\"]").click(); true');
    await cdp.waitFor(admin, 'window.__liveTvPending === 0', Boolean, 'source refresh completion');
    assert.ok(apiRequests.some((entry) => entry.profile === 'admin' && entry.method === 'POST' && entry.pathname === '/Admin/LiveTv/Sources/' + SOURCE_ID + '/Refresh'), 'administrator source refresh did not use the supported route');
    await cdp.evaluate(admin.sessionId, '(() => { const form = document.querySelector("#live-tv-source-form"); form.elements.Name.value = "Synthetic M3U source"; form.elements.LibraryId.value = "' + LIBRARY_ID + '"; form.elements.PlaylistUrl.value = "https://tv.example/channels.m3u"; form.elements.GuideUrl.value = "https://tv.example/guide.xml"; form.elements.Origin.value = "https://tv.example/"; form.elements.Addresses.value = "203.0.113.12\\n2001:db8::12"; form.requestSubmit(); return true; })()');
    await cdp.waitFor(admin, 'Array.from(document.querySelectorAll(".data-row strong"), (node) => node.textContent).includes("Synthetic M3U source")', Boolean, 'the created IPTV source');
    await cdp.waitFor(admin, 'window.__liveTvPending === 0', Boolean, 'source creation and reload');
    const sourcePost = apiRequests.find((entry) => entry.profile === 'admin' && entry.method === 'POST' && entry.pathname === '/Admin/LiveTv/Sources');
    assert.ok(sourcePost, 'administrator source form did not post');
    assert.deepEqual(sourcePost.body, {
      LibraryId: LIBRARY_ID, Name: 'Synthetic M3U source', PlaylistUrl: 'https://tv.example/channels.m3u',
      GuideUrl: 'https://tv.example/guide.xml',
      OriginPins: [{ origin: 'https://tv.example/', addresses: ['203.0.113.12', '2001:db8::12'] }],
    }, 'source payload should use the API casing and understood origin pin shape');
    const newSourceId = sourcePost.createdId;
    await cdp.evaluate(admin.sessionId, 'document.querySelector("[data-live-tv-refresh-source=\\"' + newSourceId + '\\"]").click(); true');
    await cdp.waitFor(admin, 'window.__liveTvPending === 0', Boolean, 'new source refresh completion');
    assert.ok(apiRequests.some((entry) => entry.profile === 'admin' && entry.method === 'POST' && entry.pathname === '/Admin/LiveTv/Sources/' + newSourceId + '/Refresh'), 'new source could not be refreshed');

    await cdp.evaluate(admin.sessionId, 'document.querySelector("[data-live-tv-edit-source=\\"' + newSourceId + '\\"]").click(); true');
    await cdp.waitFor(admin, 'document.querySelector("#live-tv-source-edit-form") !== null', Boolean, 'source edit form');
    assert.deepEqual(await cdp.evaluate(admin.sessionId, '(() => { const form = document.querySelector("#live-tv-source-edit-form"); return { playlist: form.elements.PlaylistUrl.value, guide: form.elements.GuideUrl.value, origin: form.elements.Origin.value, addresses: form.elements.Addresses.value }; })()'), {
      playlist: '', guide: '', origin: '', addresses: '',
    }, 'the source edit form must not prefill stored feed URLs or origin pins');
    await cdp.evaluate(admin.sessionId, '(() => { const form = document.querySelector("#live-tv-source-edit-form"); form.elements.Name.value = "Renamed IPTV source"; form.requestSubmit(); return true; })()');
    await cdp.waitFor(admin, 'Array.from(document.querySelectorAll(".data-row strong"), (node) => node.textContent).includes("Renamed IPTV source")', Boolean, 'source rename');
    await cdp.waitFor(admin, 'window.__liveTvPending === 0', Boolean, 'source rename and reload');
    const sourceRename = apiRequests.find((entry) => entry.profile === 'admin' && entry.method === 'POST' && entry.pathname === '/Admin/LiveTv/Sources/' + newSourceId);
    assert.ok(sourceRename, 'administrator source edit did not post');
    assert.deepEqual(sourceRename.body, { LibraryId: LIBRARY_ID, Name: 'Renamed IPTV source' }, 'a name-only source edit should omit all stored secrets');
    assert.deepEqual(sourceRename.configAfter, {
      PlaylistUrl: 'https://tv.example/channels.m3u',
      GuideUrl: 'https://tv.example/guide.xml',
      OriginPins: [{ origin: 'https://tv.example/', addresses: ['203.0.113.12', '2001:db8::12'] }],
    }, 'omitted settings should remain stored after a source rename');

    await cdp.evaluate(admin.sessionId, 'document.querySelector("[data-live-tv-edit-source=\\"' + newSourceId + '\\"]").click(); true');
    await cdp.waitFor(admin, 'document.querySelector("#live-tv-source-edit-form") !== null', Boolean, 'source edit form for guide clearing');
    await cdp.evaluate(admin.sessionId, '(() => { const form = document.querySelector("#live-tv-source-edit-form"); form.elements.ClearGuideUrl.checked = true; form.requestSubmit(); return true; })()');
    await cdp.waitFor(admin, 'window.__liveTvPending === 0', Boolean, 'source guide clear and reload');
    const sourceGuideClear = apiRequests.find((entry) => entry.profile === 'admin' && entry.method === 'POST' && entry.pathname === '/Admin/LiveTv/Sources/' + newSourceId && entry.body?.GuideUrl === null);
    assert.ok(sourceGuideClear, 'clearing the guide did not send an explicit null value');
    assert.deepEqual(sourceGuideClear.configAfter, {
      PlaylistUrl: 'https://tv.example/channels.m3u',
      OriginPins: [{ origin: 'https://tv.example/', addresses: ['203.0.113.12', '2001:db8::12'] }],
    }, 'clearing the guide should preserve the playlist URL and origin pins');

    await cdp.evaluate(admin.sessionId, 'document.querySelector("[data-live-tv-delete-source=\\"' + newSourceId + '\\"]").click(); true');
    await cdp.waitFor(admin, 'Array.from(document.querySelectorAll(".data-row strong"), (node) => node.textContent).includes("Renamed IPTV source") === false', Boolean, 'source deletion');
    await cdp.waitFor(admin, 'window.__liveTvPending === 0', Boolean, 'source deletion and reload');
    assert.ok(apiRequests.some((entry) => entry.profile === 'admin' && entry.method === 'DELETE' && entry.pathname === '/Admin/LiveTv/Sources/' + newSourceId), 'administrator source delete did not use its route');

    const liveTvRequests = apiRequests.filter((entry) => entry.pathname.startsWith('/LiveTv/') || entry.pathname.startsWith('/Admin/LiveTv/'));
    assert.ok(liveTvRequests.every((entry) => entry.pathname !== '/LiveTv/TunerHosts'), 'the UI requested tuner data it does not use');
    const noPlayback = await cdp.openPage(baseUrl + 'noPlayback', browserTestScript());
    pages.push(noPlayback);
    await cdp.waitFor(noPlayback, 'document.querySelector("[data-screen=\\"live-tv\\"]") !== null', Boolean, 'the playback-restricted Live TV navigation');
    await cdp.evaluate(noPlayback.sessionId, 'document.querySelector("[data-screen=\\"live-tv\\"]").click(); true');
    await cdp.waitFor(noPlayback, 'document.querySelector(".live-tv-channel") !== null', Boolean, 'channels for the playback-restricted account');
    assert.equal(await cdp.evaluate(noPlayback.sessionId, 'document.querySelector("[data-live-tv-play]") !== null'), false, 'the playback-restricted account saw Watch live');
    assert.equal(apiRequests.some((entry) => entry.profile === 'noPlayback' && entry.pathname.endsWith('/PlaybackInfo')), false, 'the playback-restricted account sent a playback negotiation request');
    process.stdout.write('Live TV browser checks passed: access gating, escaped provider text, per-channel guide race, bounded live HLS handoff and cleanup, playback policy, timer workflows and edits, recordings, and administrator source create/list/refresh/update/delete.\n');
    for (const page of pages) await cdp.send('Target.closeTarget', { targetId: page.targetId });
  } catch (error) {
    const routes = apiRequests.map((item) => item.profile + ' ' + item.method + ' ' + item.pathname + item.search).join('\n');
    throw new Error(error.message + '\\nAPI requests:\\n' + routes + '\\nChrome output:\\n' + chromeOutput.trim().slice(-2000), { cause: error });
  } finally {
    socket?.close();
    chrome.kill();
    await Promise.race([once(chrome, 'exit').catch(() => {}), new Promise((resolve) => setTimeout(resolve, 3000))]);
    const closed = new Promise((resolve) => server.close(() => resolve()));
    server.closeAllConnections();
    await closed;
    for (let attempt = 0; attempt < 10; attempt += 1) {
      try { fs.rmSync(profilePath, { recursive: true, force: true }); break; }
      catch (error) { if (error.code !== 'EPERM' || attempt === 9) break; await new Promise((resolve) => setTimeout(resolve, 150)); }
    }
  }
}

main().catch((error) => {
  console.error(error);
  if (process.env.GITHUB_ACTIONS === 'true') {
    const detail = String(error.stack || error).slice(0, 10000).replace(/%/g, '%25').replace(/\r/g, '%0D').replace(/\n/g, '%0A');
    console.log('::error title=Live TV browser checks::' + detail);
  }
  process.exitCode = 1;
});
