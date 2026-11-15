# Third-party notices

nit is licensed under MIT OR Apache-2.0. The `nit.exe` binary also contains code and data
from the third-party crates listed in `Cargo.lock`. Their licences are all permissive and are
checked by `cargo deny check licenses` against `deny.toml`. Most only ask that their copyright
and licence notice ship with the binary; the items below need a specific mention.

## Fonts (via `epaint_default_fonts`, the egui default fonts)

| Font | Licence |
|---|---|
| Ubuntu Light | Ubuntu Font Licence 1.0 |
| Hack Regular | MIT (Source Foundry Authors), with Bitstream Vera licence and DejaVu public-domain parts |
| Noto Emoji Regular | SIL Open Font License 1.1 |
| emoji-icon-font | MIT |
| egui-icons (a subset of emoji-icon-font) | MIT |

The licence texts are in the `fonts/` directory of the `epaint_default_fonts` crate.

## Syntax definitions and themes

- `syntect` (MIT) with its bundled default themes.
- `two-face` (MIT OR Apache-2.0) embeds the syntax definitions and themes of
  [bat](https://github.com/sharkdp/bat) (MIT OR Apache-2.0). Each embedded syntax definition has its
  own licence; `two-face` lists them in its `generated/acknowledgements_full` data, readable
  through its `acknowledgement` module.
- `onig` / `onig_sys` (MIT) build the Oniguruma regular expression library (BSD-2-Clause).

## Other licences in the dependency tree

Besides MIT and Apache-2.0: BSD-3-Clause (`subtle`), BSL-1.0 (`clipboard-win`, `error-code`),
CDLA-Permissive-2.0 (`webpki-roots`, the Mozilla root certificate list), ISC (`ring`,
`rustls-webpki`, `untrusted`, `libloading`), Unicode-3.0 (`icu_*`, `unicode-ident` and related
data crates) and Zlib (`foldhash`, `slotmap`, `zlib-rs`). Crates offered under a choice of
licences that includes GPL or LGPL (`self_cell`, `r-efi`) are used under their MIT or Apache-2.0
option.

A full notice file for a release can be generated with
[`cargo about`](https://github.com/EmbarkStudios/cargo-about).
