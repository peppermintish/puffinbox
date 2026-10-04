const assert = require("node:assert/strict");
const test = require("node:test");
const { createPdfLoadingOptions, inflateEntry, isSafeArchivePath, normalizeReference, parseEpub, parseZipDirectory, replaceElementChildren } = require("./book-reader.js");

function crc32(bytes) {
  let crc = 0xffffffff;
  for (const byte of bytes) {
    crc ^= byte;
    for (let bit = 0; bit < 8; bit += 1) crc = (crc >>> 1) ^ ((crc & 1) ? 0xedb88320 : 0);
  }
  return (crc ^ 0xffffffff) >>> 0;
}

function makeZip(entries) {
  const encoder = new TextEncoder();
  const localParts = [];
  const centralParts = [];
  let localOffset = 0;
  for (const source of entries) {
    const name = encoder.encode(source.name);
    const data = encoder.encode(source.data);
    const checksum = crc32(data);
    const mode = source.mode || 0;
    const local = new Uint8Array(30 + name.length + data.length);
    const localView = new DataView(local.buffer);
    localView.setUint32(0, 0x04034b50, true);
    localView.setUint16(4, 20, true);
    localView.setUint32(14, checksum, true);
    localView.setUint32(18, data.length, true);
    localView.setUint32(22, data.length, true);
    localView.setUint16(26, name.length, true);
    local.set(name, 30);
    local.set(data, 30 + name.length);

    const central = new Uint8Array(46 + name.length);
    const centralView = new DataView(central.buffer);
    centralView.setUint32(0, 0x02014b50, true);
    centralView.setUint16(4, 0x0314, true);
    centralView.setUint16(6, 20, true);
    centralView.setUint32(16, checksum, true);
    centralView.setUint32(20, data.length, true);
    centralView.setUint32(24, data.length, true);
    centralView.setUint16(28, name.length, true);
    centralView.setUint32(38, mode << 16, true);
    centralView.setUint32(42, localOffset, true);
    central.set(name, 46);

    localParts.push(local);
    centralParts.push(central);
    localOffset += local.length;
  }
  const centralSize = centralParts.reduce((sum, part) => sum + part.length, 0);
  const eocd = new Uint8Array(22);
  const eocdView = new DataView(eocd.buffer);
  eocdView.setUint32(0, 0x06054b50, true);
  eocdView.setUint16(8, entries.length, true);
  eocdView.setUint16(10, entries.length, true);
  eocdView.setUint32(12, centralSize, true);
  eocdView.setUint32(16, localOffset, true);
  const parts = [...localParts, ...centralParts, eocd];
  const archive = new Uint8Array(parts.reduce((sum, part) => sum + part.length, 0));
  let offset = 0;
  for (const part of parts) {
    archive.set(part, offset);
    offset += part.length;
  }
  return archive;
}

function xmlElement(localName, attributes = {}, textContent = "", children = []) {
  return {
    localName,
    textContent,
    children,
    getAttribute(name) { return attributes[name] || null; },
    getElementsByTagName(name) {
      const found = [];
      const visit = (element) => {
        for (const child of element.children) {
          if (name === "*" || child.localName === name) found.push(child);
          visit(child);
        }
      };
      visit(this);
      return found;
    }
  };
}

function installFakeDomParser() {
  let calls = 0;
  const original = global.DOMParser;
  global.DOMParser = class {
    parseFromString(xml) {
      calls += 1;
      if (xml.includes("container")) {
        const rootfile = xmlElement("rootfile", { "full-path": "OEBPS/package.opf" });
        const documentElement = xmlElement("container", {}, "", [rootfile]);
        return fakeDocument(documentElement);
      }
      if (xml.includes("package")) {
        const manifestItems = Array.from(xml.matchAll(/<item id="([^"]+)" href="([^"]+)" media-type="([^"]+)"\s*\/>/g),
          (match) => xmlElement("item", { id: match[1], href: match[2], "media-type": match[3] }));
        const itemrefs = Array.from(xml.matchAll(/<itemref idref="([^"]+)"\s*\/>/g),
          (match) => xmlElement("itemref", { idref: match[1] }));
        const spine = xmlElement("spine", {}, "", itemrefs);
        const title = xmlElement("title", {}, "Fixture book");
        const manifest = xmlElement("manifest", {}, "", manifestItems);
        const metadata = xmlElement("metadata", {}, "", [title]);
        return fakeDocument(xmlElement("package", {}, "", [metadata, manifest, spine]));
      }
      if (xml.includes("html")) {
        const heading = xmlElement("h1", {}, "Fixture chapter");
        const body = xmlElement("body", {}, "", [heading]);
        return fakeDocument(xmlElement("html", {}, "", [body]));
      }
      throw new Error("unexpected XML document in EPUB test fixture");
    }
  };
  return {
    calls: () => calls,
    restore: () => {
      if (original === undefined) delete global.DOMParser;
      else global.DOMParser = original;
    }
  };
}

function fakeDocument(documentElement) {
  return {
    documentElement,
    getElementsByTagName(name) {
      if (name === "parsererror") return [];
      if (name === "*") return [documentElement, ...documentElement.getElementsByTagName("*")];
      return documentElement.getElementsByTagName(name);
    }
  };
}

function minimalEpub(chapterEntries, overrides = {}) {
  const container = overrides.container || "<container/>";
  const manifest = chapterEntries.map((_, index) => `<item id="chapter${index + 1}" href="chapter-${index + 1}.xhtml" media-type="application/xhtml+xml"/>`).join("");
  const spine = chapterEntries.map((_, index) => `<itemref idref="chapter${index + 1}"/>`).join("");
  const packageDocument = overrides.packageDocument
    || `<package><metadata><title>Fixture book</title></metadata><manifest>${manifest}</manifest><spine>${spine}</spine></package>`;
  return makeZip([
    { name: "META-INF/container.xml", data: container },
    { name: "OEBPS/package.opf", data: packageDocument },
    ...chapterEntries.map((data, index) => ({ name: `OEBPS/chapter-${index + 1}.xhtml`, data }))
  ]);
}

test("PDF.js loads local support assets with PDF scripting disabled", () => {
  const options = createPdfLoadingOptions("/Books/example/Document");
  assert.equal(options.url, "/Books/example/Document");
  assert.equal(options.useSystemFonts, true);
  assert.equal(options.useWasm, true);
  assert.equal(options.wasmUrl, "/web/vendor/pdfjs/wasm/");
  assert.equal(options.cMapUrl, "/web/vendor/pdfjs/cmaps/");
  assert.equal(options.cMapPacked, true);
  assert.equal(options.standardFontDataUrl, "/web/vendor/pdfjs/standard_fonts/");
  assert.equal(options.isEvalSupported, false);
  assert.equal(options.enableXfa, false);
});

test("archive path checks block traversal and platform-specific paths", () => {
  for (const unsafe of ["../outside", "/root", "OEBPS/../../outside", "C:/secret", "OEBPS\\bad", "x//y", "x/./y"]) {
    assert.equal(isSafeArchivePath(unsafe), false, unsafe);
  }
  assert.equal(isSafeArchivePath("META-INF/container.xml"), true);
  assert.equal(normalizeReference("chapter%20one.xhtml", "OEBPS"), "OEBPS/chapter one.xhtml");
  assert.equal(normalizeReference("../../outside.xhtml", "OEBPS"), null);
  assert.equal(normalizeReference("https://example.test/book.xhtml", "OEBPS"), null);
});

test("ZIP directory can read a stored EPUB document and verifies its CRC", async () => {
  const fixture = makeZip([
    { name: "META-INF/container.xml", data: "<container/>" },
    { name: "OEBPS/chapter.xhtml", data: "<html><body><p>Read safely</p></body></html>" }
  ]);
  const archive = parseZipDirectory(fixture);
  const chapter = await inflateEntry(archive, archive.entries.get("OEBPS/chapter.xhtml"), 1024);
  assert.equal(new TextDecoder().decode(chapter), "<html><body><p>Read safely</p></body></html>");
});

test("ZIP directory rejects traversal, symbolic links, duplicate entries, and truncation", () => {
  assert.throws(() => parseZipDirectory(makeZip([{ name: "../outside", data: "x" }])), /unsafe archive path/);
  assert.throws(() => parseZipDirectory(makeZip([{ name: "link", data: "outside", mode: 0o120777 }])), /Symbolic links/);
  assert.throws(() => parseZipDirectory(makeZip([
    { name: "META-INF/container.xml", data: "one" },
    { name: "META-INF/container.xml", data: "two" }
  ])), /duplicate archive paths/);
  assert.throws(() => parseZipDirectory(new Uint8Array([0x50, 0x4b, 0x03, 0x04])), /outside the supported size limit|directory is missing/);
});

test("EPUB container and package complexity is rejected before DOMParser", async () => {
  const parser = installFakeDomParser();
  try {
    const overBudgetContainer = "<container>" + "<x/>".repeat(5001) + "</container>";
    await assert.rejects(parseEpub(minimalEpub([], { container: overBudgetContainer })), /markup limit/);
    assert.equal(parser.calls(), 0, "the over-budget container reached DOMParser");

    const overBudgetPackage = "<package>" + "<x/>".repeat(5001) + "</package>";
    await assert.rejects(parseEpub(minimalEpub([], { packageDocument: overBudgetPackage })), /markup limit/);
    assert.equal(parser.calls(), 1, "the over-budget package document reached DOMParser");
  } finally {
    parser.restore();
  }
});

test("EPUB chapters are complexity-checked before parsing and are not retained as DOMs", async () => {
  const parser = installFakeDomParser();
  try {
    const overBudgetChapter = "<html><body>" + "<p>x</p>".repeat(2501) + "</body></html>";
    await assert.rejects(parseEpub(minimalEpub([overBudgetChapter])), /markup limit/);
    assert.equal(parser.calls(), 2, "the over-budget chapter reached DOMParser");

    const book = await parseEpub(minimalEpub(["<html><body><p>Safe</p></body></html>"]));
    assert.equal(book.chapters.length, 1);
    assert.equal(book.chapters[0].title, "Fixture chapter");
    assert.equal(book.chapters[0].document, undefined);
    assert.equal(book.chapters[0].body, undefined);
  } finally {
    parser.restore();
  }
});

test("chapter complexity is bounded cumulatively across the whole EPUB", async () => {
  const parser = installFakeDomParser();
  try {
    const chapters = Array.from({ length: 8 }, () => "<html><body>" + "<p>x</p>".repeat(2000) + "</body></html>");
    await assert.rejects(parseEpub(minimalEpub(chapters)), /aggregate XML limit/);
    assert.equal(parser.calls(), 9, "an individually small chapter exceeded the cumulative budget after being parsed");
  } finally {
    parser.restore();
  }
});

test("EPUB content replacement works without the newer Element.replaceChildren API", () => {
  const firstExistingChild = { name: "old chapter text" };
  const secondExistingChild = { name: "old chapter paragraph" };
  const safeFragment = { name: "sanitized chapter fragment" };
  const content = {
    childNodes: [firstExistingChild, secondExistingChild],
    get firstChild() { return this.childNodes[0] || null; },
    removeChild(child) {
      const index = this.childNodes.indexOf(child);
      assert.notEqual(index, -1);
      this.childNodes.splice(index, 1);
      return child;
    },
    appendChild(child) {
      this.childNodes.push(child);
      return child;
    }
  };

  assert.equal(content.replaceChildren, undefined);
  replaceElementChildren(content, safeFragment);
  assert.deepEqual(content.childNodes, [safeFragment]);
  replaceElementChildren(content);
  assert.deepEqual(content.childNodes, []);
});
