//! The bytes `graph save` hands an operator: pretty JSON in which every
//! character that is unsafe to render reaches the file or stdout as a `\u`
//! escape, so the output is inert in a terminal. Escaping changes no value:
//! load parses the container and re-derives the body's canonical bytes.

use std::io::{self, Write};

use serde::Serialize;
use serde_json::ser::{Formatter, PrettyFormatter, Serializer};

use super::WorkGraphSnapshotDocument;
use crate::domain::is_unsafe_rendered_text_char;

impl WorkGraphSnapshotDocument {
    /// The saved container: pretty JSON ending in one newline, with every
    /// terminal-unsafe character in its strings escaped.
    ///
    /// # Errors
    ///
    /// Returns a serialization error if the document cannot be encoded.
    pub fn container_bytes(&self) -> serde_json::Result<Vec<u8>> {
        let mut bytes = Vec::new();
        let mut serializer =
            Serializer::with_formatter(&mut bytes, TerminalInert(PrettyFormatter::new()));
        self.serialize(&mut serializer)?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}

/// Pretty layout whose string fragments escape what a terminal would act on.
/// `serde_json` already escapes quotes, backslashes and C0 controls before a
/// fragment arrives here.
struct TerminalInert<'a>(PrettyFormatter<'a>);

impl Formatter for TerminalInert<'_> {
    fn write_string_fragment<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        fragment: &str,
    ) -> io::Result<()> {
        let mut start = 0;
        for (index, ch) in fragment.char_indices() {
            if is_unsafe_rendered_text_char(ch) {
                writer.write_all(&fragment.as_bytes()[start..index])?;
                let mut units = [0_u16; 2];
                for unit in ch.encode_utf16(&mut units) {
                    write!(writer, "\\u{unit:04x}")?;
                }
                start = index + ch.len_utf8();
            }
        }
        writer.write_all(&fragment.as_bytes()[start..])
    }

    fn begin_array<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.begin_array(writer)
    }

    fn end_array<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.end_array(writer)
    }

    fn begin_array_value<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> io::Result<()> {
        self.0.begin_array_value(writer, first)
    }

    fn end_array_value<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.end_array_value(writer)
    }

    fn begin_object<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.begin_object(writer)
    }

    fn end_object<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.end_object(writer)
    }

    fn begin_object_key<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> io::Result<()> {
        self.0.begin_object_key(writer, first)
    }

    fn begin_object_value<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.begin_object_value(writer)
    }

    fn end_object_value<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        self.0.end_object_value(writer)
    }
}
