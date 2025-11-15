use clap::CommandFactory;
use clap_complete::{generate_to, shells::{Bash, Zsh, Fish}};
use std::env;
use std::io::Error;
use std::path::PathBuf;
use std::fs;

include!("src/cli.rs");

fn main() -> Result<(), Error> {
    let outdir = match env::var_os("OUT_DIR") {
        None => return Ok(()),
        Some(outdir) => outdir,
    };

    let mut cmd = Cli::command();

    // Generate completions
    let bash_path = generate_to(Bash, &mut cmd, "autogit", &outdir)?;
    let zsh_path = generate_to(Zsh, &mut cmd, "autogit", &outdir)?;
    let fish_path = generate_to(Fish, &mut cmd, "autogit", &outdir)?;

    // Also copy them to a known location in the build directory
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let completions_dir = PathBuf::from(&manifest_dir).join("completions");
    fs::create_dir_all(&completions_dir)?;

    fs::copy(&bash_path, completions_dir.join("autogit.bash"))?;
    fs::copy(&zsh_path, completions_dir.join("_autogit"))?;
    fs::copy(&fish_path, completions_dir.join("autogit.fish"))?;

    println!("cargo:warning=Completions generated in {:?}", completions_dir);

    Ok(())
}
