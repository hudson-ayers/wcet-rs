use glob::glob;
use haybale::backend::*;
use haybale::*;
use std::fs::File;
use std::io::prelude::*;
use std::result::Result;
use std::string::String;
use std::thread;

extern crate log;
extern crate simple_logger;

/// Print all LLVM IR instructions in a given symbolic execution
pub fn print_instrs<'p>(path: &Vec<PathEntry<'p>>) {
    for entry in path {
        let location = &entry.0;
        // TODO: Below assumes terminator is not an instruction, not totally clear on how this
        // works though.
        match location.instr {
            BBInstrIndex::Instr(idx) => {
                for instr in location.bb.instrs.iter().skip(idx) {
                    println!("instruction: {:?}", instr);
                }
            }
            BBInstrIndex::Terminator => println!("Terminator."),
        }
    }
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
    let mut em: ExecutionManager<DefaultBackend> = symex_function(funcname, project, config);
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

enum KernelWorkType {
    InterruptHandlers,
    CommandSyscalls,
    SubscribeSyscalls,
    AllowSyscalls,
    MemopSyscall,
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
        KernelWorkType::InterruptHandlers => Box::new(
            project
                .all_functions()
                .filter(|(f, _m)| f.name.contains("handle_interrupt")),
        ),
        KernelWorkType::CommandSyscalls => Box::new(project.all_functions().filter(|(f, _m)| {
            f.name.contains("command")
                && f.name.contains("Driver")
                && !f.name.contains("closure")
                && !f.name.contains("command_complete")
        })),
        KernelWorkType::AllowSyscalls => Box::new(project.all_functions().filter(|(f, _m)| {
            f.name.contains("allow") && f.name.contains("Driver") && !f.name.contains("closure")
        })),
        KernelWorkType::SubscribeSyscalls => Box::new(project.all_functions().filter(|(f, _m)| {
            f.name.contains("subscribe") && f.name.contains("Driver") && !f.name.contains("closure")
        })),
        KernelWorkType::MemopSyscall => panic!("Memop support not yet implemented"),
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
) -> Result<(), String> {
    let glob2 = "/**/*.bc";
    let paths = glob(&[bc_dir, glob2].concat()).unwrap().map(|x| x.unwrap());
    let project = Project::from_bc_paths(paths)?;

    let mut config: Config<DefaultBackend> = Config::default();
    config.null_pointer_checking = config::NullPointerChecking::None; // In the Tock kernel, we trust that Rust safety mechanisms prevent null pointer dereferences.
    config.loop_bound = 50; // default is 10, raise if larger loops exist
    config.solver_query_timeout = Some(std::time::Duration::new(100, 0)); // extend query timeout
    config
        .function_hooks
        .add_rust_demangled("kernel::debug::panic", &function_hooks::abort_hook);
    let board_name = board_path_str
        .get(board_path_str.rfind('/').unwrap() + 1..)
        .unwrap();
    let demangled = rustc_demangle::demangle(func_name).to_string();
    let filename = "results_".to_owned() + board_name + "_" + &demangled + ".txt";
    println!("{:?}", filename);
    let mut file = File::create(filename).unwrap();
    match find_longest_path(func_name, &project, config) {
        Ok((len, state)) => {
            println!("len: {}", len);
            let data = "len: ".to_owned() + &len.to_string() + "";
            file.write_all(data.as_bytes()).unwrap();
            //println!("{}", state.pretty_path_source());
            //print_instrs(state.get_path());
        }
        Err(e) => {
            file.write_all(e.as_bytes()).unwrap();
        }
    }
    Ok(())
}

fn main() -> Result<(), String> {
    // comment in to enable logs in Haybale. Useful for debugging
    // but dramatically slow down executions and increase memory use.
    // generally, should be first line of main if included.
    //simple_logger::init().unwrap();

    // set to board to be evaluated. Currently, not all tock boards are supported.
    // TODO: Fix below to not use rust version of haybale crate (may need build.rs)
    let board_path_str = "tock/boards/redboard_artemis_nano";
    /*use std::process::Command;
    let output1 = Command::new("sh")
        .arg("-c")
        .arg(
            "exec bash -l cd ".to_owned()
                + &board_path_str
                + &" && source ~/.bashrc && make clean && make",
        )
        .output()
        .expect("failed to execute process");
    println!("{}", String::from_utf8(output1.stderr).unwrap());

    let output2 = Command::new("make")
        .arg("-C")
        .arg(&board_path_str)
        .output()
        .expect("failed to execute process");
    println!("{}", String::from_utf8(output2.stderr).unwrap());
    */

    // For now, assume target under analysis is thumbv7em architecture,
    // and located in the tock submodule of this crate
    let bc_dir = "tock/target/thumbv7em-none-eabi/release/deps/";

    // Make a vector to hold the children which are spawned.
    let mut functions_to_analyze = vec![];

    //begin list of all interrupts for apollo3 board/chip:

    //let func_name = "apollo3::stimer::STimer::handle_interrupt";
    //let func_name = "apollo3::iom::Iom::handle_interrupt";
    //let func_name = "apollo3::uart::Uart::handle_interrupt";
    //let func_name = "apollo3::ble::Ble::handle_interrupt";
    //let func_name = "apollo3::gpio::Port::handle_interrupt";

    let glob2 = "/**/*.bc";
    let paths = glob(&[bc_dir, glob2].concat()).unwrap().map(|x| x.unwrap());
    let project = Project::from_bc_paths(paths)?;

    let mut command_syscalls =
        retrieve_functions_for_analysis(&project, KernelWorkType::CommandSyscalls);
    let func_name = &command_syscalls.nth(0).unwrap().0.name.clone(); //led

    //let func_name = &command_syscalls.nth(1).unwrap().0.name.clone(); //gpio
    //let func_name = &command_syscalls.nth(2).unwrap().0.name.clone(); // alarm, fails
    //let func_name = &command_syscalls.nth(3).unwrap().0.name.clone(); // i2c, fails bc panic
    //let func_name = &command_syscalls.nth(4).unwrap().0.name.clone(); //ble, fails
    //let func_name = &command_syscalls.nth(5).unwrap().0.name.clone(); //console, fails

    let mut subscribe_syscalls =
        retrieve_functions_for_analysis(&project, KernelWorkType::SubscribeSyscalls);
    // comments at end indicate which function this corresponds to on the apollo3
    //let func_name = &subscribe_syscalls.nth(0).unwrap().0.name.clone(); //dummy impl, 4
    //let func_name = &subscribe_syscalls.nth(1).unwrap().0.name.clone(); //gpio
    //let func_name = &subscribe_syscalls.nth(2).unwrap().0.name.clone(); //alarm
    //let func_name = &subscribe_syscalls.nth(3).unwrap().0.name.clone(); //i2c
    //let func_name = &subscribe_syscalls.nth(4).unwrap().0.name.clone(); //ble
    //let func_name = &subscribe_syscalls.nth(5).unwrap().0.name.clone(); //console

    let mut allow_syscalls =
        retrieve_functions_for_analysis(&project, KernelWorkType::AllowSyscalls);

    //let func_name = &allow_syscalls.nth(0).unwrap().0.name.clone(); //default
    //let func_name = &allow_syscalls.nth(1).unwrap().0.name.clone(); //also default??
    //let func_name = &allow_syscalls.nth(2).unwrap().0.name.clone(); //also default??
    //let func_name = &allow_syscalls.nth(3).unwrap().0.name.clone(); //i2c
    //let func_name = &allow_syscalls.nth(4).unwrap().0.name.clone(); //ble, fails
    //let func_name = &allow_syscalls.nth(5).unwrap().0.name.clone(); //console, fails

    functions_to_analyze.push(func_name);
    functions_to_analyze.extend(allow_syscalls.map(|(f, _m)| &f.name));
    functions_to_analyze.extend(command_syscalls.map(|(f, _m)| &f.name));
    functions_to_analyze.extend(subscribe_syscalls.map(|(f, _m)| &f.name));
    let mut children = vec![];
    for f in functions_to_analyze {
        let f = f.clone();
        children.push(thread::spawn(move || {
            analyze_and_save_results(bc_dir, board_path_str, &f)
        }));
    }
    for child in children {
        // Wait for the thread to finish. Returns a result.
        let _ = child.join();
    }
    Ok(())
}
