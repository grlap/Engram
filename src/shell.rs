//! Quoting of values pasted into a printed command, so that one argument stays
//! one argument in both POSIX shells and PowerShell.

/// `value` in single quotes. A quote character inside is closed out, given
/// as a double-quoted single character and reopened, which both shells read
/// as one literal argument.
#[must_use]
pub fn quote(value: &str) -> String {
    let mut quoted = String::from("'");
    for ch in value.chars() {
        // PowerShell recognizes these typographic delimiters as well. A
        // double-quoted single character between literal segments works in
        // both shells; only ASCII quote characters are used as syntax.
        if matches!(ch, '\'' | '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}') {
            quoted.push_str("'\"");
            quoted.push(ch);
            quoted.push_str("\"'");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

/// `value` as one argument: bare when it holds only ASCII letters, digits,
/// `_` and `-` and does not start with `-`, quoted otherwise, so that no
/// shell splits it, also after `--option=`.
#[must_use]
pub fn argument(value: &str) -> String {
    let plain = !value.is_empty()
        && !value.starts_with('-')
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'));
    if plain {
        value.to_owned()
    } else {
        quote(value)
    }
}

#[cfg(test)]
mod tests {
    use super::{argument, quote};

    #[test]
    fn a_plain_value_stays_bare_and_any_other_is_quoted_as_one_argument() {
        assert_eq!(argument("20261001T000000Z-copy"), "20261001T000000Z-copy");
        assert_eq!(argument("greg"), "greg");
        assert_eq!(argument("greg.lapinski"), "'greg.lapinski'");
        assert_eq!(argument("a:b/c+d"), "'a:b/c+d'");
        assert_eq!(argument("Greg Lapinski"), "'Greg Lapinski'");
        assert_eq!(argument("O'Neil"), "'O'\"'\"'Neil'");
        assert_eq!(argument(""), "''");
        assert_eq!(argument("-x"), "'-x'");
        assert_eq!(argument("a;b"), "'a;b'");
        assert_eq!(argument("$HOME"), "'$HOME'");
        assert_eq!(quote("w-1"), "'w-1'");
    }
}
