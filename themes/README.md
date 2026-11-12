# Themes

Shipped: **Daylight** (`daylight.json`, the default: warm off-white, white
cards, Ember-red accent), **Graphite** (neutral dark greys, Ember accent),
**Midnight** (deep navy, cyan accent), **Warm Ink** (warm black, cream text)
and **OLED** (pure black for OLED screens; outlines and accents stay).

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
| `fonts.family` | string or `null` | A `.ttf`/`.otf` file, relative to this folder (e.g. `"fonts/Inter.ttf"`). `null` = Snag's built-in DM Sans (Medium for text, Bold for headings). |
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
| `preview` | Behind the live preview and clip thumbnails (keep it dark, even in light themes) |
| `control` *(optional)* | Small controls: slider rails, checkbox/radio fills, scrollbar handles. Defaults to `widget`. Set it when `widget` is the same as the background (like OLED's pure black), or those parts would disappear; OLED uses the outline color. |

The header logo follows the theme too: the glyph takes `text`, the dot takes `accent`.

## Full example

```json
{
  "name": "Daylight",
  "dark": false,
  "colors": {
    "background": "#f4f3f0",
    "panel": "#ffffff",
    "text": "#1b1c1f",
    "text_dim": "#5d6068",
    "accent": "#d9441f",
    "accent_text": "#ffffff",
    "widget": "#ffffff",
    "widget_hover": "#f2f1ed",
    "widget_active": "#e8e6e1",
    "border": "#e2e0da",
    "success": "#1f8a5b",
    "warning": "#b7791f",
    "error": "#c62828",
    "timeline": "#e8e6e1",
    "timeline_selection": "#d9441f40",
    "playhead": "#1b1c1f",
    "control": "#e8e6e1",
    "preview": "#1b1c1f"
  },
  "fonts": {
    "body": 14.0,
    "heading": 20.0,
    "button": 14.0,
    "small": 12.0,
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
      14.0,
      7.0
    ],
    "window_margin": 14.0,
    "indent": 18.0
  }
}
```

If a theme file has a mistake (bad color, invalid JSON), the Appearance tab
shows the error and the built-in theme is used until you fix it.
