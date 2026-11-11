# Themes

Shipped: **Snag Dark** (`dark.json`, Ember accent, the default), **Snag
Tide** and **Snag Dusk** (the brand's other two accents), **Snag Light**,
**Snag OLED** (pure black for OLED screens: every surface is `#000000`,
outlines and accents stay) and **Synthwave**.

Each `*.json` file in this folder is a theme. The file name (without
`.json`) is the theme's id; `name` is what the Appearance tab shows.

Selecting a theme applies it instantly. The active theme file is also
watched: save it in your editor and the window updates within a second, so
you can tweak colors live.

Every field is optional. Anything you leave out falls back to the built-in
dark theme, so this is a complete, valid theme:

```json
{ "name": "Hot pink", "colors": { "accent": "#ff3fa4" } }
```

## Schema

| Field | Type | Meaning |
|---|---|---|
| `name` | string | Display name. |
| `dark` | bool | Base palette for parts not listed here (scrollbars, shadows). |
| `colors` | object | See below. Colors are `"#RRGGBB"` or `"#RRGGBBAA"` (alpha last). |
| `fonts.body` / `heading` / `button` / `small` / `mono` | number | Font sizes in points. |
| `fonts.family` | string or `null` | A `.ttf`/`.otf` file, relative to this folder (e.g. `"fonts/Inter.ttf"`). `null` = Snag's built-in Space Grotesk (Medium for text, Bold for headings). |
| `corner_radius` | number | Widget corner radius in points (windows use 1.5x). 0 = square. |
| `spacing.item` | `[x, y]` | Gap between widgets. |
| `spacing.button_padding` | `[x, y]` | Padding inside buttons. |
| `spacing.window_margin` | number | Inner margin of panels. |
| `spacing.indent` | number | Indentation of nested content. |

### Colors

| Key | Used for |
|---|---|
| `background` | Window background, text boxes |
| `panel` | Panels, cards, status bar |
| `text`, `text_dim` | Main and secondary text |
| `accent`, `accent_text` | Selected tab, primary buttons, slider fill, links; text on accent |
| `widget`, `widget_hover`, `widget_active` | Buttons, combo boxes in their three states |
| `border` | Widget and window outlines |
| `success`, `warning`, `error` | Status messages |
| `timeline` | Trimmer track |
| `timeline_selection` | Kept region between the trim handles (use some transparency) |
| `playhead` | Trimmer playhead line |
| `control` *(optional)* | Small controls: slider rails, checkbox/radio fills, scrollbar handles. Defaults to `widget`. Set it when `widget` is the same as the background (like OLED's pure black), or those parts would disappear; OLED uses the outline color. |

The header logo follows the theme too: the glyph takes `text`, the dot takes `accent`.

## Full example

```json
{
  "name": "Snag Dark",
  "dark": true,
  "colors": {
    "background": "#0e1116",
    "panel": "#141820",
    "text": "#f2f4f8",
    "text_dim": "#9aa3b2",
    "accent": "#f04a24",
    "accent_text": "#0e1116",
    "widget": "#1c222b",
    "widget_hover": "#252c37",
    "widget_active": "#2f3743",
    "border": "#2a313c",
    "success": "#34d399",
    "warning": "#fbbf24",
    "error": "#ff6b81",
    "timeline": "#1a1f27",
    "timeline_selection": "#f04a2459",
    "playhead": "#f2f4f8"
  },
  "fonts": {
    "body": 14.0,
    "heading": 21.0,
    "button": 14.0,
    "small": 11.5,
    "mono": 13.0,
    "family": null
  },
  "corner_radius": 8.0,
  "spacing": {
    "item": [
      8.0,
      6.0
    ],
    "button_padding": [
      11.0,
      6.0
    ],
    "window_margin": 12.0,
    "indent": 18.0
  }
}
```

If a theme file has a mistake (bad color, invalid JSON), the Appearance tab
shows the error and the built-in theme is used until you fix it.
