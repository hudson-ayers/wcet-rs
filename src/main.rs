use glob::glob;
use haybale::backend::*;
use haybale::*;
use llvm_ir::{Module, Name};
use std::result::Result;
use std::string::String;

#[macro_use]
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
) -> Option<(usize, State<'p, DefaultBackend>)> {
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
                    panic!(em.state().full_error_message_with_context(e));
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
    longest_path_state.map_or(None, |state| Some((longest_path_len, state)))
}

/// Return the longest possible path for a given method call on a trait object.
pub fn longest_path_dyn_dispatch(
    bc_dir: &str,
    method_name: &str,
    trait_name: &str,
) -> Result<(usize), String> {
    // TODO: Lots of repeated code here
    let glob2 = "/**/*.bc";
    let paths = glob(&[bc_dir, glob2].concat()).unwrap().map(|x| x.unwrap());
    let project = Project::from_bc_paths(paths)?;
    let matches = project
        .all_functions()
        .filter(|(f, _m)| f.name.contains(method_name) && f.name.contains(trait_name));
    let mut longest = 0;
    let mut longest_path = None;
    for (f, _m) in matches {
        let mut config: Config<DefaultBackend> = Config::default();
        config.null_pointer_checking = config::NullPointerChecking::None; // In the Tock kernel, we trust that Rust safety mechanisms prevent null pointer dereferences.
        config.loop_bound = 100; // default is 10, go higher to detect unbounded loops
        println!("tracing {:?}", f.name);
        if let Some((len, state)) = find_longest_path(&f.name, &project, config) {
            if len > longest {
                longest = len;
                longest_path = Some(state.get_path());
                println!("new longest: {:?}", f.name);
            }
        }
    }
    Ok(longest)
}

fn main() -> Result<(), String> {
    //simple_logger::init().unwrap();

    //let bc_dir = "/home/hudson/research/real_time/test_workspace/target/debug/deps";
    let bc_dir = "/home/hudson/tock/target/thumbv7em-none-eabi/release/deps/";
    //let func_name = "haybale_test::STimer::dyn_dispatch";
    //let func_name = "dyn_dispatch";
    //let func_name = "apollo3::stimer::STimer::handle_interrupt";
    //let func_name = "apollo3::iom::Iom::handle_interrupt";
    //let func_name = "apollo3::uart::Uart::handle_interrupt";
    //let func_name = "apollo3::ble::Ble::handle_interrupt";
    //let func_name = "capsules::led::Led::Driver::command";
    //let func_name = "_ZN109_$LT$capsules..ble_advertising_driver..BLE$LT$B$C$A$GT$$u20$as$u20$kernel..hil..ble_advertising..RxClient$GT$13receive_event17hf1075a8e774afbddE";
    //let func_name = "apollo3::gpio::Port::handle_interrupt";
    //let tmp = longest_path_dyn_dispatch(bc_dir, "AlarmClient", "fired");
    //println!("Longest trait path: {:?}", tmp);
    let glob2 = "/**/*.bc";
    let paths = glob(&[bc_dir, glob2].concat()).unwrap().map(|x| x.unwrap());
    let project = Project::from_bc_paths(paths)?;

    // TODO: Replace below hack with reliable demangling approach
    let mut matches = project.all_functions().filter(|(f, _m)| {
        f.name.contains("command")
            && f.name.contains("Driver")
            && !f.name.contains("closure")
            && !f.name.contains("command_complete")
    });
    let func_name = &matches.next().unwrap().0.name.clone(); //led

    let func_name = &matches.next().unwrap().0.name.clone(); //gpio

    let func_name = &matches.next().unwrap().0.name.clone(); // alarm, fails

    let func_name = &matches.next().unwrap().0.name.clone(); // i2c, fails bc panic

    //let func_name = &matches.next().unwrap().0.name.clone(); //ble, fails

    //let func_name = &matches.next().unwrap().0.name.clone(); //console, fails

    // thats all drivers with commands

    /*let mut matches = project.all_functions().filter(|(f, _m)| {
        f.name.contains("subscribe") && f.name.contains("Driver") && !f.name.contains("closure")
    });
    let func_name = &matches.next().unwrap().0.name.clone(); //dummy impl, 4
    let func_name = &matches.next().unwrap().0.name.clone(); //gpio
    let func_name = &matches.next().unwrap().0.name.clone(); //alarm
    let func_name = &matches.next().unwrap().0.name.clone(); //i2c
    let func_name = &matches.next().unwrap().0.name.clone(); //ble
    let func_name = &matches.next().unwrap().0.name.clone(); //console
    */

    //thats all drivers with subscribe

    /*let mut matches = project.all_functions().filter(|(f, _m)| {
        f.name.contains("allow") && f.name.contains("Driver") && !f.name.contains("closure")
    });
    let func_name = &matches.next().unwrap().0.name.clone(); //default
    let func_name = &matches.next().unwrap().0.name.clone(); //also default??
    let func_name = &matches.next().unwrap().0.name.clone(); //also default??
    let func_name = &matches.next().unwrap().0.name.clone(); //i2c
    let func_name = &matches.next().unwrap().0.name.clone(); //ble, fails
    let func_name = &matches.next().unwrap().0.name.clone(); //console, fails
    */

    //thats all drivers with allow

    let demangled = rustc_demangle::demangle(func_name);
    println!("demangled: {:?}", demangled);
    let mut config: Config<DefaultBackend> = Config::default();
    config.null_pointer_checking = config::NullPointerChecking::None; // In the Tock kernel, we trust that Rust safety mechanisms prevent null pointer dereferences.
    config.loop_bound = 50; // default is 10, raise if larger loops exist
    config.solver_query_timeout = Some(std::time::Duration::new(10000, 0)); // extend query timeout
    config
        .function_hooks
        .add_rust_demangled("kernel::debug::panic", &function_hooks::abort_hook);
    if let Some((len, state)) = find_longest_path(func_name, &project, config) {
        println!("len: {}", len);
    //println!("{}", state.pretty_path_interleaved());
    //print_instrs(state.get_path());

    /*let a = &state
        .get_a_solution_for_irname(&String::from(func_name), &Name::from(1))?
        .expect("Expected there to be a solution")
        .as_u64()
        .expect("Expected solution to fit in 64 bits");
    println!("Input to two_paths that gives this path: {}", a);*/
    } else {
        panic!("No paths found");
    }
    Ok(())
}
