use futures::StreamExt;
use itertools::Itertools;
use semver::Version;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::exit;
use std::process::Stdio;
use std::time::Duration;
use stream_cancel::{StreamExt as ScStreamExt, Trigger, Tripwire};
use structopt::StructOpt;
use tempdir::TempDir;
use tokio::process;
use tokio::select;
use tokio::sync::mpsc;
use tracing::{debug, error, info, Level};
use tracing_subscriber::EnvFilter;

#[derive(StructOpt)]
struct Opts {
    cfg: PathBuf,
    #[structopt(long)]
    install: bool,
}

#[derive(Clone, Deserialize)]
struct Config {
    repo: PathBuf,
    features: Vec<Feature>,
    rust: Vec<RustVersion>,
    par: usize,
    fuzzing: Option<Fuzzing>,
}

#[derive(Clone, Deserialize)]
struct Feature {
    name: String,
    min_rust: Option<String>,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Hash)]
struct RustVersion {
    name: String,
    #[serde(default)]
    requires_pinning: Vec<VersionPin>,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Hash)]
struct VersionPin {
    dependency: String,
    version: Version,
}

#[derive(Clone, Deserialize)]
struct Fuzzing {
    rel_path: PathBuf,
    rust: String,
    duration_s: u64,
}

pub fn load_from_file<T: DeserializeOwned>(path: &Path) -> T {
    let file = std::fs::File::open(path).expect("Can't read cfg file.");
    serde_json::from_reader(file).expect("Could not parse cfg file.")
}

async fn install_toolchain(version: &RustVersion) {
    info!("Installing rust toolchain '{}'", &version.name);
    let mut rustup = process::Command::new("rustup")
        .arg("toolchain")
        .arg("install")
        .arg(&version.name)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("rustup failed");

    let status = rustup.wait().await.unwrap();
    if !status.success() {
        error!(
            "Rustup failed to install toolchain '{}' with exit status {:?}",
            version.name, status
        );
        exit(-1);
    }
}

async fn get_stable_version() -> Version {
    let cargo = process::Command::new("cargo")
        .arg("+stable")
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("cargo failed");

    let output = cargo.wait_with_output().await.unwrap();
    let out_str = String::from_utf8(output.stdout).unwrap();
    let version_str = out_str.split_whitespace().skip(1).next().unwrap();
    version_str.parse().unwrap()
}

/// Returns true if `v1` is greater or equal to `v2`, nightly is seen as highest version possible
fn versions_geq(v1: &str, v2: &str, stable: &Version) -> bool {
    if v1 == "nightly" {
        true
    } else if v2 == "nightly" {
        false
    } else {
        let v1_semver = if v1 == "stable" {
            stable.clone()
        } else {
            v1.parse().unwrap()
        };

        let v2_semver = if v2 == "stable" {
            stable.clone()
        } else {
            v2.parse().unwrap()
        };

        v1_semver >= v2_semver // assumes no rust 2.x
    }
}

async fn gen_test_matrix(cfg: &Config) -> HashMap<RustVersion, Vec<Vec<Feature>>> {
    let stable_version = get_stable_version().await;
    cfg.rust
        .iter()
        .cloned()
        .map(|rust| {
            let feature_sets = cfg
                .features
                .iter()
                .filter(|f| {
                    if let Some(min_rust_version) = f.min_rust.as_ref() {
                        versions_geq(&rust.name, min_rust_version, &stable_version)
                    } else {
                        true
                    }
                })
                .cloned()
                .powerset()
                .collect::<Vec<_>>();
            (rust, feature_sets)
        })
        .collect::<HashMap<_, _>>()
}

async fn test_rust_version(
    cfg: Config,
    rust: RustVersion,
    feature_sets: Vec<Vec<Feature>>,
    delete_path_sender: mpsc::Sender<PathBuf>,
) {
    info!("Preparing environment for rust {} tests", rust.name);
    let project_name = cfg.repo.iter().last().unwrap().to_str().unwrap();
    let tmp_dir = TempDir::new(&format!("{}-{}", project_name, rust.name)).unwrap();

    delete_path_sender
        .send(tmp_dir.path().to_path_buf())
        .await
        .unwrap();

    let tmp_dir_path = tmp_dir.path().to_path_buf();
    let repo_path = cfg.repo.to_path_buf();
    let project_name_inner = project_name.to_string();
    tokio::task::spawn_blocking(move || {
        let mut copy_options = fs_extra::dir::CopyOptions::new();
        copy_options.copy_inside = true;
        fs_extra::dir::copy(repo_path, &tmp_dir_path, &copy_options).unwrap();
        // Otherwise old rust versions may fail if the lockfile was generated with a newer one
        fs_extra::file::remove(tmp_dir_path.join(project_name_inner).join("Cargo.lock")).unwrap();
    })
    .await
    .unwrap();

    let workdir = tmp_dir.path().join(project_name);
    info!(
        "Running rust {} tests in {}",
        rust.name,
        workdir.as_os_str().to_string_lossy()
    );

    if !rust.requires_pinning.is_empty() {
        pin_dependencies(&workdir, &rust).await;
    }

    for feature_set in feature_sets {
        run_test(&workdir, &rust, &feature_set).await;
    }
}

async fn run_test(path: &Path, rust: &RustVersion, feature_set: &[Feature]) {
    let feature_str = feature_set.iter().map(|f| &f.name).join(",");
    let cargo = process::Command::new("cargo")
        .current_dir(path)
        .arg(format!("+{}", rust.name))
        .arg("test")
        .arg("--no-default-features")
        .arg("--features")
        .arg(&feature_str)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("cargo failed to execute");

    let output = cargo.wait_with_output().await.unwrap();
    if output.status.success() {
        info!(
            "Test rust={}, features=[{}] succeeded!",
            rust.name, &feature_str
        )
    } else {
        error!(
            "Test rust={}, features=[{}] failed!",
            rust.name, &feature_str
        );
        info!("std out:\n");
        std::io::stdout().write_all(&output.stdout).unwrap();
        info!("std err:\n");
        std::io::stdout().write_all(&output.stderr).unwrap();
    }
}

async fn pin_dependencies(path: &Path, rust: &RustVersion) {
    debug!("Generating lock file with rust={}", rust.name);
    let mut cargo = process::Command::new("cargo")
        .current_dir(path)
        .arg(format!("+{}", rust.name))
        .arg("generate-lockfile")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("cargo failed to execute");
    assert!(cargo.wait().await.unwrap().success());

    for pin in rust.requires_pinning.iter() {
        debug!("Pinning {} to {}", &pin.dependency, &pin.version);
        let mut cargo = process::Command::new("cargo")
            .current_dir(path)
            .arg(format!("+{}", rust.name))
            .arg("update")
            .arg("-p")
            .arg(&pin.dependency)
            .arg("--precise")
            .arg(pin.version.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("cargo failed to execute");
        assert!(cargo.wait().await.unwrap().success());
    }
}

async fn fuzz_test(base_path: &Path, cfg: &Fuzzing) {
    let fuzz_targets = std::fs::read_dir(base_path.join(&cfg.rel_path).join("fuzz_targets"))
        .unwrap()
        .map(|file| {
            file.unwrap()
                .file_name()
                .to_str()
                .unwrap()
                .split('.')
                .next()
                .unwrap()
                .to_string()
        })
        .collect::<Vec<_>>();

    // TODO: add hfuzz inputs from repo
    for fuzz_target in fuzz_targets {
        info!("Fuzzing {}", fuzz_target);
        let cargo = process::Command::new("cargo")
            .current_dir(base_path.join(&cfg.rel_path))
            .env("HFUZZ_BUILD_ARGS", "--features honggfuzz_fuzz")
            .env(
                "HFUZZ_RUN_ARGS",
                format!("--run_time {} --exit_upon_crash -v", cfg.duration_s),
            )
            .arg(format!("+{}", cfg.rust))
            .arg("hfuzz")
            .arg("run")
            .arg(&fuzz_target)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("cargo failed to execute");

        let output = cargo.wait_with_output().await.unwrap();

        if output.status.success() {
            info!("Successfully fuzzed {}", fuzz_target);
        } else {
            error!("Error while fuzzing {}", fuzz_target);
            info!("std out:\n");
            std::io::stdout().write_all(&output.stdout).unwrap();
            info!("std err:\n");
            std::io::stdout().write_all(&output.stderr).unwrap();
        }
    }
}

async fn delete_paths_on_shutdown(mut path_receiver: mpsc::Receiver<PathBuf>, tw_trigger: Trigger) {
    let mut paths = Vec::<PathBuf>::new();
    loop {
        select! {
            _ = tokio::signal::ctrl_c() => {
                tw_trigger.cancel();
                tokio::time::sleep(Duration::from_millis(500)).await;
                for path in paths {
                    debug!("Trying to delete {} before shutdown", path.to_str().unwrap());
                    process::Command::new("rm")
                        .arg("-rf")
                        .arg(path)
                        .status()
                        .await
                        .unwrap();
                }
                info!("Shutting down ...");
                exit(0);
            },
            Some(path) = path_receiver.recv() => {
                paths.push(path);
            }
        };
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive(Level::DEBUG.into()))
        .init();

    let opts: Opts = StructOpt::from_args();
    let cfg: Config = load_from_file(&opts.cfg);

    if opts.install {
        for rust in cfg.rust.iter() {
            install_toolchain(rust).await;
        }
    }

    let test_matrix = gen_test_matrix(&cfg).await;
    let (delete_path_sender, delete_path_receiver) = mpsc::channel(4);
    let (trigger, tripwire) = Tripwire::new();

    tokio::spawn(delete_paths_on_shutdown(delete_path_receiver, trigger));

    // TODO: allow more parallelism than just amount of rust version to test
    futures::stream::iter(test_matrix)
        .take_until_if(tripwire.clone())
        .for_each_concurrent(cfg.par, |(rust, feature_sets)| {
            test_rust_version(
                cfg.clone(),
                rust.clone(),
                feature_sets,
                delete_path_sender.clone(),
            )
        })
        .await;

    tokio::time::sleep(Duration::from_millis(1500)).await;

    if let Some(ref fuzz) = cfg.fuzzing {
        fuzz_test(&cfg.repo, fuzz).await;
    }
}
