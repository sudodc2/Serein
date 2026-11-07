# Serein brand assets

Serein's application mark is the [Lucide](https://lucide.dev) `user-round-group` glyph —
three people sharing a space — drawn in the brand's icy blue-to-indigo palette. Lucide is
licensed under the ISC License. These files are Serein design assets, not Apple, Discord or
any other vendor's branding, and no third-party font is embedded in them.

- `serein.svg` — full-colour gradient mark on its rounded plate; the master vector source.
- `serein-flat.svg` — flat (non-gradient) variant for small or low-colour contexts.
- `serein-mark.svg` — the glyph as solid white strokes on a 24×24 view box.
  `tools/generate-icons.py` rasterizes this one into the shared icon atlas as `serein-mark`,
  which the interface tints at draw time; `crates/platform` also embeds it for the Linux tray.
- `serein.png` — 460×460 RGBA raster render of the full-colour mark, kept for pipelines that
  want a raster source.
- `serein-1024.png` — 1024×1024 sRGB RGBA preview render of the full-colour mark.
- `serein-tray.png` — 72×72 black-on-transparent render of `serein-mark.svg`, embedded by
  `crates/platform` as the macOS menu bar template image; macOS tints it per appearance, so only
  its alpha is used. Regenerate with any SVG rasterizer, e.g.:

  ```sh
  sed 's/stroke="#fff"/stroke="#000"/g' assets/brand/serein-mark.svg |
    rsvg-convert -w 72 -h 72 -o assets/brand/serein-tray.png
  ```

The packaged platform icons built from the same artwork live in `packaging/` (`macos/Serein.icns`
and `Serein.icon`, `windows/Serein.ico`, `linux/hicolor/*`).

Brand palette: `#8BBEFF` → `#506DDC` → `#2D247C` plate gradient, `#F5FAFF` glyph strokes.
The interface accent (`design::DEFAULT_PRIMARY_RGB`, `#1a72e8`) is drawn from the same family and
is tuned so white text on it keeps a 4.5:1 contrast ratio.
