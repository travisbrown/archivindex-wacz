//! Shared wire types for the `ZipNum` summary format.

use std::borrow::Cow;
use std::io::{self, Write};

use serde::{Deserialize, Serialize};

use crate::digest::Sha256Digest;

pub const FORMAT: &str = "cdxj-gzip-1.0";

#[derive(Deserialize, Serialize)]
pub struct SummaryHeader<'a> {
    #[serde(borrow)]
    pub format: Cow<'a, str>,
    #[serde(borrow)]
    pub filename: Cow<'a, str>,
}

#[derive(Deserialize, Serialize)]
pub struct SummaryEntry {
    pub offset: u64,
    pub length: u64,
    pub digest: Sha256Digest,
}

pub fn to_json(value: &impl Serialize) -> Result<String, serde_json::Error> {
    let mut bytes = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(&mut bytes, SpacedFormatter);
    value.serialize(&mut serializer)?;
    Ok(String::from_utf8(bytes).expect("JSON serialization produces UTF-8"))
}

struct SpacedFormatter;

impl serde_json::ser::Formatter for SpacedFormatter {
    fn begin_object_key<W: ?Sized + Write>(
        &mut self,
        writer: &mut W,
        first: bool,
    ) -> io::Result<()> {
        if first {
            Ok(())
        } else {
            writer.write_all(b", ")
        }
    }

    fn begin_object_value<W: ?Sized + Write>(&mut self, writer: &mut W) -> io::Result<()> {
        writer.write_all(b": ")
    }
}
