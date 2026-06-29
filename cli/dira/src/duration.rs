//! Parse human duration strings like `1h30m`, `90m`, `45s`, `2h`. A bare integer
//! is interpreted as minutes (the common case for `dira log 45`).

/// Parse a duration into whole seconds.
pub fn parse(input: &str) -> Result<u64, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("empty duration".into());
    }
    // Bare integer → minutes.
    if let Ok(mins) = s.parse::<u64>() {
        return Ok(mins * 60);
    }

    let mut total: u64 = 0;
    let mut num = String::new();
    let mut matched = false;
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            num.push(ch);
        } else {
            let value: u64 = num
                .parse()
                .map_err(|_| format!("expected a number before '{ch}' in '{input}'"))?;
            let mult = match ch {
                'h' | 'H' => 3600,
                'm' | 'M' => 60,
                's' | 'S' => 1,
                ' ' => continue,
                other => return Err(format!("unknown duration unit '{other}' in '{input}'")),
            };
            total += value * mult;
            num.clear();
            matched = true;
        }
    }
    if !num.is_empty() {
        return Err(format!("trailing number without a unit in '{input}'"));
    }
    if !matched {
        return Err(format!("could not parse duration '{input}'"));
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn bare_integer_is_minutes() {
        assert_eq!(parse("45").unwrap(), 45 * 60);
    }

    #[test]
    fn combined_units() {
        assert_eq!(parse("1h30m").unwrap(), 5400);
        assert_eq!(parse("2h").unwrap(), 7200);
        assert_eq!(parse("90m").unwrap(), 5400);
        assert_eq!(parse("45s").unwrap(), 45);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse("abc").is_err());
        assert!(parse("10x").is_err());
        assert!(parse("").is_err());
    }
}
