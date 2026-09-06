"""Regenerate the four archive fixtures. Run from the repository root:

    python3 scripts/make-archive-fixtures.py

Deterministic on purpose — fixed zip dates and mtime=0 in both gzip headers — so re-running
produces byte-identical files and a regeneration is not a diff. The zip and the tar.gz carry the
SAME tree so their listings can be read against each other."""
import gzip, io, tarfile, zipfile, pathlib

OUT = pathlib.Path("crates/opengrok-server/src/gateway/fixtures")

# The tree, shared by the zip and the tar.gz so the two listings can be compared side by side.
# Two folders plus a file at the root, four files in all, and enough bytes that the size column
# is not all "1 B".
TREE = {
    "bundle/README.md": """# mock bundle

A fixture archive. It exists so the client's archive listing has a real central directory to
read: four entries, two folders, and sizes that differ enough to tell the columns apart.

Nothing in here is executed. The files are plausible rather than meaningful.
""",
    "bundle/src/main.rs": """//! The entry point of a program that does not exist.

fn main() {
    let names = ["ada", "grace", "katherine"];
    for (index, name) in names.iter().enumerate() {
        println!("{index}: {name}");
    }
    println!("{}", greeting("world"));
}

/// Said once, at the end.
fn greeting(who: &str) -> String {
    format!("hello, {who}")
}

#[cfg(test)]
mod tests {
    use super::greeting;

    #[test]
    fn it_greets() {
        assert_eq!(greeting("world"), "hello, world");
    }
}
""",
    "bundle/src/lib.rs": """//! A second file in the same folder, so the listing has a folder with more than one child.

/// Sum a slice, the long way round, so there is something to look at.
pub fn total(values: &[i64]) -> i64 {
    let mut sum = 0;
    for value in values {
        sum += value;
    }
    sum
}

/// The largest value, or None for an empty slice.
pub fn largest(values: &[i64]) -> Option<i64> {
    values.iter().copied().max()
}
""",
    "bundle/docs/notes.txt": """notes
=====

A plain text member, in a second folder, with no extension the viewer treats specially.

Tabs are used below on purpose:

\tone\ttwo\tthree
\tfour\tfive\tsix

The last line has no trailing newline character after it.""",
}

# 1980-01-01 00:00:00, the zip epoch — the earliest a DOS timestamp can express.
ZIP_DATE = (1980, 1, 1, 0, 0, 0)

def write_zip(path):
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as archive:
        for name, body in TREE.items():
            info = zipfile.ZipInfo(name, date_time=ZIP_DATE)
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o644 << 16
            archive.writestr(info, body)
    path.write_bytes(buf.getvalue())

def write_targz(path):
    tar = io.BytesIO()
    with tarfile.open(fileobj=tar, mode="w", format=tarfile.GNU_FORMAT) as archive:
        for name, body in TREE.items():
            raw = body.encode()
            info = tarfile.TarInfo(name)
            info.size = len(raw)
            info.mtime = 0
            info.mode = 0o644
            info.uid = info.gid = 0
            info.uname = info.gname = ""
            archive.addfile(info, io.BytesIO(raw))
    # mtime=0 in the gzip header too, or every regeneration is a diff.
    out = io.BytesIO()
    with gzip.GzipFile(fileobj=out, mode="wb", mtime=0) as gz:
        gz.write(tar.getvalue())
    path.write_bytes(out.getvalue())

def write_gz(path, source):
    out = io.BytesIO()
    with gzip.GzipFile(fileobj=out, mode="wb", mtime=0, filename="") as gz:
        gz.write(source.read_bytes())
    path.write_bytes(out.getvalue())

def write_7z(path):
    # SIGNATURE ONLY, and that is the whole point of this one: the client refuses 7z and offers
    # Download instead of a listing, so it never parses past the magic. A real 7z would need a
    # compressor we do not have and would prove nothing extra.
    #
    # The bytes after the magic are a v0.4 header with a zeroed start-header CRC — enough that
    # `file(1)` says "7-zip archive data", not enough to open. Anything that DOES try to read it
    # will fail cleanly rather than read garbage.
    magic = bytes([0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C, 0x00, 0x04])
    path.write_bytes(magic + bytes(24))

write_zip(OUT / "mock-bundle.zip")
write_targz(OUT / "mock-bundle.tar.gz")
write_gz(OUT / "mock-notes.md.gz", OUT / "mock-notes.md")
write_7z(OUT / "mock-bundle.7z")
for name in ("mock-bundle.zip", "mock-bundle.tar.gz", "mock-notes.md.gz", "mock-bundle.7z"):
    print(name, (OUT / name).stat().st_size, "bytes")
