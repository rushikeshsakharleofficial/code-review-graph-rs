//! `crg-parser` — Tree-sitter based multi-language source code parser.
//!
//! Parses source files into [`crg_core::types::NodeInfo`] and
//! [`crg_core::types::EdgeInfo`] records suitable for ingestion into the
//! code-review-graph SQLite store.
//!
//! # Quick start
//!
//! ```no_run
//! use crg_parser::{parse_file, detect_language};
//! use std::path::Path;
//!
//! let result = parse_file(Path::new("src/main.rs")).unwrap();
//! println!("{} nodes, {} edges", result.nodes.len(), result.edges.len());
//! ```

pub mod dispatch;
pub mod languages;
pub mod walker;

pub use languages::detect_language;
pub use walker::{parse_file, parse_file_bytes, ParseResult};
