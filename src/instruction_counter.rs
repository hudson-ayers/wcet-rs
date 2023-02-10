use glob::glob;
use regex::Regex;
use std::{path::PathBuf, process::Command};

use haybale::{backend::Backend, Location, State};

pub type Disassem = Vec<String>;

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

/// Build the regexes used to find the function and basic block location
fn build_func_and_bb_patterns(location: &Location) -> (Regex, Regex) {
    // matches the start of the desired function
    let func_name = &location.func.name;
    let func_pat = format!(r"^{}:$", func_name);
    let func_re = Regex::new(&func_pat).unwrap();

    // matches the start of the desired bb
    let bb_name = &location.bb.name.to_string();
    let bb_num_pat = Regex::new(r"%(bb)?(\d+)").unwrap();
    let bb_exit_pat = Regex::new(r"_.*\.exit").unwrap();
    let bb_pat = if bb_name == "%start" {
        r"^.*(@\s*%bb\.0:).*$".to_owned()
    } else if bb_num_pat.is_match(bb_name) {
        let caps = bb_num_pat.captures(bb_name).unwrap();
        let num_str = caps.get(2).unwrap().as_str();
        let num_opt = num_str.parse::<i32>();
        match num_opt {
            Ok(num) => format!(r"^.*(@\s*%bb\.{}:).*$", num),
            Err(_) => panic!("cannot parse int: {}", num_str),
        }
    } else if bb_exit_pat.is_match(bb_name) {
        format!(r"^.*(@ {}).*$", bb_name)
    } else {
        panic!("bb name format not recognized: {}", bb_name);
    };
    let bb_re = Regex::new(&bb_pat).unwrap();

    (func_re, bb_re)
}

/// Count the number of machine instructions corresponding to the current path
pub fn count_instructions<'p, B: Backend>(
    disassembly: &Disassem,
    state: &State<'p, B>,
) -> Result<(String, usize), String> {
    // matches any line that is a machine instruction
    let instr_re = Regex::new(r"^(\s*)([^@_\s\.])(.*)$").unwrap();
    // matches the start of a function or bb
    let bb_or_func_re = Regex::new(r"(^_.+:$)|(^@\s*%bb\.\d+:.*$)").unwrap();

    let mut res = String::new();
    let mut num_instrs = 0;

    // TODO: can control flow leave a bb in the middle?
    for path_entry in state.get_path().iter() {
        let location = &path_entry.0;

        // log meta-information about the current bb
        res.push_str(&format!(
            "module: {} | func: {} | bb: {}\n",
            &location.module.name, &location.func.name, &location.bb.name
        ));

        let (func_re, bb_re) = build_func_and_bb_patterns(location);

        let mut func_found = false;
        let mut current_block_instr_len = 0;
        for (i, line) in disassembly.iter().enumerate() {
            if func_re.is_match(line) {
                func_found = true;

                // skip to the start of the basic block
                let mut index = i;
                while index < disassembly.len() && !bb_re.is_match(&disassembly[index]) {
                    index += 1;
                }
                index += 1;

                // append every machine instruction encountered
                // TODO: account for OUTLINED_FUNCTIONs and other potential hazards
                while index < disassembly.len() && !bb_or_func_re.is_match(&disassembly[index]) {
                    if instr_re.is_match(&disassembly[index]) {
                        res.push_str(&disassembly[index]);
                        res.push('\n');
                        current_block_instr_len += 1;
                    }
                    index += 1;
                }

                break;
            }
        }

        num_instrs += current_block_instr_len;
        if !func_found {
            res.push_str("Did not find the function...\n");
        } else if current_block_instr_len == 0 {
            res.push_str("Basic block is empty or not found...\n");
        }
    }

    Ok((res, num_instrs))
}
