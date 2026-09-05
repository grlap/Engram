use super::*;

#[test]
fn listing_commands_use_the_same_ascii_quote_syntax_on_every_host() {
    let input = LsInput {
        search: Some("'‘’‚‛ $() `; | & > # \"“”".into()),
        limit: Some(1),
        ..LsInput::default()
    };
    assert_eq!(
        input.list_command(),
        "engram work ls --search=''\"'\"''\"‘\"''\"’\"''\"‚\"''\"‛\"' $() `; | & > # \"“”' --limit 1"
    );
}

#[cfg(unix)]
#[test]
fn listing_posix_commands_round_trip_literal_filters() {
    let text = "'‘’‚‛ $name $(printf injected) `printf injected`; | & > # \"“”";
    let input = LsInput {
        search: Some(text.into()),
        limit: Some(1),
        ..LsInput::default()
    };
    // set collects arguments instead of invoking Engram or making a store.
    let output = std::process::Command::new("sh")
        .args([
            "-c",
            &format!("set -- {}; printf '%s\\n' \"$@\"", input.list_command()),
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("engram\nwork\nls\n--search={text}\n--limit\n1\n")
    );
}

#[cfg(windows)]
#[test]
fn listing_powershell_commands_round_trip_literal_filters() {
    // Parse, never execute, the generated command. SafeGetValue accepts only
    // constant AST values, so an interpolated expression also fails the test.
    let script = r"
$ErrorActionPreference = 'Stop'
$tokens = $null
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseInput($env:ENGRAM_TEST_LIST_COMMAND, [ref]$tokens, [ref]$errors)
if ($errors.Count -ne 0 -or $ast.EndBlock.Statements.Count -ne 1) { throw 'not one literal command' }
$pipeline = $ast.EndBlock.Statements[0]
if ($pipeline.PipelineElements.Count -ne 1) { throw 'unexpected pipeline' }
$command = $pipeline.PipelineElements[0]
if ($command.Redirections.Count -ne 0) { throw 'unexpected redirection' }
$values = @($command.CommandElements | ForEach-Object { $_.SafeGetValue() })
ConvertTo-Json -Compress -EscapeHandling EscapeNonAscii -InputObject $values
";
    for text in [
        "user’s issue",
        "x’; Write-Output injected; #",
        "'‘’‚‛“”„‟ $name $(Write-Output injected) ` ; | & > #",
    ] {
        let input = LsInput {
            search: Some(text.into()),
            label: Some(text.into()),
            under: Some(text.into()),
            limit: Some(1),
            ..LsInput::default()
        };
        input.validate_listing().unwrap();
        let output = std::process::Command::new("pwsh")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .env("ENGRAM_TEST_LIST_COMMAND", input.list_command())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let values: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            values,
            json!([
                "engram",
                "work",
                "ls",
                format!("--search={text}"),
                format!("--label={text}"),
                format!("--under={text}"),
                "--limit",
                1
            ])
        );
    }
}

#[test]
fn listing_cursor_rejects_scoped_cross_project_and_unknown_anchor() {
    let (_directory, verbs, path, _) = fixture();
    let parent = add(&verbs, "Parent", None, false, 0);
    add(&verbs, "First", Some(&parent), true, 1);
    add(&verbs, "Second", Some(&parent), true, 2);
    let input = LsInput {
        under: Some(parent.clone()),
        optional: true,
        limit: Some(1),
        ..LsInput::default()
    };
    let receipt = verbs.ls(&input, at(3)).unwrap();
    let token = receipt.value["after"].as_str().unwrap();
    let continued = LsInput {
        after: Some(token.into()),
        ..input.clone()
    };
    let other = AgentVerbs::new(
        path.clone(),
        ProjectId("other".into()),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let error = other.ls(&continued, at(4)).unwrap_err();
    assert!(matches!(
        error.error,
        StoreError::WorkCatalogCursorInvalid { .. }
    ));
    assert_eq!(error.guidance().next, vec![input.list_command()]);
    // The existing parent is excluded by the direct-child filter. Alter only
    // the anchor, not the cut or filter identity, to exercise the SQL guard.
    let bytes = token
        .strip_prefix("c1-")
        .unwrap()
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect::<Vec<_>>();
    let mut cursor: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let store = crate::SqliteStore::open(&path).unwrap();
    cursor["after"] = json!(
        store
            .resolve_work_ref(&ProjectId("customer-workflow".into()), &parent)
            .unwrap()
            .work_id
    );
    let mut changed = String::from("c1-");
    for byte in serde_json::to_vec(&cursor).unwrap() {
        use std::fmt::Write;
        write!(&mut changed, "{byte:02x}").unwrap();
    }
    let connection = rusqlite::Connection::open(&path).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
    let error = verbs
        .ls(
            &LsInput {
                after: Some(changed),
                ..input.clone()
            },
            at(4),
        )
        .unwrap_err();
    assert!(
        matches!(&error.error, StoreError::WorkCatalogCursorInvalid { reason }
        if reason == "continuation item no longer matches this listing")
    );
    assert_eq!(error.guidance().next, vec![input.list_command()]);
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&connection).unwrap(),
        before
    );
}

#[test]
fn listing_cursor_refuses_fractional_expiry_and_clock_reversal() {
    let (_directory, verbs, path, _) = fixture();
    let first = add(&verbs, "First", None, false, 0);
    add(&verbs, "Second", None, false, 1);
    let fraction = chrono::Duration::microseconds(123_900);
    verbs
        .claim(
            ClaimInput {
                work_ref: first,
                ttl_seconds: Some(10),
                recover: None,
            },
            at(2) + fraction,
        )
        .unwrap();
    let input = LsInput {
        limit: Some(1),
        ..LsInput::default()
    };
    // No expiry in this millisecond: even a sub-millisecond reversal refuses.
    let observed = at(3) + fraction;
    let page = verbs.ls(&input, observed).unwrap();
    let continued = LsInput {
        after: Some(page.value["after"].as_str().unwrap().into()),
        ..input.clone()
    };
    assert!(
        verbs
            .ls(&continued, observed + chrono::Duration::microseconds(1))
            .is_ok()
    );
    let reversed = verbs.ls(&continued, observed - chrono::Duration::microseconds(1));
    assert!(matches!(
        reversed.unwrap_err().error,
        StoreError::WorkCatalogCursorInvalid { .. }
    ));
    // SQL expiry columns truncate, but the holder remains live until .123900.
    let observed = at(12) + chrono::Duration::microseconds(123_100);
    let page = verbs.ls(&input, observed).unwrap();
    assert_eq!(page.value["items"][0]["holder"], "agent");
    let continued = LsInput {
        after: Some(page.value["after"].as_str().unwrap().into()),
        ..input.clone()
    };
    let connection = rusqlite::Connection::open(&path).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
    for now in [
        at(12) + fraction,
        at(12) + chrono::Duration::milliseconds(124),
    ] {
        assert!(matches!(
            verbs.ls(&continued, now).unwrap_err().error,
            StoreError::WorkCatalogCursorInvalid { .. }
        ));
    }
    assert!(
        verbs
            .ls(&input, at(12) + chrono::Duration::milliseconds(124))
            .is_ok()
    );
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&connection).unwrap(),
        before
    );
}

#[test]
fn listing_continuation_metadata_overflow_is_an_explicit_refusal() {
    let (_directory, verbs, _, _) = fixture();
    let search = "x".repeat(MAX_AGENT_WORK_RESPONSE_BYTES / 5);
    add(&verbs, &format!("{search} first"), None, false, 0);
    add(&verbs, &format!("{search} second"), None, false, 1);
    let input = LsInput {
        search: Some(search),
        limit: Some(1),
        ..LsInput::default()
    };
    // The complete two-row receipt needs no continuation, and fits easily.
    let all = verbs
        .ls(
            &LsInput {
                limit: Some(2),
                ..input.clone()
            },
            at(2),
        )
        .unwrap();
    assert_eq!(all.value["items"].as_array().unwrap().len(), 2);
    assert!(serde_json::to_vec_pretty(&all.value).unwrap().len() < MAX_AGENT_WORK_RESPONSE_BYTES);
    let error = verbs.ls(&input, at(2)).unwrap_err();
    assert!(
        matches!(&error.error, StoreError::WorkCatalogCursorInvalid { reason }
        if reason.contains("continuation metadata") && reason.contains("shorten"))
    );
    assert_eq!(error.guidance().next, vec![input.list_command()]);
    assert!(!error.to_string().contains("row exceeds"));
}

#[test]
fn listing_zero_row_hint_names_the_first_remaining_oversized_row() {
    let (_directory, verbs, _, _) = fixture();
    add(&verbs, "First", None, false, 0);
    let second = add(&verbs, "Second", None, false, 1);
    let input = LsInput {
        verbose: true,
        limit: Some(1),
        ..LsInput::default()
    };
    let page = verbs.ls(&input, at(2)).unwrap();
    let continued = LsInput {
        after: Some(page.value["after"].as_str().unwrap().into()),
        ..input
    };
    let page = verbs.ls_with_budget(&continued, at(2), 700).unwrap();
    assert_eq!(page.value["items"], json!([]));
    assert_eq!(page.value["shown_before"], 1);
    assert_eq!(page.value["omitted"], 1);
    assert!(page.value.get("after").is_none());
    assert!(page.text().contains(&format!(
        "first remaining match is {second}; its row exceeds the page budget"
    )));
    assert_eq!(
        page.value["next"],
        json!([format!("engram work show {second}")])
    );
}
