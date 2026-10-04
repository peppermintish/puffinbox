/* Puffinbox's deliberately small, non-executing PDF/EPUB reading surface. */
(function (global) {
  "use strict";

  const MAX_ARCHIVE_BYTES = 64 * 1024 * 1024;
  const MAX_ARCHIVE_FILES = 4096;
  const MAX_EXPANDED_BYTES = 128 * 1024 * 1024;
  const MAX_XML_BYTES = 2 * 1024 * 1024;
  const MAX_CHAPTER_BYTES = 4 * 1024 * 1024;
  const MAX_READER_BYTES = 32 * 1024 * 1024;
  const MAX_BOOK_XML_BYTES = MAX_READER_BYTES + 2 * MAX_XML_BYTES;
  const MAX_XML_MARKUP_TOKENS = 5000;
  const MAX_XML_ATTRIBUTES = 3000;
  const MAX_BOOK_XML_MARKUP_TOKENS = 30000;
  const MAX_BOOK_XML_ATTRIBUTES = 20000;
  const MAX_BOOK_XML_NODE_ESTIMATE = 80000;
  const MAX_CHAPTERS = 512;
  const MAX_CHAPTER_NODES = 20000;
  const MAX_PDF_PAGES = 10000;
  const ALLOWED_ELEMENTS = new Set([
    "address", "article", "aside", "b", "blockquote", "br", "caption", "cite",
    "code", "dd", "div", "dl", "dt", "em", "figcaption", "figure", "h1", "h2",
    "h3", "h4", "h5", "h6", "hr", "i", "kbd", "li", "main", "mark", "ol",
    "p", "pre", "q", "s", "section", "small", "span", "strong", "sub", "sup",
    "table", "tbody", "td", "th", "thead", "tr", "u", "ul"
  ]);
  const DROP_SUBTREE = new Set([
    "audio", "base", "button", "canvas", "embed", "form", "iframe", "img", "input",
    "link", "math", "meta", "object", "script", "select", "style", "svg", "textarea",
    "video"
  ]);

  function fail(message) {
    throw new Error(message);
  }

  function asBytes(value) {
    if (value instanceof Uint8Array) return value;
    if (value instanceof ArrayBuffer) return new Uint8Array(value);
    fail("The EPUB archive could not be read.");
  }

  function readU16(view, offset) {
    if (offset < 0 || offset + 2 > view.byteLength) fail("The EPUB archive is truncated.");
    return view.getUint16(offset, true);
  }

  function readU32(view, offset) {
    if (offset < 0 || offset + 4 > view.byteLength) fail("The EPUB archive is truncated.");
    return view.getUint32(offset, true);
  }

  function decodeZipName(bytes, flags) {
    try {
      return new TextDecoder("utf-8", { fatal: true }).decode(bytes);
    } catch (_error) {
      if ((flags & 0x0800) !== 0) fail("The EPUB contains an invalid file name.");
      // The EPUB package entries needed for reading use ASCII names in nearly
      // all books. Legacy names are decoded as Windows-1252 rather than guessed.
      return new TextDecoder("windows-1252", { fatal: false }).decode(bytes);
    }
  }

  function isSafeArchivePath(path) {
    if (typeof path !== "string" || path.length === 0 || path.length > 1024) return false;
    if (path.startsWith("/") || path.includes("\\") || path.includes("\0") || path.includes(":")) return false;
    if (/^[A-Za-z]:/.test(path) || /[\u0000-\u001f\u007f]/.test(path)) return false;
    const parts = path.split("/");
    if (parts[parts.length - 1] === "") parts.pop();
    return parts.length > 0 && parts.every((part) => part !== "" && part !== "." && part !== "..");
  }

  function findEocd(view) {
    const bytes = new Uint8Array(view.buffer, view.byteOffset, view.byteLength);
    const start = Math.max(0, bytes.length - 22 - 65535);
    for (let offset = bytes.length - 22; offset >= start; offset -= 1) {
      if (readU32(view, offset) === 0x06054b50) {
        const commentLength = readU16(view, offset + 20);
        if (offset + 22 + commentLength === bytes.length) return offset;
      }
    }
    fail("The EPUB archive directory is missing or malformed.");
  }

  function parseZipDirectory(value) {
    const bytes = asBytes(value);
    if (bytes.byteLength < 22 || bytes.byteLength > MAX_ARCHIVE_BYTES) fail("The EPUB file is outside the supported size limit.");
    const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    const eocd = findEocd(view);
    const diskNumber = readU16(view, eocd + 4);
    const directoryDisk = readU16(view, eocd + 6);
    const diskEntryCount = readU16(view, eocd + 8);
    const entryCount = readU16(view, eocd + 10);
    const directorySize = readU32(view, eocd + 12);
    const directoryOffset = readU32(view, eocd + 16);
    if (diskNumber !== 0 || directoryDisk !== 0 || diskEntryCount !== entryCount) fail("Multi-disk EPUB archives are not supported.");
    if (entryCount === 0xffff || directorySize === 0xffffffff || directoryOffset === 0xffffffff) fail("ZIP64 EPUB archives are not supported.");
    if (entryCount === 0 || entryCount > MAX_ARCHIVE_FILES) fail("The EPUB has too many archive entries.");
    if (directoryOffset + directorySize !== eocd) fail("The EPUB archive directory has unexpected trailing data.");

    const entries = new Map();
    let cursor = directoryOffset;
    let expandedTotal = 0;
    for (let index = 0; index < entryCount; index += 1) {
      if (readU32(view, cursor) !== 0x02014b50) fail("The EPUB archive directory is malformed.");
      const flags = readU16(view, cursor + 8);
      const method = readU16(view, cursor + 10);
      const crc = readU32(view, cursor + 16);
      const compressedSize = readU32(view, cursor + 20);
      const uncompressedSize = readU32(view, cursor + 24);
      const nameLength = readU16(view, cursor + 28);
      const extraLength = readU16(view, cursor + 30);
      const commentLength = readU16(view, cursor + 32);
      const startDisk = readU16(view, cursor + 34);
      const externalAttributes = readU32(view, cursor + 38);
      const localOffset = readU32(view, cursor + 42);
      const next = cursor + 46 + nameLength + extraLength + commentLength;
      if (next > eocd || nameLength === 0 || startDisk !== 0) fail("The EPUB archive directory is malformed.");
      if ((flags & 0x0001) !== 0 || (flags & 0x0040) !== 0) fail("Encrypted EPUB entries are not supported.");
      if (method !== 0 && method !== 8) fail("The EPUB uses an unsupported compression method.");
      if (compressedSize === 0xffffffff || uncompressedSize === 0xffffffff || localOffset === 0xffffffff) fail("ZIP64 EPUB entries are not supported.");
      if (compressedSize > MAX_ARCHIVE_BYTES || uncompressedSize > MAX_EXPANDED_BYTES) fail("An EPUB archive entry exceeds the supported size limit.");
      expandedTotal += uncompressedSize;
      if (expandedTotal > MAX_EXPANDED_BYTES) fail("The expanded EPUB exceeds the supported size limit.");

      const nameBytes = bytes.subarray(cursor + 46, cursor + 46 + nameLength);
      const name = decodeZipName(nameBytes, flags);
      const isDirectory = name.endsWith("/");
      if (!isSafeArchivePath(name)) fail("The EPUB contains an unsafe archive path.");
      const unixMode = externalAttributes >>> 16;
      if ((unixMode & 0xf000) === 0xa000) fail("Symbolic links are not allowed in EPUB archives.");
      if (entries.has(name)) fail("The EPUB contains duplicate archive paths.");
      entries.set(name, {
        name,
        isDirectory,
        flags,
        method,
        crc,
        compressedSize,
        uncompressedSize,
        localOffset,
        directoryOffset
      });
      cursor = next;
    }
    if (cursor !== eocd) fail("The EPUB archive directory has trailing entries.");
    return { bytes, view, entries };
  }

  function normalizeReference(reference, baseDirectory) {
    if (typeof reference !== "string" || reference.length === 0 || reference.length > 2048) return null;
    if (reference.includes("\\") || reference.startsWith("/") || reference.includes("?")) return null;
    const pathPart = reference.split("#", 1)[0];
    if (!pathPart) return null;
    let decoded;
    try {
      decoded = decodeURIComponent(pathPart);
    } catch (_error) {
      return null;
    }
    if (decoded.startsWith("/") || decoded.includes("\\") || decoded.includes(":")) return null;
    const parts = [];
    if (baseDirectory) parts.push(...baseDirectory.split("/"));
    for (const part of decoded.split("/")) {
      if (part === "" || part === "." || part === "..") return null;
      parts.push(part);
    }
    const resolved = parts.join("/");
    return isSafeArchivePath(resolved) ? resolved : null;
  }

  function crc32(bytes) {
    let crc = 0xffffffff;
    for (let index = 0; index < bytes.length; index += 1) {
      crc ^= bytes[index];
      for (let bit = 0; bit < 8; bit += 1) crc = (crc >>> 1) ^ ((crc & 1) ? 0xedb88320 : 0);
    }
    return (crc ^ 0xffffffff) >>> 0;
  }

  function concatBytes(chunks, total) {
    const result = new Uint8Array(total);
    let offset = 0;
    for (const chunk of chunks) {
      result.set(chunk, offset);
      offset += chunk.length;
    }
    return result;
  }

  async function inflateEntry(archive, entry, maximumBytes) {
    if (!entry || entry.isDirectory) fail("An EPUB reading file is missing.");
    if (entry.uncompressedSize > maximumBytes) fail("An EPUB reading file exceeds the supported size limit.");
    const { bytes, view } = archive;
    const local = entry.localOffset;
    if (local + 30 > bytes.length || readU32(view, local) !== 0x04034b50) fail("An EPUB file entry is malformed.");
    const flags = readU16(view, local + 6);
    const method = readU16(view, local + 8);
    const nameLength = readU16(view, local + 26);
    const extraLength = readU16(view, local + 28);
    const dataStart = local + 30 + nameLength + extraLength;
    const dataEnd = dataStart + entry.compressedSize;
    if (flags !== entry.flags || method !== entry.method || dataEnd > entry.directoryOffset) fail("An EPUB file entry is inconsistent.");
    const localName = decodeZipName(bytes.subarray(local + 30, local + 30 + nameLength), flags);
    if (localName !== entry.name) fail("An EPUB file entry has a mismatched name.");
    const compressed = bytes.subarray(dataStart, dataEnd);
    let result;
    if (entry.method === 0) {
      if (entry.compressedSize !== entry.uncompressedSize) fail("An EPUB stored entry has inconsistent lengths.");
      result = new Uint8Array(compressed);
    } else {
      if (typeof DecompressionStream !== "function") fail("This browser cannot safely decompress EPUB chapters.");
      const reader = new Blob([compressed]).stream().pipeThrough(new DecompressionStream("deflate-raw")).getReader();
      const chunks = [];
      let total = 0;
      try {
        while (true) {
          const part = await reader.read();
          if (part.done) break;
          total += part.value.byteLength;
          if (total > maximumBytes || total > entry.uncompressedSize) {
            await reader.cancel();
            fail("An expanded EPUB chapter exceeded its declared size.");
          }
          chunks.push(part.value);
        }
      } finally {
        reader.releaseLock();
      }
      result = concatBytes(chunks, total);
    }
    if (result.byteLength !== entry.uncompressedSize || crc32(result) !== entry.crc) fail("An EPUB file entry failed its integrity check.");
    return result;
  }

  function createXmlBudget() {
    return { bytes: 0, markupTokens: 0, attributes: 0, nodeEstimate: 0 };
  }

  function validateXmlComplexity(xml, byteLength, budget, chapter = false) {
    let markupTokens = 0;
    let attributes = 0;
    for (let index = 0; index < xml.length; index += 1) {
      if (xml[index] === "<") markupTokens += 1;
      else if (xml[index] === "=") attributes += 1;
      if (markupTokens > MAX_XML_MARKUP_TOKENS || attributes > MAX_XML_ATTRIBUTES) {
        fail("An EPUB XML document exceeds the safe markup limit.");
      }
    }
    // Bound element, text, and attribute nodes before DOMParser can allocate
    // them: each '<' starts at most one markup node and can split at most one
    // text node on either side; each '=' conservatively counts one attribute.
    const nodeEstimate = markupTokens * 2 + attributes + 1;
    if (chapter && nodeEstimate > MAX_CHAPTER_NODES) {
      fail("An EPUB chapter exceeds the safe document-node limit.");
    }
    budget.bytes += byteLength;
    budget.markupTokens += markupTokens;
    budget.attributes += attributes;
    budget.nodeEstimate += nodeEstimate;
    if (budget.bytes > MAX_BOOK_XML_BYTES
        || budget.markupTokens > MAX_BOOK_XML_MARKUP_TOKENS
        || budget.attributes > MAX_BOOK_XML_ATTRIBUTES
        || budget.nodeEstimate > MAX_BOOK_XML_NODE_ESTIMATE) {
      fail("The EPUB book exceeds the safe aggregate XML limit.");
    }
  }

  function parseXml(bytes, budget, chapter = false) {
    if (bytes.byteLength > MAX_XML_BYTES) fail("An EPUB XML document is too large.");
    const xml = new TextDecoder("utf-8", { fatal: true }).decode(bytes);
    if (/<!\s*(?:DOCTYPE|ENTITY)/i.test(xml)) fail("EPUB documents with custom entities are not supported.");
    validateXmlComplexity(xml, bytes.byteLength, budget, chapter);
    const parsed = new DOMParser().parseFromString(xml, "application/xml");
    if (parsed.getElementsByTagName("parsererror").length > 0) fail("An EPUB XML document is malformed.");
    return parsed;
  }

  function descendantsByLocalName(root, name) {
    return Array.from(root.getElementsByTagName("*")).filter((element) => element.localName === name);
  }

  function normalizedTitle(value, fallback) {
    const title = String(value || "").replace(/\s+/g, " ").trim().slice(0, 256);
    return title || fallback;
  }

  async function parseEpub(value) {
    const archive = parseZipDirectory(value);
    const xmlBudget = createXmlBudget();
    const containerEntry = archive.entries.get("META-INF/container.xml");
    if (!containerEntry) fail("This EPUB has no package index.");
    const containerDocument = parseXml(await inflateEntry(archive, containerEntry, MAX_XML_BYTES), xmlBudget);
    const rootfile = descendantsByLocalName(containerDocument, "rootfile").find((node) => node.getAttribute("full-path"));
    if (!rootfile) fail("This EPUB has no package index.");
    const packagePath = normalizeReference(rootfile.getAttribute("full-path"), "");
    const packageEntry = packagePath && archive.entries.get(packagePath);
    if (!packageEntry) fail("The EPUB package index is missing.");
    const packageDocument = parseXml(await inflateEntry(archive, packageEntry, MAX_XML_BYTES), xmlBudget);
    const packageDirectory = packagePath.includes("/") ? packagePath.slice(0, packagePath.lastIndexOf("/")) : "";
    const manifest = new Map();
    for (const node of descendantsByLocalName(packageDocument, "item")) {
      const id = node.getAttribute("id");
      const mediaType = (node.getAttribute("media-type") || "").toLowerCase();
      const href = normalizeReference(node.getAttribute("href"), packageDirectory);
      if (!id || id.length > 256 || manifest.has(id) || !href) continue;
      manifest.set(id, { href, mediaType });
    }
    const spine = descendantsByLocalName(packageDocument, "spine")[0];
    if (!spine) fail("The EPUB has no reading order.");
    const references = descendantsByLocalName(spine, "itemref");
    if (references.length === 0 || references.length > MAX_CHAPTERS) fail("The EPUB has no supported chapters or has too many chapters.");
    const chapters = [];
    let totalBytes = 0;
    for (const reference of references) {
      const manifestItem = manifest.get(reference.getAttribute("idref"));
      if (!manifestItem || manifestItem.mediaType !== "application/xhtml+xml") continue;
      const entry = archive.entries.get(manifestItem.href);
      if (!entry || entry.isDirectory) continue;
      totalBytes += entry.uncompressedSize;
      if (totalBytes > MAX_READER_BYTES) fail("The EPUB reading text exceeds the supported size limit.");
      const bytes = await inflateEntry(archive, entry, MAX_CHAPTER_BYTES);
      const chapterDocument = parseXml(bytes, xmlBudget, true);
      const body = descendantsByLocalName(chapterDocument, "body")[0] || chapterDocument.documentElement;
      const heading = descendantsByLocalName(body, "h1")[0]
        || descendantsByLocalName(body, "h2")[0]
        || descendantsByLocalName(body, "h3")[0]
        || descendantsByLocalName(chapterDocument, "title")[0];
      const fallback = "Chapter " + String(chapters.length + 1);
      chapters.push({ title: normalizedTitle(heading && heading.textContent, fallback), name: manifestItem.href });
    }
    if (chapters.length === 0) fail("This EPUB contains no supported XHTML chapters.");
    const dcTitle = descendantsByLocalName(packageDocument, "title")[0];
    return {
      title: normalizedTitle(dcTitle && dcTitle.textContent, "Book"),
      archive,
      chapters
    };
  }

  function safeSourceId(value) {
    return /^[A-Za-z][A-Za-z0-9_.:-]{0,127}$/.test(value);
  }

  function safeChapterFragment(sourceBody) {
    const ids = new Set();
    const elements = Array.from(sourceBody.getElementsByTagName("*"));
    if (elements.length > MAX_CHAPTER_NODES) fail("An EPUB chapter contains too many document elements.");
    for (const element of elements) {
      const id = element.getAttribute("id");
      if (id && safeSourceId(id)) ids.add(id);
    }
    return ids;
  }

  function copySafeNode(source, targetDocument, ids, depth, budget) {
    budget.nodes += 1;
    if (budget.nodes > MAX_CHAPTER_NODES || depth > 256) fail("An EPUB chapter is too deeply nested or too complex.");
    if (source.nodeType === 3) return targetDocument.createTextNode(source.nodeValue || "");
    if (source.nodeType !== 1) return null;
    const name = String(source.localName || source.nodeName).toLowerCase();
    if (DROP_SUBTREE.has(name)) return null;
    const allowed = ALLOWED_ELEMENTS.has(name);
    const target = allowed ? targetDocument.createElement(name) : targetDocument.createDocumentFragment();
    if (allowed) {
      const id = source.getAttribute("id");
      if (id && safeSourceId(id)) target.setAttribute("id", id);
      if (name === "a") {
        const href = source.getAttribute("href") || "";
        const fragment = href.startsWith("#") ? href.slice(1) : "";
        if (fragment && ids.has(fragment) && safeSourceId(fragment)) target.setAttribute("href", "#" + fragment);
      }
    }
    for (const child of Array.from(source.childNodes)) {
      const safeChild = copySafeNode(child, targetDocument, ids, depth + 1, budget);
      if (safeChild) target.appendChild(safeChild);
    }
    return target;
  }

  function replaceElementChildren(target, child) {
    while (target.firstChild) target.removeChild(target.firstChild);
    if (child) target.appendChild(child);
  }

  function renderSafeChapter(chapter, target) {
    const ids = safeChapterFragment(chapter.body);
    const fragment = document.createDocumentFragment();
    const budget = { nodes: 0 };
    for (const child of Array.from(chapter.body.childNodes)) {
      const safeChild = copySafeNode(child, document, ids, 0, budget);
      if (safeChild) fragment.appendChild(safeChild);
    }
    replaceElementChildren(target, fragment);
  }

  function apiUrl(path) {
    const url = new URL(path, global.location.origin);
    const queryKey = new URLSearchParams(global.location.search).get("ApiKey");
    if (queryKey) url.searchParams.set("ApiKey", queryKey);
    return url.href;
  }

  function createPdfLoadingOptions(url) {
    return {
      url,
      withCredentials: true,
      rangeChunkSize: 256 * 1024,
      disableStream: true,
      disableAutoFetch: true,
      isEvalSupported: false,
      enableXfa: false,
      useWasm: true,
      useWorkerFetch: true,
      useSystemFonts: true,
      cMapUrl: "/web/vendor/pdfjs/cmaps/",
      cMapPacked: true,
      standardFontDataUrl: "/web/vendor/pdfjs/standard_fonts/",
      wasmUrl: "/web/vendor/pdfjs/wasm/",
      maxImageSize: 10000000,
      stopAtErrors: true
    };
  }

  function startReader() {
    const root = document.getElementById("book-reader");
    if (!root) return;
    const itemId = root.dataset.itemId;
    const format = root.dataset.format;
    const title = document.getElementById("book-title");
    const status = document.getElementById("reader-status");
    const error = document.getElementById("reader-error");
    const download = document.getElementById("download-link");
    const apiKey = new URLSearchParams(global.location.search).get("ApiKey");
    title.textContent = root.dataset.bookTitle || title.textContent;
    if (download && apiKey) download.href = apiUrl("/Books/" + encodeURIComponent(itemId) + "/Download");

    function showError(reason) {
      error.textContent = reason instanceof Error ? reason.message : "This book could not be opened.";
      error.hidden = false;
      status.textContent = "Unable to open this book";
    }

    if (format === "pdf") {
      const panel = document.getElementById("pdf-panel");
      const canvas = document.getElementById("pdf-canvas");
      const pageLabel = document.getElementById("pdf-page-label");
      const previous = document.getElementById("previous-page");
      const next = document.getElementById("next-page");
      let pdfjs;
      let pdfDocument;
      let loadingTask;
      let readerClosed = false;
      let pageNumber = 1;
      let renderTask;
      let renderRevision = 0;

      async function renderPage() {
        const revision = ++renderRevision;
        if (renderTask) renderTask.cancel();
        previous.disabled = true;
        next.disabled = true;
        try {
          const page = await pdfDocument.getPage(pageNumber);
          if (revision !== renderRevision) return;
          const baseViewport = page.getViewport({ scale: 1 });
          if (!Number.isFinite(baseViewport.width) || !Number.isFinite(baseViewport.height) || baseViewport.width <= 0 || baseViewport.height <= 0) {
            fail("This PDF page has unsupported dimensions.");
          }
          const scale = Math.min(1.5, 1800 / baseViewport.width, 2400 / baseViewport.height);
          const viewport = page.getViewport({ scale });
          canvas.width = Math.max(1, Math.ceil(viewport.width));
          canvas.height = Math.max(1, Math.ceil(viewport.height));
          canvas.setAttribute("aria-label", "PDF page " + String(pageNumber));
          const context = canvas.getContext("2d", { alpha: false });
          if (!context) fail("This browser cannot draw PDF pages.");
          const task = page.render({
            canvas,
            canvasContext: context,
            viewport,
            annotationMode: pdfjs.AnnotationMode.DISABLE,
            background: "rgb(255, 255, 255)"
          });
          renderTask = task;
          await task.promise;
          if (revision !== renderRevision) return;
          pageLabel.textContent = "Page " + String(pageNumber) + " of " + String(pdfDocument.numPages);
          status.textContent = "PDF page " + String(pageNumber) + " of " + String(pdfDocument.numPages);
          previous.disabled = pageNumber <= 1;
          next.disabled = pageNumber >= pdfDocument.numPages;
          global.scrollTo(0, 0);
        } catch (reason) {
          if (!readerClosed && revision === renderRevision && reason && reason.name !== "RenderingCancelledException") showError(reason);
        }
      }

      previous.addEventListener("click", () => {
        if (pageNumber > 1) {
          pageNumber -= 1;
          renderPage();
        }
      });
      next.addEventListener("click", () => {
        if (pdfDocument && pageNumber < pdfDocument.numPages) {
          pageNumber += 1;
          renderPage();
        }
      });

      (async function loadPdf() {
        try {
          pdfjs = await import("/web/vendor/pdfjs/pdf.min.mjs");
          pdfjs.GlobalWorkerOptions.workerSrc = "/web/vendor/pdfjs/pdf.worker.min.mjs";
          if (readerClosed) return;
          loadingTask = pdfjs.getDocument(createPdfLoadingOptions(
            apiUrl("/Books/" + encodeURIComponent(itemId) + "/Document")
          ));
          pdfDocument = await loadingTask.promise;
          if (readerClosed) return;
          if (pdfDocument.numPages < 1 || pdfDocument.numPages > MAX_PDF_PAGES) {
            await loadingTask.destroy();
            pdfDocument = null;
            fail("This PDF has too many pages to read here.");
          }
          panel.hidden = false;
          await renderPage();
        } catch (reason) {
          if (!readerClosed) showError(reason);
        }
      })();

      global.addEventListener("pagehide", () => {
        readerClosed = true;
        if (renderTask) renderTask.cancel();
        if (loadingTask) void loadingTask.destroy().catch(() => {});
      }, { once: true });
      return;
    }

    if (format !== "epub") {
      showError(new Error("This book format is not supported."));
      return;
    }

    const panel = document.getElementById("epub-panel");
    const select = document.getElementById("chapter-list");
    const content = document.getElementById("epub-content");
    const previous = document.getElementById("previous-chapter");
    const next = document.getElementById("next-chapter");
    const progressKey = "puffinbox:book:" + itemId;
    let book;
    let chapterIndex = 0;
    let chapterLoadRevision = 0;

    async function updateChapter(index, focusContent) {
      const revision = ++chapterLoadRevision;
      chapterIndex = Math.max(0, Math.min(index, book.chapters.length - 1));
      select.value = String(chapterIndex);
      select.disabled = true;
      previous.disabled = true;
      next.disabled = true;
      replaceElementChildren(content);
      status.textContent = "Loading chapter " + String(chapterIndex + 1) + " of " + String(book.chapters.length);
      try {
        const chapter = book.chapters[chapterIndex];
        const entry = book.archive.entries.get(chapter.name);
        const bytes = await inflateEntry(book.archive, entry, MAX_CHAPTER_BYTES);
        const chapterDocument = parseXml(bytes, createXmlBudget(), true);
        const body = descendantsByLocalName(chapterDocument, "body")[0] || chapterDocument.documentElement;
        if (revision !== chapterLoadRevision) return;
        renderSafeChapter({ body }, content);
        previous.disabled = chapterIndex === 0;
        next.disabled = chapterIndex === book.chapters.length - 1;
        status.textContent = "Chapter " + String(chapterIndex + 1) + " of " + String(book.chapters.length);
        try {
          global.localStorage.setItem(progressKey, String(chapterIndex));
        } catch (_error) {
          // Private browsing may disable local storage; reading still works.
        }
        if (focusContent) content.focus();
        global.scrollTo(0, 0);
      } catch (reason) {
        if (revision === chapterLoadRevision) showError(reason);
      } finally {
        if (revision === chapterLoadRevision) select.disabled = false;
      }
    }

    select.addEventListener("change", () => { void updateChapter(Number(select.value), true); });
    previous.addEventListener("click", () => { void updateChapter(chapterIndex - 1, true); });
    next.addEventListener("click", () => { void updateChapter(chapterIndex + 1, true); });

    (async function load() {
      try {
        const response = await fetch(apiUrl("/Books/" + encodeURIComponent(itemId) + "/Epub"), {
          credentials: "same-origin",
          cache: "no-store",
          headers: { Accept: "application/epub+zip" }
        });
        if (!response.ok) fail("The EPUB could not be loaded. Check book access and try again.");
        const blob = await response.blob();
        if (blob.size > MAX_ARCHIVE_BYTES) fail("The EPUB file is outside the supported size limit.");
        book = await parseEpub(await blob.arrayBuffer());
        title.textContent = book.title;
        for (let index = 0; index < book.chapters.length; index += 1) {
          const option = document.createElement("option");
          option.value = String(index);
          option.textContent = book.chapters[index].title;
          select.appendChild(option);
        }
        let saved = 0;
        try {
          const candidate = Number(global.localStorage.getItem(progressKey));
          if (Number.isInteger(candidate)) saved = candidate;
        } catch (_error) {
          saved = 0;
        }
        panel.hidden = false;
        await updateChapter(saved, false);
      } catch (reason) {
        showError(reason);
      }
    })();
  }

  const exported = {
    isSafeArchivePath,
    normalizeReference,
    parseZipDirectory,
    parseEpub,
    inflateEntry,
    createXmlBudget,
    validateXmlComplexity,
    parseXml,
    replaceElementChildren,
    createPdfLoadingOptions
  };
  if (typeof module === "object" && module.exports) module.exports = exported;
  if (typeof document !== "undefined") {
    if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", startReader, { once: true });
    else startReader();
  }
})(globalThis);
