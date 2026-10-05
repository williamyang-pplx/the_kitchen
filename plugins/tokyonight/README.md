# tokyonight

Tokyo Night ("night") color theme for the viewer's syntax highlighting, to
match the app's UI.

`themes/TokyoNight.tmTheme` is folke/tokyonight.nvim's
`extras/sublime/tokyonight_night.tmTheme` at commit
cdc07ac78467a233fd62c493de29a17e0cf2b2b6 (Apache-2.0, see LICENSE), unchanged
except for a block of `the_kitchen:` rules at the top of `settings`. They give
colors to the nvim-treesitter groups the upstream theme leaves unstyled
(syntect picks the first of equally specific rules, so they go first):

| nvim group | scopes | color |
|---|---|---|
| `@property` | `variable.other.property`, `variable.other.member`, YAML keys | `#73daca` |
| `@module` | `meta.path`, `entity.name.namespace`, `variable.namespace` | `#7dcfff` |
| `@function.macro` | `support.macro`, `entity.name.macro` | `#7dcfff` |
| `@attribute` | `variable.annotation`, decorators | `#7dcfff` |
| `@type.builtin` | `storage.type.primitive`, `support.type.primitive` | `#27a1b9` |
| `@label` | `entity.name.label`, `variable.other.label`, lifetimes | `#7aa2f7` |

Only foreground colors are used; the background is the app's.
