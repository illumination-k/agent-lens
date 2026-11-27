//! Captured `quint parse` output for the files under `testdata/`, so unit
//! tests exercise the IR handling without the quint CLI. Regenerate with
//! `testdata/regen.sh` after changing a fixture.

use std::path::Path;

use crate::cli::{ParsedFile, from_captured};

pub(crate) struct Fixture {
    pub source: &'static str,
    pub file: ParsedFile,
}

macro_rules! fixture {
    ($name:literal) => {
        Fixture {
            source: include_str!(concat!("../testdata/", $name, ".qnt")),
            file: from_captured(
                include_str!(concat!("../testdata/", $name, ".ir.json")),
                include_str!(concat!("../testdata/", $name, ".map.json")),
                Path::new(concat!("/fixture/", $name, ".qnt")),
            ),
        }
    };
}

pub(crate) fn counter() -> Fixture {
    fixture!("counter")
}

pub(crate) fn helpers() -> Fixture {
    fixture!("helpers")
}
