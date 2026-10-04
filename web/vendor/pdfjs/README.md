# PDF.js browser assets

These files are unchanged assets from the official `pdfjs-dist` npm package,
version `6.3.289`. The downloaded [registry archive](https://registry.npmjs.org/pdfjs-dist/-/pdfjs-dist-6.3.289.tgz)
matched this published SHA-512 integrity value:

```text
sha512-ZHjSVpDa3D6izMq8/04lvkhkATUmL9px6ChPaXc1k6nU2Mrhlg1/7F0bdUqCwUjw3NsPTfPZsMDUU6ZIcRaeQw==
```

`provenance.json` records the SHA-256 of each retained file and its license
notice. `scripts/check_pdf_assets.py` verifies the asset selection and bytes
before builds. The license bundle includes all nine notices and this record.

| Assets | Terms |
| --- | --- |
| Display module and worker | Apache-2.0 |
| Adobe predefined CMaps | BSD-3-Clause |
| Foxit base fonts | BSD-3-Clause |
| qcms decoder | MIT |
| JBIG2 decoder | BSD-3-Clause and Apache-2.0 |
| OpenJPEG decoder | BSD-2-Clause |

The reader loads these resources from the server itself. The upstream
Liberation fonts and `LICENSE_LIBERATION` carry GPL font terms and are excluded.
Unembedded fonts may therefore still depend on the browser's installed fonts;
the retained Foxit fonts cover Symbol, Dingbats, Courier, and Times substitutes.
PDF scripting and XFA are disabled; QuickJS scripting assets are excluded.

The synthetic browser check renders CMap-dependent Japanese text, Foxit Symbol
glyphs, a JPEG2000 color image, and a JBIG2 MMR image. It checks text and pixels,
including the JavaScript decoder fallbacks. These cases establish a bounded
regression check, not support for every PDF encoding or font.
