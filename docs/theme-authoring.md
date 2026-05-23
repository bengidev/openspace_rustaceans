# Theme authoring

Theme files are TOML and parse into `openspace-theme::Theme` without filesystem access.

## Top-level keys

```toml
id = "default-dark"
appearance = "dark" # light | dark | high-contrast

[metadata]
display_name = "Default Dark"
author = "Optional"
description = "Optional"
```

## Required `[ui]` tokens

`background`, `foreground`, `accent`, `surface`, `border`.

## Required `[syntax]` tokens

`keyword`, `string`, `comment`, `number`, `function`, `type`, `variable`, `operator`.

## Required `[terminal]` tokens

`black`, `red`, `green`, `yellow`, `blue`, `magenta`, `cyan`, `white`, `bright_black`, `bright_red`, `bright_green`, `bright_yellow`, `bright_blue`, `bright_magenta`, `bright_cyan`, `bright_white`.

## Colour syntax

Accepted forms:

- `#RRGGBB`
- `#RRGGBBAA`
- `rgb(r, g, b)`
- `rgba(r, g, b, a)` where alpha is `0..=255` or `0.0..=1.0`

Serialisation emits canonical `#RRGGBBAA`. Missing tokens and invalid colours return typed `ThemeError` variants.

## Bundled themes

`default-light`, `default-dark`, `solarized-light`, `solarized-dark`, `gruvbox`, `nord`.
