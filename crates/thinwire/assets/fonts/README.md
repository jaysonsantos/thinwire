# Bundled fonts

| File | Font | Version | License | Source |
| --- | --- | --- | --- | --- |
| `InterVariable.ttf` | Inter | 4.1 | SIL Open Font License 1.1 (`OFL.txt`) | <https://github.com/rsms/inter/releases/tag/v4.1> |

The binary embeds `InterVariable.ttf` with `include_bytes!`. The app uses the `wght` axis at 400, 500, and 600.

Each binary release must ship `OFL.txt`. `scripts/stage-os-artifact.sh` copies it to `THIRD_PARTY_NOTICES/Inter-OFL.txt`.

Keep the copyright line and `OFL.txt` with the font file.

SHA-256 of `InterVariable.ttf`: `4989b125924991b90d05b2d16e0e388c48f7d5bb8b30539bbf9c755278d0ccaf`.
