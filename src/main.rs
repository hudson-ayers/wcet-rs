use clap::arg_enum;
use glob::glob;
use haybale::backend::*;
use haybale::*;
use simple_logger::SimpleLogger;
use std::collections::HashMap;
use std::fs::File;
use std::io::prelude::*;
use std::result::Result;
use std::string::String;
use std::sync::{Arc, Mutex};
use std::thread;
use std::vec::Vec;
use structopt::StructOpt;

extern crate log;

/// Print all LLVM IR instructions in a given symbolic execution
pub fn print_instrs<'p>(path: &Vec<PathEntry<'p>>) -> String {
    let mut ret = String::new();
    for entry in path {
        let location = &entry.0;
        // TODO: Below assumes terminator is not an instruction, not totally clear on how this
        // works though.
        match location.instr {
            BBInstrIndex::Instr(idx) => {
                for instr in location.bb.instrs.iter().skip(idx) {
                    ret += &format!("{:?}\n", instr);
                }
            }
            BBInstrIndex::Terminator => println!("Terminator."),
        }
    }
    ret
}

/// Returns the number of LLVM instructions in this path.
/// A path is represented as a vector of `PathEntry`s, and
/// each PathEntry describes a sequential set of instructions in a basic block,
/// not necessarily starting at the beginning of that basic block.
/// Thus we have to investigate each path entry to count the number of instructions
/// described by it.
/// However, function calls complicate this: if function calls are not inlined, then the entire
/// function is counted as a single instruction!
pub fn get_path_length<'p>(path: &Vec<PathEntry<'p>>) -> usize {
    path.iter().fold(0, |acc, entry| {
        let location = &entry.0;
        // TODO: Below assumes terminator is not an instruction, not totally clear on how this
        // works though.
        let entry_len = match location.instr {
            BBInstrIndex::Instr(idx) => location.bb.instrs.len() - idx,
            BBInstrIndex::Terminator => 0,
        };
        acc + entry_len
    })
}

/// Given a function name and project/configuration, returns the longest path
/// (in llvm IR "instructions") through that function, as well as a copy of the `State` of
/// the execution manager at the conclusion of symbolically executing that path. Ties
/// are broken at random.
pub fn find_longest_path<'p>(
    funcname: &str,
    project: &'p Project,
    config: Config<'p, DefaultBackend>,
) -> Result<(usize, State<'p, DefaultBackend>), String> {
    let mut em: ExecutionManager<DefaultBackend> =
        symex_function(funcname, project, config, None).unwrap();
    //TODO: Following code could probably be more functional
    let mut longest_path_len = 0;
    let mut longest_path_state = None;
    loop {
        match em.next() {
            Some(res) => match res {
                Ok(_) => {
                    println!("next() worked");
                }
                Err(e) => {
                    return Err(em.state().full_error_message_with_context(e));
                }
            },
            None => break,
        }
        let state = em.state();
        let path = state.get_path();
        let len = get_path_length(path);
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

            //functions_to_analyze.extend(allow_syscalls.map(|(f, _m)| &f.name));
            //functions_to_analyze.extend(command_syscalls.map(|(f, _m)| &f.name));
            //functions_to_analyze.extend(subscribe_syscalls.map(|(f, _m)| &f.name));
            //functions_to_analyze.extend(interrupt_handlers.map(|(f, _m)| &f.name));
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
    let board_name = board_path_str
        .get(board_path_str.rfind('/').unwrap() + 1..)
        .unwrap();
    let demangled = rustc_demangle::demangle(func_name).to_string();
    let filename = "results/".to_owned() + board_name + "/" + &demangled + ".txt";
    println!("{:?}", filename);
    let path = std::path::Path::new(&filename);
    let prefix = path.parent().unwrap();
    std::fs::create_dir_all(prefix).unwrap();
    let mut file = File::create(path).unwrap();
    let ret = match find_longest_path(func_name, &project, config) {
        Ok((len, state)) => {
            println!("len: {}", len);
            let data = "len: ".to_owned()
                + &len.to_string()
                + ""
                + &state.pretty_path_llvm()
                + "\n"
                + &state.pretty_path_source()
                + "\n"
                + &print_instrs(state.get_path());
            file.write_all(data.as_bytes()).unwrap();
            //println!("{}", state.pretty_path_source());
            //print_instrs(state.get_path());
            Ok(len.to_string())
        }
        Err(e) => {
            file.write_all(e.as_bytes()).unwrap();
            Err("Fail".to_string())
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
    #[structopt(short, long, default_value = "100")]
    timeout: u64,

    /// Name of the tock board to analyze
    #[structopt(short, long, default_value = "redboard_artemis_nano")]
    board: String,

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
    #[structopt(possible_values = &KernelWorkType::variants(), case_insensitive = true, default_value = "all")]
    functions: KernelWorkType,
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
    let board_path_str = "tock/boards/".to_string() + &opt.board;
    if !opt.skip_compile {
        println!("Compiling {:?}, please wait...", board_path_str);

        use std::process::Command;
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
    }

    // For now, assume target under analysis, located in the tock submodule of this crate.
    // Assume it is a thumbv7 target unless it is one of three whitelisted riscv targets.

    let bc_dir = if board_path_str.contains("opentitan")
        || board_path_str.contains("arty_e21")
        || board_path_str.contains("hifive1")
    {
        "tock/target/riscv32imc-unknown-none-elf/release/deps/"
    } else {
        "tock/target/thumbv7em-none-eabi/release/deps/"
    };

    let paths = glob(&[bc_dir, "/**/*.bc"].concat())
        .unwrap()
        .map(|x| x.unwrap());
    let project = Project::from_bc_paths(paths)?;

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
    for f in functions_to_analyze {
        let f = f.clone();
        let arc = arc.clone();
        let name = board_path_str.clone();
        children.push(thread::spawn(move || {
            match analyze_and_save_results(bc_dir, &name, &f, timeout) {
                Ok(s) => {
                    arc.lock().map_or((), |mut map| {
                        map.insert(f, s);
                    });
                }
                _ => {}
            }
        }));
    }
    for child in children {
        let _ = child.join();
    }
    // Now, result of each thread is in all_results.
    let filename = "results/".to_owned() + &opt.board + "/summary.txt";
    println!("{:?}", filename);
    let mut file = File::create(filename).unwrap();

    let mut data = String::new();
    let data = arc
        .lock()
        .map(|map| {
            for (k, v) in map.iter() {
                data = data + k + ": " + v;
            }
            data
        })
        .unwrap();
    file.write_all(data.as_bytes()).unwrap();

    Ok(())
}
