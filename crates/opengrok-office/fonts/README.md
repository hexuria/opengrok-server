# fonts/

Substitute faces vendored verbatim from `@betteroffice/fonts` 0.4.0 (`assets/`), the font set
the BetterOffice engine measures text against. Every face is licensed under the SIL Open Font
License — per-family texts in `LICENSES/`, the package licence in `LICENSE`.

Why they are vendored rather than assumed present: both BetterOffice raster paths
(`Document::register_font`, `Presentation::register_font`) refuse text they cannot measure, and
a slim deploy host carries no fonts at all. Registration maps each face onto the MS family it
metric-matches (Carlito → Calibri, Liberation Sans → Arial, …) in `src/fonts.rs`, which is what
keeps pagination close to Word's — upstream scores this set at 84.6% of real documents within
one page of Word's own count, against 70.8% with no font provider.

Not shipped: `@betteroffice/fonts-cjk` — CJK documents render with missing glyphs until that
package is vendored the same way. A tracked gap, not an oversight.
