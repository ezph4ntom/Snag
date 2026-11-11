# Themes

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
| `fonts.family` | string or `null` | A `.ttf`/`.otf` file, relative to this folder (e.g. `"fonts/Inter.ttf"`). `null` = built-in font. |
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

## Full example

```json
{
  "name": "Dark",
  "dark": true,
  "colors": {
    "background": "#1b1d22",
    "panel": "#23262d",
    "text": "#e4e6eb",
    "text_dim": "#9aa0ab",
    "accent": "#4f8cff",
    "accent_text": "#ffffff",
    "widget": "#31353e",
    "widget_hover": "#3b404b",
    "widget_active": "#464c59",
    "border": "#3a3f4a",
    "success": "#3fc67f",
    "warning": "#f0a52a",
    "error": "#ef5350",
    "timeline": "#2c3038",
    "timeline_selection": "#4f8cff55",
    "playhead": "#ffffff"
  },
  "fonts": { "body": 14.0, "heading": 20.0, "button": 14.0, "small": 11.5, "mono": 13.0, "family": null },
  "corner_radius": 6.0,
  "spacing": { "item": [8.0, 6.0], "button_padding": [10.0, 5.0], "window_margin": 12.0, "indent": 18.0 }
}
```

If a theme file has a mistake (bad color, invalid JSON), the Appearance tab
shows the error and the built-in theme is used until you fix it.
