use crate::lexer::{tokenize, Token};

/// Evaluate an arithmetic expression with + - * / and parentheses.
pub fn eval(src: &str) -> Result<f64, String> {
    let tokens = tokenize(src)?;
    let mut pos = 0;
    let value = expr(&tokens, &mut pos)?;
    if pos != tokens.len() {
        return Err("trailing input".into());
    }
    Ok(value)
}

// BUG: all binary operators share one precedence level, evaluated left to right.
fn expr(t: &[Token], pos: &mut usize) -> Result<f64, String> {
    let mut acc = atom(t, pos)?;
    while *pos < t.len() {
        let op = t[*pos].clone();
        if !matches!(op, Token::Plus | Token::Minus | Token::Star | Token::Slash) {
            break;
        }
        *pos += 1;
        let rhs = atom(t, pos)?;
        acc = match op {
            Token::Plus => acc + rhs,
            Token::Minus => acc - rhs,
            Token::Star => acc * rhs,
            _ => acc / rhs,
        };
    }
    Ok(acc)
}

fn atom(t: &[Token], pos: &mut usize) -> Result<f64, String> {
    match t.get(*pos) {
        Some(Token::Num(n)) => {
            *pos += 1;
            Ok(*n)
        }
        Some(Token::LParen) => {
            *pos += 1;
            let v = expr(t, pos)?;
            if t.get(*pos) != Some(&Token::RParen) {
                return Err("expected )".into());
            }
            *pos += 1;
            Ok(v)
        }
        other => Err(format!("unexpected {other:?}")),
    }
}
