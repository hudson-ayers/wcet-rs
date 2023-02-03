use glob::glob;
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    process::Command,
};

use haybale::{backend::Backend, BBInstrIndex, State};
use llvm_ir::HasDebugLoc;

pub type Disassem = String;

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
    read_llc_output(bc_path)
}

pub fn count_instructions<'p, B: Backend>(
    disassembly: &Disassem,
    state: &State<'p, B>,
) -> Result<(String, usize), String> {
    let mut res = String::new();
    for path_entry in state.get_path().iter() {
        let location = &path_entry.0;
        let module_name = &location.module.name;
        let func_name = &location.func.name;
        let bb_name = &location.bb.name;
        let segment = format!(
            "module: {} | func: {} | bb: {}\n",
            module_name, func_name, bb_name
        );
        res.push_str(&segment);
    }
    Ok((res, 1))
}
