use clap::arg_enum;
use glob::glob;
use haybale::backend::*;
use haybale::*;
use simple_logger::SimpleLogger;
use std::collections::HashMap;
use std::fs::File;
use std::io::prelude::*;
use std::process::{Command, Stdio};
use std::result::Result;
use std::string::String;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;
use std::vec::Vec;
use structopt::StructOpt;

extern crate log;

static RETRY_ONGOING: AtomicBool = AtomicBool::new(false);

/// Given a function name and project/configuration, returns the longest path
/// (in llvm IR "instructions") through that function, as well as a copy of the `State` of
/// the execution manager at the conclusion of symbolically executing that path. Ties
/// are broken at random.
// TODO: just reuse find_longest_path in my haybale fork
pub fn find_longest_path<'p>(
    funcname: &str,
    project: &'p Project,
    config: Config<'p, DefaultBackend>,
    time_results: bool,
) -> Result<(usize, State<'p, DefaultBackend>), String> {
    let mut em: ExecutionManager<DefaultBackend> =
        symex_function(funcname, project, config, None).unwrap();
    let mut longest_path_len = 0;
    let mut longest_path_state = None;
    let mut i = 0;
    loop {
        let start = Instant::now();
        match em.next() {
            Some(res) => match res {
                Ok(_) => {
                    if time_results {
                        print!(
                            "Call to next() #{} completed in {} seconds ",
                            i,
                            start.elapsed().as_secs()
                        );
                    }
                }
                Err(Error::UnreachableInstruction) => {
                    // Rust inserts unreachable assertions along paths that it knows will not be
                    // reachable unless we violate Rust's memory/type safety. LLVM IR on its own
                    // does not have enough information to know these paths will never be
                    // reachable, so sometimes haybale will attempt to execute unreachable
                    // instructions. We simply have to ignore all paths containing these
                    // instructions.
                    i += 1;
                    continue;
                }
                Err(Error::SolverError(e)) => {
                    if !e.contains("timed out") {
                        println!("Solver error, not a timeout.");
                        return Err(em
                            .state()
                            .full_error_message_with_context(Error::SolverError(e)));
                    } else {
                        println!("{}", e);
                        if RETRY_ONGOING.load(Ordering::Relaxed) {
                            panic!("Double timeout");
                        }
                    }
                    println!("Solver timeout detected! Attempting to loosen constraints.");
                    // This is usually a timeout, so for now we will assume this is always a
                    // timeout. My approach to solver timeouts is:
                    //
                    // 1. Find the enclosing function of the current location when we timed out
                    //
                    let state = em.mut_state();
                    println!(
                        "Location of timeout: {}",
                        state.cur_loc.to_string_no_module()
                    );
                    let callsite = &state.stack.last().unwrap().callsite;
                    //
                    // 2. Instruct Haybale that the next time we call this function, we are going
                    //    to execute it without any constraints on its inputs -- e.g. we are going
                    //    to assume that all values in memory could be anything, and all passed in
                    //    parameters could be anything.
                    //    TODO: What if this function is called in a loop? This will not push any
                    //    new backtrack points on but will lead to multiple calls..I guess we want
                    //    to keep the function unconstrained until the current backtrack point is
                    //    complete. Configured to panic internally if this happens, lets see if it
                    //    is an issue in practice.
                    //
                    state.fn_to_clear = Some(callsite.clone());
                    println!("fn_to_clear: {:?}", state.fn_to_clear);
                    //
                    // 3. Find where in the failing path this function was last called, and then find
                    //    the backtracking point immediately preceding this call
                    //
                    let restart_point = state.last_backtrack_point.take().unwrap();
                    // Verify that the restart point is not in the same call frame as the point of
                    // failure, if it is backtracking will not help us because the constraints will
                    // be unchanged. TODO improve this to not panic and instead print useful info.
                    assert!(&(restart_point.stack.last().unwrap().callsite) != callsite);
                    println!("restart point: {:?}", restart_point.loc);
                    state.solver.push(1); // Solver level needs to be in sync with backtrack queue
                    state.backtrack_points.borrow_mut().push(restart_point);
                    RETRY_ONGOING.store(true, Ordering::Relaxed);
                    // now next call to next() should resume from restart_point!
                    continue;
                }
                Err(e) => {
                    println!(
                        "Call to next() # {} failed after {} seconds",
                        i,
                        start.elapsed().as_secs()
                    );
                    println!(
                        "Failed while executing instruction in {}",
                        em.state().cur_loc.func.name
                    );
                    println!("Pretty path source: {}", em.state().pretty_path_source());
                    return Err(em.state().full_error_message_with_context(e));
                }
            },
            None => break,
        }
        i += 1;
        let state = em.state();
        let path = state.get_path();
        let len = haybale::state::get_path_length(path);
        if len > longest_path_len {
            longest_path_len = len;
            longest_path_state = Some(state.clone());
        }
    }
    longest_path_state.map_or(Err("No Paths found".to_string()), |state| {
        Ok((longest_path_len, state))
    })
}

arg_enum! {
    #[derive(Debug)]
    enum KernelWorkType {
        Interrupts,
        Commands,
        Subscribes,
        Allows,
        Memops,
        All,
    }
}

/// Function for retrieving the types of Tock functions which this tool is capable of profiling,
/// by matching on the mangled function names.
fn retrieve_functions_for_analysis<'p>(
    project: &'p Project,
    kind: KernelWorkType,
) -> Box<dyn Iterator<Item = (&llvm_ir::function::Function, &llvm_ir::module::Module)> + 'p> {
    // TODO: Filtering on demangled function names should allow for more precise matches with fewer
    // false positives
    //let demangled = rustc_demangle::demangle(func_name);
    match kind {
        // TODO: Allow handle_xx_interrupt pattern as well
        KernelWorkType::Interrupts => Box::new(
            project
                .all_functions()
                .filter(|(f, _m)| f.name.contains("handle_interrupt")),
        ),
        KernelWorkType::Commands => Box::new(project.all_functions().filter(|(f, _m)| {
            f.name.contains("command")
                && f.name.contains("Driver")
                && !f.name.contains("closure") //manual exclusion
                && !f.name.contains("command_complete") //manual exclusion
        })),
        KernelWorkType::Allows => Box::new(project.all_functions().filter(|(f, _m)| {
            f.name.contains("allow") && f.name.contains("Driver") && !f.name.contains("closure")
        })),
        KernelWorkType::Subscribes => Box::new(project.all_functions().filter(|(f, _m)| {
            f.name.contains("subscribe") && f.name.contains("Driver") && !f.name.contains("closure")
        })),
        KernelWorkType::Memops => panic!("Memop support not yet implemented"),
        KernelWorkType::All => {
            let command_syscalls =
                retrieve_functions_for_analysis(&project, KernelWorkType::Commands);

            let subscribe_syscalls =
                retrieve_functions_for_analysis(&project, KernelWorkType::Subscribes);
            let allow_syscalls = retrieve_functions_for_analysis(&project, KernelWorkType::Allows);

            let interrupt_handlers =
                retrieve_functions_for_analysis(&project, KernelWorkType::Interrupts);
            Box::new(
                command_syscalls
                    .chain(subscribe_syscalls)
                    .chain(allow_syscalls)
                    .chain(interrupt_handlers),
            )
        }
    }
}

/// Given a bc directory and a function name to analyze, this function
/// will symbolically execute the passed function, and write the results to a file.
/// This is useful for performing multiple symbolic executions simultaneously,
/// especially because each execution is single threaded.
fn analyze_and_save_results(
    bc_dir: &str,
    board_path_str: &str,
    func_name: &str,
    timeout_s: u64,
    resultspath: &str,
    time_results: bool,
) -> Result<String, String> {
    let paths = glob(&[bc_dir, "/**/*.bc"].concat())
        .unwrap()
        .map(|x| x.unwrap());
    let project = Project::from_bc_paths(paths)?;

    let mut config: Config<DefaultBackend> = Config::default();
    config.null_pointer_checking = config::NullPointerChecking::None; // In the Tock kernel, we trust that Rust safety mechanisms prevent null pointer dereferences.
    config.loop_bound = 50; // default is 10, raise if larger loops exist
    config.solver_query_timeout = Some(std::time::Duration::new(timeout_s, 0)); // extend query timeout
    config
        .function_hooks
        .add_rust_demangled("kernel::debug::panic", &function_hooks::abort_hook);
    config
        .function_hooks
        .add_rust_demangled("core::panicking::panic_fmt", &function_hooks::abort_hook);
    config.longest_path_optimizations = true;
    let board_name = board_path_str
        .get(board_path_str.rfind('/').unwrap() + 1..)
        .unwrap();
    let demangled = rustc_demangle::demangle(func_name).to_string();
    let filename = format!("{}/{}/{}.txt", resultspath, board_name, demangled);
    println!("{:?}", filename);
    let path = std::path::Path::new(&filename);
    let prefix = path.parent().unwrap();
    std::fs::create_dir_all(prefix).unwrap();
    let mut file = File::create(path).unwrap();
    let ret = match find_longest_path(func_name, &project, config, time_results) {
        Ok((len, state)) => {
            println!("len: {}", len);
            let data = "len: ".to_owned()
                + &len.to_string()
                + "\n"
                + &state.pretty_path_llvm_instructions();
            // + "\n"
            //+ &state.pretty_path_source();
            file.write_all(data.as_bytes()).unwrap();
            Ok(len.to_string())
        }
        Err(e) => {
            println!("{}", e);
            file.write_all(e.as_bytes()).unwrap();
            Err("Fail: ".to_string() + &e)
        }
    };
    ret
}

#[derive(StructOpt, Debug)]
#[structopt(name = "basic")]
struct Opt {
    /// Activate debug mode
    #[structopt(short, long)]
    debug: bool,

    /// Pass this to skip recompiling the binary in the tock submodule
    #[structopt(long)]
    skip_compile: bool,

    /// Verbose mode (-v, -vv, -vvv, etc.)
    #[structopt(short, long, parse(from_occurrences))]
    verbose: u8,

    /// Timeout passed to Haybale runs (in seconds)
    /// This is only the timeout for the initial runs,
    /// not the partitioned runs
    #[structopt(short, long, default_value = "25")]
    timeout: u64,

    /// Name of the tock board to analyze
    #[structopt(short, long, default_value = "imixmini")]
    board: String,

    /// Index of function, to run a specific function within
    /// the function list
    #[structopt(short = "i", long, default_value = "0")]
    function_index: usize,

    /// Pass components of a function name to run
    /// only on a specific function containing all those components.
    /// Use this argument multiple times to include multiple components,
    /// e.g. '-c ble -c fired' to run on the first matched function containing
    /// both "ble" and "fired"
    /// Not compatible with function_index
    #[structopt(short = "c", long)]
    func_name_contains: Option<Vec<String>>,

    /// Types of function for which to find longest path
    #[structopt(short, long, possible_values = &KernelWorkType::variants(), case_insensitive = true, default_value = "all")]
    functions: KernelWorkType,

    #[structopt(short = "p", long = "tockpath", default_value = "tock")]
    tockpath: String,

    #[structopt(short = "r", long = "resultspath", default_value = "results")]
    resultspath: String,

    #[structopt(short = "g", long)]
    save_git_history: bool,

    #[structopt(long = "time")]
    time_results: bool,
}

fn main() -> Result<(), String> {
    let opt = Opt::from_args(); // get CLI inputs

    if opt.verbose >= 1 {
        // Enable logs in Haybale. Useful for debugging
        // but dramatically slow down executions and increase memory use.
        // generally, should be first line of main if included.
        SimpleLogger::new().init().unwrap();
    }

    // set to board to be evaluated. Currently, not all tock boards are supported.
    // This works because this crate uses the same rust toolchain as Tock.
    let board_path_str = opt.tockpath.to_owned() + "/boards/" + &opt.board.to_owned();
    if !opt.skip_compile {
        println!("Compiling {:?}, please wait...", board_path_str);

        assert!(Command::new("make")
            .arg("-C")
            .arg(&board_path_str)
            .arg("clean")
            .output()
            .expect("failed to execute make clean")
            .status
            .success());
        let output = Command::new("make")
            .arg("-C")
            .arg(&board_path_str)
            .output()
            .expect("failed to execute make");
        assert!(output.status.success());
        let str_output = String::from_utf8(output.stderr).unwrap();
        if !str_output.contains("Finished release") {
            panic!("Build failed, output: {}", str_output);
        }
        println!("Finished building");
    }
    println!("starting");

    if opt.save_git_history {
        // Save current program state into a file, for reproducability later
        // Uses git commands for this
        let git_diff_filename = (&opt.resultspath).to_owned() + "/git_diff.txt";
        let git_diff_file = File::create(git_diff_filename).unwrap();
        assert!(Command::new("git")
            .current_dir(opt.tockpath.to_owned())
            .arg("diff")
            .stdout(git_diff_file)
            .status()
            .expect("Failed to execute git diff")
            .success());

        // Do same thing for git log
        let git_log_filename = (&opt.resultspath).to_owned() + "/git_log.txt";
        let git_log_file = File::create(git_log_filename).unwrap();

        let git_log_out = Command::new("git")
            .current_dir(opt.tockpath.to_owned())
            .arg("log")
            .stdout(Stdio::piped())
            .spawn()
            .expect("Failed to execute git log")
            .stdout
            .expect("Failed to open log stdout");

        assert!(Command::new("head")
            .stdin(Stdio::from(git_log_out))
            .stdout(git_log_file)
            .arg("-n")
            .arg("60")
            .status()
            .expect("Failed to execute head")
            .success());
    }

    // For now, assume target under analysis, located in the tock submodule of this crate.
    // Assume it is a thumbv7 target unless it is one of three whitelisted riscv targets.
    let bc_dir: String = opt.tockpath.clone()
        + if board_path_str.contains("opentitan")
            || board_path_str.contains("arty_e21")
            || board_path_str.contains("hifive1")
        {
            "/target/riscv32imc-unknown-none-elf/release/deps/"
        } else {
            "/target/thumbv7em-none-eabi/release/deps/"
        };

    let paths = glob(&[&bc_dir, "/**/*.bc"].concat())
        .unwrap()
        .map(|x| x.unwrap());
    println!("globbed");
    let project = Project::from_bc_paths(paths)?;
    println!("Project loaded");

    let mut functions_to_analyze = vec![];
    let mut func_name_iter = retrieve_functions_for_analysis(&project, opt.functions);
    if opt.func_name_contains.is_some() {
        let vec = opt.func_name_contains.unwrap().clone();
        println!("func_name_contains: {:?}", vec);
        let func_name = &project
            .all_functions()
            .filter(|(f, _m)| {
                let demangled = rustc_demangle::demangle(&f.name);

                let mut matched = true;
                for s in vec.iter() {
                    if !demangled.to_string().contains(s) {
                        matched = false;
                    }
                    // mangled name match always indicates to include this
                    if f.name.trim() == s.trim() {
                        matched = true;
                        break;
                    }
                }
                matched
            })
            .next()
            .unwrap()
            .0
            .name;
        println!("Profiling {:?}", func_name);
        functions_to_analyze.push(func_name);
    } else if opt.function_index == 0 {
        functions_to_analyze.extend(func_name_iter.map(|(f, _m)| &f.name));
    } else {
        functions_to_analyze.push(&func_name_iter.nth(opt.function_index - 1).unwrap().0.name);
    }

    let mut children = vec![];
    let all_results = Mutex::new(HashMap::new());
    let arc = Arc::new(all_results);
    let timeout = opt.timeout;
    let start = Instant::now();
    for f in functions_to_analyze {
        let f = f.clone();
        let arc = arc.clone();
        let name = board_path_str.clone();
        let bc_dir_cpy = bc_dir.clone();
        let resultspath = opt.resultspath.clone();
        let time_results = opt.time_results;
        children.push(thread::spawn(move || {
            match analyze_and_save_results(
                &bc_dir_cpy,
                &name,
                &f,
                timeout,
                &resultspath,
                time_results,
            ) {
                Ok(s) => {
                    arc.lock().map_or((), |mut map| {
                        map.insert(f, s);
                    });
                }
                Err(e) => {
                    arc.lock().map_or((), |mut map| {
                        map.insert(f, e);
                    });
                }
            }
        }));
    }

    let end = Instant::now();
    for child in children {
        let _ = child.join();
    }
    // Now, result of each thread is in all_results.
    let filename = (&opt.resultspath).to_owned() + "/" + &opt.board + "/summary.txt";
    println!("{:?}", filename);
    let mut file = File::create(filename).unwrap();

    let mut data = String::new();
    let data = arc
        .lock()
        .map(|map| {
            for (k, v) in map.iter() {
                data = data + k + ": " + v + "\n";
            }
            data
        })
        .unwrap();
    file.write_all(data.as_bytes()).unwrap();

    if opt.time_results {
        // Write how long the entire operation took
        // This might go at board level instead, not sure
        let time_filename = (&opt.resultspath).to_owned() + "/time.txt";
        let mut time_file = File::create(time_filename).unwrap();
        let total_duration = end.duration_since(start);
        let duration_str = format!("Elapsed: {:?}", total_duration);
        time_file.write_all(duration_str.as_bytes()).unwrap();
    }

    Ok(())
}
