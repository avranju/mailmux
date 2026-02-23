use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Structured transaction data extracted by the LLM.
#[derive(Debug, Deserialize)]
pub struct TransactionData {
    /// "found" when the email is a bank transaction notification, "not_found" otherwise.
    pub status: String,
    pub amount: Option<f64>,
    /// "deposit" or "withdrawal"
    pub transaction_type: Option<String>,
    pub narration: Option<String>,
}

#[derive(Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    messages: Vec<Message<'a>>,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
}

#[derive(Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
}

const PROMPT_TEMPLATE: &str = "\
Extract bank transaction information from the following email.

Subject: {subject}

Body:
{body}

Return a JSON object with exactly these fields:
- \"status\": \"found\" if this is a bank transaction notification with a monetary amount, \
\"not_found\" otherwise
- \"amount\": the transaction amount as a number (e.g. 1234.56), or null
- \"transaction_type\": \"deposit\" or \"withdrawal\", or null
- \"narration\": a brief description of the merchant or transaction purpose, or null

Return only valid JSON with no other text or markdown formatting.";

pub async fn extract_transaction(
    client: &reqwest::Client,
    api_key: &str,
    model: &str,
    subject: &str,
    body: &str,
) -> Result<TransactionData> {
    let prompt = PROMPT_TEMPLATE
        .replace("{subject}", subject)
        .replace("{body}", body);

    let request_body = MessagesRequest {
        model,
        max_tokens: 256,
        messages: vec![Message {
            role: "user",
            content: &prompt,
        }],
    };

    let response = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&request_body)
        .send()
        .await
        .context("sending request to Anthropic API")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!("Anthropic API error {status}: {body}");
    }

    let resp: MessagesResponse = response
        .json()
        .await
        .context("parsing Anthropic API response")?;

    let text = resp
        .content
        .into_iter()
        .find(|b| b.kind == "text")
        .and_then(|b| b.text)
        .ok_or_else(|| anyhow::anyhow!("no text content in Anthropic API response"))?;

    // Strip markdown code fences the model may wrap the JSON in.
    let json_str = text
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    serde_json::from_str(json_str)
        .with_context(|| format!("parsing LLM JSON output: {json_str}"))
}
