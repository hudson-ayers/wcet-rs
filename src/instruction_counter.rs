use glob::glob;
use once_cell::sync::Lazy;
use regex::Regex;
use std::{path::PathBuf, process::Command};

use haybale::{backend::Backend, Location, State};

pub type Disassem = Vec<String>;

// matches any line that is a machine instruction
static INST: Lazy<Regex> = Lazy::new(|| Regex::new(r"^(\s*)([^@_\s\.])(.*)$").unwrap());
// matches the start of a function
static ANY_FUNC: Lazy<Regex> = Lazy::new(|| Regex::new("^_.+:$").unwrap());
// matches the start of a function or bb
static ANY_BB_OR_FUNC: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(^_.+:$)|(^@\s*%bb\.\d+:.*$)|(^\.LBB.*:$)").unwrap());

/// Find the bc file to be passed to llc
fn find_bc_file(bc_dir: &String, board_name: &String) -> String {
    let pat = format!(r"{}{}*.bc", bc_dir, board_name);
    let paths = glob(&pat)
        .unwrap()
        .map(|x| x.unwrap())
        .collect::<Vec<PathBuf>>();
    if paths.len() != 1 {
        panic!("found multiple dep files");
    }
    paths[0].to_str().unwrap().to_owned()
}

/// Read the output of llc from disc
fn read_llc_output(mut bc_path: String) -> String {
    let len = bc_path.len();
    // change the file extension from ".bc" to ".s"
    bc_path.truncate(len - 2);
    bc_path.push_str("s");
    std::fs::read_to_string(bc_path).expect("could not open llc output")
}

/// Generate annotated disassembly using llc
pub fn get_disassembly(bc_dir: &String, board_name: &String) -> Disassem {
    let bc_path = find_bc_file(bc_dir, board_name);

    let mut llc_command = Command::new("llc-13");
    llc_command.arg(&bc_path);
    llc_command.status().expect("llc process failed to execute");

    let llc_output = read_llc_output(bc_path);
    llc_output.lines().map(|s| s.to_owned()).collect()
}

/// Apply this transformation:
///     %bb_name → %"bb_name"
fn quote_bb_name(bb_name: &String) -> String {
    let mut res = bb_name.to_owned();
    if res.len() < 1 {
        panic!("bb_name too short");
    }
    res.insert(1, '"');
    res.push('"');
    res
}

/// Build the regexes used to find the function and basic block location
fn build_func_and_bb_patterns(location: &Location) -> (Regex, Regex) {
    // matches the start of the desired function
    let func_name = &location.func.name;
    let func_pat = format!(r"^{}:$", regex::escape(func_name));
    let func_re = Regex::new(&func_pat).unwrap();

    // matches the start of the desired bb
    let bb_name = &location.bb.name.to_string();
    let bb_num_pat = Regex::new(r"%(bb)?(\d+)").unwrap();
    let bb_exit_pat = Regex::new(r"%_.*\.exit").unwrap();
    let bb_pat = if bb_name == "%start" {
        // Start of function maps to anything
        r"^.*(@\s*%bb\.0:).*$".to_owned()
    } else if bb_num_pat.is_match(bb_name) {
        let caps = bb_num_pat.captures(bb_name).unwrap();
        let num_str = caps.get(2).unwrap().as_str();
        let num_opt = num_str.parse::<i32>();
        match num_opt {
            Ok(num) => format!(r"^((.*(@\s*%bb\.{}:))|(\.LBB\d+_{}:)).*$", num, num),
            Err(_) => panic!("cannot parse int: {}", num_str),
        }
    } else if bb_exit_pat.is_match(bb_name) {
        format!(r"^.*(@ {}).*$", regex::escape(&quote_bb_name(bb_name)))
    } else {
        panic!("bb name format not recognized: {}", bb_name);
    };
    let bb_re = Regex::new(&bb_pat).unwrap();

    (func_re, bb_re)
}

fn find_outlined_function(
    instr: &str,
    disassembly: &Disassem,
    instr_re: &Regex,
) -> (String, usize) {
    let mut func_name = instr[4..].to_owned();
    func_name.push(':');

    let mut i = 0;

    while i < disassembly.len() && !(disassembly[i] == func_name) {
        i += 1;
    }
    i += 1;

    let mut res = func_name;
    res.push('\n');
    let mut func_len = 0;
    while i < disassembly.len() && !disassembly[i].contains(".Lfunc_end") {
        if instr_re.is_match(&disassembly[i]) {
            res.push_str(&disassembly[i]);
            res.push('\n');
            func_len += 1;
        }
        i += 1;
    }
    res.push_str("OUTLINED_FUNCTION_END\n");

    (res, func_len)
}

/// Given an index i that points to the first line of a function,
/// find the desired basic block within it and append the instructions
/// contained within to res. Return whether the basic block was found
/// and the number of instructions it contains.
fn find_bb_and_count(
    disassembly: &Disassem,
    i: usize,
    bb_re: &Regex,
    res: &mut String,
) -> (bool, usize) {
    let mut current_block_instr_len = 0;
    let mut index = i;

    // skip to the start of the basic block
    while index < disassembly.len() && !bb_re.is_match(&disassembly[index]) {
        if ANY_FUNC.is_match(&disassembly[index]) {
            return (false, 0);
        }
        index += 1;
    }
    index += 1;

    // append every machine instruction encountered
    while index < disassembly.len() && !ANY_BB_OR_FUNC.is_match(&disassembly[index]) {
        if INST.is_match(&disassembly[index]) {
            res.push_str(&disassembly[index]);
            res.push('\n');
            current_block_instr_len += 1;

            if disassembly[index].contains("bl	OUTLINED_FUNCTION") {
                let (outlined_str, outlined_len) =
                    find_outlined_function(&disassembly[index], disassembly, &INST);
                res.push_str(&outlined_str);
                current_block_instr_len += outlined_len;
            }
        }
        index += 1;
    }

    (true, current_block_instr_len)
}

/// Count the number of machine instructions corresponding to the current path
pub fn count_instructions<'p, B: Backend>(
    disassembly: &Disassem,
    state: &State<'p, B>,
) -> Result<(String, usize), String> {
    let mut res = String::new();
    let mut num_instrs = 0;

    for path_entry in state.get_path().iter() {
        let location = &path_entry.0;

        // log meta-information about the current bb
        res.push_str(&format!(
            "module: {} | func: {} | bb: {}\n",
            &location.module.name, &location.func.name, &location.bb.name
        ));

        let (func_re, bb_re) = build_func_and_bb_patterns(location);

        let mut func_found = false;
        let mut bb_found = false;
        let mut current_block_instr_len = 0;
        for (i, line) in disassembly.iter().enumerate() {
            if func_re.is_match(line) {
                func_found = true;

                (bb_found, current_block_instr_len) =
                    find_bb_and_count(disassembly, i + 1, &bb_re, &mut res);

                break;
            }
        }

        num_instrs += current_block_instr_len;
        if !func_found {
            res.push_str("Function not found...\n");
        } else if !bb_found {
            res.push_str("Basic block not found...\n");
        } else if current_block_instr_len == 0 {
            res.push_str("Basic block is empty...\n");
        }
    }

    Ok((res, num_instrs))
}
