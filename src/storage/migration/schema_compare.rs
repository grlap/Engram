use super::SchemaEntry;

pub(super) fn rebuildable(entry: &SchemaEntry) -> bool {
    super::super::schema_object_matches_durability(
        &super::super::SchemaDefinition {
            object_type: entry.kind.clone(),
            name: entry.name.clone(),
            sql: entry.sql.clone().unwrap_or_default(),
        },
        super::super::SchemaDurability::Rebuildable,
    )
}

/// Ignore whitespace between tokens, never inside quoted names, string values
/// or comments. This is conservative comparison, not SQL execution or parsing
/// arbitrary future schema semantics.
pub(super) fn tokens(sql: &str) -> Vec<String> {
    let mut chars = sql.chars().peekable();
    let mut result = Vec::new();
    while let Some(character) = chars.next() {
        if character.is_whitespace() {
            continue;
        }
        let mut token = String::from(character);
        if matches!(character, '\'' | '"' | '`' | '[') {
            let closing = if character == '[' { ']' } else { character };
            while let Some(next) = chars.next() {
                token.push(next);
                if next == closing {
                    if chars.peek() == Some(&closing) && closing != ']' {
                        if let Some(escaped) = chars.next() {
                            token.push(escaped);
                        }
                    } else {
                        break;
                    }
                }
            }
        } else if character == '-' && chars.peek() == Some(&'-') {
            for next in chars.by_ref() {
                token.push(next);
                if next == '\n' {
                    break;
                }
            }
        } else if character == '/' && chars.peek() == Some(&'*') {
            let mut previous = character;
            for next in chars.by_ref() {
                token.push(next);
                if previous == '*' && next == '/' {
                    break;
                }
                previous = next;
            }
        } else if character.is_alphanumeric() || character == '_' {
            while chars
                .peek()
                .is_some_and(|next| next.is_alphanumeric() || *next == '_')
            {
                if let Some(next) = chars.next() {
                    token.push(next);
                }
            }
        }
        result.push(token);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::tokens;

    #[test]
    fn migration_schema_spacing_is_not_string_or_comment_equivalence() {
        assert_eq!(
            tokens("CREATE INDEX x ON t(a,b)"),
            tokens("CREATE INDEX x\n ON t ( a, b )")
        );
        assert_ne!(tokens("DEFAULT 'a  b'"), tokens("DEFAULT 'a b'"));
        assert_ne!(tokens("CHECK(x- -1)"), tokens("CHECK(x--1)"));
        assert_ne!(tokens("CHECK(x/*a b*/=1)"), tokens("CHECK(x/*a  b*/=1)"));
        assert_eq!(tokens("DEFAULT 'it''s ok'"), tokens("DEFAULT\n'it''s ok'"));
    }
}
