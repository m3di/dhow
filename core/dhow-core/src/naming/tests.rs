//! T-3 naming table (ENGINE-SPEC §3, §15).

use super::*;

struct Case {
    name: &'static str,
    cd: Option<&'static str>,
    ct: Option<&'static str>,
    url: &'static str,
    /// `None`: same as `url` (no redirect).
    original: Option<&'static str>,
    fs: TargetFs,
    existing: &'static [&'static str],
    expect: &'static str,
}

const BASE: Case = Case {
    name: "",
    cd: None,
    ct: None,
    url: "https://example.com/files/report.pdf",
    original: None,
    fs: TargetFs::Posix,
    existing: &[],
    expect: "",
};

const W: TargetFs = TargetFs::Windows;

#[rustfmt::skip]
const CASES: &[Case] = &[
    // §3 step 1: filename* (RFC 8187)
    Case { name: "filename* beats filename", cd: Some("attachment; filename*=UTF-8''na%C3%AFve%20file.txt; filename=\"naive file.txt\""), expect: "naïve file.txt", ..BASE },
    Case { name: "filename* after filename still wins", cd: Some("attachment; filename=\"plain.txt\"; filename*=UTF-8''fancy.txt"), expect: "fancy.txt", ..BASE },
    Case { name: "filename* ISO-8859-1", cd: Some("attachment; filename*=iso-8859-1'en'%A3%20rates.txt"), expect: "£ rates.txt", ..BASE },
    Case { name: "filename* charset case-insensitive", cd: Some("attachment; filename*=utf-8''a.txt"), expect: "a.txt", ..BASE },
    Case { name: "filename* other charset rejected", cd: Some("attachment; filename*=Shift_JIS''%82%A0.txt; filename=\"fallback.txt\""), expect: "fallback.txt", ..BASE },
    Case { name: "filename* invalid UTF-8 rejected", cd: Some("attachment; filename*=UTF-8''%FF.txt; filename=\"ok.txt\""), expect: "ok.txt", ..BASE },
    Case { name: "filename* malformed escape rejected", cd: Some("attachment; filename*=UTF-8''%G1.txt"), expect: "report.pdf", ..BASE },
    Case { name: "filename* traversal stripped", cd: Some("attachment; filename*=UTF-8''..%2F..%2Fetc%2Fpasswd"), expect: "passwd", ..BASE },
    Case { name: "filename* control chars stripped", cd: Some("attachment; filename*=UTF-8''%07bell%0A.txt"), expect: "bell.txt", ..BASE },
    // §3 step 2: filename (RFC 6266)
    Case { name: "quoted filename", cd: Some("attachment; filename=\"Quarterly Report.xlsx\""), expect: "Quarterly Report.xlsx", ..BASE },
    Case { name: "unquoted filename", cd: Some("attachment; filename=data.csv"), expect: "data.csv", ..BASE },
    Case { name: "inline disposition", cd: Some("inline; filename=\"view.pdf\""), expect: "view.pdf", ..BASE },
    Case { name: "escaped quotes kept on posix", cd: Some(r#"attachment; filename="say \"hi\".txt""#), expect: "say \"hi\".txt", ..BASE },
    Case { name: "quotes stripped on windows", cd: Some(r#"attachment; filename="say \"hi\".txt""#), fs: W, expect: "say hi.txt", ..BASE },
    Case { name: "unix path stripped", cd: Some("attachment; filename=\"/etc/passwd\""), expect: "passwd", ..BASE },
    Case { name: "windows path stripped", cd: Some(r#"attachment; filename="C:\Users\x\evil.exe""#), expect: "evil.exe", ..BASE },
    Case { name: "traversal to dotfile", cd: Some("attachment; filename=\"../../.bashrc\""), expect: "bashrc", ..BASE },
    Case { name: "raw UTF-8 filename", cd: Some("attachment; filename=\"日本語.zip\""), expect: "日本語.zip", ..BASE },
    Case { name: "surrounding whitespace trimmed", cd: Some("attachment; filename=\"  spaced.txt  \""), expect: "spaced.txt", ..BASE },
    Case { name: "empty filename falls through", cd: Some("attachment; filename=\"\""), expect: "report.pdf", ..BASE },
    Case { name: "dots-only filename falls through", cd: Some("attachment; filename=\"...\""), expect: "report.pdf", ..BASE },
    Case { name: "no filename param", cd: Some("attachment"), expect: "report.pdf", ..BASE },
    // §3 steps 3–4: URL path
    Case { name: "query ignored", url: "https://cdn.example.com/a/b/movie.mkv?sig=abc&n=1.zip", expect: "movie.mkv", ..BASE },
    Case { name: "percent-decoded", url: "https://example.com/my%20file%20(1).zip", expect: "my file (1).zip", ..BASE },
    Case { name: "UTF-8 percent-decoded", url: "https://example.com/%D9%81%D8%A7%DB%8C%D9%84.pdf", expect: "فایل.pdf", ..BASE },
    Case { name: "Latin-1 fallback", url: "https://example.com/caf%E9.txt", expect: "café.txt", ..BASE },
    Case { name: "stray percent kept", url: "https://example.com/50%off.txt", expect: "50%off.txt", ..BASE },
    Case { name: "encoded slash removed", url: "https://example.com/a%2Fb.txt", expect: "ab.txt", ..BASE },
    Case { name: "trailing slash uses last segment", url: "https://example.com/releases/v2/", expect: "v2", ..BASE },
    Case { name: "effective URL beats original", url: "https://cdn.example.com/blobs/abc123.bin", original: Some("https://example.com/download/app.dmg"), expect: "abc123.bin", ..BASE },
    Case { name: "original when effective has no path", url: "https://cdn.example.com/", original: Some("https://example.com/get/app.dmg"), expect: "app.dmg", ..BASE },
    Case { name: "backslash kept on posix", url: "https://example.com/a%5Cb.txt", expect: "a\\b.txt", ..BASE },
    Case { name: "backslash stripped on windows", url: "https://example.com/a%5Cb.txt", fs: W, expect: "ab.txt", ..BASE },
    // §3 step 5: host
    Case { name: "host fallback", url: "https://example.com/", expect: "example.com-download", ..BASE },
    Case { name: "host fallback gets type extension", url: "https://example.com/", ct: Some("application/pdf"), expect: "example.com-download.pdf", ..BASE },
    Case { name: "IP host fallback", url: "http://192.168.1.10/", expect: "192.168.1.10-download", ..BASE },
    // Content-Type extension
    Case { name: "type extension appended", url: "https://example.com/download", ct: Some("application/pdf"), expect: "download.pdf", ..BASE },
    Case { name: "type params ignored", url: "https://example.com/notes", ct: Some("text/plain; charset=utf-8"), expect: "notes.txt", ..BASE },
    Case { name: "curated jpeg", url: "https://example.com/image", ct: Some("image/jpeg"), expect: "image.jpg", ..BASE },
    Case { name: "octet-stream appends nothing", url: "https://example.com/blob", ct: Some("application/octet-stream"), expect: "blob", ..BASE },
    Case { name: "existing extension kept", url: "https://example.com/photo.png", ct: Some("image/jpeg"), expect: "photo.png", ..BASE },
    Case { name: "numeric extension counts", url: "https://example.com/backup.001", ct: Some("application/zip"), expect: "backup.001", ..BASE },
    Case { name: "spaced suffix is not an extension", url: "https://example.com/Movie.Final%20Cut", ct: Some("video/mp4"), expect: "Movie.Final Cut.mp4", ..BASE },
    Case { name: "compound extension kept", url: "https://example.com/src.tar.gz", ct: Some("application/gzip"), expect: "src.tar.gz", ..BASE },
    Case { name: "leading dot stripped, then extension", url: "https://example.com/.env", ct: Some("text/plain"), expect: "env.txt", ..BASE },
    // Windows sanitizing
    Case { name: "windows forbidden chars", cd: Some("attachment; filename=\"a<b>:c|d?*.txt\""), fs: W, expect: "abcd.txt", ..BASE },
    Case { name: "same chars fine on posix", cd: Some("attachment; filename=\"a<b>:c|d?*.txt\""), expect: "a<b>:c|d?*.txt", ..BASE },
    Case { name: "windows trailing dots and spaces", cd: Some("attachment; filename=\"notes.txt. . \""), fs: W, expect: "notes.txt", ..BASE },
    Case { name: "reserved name", cd: Some("attachment; filename=CON"), fs: W, expect: "_CON", ..BASE },
    Case { name: "reserved name with extension", cd: Some("attachment; filename=con.txt"), fs: W, expect: "_con.txt", ..BASE },
    Case { name: "reserved with compound extension", cd: Some("attachment; filename=LPT1.tar.gz"), fs: W, expect: "_LPT1.tar.gz", ..BASE },
    Case { name: "reserved with space before dot", cd: Some("attachment; filename=\"AUX .txt\""), fs: W, expect: "_AUX .txt", ..BASE },
    Case { name: "reserved after type extension", url: "https://example.com/nul", ct: Some("text/plain"), fs: W, expect: "_nul.txt", ..BASE },
    Case { name: "COM10 not reserved", cd: Some("attachment; filename=COM10.txt"), fs: W, expect: "COM10.txt", ..BASE },
    Case { name: "CONSOLE not reserved", cd: Some("attachment; filename=CONSOLE.txt"), fs: W, expect: "CONSOLE.txt", ..BASE },
    Case { name: "reserved fine on posix", cd: Some("attachment; filename=CON.txt"), expect: "CON.txt", ..BASE },
    // Collisions
    Case { name: "collision", existing: &["report.pdf"], expect: "report (2).pdf", ..BASE },
    Case { name: "second collision", existing: &["report.pdf", "report (2).pdf"], expect: "report (3).pdf", ..BASE },
    Case { name: "part file collides", existing: &["report.pdf.part"], expect: "report (2).pdf", ..BASE },
    Case { name: "sidecar collides", existing: &["report.pdf.dhow"], expect: "report (2).pdf", ..BASE },
    Case { name: "collision ignores case", existing: &["REPORT.PDF"], expect: "report (2).pdf", ..BASE },
    Case { name: "compound collision", url: "https://example.com/src.tar.gz", existing: &["src.tar.gz"], expect: "src (2).tar.gz", ..BASE },
    Case { name: "collision without extension", url: "https://example.com/README", existing: &["README"], expect: "README (2)", ..BASE },
    Case { name: "similar names don't collide", existing: &["report.pdf.bak", "report"], expect: "report.pdf", ..BASE },
];

#[test]
fn naming_table() {
    assert!(CASES.len() >= 50, "T-3 requires 50+ cases");
    let mut failures = Vec::new();
    for c in CASES {
        let url = Url::parse(c.url).unwrap();
        let original = Url::parse(c.original.unwrap_or(c.url)).unwrap();
        let base = base_name(c.cd, c.ct, &url, &original, c.fs);
        let got = deduplicate(&base, c.existing);
        if got != c.expect {
            failures.push(format!(
                "{}: expected {:?}, got {:?}",
                c.name, c.expect, got
            ));
        }
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

#[test]
fn case_names_are_unique() {
    let mut names: Vec<_> = CASES.iter().map(|c| c.name).collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), CASES.len());
}

fn url(path: &str) -> Url {
    Url::parse(&format!("https://example.com/{path}")).unwrap()
}

#[test]
fn long_stem_is_truncated_keeping_extension() {
    let u = url(&format!("{}.pdf", "a".repeat(300)));
    let name = base_name(None, None, &u, &u, TargetFs::Posix);
    assert_eq!(name.len(), MAX_NAME_BYTES);
    assert!(name.ends_with("a.pdf"));
}

#[test]
fn truncation_respects_char_boundaries() {
    let u = url(&format!("{}.txt", "%C3%A9".repeat(200)));
    let name = base_name(None, None, &u, &u, TargetFs::Posix);
    assert!(name.len() <= MAX_NAME_BYTES);
    assert_eq!(name, format!("{}.txt", "é".repeat(121)));
}

#[test]
fn truncation_does_not_leave_trailing_dot_or_space() {
    // The 242-byte budget cuts right after the space.
    let stem = format!("{} b", "a".repeat(241));
    let u = url(&format!("{stem}xxxxxxxxxx.zip"));
    let name = base_name(None, None, &u, &u, TargetFs::Windows);
    assert_eq!(name, format!("{}.zip", "a".repeat(241)));
}

#[test]
fn collision_suffix_fits_the_limit() {
    let u = url(&format!("{}.pdf", "a".repeat(300)));
    let base = base_name(None, None, &u, &u, TargetFs::Posix);
    let next = deduplicate(&base, std::slice::from_ref(&base));
    assert_eq!(next.len(), MAX_NAME_BYTES);
    assert!(next.ends_with(" (2).pdf"));
}

#[test]
fn suggest_name_uses_probe_fields() {
    let info = ProbeInfo {
        effective_url: url("x/y.bin"),
        status: 200,
        total_size: Some(1),
        resumable: crate::probe::Resumable::Yes,
        etag: None,
        last_modified: None,
        content_type: Some("application/zip".into()),
        content_disposition: Some("attachment; filename=\"archive\"".into()),
        digest: None,
        suggested_name: String::new(),
    };
    let name = suggest_name(&info, &url("orig"), TargetFs::Posix, &["archive.zip"]);
    assert_eq!(name, "archive (2).zip");
}

/// ENGINE-SPEC §13.2.
#[test]
fn group_keys() {
    let key = |name| group_key(name).map(|(k, _)| k);
    for (name, expect) in [
        ("A.part1.rar", Some("a.rar")),
        ("A.Part2.rar", Some("a.rar")),
        ("A.part1 (2).rar", Some("a.rar")),
        ("B.rar", Some("b.rar")),
        ("Movie_CD-2.mkv", Some("movie.mkv")),
        ("Show S01E02.mkv", Some("show.mkv")),
        ("Show - ep03 [1080p].mkv", Some("show ep03 [1080p].mkv")),
        ("Show [01].mkv", Some("show.mkv")),
        ("Book vol.3 of 5.pdf", Some("book.pdf")),
        ("IMG_0042.JPG", Some("img.jpg")),
        ("deep 3.txt", Some("deep.txt")),
        ("photo 20240101.jpg", Some("photo 20240101.jpg")),
        ("Backup.7z.001", Some("backup 7z")),
        ("Backup.7z.002", Some("backup 7z")),
        ("Backup.7z (2).001", Some("backup 7z")),
        ("Film (1999).mkv", Some("film (1999).mkv")),
        ("Film (2021).mkv", Some("film (2021).mkv")),
        ("Clip [007].mp4", Some("clip.mp4")),
        ("notes", Some("notes")),
        ("x2.c", None),
        ("01.mp3", None),
        ("Part 1.rar", None),
    ] {
        assert_eq!(key(name).as_deref(), expect, "{name}");
    }
    assert_eq!(
        group_key("Movie.Part1.rar").unwrap().1,
        "Movie",
        "display keeps the casing"
    );
    assert_eq!(
        group_key("My.Big_Show.S02E10.mkv").unwrap().1,
        "My Big Show"
    );
}
