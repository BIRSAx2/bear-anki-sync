pub fn is_auth_error_message(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("auth token not found")
        || lower.contains("authentication_required")
        || lower.contains("request needs authorization")
        || lower.contains("421 misdirected request")
        || lower.contains("401 unauthorized")
}
