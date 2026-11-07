<!-- SPDX-FileCopyrightText: 2026 Slavi Pantaleev
SPDX-License-Identifier: Apache-2.0 -->

# ArtiDezi branding

The robot's face carries the original four-square mark: three ivory tiles and a
peach tile, framed in navy.

- [Bot avatar](artidezi-bot-avatar-v1.png): 1254 × 1254 PNG with a navy background,
  suitable for square or circular avatars.
- [Website logo](artidezi-bot.svg): editable SVG for the navigation, login page and
  favicon. Its simplified shapes stay readable at small sizes.
- [Preview](preview.html): open locally to compare both assets at common display sizes.
- [Transparent original](artidezi-bot-transparent-v1.png): first generated candidate.
- [Generation prompts](avatar-prompts.md): raster provenance and prompts.

The website embeds only the SVG in the Rust binary and serves it through the
same content-addressed cache policy as its CSS and JavaScript. Editing the SVG
requires rebuilding the binary; no separate asset deployment is needed.
