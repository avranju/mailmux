use mail_parser::{Address, HeaderName, Message};

const MAX_RESULTS_BYTES: usize = 16 * 1024;

pub fn valid_authserv_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 253
        && value.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label.as_bytes()[0].is_ascii_alphanumeric()
                && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

/// Trust only the receiver's topmost result. The receiver must remove forged
/// headers claiming its authserv-id before adding its own (RFC 8601 section 5).
pub fn authenticated(message: &Message<'_>, expected_sender: &str, trusted: &[String]) -> bool {
    if message.header_values(HeaderName::From).count() != 1 {
        return false;
    }
    let Some(Address::List(addresses)) = message.from() else {
        return false;
    };
    if addresses.len() != 1 {
        return false;
    }
    let Some(sender) = addresses[0].address.as_deref() else {
        return false;
    };
    if !sender.eq_ignore_ascii_case(expected_sender.trim()) {
        return false;
    }
    let Some((_, domain)) = sender.rsplit_once('@') else {
        return false;
    };
    if !valid_authserv_id(domain) {
        return false;
    }
    // Find the first header by name before attempting UTF-8 conversion. A
    // malformed first occurrence cannot fall through to a later forged pass.
    let Some(header) = message.headers().iter().find(|h| {
        h.name
            .as_str()
            .eq_ignore_ascii_case("Authentication-Results")
    }) else {
        return false;
    };
    let Some(raw) = message
        .raw_message
        .get(header.offset_start as usize..header.offset_end as usize)
    else {
        return false;
    };
    let Ok(raw) = std::str::from_utf8(raw) else {
        return false;
    };
    let Some((authserv_id, dmarc_domain)) = parse_results(raw) else {
        return false;
    };
    trusted
        .iter()
        .any(|id| id.eq_ignore_ascii_case(&authserv_id))
        && dmarc_domain.eq_ignore_ascii_case(domain)
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn cfws(&mut self) -> Option<()> {
        loop {
            while self
                .bytes
                .get(self.pos)
                .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
            {
                self.pos += 1;
            }
            if self.bytes.get(self.pos) != Some(&b'(') {
                return Some(());
            }
            self.pos += 1;
            let mut depth = 1usize;
            while depth != 0 {
                match *self.bytes.get(self.pos)? {
                    b'\\' => {
                        self.pos += 1;
                        self.bytes.get(self.pos)?;
                    }
                    b'(' => {
                        depth += 1;
                        if depth > 16 {
                            return None;
                        }
                    }
                    b')' => depth -= 1,
                    _ => {}
                }
                self.pos += 1;
            }
        }
    }

    fn punctuation(&mut self, byte: u8) -> Option<()> {
        self.cfws()?;
        if self.bytes.get(self.pos) != Some(&byte) {
            return None;
        }
        self.pos += 1;
        self.cfws()
    }

    fn keyword(&mut self) -> Option<String> {
        self.cfws()?;
        let start = self.pos;
        while self
            .bytes
            .get(self.pos)
            .is_some_and(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            self.pos += 1;
        }
        if start == self.pos {
            return None;
        }
        Some(
            std::str::from_utf8(&self.bytes[start..self.pos])
                .ok()?
                .to_ascii_lowercase(),
        )
    }

    fn value(&mut self) -> Option<String> {
        self.cfws()?;
        if self.bytes.get(self.pos) == Some(&b'"') {
            self.pos += 1;
            let mut value = Vec::new();
            loop {
                let mut byte = *self.bytes.get(self.pos)?;
                self.pos += 1;
                match byte {
                    b'"' => return String::from_utf8(value).ok(),
                    b'\\' => {
                        byte = *self.bytes.get(self.pos)?;
                        self.pos += 1;
                    }
                    b'\r' | b'\n' | 0 => return None,
                    _ => {}
                }
                if byte.is_ascii_control() {
                    return None;
                }
                value.push(byte);
            }
        }
        let start = self.pos;
        while self.bytes.get(self.pos).is_some_and(|b| {
            !b.is_ascii_whitespace()
                && !matches!(b, b';' | b'=' | b'(' | b')' | b'"' | b'\\')
                && !b.is_ascii_control()
        }) {
            self.pos += 1;
        }
        if start == self.pos {
            return None;
        }
        Some(
            std::str::from_utf8(&self.bytes[start..self.pos])
                .ok()?
                .to_owned(),
        )
    }
}

/// Consume complete method/property tokens, ignoring comments and quoted
/// reasons. No substring search or later duplicate DMARC result can authorize.
fn parse_results(raw: &str) -> Option<(String, String)> {
    if raw.len() > MAX_RESULTS_BYTES {
        return None;
    }
    let mut cursor = Cursor {
        bytes: raw.as_bytes(),
        pos: 0,
    };
    let id = cursor.value()?;
    if !valid_authserv_id(&id) {
        return None;
    }
    cursor.cfws()?;
    if cursor.bytes.get(cursor.pos).is_some_and(u8::is_ascii_digit) && cursor.keyword()? != "1" {
        return None;
    }
    let mut dmarc = None;
    let mut dmarc_seen = false;
    loop {
        cursor.cfws()?;
        if cursor.pos == cursor.bytes.len() {
            break;
        }
        cursor.punctuation(b';')?;
        let method = cursor.keyword()?;
        cursor.cfws()?;
        if cursor.bytes.get(cursor.pos) == Some(&b'/') {
            cursor.punctuation(b'/')?;
            if cursor.keyword()? != "1" {
                return None;
            }
        }
        cursor.punctuation(b'=')?;
        let result = cursor.keyword()?;
        let mut from = None;
        loop {
            cursor.cfws()?;
            if cursor.pos == cursor.bytes.len() || cursor.bytes[cursor.pos] == b';' {
                break;
            }
            let mut key = cursor.keyword()?;
            cursor.cfws()?;
            if cursor.bytes.get(cursor.pos) == Some(&b'.') {
                cursor.punctuation(b'.')?;
                key.push('.');
                key.push_str(&cursor.keyword()?);
            }
            cursor.punctuation(b'=')?;
            let value = cursor.value()?;
            if method == "dmarc" && key == "header.from" {
                if from.is_some() || !valid_authserv_id(&value) {
                    return None;
                }
                from = Some(value);
            }
        }
        if method == "dmarc" {
            if dmarc_seen {
                return None;
            }
            dmarc_seen = true;
            if result == "pass" {
                dmarc = from;
            }
        }
    }
    Some((id, dmarc?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mail_parser::MessageParser;

    fn allowed(headers: &str) -> bool {
        let raw = format!(
            "{headers}\r\nFrom: Bank <alerts@bank.test>\r\nSubject: Alert\r\n\r\nTransaction"
        );
        let message = MessageParser::default().parse(raw.as_bytes()).unwrap();
        authenticated(&message, "alerts@bank.test", &["mx.receiver.test".into()])
    }

    #[test]
    fn authentic_receiver_pass_supports_folding_comments_and_quoted_values() {
        assert!(allowed(
            "Authentication-Results: mx.receiver.test; dkim=pass header.d=bank.test; dmarc=pass (p=reject) header.from=bank.test"
        ));
        assert!(allowed(
            "Authentication-Results: (trusted) \"MX.RECEIVER.TEST\" 1;\r\n dmarc / 1 = pass (nested (comment)) header . from = \"BANK.TEST\""
        ));
    }

    #[test]
    fn missing_failed_untrusted_and_forged_later_results_fail_closed() {
        for value in [
            "",
            "Authentication-Results: evil.test; dmarc=pass header.from=bank.test",
            "Authentication-Results: mx.receiver.test; dmarc=fail header.from=bank.test",
            "Authentication-Results: mx.receiver.test; dmarc=pass header.from=evil.test",
            "Authentication-Results: mx.receiver.test; spf=pass smtp.mailfrom=bank.test",
            "Authentication-Results: mx.receiver.test; dmarc=pass header.from=bank.test; dmarc=fail header.from=bank.test",
            "Authentication-Results: mx.receiver.test; dmarc=fail reason=\"dmarc=pass header.from=bank.test\"",
            "Authentication-Results: mx.receiver.test; spf=pass (dmarc=pass header.from=bank.test)",
            "Authentication-Results: mx.receiver.test; dmarc=pass header.from=bank.test header.from=evil.test",
            "Authentication-Results: mx.receiver.test 2; dmarc=pass header.from=bank.test",
            "Authentication-Results: mx.receiver.test; dmarc=pass header.from=bank.test (unclosed",
            "Authentication-Results: mx.receiver.test; dmarc=fail header.from=bank.test\r\nAuthentication-Results: mx.receiver.test; dmarc=pass header.from=bank.test",
            "Authentication-Results: evil.test; dmarc=pass header.from=bank.test\r\nAuthentication-Results: mx.receiver.test; dmarc=pass header.from=bank.test",
            "From: attacker@evil.test\r\nAuthentication-Results: mx.receiver.test; dmarc=pass header.from=bank.test",
        ] {
            assert!(!allowed(value), "authorized: {value}");
        }
    }

    #[test]
    fn oversized_and_invalid_byte_headers_cannot_fall_through_to_a_later_pass() {
        assert!(!allowed(&format!(
            "Authentication-Results: mx.receiver.test; dmarc=pass header.from=bank.test ({} )",
            "x".repeat(MAX_RESULTS_BYTES)
        )));
        let raw = b"Authentication-Results: mx.receiver.test; dmarc=fail \xff\r\nAuthentication-Results: mx.receiver.test; dmarc=pass header.from=bank.test\r\nFrom: alerts@bank.test\r\n\r\nBody";
        let message = MessageParser::default().parse(raw).unwrap();
        assert!(!authenticated(
            &message,
            "alerts@bank.test",
            &["mx.receiver.test".into()]
        ));
    }
}
