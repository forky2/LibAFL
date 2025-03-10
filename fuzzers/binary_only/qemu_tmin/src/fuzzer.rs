//! A libfuzzer-like fuzzer using qemu for binary-only coverage
#[cfg(feature = "i386")]
use core::mem::size_of;
use core::time::Duration;
use std::{env, fmt::Write, fs::DirEntry, io, path::PathBuf, process, ptr::NonNull};

use clap::{builder::Str, Parser};
use libafl::{
    corpus::{Corpus, HasCurrentCorpusId, InMemoryCorpus, InMemoryOnDiskCorpus, NopCorpus},
    events::{
        launcher::Launcher, ClientDescription, EventConfig, LlmpRestartingEventManager, SendExiting, SimpleRestartingEventManager,
    },
    executors::ExitKind,
    feedbacks::MaxMapFeedback,
    fuzzer::StdFuzzer,
    inputs::{BytesInput, HasTargetBytes},
    monitors::{MultiMonitor, SimpleMonitor},
    mutators::{havoc_mutations, StdScheduledMutator},
    observers::{ConstMapObserver, HitcountsMapObserver},
    schedulers::QueueScheduler,
    stages::{ObserverEqualityFactory, StagesTuple, StdTMinMutationalStage},
    state::{HasCorpus, StdState},
    Error,
};
use libafl_bolts::{
    core_affinity::Cores,
    os::unix_signals::Signal,
    rands::StdRand,
    shmem::{ShMemProvider, StdShMemProvider},
    tuples::tuple_list,
    AsSlice, AsSliceMut,
};
use libafl_qemu::{
    elf::EasyElf,
    modules::{SnapshotModule, StdEdgeCoverageChildModule},
    ArchExtras, Emulator, GuestAddr, GuestReg, MmapPerms, Qemu, QemuExecutor,
    QemuExitReason, QemuRWError, QemuExitError, QemuShutdownCause, Regs,
};
use libafl_targets::{EDGES_MAP_DEFAULT_SIZE, EDGES_MAP_PTR};

#[derive(Default)]
pub struct Version;

/// Parse a millis string to a [`Duration`]. Used for arg parsing.
fn timeout_from_millis_str(time: &str) -> Result<Duration, Error> {
    Ok(Duration::from_millis(time.parse()?))
}

impl From<Version> for Str {
    fn from(_: Version) -> Str {
        let version = [
            ("Architecture:", env!("CPU_TARGET")),
            ("Build Timestamp:", env!("VERGEN_BUILD_TIMESTAMP")),
            ("Describe:", env!("VERGEN_GIT_DESCRIBE")),
            ("Commit SHA:", env!("VERGEN_GIT_SHA")),
            ("Commit Date:", env!("VERGEN_RUSTC_COMMIT_DATE")),
            ("Commit Branch:", env!("VERGEN_GIT_BRANCH")),
            ("Rustc Version:", env!("VERGEN_RUSTC_SEMVER")),
            ("Rustc Channel:", env!("VERGEN_RUSTC_CHANNEL")),
            ("Rustc Host Triple:", env!("VERGEN_RUSTC_HOST_TRIPLE")),
            ("Rustc Commit SHA:", env!("VERGEN_RUSTC_COMMIT_HASH")),
            ("Cargo Target Triple", env!("VERGEN_CARGO_TARGET_TRIPLE")),
        ]
        .iter()
        .fold(String::new(), |mut output, (k, v)| {
            let _ = writeln!(output, "{k:25}: {v}");
            output
        });

        format!("\n{version:}").into()
    }
}

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
#[command(
    name = format!("qemu_tmin-{}",env!("CPU_TARGET")),
    version = Version::default(),
    about,
    long_about = "Module for generating DrCov coverage data using QEMU instrumentation"
)]
pub struct FuzzerOptions {
    #[arg(long, help = "Output directory")]
    output: String,

    #[arg(long, help = "Input directory")]
    input: PathBuf,

    #[arg(long, help = "Timeout in seconds", default_value = "5000", value_parser = timeout_from_millis_str)]
    timeout: Duration,

    #[arg(long = "port", help = "Broker port", default_value_t = 1337_u16)]
    port: u16,

    #[arg(long, help = "Cpu cores to use", default_value = "all", value_parser = Cores::from_cmdline)]
    cores: Cores,

    #[arg(
        long,
        help = "Number of iterations for minimization",
        default_value_t = 1024_usize
    )]
    iterations: usize,

    #[clap(short, long, help = "Enable output from the fuzzer clients")]
    verbose: bool,

    #[arg(last = true, help = "Arguments passed to the target")]
    args: Vec<String>,
}

pub const MAX_INPUT_SIZE: usize = 1048576; // 1MB

pub fn fuzz() -> Result<(), Error> {
    env_logger::init();
    let mut options = FuzzerOptions::parse();

    let corpus_files = options.input
        .read_dir()
        .expect("Failed to read corpus dir")
        .map(|x| Ok(x?.path()))
        .collect::<Result<Vec<PathBuf>, io::Error>>()
        .expect("Failed to read dir entry");

    // let num_files = corpus_files.len();
    // let num_cores = options.cores.ids.len();
    // let files_per_core = (num_files as f64 / num_cores as f64).ceil() as usize;

    let program = env::args().next().unwrap();
    log::info!("Program: {program:}");

    options.args.insert(0, program);
    log::info!("ARGS: {:#?}", options.args);

    env::remove_var("LD_LIBRARY_PATH");

    // let mut run_client = |state: Option<_>,
    //                       mut mgr: LlmpRestartingEventManager<_, _, _, _, _>,
    //                       client_description: ClientDescription| {
        let mut shmem_provider = StdShMemProvider::new().expect("Failed to init shared memory");

        let mut edges_shmem = shmem_provider.new_shmem(EDGES_MAP_DEFAULT_SIZE).unwrap();
        let edges = edges_shmem.as_slice_mut();
        unsafe { EDGES_MAP_PTR = edges.as_mut_ptr() };

        let mut edges_observer = unsafe {
            HitcountsMapObserver::new(ConstMapObserver::from_mut_ptr(
                "edges",
                NonNull::new(edges.as_mut_ptr())
                    .expect("The edge map pointer is null.")
                    .cast::<[u8; EDGES_MAP_DEFAULT_SIZE]>(),
            ))
        };

        #[cfg(feature = "fork")]
        let modules = tuple_list!(StdEdgeCoverageChildModule::builder()
            .const_map_observer(edges_observer.as_mut())
            .build()?);
    
        #[cfg(feature = "snapshot")]
        let modules = tuple_list!(
            StdEdgeCoverageChildModule::builder()
                .const_map_observer(edges_observer.as_mut())
                .build()?,
            SnapshotModule::new()
        );

        let emulator = Emulator::empty()
            .qemu_parameters(options.args.clone())
            .modules(modules)
            .build()?;
        let qemu = emulator.qemu();

        let mut elf_buffer = Vec::new();
        let elf = EasyElf::from_file(qemu.binary_path(), &mut elf_buffer).unwrap();

        let test_one_input_ptr = elf
            .resolve_symbol("LLVMFuzzerTestOneInput", qemu.load_addr())
            .expect("Symbol LLVMFuzzerTestOneInput not found");
        log::info!("LLVMFuzzerTestOneInput @ {test_one_input_ptr:#x}");

        qemu.entry_break(test_one_input_ptr);

        // for m in qemu.mappings() {
        //     log::info!(
        //         "Mapping: 0x{:016x}-0x{:016x}, {}",
        //         m.start(),
        //         m.end(),
        //         m.path().unwrap_or(&"<EMPTY>".to_string())
        //     );
        // }

        let pc: GuestReg = qemu.read_reg(Regs::Pc).unwrap();
        log::info!("Break at {pc:#x}");

        let ret_addr: GuestAddr = qemu.read_return_address().unwrap();
        log::info!("Return address = {ret_addr:#x}");

        qemu.set_breakpoint(ret_addr);

        let input_addr = qemu
            .map_private(0, MAX_INPUT_SIZE, MmapPerms::ReadWrite)
            .unwrap();
        log::info!("Placing input at {input_addr:#x}");

        let stack_ptr: GuestAddr = qemu.read_reg(Regs::Sp).unwrap();

        let monitor = SimpleMonitor::with_user_monitor(|s| {
            println!("{s}");
        });
        let (state, mut mgr) = match SimpleRestartingEventManager::launch(monitor, &mut shmem_provider)
        {
            Ok(res) => res,
            Err(err) => match err {
                Error::ShuttingDown => {
                    return Ok(());
                }
                _ => {
                    panic!("Failed to setup the restarter: {err}");
                }
            },
        };
    

        // let reset = |qemu: Qemu, buf: &[u8], len: GuestReg| -> Result<(), QemuRWError> {
        //     unsafe {
        //         qemu.write_mem(input_addr, buf)?;
        //         qemu.write_reg(Regs::Pc, test_one_input_ptr)?;
        //         qemu.write_reg(Regs::Sp, stack_ptr)?;
        //         qemu.write_return_address(ret_addr)?;
        //         qemu.write_function_argument(0, input_addr)?;
        //         qemu.write_function_argument(1, len)?;

        //         match qemu.run() {
        //             Ok(QemuExitReason::Breakpoint(_)) => {}
        //             Ok(QemuExitReason::End(QemuShutdownCause::HostSignal(
        //                 Signal::SigInterrupt,
        //             ))) => process::exit(0),
        //             _ => panic!("Unexpected QEMU exit."),
        //         }

        //         Ok(())
        //     }
        // };

        let mut feedback = MaxMapFeedback::new(&edges_observer);

        let mut objective = ();

        let mut state = state.unwrap_or_else(|| {
            StdState::new(
                StdRand::new(),
                // InMemoryOnDiskCorpus::new(PathBuf::from(&options.output)).unwrap(),
                InMemoryCorpus::new(),
                // InMemoryCorpus::new(),
                NopCorpus::new(),
                &mut feedback,
                &mut objective,
            )
            .unwrap()
        });

        let scheduler = QueueScheduler::new();
        let mut fuzzer = StdFuzzer::new(scheduler, feedback, objective);

        #[cfg(feature = "snapshot")]
        let mut harness =
            |_emulator: &mut Emulator<_, _, _, _, _, _, _>, _state: &mut _, input: &BytesInput| {
                let target = input.target_bytes();
                // let qemu = emulator.qemu();
                let mut buf = target.as_slice();
                let mut len = buf.len();
                if len > MAX_INPUT_SIZE {
                    buf = &buf[0..MAX_INPUT_SIZE];
                    len = MAX_INPUT_SIZE;
                }
                let len = len as GuestReg;
                // reset(qemu, buf, len).unwrap();
                unsafe {
                    qemu.write_mem(input_addr, buf).expect("qemu write failed.");

                    qemu.write_reg(Regs::Pc, test_one_input_ptr).unwrap();
                    qemu.write_reg(Regs::Sp, stack_ptr).unwrap();
                    qemu.write_return_address(ret_addr).unwrap();
                    qemu.write_function_argument(0, input_addr).unwrap();
                    qemu.write_function_argument(1, len).unwrap();

                    match qemu.run() {
                        Ok(QemuExitReason::Breakpoint(_)) => {}
                        Ok(QemuExitReason::End(QemuShutdownCause::HostSignal(
                            Signal::SigInterrupt,
                        ))) => process::exit(0),
                        Err(QemuExitError::UnexpectedExit) => return ExitKind::Crash,
                        _ => panic!("Unexpected QEMU exit."),
                    }
                }

                ExitKind::Ok
            };

        // let core_id = client_description.core_id();
        // let core_idx = options
        //     .cores
        //     .position(core_id)
        //     .expect("Failed to get core index");

        // let files = corpus_files
        //     .iter()
        //     .skip(files_per_core * core_idx)
        //     .take(files_per_core)
        //     .map(|x| x.path())
        //     .collect::<Vec<PathBuf>>();

        // if files.is_empty() {
        //     mgr.send_exiting()?;
        //     Err(Error::ShuttingDown)?
        // }

        /////////////////
        // let minimizer = StdScheduledMutator::new(havoc_mutations());
        // let factory = ObserverEqualityFactory::new(&edges_observer);
        // let mut stages = tuple_list!(StdTMinMutationalStage::new(
        //     minimizer,
        //     factory,
        //     options.iterations
        // ));
        /////////////////

        #[cfg(feature = "snapshot")]
        let mut executor = QemuExecutor::new(
            emulator,
            &mut harness,
            tuple_list!(edges_observer),
            &mut fuzzer,
            &mut state,
            &mut mgr,
            options.timeout,
        )
        .expect("Failed to create QemuExecutor");

                                    println!("Importing {} seeds...", corpus_files.len());
                                    if state.must_load_initial_inputs() {
                                        state
                                            .load_initial_inputs(&mut fuzzer, &mut executor, &mut mgr, &corpus_files)
                                            .unwrap_or_else(|_| {
                                                println!("Failed to load initial corpus");
                                                process::exit(0);
                                            });
                                        println!("Imported {} seeds from disk.", state.corpus().count());
                                    }
    
        ////////// FORCED DOESN'T DO ANYTHING USEFUL. If the file's the same, it's the same!
        // state.load_initial_inputs_by_filenames_forced(
        //     &mut fuzzer,
        //     &mut executor,
        //     &mut mgr,
        //     &corpus_files,
        // )?;
        // log::info!("Processed {} inputs from disk.", corpus_files.len());

        //////// THE REAL WORK HERE
        // let first_id = state.corpus().first().expect("Empty corpus");
        // state.set_corpus_id(first_id)?;

        // stages.perform_all(&mut fuzzer, &mut executor, &mut state, &mut mgr)?;
        //////// REAL WORK OVER

        // let size = state.corpus().count();
        // println!(
        //     "Removed {} duplicates from {} seeds",
        //     corpus_files.len() - size,
        //     corpus_files.len()
        // );
    
        mgr.send_exiting()?;
        // Err(Error::ShuttingDown)?
        Ok(())
    // };




    // let stdout = if options.verbose {
    //     None
    // } else {
    //     Some("/dev/null")
    // };

    // match Launcher::builder()
    //     .shmem_provider(StdShMemProvider::new().expect("Failed to init shared memory"))
    //     .broker_port(options.port)
    //     .configuration(EventConfig::from_build_id())
    //     .monitor(MultiMonitor::new(|s| println!("{s}")))
    //     .run_client(&mut run_client)
    //     .cores(&options.cores)
    //     .stdout_file(stdout)
    //     .stderr_file(stdout)
    //     .build()
    //     .launch()
    // {
    //     Ok(()) => (),
    //     Err(Error::ShuttingDown) => println!("Run finished successfully."),
    //     Err(err) => panic!("Failed to run launcher: {err:?}"),
    // }
}
