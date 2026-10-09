# Provider logo assets

`claude.png` and `codex.png` are the 128×128 Discord thumbnail attachments used by quota notifications. Their standalone SVG sources reuse the `logo-claude` and `logo-codex` symbols from [`ui/logos.svg`](../../../ui/logos.svg), with a transparent 32×32 canvas and four-unit padding. Claude uses its `#D97757` orange mark; Codex uses `#10A37F` green. Colored marks remain visible on light and dark Discord themes without a background tile. The Discord embed accents use matching colors.

The dashboard's `ui/logos.svg` is the canonical artwork. Regenerate both standalone SVGs and PNGs from it with Python's standard library and [librsvg's `rsvg-convert`](https://gitlab.gnome.org/GNOME/librsvg):

```sh
python3 scripts/generate-notification-logos.py
```

The script uses these rasterization commands internally:

```sh
rsvg-convert -w 128 -h 128 -o src/notifications/assets/claude.png src/notifications/assets/claude.svg
rsvg-convert -w 128 -h 128 -o src/notifications/assets/codex.png src/notifications/assets/codex.svg
```

No metadata stripping step is required. `rsvg-convert` is only needed to regenerate the checked-in PNGs; the running application reads the bundled PNG data and does not invoke an image tool or fetch logos from a host.

The SVG and PNG files derive from LobeHub Icons, licensed under the MIT License; the attribution and license text are included at the top of each SVG source and in [`ui/logos.svg`](../../../ui/logos.svg). Claude and ChatGPT/Codex logos are trademarks of their owners. They identify the subscription provider in a proxy notification and do not represent an official Claude, ChatGPT, or Codex application or imply endorsement.
