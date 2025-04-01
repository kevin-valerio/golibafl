use clap::{Parser, Subcommand};
use libafl::{
    corpus::{CachedOnDiskCorpus, Corpus, OnDiskCorpus},
    executors::{inprocess::InProcessExecutor, ExitKind},
    feedback_or, feedback_or_fast,
    feedbacks::{CrashFeedback, MaxMapFeedback},
    fuzzer::{Fuzzer, StdFuzzer},
    inputs::{BytesInput, HasTargetBytes},
    mutators::scheduled::StdScheduledMutator,
    nonzero,
    prelude::{
        havoc_mutations, powersched::PowerSchedule, tokens_mutations, CalibrationStage, CanTrack,
        ClientDescription, EventConfig, I2SRandReplace, IndexesLenTimeMinimizerScheduler, Launcher,
        RandBytesGenerator, SimpleMonitor, StdMOptMutator, StdMapObserver, StdWeightedScheduler,
        TimeFeedback, TimeObserver, Tokens,
    },
    stages::{mutational::StdMutationalStage, StdPowerMutationalStage, TracingStage},
    state::{HasCorpus, StdState},
    Error, HasMetadata,
};
use libafl_bolts::{
    prelude::{Cores, StdShMemProvider},
    rands::StdRand,
    shmem::ShMemProvider,
    tuples::{tuple_list, Merge},
};
use libafl_targets::{
    autotokens, extra_counters, libfuzzer::libfuzzer_test_one_input, libfuzzer_initialize,
    CmpLogObserver, COUNTERS_MAPS,
};
use mimalloc::MiMalloc;
use std::{env, fs::read_dir, path::PathBuf, time::Duration};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

// Command line arguments with clap
#[derive(Subcommand, Debug, Clone)]
enum Mode {
    Run {
        #[clap(short, long, value_name = "DIR", default_value = "./input")]
        input: PathBuf,
    },
    Fuzz {
        #[clap(
            short = 'j',
            long,
            value_parser = Cores::from_cmdline,
            help = "Spawn clients in each of the provided cores. Broker runs in the 0th core. 'all' to select all available cores. 'none' to run a client without binding to any core. eg: '1,2-4,6' selects the cores 1,2,3,4,6.",
            name = "CORES",
            default_value = "all",
            )]
        cores: Cores,

        #[clap(
            short = 'p',
            long,
            help = "Choose the broker TCP port, default is 1337",
            name = "PORT",
            default_value = "1337"
        )]
        broker_port: u16,

        #[clap(
            short,
            long,
            value_name = "DIR",
            default_value = "./input",
            help = "Initial corpus directory (will only be read)"
        )]
        input: PathBuf,

        #[clap(
            short,
            long,
            value_name = "OUTPUT",
            default_value = "./output",
            help = "Fuzzer's output directory"
        )]
        output: PathBuf,
    },
}
// Clap top level struct for args
// `Parser` is needed for the top-level command-line interface
#[derive(Parser, Debug, Clone)]
struct Cli {
    #[command(subcommand)]
    mode: Mode,
}

// Run the corpus without fuzzing
fn run(input: PathBuf) {
    let files = if input.is_dir() {
        input
            .read_dir()
            .expect("Unable to read dir")
            .filter_map(core::result::Result::ok)
            .map(|e| e.path())
            .collect()
    } else {
        vec![input]
    };

    // Call LLVMFuzzerInitialize() if present.
    let args: Vec<String> = env::args().collect();
    if unsafe { libfuzzer_initialize(&args) } == -1 {
        println!("Warning: LLVMFuzzerInitialize failed with -1");
    }

    for f in &files {
        println!("\x1b[33mRunning: {}\x1b[0m", f.display());
        let inp =
            std::fs::read(f).unwrap_or_else(|_| panic!("Unable to read file {}", &f.display()));
        if inp.len() > 1 {
            println!("INPUT: {inp:?}");
            unsafe {
                libfuzzer_test_one_input(&inp);
            }
        }
    }
}

// Fuzzing function, wrapping the exported libfuzzer functions from golang
#[allow(clippy::too_many_lines)]
#[allow(static_mut_refs)]
fn fuzz(cores: &Cores, broker_port: u16, input: &PathBuf, output: &PathBuf) {
    let args: Vec<String> = env::args().collect();
    if unsafe { libfuzzer_initialize(&args) } == -1 {
        println!("Warning: LLVMFuzzerInitialize failed with -1");
    }
    let shmem_provider = StdShMemProvider::new().expect("Failed to init shared memory");
    let monitor = SimpleMonitor::new(|s| println!("{s}"));

    let mut run_client = |state: Option<_>,
                          mut restarting_mgr,
                          client_description: ClientDescription| {
        // We assume COUNTERS_MAP len == 1  so that we can use StdMapObserver instead of Multimapobserver to improve performance.
        let counters_map_len = unsafe { COUNTERS_MAPS.len() };
        assert!(
            (counters_map_len == 1),
            "{}",
            format!("Unexpected COUNTERS_MAPS length: {counters_map_len}")
        );
        let edges = unsafe { extra_counters() };
        let edges_observer =
            StdMapObserver::from_mut_slice("edges", edges.into_iter().next().unwrap())
                .track_indices();

        // Observers
        let time_observer = TimeObserver::new("time");
        let cmplog_observer = CmpLogObserver::new("cmplog", true);
        let map_feedback = MaxMapFeedback::new(&edges_observer);
        let calibration = CalibrationStage::new(&map_feedback);

        let mut feedback = feedback_or_fast!(
            // New maximization map feedback linked to the edges observer and the feedback state
            map_feedback,
            // Time feedback, this one does not need a feedback state
            TimeFeedback::new(&time_observer)
        );

        // A feedback to choose if an input is a solution or not
        let mut objective = feedback_or_fast!(CrashFeedback::new());

        // create a State from scratch
        let mut state = state.unwrap_or_else(|| {
            StdState::new(
                StdRand::new(),
                // Corpus that will be evolved
                CachedOnDiskCorpus::new(
                    format!("{}/queue/{}", output.display(), client_description.id()),
                    4096,
                )
                .unwrap(),
                // Corpus in which we store solutions
                OnDiskCorpus::new(format!("{}/crashes", output.display())).unwrap(),
                &mut feedback,
                &mut objective,
            )
            .unwrap()
        });

        // Setup a randomic Input2State stage
        let i2s =
            StdMutationalStage::new(StdScheduledMutator::new(tuple_list!(I2SRandReplace::new())));

        // Setup a MOPT mutator
        let mutator = StdMOptMutator::new(
            &mut state,
            havoc_mutations().merge(tokens_mutations()),
            7,
            5,
        )?;

        let power: StdPowerMutationalStage<_, _, BytesInput, _, _, _> =
            StdPowerMutationalStage::new(mutator);

        let scheduler = IndexesLenTimeMinimizerScheduler::new(
            &edges_observer,
            StdWeightedScheduler::with_schedule(
                &mut state,
                &edges_observer,
                Some(PowerSchedule::fast()),
            ),
        );

        // A fuzzer with feedbacks and a corpus scheduler
        let mut fuzzer = StdFuzzer::new(scheduler, feedback, objective);

        // The closure that we want to fuzz
        let mut harness = |input: &BytesInput| {
            let target = input.target_bytes();
            unsafe {
                libfuzzer_test_one_input(&target);
            }
            ExitKind::Ok
        };

        let mut tracing_harness = harness;

        let mut executor = InProcessExecutor::with_timeout(
            &mut harness,
            tuple_list!(edges_observer, time_observer),
            &mut fuzzer,
            &mut state,
            &mut restarting_mgr,
            Duration::new(1, 0),
        )?;

        // Setup a tracing stage in which we log comparisons
        let tracing = TracingStage::new(InProcessExecutor::new(
            &mut tracing_harness,
            tuple_list!(cmplog_observer),
            &mut fuzzer,
            &mut state,
            &mut restarting_mgr,
        )?);

        let mut stages = tuple_list!(calibration, tracing, i2s, power);

        if state.metadata_map().get::<Tokens>().is_none() {
            let mut toks = Tokens::default();
            toks += autotokens()?;

            if !toks.is_empty() {
                state.add_metadata(toks);
            }
        }

        // Load corpus from input folder
        // In case the corpus is empty (on first run), reset
        if state.must_load_initial_inputs() {
            if read_dir(input).iter().len() == 0 {
                // Generator of printable bytearrays of max size 32
                let mut generator = RandBytesGenerator::new(nonzero!(32));

                // Generate 8 initial inputs
                state
                    .generate_initial_inputs(
                        &mut fuzzer,
                        &mut executor,
                        &mut generator,
                        &mut restarting_mgr,
                        8,
                    )
                    .expect("Failed to generate the initial corpus");
                println!(
                    "We imported {} inputs from the generator.",
                    state.corpus().count()
                );
            } else {
                println!("Loading from {:?}", input);
                // Load from disk
                state
                    .load_initial_inputs(
                        &mut fuzzer,
                        &mut executor,
                        &mut restarting_mgr,
                        &[input.to_path_buf()],
                    )
                    .unwrap_or_else(|_| {
                        panic!("Failed to load initial corpus at {:?}", input);
                    });
                println!("We imported {} inputs from disk.", state.corpus().count());
            }
        }

        fuzzer.fuzz_loop(&mut stages, &mut executor, &mut state, &mut restarting_mgr)?;
        Ok(())
    };
    match Launcher::builder()
        .shmem_provider(shmem_provider)
        .configuration(EventConfig::from_name("default"))
        .monitor(monitor)
        .run_client(&mut run_client)
        .cores(cores)
        .broker_port(broker_port)
        .stdout_file(Some("/dev/null")) // Comment this out for debugging
        .build()
        .launch()
    {
        Ok(()) => (),
        Err(Error::ShuttingDown) => println!("Fuzzing stopped by user. Good bye."),
        Err(err) => panic!("Failed to run launcher: {err:?}"),
    }
}

// Entry point wrapping clap and calling fuzz or run
pub fn main() {
    let cli = Cli::parse();

    match cli.mode {
        Mode::Fuzz {
            cores,
            broker_port,
            input,
            output,
        } => fuzz(&cores, broker_port, &input, &output),
        Mode::Run { input } => {
            run(input);
        }
    }
}
