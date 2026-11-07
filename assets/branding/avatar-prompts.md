<!-- SPDX-FileCopyrightText: 2026 Slavi Pantaleev
SPDX-License-Identifier: Apache-2.0 -->

# Avatar generation prompts

The PNG assets were generated with the built-in image-generation tool. The second
pass edited the first image to add the navy backdrop and more room around the robot.
The tool returned 1254 × 1254 images despite the requested 1024 × 1024 size.
The avatar PNG was losslessly recompressed with OptiPNG; its pixels and embedded
provenance metadata are preserved. The SVG companion is drawn with native vector
shapes and has no embedded raster image.

## Initial generation

```text
Use case: logo-brand
Asset type: square GitHub App avatar for ArtiDezi, a dependable software-maintenance bot.
Primary request: Create a polished, memorable robot mascot portrait with a clear silhouette and a warm, quietly capable expression. This is one finished avatar, not a concept sheet.
Brand context: The existing ArtiDezi mark consists of four rounded squares in a two-by-two grid: three ivory squares and one peach square at the bottom right, on deep navy. Reinterpret that visual idea as the robot's actual face, with four large softly rounded modular face tiles, simple inset navy eyes in the upper tiles, and a subtle friendly mouth. The lower-right tile is a peach button-like cheek. Keep the expression coherent and charming.
Style/medium: Premium minimal illustrated mascot with a little soft enamel depth, clean broad shapes and crisp edges. Restrained shading, no grain or surface noise. A very short simple antenna and compact side connectors may help the silhouette read as a robot; the four-tile face is the distinctive feature.
Composition/framing: 1:1 square PNG, 1024 by 1024. One centered front-facing head, filling approximately 70 percent of the square. Keep all essential features safely inside a circular avatar crop, with generous clear margin. Large readable features that survive a 32-pixel display.
Scene/backdrop: Uniform full-bleed dark navy #172b3a.
Color palette: Warm ivory #f5f3ed, navy #172b3a, peach #eab489. Use peach as the distinctive accent, not a neon orange.
Text: None.
Avoid: Words, initials, numbers, typography, watermarks, borders, UI mockups, full body, hands, tools, scenery, floating decorations, tiny circuitry, glossy chrome, photorealism, gaming/esports styling, angry eyes or a generic AI sparkle. No third-party logos.
```

## Avatar refinement

Reference: `artidezi-bot-transparent-v1.png`.

```text
Use case: logo-brand
Asset type: final square GitHub bot avatar, with an OPAQUE background.
Input image: reference and edit target, the previously generated ArtiDezi modular robot face.
Primary request: Keep the robot design, ivory/peach/navy palette, eyes, smile, antenna and soft enamel appearance unchanged. Place the complete robot on a perfectly uniform opaque dark navy background, exactly #172b3a, extending to all four image edges. This background is essential: do not return a cutout or transparent PNG.
Composition: Produce a square 1024 by 1024 image. Scale the entire robot down slightly so the antenna, side connectors and head all fit comfortably within the central 76% of the canvas and within an inscribed circular avatar crop. Keep the robot centered, with optical balance around its face and antenna. Preserve the four-tile face (three ivory tiles and the peach lower-right tile).
Edges: clean anti-aliased edges against the navy background, no pale fringe, no stray pixels.
Constraints: one avatar only, no lettering, no watermark, no border, no additional objects. Preserve the expression and design. Every background pixel must be opaque navy; no transparency.
```

