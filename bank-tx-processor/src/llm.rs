use anyhow::{Context, Result};
use genai::Client;
use genai::chat::{ChatMessage, ChatOptions, ChatRequest, ChatResponseFormat, JsonSpec};
use serde::Deserialize;

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

const PROMPT_TEMPLATE: &str = "\
Extract bank transaction information from the following email.

Subject: {subject}

Body:
{body}";

pub async fn extract_transaction(
    client: &Client,
    model: &str,
    subject: &str,
    body: &str,
) -> Result<TransactionData> {
    let prompt = PROMPT_TEMPLATE
        .replace("{subject}", subject)
        .replace("{body}", body);

    let schema = serde_json::json!({
        "type": "object",
        "properties": {
            "status": {
                "type": "string",
                "enum": ["found", "not_found"],
                "description": "\"found\" if this is a bank transaction notification with a monetary amount, \"not_found\" otherwise"
            },
            "amount": {
                "type": "number",
                "description": "Transaction amount as a number, e.g. 1234.56"
            },
            "transaction_type": {
                "type": "string",
                "enum": ["deposit", "withdrawal"]
            },
            "narration": {
                "type": "string",
                "description": "Brief description of the merchant or transaction purpose"
            }
        },
        "required": ["status"]
    });

    let chat_req = ChatRequest::new(vec![
        ChatMessage::system(
            "You extract structured bank transaction data from notification emails. \
             Return only the JSON object, with no additional text.",
        ),
        ChatMessage::user(prompt),
    ]);

    let options = ChatOptions::default().with_response_format(ChatResponseFormat::JsonSpec(
        JsonSpec::new("transaction", schema),
    ));

    let res = client
        .exec_chat(model, chat_req, Some(&options))
        .await
        .context("calling LLM API")?;

    let text = res
        .first_text()
        .ok_or_else(|| anyhow::anyhow!("no text content in LLM response"))?;

    // Strip markdown code fences the model may still wrap the JSON in.
    let json_str = text
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();

    serde_json::from_str(json_str)
        .with_context(|| format!("parsing LLM JSON output: {json_str}"))
}
