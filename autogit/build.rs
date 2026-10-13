use clap::CommandFactory;
use clap_complete::{
    generate_to,
    shells::{Bash, Fish, Zsh},
};
use std::env;
use std::io::Error;

include!("src/cli.rs");

fn main() -> Result<(), Error> {
    println!("cargo:rerun-if-changed=src/cli.rs");
    let outdir = match env::var_os("OUT_DIR") {
        None => return Ok(()),
        Some(outdir) => outdir,
    };

    let mut cmd = Cli::command();

    // Generate completions
    generate_to(Bash, &mut cmd, "autogit", &outdir)?;
    generate_to(Zsh, &mut cmd, "autogit", &outdir)?;
    generate_to(Fish, &mut cmd, "autogit", &outdir)?;

    Ok(())
}
