/* Original offline delivery worker for Puffinbox. */
importScripts('/web/offline-cache.js');

const SHELL_CACHE = 'puffinbox-shell-v1';
const SHELL_URLS = [
  '/web/index.html', '/web/styles.css', '/web/client-compat.js', '/web/device-id.js',
  '/web/playback-heartbeat.js', '/web/jmp-player.js', '/web/offline-cache.js',
  '/web/app.js', '/web/vendor/hls.min.js',
];
const CHUNK_SIZE = self.PuffinboxOfflineCache.CHUNK_SIZE;

self.addEventListener('install', (event) => {
  event.waitUntil((async () => {
    const cache = await caches.open(SHELL_CACHE);
    await Promise.all(SHELL_URLS.map(async (url) => {
      try {
        const response = await fetch(url, { cache: 'reload' });
        if (response.ok) await cache.put(url, response);
      } catch (_) { /* The currently open page can still install the worker offline. */ }
    }));
    await self.skipWaiting();
  })());
});

self.addEventListener('activate', (event) => {
  event.waitUntil((async () => {
    const keys = await caches.keys();
    await Promise.all(keys.filter((key) => key.startsWith('puffinbox-shell-') && key !== SHELL_CACHE).map((key) => caches.delete(key)));
    await self.clients.claim();
  })());
});

self.addEventListener('fetch', (event) => {
  const request = event.request;
  if (request.method !== 'GET' && request.method !== 'HEAD') return;
  const url = new URL(request.url);
  if (url.origin !== self.location.origin || !url.pathname.startsWith('/web/')) return;
  if (url.pathname.startsWith('/web/__offline/')) {
    event.respondWith(serveOffline(request, url.pathname));
    return;
  }
  if (request.mode === 'navigate' && (url.pathname === '/web/' || url.pathname === '/web/index.html')) {
    event.respondWith(networkFirstShell(request));
    return;
  }
  if (SHELL_URLS.includes(url.pathname)) event.respondWith(cacheFirstAsset(request));
});

async function networkFirstShell(request) {
  const cache = await caches.open(SHELL_CACHE);
  try {
    const response = await fetch(request);
    if (response.ok) await cache.put('/web/index.html', response.clone());
    return response;
  } catch (_) {
    return (await cache.match('/web/index.html')) || new Response('Puffinbox is not available offline yet.', { status: 503, headers: { 'Content-Type': 'text/plain; charset=utf-8' } });
  }
}

async function cacheFirstAsset(request) {
  const cache = await caches.open(SHELL_CACHE);
  try {
    const response = await fetch(request);
    if (response.ok) await cache.put(request, response.clone());
    return response;
  } catch (_) {
    const cached = await cache.match(request, { ignoreSearch: true });
    if (cached) return cached;
    return new Response('This Puffinbox screen asset is not stored in this browser.', { status: 503, headers: { 'Content-Type': 'text/plain; charset=utf-8' } });
  }
}

async function serveOffline(request, pathname) {
  const parts = pathname.split('/').filter(Boolean);
  if (parts.length !== 4 || parts[0] !== 'web' || parts[1] !== '__offline') return new Response('Not found', { status: 404 });
  let accountId;
  let urlToken;
  try { accountId = decodeURIComponent(parts[2]); urlToken = decodeURIComponent(parts[3]); }
  catch (_) { return new Response('Not found', { status: 404 }); }
  if (!/^[0-9a-f-]{16,64}$/i.test(accountId) || !/^[0-9a-f]{32}$/i.test(urlToken)) return new Response('Not found', { status: 404 });
  let entry;
  try { entry = await self.PuffinboxOfflineCache.getPackageByToken(accountId, urlToken); }
  catch (_) { return new Response('Offline storage could not be read.', { status: 503 }); }
  if (!entry) return new Response('This offline copy is unavailable for the active account.', { status: 404 });

  const size = entry.sourceSize;
  if (!Number.isSafeInteger(size) || size < 0 || size > self.PuffinboxOfflineCache.MAX_ITEM_SIZE) return new Response('Invalid offline item.', { status: 500 });
  const etag = `"${entry.sha256}"`;
  const requestedRange = request.headers.get('Range');
  const ifRange = request.headers.get('If-Range');
  const useRange = requestedRange && (!ifRange || ifRange === etag);
  const range = useRange ? parseRange(requestedRange, size) : null;
  if (useRange && !range) {
    return new Response(null, { status: 416, headers: { 'Content-Range': `bytes */${size}`, 'Accept-Ranges': 'bytes', ETag: etag } });
  }
  const start = range ? range.start : 0;
  const end = range ? range.end : Math.max(0, size - 1);
  const partial = !!range;
  const mime = safeContentType(entry.contentType, entry.itemType, entry.itemName);
  const headers = new Headers({
    'Accept-Ranges': 'bytes', 'Cache-Control': 'private, no-store',
    'Content-Length': String(size === 0 ? 0 : end - start + 1),
    'Content-Type': mime, ETag: etag,
    'X-Content-Type-Options': 'nosniff',
    'Content-Disposition': mime === 'application/octet-stream' ? `attachment; filename="${safeFilename(entry.fileName || entry.itemName)}"` : 'inline',
  });
  if (partial) headers.set('Content-Range', `bytes ${start}-${end}/${size}`);
  if (request.method === 'HEAD' || size === 0) return new Response(null, { status: partial ? 206 : 200, headers });
  if (typeof ReadableStream === 'undefined') return new Response('Streaming offline files is unavailable in this browser.', { status: 501 });
  return new Response(streamChunks(entry, start, end), { status: partial ? 206 : 200, headers });
}

function parseRange(value, size) {
  if (!Number.isSafeInteger(size) || size === 0 || !value.startsWith('bytes=') || value.includes(',')) return null;
  const match = /^bytes=(\d*)-(\d*)$/.exec(value.trim());
  if (!match) return null;
  let start;
  let end;
  if (match[1] === '') {
    const suffix = Number(match[2]);
    if (!Number.isSafeInteger(suffix) || suffix <= 0) return null;
    start = Math.max(0, size - suffix);
    end = size - 1;
  } else {
    start = Number(match[1]);
    end = match[2] === '' ? size - 1 : Number(match[2]);
    if (!Number.isSafeInteger(start) || !Number.isSafeInteger(end) || start > end || start >= size) return null;
    end = Math.min(end, size - 1);
  }
  return { start, end };
}

function streamChunks(entry, start, end) {
  let position = start;
  return new ReadableStream({
    async pull(controller) {
      if (position > end) { controller.close(); return; }
      const index = Math.floor(position / entry.chunkSize);
      try {
        const row = await self.PuffinboxOfflineCache.getChunkForActiveAccount(
          entry.accountId, entry.packageId, index, entry.accountGeneration, entry.transferToken,
        );
        if (!row) throw new Error('Offline chunk missing.');
        const bytes = new Uint8Array(row.bytes);
        if (new self.PuffinboxOfflineCache.Sha256().update(bytes).digestHex() !== row.digest) throw new Error('Offline chunk failed its integrity check.');
        const chunkStart = index * entry.chunkSize;
        const from = position - chunkStart;
        const to = Math.min(bytes.byteLength, end - chunkStart + 1);
        if (from < 0 || to <= from) throw new Error('Offline chunk bounds are invalid.');
        const output = bytes.subarray(from, to).slice();
        position += output.byteLength;
        controller.enqueue(output);
      } catch (_) { controller.error(new Error('A local offline chunk is missing or failed its integrity check.')); }
    },
  });
}

function safeContentType(contentType, itemType, name) {
  const value = String(contentType || '').split(';', 1)[0].trim().toLowerCase();
  const extension = String(name || '').split('.').pop().toLowerCase();
  const safeInline = new Set([
    'video/mp4', 'video/webm', 'video/x-matroska', 'video/quicktime',
    'audio/mpeg', 'audio/mp4', 'audio/aac', 'audio/flac', 'audio/wav', 'audio/ogg', 'audio/webm',
    'image/jpeg', 'image/png', 'image/gif', 'image/webp', 'image/avif', 'image/bmp',
  ]);
  if (safeInline.has(value)) return value;
  const byExtension = {
    mp4: 'video/mp4', m4v: 'video/mp4', webm: 'video/webm', mkv: 'video/x-matroska', mov: 'video/quicktime',
    mp3: 'audio/mpeg', m4a: 'audio/mp4', aac: 'audio/aac', flac: 'audio/flac', wav: 'audio/wav', ogg: 'audio/ogg', opus: 'audio/ogg',
    jpg: 'image/jpeg', jpeg: 'image/jpeg', png: 'image/png', gif: 'image/gif', webp: 'image/webp', avif: 'image/avif', bmp: 'image/bmp',
  };
  const inferred = byExtension[extension];
  if (inferred) return inferred;
  if (['Book', 'EBook', 'AudioBook'].includes(itemType)) return 'application/octet-stream';
  return 'application/octet-stream';
}

function safeFilename(value) {
  const name = String(value || 'offline-item').replace(/[\r\n"\\/]/g, '_').replace(/[^\x20-\x7e]/g, '_').slice(0, 120);
  return name || 'offline-item';
}
