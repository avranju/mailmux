use crate::config::PayeeRule;

/// Keep unfamiliar narrations intact. Explicit rules take precedence over the
/// short, whole-token built-in list; numeric references never become payees.
pub fn normalize(narration: &str, rules: &[PayeeRule]) -> String {
    let compact = narration.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = compact.to_ascii_lowercase();
    if let Some(rule) = rules
        .iter()
        .find(|rule| lower.contains(&rule.contains.trim().to_ascii_lowercase()))
    {
        return rule.name.trim().to_string();
    }
    let tokens: Vec<_> = lower.split(|c: char| !c.is_ascii_alphanumeric()).collect();
    for (key, name) in [
        ("swiggy", "Swiggy"),
        ("zomato", "Zomato"),
        ("amazon", "Amazon"),
    ] {
        if tokens.contains(&key) {
            return name.to_string();
        }
    }
    compact
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cleans_known_payees_and_preserves_unknown_names() {
        assert_eq!(normalize("UPI/DR/12345/SWIGGY*ORDER", &[]), "Swiggy");
        assert_eq!(normalize("  Local   Cafe  ", &[]), "Local Cafe");
        assert_eq!(normalize("SWIGGYFAKE/123", &[]), "SWIGGYFAKE/123");
        assert_eq!(
            normalize("UPI/DR/12345/UNKNOWN", &[]),
            "UPI/DR/12345/UNKNOWN"
        );
        let rules = [PayeeRule {
            contains: "swiggy".into(),
            name: "Food delivery".into(),
        }];
        assert_eq!(
            normalize("UPI/DR/12345/SWIGGY*ORDER", &rules),
            "Food delivery"
        );
    }
}
