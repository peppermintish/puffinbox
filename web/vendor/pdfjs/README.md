# PDF.js browser assets

These files come from the official `pdfjs-dist` npm package, version `6.3.289`.
The package reports Apache-2.0 and its downloaded tarball matched the registry's
published SHA-512 integrity value:

```text
sha512-ZHjSVpDa3D6izMq8/04lvkhkATUmL9px6ChPaXc1k6nU2Mrhlg1/7F0bdUqCwUjw3NsPTfPZsMDUU6ZIcRaeQw==
```

The browser reader uses the Apache-2.0 display module and PDF worker plus the
MIT-licensed qcms WebAssembly decoder. PDF.js CMaps, Foxit base fonts, and the
JBIG2 and OpenJPEG WebAssembly decoders were removed because their licenses are
outside the project's MIT/Apache-2.0 allowlist. The reader uses browser system
fonts when a PDF does not embed a font, and it does not load PDF.js's scripting
manager or XFA support. See `LICENSE` and the qcms notices under `wasm/`.

This reduced asset set still reads ordinary PDFs with embedded fonts or system
font substitutes. PDFs that require Adobe predefined CMaps may fail or render
text incorrectly, and pages that use JBIG2 or JPEG2000 images cannot decode
those images. ICC color-profile processing remains available through qcms.
