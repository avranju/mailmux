use super::normalize;
use crate::config::ContentConfig;

#[derive(Debug)]
pub struct ExtractedText {
    pub text: String,
    pub truncated: bool,
}

fn sanitize_error(error: &str) -> String {
    let mut out: String = error
        .chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect();
    if out.is_empty() {
        out = "attachment extraction failed".into();
    }
    out
}

pub fn extract(
    media: &str,
    filename: Option<&str>,
    bytes: &[u8],
    cfg: &ContentConfig,
) -> (String, Option<String>, Option<String>, bool) {
    if bytes.len() > cfg.max_attachment_bytes {
        return ("too_large".into(), None, None, false);
    }
    if media.eq_ignore_ascii_case("application/pdf") && !cfg.pdf_enabled {
        return ("disabled".into(), None, None, false);
    }
    let supported = media.eq_ignore_ascii_case("text/html")
        || filename.is_some_and(|x| x.to_ascii_lowercase().ends_with(".html"))
        || media.starts_with("text/")
        || media.eq_ignore_ascii_case("text/calendar")
        || media.eq_ignore_ascii_case("application/pdf");
    if !supported {
        return ("unsupported".into(), None, None, false);
    }
    let result = std::panic::catch_unwind(|| -> Result<String, String> {
        if media.eq_ignore_ascii_case("text/html")
            || filename.is_some_and(|x| x.to_ascii_lowercase().ends_with(".html"))
        {
            html2text::from_read(bytes, 100).map_err(|e| e.to_string())
        } else if media.starts_with("text/") || media.eq_ignore_ascii_case("text/calendar") {
            Ok(String::from_utf8_lossy(bytes).into_owned())
        } else {
            pdf_extract::extract_text_from_mem(bytes).map_err(|e| e.to_string())
        }
    });
    match result {
        Ok(Ok(text)) => {
            let t = normalize::truncate(&normalize::clean(&text), cfg.max_attachment_text_chars);
            if t.0.is_empty() {
                ("empty".into(), None, None, t.1)
            } else {
                ("extracted".into(), Some(t.0), None, t.1)
            }
        }
        Ok(Err(error)) => ("error".into(), None, Some(sanitize_error(&error)), false),
        Err(_) => (
            "error".into(),
            None,
            Some("attachment extraction failed".into()),
            false,
        ),
    }
}
