/// Parse "1h30m", "45m", "2h", "90s" into seconds.
pub fn parse_duration(text: &str) -> Option<u64> {
    let mut total = 0;
    let mut number = String::new();
    for c in text.chars() {
        if c.is_ascii_digit() {
            number.push(c);
        } else if c == 'h' {
            total += number.parse::<u64>().ok()? * 3600;
            number.clear();
        } else {
            return None;
        }
    }
    if !number.is_empty() || text.is_empty() {
        return None;
    }
    Some(total)
}
