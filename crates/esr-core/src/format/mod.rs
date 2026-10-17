//! Session_File_Format: Snapshot_Serializer and Snapshot_Parser.
//!
//! Requirements 11.3–11.9.

pub mod escape;
pub mod parse;
pub mod serialize;
pub mod tokens;

pub use parse::{parse, ParseError, ParseErrorKind};
pub use serialize::{serialize, FORMAT_VERSION};
