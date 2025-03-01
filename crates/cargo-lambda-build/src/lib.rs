use cargo_lambda_metadata::{cargo::binary_targets, fs::rename};
use cargo_zigbuild::Build as ZigBuild;
use clap::{Args, ValueHint};
use miette::{IntoDiagnostic, Result, WrapErr};
use object::{read::File as ObjectFile, Architecture, Object};
use sha2::{Digest, Sha256};
use std::{
    fs::{create_dir_all, read, File},
    io::Write,
    path::{Path, PathBuf},
};
use strum_macros::EnumString;

mod toolchain;
mod zig;

#[derive(Args, Clone, Debug)]
#[clap(name = "build")]
pub struct Build {
    /// The format to produce the compile Lambda into, acceptable values are [Binary, Zip].
    #[clap(long, default_value_t = OutputFormat::Binary)]
    output_format: OutputFormat,

    /// Directory where the final lambda binaries will be located
    #[clap(short, long, value_hint = ValueHint::DirPath)]
    lambda_dir: Option<PathBuf>,

    /// Shortcut for --target aarch64-unknown-linux-gnu
    #[clap(long)]
    arm64: bool,

    #[clap(flatten)]
    build: ZigBuild,
}

pub use cargo_zigbuild::Zig;

const TARGET_ARM: &str = "aarch64-unknown-linux-gnu";
const TARGET_X86_64: &str = "x86_64-unknown-linux-gnu";

#[derive(Clone, Debug, strum_macros::Display, EnumString)]
#[strum(ascii_case_insensitive)]
enum OutputFormat {
    Binary,
    Zip,
}

impl Build {
    pub async fn run(&mut self) -> Result<()> {
        let rustc_meta = rustc_version::version_meta().into_diagnostic()?;
        let host_target = &rustc_meta.host;
        let release_channel = &rustc_meta.channel;

        if self.arm64 && !self.build.target.is_empty() {
            return Err(miette::miette!(
                "invalid options: --arm and --target cannot be specified at the same time"
            ));
        }

        if self.arm64 {
            self.build.target = vec![TARGET_ARM.into()];
        }

        let build_target = self.build.target.get(0);
        match build_target {
            Some(target) => {
                // Validate that the build target is supported in AWS Lambda
                check_build_target(target)?;
            }
            // No explicit target, but build host same as target host
            None if host_target == TARGET_ARM || host_target == TARGET_X86_64 => {
                // Set the target explicitly, so it's easier to find the binaries later
                self.build.target = vec![host_target.into()];
            }
            // No explicit target, and build host not compatible with Lambda hosts
            None => {
                self.build.target = vec![TARGET_X86_64.into()];
            }
        };

        let final_target = self
            .build
            .target
            .get(0)
            .map(|x| x.split_once('.').map(|(t, _)| t).unwrap_or(x.as_str()))
            .unwrap_or(TARGET_X86_64);

        let profile = match self.build.profile.as_deref() {
            Some("dev" | "test") => "debug",
            Some("release" | "bench") => "release",
            Some(profile) => profile,
            None if self.build.release => "release",
            None => "debug",
        };

        // confirm that target component is included in host toolchain, or add
        // it with `rustup` otherwise.
        toolchain::check_target_component_with_rustc_meta(
            final_target,
            host_target,
            release_channel,
        )
        .await?;

        let manifest_path = self
            .build
            .manifest_path
            .as_deref()
            .unwrap_or_else(|| Path::new("Cargo.toml"));
        let binaries = binary_targets(manifest_path)?;

        if !self.build.bin.is_empty() {
            for name in &self.build.bin {
                if !binaries.contains(name) {
                    return Err(miette::miette!(
                        "binary target is missing from this project: {}",
                        name
                    ));
                }
            }
        }

        if !self.build.disable_zig_linker {
            zig::check_installation().await?;
        }

        let mut cmd = self
            .build
            .build_command("build")
            .map_err(|e| miette::miette!("{}", e))?;
        if self.build.release {
            cmd.env("RUSTFLAGS", "-C strip=symbols");
        }

        let mut child = cmd
            .spawn()
            .into_diagnostic()
            .wrap_err("Failed to run cargo build")?;
        let status = child
            .wait()
            .into_diagnostic()
            .wrap_err("Failed to wait on cargo build process")?;
        if !status.success() {
            std::process::exit(status.code().unwrap_or(1));
        }

        let target_dir = Path::new("target");
        let lambda_dir = if let Some(dir) = &self.lambda_dir {
            dir.clone()
        } else {
            target_dir.join("lambda")
        };

        let base = target_dir.join(final_target).join(profile);

        for name in &binaries {
            let binary = base.join(name);
            if binary.exists() {
                let bootstrap_dir = lambda_dir.join(name);
                create_dir_all(&bootstrap_dir).into_diagnostic()?;
                match self.output_format {
                    OutputFormat::Binary => {
                        rename(binary, bootstrap_dir.join("bootstrap")).into_diagnostic()?;
                    }
                    OutputFormat::Zip => {
                        zip_binary(binary, bootstrap_dir)?;
                    }
                }
            }
        }

        Ok(())
    }
}

/// Validate that the build target is supported in AWS Lambda
///
/// Here we use *starts with* instead of an exact match because:
///   - the target could also also be a *musl* variant: `x86_64-unknown-linux-musl`
///   - the target could also [specify a glibc version], which `cargo-zigbuild` supports
///
/// [specify a glibc version]: https://github.com/messense/cargo-zigbuild#specify-glibc-version
fn check_build_target(target: &str) -> Result<()> {
    if !target.starts_with("aarch64-unknown-linux") && !target.starts_with("x86_64-unknown-linux") {
        // Unsupported target for an AWS Lambda environment
        return Err(miette::miette!(
            "Invalid or unsupported target for AWS Lambda: {}",
            target
        ));
    }

    Ok(())
}

pub struct BinaryArchive {
    pub architecture: String,
    pub sha256: String,
    pub path: PathBuf,
}

/// Search for the bootstrap file for a function inside the target directory.
/// If the binary file exists, it creates the zip archive and extracts its architectury by reading the binary.
pub fn find_function_archive<P: AsRef<Path>>(
    name: &str,
    base_dir: &Option<P>,
) -> Result<BinaryArchive> {
    let target_dir = Path::new("target");
    let bootstrap_dir = if let Some(dir) = base_dir {
        dir.as_ref().join(name)
    } else {
        target_dir.join("lambda").join(name)
    };

    let binary_path = bootstrap_dir.join("bootstrap");
    if !binary_path.exists() {
        return Err(miette::miette!(
            "bootstrap file for {name} not found, use `cargo lambda build` to create it"
        ));
    }

    zip_binary(binary_path, bootstrap_dir)
}

/// Create a zip file from a function binary.
/// The binary inside the zip file is always called `bootstrap`.
fn zip_binary<P: AsRef<Path>>(binary_path: P, destination_directory: P) -> Result<BinaryArchive> {
    let path = binary_path.as_ref();
    let dir = destination_directory.as_ref();
    let zipped = dir.join("bootstrap.zip");

    let zipped_binary = File::create(&zipped).into_diagnostic()?;
    let binary_data = read(path).into_diagnostic()?;
    let binary_data = &*binary_data;
    let object = ObjectFile::parse(binary_data).into_diagnostic()?;

    let arch = match object.architecture() {
        Architecture::Aarch64 => "arm64",
        Architecture::X86_64 => "x86_64",
        other => return Err(miette::miette!("invalid binary architecture: {:?}", other)),
    };

    let mut hasher = Sha256::new();
    hasher.update(binary_data);
    let sha256 = format!("{:X}", hasher.finalize());

    let mut zip = zip::ZipWriter::new(zipped_binary);
    zip.start_file("bootstrap", Default::default())
        .into_diagnostic()?;
    zip.write_all(binary_data).into_diagnostic()?;
    zip.finish().into_diagnostic()?;

    Ok(BinaryArchive {
        architecture: arch.into(),
        path: zipped,
        sha256,
    })
}
