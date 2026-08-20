pub fn clean(input: &str) -> String {
    let s = input.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::new();
    let mut blanks = 0;
    for line in s.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            blanks += 1;
            if blanks > 2 {
                continue;
            }
        } else {
            blanks = 0
        }
        out.push_str(line);
        out.push('\n')
    }
    out.trim_end().to_owned()
}

pub fn truncate(s: &str, max: usize) -> (String, bool) {
    if s.chars().count() <= max {
        return (s.to_owned(), false);
    }
    (s.chars().take(max).collect(), true)
}

pub fn html(s: &str) -> String {
    html2text::from_read(s.as_bytes(), 100).unwrap_or_else(|_| s.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_lines_without_stripping_quotes() {
        assert_eq!(
            clean("a\r\n\r\n\r\n\r\n> quote  \r\nb"),
            "a\n\n\n> quote\nb"
        );
    }

    #[test]
    fn truncation_never_splits_unicode() {
        let (value, truncated) = truncate("é界x", 2);
        assert_eq!(value, "é界");
        assert!(truncated);
    }

    #[test]
    fn html_becomes_text() {
        let value = html("<b>Hello</b> &amp; world");
        assert!(value.contains("Hello") && value.contains("world"));
        assert!(!value.contains("<b>"));
    }
}
