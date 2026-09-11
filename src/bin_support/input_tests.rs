use super::*;
use serde_json::{Value, json};

#[test]
fn remember_text_accepts_exactly_one_source_and_preserves_revision_flags() {
    for text in [vec!["body"], vec!["--text", "body"], vec!["--text=body"]] {
        let mut args = vec!["engram", "work", "remember"];
        args.extend(text);
        args.extend(["--key", "note", "--revise", "--expected-revision", "2"]);
        assert!(Cli::try_parse_from(args).is_ok());
    }
    for args in [
        vec!["engram", "work", "remember"],
        vec!["engram", "work", "remember", "--key", "note"],
        vec!["engram", "work", "remember", "--text"],
    ] {
        let error = Cli::try_parse_from(args).unwrap_err();
        assert!(error.to_string().contains("TEXT"));
        assert!(!error.to_string().contains("-- --text"));
    }
    for args in [
        ["engram", "work", "remember", "body", "--text", "other"],
        ["engram", "work", "remember", "--text", "other", "body"],
    ] {
        let error = Cli::try_parse_from(args).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
        assert!(error.to_string().contains("--text"));
        assert!(error.to_string().contains("TEXT"));
    }
    for text in [vec!["body"], vec!["--text", "body"]] {
        for options in [vec!["--revise"], vec!["--expected-revision", "1"]] {
            let mut args = vec!["engram", "work", "remember"];
            args.extend(text.clone());
            args.extend(options);
            assert_eq!(
                Cli::try_parse_from(args).unwrap_err().kind(),
                clap::error::ErrorKind::MissingRequiredArgument
            );
        }
    }
}

#[test]
fn json_file_bom_is_stripped_once_without_changing_inline_or_body_bytes() {
    let temp = test_support::temp_home().unwrap();
    let path = temp.path().join("input.json");
    let argument = format!("@{}", path.display());
    let body = json!({"text":"\u{feff}inside\u{feff}", "value":3});
    let plain = body.to_string();
    for source in [&plain, &format!("\u{feff}{plain}")] {
        fs::write(&path, source).unwrap();
        let unbounded: Value = parse_json_input(&argument).unwrap();
        let bounded: Value = parse_bounded_json_input(&argument, "test", 1024).unwrap();
        assert_eq!(unbounded, body);
        assert_eq!(bounded, body);
        assert_eq!(
            engram::CanonicalObject::freeze(&bounded).unwrap().bytes(),
            engram::CanonicalObject::freeze(&body).unwrap().bytes()
        );
    }
    assert_eq!(parse_json_input::<Value>(&plain).unwrap(), body);
    assert_eq!(
        parse_bounded_json_input::<Value>(&plain, "test", 1024).unwrap(),
        body
    );
    let marked = format!("\u{feff}{plain}");
    assert!(parse_json_input::<Value>(&marked).is_err());
    assert!(parse_bounded_json_input::<Value>(&marked, "test", 1024).is_err());
    for invalid in ["\u{feff}\u{feff}{}", " \u{feff}{}", "\u{feff}{", "\u{feff}"] {
        fs::write(&path, invalid).unwrap();
        assert!(parse_json_input::<Value>(&argument).is_err());
        assert!(parse_bounded_json_input::<Value>(&argument, "test", 1024).is_err());
    }
    fs::write(&path, [0xef, 0xbb, 0xbf, 0xff]).unwrap();
    assert!(parse_json_input::<Value>(&argument).is_err());
    assert!(parse_bounded_json_input::<Value>(&argument, "test", 1024).is_err());
}

#[test]
fn json_file_bom_counts_toward_the_raw_limit() {
    let temp = test_support::temp_home().unwrap();
    let path = temp.path().join("bounded.json");
    let argument = format!("@{}", path.display());
    for (source, limit) in [("0", 1), ("\u{feff}0", 4), ("\u{feff}0 ", 5)] {
        fs::write(&path, source).unwrap();
        assert_eq!(
            parse_bounded_json_input::<Value>(&argument, "test", limit).unwrap(),
            json!(0)
        );
        let error = parse_bounded_json_input::<Value>(&argument, "test", limit - 1).unwrap_err();
        assert!(error.to_string().contains("exceeds"), "{error}");
    }
}
