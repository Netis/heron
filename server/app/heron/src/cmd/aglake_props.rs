//! `heron aglake-props` — print the index-time extraction config the aglake
//! backend wants, for the operator to merge into aglaked's `props.toml`.
//!
//! Printing rather than writing is deliberate. That file lives in aglaked's
//! data directory, next to indexes Heron does not own, and it configures a
//! process Heron does not manage — silently editing it would be a surprising
//! reach across a boundary, and would not help the data already on disk anyway,
//! since index-time settings never apply retroactively.
//!
//! The stanzas are generated from the backend's own event structs, so adding a
//! column to an event updates this output with it.

use std::path::Path;

use clap::Args;

#[derive(Debug, Args)]
pub struct AglakePropsArgs {
    /// Print only the stanzas, without the explanatory header — for piping
    /// straight into an existing props.toml.
    #[arg(long)]
    pub bare: bool,
}

pub fn run(_config_arg: Option<&Path>, args: &AglakePropsArgs) -> i32 {
    let text = h_storage_aglake::render_props();
    if args.bare {
        // Everything before the first stanza is commentary.
        match text.find("\n[sourcetype.") {
            Some(i) => print!("{}", text[i + 1..].trim_start_matches('\n')),
            None => print!("{text}"),
        }
    } else {
        print!("{text}");
    }
    0
}
