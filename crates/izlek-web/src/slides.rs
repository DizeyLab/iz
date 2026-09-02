//! Reading an uploaded presentation into the outline the file viewer lays
//! out.
//!
//! No browser renders a deck, so like a workbook ([`crate::sheet`]) this one
//! is not a src on an element: the bytes are parsed here and come out as
//! strings the view puts on the page. Text only — a slide's images, shapes,
//! layout and theme are not carried over.
//!
//! A deck is read whole: the text of a slide is a few hundred bytes, so
//! unlike a workbook there is nothing worth streaming. Paging happens in the
//! view, one slide per page, moved by links.
//!
//! Only the parts the format fixes are opened: the slide order comes from
//! `ppt/presentation.xml`, resolved to slide parts through
//! `ppt/_rels/presentation.xml.rels`, and each slide's text is every `<a:t>`
//! run in its part. A `.ppt` — the binary OLE format — has no such parts and
//! never sniffs its way here; it stays a download.

use std::collections::HashMap;
use std::io::{Cursor, Read};

use quick_xml::Reader;
use quick_xml::escape::unescape;
use quick_xml::events::{BytesStart, Event};
use zip::ZipArchive;

/// One slide: the paragraphs of text it carries, in the order they sit on
/// the slide.
pub(crate) struct Slide {
    pub paragraphs: Vec<Paragraph>,
}

/// One paragraph of a slide. `title` marks a paragraph from a title
/// placeholder (`type="title"` or `ctrTitle`), which the view sets off as
/// the slide's heading.
pub(crate) struct Paragraph {
    pub title: bool,
    pub text: String,
}

/// The whole deck: every slide with text, in the order the presentation
/// declares, and which of them the viewer shows — the requested one, moved
/// into range the way a sheet's window is.
pub(crate) struct Deck {
    pub index: usize,
    pub slides: Vec<Slide>,
}

/// The deck the bytes hold, showing its `index`-th slide. `None` when the
/// bytes are not a deck this reader understands — a zip without the
/// presentation parts, or one truncated in transit. An out-of-range index (a
/// hand-edited query string) falls back to the last slide rather than
/// failing; a deck always has one to show.
pub(crate) fn read(bytes: Vec<u8>, index: usize) -> Option<Deck> {
    let mut archive = ZipArchive::new(Cursor::new(bytes)).ok()?;
    let parts = slide_order(&mut archive)?;
    let mut slides = Vec::with_capacity(parts.len());
    for part in parts {
        slides.push(slide(&mut archive, &part)?);
    }
    Some(Deck {
        index: index.min(slides.len() - 1),
        slides,
    })
}

/// The slide parts in presentation order: the `r:id`s of the `p:sldId`
/// entries in `ppt/presentation.xml`, resolved to part names through the
/// presentation's relationships. The list, not the filename, is the order —
/// a deck renumbered by its writer still plays the way it was saved. `None`
/// when either side of that mapping is missing or nothing resolves.
fn slide_order(archive: &mut ZipArchive<Cursor<Vec<u8>>>) -> Option<Vec<String>> {
    let presentation = entry(archive, "ppt/presentation.xml")?;
    let rels = entry(archive, "ppt/_rels/presentation.xml.rels")?;

    // The relationship ids, in the order the presentation lists them.
    let mut ids: Vec<String> = Vec::new();
    let mut reader = Reader::from_str(&presentation);
    loop {
        match &reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) if is(e, b"p:sldId") => {
                if let Some(id) = attribute(e, "r:id") {
                    ids.push(id);
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => return None,
        }
    }

    // Id -> target part. A relationship target is relative to the
    // presentation's own directory (`slides/slide1.xml`); a leading `/`
    // names the part from the package root. Both normalize to `ppt/…`.
    let mut targets: HashMap<String, String> = HashMap::new();
    let mut reader = Reader::from_str(&rels);
    loop {
        match &reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) if is(e, b"Relationship") => {
                if let (Some(id), Some(target)) = (attribute(e, "Id"), attribute(e, "Target")) {
                    let target = target.trim_start_matches('/');
                    let part = if let Some(part) = target.strip_prefix("ppt/") {
                        part.to_string()
                    } else {
                        format!("ppt/{target}")
                    };
                    targets.insert(id, part);
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => return None,
        }
    }

    let parts: Vec<String> = ids
        .into_iter()
        .filter_map(|id| targets.remove(&id))
        .collect();
    (!parts.is_empty()).then_some(parts)
}

/// The named part of the archive, decoded as UTF-8. Slide XML is UTF-8 by
/// the format's own rule, so a part that is not text is a part that is not
/// a slide.
fn entry(archive: &mut ZipArchive<Cursor<Vec<u8>>>, name: &str) -> Option<String> {
    let mut part = archive.by_name(name).ok()?;
    let mut xml = String::new();
    part.read_to_string(&mut xml).ok()?;
    Some(xml)
}

/// Every `<a:t>` run of one slide part, grouped by the `<a:p>` paragraph
/// each sits in, the runs of one paragraph joined in order — a run boundary
/// is a change of styling, not a change of words. A paragraph that comes out
/// empty is dropped: an outline lists what is on the slide, and an empty
/// line is not on it. Text inside a field (`a:fld` — a slide number, a
/// date) is dropped with it: the placeholder is not text anyone wrote.
fn slide(archive: &mut ZipArchive<Cursor<Vec<u8>>>, part: &str) -> Option<Slide> {
    let xml = entry(archive, part)?;
    let mut reader = Reader::from_str(&xml);
    let mut paragraphs: Vec<Paragraph> = Vec::new();
    // Whether the shape holding the current text is a title. A `p:ph` in the
    // shape's own header says so, and it speaks for every paragraph the
    // shape's body carries until the shape ends.
    let mut title_shape = false;
    // The paragraph being collected, as (from a title shape, text so far).
    let mut current: Option<(bool, String)> = None;
    // Inside an `a:t` run: text events outside one are the whitespace
    // between elements, and are not words.
    let mut in_run = false;
    // Inside an `a:fld` field, whose placeholder text is skipped.
    let mut in_field = false;
    loop {
        match &reader.read_event() {
            Ok(Event::Start(e)) => match e.name().as_ref() {
                b"p:sp" => title_shape = false,
                b"p:ph" => title_shape |= title_placeholder(e),
                b"a:p" => current = Some((title_shape, String::new())),
                b"a:t" => in_run = true,
                b"a:fld" => in_field = true,
                _ => {}
            },
            // A self-closing placeholder has no body of its own, but its
            // type still titles the shape's paragraphs.
            Ok(Event::Empty(e)) if is(e, b"p:ph") => title_shape |= title_placeholder(e),
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"a:p" => {
                    if let Some((title, text)) = current.take() {
                        if !text.is_empty() {
                            paragraphs.push(Paragraph { title, text });
                        }
                    }
                }
                b"a:t" => in_run = false,
                b"a:fld" => in_field = false,
                b"p:sp" => title_shape = false,
                _ => {}
            },
            // The reader splits a character reference out of the text, so
            // `&amp;` arrives here as its own event. Numeric refs resolve
            // themselves; the five the XML grammar defines are all a slide
            // can carry, and anything else is not a character at all.
            Ok(Event::GeneralRef(r)) => {
                if in_run && !in_field {
                    let ch = match r.resolve_char_ref() {
                        Ok(Some(ch)) => Some(ch),
                        Ok(None) => match std::str::from_utf8(&r) {
                            Ok("amp") => Some('&'),
                            Ok("lt") => Some('<'),
                            Ok("gt") => Some('>'),
                            Ok("quot") => Some('"'),
                            Ok("apos") => Some('\''),
                            // A reference the XML grammar does not define
                            // is a malformed document; such a deck does not
                            // open.
                            _ => return None,
                        },
                        Err(_) => return None,
                    };
                    if let Some((_, text_so_far)) = current.as_mut() {
                        if let Some(ch) = ch {
                            text_so_far.push(ch);
                        }
                    }
                }
            }
            Ok(Event::Text(t)) => {
                if in_run && !in_field {
                    let text = t.decode().ok()?;
                    if let Some((_, text_so_far)) = current.as_mut() {
                        text_so_far.push_str(&text);
                    }
                }
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    Some(Slide { paragraphs })
}

/// Whether this `p:ph` names a title. The `type` attribute is absent on the
/// default body placeholder, and only `title` and `ctrTitle` are titles.
fn title_placeholder(ph: &BytesStart) -> bool {
    matches!(ph.try_get_attribute("type"), Ok(Some(attr))
        if std::str::from_utf8(&attr.value).is_ok_and(|decoded| {
            matches!(unescape(decoded).as_deref(), Ok("title" | "ctrTitle"))
        }))
}

/// Whether this element's name is `name`, prefix included. The prefixes are
/// the format's own (`p:` for presentation parts, `a:` for drawing), fixed
/// by the schema rather than chosen by the writer.
fn is(element: &BytesStart, name: &[u8]) -> bool {
    element.name().as_ref() == name
}

/// An element attribute's value, unescaped. `None` when absent — an `r:id`
/// nobody wrote is a slide with no place in the order, not a broken deck.
fn attribute(element: &BytesStart, name: &str) -> Option<String> {
    let attr = element.try_get_attribute(name).ok().flatten()?;
    let decoded = std::str::from_utf8(&attr.value).ok()?;
    unescape(decoded).ok().map(|value| value.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    /// A deck of the named slide parts, dressed as the minimal package the
    /// format requires: a presentation listing its slides by relationship
    /// id, and the relationships naming the slide parts.
    fn deck(presentation: &str, rels: &str, slides: &[(&str, &str)]) -> Vec<u8> {
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default();
        let mut add = |name: &str, xml: &str| {
            zip.start_file(name, options).unwrap();
            zip.write_all(xml.as_bytes()).unwrap();
        };
        add("ppt/presentation.xml", presentation);
        add("ppt/_rels/presentation.xml.rels", rels);
        for (name, xml) in slides {
            add(&format!("ppt/slides/{name}.xml"), xml);
        }
        zip.finish().unwrap().into_inner()
    }

    fn presentation(order: &[&str]) -> String {
        let list: String = order
            .iter()
            .enumerate()
            .map(|(position, id)| format!(r#"<p:sldId id="{}" r:id="{id}"/>"#, 256 + position))
            .collect();
        format!(
            r#"<p:presentation xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:sldIdLst>{list}</p:sldIdLst></p:presentation>"#
        )
    }

    fn rels(targets: &[(&str, &str)]) -> String {
        let entries: String = targets
            .iter()
            .map(|(id, target)| format!(r#"<Relationship Id="{id}" Target="{target}"/>"#))
            .collect();
        format!(
            r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{entries}</Relationships>"#
        )
    }

    fn slide_xml(paragraphs: &str) -> String {
        format!(
            r#"<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld><p:spTree>{paragraphs}</p:spTree></p:cSld></p:sld>"#
        )
    }

    fn titled(text: &str) -> String {
        format!(
            r#"<p:sp><p:nvSpPr><p:nvPr><p:ph type="title"/></p:nvPr></p:nvSpPr><p:txBody><a:p><a:r><a:t>{text}</a:t></a:r></a:p></p:txBody></p:sp>"#
        )
    }

    fn body(text: &str) -> String {
        format!(
            r#"<p:sp><p:nvSpPr><p:nvPr><p:ph type="body"/></p:nvPr></p:nvSpPr><p:txBody>{text}</p:txBody></p:sp>"#
        )
    }

    #[test]
    fn slides_come_in_the_order_the_presentation_declares() {
        // The list names slide2 first: the order is the presentation's, not
        // the filenames'.
        let bytes = deck(
            &presentation(&["rId3", "rId2"]),
            &rels(&[
                ("rId1", "slideMasters/slideMaster1.xml"),
                ("rId2", "slides/slide1.xml"),
                ("rId3", "slides/slide2.xml"),
            ]),
            &[
                ("slide1", &slide_xml(&titled("One"))),
                ("slide2", &slide_xml(&titled("Two"))),
            ],
        );
        let deck = read(bytes, 0).expect("the deck opens");
        assert_eq!(deck.slides.len(), 2);
        assert_eq!(deck.slides[0].paragraphs[0].text, "Two");
        assert_eq!(deck.slides[1].paragraphs[0].text, "One");
    }

    #[test]
    fn runs_join_and_a_title_is_marked_as_one() {
        // The runs split one sentence mid-word; the entity is one character
        // once decoded. The field paragraph (a slide number) contributes
        // nothing, and an empty paragraph is not on the slide.
        let one = slide_xml(&format!(
            "{}{}",
            titled("Deck &amp; title"),
            body(
                r#"<a:p><a:r><a:t>First </a:t></a:r><a:r><a:t>sentence</a:t></a:r></a:p><a:p><a:fld type="slidenum"><a:t>‹#›</a:t></a:fld></a:p><a:p><a:r><a:t>Second sentence</a:t></a:r></a:p>"#
            )
        ));
        let two = slide_xml(&body("<a:p><a:r><a:t>Only paragraph</a:t></a:r></a:p>"));
        let bytes = deck(
            &presentation(&["rId2", "rId3"]),
            &rels(&[("rId2", "slides/slide1.xml"), ("rId3", "slides/slide2.xml")]),
            &[("slide1", &one), ("slide2", &two)],
        );
        let deck = read(bytes, 0).expect("the deck opens");
        let first = &deck.slides[0].paragraphs;
        assert_eq!(first.len(), 3);
        assert_eq!(first[0].text, "Deck & title");
        assert!(first[0].title);
        assert_eq!(first[1].text, "First sentence");
        assert!(!first[1].title);
        assert_eq!(first[2].text, "Second sentence");
        let second = &deck.slides[1].paragraphs;
        assert_eq!(second.len(), 1);
        assert!(!second[0].title);
    }

    #[test]
    fn an_index_past_the_end_stays_in_range() {
        let bytes = deck(
            &presentation(&["rId2", "rId3"]),
            &rels(&[("rId2", "slides/slide1.xml"), ("rId3", "slides/slide2.xml")]),
            &[
                ("slide1", &slide_xml(&titled("One"))),
                ("slide2", &slide_xml(&titled("Two"))),
            ],
        );
        let deck = read(bytes, 99).expect("the deck opens");
        assert_eq!(deck.index, 1);
        assert_eq!(deck.slides[deck.index].paragraphs[0].text, "Two");
    }

    #[test]
    fn bytes_that_are_not_a_deck_have_no_slides() {
        assert!(read(b"PK\x03\x04truncated".to_vec(), 0).is_none());
        assert!(read(Vec::new(), 0).is_none());
        // A zip that is not a deck — no presentation parts at all.
        assert!(read(deck("", "", &[]), 0).is_none());
    }
}
