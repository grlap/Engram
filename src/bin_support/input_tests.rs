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
        let bounded: Value = parse_bounded_json_input(&argument, "test", 1024).unwrap();
        assert_eq!(bounded, body);
        assert_eq!(
            engram::CanonicalObject::freeze(&bounded).unwrap().bytes(),
            engram::CanonicalObject::freeze(&body).unwrap().bytes()
        );
    }
    assert_eq!(
        parse_bounded_json_input::<Value>(&plain, "test", 1024).unwrap(),
        body
    );
    let marked = format!("\u{feff}{plain}");
    assert!(parse_bounded_json_input::<Value>(&marked, "test", 1024).is_err());
    for invalid in ["\u{feff}\u{feff}{}", " \u{feff}{}", "\u{feff}{", "\u{feff}"] {
        fs::write(&path, invalid).unwrap();
        assert!(parse_bounded_json_input::<Value>(&argument, "test", 1024).is_err());
    }
    fs::write(&path, [0xef, 0xbb, 0xbf, 0xff]).unwrap();
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

struct CountingReader<R> {
    inner: R,
    consumed: u64,
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.consumed += n as u64;
        Ok(n)
    }
}

#[test]
fn bounded_inline_json_refuses_raw_overflow_including_whitespace_and_utf8() {
    assert_eq!(
        parse_bounded_json_input::<Value>("0", "test", 1).unwrap(),
        json!(0)
    );
    let whitespace = parse_bounded_json_input::<Value>("0 ", "test", 1).unwrap_err();
    let whitespace_text = whitespace.to_string();
    assert!(whitespace_text.contains("exceeds"), "{whitespace_text}");
    assert!(
        !whitespace_text.contains("invalid test JSON"),
        "whitespace overflow must refuse before decode: {whitespace_text}"
    );
    assert_eq!(
        parse_bounded_json_input::<Value>("0 ", "test", 2).unwrap(),
        json!(0)
    );

    let ascii = "\"e\"";
    let utf8 = "\"é\"";
    assert_eq!(ascii.len(), 3);
    assert_eq!(utf8.chars().count(), 3);
    assert_eq!(utf8.len(), 4);
    assert_eq!(
        parse_bounded_json_input::<Value>(ascii, "test", 3).unwrap(),
        json!("e")
    );
    let utf8_overflow = parse_bounded_json_input::<Value>(utf8, "test", 3).unwrap_err();
    let utf8_text = utf8_overflow.to_string();
    assert!(utf8_text.contains("exceeds"), "{utf8_text}");
    assert!(
        !utf8_text.contains("invalid test JSON"),
        "UTF-8 overflow must refuse before decode: {utf8_text}"
    );
    assert_eq!(
        parse_bounded_json_input::<Value>(utf8, "test", 4).unwrap(),
        json!("é")
    );
}

#[test]
fn bounded_read_refuses_before_consuming_more_than_cap_plus_one() {
    let max: u64 = 32;
    let extra = 64;
    let source = vec![b'a'; 32 + extra];
    let mut reader = CountingReader {
        inner: io::Cursor::new(source),
        consumed: 0,
    };
    let result = read_bounded_bytes(&mut reader, max);
    assert_eq!(
        reader.consumed,
        max + 1,
        "independent I/O counter: consumed == cap+1 distinguishes take(max+1) from whole-read-then-len"
    );
    match result {
        Err(BoundedReadError::Overflow { consumed }) => {
            assert_eq!(consumed, max + 1);
        }
        other => panic!("expected overflow, got {other:?}"),
    }
}
