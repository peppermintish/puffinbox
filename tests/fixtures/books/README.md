# Original PDF fixtures

These small synthetic documents are project fixtures covered by Puffinbox's
MIT or Apache-2.0 license. They contain no copied books or third-party PDFs.

| File | Contents and expected result |
| --- | --- |
| `foxit-symbol.pdf` | Unembedded Symbol font; five visible Greek glyphs corresponding to `abcde`. |
| `japanese-cmap.pdf` | Unembedded Japanese font with `90ms-RKSJ-H`; extracted text must be `日本語`. Glyph appearance depends on installed Japanese fonts. |
| `jpeg2000.pdf` | Lossless 64×64 JPEG2000 image; red, green, blue, and yellow quadrants. |
| `jbig2-mmr.pdf` | 64×64 monochrome image; black central 32×32 square on white. |

The image fixtures use original generated pixels. JPEG2000 encoding and TIFF
Group 4 MMR compression used Pillow as an external fixture tool. The MMR bytes
are wrapped in an embedded JBIG2 page information segment, immediate generic
region, and end-of-page segment. The segment layout follows
[ITU-T T.88](https://www.itu.int/rec/T-REC-T.88-200002-I/en), sections 7.4.6 and
7.4.8; MMR is enabled and reserved flags are zero. No encoder is needed to run
the checks.

The Linux browser checks require a Japanese system font for the unembedded-font
fixture. CI installs Ubuntu's `fonts-noto-cjk` package; the fonts are covered by
the [SIL Open Font License](https://github.com/notofonts/noto-cjk/blob/main/Serif/LICENSE).
This is a browser test prerequisite, outside the Puffinbox image and release
bundle. Keep the text and rendered-pixel assertions enabled.
