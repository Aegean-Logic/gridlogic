# GridLogic app icon

**Sheet trace**: a spreadsheet page with an ink header row, a light grid and a sea trace that ends
in the aqua node, on a marble tile. It is drawn from the Aegean Logic palette so GridLogic sits with
the rest of the Aegean Office suite. It is original geometry written by hand for this repository,
deliberately not modelled on any vendor's spreadsheet logo (no letters, no "X", no overlapping
panels).

Two masters:

- `gridcraft.svg`: the full artwork, used from 48 px up.
- `gridcraft-small.svg`: a simplified glyph for 32 px and below. It drops the grid lines, thickens
  the frame and trace and enlarges the node, so the icon still reads at 16 px.

## Palette

| Colour | Hex | Used for |
|---|---|---|
| Ink | `#0A2A3B` | Sheet frame and header row |
| Sea | `#12718C` | The trace |
| Aqua | `#2FC6C0` | The node at the end of the trace (one per icon) |
| Marble | `#F4F2EC` | The tile |
| White | `#FFFFFF` | The sheet, and the ring around the node |
| Grid | `#CED4D8` | Grid lines (full artwork only) |

## Geometry

A 512 × 512 viewBox; the tile is a rounded square with `rx=112`. Windows and Linux use the full
tile. The macOS icons (`.icns`, `gridcraft-macos-512.png`) sit on Apple's grid: an 824/1024
body with a transparent margin.

## Files

| File | Use |
|---|---|
| `gridcraft.svg` | Canonical artwork (48 px and up); also `hicolor/scalable` |
| `gridcraft-small.svg` | Small-size glyph (32 px and below) |
| `gridcraft-1024.png` | 1024 px render of the full tile |
| `gridcraft-macos-512.png` | Runtime Dock icon on macOS (for `apps/gridcraft` to embed) |
| `gridcraft.icns` | macOS bundle icon (`CFBundleIconFile`), 16–1024 px |
| `gridcraft.ico` | Windows exe icon and web favicon, 16–256 px (embedded by `apps/gridcraft/build.rs`) |
| `hicolor/<size>/apps/uk.co.aegeanlogic.gridlogic.png` | Linux icon theme, 16–512 px; the 256 px one is also the runtime window icon on Windows and Linux |
| `hicolor/scalable/apps/uk.co.aegeanlogic.gridlogic.svg` | Linux scalable icon |
| `LICENSE.txt` | MIT OR Apache-2.0 |

The app ID is `uk.co.aegeanlogic.gridlogic`. It's used for the hicolor icon names, the Wayland app
ID and `packaging/linux/uk.co.aegeanlogic.gridlogic.desktop`.

## Regenerate

Edit `gridcraft.svg` or `gridcraft-small.svg`, then run `packaging/icons.sh`. It needs `resvg`
(`cargo install resvg`) and runs on any platform: it packs the `.ico` with `cargo xtask ico` and
the `.icns` with `cargo xtask icns`.
