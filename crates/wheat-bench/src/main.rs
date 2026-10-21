//! `wheat-bench`: runs Wheat's benchmark profiles. See the library docs.

// The same allocator as `wheat` and `wheat-server`, so the work the cases run in
// this process is measured as it runs there.
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("wheat-bench measures with Linux-only interfaces (wait4, /proc)");
    std::process::exit(2);
}

#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    linux::main()
}

#[cfg(target_os = "linux")]
mod linux {
    use std::{io::Write, path::PathBuf};

    use anyhow::Result;
    use clap::{Parser, Subcommand};
    use wheat_bench::{
        Destination, corpus, history,
        host::Checkout,
        publish,
        quick::{self, Scale},
        scratch_root, wheat_binary,
    };

    #[derive(Parser)]
    #[command(about = "Run Wheat's benchmark profiles")]
    struct Cli {
        #[command(subcommand)]
        command: Command,
    }

    #[derive(Subcommand)]
    enum Command {
        /// The `wheat-quick` profile: a few minutes of focused scale cases.
        Quick {
            /// `smoke` uses tiny sizes and checks no budgets; for tests only.
            #[arg(long, value_enum, default_value_t = Scale::Full)]
            scale: Scale,
            /// Run only this case (repeatable). Default: every case.
            #[arg(long = "case", value_name = "NAME")]
            cases: Vec<String>,
            #[command(flatten)]
            output: Output,
        },
        /// The `wheat-gothab-workflow` profile against a full Git clone (about 55
        /// minutes on the Gothab repository).
        Corpus {
            /// A non-shallow Git checkout to import.
            corpus: PathBuf,
            #[command(flatten)]
            output: Output,
        },
        /// Compare two results files from `bench/results`.
        Compare { old: PathBuf, new: PathBuf },
        /// Run one quick case in this process and print its output as JSON.
        #[command(hide = true)]
        Case {
            name: String,
            #[arg(long, value_enum)]
            scale: Scale,
            #[arg(long)]
            wheat: PathBuf,
            #[arg(long)]
            scratch: PathBuf,
        },
    }

    #[derive(clap::Args)]
    struct Output {
        /// Where to write the Gothab profile report.
        #[arg(long, default_value = "profile.json")]
        output: PathBuf,
        /// Don't save a results file under `bench/results` (CI runs).
        #[arg(long)]
        no_save: bool,
    }

    pub(crate) fn main() -> Result<()> {
        match Cli::parse().command {
            Command::Quick {
                scale,
                cases,
                output,
            } => {
                let destination = Destination::new(output.output, output.no_save)?;
                let checkout = Checkout::find(&std::env::current_dir()?)?;
                let report = quick::run(
                    &std::env::current_exe()?,
                    &wheat_binary()?,
                    scale,
                    &cases,
                    &scratch_root()?,
                    &checkout,
                )?;
                publish(&report, &destination)
            }
            Command::Corpus { corpus, output } => {
                let destination = Destination::new(output.output, output.no_save)?;
                let checkout = Checkout::find(&std::env::current_dir()?)?;
                let report = corpus::run(&corpus, wheat_binary()?, &scratch_root()?, &checkout)?;
                publish(&report, &destination)
            }
            Command::Compare { old, new } => {
                let old = history::Results::read(&old)?;
                let new = history::Results::read(&new)?;
                print!("{}", history::compare(&old, &new));
                Ok(())
            }
            Command::Case {
                name,
                scale,
                wheat,
                scratch,
            } => {
                // Spans of this process, such as an in-process server's; reported on
                // stderr or to `WHEAT_TRACE` when the case ends.
                let _span_trace = wheat_trace::install_from_env();
                let output = quick::run_case(&name, scale, wheat, &scratch)?;
                let mut stdout = std::io::stdout().lock();
                serde_json::to_writer(&mut stdout, &output)?;
                writeln!(stdout)?;
                Ok(())
            }
        }
    }
}
