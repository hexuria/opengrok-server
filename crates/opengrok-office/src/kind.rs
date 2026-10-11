/// Which OOXML flavour a file is. The caller labels it — office_open carries the path it read
/// from the box — and the engine trusts that label rather than re-sniffing the zip's
/// `[Content_Types].xml`, because the tools already reject the mismatched file at open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Docx,
    Xlsx,
    Pptx,
}

impl Kind {
    pub fn from_filename(name: &str) -> Option<Self> {
        match name.rsplit('.').next()?.to_ascii_lowercase().as_str() {
            "docx" => Some(Self::Docx),
            "xlsx" => Some(Self::Xlsx),
            "pptx" => Some(Self::Pptx),
            _ => None,
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Docx => "docx",
            Self::Xlsx => "xlsx",
            Self::Pptx => "pptx",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_map_and_round_trip() {
        assert_eq!(Kind::from_filename("Report.DOCX"), Some(Kind::Docx));
        assert_eq!(
            Kind::from_filename("~/office/budget.xlsx"),
            Some(Kind::Xlsx)
        );
        assert_eq!(Kind::from_filename("deck.pptx"), Some(Kind::Pptx));
        assert_eq!(Kind::from_filename("notes.doc"), None);
        assert_eq!(Kind::from_filename("no-extension"), None);
    }
}
