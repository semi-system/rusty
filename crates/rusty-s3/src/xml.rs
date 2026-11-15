//! Just enough XML for S3: the text of a named element, and the `Contents` entries of a `ListObjectsV2` reply. Not a parser: S3 replies are flat and machine-written, and
//! everything taken out of them is checked afterwards (a listed key must look like one of ours, a size and a time must parse).

/// The text of the first `<name>...</name>` in `xml`, entities decoded.
pub fn text_of(xml: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(decode(&xml[start..end]))
}

/// The slices between `<name>` and `</name>`, for every occurrence.
pub fn blocks<'a>(xml: &'a str, name: &str) -> Vec<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(s) = rest.find(&open) {
        let body = &rest[s + open.len()..];
        let Some(e) = body.find(&close) else { break };
        out.push(&body[..e]);
        rest = &body[e + close.len()..];
    }
    out
}

fn decode(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Name>b</Name><Prefix>p/</Prefix><KeyCount>2</KeyCount><IsTruncated>true</IsTruncated>
<NextContinuationToken>tok&amp;1</NextContinuationToken>
<Contents><Key>p/aa/bb/one</Key><LastModified>2026-11-15T09:50:49.000Z</LastModified><Size>10</Size></Contents>
<Contents><Key>p/aa/bb/two</Key><LastModified>2026-11-15T09:50:50.000Z</LastModified><Size>20</Size></Contents></ListBucketResult>"#;

    #[test]
    fn a_listing_is_read_entry_by_entry() {
        assert_eq!(text_of(LIST, "IsTruncated").as_deref(), Some("true"));
        assert_eq!(text_of(LIST, "NextContinuationToken").as_deref(), Some("tok&1"));
        let entries = blocks(LIST, "Contents");
        assert_eq!(entries.len(), 2);
        assert_eq!(text_of(entries[1], "Key").as_deref(), Some("p/aa/bb/two"));
        assert_eq!(text_of(entries[1], "Size").as_deref(), Some("20"));
        assert_eq!(text_of(LIST, "Missing"), None);
        assert!(blocks("<Contents><Key>x</Key>", "Contents").is_empty(), "an unclosed element is ignored");
        assert_eq!(text_of("<Error><Code>AccessDenied</Code></Error>", "Code").as_deref(), Some("AccessDenied"));
    }
}
