# Fonts

The auggiesoft house type (from the auggiesoft UI guide), compiled into the binary.
All SIL Open Font License 1.1, latin subset. egui can't read woff2, so these are the guide's bundled
`tokens/fonts/*.woff2` files decompressed to TTF with fontTools (same glyphs and tables).

| File | Family | Role | Licence |
|---|---|---|---|
| `Barlow-500.ttf` | Barlow Medium | UI text (500, because most UI text is under 14 px on a dark ground) | `OFL-Barlow.txt` |
| `BarlowTabular-600.ttf` | Barlow SemiBold Tabular | Headline numerals in the HUD tiles | `OFL-Barlow.txt` |
| `BarlowSemiCondensed-600.ttf` | Barlow Semi Condensed SemiBold | Uppercase labels | `OFL-Barlow.txt` |
| `AuggieMono-400.ttf`, `AuggieMono-500.ttf` | Auggie Mono | Values, frequencies, axis ticks | `OFL-IBMPlex.txt` |

`BarlowTabular-600.ttf` is a Modified Version of Barlow SemiBold: egui can't switch OpenType
features on, so the `tnum` (tabular figures) feature is frozen into the default digits, which keeps
the HUD numbers from jittering as they change. Barlow has no Reserved Font Name. Rebuild:

```sh
uv run --no-project --with fonttools --with brotli python -c "from fontTools.ttLib import TTFont; \
  f = TTFont('Barlow-600.woff2'); f.flavor = None; f.save('Barlow-600.ttf')"
uvx --from opentype-feature-freezer pyftfeatfreeze -f tnum -S -U Tabular Barlow-600.ttf BarlowTabular-600.ttf
```

Auggie Mono is IBM Plex Mono with the slashed zero frozen in, renamed because "Plex" is a Reserved
Font Name; the recipe is in the guide. Glyphs missing from these subsets
(the thin space, ↶) fall back to egui's built-in fonts.
