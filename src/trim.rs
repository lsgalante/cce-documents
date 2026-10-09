//! Cut PDFium's incremental update down to what actually changed.
//!
//! PDFium's incremental save appends every object it holds in memory — and
//! loading a page at all loads its content stream and resources — so an
//! update made for one highlight also carries the page's content and fonts
//! again (285 KB for one page of the CUPS sample form). This reads the
//! original and PDFium's result with lopdf, keeps the objects that are new
//! or differ, and writes those as the update instead.
//!
//! "Differ" is judged on meaning, not bytes: a stream PDFium wrote back
//! decompressed, or compressed differently, is the same stream if its
//! dictionary (less /Length, /Filter, /DecodeParms) and decoded content
//! are.

use lopdf::{Dictionary, IncrementalDocument, Object, Stream};

/// `original` plus an update holding only the objects of `updated` (the
/// original plus PDFium's update) that are new or changed.
pub fn trim(original: &[u8], updated: &[u8]) -> Result<Vec<u8>, String> {
    let prev = lopdf::Document::load_mem(original).map_err(|e| format!("reading the original: {e}"))?;
    let full = lopdf::Document::load_mem(updated).map_err(|e| format!("reading PDFium's update: {e}"))?;
    let changed: Vec<_> = full
        .objects
        .iter()
        .filter(|(id, obj)| prev.objects.get(id).is_none_or(|old| !same(old, obj)))
        .map(|(id, obj)| (*id, obj.clone()))
        .collect();
    // The update must point back at the original's cross-reference
    // section. lopdf sets /Prev only when it read that offset itself, and
    // it does not always (it recorded 0 for a plain qpdf-written file), so
    // it is taken from the file's last `startxref`.
    let prev_xref = last_startxref(original).ok_or("the original has no startxref")?;
    let mut inc = IncrementalDocument::create_from(original.to_vec(), prev);
    inc.new_document.trailer.set("Prev", prev_xref);
    for (id, obj) in changed {
        inc.new_document.objects.insert(id, obj);
    }
    inc.new_document.max_id = inc.new_document.max_id.max(full.max_id);
    for key in [b"Root".as_slice(), b"Info".as_slice()] {
        if let Ok(v) = full.trailer.get(key) {
            inc.new_document.trailer.set(key, v.clone());
        }
    }
    let mut out = Vec::new();
    inc.save_to(&mut out).map_err(|e| format!("writing the update: {e}"))?;
    Ok(out)
}

/// The offset after a file's last `startxref` keyword.
fn last_startxref(bytes: &[u8]) -> Option<i64> {
    let at = bytes.windows(9).rposition(|w| w == b"startxref")? + 9;
    let rest = &bytes[at..];
    let digits: Vec<u8> = rest.iter().skip_while(|b| b.is_ascii_whitespace()).take_while(|b| b.is_ascii_digit()).copied().collect();
    std::str::from_utf8(&digits).ok()?.parse().ok()
}

fn same(a: &Object, b: &Object) -> bool {
    match (a, b) {
        (Object::Stream(x), Object::Stream(y)) => same_stream(x, y),
        (Object::Dictionary(x), Object::Dictionary(y)) => same_dict(x, y, &[]),
        (Object::Array(x), Object::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| same(p, q)),
        _ => a == b,
    }
}

/// Two dictionaries with the same keys and values, ignoring `skip` and
/// the order of entries.
fn same_dict(x: &Dictionary, y: &Dictionary, skip: &[&[u8]]) -> bool {
    let keys = |d: &Dictionary| d.iter().filter(|(k, _)| !skip.contains(&k.as_slice())).count();
    keys(x) == keys(y)
        && x.iter()
            .filter(|(k, _)| !skip.contains(&k.as_slice()))
            .all(|(k, v)| y.get(k).is_ok_and(|w| same(v, w)))
}

fn same_stream(x: &Stream, y: &Stream) -> bool {
    const ENCODING: &[&[u8]] = &[b"Length", b"Filter", b"DecodeParms"];
    if !same_dict(&x.dict, &y.dict, ENCODING) {
        return false;
    }
    // Bounded: a hostile file's stream must not balloon here.
    const LIMIT: usize = 512 << 20;
    match (x.get_plain_content_with_limit(LIMIT), y.get_plain_content_with_limit(LIMIT)) {
        (Ok(p), Ok(q)) => p == q,
        // An encoding lopdf cannot undo (an image codec): equal only if
        // stored identically.
        _ => x.content == y.content && same_dict(&x.dict, &y.dict, &[b"Length"]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_startxref_wins() {
        assert_eq!(last_startxref(b"...startxref\n12\n%%EOF\n...startxref\r\n672\r\n%%EOF\n"), Some(672));
        assert_eq!(last_startxref(b"no table here"), None);
    }
}
