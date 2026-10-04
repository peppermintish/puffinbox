#!/usr/bin/env node
'use strict';

// This browser and its HTTP fixture server belong only to this test process.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const http = require('node:http');
const net = require('node:net');
const os = require('node:os');
const path = require('node:path');
const { spawn, spawnSync } = require('node:child_process');
const { once } = require('node:events');

const ROOT = path.resolve(__dirname, '..');
const FIXTURES = ['foxit-symbol', 'japanese-cmap', 'jpeg2000', 'jbig2-mmr'];
const BOOKS = [...FIXTURES, 'slow-foxit'];
const requests = [];
const results = [];
const policy = fs.readFileSync(path.join(ROOT, 'src/media_features/books.rs'), 'utf8')
  .match(/default-src 'none'; script-src[^"\n]+/)[0];

function createServer() {
  return http.createServer((request, response) => {
    const url = new URL(request.url, 'http://localhost');
    requests.push(url.pathname);
    response.setHeader('Cache-Control', 'no-store');
    if (url.pathname.startsWith('/reader/')) {
      const name = url.pathname.slice('/reader/'.length);
      if (!BOOKS.includes(name)) { response.writeHead(404); response.end(); return; }
      response.setHeader('Content-Security-Policy', policy);
      response.setHeader('Content-Type', 'text/html; charset=utf-8');
      response.end(`<!doctype html><html><head><script src="/web/book-reader.js" defer></script></head>
        <body><main id="book-reader" data-item-id="${name}" data-format="pdf" data-book-title="${name}">
        <h1 id="book-title"></h1><p id="reader-status"></p><a id="download-link"></a>
        <section id="pdf-panel" hidden><button id="previous-page">Previous</button>
        <span id="pdf-page-label"></span><button id="next-page">Next</button><canvas id="pdf-canvas"></canvas></section>
        <p id="reader-error" hidden></p></main></body></html>`);
      return;
    }
    let file;
    const document = url.pathname.match(/^\/Books\/([a-z0-9-]+)\/Document$/);
    if (document && BOOKS.includes(document[1])) {
      file = path.join(ROOT, 'tests/fixtures/books', (document[1] === 'slow-foxit' ? 'foxit-symbol' : document[1]) + '.pdf');
      response.setHeader('Content-Type', 'application/pdf');
    } else if (url.pathname === '/web/book-reader.js') {
      file = path.join(ROOT, 'web/book-reader.js');
      response.setHeader('Content-Type', 'text/javascript');
    } else if (url.pathname.startsWith('/web/vendor/pdfjs/')) {
      const relative = decodeURIComponent(url.pathname.slice('/web/vendor/pdfjs/'.length));
      const vendor = path.join(ROOT, 'web/vendor/pdfjs');
      file = path.resolve(vendor, relative);
      if (!file.startsWith(vendor + path.sep) || !fs.existsSync(file) || !fs.statSync(file).isFile()) {
        response.writeHead(404); response.end(); return;
      }
      response.setHeader('Content-Type', file.endsWith('.wasm') ? 'application/wasm'
        : /\.(mjs|js)$/.test(file) ? 'text/javascript' : 'application/octet-stream');
    } else { response.writeHead(404); response.end(); return; }
    const bytes = fs.readFileSync(file);
    response.setHeader('Content-Length', bytes.length);
    if (document?.[1] === 'slow-foxit') setTimeout(() => { if (!response.destroyed) response.end(bytes); }, 1500);
    else response.end(bytes);
  });
}

function findChrome() {
  const candidates = [process.env.CHROME_BIN, process.env.CHROME_PATH,
    ...(process.platform === 'win32' ? [
      'C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe',
      'C:\\Program Files (x86)\\Google\\Chrome\\Application\\chrome.exe',
      'C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe',
    ] : ['/usr/bin/google-chrome', '/usr/bin/chromium', '/usr/bin/chromium-browser'])].filter(Boolean);
  for (const candidate of candidates) {
    if (fs.existsSync(candidate)) return candidate;
    const found = spawnSync(process.platform === 'win32' ? 'where.exe' : 'which', [candidate], { encoding: 'utf8', windowsHide: true });
    if (found.status === 0 && found.stdout.trim()) return found.stdout.trim().split(/\r?\n/)[0];
  }
  throw new Error('Set CHROME_BIN to a Chrome or Chromium executable to run PDF rendering checks.');
}

async function freePort() {
  const server = net.createServer();
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  const port = server.address().port;
  await new Promise((resolve, reject) => server.close(error => error ? reject(error) : resolve()));
  return port;
}

class DevTools {
  constructor(socket) {
    this.socket = socket;
    this.nextId = 1;
    this.pending = new Map();
    this.diagnostics = [];
    this.exceptions = [];
    socket.addEventListener('message', event => {
      const message = JSON.parse(event.data);
      if (message.method === 'Runtime.consoleAPICalled') {
        this.diagnostics.push(message.params.args.map(argument => argument.value || argument.description).join(' '));
      }
      if (message.method === 'Runtime.exceptionThrown') this.exceptions.push(message.params.exceptionDetails.exception?.description || message.params.exceptionDetails.text);
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
        pending.reject(new Error('Test browser closed before the command completed.'));
      }
      this.pending.clear();
    });
  }
  send(method, params = {}, sessionId) {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      const timeout = setTimeout(() => { this.pending.delete(id); reject(new Error(`Browser command timed out: ${method}`)); }, 30000);
      this.pending.set(id, { resolve, reject, timeout });
      this.socket.send(JSON.stringify({ id, method, params, ...(sessionId ? { sessionId } : {}) }));
    });
  }
  async evaluate(sessionId, expression) {
    const response = await this.send('Runtime.evaluate', { expression, awaitPromise: true, returnByValue: true }, sessionId);
    if (response.exceptionDetails) throw new Error(response.exceptionDetails.exception?.description || response.exceptionDetails.text);
    return response.result?.value;
  }
  async open(url) {
    const { targetId } = await this.send('Target.createTarget', { url: 'about:blank' });
    const { sessionId } = await this.send('Target.attachToTarget', { targetId, flatten: true });
    await this.send('Page.enable', {}, sessionId);
    await this.send('Runtime.enable', {}, sessionId);
    await this.send('Page.addScriptToEvaluateOnNewDocument', { source: `
      window.module = {exports: {}};
      window.__pdfWorkers = [];
      const NativeWorker = window.Worker;
      window.Worker = class extends NativeWorker {
        constructor(...args) { super(...args); this.closed = false; window.__pdfWorkers.push(this); }
        terminate() { this.closed = true; return super.terminate(); }
      };` }, sessionId);
    await this.send('Page.navigate', { url }, sessionId);
    return { targetId, sessionId };
  }
  async ready(page) {
    const deadline = Date.now() + 20000;
    let last;
    while (Date.now() < deadline) {
      last = await this.evaluate(page.sessionId, `({status: document.querySelector('#reader-status')?.textContent,
        error: document.querySelector('#reader-error')?.hidden === false ? document.querySelector('#reader-error').textContent : null})`);
      if (last?.error) throw new Error(last.error);
      if (last?.status === 'PDF page 1 of 1') return;
      await new Promise(resolve => setTimeout(resolve, 100));
    }
    throw new Error(`PDF reader did not render its page: ${JSON.stringify(last)}`);
  }
  async closeReader(page) {
    const workers = await this.evaluate(page.sessionId, 'window.__pdfWorkers.filter(worker => !worker.closed).length');
    assert.equal(workers, 1, 'Reader must own one live PDF worker before closing');
    await this.evaluate(page.sessionId, "window.dispatchEvent(new PageTransitionEvent('pagehide')); true");
    const deadline = Date.now() + 10000;
    while (Date.now() < deadline) {
      if (await this.evaluate(page.sessionId, 'window.__pdfWorkers.every(worker => worker.closed)')) return;
      await new Promise(resolve => setTimeout(resolve, 100));
    }
    throw new Error('Reader left its real PDF worker running after pagehide');
  }
}

// Sample well inside each image region so interpolation at edges cannot hide a failed decoder.
const SAMPLE = `(() => {
  const canvas = document.querySelector('#pdf-canvas');
  const context = canvas.getContext('2d');
  const point = (x, y) => Array.from(context.getImageData(Math.floor(canvas.width*x), Math.floor(canvas.height*y), 1, 1).data);
  const rgba = context.getImageData(0, 0, canvas.width, canvas.height).data;
  let ink = 0;
  for (let i = 0; i < rgba.length; i += 4) if (rgba[i] < 200 || rgba[i+1] < 200 || rgba[i+2] < 200) ink++;
  return {width: canvas.width, height: canvas.height, ink,
    samples: [[.25,.25],[.75,.25],[.25,.75],[.75,.75],[.5,.5],[.1,.1]].map(([x,y]) => point(x,y))};
})()`;

function verifyPixels(name, sample) {
  if (name === 'jpeg2000') {
    const expected = [[220,20,20,255],[20,220,20,255],[20,20,220,255],[220,220,20,255]];
    expected.forEach((color, index) => assert.deepEqual(sample.samples[index], color, `JPEG2000 quadrant ${index + 1}`));
  } else if (name === 'jbig2-mmr') {
    assert.deepEqual(sample.samples[4], [0,0,0,255], `JBIG2 central square: ${JSON.stringify(sample)}`);
    assert.deepEqual(sample.samples[5], [255,255,255,255], 'JBIG2 background');
    assert.equal(sample.ink, 48 * 48, 'JBIG2 decoded black square area');
  } else assert.ok(sample.ink > 100, `${name} rendered a blank page`);
}

async function main() {
  const server = createServer();
  let chrome, socket, profile, cdp;
  let output = '';
  let launchError;
  const evidence = process.env.PUFFINBOX_PDF_BROWSER_EVIDENCE_DIR;
  if (evidence) fs.mkdirSync(evidence, { recursive: true });
  try {
    server.listen(0, '127.0.0.1');
    await once(server, 'listening');
    const debugPort = await freePort();
    profile = fs.mkdtempSync(path.join(os.tmpdir(), 'puffinbox-pdf-browser-'));
    chrome = spawn(findChrome(), ['--headless=new', '--disable-gpu', '--no-first-run', '--no-default-browser-check',
      '--disable-background-networking', `--remote-debugging-port=${debugPort}`, '--remote-allow-origins=*',
      `--user-data-dir=${profile}`, ...(process.platform === 'win32' ? [] : ['--no-sandbox']), 'about:blank'],
    { stdio: ['ignore','ignore','pipe'], windowsHide: true });
    chrome.stderr.on('data', chunk => { output += chunk; });
    chrome.on('error', error => { launchError = error; });
    let endpoint;
    for (let attempt = 0; attempt < 150; attempt++) {
      if (launchError) throw launchError;
      if (chrome.exitCode != null) throw new Error(`Test Chrome exited: ${output}`);
      try { const response = await fetch(`http://127.0.0.1:${debugPort}/json/version`); if (response.ok) endpoint = (await response.json()).webSocketDebuggerUrl; } catch (_) {}
      if (endpoint) break;
      await new Promise(resolve => setTimeout(resolve, 100));
    }
    if (!endpoint) throw new Error(`Chrome debugging endpoint unavailable: ${output}`);
    socket = new WebSocket(endpoint);
    await once(socket, 'open');
    cdp = new DevTools(socket);
    const base = `http://127.0.0.1:${server.address().port}`;
    for (const name of FIXTURES) {
      const offset = requests.length;
      const page = await cdp.open(`${base}/reader/${name}`);
      await cdp.ready(page);
      const sample = await cdp.evaluate(page.sessionId, SAMPLE);
      verifyPixels(name, sample);
      const text = await cdp.evaluate(page.sessionId, `(async () => {
        const pdfjs = await import('/web/vendor/pdfjs/pdf.min.mjs');
        const document = await pdfjs.getDocument(module.exports.createPdfLoadingOptions('/Books/${name}/Document')).promise;
        try { const page = await document.getPage(1); return (await page.getTextContent()).items.map(item => item.str).join(''); }
        finally { await document.loadingTask.destroy(); }
      })()`);
      if (name === 'japanese-cmap') {
        assert.equal(text, '日本語', 'CMaps must map the Japanese characters accurately');
        assert.ok(requests.slice(offset).some(url => url.endsWith('/90ms-RKSJ-H.bcmap')), 'Japanese encoding CMap was not fetched');
        assert.ok(requests.slice(offset).some(url => url.endsWith('/Adobe-Japan1-UCS2.bcmap')), 'Japanese Unicode CMap was not fetched');
      }
      if (name === 'foxit-symbol') assert.equal(text, 'αβχδε', 'Symbol glyph mapping');
      if (['jpeg2000','jbig2-mmr'].includes(name)) {
        const decoder = name === 'jpeg2000' ? 'openjpeg' : 'jbig2';
        assert.ok(requests.slice(offset).some(url => url.endsWith(`/${decoder}.wasm`)), `${decoder} WebAssembly was not fetched`);
      }
      results.push({name, mode: 'reader', text, ...sample, assets: requests.slice(offset).filter(url => url.startsWith('/web/vendor/pdfjs/'))});
      if (evidence) {
        const shot = await cdp.send('Page.captureScreenshot', { format: 'png' }, page.sessionId);
        fs.writeFileSync(path.join(evidence, `${name}.png`), Buffer.from(shot.data, 'base64'));
      }
      if (name !== 'japanese-cmap') {
        const fallbackOffset = requests.length;
        await cdp.evaluate(page.sessionId, `(async () => {
          const pdfjs = await import('/web/vendor/pdfjs/pdf.min.mjs');
          const options = module.exports.createPdfLoadingOptions('/Books/${name}/Document');
          options.useSystemFonts = false; options.useWasm = false;
          const document = await pdfjs.getDocument(options).promise;
          try {
            const page = await document.getPage(1), canvas = window.document.querySelector('#pdf-canvas');
            await page.render({canvas, canvasContext: canvas.getContext('2d'), viewport: page.getViewport({scale: 1.5}),
              annotationMode: pdfjs.AnnotationMode.DISABLE}).promise;
          } finally { await document.loadingTask.destroy(); }
        })()`);
        const fallback = await cdp.evaluate(page.sessionId, SAMPLE);
        verifyPixels(name, fallback);
        const asset = name === 'foxit-symbol' ? '/FoxitSymbol.pfb' : `/${name === 'jpeg2000' ? 'openjpeg' : 'jbig2'}_nowasm_fallback.js`;
        assert.ok(requests.slice(fallbackOffset).some(url => url.endsWith(asset)), `Missing forced fallback resource ${asset}`);
        results.push({name, mode: 'forced-fallback', ...fallback, assets: requests.slice(fallbackOffset).filter(url => url.startsWith('/web/vendor/pdfjs/'))});
      }
      await cdp.closeReader(page);
      results.push({name, mode: 'worker-cleanup', passed: true});
      await cdp.send('Target.closeTarget', { targetId: page.targetId });
    }
    const pendingPage = await cdp.open(`${base}/reader/slow-foxit`);
    const deadline = Date.now() + 10000;
    while (!requests.includes('/Books/slow-foxit/Document') && Date.now() < deadline) await new Promise(resolve => setTimeout(resolve, 50));
    assert.ok(requests.includes('/Books/slow-foxit/Document'), 'Pending PDF document request never started');
    assert.notEqual(await cdp.evaluate(pendingPage.sessionId, "document.querySelector('#reader-status').textContent"), 'PDF page 1 of 1', 'Slow fixture finished before pending-load cleanup was checked');
    await cdp.closeReader(pendingPage);
    results.push({name: 'slow-foxit', mode: 'pending-load-cleanup', passed: true});
    await cdp.send('Target.closeTarget', { targetId: pendingPage.targetId });
    assert.equal(requests.some(url => /liberation|quickjs/i.test(url)), false, 'Excluded font or scripting resource requested');
    assert.deepEqual(cdp.exceptions, [], 'PDF reader produced uncaught browser exceptions');
    console.log(JSON.stringify({passed: results.length, policy, results, diagnostics: cdp.diagnostics}, null, 2));
  } finally {
    if (evidence) fs.writeFileSync(path.join(evidence, 'results.json'), JSON.stringify({results, requests, diagnostics: cdp?.diagnostics || []}, null, 2) + '\n');
    if (socket) socket.close();
    if (chrome && chrome.exitCode == null) {
      const exited = once(chrome, 'exit');
      chrome.kill();
      await Promise.race([exited, new Promise(resolve => setTimeout(resolve, 5000))]);
    }
    server.closeAllConnections();
    await new Promise(resolve => server.close(resolve));
    if (profile && path.dirname(path.resolve(profile)) === path.resolve(os.tmpdir())
        && path.basename(profile).startsWith('puffinbox-pdf-browser-') && !fs.lstatSync(profile).isSymbolicLink()) {
      fs.rmSync(profile, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
    }
  }
}

main().catch(error => {
  console.error(error.stack || error);
  if (process.env.GITHUB_ACTIONS === 'true') {
    const message = String(error.stack || error).replaceAll('%', '%25').replaceAll('\r', '%0D').replaceAll('\n', '%0A');
    console.error(`::error title=PDF browser check::${message}`);
  }
  process.exitCode = 1;
});
