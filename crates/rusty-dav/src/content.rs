//! The text of an iCalendar (RFC 5545) or a vCard (RFC 6350) object as a tree of components and properties: unfolded, checked for structure and bounded. It reads; it never writes: an object is stored and
//! returned byte for byte. Values are kept as written (the escapes of text are not undone: a reader that needs a text unescapes it).

/// The most bytes of an object (the same as the table's check).
pub const MAX_BYTES: usize = 1024 * 1024;
/// The deepest nesting of components (a calendar, an event, an alarm: three).
pub const MAX_DEPTH: usize = 8;
/// Content lines and components of one object.
pub const MAX_LINES: usize = 20_000;
pub const MAX_COMPONENTS: usize = 5_000;
/// The longest content line once unfolded.
pub const MAX_LINE_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prop {
    /// Upper case, without a group (`item1.EMAIL` is `EMAIL`).
    pub name: String,
    /// Parameters as written, names in upper case; a value without its quotes.
    pub params: Vec<(String, String)>,
    pub value: String,
}

impl Prop {
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Component {
    /// Upper case: `VCALENDAR`, `VEVENT`, `VCARD`...
    pub name: String,
    pub props: Vec<Prop>,
    pub children: Vec<Component>,
}

impl Component {
    pub fn prop(&self, name: &str) -> Option<&Prop> {
        self.props.iter().find(|p| p.name == name)
    }

    pub fn props_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Prop> {
        self.props.iter().filter(move |p| p.name == name)
    }
}

/// The lines of the text with their folds undone (a line that starts with a space or a tab continues the one before it); `Err` for a text over a limit.
fn unfold(text: &str) -> Result<Vec<String>, &'static str> {
    if text.len() > MAX_BYTES {
        return Err("the object is too large");
    }
    let mut lines: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if line.contains('\r') {
            return Err("a bare CR");
        }
        if let Some(rest) = line.strip_prefix([' ', '\t']) {
            match lines.last_mut() {
                Some(last) => last.push_str(rest),
                None => return Err("a continuation with no line before it"),
            }
        } else {
            lines.push(line.to_owned());
        }
        if lines.len() > MAX_LINES {
            return Err("too many lines");
        }
    }
    // The text ends with a line break: the last element is empty.
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    if lines.iter().any(|l| l.len() > MAX_LINE_BYTES) {
        return Err("a line is too long");
    }
    if lines.iter().any(|l| l.is_empty() || l.chars().any(|c| c == '\0' || (c.is_control() && c != '\t'))) {
        return Err("an empty line or a control character");
    }
    Ok(lines)
}

/// Split `NAME;P=V;P="V:2":value` into the name (and group), the parameters and the value; the first `:` outside quotes ends the head.
fn content_line(line: &str) -> Result<Prop, &'static str> {
    let mut in_quotes = false;
    let mut colon = None;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ':' if !in_quotes => {
                colon = Some(i);
                break;
            }
            _ => {}
        }
    }
    let Some(colon) = colon else { return Err("a line with no colon") };
    let (head, value) = (line.get(..colon).unwrap_or(""), line.get(colon + 1..).unwrap_or(""));
    let mut parts = Vec::new();
    let (mut start, mut quoted) = (0, false);
    for (i, c) in head.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ';' if !quoted => {
                parts.push(head.get(start..i).unwrap_or(""));
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(head.get(start..).unwrap_or(""));
    let mut it = parts.into_iter();
    let full = it.next().unwrap_or("");
    let name = full.rsplit('.').next().unwrap_or("");
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err("a property name is not valid");
    }
    let mut params = Vec::new();
    for p in it {
        let Some((n, v)) = p.split_once('=') else { return Err("a parameter has no value") };
        if n.is_empty() || !n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err("a parameter name is not valid");
        }
        params.push((n.to_ascii_uppercase(), v.trim_matches('"').to_owned()));
    }
    Ok(Prop { name: name.to_ascii_uppercase(), params, value: value.to_owned() })
}

/// Read one object: `BEGIN:X` ... `END:X` with nothing before or after it.
pub fn parse(text: &str) -> Result<Component, &'static str> {
    let lines = unfold(text)?;
    let mut stack: Vec<Component> = Vec::new();
    let mut done: Option<Component> = None;
    let mut components = 0usize;
    for line in &lines {
        if done.is_some() {
            return Err("text after the end of the object");
        }
        let prop = content_line(line)?;
        match prop.name.as_str() {
            "BEGIN" => {
                if stack.len() >= MAX_DEPTH {
                    return Err("components are nested too deep");
                }
                components += 1;
                if components > MAX_COMPONENTS {
                    return Err("too many components");
                }
                let name = prop.value.to_ascii_uppercase();
                if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                    return Err("a component name is not valid");
                }
                stack.push(Component { name, props: Vec::new(), children: Vec::new() });
            }
            "END" => {
                let Some(top) = stack.pop() else { return Err("END with no BEGIN") };
                if top.name != prop.value.to_ascii_uppercase() {
                    return Err("END does not match BEGIN");
                }
                match stack.last_mut() {
                    Some(parent) => parent.children.push(top),
                    None => done = Some(top),
                }
            }
            _ => match stack.last_mut() {
                Some(top) => top.props.push(prop),
                None => return Err("a property outside a component"),
            },
        }
    }
    if !stack.is_empty() {
        return Err("a component is not closed");
    }
    done.ok_or("no component")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tree_of_components_with_folds_groups_and_quoted_parameters() {
        let c = parse("BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Ada\r\n  Lovelace\r\nitem1.EMAIL;TYPE=work;X-A=\"a:b;c\":ada@example.org\r\nEND:VCARD\r\n").unwrap();
        assert_eq!(c.name, "VCARD");
        assert_eq!(c.prop("FN").unwrap().value, "Ada Lovelace");
        let e = c.prop("EMAIL").unwrap();
        assert_eq!((e.value.as_str(), e.param("TYPE"), e.param("X-A")), ("ada@example.org", Some("work"), Some("a:b;c")));
        let n = parse("BEGIN:VCALENDAR\nBEGIN:VEVENT\nBEGIN:VALARM\nEND:VALARM\nEND:VEVENT\nEND:VCALENDAR\n").unwrap();
        assert_eq!(n.children[0].children[0].name, "VALARM");
    }

    #[test]
    fn what_is_not_one_object_is_refused() {
        for bad in [
            "",
            "BEGIN:VCARD\r\n",
            "BEGIN:VCARD\r\nEND:VEVENT\r\n",
            "END:VCARD\r\n",
            "FN:x\r\nBEGIN:VCARD\r\nEND:VCARD\r\n",
            "BEGIN:VCARD\r\nEND:VCARD\r\nBEGIN:VCARD\r\nEND:VCARD\r\n",
            "BEGIN:VCARD\r\nno colon here\r\nEND:VCARD\r\n",
            "BEGIN:VCARD\r\nA\rB:x\r\nEND:VCARD\r\n",
            " continued\r\nBEGIN:VCARD\r\nEND:VCARD\r\n",
            "BEGIN:VCARD\r\n\r\nEND:VCARD\r\n",
            "BEGIN:VCARD\r\nX;P:y\r\nEND:VCARD\r\n",
            "BEGIN:VCARD\r\nBAD NAME:y\r\nEND:VCARD\r\n",
            "BEGIN:VCARD\r\nX:a\u{1}b\r\nEND:VCARD\r\n",
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_limits_hold() {
        let deep = format!("{}{}", "BEGIN:A\n".repeat(MAX_DEPTH + 1), "END:A\n".repeat(MAX_DEPTH + 1));
        assert!(parse(&deep).is_err());
        let ok = format!("{}{}", "BEGIN:A\n".repeat(MAX_DEPTH), "END:A\n".repeat(MAX_DEPTH));
        assert!(parse(&ok).is_ok());
        let long = format!("BEGIN:A\nX:{}\nEND:A\n", "y".repeat(MAX_LINE_BYTES + 1));
        assert!(parse(&long).is_err());
        let big = format!("BEGIN:A\nX:{}\nEND:A\n", "y ".repeat(MAX_BYTES));
        assert!(parse(&big).is_err());
        let many = format!("BEGIN:A\n{}END:A\n", "X:1\n".repeat(MAX_LINES));
        assert!(parse(&many).is_err());
    }
}
