//! `ctxlake config` — print the resolved configuration.
//!
//! There is nothing to elide by *masking* a value: AGENTS.md invariant 10 means the
//! struct itself can never hold a secret, only the name of an env var that resolves
//! to one. So "with secrets elided" is satisfied by construction — this just prints
//! the config, plus a footer saying why that's safe, so a reader doesn't have to
//! take it on faith.

use anyhow::Result;

use crate::config::Config;

pub fn run(cfg: &Config) -> Result<()> {
    let toml = toml::to_string_pretty(cfg)?;
    print!("{toml}");
    println!(
        "\n# No field in this file can hold a secret value — only the NAME of an env\n\
         # var (see api_key_env above, when [summarize.batch] is set). AGENTS.md\n\
         # invariant 10; `ctxlake doctor` reports whether that name resolves, never\n\
         # what it resolves to."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prints_the_config_and_never_panics_on_defaults() {
        let cfg = Config::new("file:///tmp/lake", "myteam", "cc-01");
        run(&cfg).unwrap();
    }
}
