//! Shared kernel types for bro-facing crates.
//!
//! This crate is deliberately small: ids, refs, and lightweight error shapes
//! that both protocol DTOs and capability traits can name without depending on
//! either implementation crate.

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id_type!(BroId);
id_type!(SessionId);
id_type!(TaskId);
id_type!(AtomRef);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BroError {
    pub code: String,
    pub message: String,
}

impl BroError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}
