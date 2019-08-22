#![feature(proc_macro_diagnostic, type_ascription)]
#![recursion_limit = "128"]

extern crate proc_macro;

use quote::{format_ident, quote};
use syn::{parse_macro_input, spanned::Spanned as _};

// per rustc: "functions tagged with `#[proc_macro]` must currently reside in the root of the crate"
include!("utils.rs");
include!("default_attr.rs");
include!("event_derive.rs");
include!("service_derive.rs");
