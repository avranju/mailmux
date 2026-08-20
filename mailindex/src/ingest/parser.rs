use crate::{
    config::ContentConfig,
    ingest::{NormalizedAttachment, extractors, normalize},
};
use anyhow::Result;
use mail_parser::{Addr, Address, MessageParser, MimeHeaders, PartType};

pub struct Parsed {
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub sent_at: Option<String>,
    pub subject: Option<String>,
    pub sender: Option<String>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub reply_to: Vec<String>,
    pub body: String,
    pub body_truncated: bool,
    pub attachments: Vec<NormalizedAttachment>,
}

pub fn parse(raw: &[u8], cfg: &ContentConfig) -> Result<Parsed> {
    let m = MessageParser::default()
        .parse(raw)
        .ok_or_else(|| anyhow::anyhow!("invalid RFC 5322 message"))?;
    // Prefer the authoritative plain alternative.  HTML is only a fallback and
    // must go through the same text conversion as HTML attachments.
    // An empty plain-text alternative is not meaningful.  In that case use
    // the primary HTML body rather than returning a blank canonical body.
    let plain = m
        .body_text(0)
        .map(|x| normalize::clean(x.as_ref()))
        .filter(|x| !x.trim().is_empty());
    let body_raw = plain.or_else(|| {
        m.body_html(0)
            .map(|x| normalize::clean(&normalize::html(&x)))
            .filter(|x| !x.trim().is_empty())
    });
    let (body, body_truncated) = body_raw
        .map(|x| normalize::truncate(&x, cfg.max_body_chars))
        .unwrap_or_default();
    let date = m.date().map(|d| d.to_rfc3339());
    let addresses =
        |a: Option<&Address<'_>>| -> Vec<String> { a.map(flat_addresses).unwrap_or_default() };
    let refs = m
        .references()
        .as_text_list()
        .map(|x| x.iter().map(|v| v.to_string()).collect())
        .unwrap_or_default();
    let mut attachments = Vec::new();
    for (i, p) in m.attachments().enumerate() {
        let (bytes, media) = match &p.body {
            PartType::Binary(b) | PartType::InlineBinary(b) => (
                b.as_ref(),
                p.content_type()
                    .map(|x| format!("{}/{}", x.c_type, x.c_subtype.as_deref().unwrap_or(""))),
            ),
            PartType::Text(t) => (t.as_bytes(), Some("text/plain".into())),
            PartType::Html(t) => (t.as_bytes(), Some("text/html".into())),
            _ => (b"".as_slice(), None),
        };
        let media = media
            .or_else(|| {
                p.content_type()
                    .map(|x| format!("{}/{}", x.c_type, x.c_subtype.as_deref().unwrap_or("")))
            })
            .unwrap_or_else(|| "application/octet-stream".into());
        let (status, text, error, truncated) =
            extractors::extract(&media, p.attachment_name(), bytes, cfg);
        attachments.push(NormalizedAttachment {
            part_index: i as i64,
            filename: p.attachment_name().map(str::to_owned),
            media_type: Some(media),
            disposition: p.content_disposition().map(|x| x.c_type.to_string()),
            content_id: p.content_id().map(str::to_owned),
            size_bytes: bytes.len() as i64,
            sha256: crate::ingest::sha256(bytes),
            status,
            error,
            text,
            text_truncated: truncated,
        });
    }
    Ok(Parsed {
        message_id: m.message_id().map(str::to_owned),
        in_reply_to: m.in_reply_to().as_text().map(str::to_owned),
        references: refs,
        sent_at: date,
        subject: m.subject().map(str::to_owned),
        sender: m.from().and_then(|a| flat_addresses(a).into_iter().next()),
        to: addresses(m.to()),
        cc: addresses(m.cc()),
        bcc: addresses(m.bcc()),
        reply_to: addresses(m.reply_to()),
        body,
        body_truncated,
        attachments,
    })
}

fn flat_addresses(a: &Address<'_>) -> Vec<String> {
    match a {
        Address::List(xs) => xs.iter().filter_map(addr).collect(),
        Address::Group(gs) => gs
            .iter()
            .flat_map(|g| g.addresses.iter().filter_map(addr))
            .collect(),
    }
}

fn addr(a: &Addr<'_>) -> Option<String> {
    a.address.as_ref().map(|x| match &a.name {
        Some(n) => format!("{} <{}>", n, x),
        None => x.to_string(),
    })
}
