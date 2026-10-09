#[derive(Debug, Clone, PartialEq)]
pub enum Token { Num(f64), Plus, Minus, Star, Slash, LParen, RParen }

pub fn tokenize(src: &str) -> Result<Vec<Token>, String> {
    let mut out = Vec::new();
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            ' ' => {}
            '+' => out.push(Token::Plus),
            '-' => out.push(Token::Minus),
            '*' => out.push(Token::Star),
            '/' => out.push(Token::Slash),
            '(' => out.push(Token::LParen),
            ')' => out.push(Token::RParen),
            d if d.is_ascii_digit() || d == '.' => {
                let start = i;
                while i + 1 < chars.len() && (chars[i + 1].is_ascii_digit() || chars[i + 1] == '.') {
                    i += 1;
                }
                let s: String = chars[start..=i].iter().collect();
                out.push(Token::Num(s.parse().map_err(|_| format!("bad number {s}"))?));
            }
            other => return Err(format!("unexpected {other}")),
        }
        i += 1;
    }
    Ok(out)
}
