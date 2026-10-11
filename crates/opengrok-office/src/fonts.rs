//! The vendored substitute faces a session registers before it measures or renders text.
//!
//! The set and its aliases are transcribed from `@betteroffice/fonts` 0.4.0 (SIL OFL; the
//! licence texts ship beside the files in `fonts/LICENSES/`). Faces are embedded with
//! `include_bytes!` rather than read from a font directory because the host a deploy lands on
//! is not guaranteed to have one — a slim container has no fonts at all.
//!
//! Only real faces are registered: when a family lacks a style (Comic Relief has no italic,
//! Heebo no italic or bold-italic) the engines fall back within the family rather than draw a
//! synthesized style, which is what upstream's own render test relies on.

use crate::Error;

struct Family {
    name: &'static str,
    /// Names this family metric-substitutes for, registered pointing at the same bytes: a
    /// document asking for Calibri is measured with Carlito's advance widths, which is what
    /// keeps pagination close to Word's.
    aliases: &'static [&'static str],
    regular: &'static [u8],
    bold: Option<&'static [u8]>,
    italic: Option<&'static [u8]>,
    bold_italic: Option<&'static [u8]>,
}

macro_rules! f {
    ($file:literal) => {
        include_bytes!(concat!("../fonts/", $file))
    };
}

static FAMILIES: &[Family] = &[
    Family {
        name: "Caladea",
        aliases: &["Cambria"],
        regular: f!("Caladea-Regular.ttf"),
        bold: Some(f!("Caladea-Bold.ttf")),
        italic: Some(f!("Caladea-Italic.ttf")),
        bold_italic: Some(f!("Caladea-BoldItalic.ttf")),
    },
    Family {
        name: "Carlito",
        aliases: &["Calibri"],
        regular: f!("Carlito-Regular.ttf"),
        bold: Some(f!("Carlito-Bold.ttf")),
        italic: Some(f!("Carlito-Italic.ttf")),
        bold_italic: Some(f!("Carlito-BoldItalic.ttf")),
    },
    Family {
        name: "Comic Relief",
        aliases: &["Comic Sans MS"],
        regular: f!("ComicRelief-Regular.ttf"),
        bold: Some(f!("ComicRelief-Bold.ttf")),
        italic: None,
        bold_italic: None,
    },
    Family {
        name: "DM Sans",
        aliases: &[],
        regular: f!("DMSans-Regular.ttf"),
        bold: Some(f!("DMSans-Bold.ttf")),
        italic: Some(f!("DMSans-Italic.ttf")),
        bold_italic: Some(f!("DMSans-BoldItalic.ttf")),
    },
    Family {
        name: "DM Serif Display",
        aliases: &[],
        regular: f!("DMSerifDisplay-Regular.ttf"),
        bold: None,
        italic: Some(f!("DMSerifDisplay-Italic.ttf")),
        bold_italic: None,
    },
    Family {
        name: "Gelasio",
        aliases: &["Georgia"],
        regular: f!("Gelasio-Regular.ttf"),
        bold: Some(f!("Gelasio-Bold.ttf")),
        italic: Some(f!("Gelasio-Italic.ttf")),
        bold_italic: Some(f!("Gelasio-BoldItalic.ttf")),
    },
    Family {
        name: "Heebo",
        aliases: &[],
        regular: f!("Heebo-Regular.ttf"),
        bold: Some(f!("Heebo-Bold.ttf")),
        italic: None,
        bold_italic: None,
    },
    Family {
        name: "Inter",
        aliases: &[],
        regular: f!("Inter-Regular.ttf"),
        bold: Some(f!("Inter-Bold.ttf")),
        italic: Some(f!("Inter-Italic.ttf")),
        bold_italic: Some(f!("Inter-BoldItalic.ttf")),
    },
    Family {
        name: "Liberation Mono",
        aliases: &["Courier New"],
        regular: f!("LiberationMono-Regular.ttf"),
        bold: Some(f!("LiberationMono-Bold.ttf")),
        italic: Some(f!("LiberationMono-Italic.ttf")),
        bold_italic: Some(f!("LiberationMono-BoldItalic.ttf")),
    },
    Family {
        name: "Liberation Sans",
        aliases: &["Arial"],
        regular: f!("LiberationSans-Regular.ttf"),
        bold: Some(f!("LiberationSans-Bold.ttf")),
        italic: Some(f!("LiberationSans-Italic.ttf")),
        bold_italic: Some(f!("LiberationSans-BoldItalic.ttf")),
    },
    Family {
        name: "Liberation Serif",
        aliases: &["Times New Roman"],
        regular: f!("LiberationSerif-Regular.ttf"),
        bold: Some(f!("LiberationSerif-Bold.ttf")),
        italic: Some(f!("LiberationSerif-Italic.ttf")),
        bold_italic: Some(f!("LiberationSerif-BoldItalic.ttf")),
    },
    Family {
        name: "Montserrat",
        aliases: &[],
        regular: f!("Montserrat-Regular.ttf"),
        bold: Some(f!("Montserrat-Bold.ttf")),
        italic: Some(f!("Montserrat-Italic.ttf")),
        bold_italic: Some(f!("Montserrat-BoldItalic.ttf")),
    },
    Family {
        name: "Noto Naskh Arabic",
        aliases: &[],
        regular: f!("NotoNaskhArabic-Regular.ttf"),
        bold: None,
        italic: None,
        bold_italic: None,
    },
    Family {
        name: "Noto Sans Arabic",
        aliases: &[],
        regular: f!("NotoSansArabic-Regular.ttf"),
        bold: Some(f!("NotoSansArabic-Bold.ttf")),
        italic: None,
        bold_italic: None,
    },
    Family {
        name: "Noto Sans Hebrew",
        aliases: &[],
        regular: f!("NotoSansHebrew-Regular.ttf"),
        bold: Some(f!("NotoSansHebrew-Bold.ttf")),
        italic: None,
        bold_italic: None,
    },
    Family {
        name: "Open Sans",
        aliases: &[],
        regular: f!("OpenSans-Regular.ttf"),
        bold: Some(f!("OpenSans-Bold.ttf")),
        italic: Some(f!("OpenSans-Italic.ttf")),
        bold_italic: Some(f!("OpenSans-BoldItalic.ttf")),
    },
    Family {
        name: "Oswald",
        aliases: &[],
        regular: f!("Oswald-Regular.ttf"),
        bold: Some(f!("Oswald-Bold.ttf")),
        italic: None,
        bold_italic: None,
    },
    Family {
        name: "Poppins",
        aliases: &[],
        regular: f!("Poppins-Regular.ttf"),
        bold: Some(f!("Poppins-Bold.ttf")),
        italic: Some(f!("Poppins-Italic.ttf")),
        bold_italic: Some(f!("Poppins-BoldItalic.ttf")),
    },
    Family {
        name: "Roboto",
        aliases: &[],
        regular: f!("Roboto-Regular.ttf"),
        bold: Some(f!("Roboto-Bold.ttf")),
        italic: Some(f!("Roboto-Italic.ttf")),
        bold_italic: Some(f!("Roboto-BoldItalic.ttf")),
    },
    Family {
        name: "Source Sans 3",
        aliases: &[],
        regular: f!("SourceSans3-Regular.ttf"),
        bold: Some(f!("SourceSans3-Bold.ttf")),
        italic: Some(f!("SourceSans3-Italic.ttf")),
        bold_italic: Some(f!("SourceSans3-BoldItalic.ttf")),
    },
];

/// Every (name, bold, italic, bytes) a font registry should learn: each face under its own
/// family, then again under every alias it substitutes for.
fn faces() -> impl Iterator<Item = (&'static str, bool, bool, &'static [u8])> {
    FAMILIES.iter().flat_map(|family| {
        let styles = [
            (family.regular, false, false),
            (family.bold.unwrap_or(&[]), true, false),
            (family.italic.unwrap_or(&[]), false, true),
            (family.bold_italic.unwrap_or(&[]), true, true),
        ];
        std::iter::once(family.name)
            .chain(family.aliases.iter().copied())
            .flat_map(move |name| {
                styles
                    .into_iter()
                    .filter(|(bytes, _, _)| !bytes.is_empty())
                    .map(move |(bytes, bold, italic)| (name, bold, italic, bytes))
            })
    })
}

pub(crate) fn register_in_docx(document: &mut betteroffice_docx::Document) -> Result<(), Error> {
    for (family, bold, italic, bytes) in faces() {
        document.register_font(family, bold, italic, bytes)?;
    }
    Ok(())
}

pub(crate) fn register_in_pptx(
    presentation: &mut betteroffice_pptx::Presentation,
) -> Result<(), Error> {
    for (family, bold, italic, bytes) in faces() {
        presentation.register_font(family, bold, italic, bytes)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTF_MAGIC: [u8; 4] = [0x00, 0x01, 0x00, 0x00];

    #[test]
    fn every_registered_face_is_a_real_ttf() {
        let mut count = 0;
        for (_, _, _, bytes) in faces() {
            assert_eq!(bytes[..4], TTF_MAGIC, "not a TrueType face");
            count += 1;
        }
        // 64 vendored files, each also registered under its family's aliases where it has any.
        assert!(count > 64);
    }

    #[test]
    fn the_ms_core_families_all_resolve() {
        for ms in [
            "Calibri",
            "Cambria",
            "Arial",
            "Times New Roman",
            "Courier New",
            "Georgia",
            "Comic Sans MS",
        ] {
            assert!(
                faces().any(|(name, _, _, _)| name == ms),
                "{ms} has no registered substitute"
            );
        }
    }
}
