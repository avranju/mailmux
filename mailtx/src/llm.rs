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
    /// Transaction date/time from the email, preferably RFC3339. Can be date-only (YYYY-MM-DD).
    pub transaction_date: Option<String>,
    /// Spending category for the transaction (e.g. "Groceries", "Dining", "Utilities").
    pub category: Option<String>,
}

const PROMPT_TEMPLATE: &str = "\
Extract bank transaction information from the following email.

Subject: {subject}

Body:
{body}

{categories_section}";

pub async fn extract_transaction(
    client: &Client,
    model: &str,
    subject: &str,
    body: &str,
    existing_categories: &[String],
) -> Result<TransactionData> {
    let categories_section = if existing_categories.is_empty() {
        "Infer an appropriate spending category for this transaction (e.g. \"Groceries\", \"Dining\", \"Utilities\", \"Transport\").".to_string()
    } else {
        format!(
            "Assign a spending category for this transaction. \
             Prefer one of the following existing categories if applicable:\n{}\n\
             If none of the existing categories fit, suggest a new short category name.",
            existing_categories
                .iter()
                .map(|c| format!("- {c}"))
                .collect::<Vec<_>>()
                .join("\n")
        )
    };

    let prompt = PROMPT_TEMPLATE
        .replace("{subject}", subject)
        .replace("{body}", body)
        .replace("{categories_section}", &categories_section);

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
            },
            "transaction_date": {
                "type": "string",
                "description": "Transaction timestamp from the email. Prefer RFC3339 (e.g. 2026-02-26T13:45:00+05:30). If only date is available, return YYYY-MM-DD."
            },
            "category": {
                "type": "string",
                "description": "Spending category for the transaction. Use an existing category if applicable, otherwise suggest a concise new one."
            }
        },
        "required": ["status"]
    });

    let chat_req = ChatRequest::new(vec![
        ChatMessage::system(
            "You extract structured bank transaction data from notification emails. \
             Return only the JSON object, with no additional text. \
             If transaction date/time is present, extract it into transaction_date. \
             Always assign a spending category for the transaction.",
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

    serde_json::from_str(json_str).with_context(|| format!("parsing LLM JSON output: {json_str}"))
}
