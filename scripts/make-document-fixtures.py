"""Regenerate the document fixtures that are not plain files. Run from the repository root:

    python3 scripts/make-document-fixtures.py

Deterministic, like scripts/make-archive-fixtures.py: every zip entry carries the same fixed
date, so a regeneration is not a diff.

WHY THESE ARE BUILT AND NOT DOWNLOADED. Every one is a document format the desktop has a reader
for, and a reader is only worth testing against a file that really contains the constructs it
claims to render. A stub that opens and shows nothing teaches the reader's author that their code
works. So the OOXML and ODF files here are structurally complete packages — content types,
relationships, masters, manifests — and open in real applications, not just in ours.

THE TWO EXCEPTIONS ARE DELIBERATE. `mock-legacy.doc` and `mock-page.pages` exist to be REFUSED:
the desktop has no reader for either and must fall to its Download prompt. The refusal is decided
on the magic (doc) or the package shape (pages), and neither is parsed further, so building a
real OLE compound document would prove nothing the header does not. `mock-page.pages` is a real
zip anyway, because its shape IS the signal; `mock-legacy.doc` is a header and zeroes, and the
comment on it says so, so nobody later "fixes" it into something listable and quietly removes the
Download path's only coverage. Same call as `mock-bundle.7z`, and the opposite of the one made
for the pdf and docx, whose readers really do parse.
"""

import pathlib
import struct
import zipfile

OUT = pathlib.Path("crates/opengrok-server/src/gateway/fixtures")

# 1980-01-01, the earliest a DOS timestamp can express, so every entry is fixed.
ZIP_DATE = (1980, 1, 1, 0, 0, 0)


def write_zip(path, parts, first_stored=None):
    """Write a zip deterministically. `first_stored` is a (name, bytes) pair written FIRST and
    UNCOMPRESSED — ODF requires exactly that of its `mimetype` entry, and a reader that sniffs
    the package by reading the first entry's raw bytes gets nothing if it is deflated."""
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as archive:
        if first_stored is not None:
            name, body = first_stored
            info = zipfile.ZipInfo(name, date_time=ZIP_DATE)
            info.compress_type = zipfile.ZIP_STORED
            archive.writestr(info, body)
        for name, body in parts.items():
            info = zipfile.ZipInfo(name, date_time=ZIP_DATE)
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o644 << 16
            archive.writestr(info, body if isinstance(body, bytes) else body.encode())


XML = '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>\n'

# ---------------------------------------------------------------- pptx

P_NS = (
    'xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" '
    'xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" '
    'xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"'
)


def shape_tree(shapes):
    """A `p:spTree`, which every slide, layout and master needs whatever else it carries."""
    return f"""<p:cSld><p:spTree>
<p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr>
<p:grpSpPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/>
<a:chOff x="0" y="0"/><a:chExt cx="0" cy="0"/></a:xfrm></p:grpSpPr>
{shapes}
</p:spTree></p:cSld>"""


def text_box(ident, name, x, y, cx, cy, paragraphs, placeholder=None):
    """One text shape. `paragraphs` is a list of (indent level, text)."""
    ph = f'<p:ph type="{placeholder}"/>' if placeholder else ""
    body = "".join(
        f'<a:p><a:pPr lvl="{level}"/><a:r><a:rPr lang="en-US" dirty="0"/>'
        f"<a:t>{text}</a:t></a:r></a:p>"
        for level, text in paragraphs
    )
    return f"""<p:sp>
<p:nvSpPr><p:cNvPr id="{ident}" name="{name}"/><p:cNvSpPr><a:spLocks noGrp="1"/></p:cNvSpPr>
<p:nvPr>{ph}</p:nvPr></p:nvSpPr>
<p:spPr><a:xfrm><a:off x="{x}" y="{y}"/><a:ext cx="{cx}" cy="{cy}"/></a:xfrm>
<a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr>
<p:txBody><a:bodyPr/><a:lstStyle/>{body}</p:txBody>
</p:sp>"""


def picture(ident, name, embed, x, y, cx, cy):
    return f"""<p:pic>
<p:nvPicPr><p:cNvPr id="{ident}" name="{name}"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr>
<p:blipFill><a:blip r:embed="{embed}"/><a:stretch><a:fillRect/></a:stretch></p:blipFill>
<p:spPr><a:xfrm><a:off x="{x}" y="{y}"/><a:ext cx="{cx}" cy="{cy}"/></a:xfrm>
<a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr>
</p:pic>"""


def slide(shapes):
    return XML + f"<p:sld {P_NS}>{shape_tree(shapes)}<p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sld>"


def rels(entries):
    """`entries` is a list of (id, type-suffix, target [, mode])."""
    base = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/"
    body = "".join(
        f'<Relationship Id="{rid}" Type="{base}{kind}" Target="{target}"'
        + (f' TargetMode="{mode[0]}"' if mode else "")
        + "/>"
        for rid, kind, target, *mode in entries
    )
    return (
        XML
        + '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">'
        + body
        + "</Relationships>"
    )


# A theme is required by the schema and is mostly boilerplate; this is the smallest one that
# names every scheme a master may reference.
THEME = (
    XML
    + """<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="Mock">
<a:themeElements>
<a:clrScheme name="Mock"><a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1>
<a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1>
<a:dk2><a:srgbClr val="1F1F1F"/></a:dk2><a:lt2><a:srgbClr val="EEEEEE"/></a:lt2>
<a:accent1><a:srgbClr val="4472C4"/></a:accent1><a:accent2><a:srgbClr val="ED7D31"/></a:accent2>
<a:accent3><a:srgbClr val="A5A5A5"/></a:accent3><a:accent4><a:srgbClr val="FFC000"/></a:accent4>
<a:accent5><a:srgbClr val="5B9BD5"/></a:accent5><a:accent6><a:srgbClr val="70AD47"/></a:accent6>
<a:hlink><a:srgbClr val="0563C1"/></a:hlink><a:folHlink><a:srgbClr val="954F72"/></a:folHlink>
</a:clrScheme>
<a:fontScheme name="Mock">
<a:majorFont><a:latin typeface="Helvetica"/><a:ea typeface=""/><a:cs typeface=""/></a:majorFont>
<a:minorFont><a:latin typeface="Helvetica"/><a:ea typeface=""/><a:cs typeface=""/></a:minorFont>
</a:fontScheme>
<a:fmtScheme name="Mock">
<a:fillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
<a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
<a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:fillStyleLst>
<a:lnStyleLst>
<a:ln w="6350"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln>
<a:ln w="12700"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln>
<a:ln w="19050"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln></a:lnStyleLst>
<a:effectStyleLst><a:effectStyle><a:effectLst/></a:effectStyle>
<a:effectStyle><a:effectLst/></a:effectStyle>
<a:effectStyle><a:effectLst/></a:effectStyle></a:effectStyleLst>
<a:bgFillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
<a:solidFill><a:schemeClr val="phClr"/></a:solidFill>
<a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:bgFillStyleLst>
</a:fmtScheme>
</a:themeElements><a:objectDefaults/><a:extraClrSchemeLst/></a:theme>"""
)

CLR_MAP = (
    '<p:clrMap bg1="lt1" tx1="dk1" bg2="lt2" tx2="dk2" accent1="accent1" accent2="accent2" '
    'accent3="accent3" accent4="accent4" accent5="accent5" accent6="accent6" hlink="hlink" '
    'folHlink="folHlink"/>'
)


def write_pptx(path, png):
    slide1 = slide(
        text_box(2, "Title", 685800, 2130425, 7772400, 1470025,
                 [(0, "Mock deck")], placeholder="ctrTitle")
        + text_box(3, "Subtitle", 1371600, 3886200, 6400800, 1752600,
                   [(0, "Three slides, built by scripts/make-document-fixtures.py")],
                   placeholder="subTitle")
    )
    # Nested bullets: the outline reader has to keep the levels, not flatten them.
    slide2 = slide(
        text_box(2, "Title", 685800, 457200, 7772400, 1143000,
                 [(0, "What this slide is for")], placeholder="title")
        + text_box(3, "Content", 685800, 1600200, 7772400, 4114800, [
            (0, "A first-level bullet"),
            (1, "A second-level bullet under it"),
            (2, "A third level, to prove nesting survives"),
            (0, "Back to the first level"),
            (1, "One more child"),
        ], placeholder="body")
    )
    # An image and speaker notes, the two things a text-only outline would silently drop.
    slide3 = slide(
        text_box(2, "Title", 685800, 457200, 7772400, 1143000,
                 [(0, "A slide with a picture")], placeholder="title")
        + picture(4, "Mock image", "rId2", 685800, 1600200, 2743200, 2743200)
        + text_box(5, "Caption", 3810000, 1600200, 4648200, 1143000,
                   [(0, "The picture is ppt/media/image1.png, a real PNG.")])
    )
    notes = XML + f"""<p:notes {P_NS}>{shape_tree(
        text_box(2, "Notes", 0, 0, 6858000, 4571999, [
            (0, "Speaker notes for slide three."),
            (0, "A reader that shows only slide bodies will not show this line."),
        ], placeholder="body")
    )}<p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:notes>"""

    master = XML + f"""<p:sldMaster {P_NS}>{shape_tree("")}{CLR_MAP}
<p:sldLayoutIdLst><p:sldLayoutId id="2147483649" r:id="rId1"/></p:sldLayoutIdLst>
</p:sldMaster>"""
    layout = XML + f"""<p:sldLayout {P_NS} type="obj" preserve="1">{shape_tree("")}
<p:clrMapOvr><a:masterClrMapping/></p:clrMapOvr></p:sldLayout>"""
    notes_master = XML + f"""<p:notesMaster {P_NS}>{shape_tree("")}{CLR_MAP}</p:notesMaster>"""

    ct = XML + """<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Default Extension="png" ContentType="image/png"/>
<Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>
<Override PartName="/ppt/slideMasters/slideMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideMaster+xml"/>
<Override PartName="/ppt/slideLayouts/slideLayout1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slideLayout+xml"/>
<Override PartName="/ppt/notesMasters/notesMaster1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.notesMaster+xml"/>
<Override PartName="/ppt/slides/slide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
<Override PartName="/ppt/slides/slide2.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
<Override PartName="/ppt/slides/slide3.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.slide+xml"/>
<Override PartName="/ppt/notesSlides/notesSlide1.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.notesSlide+xml"/>
<Override PartName="/ppt/theme/theme1.xml" ContentType="application/vnd.openxmlformats-officedocument.theme+xml"/>
</Types>"""

    presentation = XML + f"""<p:presentation {P_NS}>
<p:sldMasterIdLst><p:sldMasterId id="2147483648" r:id="rId1"/></p:sldMasterIdLst>
<p:notesMasterIdLst><p:notesMasterId r:id="rId5"/></p:notesMasterIdLst>
<p:sldIdLst><p:sldId id="256" r:id="rId2"/><p:sldId id="257" r:id="rId3"/>
<p:sldId id="258" r:id="rId4"/></p:sldIdLst>
<p:sldSz cx="9144000" cy="6858000"/><p:notesSz cx="6858000" cy="9144000"/>
</p:presentation>"""

    write_zip(path, {
        "[Content_Types].xml": ct,
        "_rels/.rels": rels([("rId1", "officeDocument", "ppt/presentation.xml")]),
        "ppt/presentation.xml": presentation,
        "ppt/_rels/presentation.xml.rels": rels([
            ("rId1", "slideMaster", "slideMasters/slideMaster1.xml"),
            ("rId2", "slide", "slides/slide1.xml"),
            ("rId3", "slide", "slides/slide2.xml"),
            ("rId4", "slide", "slides/slide3.xml"),
            ("rId5", "notesMaster", "notesMasters/notesMaster1.xml"),
            ("rId6", "theme", "theme/theme1.xml"),
        ]),
        "ppt/slideMasters/slideMaster1.xml": master,
        "ppt/slideMasters/_rels/slideMaster1.xml.rels": rels([
            ("rId1", "slideLayout", "../slideLayouts/slideLayout1.xml"),
            ("rId2", "theme", "../theme/theme1.xml"),
        ]),
        "ppt/slideLayouts/slideLayout1.xml": layout,
        "ppt/slideLayouts/_rels/slideLayout1.xml.rels": rels([
            ("rId1", "slideMaster", "../slideMasters/slideMaster1.xml"),
        ]),
        "ppt/notesMasters/notesMaster1.xml": notes_master,
        "ppt/notesMasters/_rels/notesMaster1.xml.rels": rels([
            ("rId1", "theme", "../theme/theme1.xml"),
        ]),
        "ppt/slides/slide1.xml": slide1,
        "ppt/slides/_rels/slide1.xml.rels": rels([
            ("rId1", "slideLayout", "../slideLayouts/slideLayout1.xml"),
        ]),
        "ppt/slides/slide2.xml": slide2,
        "ppt/slides/_rels/slide2.xml.rels": rels([
            ("rId1", "slideLayout", "../slideLayouts/slideLayout1.xml"),
        ]),
        "ppt/slides/slide3.xml": slide3,
        "ppt/slides/_rels/slide3.xml.rels": rels([
            ("rId1", "slideLayout", "../slideLayouts/slideLayout1.xml"),
            ("rId2", "image", "../media/image1.png"),
        ]),
        "ppt/notesSlides/notesSlide1.xml": notes,
        "ppt/notesSlides/_rels/notesSlide1.xml.rels": rels([
            ("rId1", "notesMaster", "../notesMasters/notesMaster1.xml"),
            ("rId2", "slide", "../slides/slide3.xml"),
        ]),
        "ppt/theme/theme1.xml": THEME,
        "ppt/media/image1.png": png,
    })


# ---------------------------------------------------------------- ODF

ODF_NS = (
    'xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" '
    'xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" '
    'xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" '
    'xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" '
    'xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" '
    'xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" '
    'xmlns:xlink="http://www.w3.org/1999/xlink" '
    'xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" '
    'xmlns:presentation="urn:oasis:names:tc:opendocument:xmlns:presentation:1.0"'
)


def odf_manifest(media_type, extra=()):
    entries = "".join(
        f'<manifest:file-entry manifest:full-path="{p}" manifest:media-type="{m}"/>'
        for p, m in extra
    )
    return XML + f"""<manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.3">
<manifest:file-entry manifest:full-path="/" manifest:media-type="{media_type}"/>
<manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/>
<manifest:file-entry manifest:full-path="styles.xml" manifest:media-type="text/xml"/>
<manifest:file-entry manifest:full-path="meta.xml" manifest:media-type="text/xml"/>
{entries}</manifest:manifest>"""


ODF_META = XML + f"""<office:document-meta {ODF_NS} office:version="1.3">
<office:meta><dc:title xmlns:dc="http://purl.org/dc/elements/1.1/">Mock document</dc:title>
</office:meta></office:document-meta>"""

ODF_STYLES = XML + f"""<office:document-styles {ODF_NS} office:version="1.3">
<office:styles>
<style:style style:name="Standard" style:family="paragraph"/>
<style:style style:name="Bold" style:family="text">
<style:text-properties fo:font-weight="bold"/></style:style>
<style:style style:name="Italic" style:family="text">
<style:text-properties fo:font-style="italic"/></style:style>
</office:styles>
<office:automatic-styles/><office:master-styles/></office:document-styles>"""


def write_odt(path):
    body = """<text:h text:style-name="Heading_20_1" text:outline-level="1">Mock notes</text:h>
<text:p>A paragraph with a <text:span text:style-name="Bold">bold run</text:span>, an <text:span text:style-name="Italic">italic run</text:span>, and a <text:a xlink:href="https://example.com/">hyperlink</text:a>.</text:p>
<text:h text:style-name="Heading_20_2" text:outline-level="2">Lists</text:h>
<text:list text:style-name="Bullets">
<text:list-item><text:p>First bullet</text:p></text:list-item>
<text:list-item><text:p>Second bullet</text:p></text:list-item>
<text:list-item><text:p>Third bullet</text:p></text:list-item>
</text:list>
<text:list text:style-name="Numbers">
<text:list-item><text:p>Step one</text:p></text:list-item>
<text:list-item><text:p>Step two</text:p></text:list-item>
</text:list>
<text:h text:style-name="Heading_20_2" text:outline-level="2">A table</text:h>
<table:table table:name="Mock" table:style-name="Mock">
<table:table-column table:number-columns-repeated="3"/>
<table:table-row>
<table:table-cell office:value-type="string"><text:p>Column A</text:p></table:table-cell>
<table:table-cell office:value-type="string"><text:p>Column B</text:p></table:table-cell>
<table:table-cell office:value-type="string"><text:p>Column C</text:p></table:table-cell>
</table:table-row>
<table:table-row>
<table:table-cell office:value-type="string"><text:p>one</text:p></table:table-cell>
<table:table-cell office:value-type="string"><text:p>two</text:p></table:table-cell>
<table:table-cell office:value-type="string"><text:p>three</text:p></table:table-cell>
</table:table-row>
</table:table>"""
    content = XML + f"""<office:document-content {ODF_NS} office:version="1.3">
<office:automatic-styles>
<text:list-style style:name="Bullets"><text:list-level-style-bullet text:level="1"
text:bullet-char="•"/></text:list-style>
<text:list-style style:name="Numbers"><text:list-level-style-number text:level="1"
style:num-format="1"/></text:list-style>
</office:automatic-styles>
<office:body><office:text>{body}</office:text></office:body></office:document-content>"""
    write_zip(
        path,
        {
            "META-INF/manifest.xml": odf_manifest("application/vnd.oasis.opendocument.text"),
            "content.xml": content,
            "styles.xml": ODF_STYLES,
            "meta.xml": ODF_META,
        },
        first_stored=("mimetype", b"application/vnd.oasis.opendocument.text"),
    )


def write_odp(path):
    def page(name, title, bullets):
        items = "".join(f"<text:p>{b}</text:p>" for b in bullets)
        return f"""<draw:page draw:name="{name}" draw:master-page-name="Default">
<draw:frame presentation:class="title" svg:width="20cm" svg:height="3cm" svg:x="2cm" svg:y="1cm">
<draw:text-box><text:p>{title}</text:p></draw:text-box></draw:frame>
<draw:frame presentation:class="outline" svg:width="20cm" svg:height="10cm" svg:x="2cm" svg:y="5cm">
<draw:text-box>{items}</draw:text-box></draw:frame>
</draw:page>"""

    content = XML + f"""<office:document-content {ODF_NS} office:version="1.3">
<office:automatic-styles/>
<office:body><office:presentation>
{page("Slide 1", "Mock deck, ODF edition", [
    "The first slide's first bullet",
    "Its second bullet",
])}
{page("Slide 2", "The second slide", [
    "Another bullet",
    "And one more, so the count differs from slide one",
    "Three here, two there",
])}
</office:presentation></office:body></office:document-content>"""
    write_zip(
        path,
        {
            "META-INF/manifest.xml": odf_manifest(
                "application/vnd.oasis.opendocument.presentation"
            ),
            "content.xml": content,
            "styles.xml": ODF_STYLES,
            "meta.xml": ODF_META,
        },
        first_stored=(
            "mimetype",
            b"application/vnd.oasis.opendocument.presentation",
        ),
    )


# ---------------------------------------------------------------- rtf

# `\u233?` is the RTF unicode escape: the code point, then an ASCII fallback character a reader
# without unicode support shows instead. A reader that strips control words carelessly leaves the
# stray `?` behind, which is exactly the bug this line is here to make visible.
RTF = (
    r"{\rtf1\ansi\ansicpg1252\deff0{\fonttbl{\f0\fswiss Helvetica;}}"
    "\n"
    r"{\colortbl;\red0\green0\blue0;}"
    "\n"
    r"\b\fs36 Mock rich text\b0\fs24\par"
    "\n"
    r"\par"
    "\n"
    r"The first paragraph carries a \b bold run\b0 , an \i italic run\i0 , a tab:\tab after it, "
    r"and a unicode escape for e-acute: \u233?.\par"
    "\n"
    r"\par"
    "\n"
    r"The second paragraph exists so a reader that drops paragraph breaks runs the two together "
    r"visibly, rather than looking correct on a single-paragraph file.\par"
    "\n"
    "}"
)


# ---------------------------------------------------------------- the two refusals


def write_doc(path):
    """A .doc HEADER and nothing else — see this module's docstring. 512 bytes: the OLE compound
    file magic, the version and byte-order fields `file(1)` reads, and zeroes. It is meant to be
    refused on the magic; if anything ever tries to read the directory it will fail cleanly rather
    than walk into garbage."""
    header = bytearray(512)
    header[0:8] = bytes([0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1])  # magic
    header[24:26] = struct.pack("<H", 0x003E)  # minor version
    header[26:28] = struct.pack("<H", 0x0003)  # major version 3
    header[28:30] = struct.pack("<H", 0xFFFE)  # little-endian marker
    header[30:32] = struct.pack("<H", 9)  # 512-byte sectors
    header[32:34] = struct.pack("<H", 6)  # 64-byte mini sectors
    header[44:48] = struct.pack("<I", 1)  # one FAT sector
    header[48:52] = struct.pack("<I", 0xFFFFFFFE)  # no directory sector
    header[60:64] = struct.pack("<I", 0xFFFFFFFE)  # no mini FAT
    header[68:72] = struct.pack("<I", 0xFFFFFFFE)  # no DIFAT
    path.write_bytes(bytes(header))


def write_pages(path):
    """A real zip shaped like a .pages bundle — `Index/` and `Metadata/`, which is what a reader
    keys on to recognise the format it then declines to open."""
    write_zip(path, {
        "Index/Document.iwa": bytes([0x00, 0x00, 0x12, 0x08]) + b"mock iwa payload, not real",
        "Index/Metadata.iwa": bytes([0x00, 0x00, 0x12, 0x08]) + b"mock iwa payload, not real",
        "Metadata/Properties.plist": """<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>documentVersion</key><string>1</string>
<key>fileFormatVersion</key><string>13.2</string>
</dict></plist>""",
        "Metadata/BuildVersionHistory.plist": """<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><array><string>Mock fixture, not produced by Pages</string></array></plist>""",
    })


if __name__ == "__main__":
    write_pptx(OUT / "mock-deck.pptx", (OUT / "mock-image.png").read_bytes())
    write_odt(OUT / "mock-notes.odt")
    write_odp(OUT / "mock-deck.odp")
    (OUT / "mock-rich.rtf").write_text(RTF)
    write_doc(OUT / "mock-legacy.doc")
    write_pages(OUT / "mock-page.pages")
    for name in (
        "mock-deck.pptx",
        "mock-notes.odt",
        "mock-deck.odp",
        "mock-rich.rtf",
        "mock-legacy.doc",
        "mock-page.pages",
    ):
        print(f"{name:22} {(OUT / name).stat().st_size:6} bytes")
