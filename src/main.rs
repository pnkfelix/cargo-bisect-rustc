// Copyright 2018 The Rust Project Developers
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

use std::env;
use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::Read;
use std::path::PathBuf;
use std::process::{self, Command};
use std::str::FromStr;

use chrono::{Date, DateTime, Duration, Utc};
use colored::*;
use failure::{bail, format_err, Fail, Error};
use log::debug;
use reqwest::blocking::Client;
use structopt::StructOpt;
use tee::TeeReader;

mod least_satisfying;
mod repo_access;
mod toolchains;

use crate::least_satisfying::{least_satisfying, Satisfies};
use crate::repo_access::{AccessViaGithub, AccessViaLocalGit, RustRepositoryAccessor};
use crate::toolchains::*;

#[derive(Debug, Clone, PartialEq)]
pub struct Commit {
    pub sha: String,
    pub date: DateTime<Utc>,
    pub summary: String,
}

/// The first commit which build artifacts are made available through the CI for
/// bisection.
///
/// Due to our deletion policy which expires builds after 167 days, the build
/// artifacts of this commit itself is no longer available, so this may not be entirely useful;
/// however, it does limit the amount of commits somewhat.
const EPOCH_COMMIT: &str = "927c55d86b0be44337f37cf5b0a76fb8ba86e06c";

#[derive(Debug, StructOpt)]
#[structopt(after_help = "EXAMPLES:
    Run a fully automatic nightly bisect doing `cargo check`:
    ```
    cargo bisect-rustc --start 2018-07-07 --end 2018-07-30 --test-dir ../my_project/ -- check
    ```

    Run a PR-based bisect with manual prompts after each run doing `cargo build`:
    ```
    cargo bisect-rustc --start 6a1c0637ce44aeea6c60527f4c0e7fb33f2bcd0d \\
      --end 866a713258915e6cbb212d135f751a6a8c9e1c0a --test-dir ../my_project/ --prompt -- build
    ```")]
struct Opts {
    #[structopt(
        long = "regress",
        default_value = "error",
        help = "Custom regression definition",
        long_help = "Custom regression definition \
                     [error|non-error|ice|non-ice|success]"
    )]
    regress: String,

    #[structopt(
        short = "a",
        long = "alt",
        help = "Download the alt build instead of normal build"
    )]
    alt: bool,

    #[structopt(
        long = "host",
        help = "Host triple for the compiler",
        default_value = "unknown"
    )]
    host: String,

    #[structopt(long = "target", help = "Cross-compilation target platform")]
    target: Option<String>,

    #[structopt(long = "preserve", help = "Preserve the downloaded artifacts")]
    preserve: bool,

    #[structopt(
        long = "preserve-target",
        help = "Preserve the target directory used for builds"
    )]
    preserve_target: bool,

    #[structopt(
        long = "with-cargo",
        help = "Download cargo [default: installed cargo]"
    )]
    with_cargo: bool,

    #[structopt(long = "with-src", help = "Download rust-src [default: no download]")]
    with_src: bool,

    #[structopt(
        long = "test-dir",
        help = "Root directory for tests",
        default_value = ".",
        parse(from_os_str)
    )]
    test_dir: PathBuf,

    #[structopt(
        long = "prompt",
        help = "Manually evaluate for regression with prompts"
    )]
    prompt: bool,

    #[structopt(short = "v", long = "verbose", parse(from_occurrences))]
    verbosity: usize,

    #[structopt(
        help = "Arguments to pass to cargo or the file specified by --script during tests",
        multiple = true,
        last = true,
        parse(from_os_str)
    )]
    command_args: Vec<OsString>,

    #[structopt(long = "start", help = "Left bound for search (*without* regression)")]
    start: Option<Bound>,

    #[structopt(long = "end", help = "Right bound for search (*with* regression)")]
    end: Option<Bound>,

    #[structopt(long = "by-commit", help = "Bisect via commit artifacts")]
    by_commit: bool,

    #[structopt(
        long = "access",
        help = "How to access Rust git repository [github|checkout]"
    )]
    access: Option<String>,

    #[structopt(long = "install", help = "Install the given artifact")]
    install: Option<Bound>,

    #[structopt(
        long = "force-install",
        help = "Force installation over existing artifacts"
    )]
    force_install: bool,

    #[structopt(
        long = "script",
        help = "Script replacement for `cargo build` command",
        parse(from_os_str)
    )]
    script: Option<PathBuf>,
}

pub type GitDate = Date<Utc>;

#[derive(Clone, Debug)]
enum Bound {
    Commit(String),
    Date(GitDate),
}

#[derive(Fail, Debug)]
#[fail(display = "will never happen")]
struct BoundParseError {}

const YYYY_MM_DD: &str = "%Y-%m-%d";

impl FromStr for Bound {
    type Err = BoundParseError;
    fn from_str(s: &str) -> Result<Bound, BoundParseError> {
        match chrono::NaiveDate::parse_from_str(s, YYYY_MM_DD) {
            Ok(date) => Ok(Bound::Date(Date::from_utc(date, Utc))),
            Err(_) => Ok(Bound::Commit(s.to_string())),
        }
    }
}

impl Bound {
    fn sha(self) -> Result<String, Error> {
        match self {
            Bound::Commit(commit) => Ok(commit),
            Bound::Date(date) => {
                let date_str = date.format(YYYY_MM_DD);
                let url = format!(
                    "{}/{}/channel-rust-nightly-git-commit-hash.txt",
                    NIGHTLY_SERVER, date_str
                );

                eprintln!("fetching {}", url);
                let client = Client::new();
                let name = format!("nightly manifest {}", date_str);
                let (response, mut bar) = download_progress(&client, &name, &url)?;
                let mut response = TeeReader::new(response, &mut bar);
                let mut commit = String::new();
                response.read_to_string(&mut commit)?;

                eprintln!("converted {} to {}", date_str, commit);

                Ok(commit)
            }
        }
    }

    fn as_commit(self) -> Result<Self, Error> {
        self.sha().map(Bound::Commit)
    }
}

impl Opts {
    fn emit_cargo_output(&self) -> bool {
        self.verbosity >= 2
    }
}

#[derive(Debug, Fail)]
struct ExitError(i32);

impl fmt::Display for ExitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "exiting with {}", self.0)
    }
}

impl Config {
    fn default_outcome_of_output(&self, output: process::Output) -> TestOutcome {
        let status = output.status;
        let stdout_utf8 = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr_utf8 = String::from_utf8_lossy(&output.stderr).to_string();

        debug!(
            "status: {:?} stdout: {:?} stderr: {:?}",
            status, stdout_utf8, stderr_utf8
        );

        let saw_ice = || -> bool { stderr_utf8.contains("error: internal compiler error") };

        let input = (self.output_processing_mode(), status.success());
        let result = match input {
            (OutputProcessingMode::RegressOnErrorStatus, true) => TestOutcome::Baseline,
            (OutputProcessingMode::RegressOnErrorStatus, false) => TestOutcome::Regressed,

            (OutputProcessingMode::RegressOnSuccessStatus, true) => TestOutcome::Regressed,
            (OutputProcessingMode::RegressOnSuccessStatus, false) => TestOutcome::Baseline,

            (OutputProcessingMode::RegressOnIceAlone, _) => {
                if saw_ice() {
                    TestOutcome::Regressed
                } else {
                    TestOutcome::Baseline
                }
            }
            (OutputProcessingMode::RegressOnNotIce, _) => {
                if saw_ice() {
                    TestOutcome::Baseline
                } else {
                    TestOutcome::Regressed
                }
            }

            (OutputProcessingMode::RegressOnNonCleanError, true) => TestOutcome::Regressed,
            (OutputProcessingMode::RegressOnNonCleanError, false) => {
                if saw_ice() {
                    TestOutcome::Regressed
                } else {
                    TestOutcome::Baseline
                }
            }
        };
        debug!(
            "default_outcome_of_output: input: {:?} result: {:?}",
            input, result
        );
        result
    }

    fn output_processing_mode(&self) -> OutputProcessingMode {
        match self.args.regress.as_str() {
            "error" => OutputProcessingMode::RegressOnErrorStatus,
            "non-error" => OutputProcessingMode::RegressOnNonCleanError,
            "ice" => OutputProcessingMode::RegressOnIceAlone,
            "non-ice" => OutputProcessingMode::RegressOnNotIce,
            "success" => OutputProcessingMode::RegressOnSuccessStatus,
            setting => panic!("Unknown --regress setting: {:?}", setting),
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, StructOpt)]
/// Customize what is treated as regression.
enum OutputProcessingMode {
    /// `RegressOnErrorStatus`: Marks test outcome as `Regressed` if and only if
    /// the `rustc` process reports a non-success status. This corresponds to
    /// when `rustc` has an internal compiler error (ICE) or when it detects an
    /// error in the input program.
    ///
    /// This covers the most common use case for `cargo-bisect-rustc` and is
    /// thus the default setting.
    ///
    /// You explicitly opt into this seting via `--regress=error`.
    RegressOnErrorStatus,

    /// `RegressOnSuccessStatus`: Marks test outcome as `Regressed` if and only
    /// if the `rustc` process reports a success status. This corresponds to
    /// when `rustc` believes it has successfully compiled the program. This
    /// covers the use case for when you want to bisect to see when a bug was
    /// fixed.
    ///
    /// You explicitly opt into this seting via `--regress=success`.
    RegressOnSuccessStatus,

    /// `RegressOnIceAlone`: Marks test outcome as `Regressed` if and only if
    /// the `rustc` process issues a diagnostic indicating that an internal
    /// compiler error (ICE) occurred. This covers the use case for when you
    /// want to bisect to see when an ICE was introduced pon a codebase that is
    /// meant to produce a clean error.
    ///
    /// You explicitly opt into this seting via `--regress=ice`.
    RegressOnIceAlone,

    /// `RegressOnNotIce`: Marks test outcome as `Regressed` if and only if
    /// the `rustc` process does not issue a diagnostic indicating that an
    /// internal compiler error (ICE) occurred. This covers the use case for
    /// when you want to bisect to see when an ICE was fixed.
    ///
    /// You explicitly opt into this setting via `--regress=non-ice`
    RegressOnNotIce,

    /// `RegressOnNonCleanError`: Marks test outcome as `Baseline` if and only
    /// if the `rustc` process reports error status and does not issue any
    /// diagnostic indicating that an internal compiler error (ICE) occurred.
    /// This is the use case if the regression is a case where an ill-formed
    /// program has stopped being properly rejected by the compiler.
    ///
    /// (The main difference between this case and `RegressOnSuccessStatus` is
    /// the handling of ICE: `RegressOnSuccessStatus` assumes that ICE should be
    /// considered baseline; `RegressOnNonCleanError` assumes ICE should be
    /// considered a sign of a regression.)
    ///
    /// You explicitly opt into this seting via `--regress=non-error`.
    RegressOnNonCleanError,
}

impl OutputProcessingMode {
    fn must_process_stderr(&self) -> bool {
        match self {
            OutputProcessingMode::RegressOnErrorStatus
            | OutputProcessingMode::RegressOnSuccessStatus => false,

            OutputProcessingMode::RegressOnNonCleanError
            | OutputProcessingMode::RegressOnIceAlone
            | OutputProcessingMode::RegressOnNotIce => true,
        }
    }
}

// A simpler wrapper struct to make up for impoverished `Command` in libstd.
struct CommandTemplate(Vec<String>);

impl CommandTemplate {
    fn new(strings: impl Iterator<Item = String>) -> Self {
        CommandTemplate(strings.collect())
    }

    fn command(&self) -> Command {
        assert!(!self.0.is_empty());
        let mut cmd = Command::new(&self.0[0]);
        for arg in &self.0[1..] {
            cmd.arg(arg);
        }
        cmd
    }

    fn string(&self) -> String {
        assert!(!self.0.is_empty());
        let mut s = self.0[0].to_string();
        for arg in &self.0[1..] {
            s.push_str(" ");
            s.push_str(arg);
        }
        s
    }

    fn status(&self) -> Result<process::ExitStatus, InstallError> {
        self.command()
            .status()
            .map_err(|cause| InstallError::Subcommand {
                command: self.string(),
                cause,
            })
    }

    fn output(&self) -> Result<process::Output, InstallError> {
        self.command()
            .output()
            .map_err(|cause| InstallError::Subcommand {
                command: self.string(),
                cause,
            })
    }
}

struct Config {
    args: Opts,
    rustup_tmp_path: PathBuf,
    toolchains_path: PathBuf,
    target: String,
    is_commit: bool,
    repo_access: Box<dyn RustRepositoryAccessor>,
}

impl Config {
    fn from_args(mut args: Opts) -> Result<Config, Error> {
        if args.host == "unknown" {
            if let Some(host) = option_env!("HOST") {
                args.host = host.to_string();
            } else {
                bail!(
                    "Failed to auto-detect host triple and was not specified. Please provide it via --host"
                );
            }
        }

        let target = args.target.clone().unwrap_or_else(|| args.host.clone());
        let mut args = args;

        let mut toolchains_path = match env::var_os("RUSTUP_HOME") {
            Some(h) => PathBuf::from(h),
            None => {
                let mut home =
                    dirs::home_dir().ok_or_else(|| format_err!("Could not find home."))?;
                home.push(".rustup");
                home
            }
        };

        // We will download and extract the tarballs into this directory before installing.
        // Using `~/.rustup/tmp` instead of $TMPDIR ensures we could always perform installation by
        // renaming instead of copying the whole directory.
        let rustup_tmp_path = toolchains_path.join("tmp");
        if !rustup_tmp_path.exists() {
            fs::create_dir(&rustup_tmp_path)?;
        }

        toolchains_path.push("toolchains");
        if !toolchains_path.is_dir() {
            bail!(
                "`{}` is not a directory. Please install rustup.",
                toolchains_path.display()
            );
        }

        let is_commit = match (args.start.clone(), args.end.clone()) {
            (Some(Bound::Commit(_)), Some(Bound::Commit(_)))
            | (None, Some(Bound::Commit(_)))
            | (Some(Bound::Commit(_)), None) => Some(true),

            (Some(Bound::Date(_)), Some(Bound::Date(_)))
            | (None, Some(Bound::Date(_)))
            | (Some(Bound::Date(_)), None) => Some(false),

            (None, None) => None,

            (start, end) => bail!(
                "cannot take different types of bounds for start/end, got start: {:?} and end {:?}",
                start,
                end
            ),
        };

        if is_commit == Some(false) && args.by_commit {
            eprintln!("finding commit range that corresponds to dates specified");
            match (args.start, args.end) {
                (Some(b1), Some(b2)) => {
                    args.start = Some(b1.as_commit()?);
                    args.end = Some(b2.as_commit()?);
                }
                _ => unreachable!(),
            }
        }

        let repo_access: Box<dyn RustRepositoryAccessor>;
        repo_access = match args.access.as_ref().map(|x| x.as_str()) {
            None | Some("checkout") => Box::new(AccessViaLocalGit),
            Some("github") => Box::new(AccessViaGithub),
            Some(other) => bail!("unknown access argument: {}", other),
        };

        Ok(Config {
            is_commit: args.by_commit || is_commit == Some(true),
            args,
            target,
            toolchains_path,
            rustup_tmp_path,
            repo_access,
        })
    }
}

fn check_bounds(start: &Option<Bound>, end: &Option<Bound>) -> Result<(), Error> {
    match (&start, &end) {
        (Some(Bound::Date(start)), Some(Bound::Date(end))) if end < start => {
            bail!(
                "end should be after start, got start: {:?} and end {:?}",
                start,
                end
            );
        }
        _ => {}
    }

    Ok(())
}

// Application entry point
fn run() -> Result<(), Error> {
    env_logger::try_init()?;
    let args = env::args_os().filter(|a| a != "bisect-rustc");
    let args = Opts::from_iter(args);
    check_bounds(&args.start, &args.end)?;
    let cfg = Config::from_args(args)?;

    let client = Client::new();

    if let Some(ref bound) = cfg.args.install {
        install(&cfg, &client, bound)
    } else {
        bisect(&cfg, &client)
    }
}

fn install(cfg: &Config, client: &Client, bound: &Bound) -> Result<(), Error> {
    match *bound {
        Bound::Commit(ref sha) => {
            let sha = cfg.repo_access.commit(sha)?.sha;
            let mut t = Toolchain {
                spec: ToolchainSpec::Ci {
                    commit: sha,
                    alt: cfg.args.alt,
                },
                host: cfg.args.host.clone(),
                std_targets: vec![cfg.args.host.clone(), cfg.target.clone()],
            };
            t.std_targets.sort();
            t.std_targets.dedup();
            let dl_params = DownloadParams::for_ci(cfg);
            t.install(client, &dl_params)?;
        }
        Bound::Date(date) => {
            let mut t = Toolchain {
                spec: ToolchainSpec::Nightly { date },
                host: cfg.args.host.clone(),
                std_targets: vec![cfg.args.host.clone(), cfg.target.clone()],
            };
            t.std_targets.sort();
            t.std_targets.dedup();
            let dl_params = DownloadParams::for_nightly(cfg);
            t.install(client, &dl_params)?;
        }
    }

    Ok(())
}

// bisection entry point
fn bisect(cfg: &Config, client: &Client) -> Result<(), Error> {
    if cfg.is_commit {
        let bisection_result = bisect_ci(&cfg, &client)?;
        print_results(cfg, client, &bisection_result);
    } else {
        let nightly_bisection_result = bisect_nightlies(&cfg, &client)?;
        print_results(cfg, client, &nightly_bisection_result);
        let nightly_regression = &nightly_bisection_result.searched[nightly_bisection_result.found];

        if let ToolchainSpec::Nightly { date } = nightly_regression.spec {
            let previous_date = date - chrono::Duration::days(1);

            let working_commit = Bound::Date(previous_date).sha()?;
            let bad_commit = Bound::Date(date).sha()?;
            eprintln!(
                "looking for regression commit between {} and {}",
                previous_date.format(YYYY_MM_DD),
                date.format(YYYY_MM_DD),
            );

            let ci_bisection_result =
                bisect_ci_via(cfg, client, &*cfg.repo_access, &working_commit, &bad_commit)?;

            print_results(cfg, client, &ci_bisection_result);
            print_final_report(cfg, &nightly_bisection_result, &ci_bisection_result);
        }
    }

    Ok(())
}

fn searched_range(
    cfg: &Config,
    searched_toolchains: &Vec<Toolchain>,
) -> (ToolchainSpec, ToolchainSpec) {
    let first_toolchain = searched_toolchains.first().unwrap().spec.clone();
    let last_toolchain = searched_toolchains.last().unwrap().spec.clone();

    match (&first_toolchain, &last_toolchain) {
        (ToolchainSpec::Ci { .. }, ToolchainSpec::Ci { .. }) => (first_toolchain, last_toolchain),

        _ => {
            let start_toolchain = if let Some(Bound::Date(date)) = cfg.args.start {
                ToolchainSpec::Nightly { date }
            } else {
                first_toolchain
            };

            (
                start_toolchain,
                ToolchainSpec::Nightly {
                    date: get_end_date(cfg),
                },
            )
        }
    }
}

fn print_results(cfg: &Config, client: &Client, bisection_result: &BisectionResult) {
    let BisectionResult {
        searched: toolchains,
        dl_spec,
        found,
    } = bisection_result;

    let (start, end) = searched_range(cfg, toolchains);

    eprintln!("searched toolchains {} through {}", start, end);

    if toolchains[*found] == *toolchains.last().unwrap() {
        let t = &toolchains[*found];
        let r = match t.install(&client, &dl_spec) {
            Ok(()) => {
                let outcome = t.test(&cfg);
                if !cfg.args.preserve {
                    let _ = t.remove(&dl_spec);
                }
                // we want to fail, so a successful build doesn't satisfy us
                match outcome {
                    TestOutcome::Baseline => Satisfies::No,
                    TestOutcome::Regressed => Satisfies::Yes,
                }
            }
            Err(_) => {
                let _ = t.remove(&dl_spec);
                Satisfies::Unknown
            }
        };
        match r {
            Satisfies::Yes => {}
            Satisfies::No | Satisfies::Unknown => {
                eprintln!("error: The regression was not found. Expanding the bounds may help.");
                return;
            }
        }
    }

    let tc_found = format!("Regression in {}", toolchains[*found]);
    eprintln!("");
    eprintln!("");
    eprintln!("{}", "*".repeat(80).dimmed().bold());
    eprintln!("{}", tc_found.red());
    eprintln!("{}", "*".repeat(80).dimmed().bold());
    eprintln!("");
}

fn print_final_report(
    cfg: &Config,
    nightly_bisection_result: &BisectionResult,
    ci_bisection_result: &BisectionResult,
) {
    let BisectionResult {
        searched: nightly_toolchains,
        found: nightly_found,
        ..
    } = nightly_bisection_result;

    let BisectionResult {
        searched: ci_toolchains,
        found: ci_found,
        ..
    } = ci_bisection_result;

    #[rustfmt::skip]
    eprintln!("{}", "==================================================================================".dimmed());
    #[rustfmt::skip]
    eprintln!("{}", "= Please file this regression report on the rust-lang/rust GitHub repository     =".dimmed());
    #[rustfmt::skip]
    eprintln!("{}", "=        New issue: https://github.com/rust-lang/rust/issues/new                 =".dimmed());
    #[rustfmt::skip]
    eprintln!("{}", "=     Known issues: https://github.com/rust-lang/rust/issues                     =".dimmed());
    #[rustfmt::skip]
    eprintln!("{}", "= Copy and paste the text below into the issue report thread.  Thanks!           =".dimmed());
    #[rustfmt::skip]
    eprintln!("{}", "==================================================================================".dimmed());

    eprintln!("");

    let (start, end) = searched_range(cfg, nightly_toolchains);

    eprintln!("searched nightlies: from {} to {}", start, end);

    eprintln!("regressed nightly: {}", nightly_toolchains[*nightly_found],);

    eprintln!(
        "searched commits: from https://github.com/rust-lang/rust/commit/{} to https://github.com/rust-lang/rust/commit/{1}",
        ci_toolchains.first().unwrap(),
        ci_toolchains.last().unwrap(),
    );

    eprintln!(
        "regressed commit: https://github.com/rust-lang/rust/commit/{}",
        ci_toolchains[*ci_found],
    );

    eprintln!("");
    eprintln!("<details>");
    eprintln!(
        "<summary>bisected with <a href='{}'>cargo-bisect-rustc</a> v{}</summary>",
        env!("CARGO_PKG_REPOSITORY"),
        env!("CARGO_PKG_VERSION"),
    );
    eprintln!("");
    eprintln!("");
    if let Some(host) = option_env!("HOST") {
        eprintln!("Host triple: {}", host);
    }

    eprintln!("Reproduce with:");
    eprintln!("```bash");
    eprint!("cargo bisect-rustc ");
    for (index, arg) in env::args_os().enumerate() {
        if index > 1 {
            eprint!("{} ", arg.to_string_lossy());
        }
    }
    eprintln!("");
    eprintln!("```");
    eprintln!("</details>");
}

struct NightlyFinderIter {
    start_date: GitDate,
    current_date: GitDate,
}

impl NightlyFinderIter {
    fn new(start_date: GitDate) -> Self {
        Self {
            start_date,
            current_date: start_date,
        }
    }
}

impl Iterator for NightlyFinderIter {
    type Item = GitDate;

    fn next(&mut self) -> Option<GitDate> {
        let current_distance = self.start_date - self.current_date;

        let jump_length = if current_distance.num_days() < 7 {
            // first week jump by two days
            2
        } else if current_distance.num_days() < 49 {
            // from 2nd to 7th week jump weekly
            7
        } else {
            // from 7th week jump by two weeks
            14
        };

        self.current_date = self.current_date - chrono::Duration::days(jump_length);
        Some(self.current_date)
    }
}

#[test]
fn test_nightly_finder_iterator() {
    let start_date = chrono::Date::from_utc(
        chrono::naive::NaiveDate::from_ymd(2019, 01, 01),
        chrono::Utc,
    );

    let mut iter = NightlyFinderIter::new(start_date);

    assert_eq!(start_date - chrono::Duration::days(2), iter.next().unwrap());
    assert_eq!(start_date - chrono::Duration::days(4), iter.next().unwrap());
    assert_eq!(start_date - chrono::Duration::days(6), iter.next().unwrap());
    assert_eq!(start_date - chrono::Duration::days(8), iter.next().unwrap());
    assert_eq!(
        start_date - chrono::Duration::days(15),
        iter.next().unwrap()
    );
    assert_eq!(
        start_date - chrono::Duration::days(22),
        iter.next().unwrap()
    );
    assert_eq!(
        start_date - chrono::Duration::days(29),
        iter.next().unwrap()
    );
    assert_eq!(
        start_date - chrono::Duration::days(36),
        iter.next().unwrap()
    );
    assert_eq!(
        start_date - chrono::Duration::days(43),
        iter.next().unwrap()
    );
    assert_eq!(
        start_date - chrono::Duration::days(50),
        iter.next().unwrap()
    );
    assert_eq!(
        start_date - chrono::Duration::days(64),
        iter.next().unwrap()
    );
    assert_eq!(
        start_date - chrono::Duration::days(78),
        iter.next().unwrap()
    );
}

fn install_and_test(
    t: &Toolchain,
    cfg: &Config,
    client: &Client,
    dl_spec: &DownloadParams,
) -> Result<Satisfies, InstallError> {
    match t.install(&client, &dl_spec) {
        Ok(()) => {
            let outcome = t.test(&cfg);
            // we want to fail, so a successful build doesn't satisfy us
            let r = match outcome {
                TestOutcome::Baseline => Satisfies::No,
                TestOutcome::Regressed => Satisfies::Yes,
            };
            eprintln!("RESULT: {}, ===> {}", t, r);
            if !cfg.args.preserve {
                let _ = t.remove(&dl_spec);
            }
            eprintln!();
            Ok(r)
        }
        Err(error) => {
            if !cfg.args.preserve {
                let _ = t.remove(&dl_spec);
            }
            Err(error)
        }
    }
}

fn bisect_to_regression(
    toolchains: &[Toolchain],
    cfg: &Config,
    client: &Client,
    dl_spec: &DownloadParams,
) -> Result<usize, InstallError> {
    let found = least_satisfying(&toolchains, |t| {
        match install_and_test(&t, &cfg, &client, &dl_spec) {
            Ok(r) => r,
            Err(_) => Satisfies::Unknown,
        }
    });
    Ok(found)
}

fn get_start_date(cfg: &Config) -> chrono::Date<Utc> {
    if let Some(Bound::Date(date)) = cfg.args.start {
        date
    } else {
        get_end_date(cfg)
    }
}

fn get_end_date(cfg: &Config) -> chrono::Date<Utc> {
    if let Some(Bound::Date(date)) = cfg.args.end {
        date
    } else {
        if let Some(date) = Toolchain::default_nightly() {
            date
        } else {
            chrono::Utc::now().date()
        }
    }
}

// nightlies branch of bisect execution
fn bisect_nightlies(cfg: &Config, client: &Client) -> Result<BisectionResult, Error> {
    if cfg.args.alt {
        bail!("cannot bisect nightlies with --alt: not supported");
    }

    let dl_spec = DownloadParams::for_nightly(&cfg);

    // before this date we didn't have -std packages
    let end_at = chrono::Date::from_utc(
        chrono::naive::NaiveDate::from_ymd(2015, 10, 20),
        chrono::Utc,
    );
    let mut first_success = None;

    let mut nightly_date = get_start_date(cfg);
    let mut last_failure = get_end_date(cfg);
    let has_start = cfg.args.start.is_some();

    let mut nightly_iter = NightlyFinderIter::new(nightly_date);

    // this loop tests nightly toolchains to:
    // (1) validate that start date does not have regression (if defined on command line)
    // (2) identify a nightly date range for the bisection routine
    //
    // The tests here must be constrained to dates after 2015-10-20 (`end_at` date)
    // because -std packages were not available prior
    while nightly_date > end_at {
        let mut t = Toolchain {
            spec: ToolchainSpec::Nightly { date: nightly_date },
            host: cfg.args.host.clone(),
            std_targets: vec![cfg.args.host.clone(), cfg.target.clone()],
        };
        t.std_targets.sort();
        t.std_targets.dedup();
        if t.is_current_nightly() {
            eprintln!(
                "checking {} from the currently installed default nightly \
                       toolchain as the last failure",
                t
            );
        }

        match install_and_test(&t, &cfg, &client, &dl_spec) {
            Ok(r) => {
                // If Satisfies::No, then the regression was not identified in this nightly.
                // Break out of the loop and use this as the start date for the
                // bisection range
                if let Satisfies::No = r {
                    first_success = Some(nightly_date);
                    break;
                } else if has_start {
                    // If this date was explicitly defined on the command line &
                    // has regression, then this is an error in the test definition.
                    // The user must re-define the start date and try again
                    bail!(
                        "the start of the range ({}) must not reproduce the regression",
                        t
                    );
                } else {
                    last_failure = nightly_date;
                }

                nightly_date = nightly_iter.next().unwrap();
            }
            Err(InstallError::NotFound { .. }) => {
                // go back just one day, presumably missing a nightly
                nightly_date = nightly_date - chrono::Duration::days(1);
                eprintln!(
                    "*** unable to install {}. roll back one day and try again...",
                    t
                );
                if has_start {
                    bail!("could not find {}", t);
                }
            }
            Err(error) => return Err(error.into()),
        }
    }

    let first_success = first_success.ok_or(format_err!("could not find a nightly that built"))?;

    // confirm that the end of the date range has the regression
    let mut t_end = Toolchain {
        spec: ToolchainSpec::Nightly { date: last_failure },
        host: cfg.args.host.clone(),
        std_targets: vec![cfg.args.host.clone(), cfg.target.clone()],
    };
    t_end.std_targets.sort();
    t_end.std_targets.dedup();

    match install_and_test(&t_end, &cfg, &client, &dl_spec) {
        Ok(r) => {
            // If Satisfies::No, then the regression was not identified in this nightly.
            // this is an error, abort with error message
            if r == Satisfies::No {
                bail!(
                    "the end of the range ({}) does not reproduce the regression",
                    t_end
                );
            }
        }
        Err(error) => return Err(error.into()),
    }

    let toolchains = toolchains_between(
        cfg,
        ToolchainSpec::Nightly {
            date: first_success,
        },
        ToolchainSpec::Nightly { date: last_failure },
    );

    let found = bisect_to_regression(&toolchains, &cfg, client, &dl_spec)?;

    Ok(BisectionResult {
        dl_spec,
        searched: toolchains,
        found,
    })
}

fn toolchains_between(cfg: &Config, a: ToolchainSpec, b: ToolchainSpec) -> Vec<Toolchain> {
    match (a, b) {
        (ToolchainSpec::Nightly { date: a }, ToolchainSpec::Nightly { date: b }) => {
            let mut toolchains = Vec::new();
            let mut date = a;
            while date <= b {
                let mut t = Toolchain {
                    spec: ToolchainSpec::Nightly { date },
                    host: cfg.args.host.clone(),
                    std_targets: vec![cfg.args.host.clone(), cfg.target.clone()],
                };
                t.std_targets.sort();
                t.std_targets.dedup();
                toolchains.push(t);
                date = date + Duration::days(1);
            }
            toolchains
        }
        _ => unimplemented!(),
    }
}

// CI branch of bisect execution
fn bisect_ci(cfg: &Config, client: &Client) -> Result<BisectionResult, Error> {
    eprintln!("bisecting ci builds");
    let start = if let Some(Bound::Commit(ref sha)) = cfg.args.start {
        sha
    } else {
        EPOCH_COMMIT
    };

    let end = if let Some(Bound::Commit(ref sha)) = cfg.args.end {
        sha
    } else {
        "origin/master"
    };

    eprintln!("starting at {}, ending at {}", start, end);

    bisect_ci_via(cfg, client, &*cfg.repo_access, start, end)
}

fn bisect_ci_via(
    cfg: &Config,
    client: &Client,
    access: &dyn RustRepositoryAccessor,
    start_sha: &str,
    end_ref: &str,
) -> Result<BisectionResult, Error> {
    let end_sha = access.commit(end_ref)?.sha;
    let commits = access.commits(start_sha, &end_sha)?;

    assert_eq!(commits.last().expect("at least one commit").sha, end_sha);

    commits.iter().zip(commits.iter().skip(1)).all(|(a, b)| {
        let sorted_by_date = a.date <= b.date;
        assert!(
            sorted_by_date,
            "commits must chronologically ordered,\
                                 but {:?} comes after {:?}",
            a, b
        );
        sorted_by_date
    });

    for (j, commit) in commits.iter().enumerate() {
        eprintln!(
            "  commit[{}] {}: {}",
            j,
            commit.date.date(),
            commit.summary.split("\n").next().unwrap()
        )
    }

    bisect_ci_in_commits(cfg, client, &start_sha, &end_sha, commits)
}

fn bisect_ci_in_commits(
    cfg: &Config,
    client: &Client,
    start: &str,
    end: &str,
    mut commits: Vec<Commit>,
) -> Result<BisectionResult, Error> {
    let dl_spec = DownloadParams::for_ci(cfg);
    let now = chrono::Utc::now();
    commits.retain(|c| now.signed_duration_since(c.date).num_days() < 167);

    if commits.is_empty() {
        bail!(
            "no commits between {} and {} within last 167 days",
            start,
            end
        );
    }

    if let Some(ref c) = commits.last() {
        if end != "origin/master" && !c.sha.starts_with(end) {
            bail!("expected to end with {}, but ended with {}", end, c.sha);
        }
    }

    eprintln!("validated commits found, specifying toolchains");
    eprintln!("");

    let toolchains = commits
        .into_iter()
        .map(|commit| {
            let mut t = Toolchain {
                spec: ToolchainSpec::Ci {
                    commit: commit.sha,
                    alt: cfg.args.alt,
                },
                host: cfg.args.host.clone(),
                std_targets: vec![cfg.args.host.clone(), cfg.target.clone()],
            };
            t.std_targets.sort();
            t.std_targets.dedup();
            t
        })
        .collect::<Vec<_>>();

    if !toolchains.is_empty() {
        // validate commit at start of range
        match install_and_test(&toolchains[0], &cfg, &client, &dl_spec) {
            Ok(r) => {
                // If Satisfies::Yes, then the commit at the beginning of the range
                // has the regression, this is an error
                if r == Satisfies::Yes {
                    bail!(
                        "the commit at the start of the range ({}) includes the regression",
                        &toolchains[0]
                    );
                }
            }
            Err(error) => return Err(error.into()),
        }

        // validate commit at end of range
        match install_and_test(&toolchains[toolchains.len() - 1], &cfg, &client, &dl_spec) {
            Ok(r) => {
                // If Satisfies::No, then the regression was not identified at the end of the
                // commit range, this is an error
                if r == Satisfies::No {
                    bail!(
                        "the commit at the end of the range ({}) does not reproduce the regression",
                        &toolchains[toolchains.len() - 1]
                    );
                }
            }
            Err(error) => return Err(error.into()),
        }
    }

    let found = bisect_to_regression(&toolchains, &cfg, client, &dl_spec)?;

    Ok(BisectionResult {
        searched: toolchains,
        found,
        dl_spec,
    })
}

#[derive(Clone)]
struct BisectionResult {
    searched: Vec<Toolchain>,
    found: usize,
    dl_spec: DownloadParams,
}

fn main() {
    if let Err(err) = run() {
        match err.downcast::<ExitError>() {
            Ok(ExitError(code)) => process::exit(code),
            Err(err) => {
                let error_str = "ERROR:".red().bold();
                eprintln!("{} {}", error_str, err);
                process::exit(1);
            }
        }
    }
}
