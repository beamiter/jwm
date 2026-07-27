//! What a clipboard offer is made of, judged without any window-management
//! policy.
//!
//! Every backend faces the same two questions before it reads a payload:
//! *may* this be recorded, and *which* of the advertised types should be
//! asked for. Both are answered from MIME names alone — X11 target atoms and
//! Wayland MIME strings are the same vocabulary — so they live here, below
//! the backends, rather than in the history that consumes the result. The
//! clipboard history re-exports them, so policy callers and backends decide
//! alike by construction.

/// Clipboard payloads larger than this are ignored: a copied image or a
/// multi-megabyte log is not something the picker can usefully show, and
/// holding fifty of them would be a real memory cost.
pub const MAX_TEXT_BYTES: usize = 256 * 1024;

/// MIME types by which an application asks clipboard managers not to store
/// what it just copied.
///
/// `x-kde-passwordManagerHint` is the de-facto standard — password managers
/// offer it alongside the text, and every clipboard manager that respects
/// privacy checks for it. Honoring it is the difference between a history
/// and a password leak.
const SECRET_HINTS: [&str; 3] = [
    "x-kde-passwordManagerHint",
    "application/x-secret",
    "x-secret",
];

/// Whether an offer is marked as a secret and must not be recorded.
///
/// The comparison is case-insensitive and matches on a suffix, because the
/// hint travels both bare and with a vendor prefix depending on the toolkit.
#[must_use]
pub fn is_secret(mime_types: &[String]) -> bool {
    mime_types.iter().any(|mime| {
        let mime = mime.to_ascii_lowercase();
        SECRET_HINTS
            .iter()
            .any(|hint| mime.ends_with(&hint.to_ascii_lowercase()))
    })
}

/// Pick the text-ish MIME type to ask for, preferring UTF-8.
///
/// Returns `None` when the offer holds nothing this history can store — an
/// image or a file list is a legitimate clipboard payload the picker simply
/// has no way to show.
#[must_use]
pub fn preferred_text_mime(mime_types: &[String]) -> Option<String> {
    const PREFERRED: [&str; 4] = [
        "text/plain;charset=utf-8",
        "UTF8_STRING",
        "text/plain",
        "STRING",
    ];
    for wanted in PREFERRED {
        if let Some(found) = mime_types
            .iter()
            .find(|mime| mime.eq_ignore_ascii_case(wanted))
        {
            return Some(found.clone());
        }
    }
    None
}
