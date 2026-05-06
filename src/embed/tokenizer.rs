/// Split code identifiers into sub-tokens for better embedding quality.
///
/// "PaymentGatewayService" -> ["payment", "gateway", "service"]
/// "get_user_info"         -> ["get", "user", "info"]
/// "XMLParser"             -> ["xml", "parser"]
#[allow(dead_code)] // TODO: integrate into embedding context building
pub fn tokenize(identifier: &str) -> Vec<String> {
    let mut tokens = Vec::new();

    // First split on underscores
    for part in identifier.split('_') {
        if part.is_empty() {
            continue;
        }
        // Then split CamelCase
        tokens.extend(split_camel(part));
    }

    tokens
}

fn split_camel(s: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = s.chars().collect();
    let mut start = 0;

    for i in 1..chars.len() {
        let prev_lower = chars[i - 1].is_lowercase();
        let curr_upper = chars[i].is_uppercase();

        // Split at lowercase->uppercase boundary
        if prev_lower && curr_upper {
            tokens.push(chars[start..i].iter().collect::<String>().to_lowercase());
            start = i;
        }

        // Split at uppercase->uppercase+lowercase boundary (e.g., "XMLParser" -> "XML" + "Parser")
        if i + 1 < chars.len()
            && chars[i - 1].is_uppercase()
            && chars[i].is_uppercase()
            && chars[i + 1].is_lowercase()
        {
            tokens.push(chars[start..i].iter().collect::<String>().to_lowercase());
            start = i;
        }
    }

    if start < chars.len() {
        tokens.push(chars[start..].iter().collect::<String>().to_lowercase());
    }

    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camel_case() {
        assert_eq!(tokenize("PaymentGateway"), vec!["payment", "gateway"]);
    }

    #[test]
    fn snake_case() {
        assert_eq!(tokenize("get_user_info"), vec!["get", "user", "info"]);
    }

    #[test]
    fn acronym() {
        assert_eq!(tokenize("XMLParser"), vec!["xml", "parser"]);
    }

    #[test]
    fn mixed() {
        assert_eq!(
            tokenize("HTTP_ResponseHandler"),
            vec!["http", "response", "handler"]
        );
    }

    #[test]
    fn single_word() {
        assert_eq!(tokenize("main"), vec!["main"]);
    }
}
