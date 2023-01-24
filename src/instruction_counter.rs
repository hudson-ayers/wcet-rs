use std::{
    collections::{HashMap, HashSet},
    process::Command,
};

use haybale::{backend::Backend, BBInstrIndex, State};
use llvm_ir::HasDebugLoc;
use regex::Regex;

const LOOKUP_THRESHOLD_PRE: usize = 5;
const LOOKUP_THRESHOLD_POST: usize = 2;

#[derive(Clone)]
pub struct LookupInfo {
    source_line: usize,
    disassem_line: usize,
    num_instrs: usize,
    instrs_str: String,
}
pub type SourceLookUp = HashMap<String, Vec<LookupInfo>>;
pub type Disassem = (Vec<String>, SourceLookUp);

fn count_outlined_functions(disassem: &Vec<String>) -> HashMap<String, (usize, String)> {
    let re = Regex::new(r"^[0-9a-f]+ <OUTLINED_FUNCTION_\d+>:").unwrap();
    let name_re = Regex::new(r"(OUTLINED_FUNCTION_\d+)").unwrap();
    let mut res = HashMap::new();
    for (i, line) in disassem.iter().enumerate() {
        if re.is_match(line) {
            let name = &name_re.captures(line).unwrap()[1];
            let alt_name = &name_re.captures(&disassem[i + 1]).unwrap()[1];
            let mut count = 0;
            let mut index = i + 2;
            let mut outlined_str = name.to_owned();
            while index < disassem.len() && disassem[index] != "" {
                index += 1;
                count += 1;
                outlined_str.push_str(&format!("\n{}", disassem[index]));
            }
            outlined_str.push_str(&("End of ".to_owned() + name));

            if !res.contains_key(name) {
                res.insert(name.to_owned(), (count, outlined_str.clone()));
            }
            if !res.contains_key(alt_name) {
                res.insert(alt_name.to_owned(), (count, outlined_str));
            }
        }
    }

    res
}

pub fn get_disassembly(elf_path: &String) -> Disassem {
    let output = Command::new("arm-none-eabi-objdump")
        .arg("-C")
        .arg("--line-numbers")
        .arg("-d")
        .arg(elf_path)
        .output()
        .expect("failed to execute objdump");

    let str_output = String::from_utf8(output.stdout).expect("failed to parse objdump output");

    let mut source_lookup: SourceLookUp = HashMap::new();
    let disassem: Vec<String> = str_output.lines().map(|s| s.to_owned()).collect();

    let re = Regex::new(r"^\s+[0-9a-f]{5}:.*").unwrap();
    let outlined = Regex::new(r"(OUTLINED_FUNCTION_\d+)").unwrap();
    let outlined_count = count_outlined_functions(&disassem);

    for (i, line) in disassem.iter().enumerate() {
        if line.starts_with('/') {
            let file_and_line = line.split(':').collect::<Vec<&str>>();
            if file_and_line.len() != 2 {
                panic!("expected a filename and a line number in \"{}\"", line);
            }
            let source_file = file_and_line[0];
            let source_line = file_and_line[1]
                .parse::<usize>()
                .expect("failed to parse line number");

            let mut count = 0;
            let mut index = i + 1;
            let mut instrs_str = String::new();
            while index < disassem.len() && re.is_match(&disassem[index]) {
                if !disassem[index].contains(".word") {
                    count += 1;
                    instrs_str.push_str(&format!("{}\n", disassem[index]));
                    if outlined.is_match(&disassem[index]) {
                        let name = &outlined.captures(&disassem[index]).unwrap()[1];
                        let outlined_count_and_str = &outlined_count[name];
                        count += outlined_count_and_str.0;
                        instrs_str.push_str(&format!("{}\n", outlined_count_and_str.1));
                    }
                }
                index += 1;
            }

            let lookupinfo = LookupInfo {
                source_line,
                disassem_line: i + 1,
                num_instrs: count,
                instrs_str,
            };

            match source_lookup.get_mut(source_file) {
                Some(vec) => {
                    vec.push(lookupinfo);
                }
                None => {
                    source_lookup.insert(source_file.to_owned(), Vec::new());
                    source_lookup.get_mut(source_file).unwrap().push(lookupinfo);
                }
            }
        }
    }

    println!("done");
    (disassem, source_lookup)
}

pub fn count_instructions<'p, B: Backend>(
    disassembly: &Disassem,
    state: &State<'p, B>,
) -> Result<(String, usize), String> {
    // to correctly print an instruction trace in the presence of function calls,
    // we need to know which calls divert control flow to another basic block. This
    // can be determined by finding all basic blocks in the path which do not
    // begin at instruction 0.
    let mut reenter_set = HashSet::new(); //Store (bb_name, instr_idx) of all calls that leave bb
    for path_entry in state.get_path().iter() {
        match path_entry.0.instr {
            BBInstrIndex::Instr(idx) => {
                if idx != 0 {
                    reenter_set.insert((path_entry.0.bb.name.clone(), idx - 1));
                }
            }
            BBInstrIndex::Terminator => {
                let num_instrs = path_entry.0.bb.instrs.len();
                if num_instrs > 0 {
                    // call is last instruction in block
                    reenter_set.insert((path_entry.0.bb.name.clone(), num_instrs - 1));
                }
            }
        }
    }
    let mut ir_path_str = String::new();
    let mut total_assembly_instrs = 0;
    let mut seen = HashMap::new();
    for path_entry in state.get_path().iter() {
        let location = &path_entry.0;
        match location.instr {
            BBInstrIndex::Instr(start) => {
                let mut broke_early = false;
                for (i, instr) in location.bb.instrs.iter().skip(start).enumerate() {
                    let idx = start + i;
                    let debug = instr.get_debug_loc().as_ref();
                    match debug {
                        Some(debug_loc) => {
                            if debug_loc.line == 0 {
                                ir_path_str
                                    .push_str(&format!("{} | NO DEBUG LOC AVAILABLE!!!!\n", instr))
                            } else {
                                let filename = match debug_loc.directory.as_ref() {
                                    Some(directory) => {
                                        directory.to_owned() + "/" + &debug_loc.filename
                                    }
                                    None => debug_loc.filename.to_owned(),
                                };
                                match disassembly.1.get(&filename) {
                                    Some(source_locs) => {
                                        let debug_source_line = debug_loc.line as usize;
                                        // Find the assembly instruction with the source line closest to
                                        // the source line associated with the LLVM IR instruction.
                                        // But the assembly instruction must have a source line that came
                                        // before the IR source line.
                                        let closest_disassem_line = source_locs
                                            .iter()
                                            .map(|lookupinfo| {
                                                (
                                                    if lookupinfo.source_line > debug_source_line {
                                                        let diff = lookupinfo.source_line
                                                            - debug_source_line;
                                                        if diff <= LOOKUP_THRESHOLD_POST {
                                                            diff
                                                        } else {
                                                            usize::MAX
                                                        }
                                                    } else {
                                                        let diff = debug_source_line
                                                            - lookupinfo.source_line;
                                                        if diff <= LOOKUP_THRESHOLD_PRE {
                                                            diff
                                                        } else {
                                                            usize::MAX
                                                        }
                                                    },
                                                    lookupinfo.disassem_line,
                                                    lookupinfo.num_instrs,
                                                    &lookupinfo.instrs_str,
                                                )
                                            })
                                            .min()
                                            .unwrap();
                                        if closest_disassem_line.0 == usize::MAX {
                                            ir_path_str.push_str(&format!(
                                                "{} | {}, LOOKUP_THRESHOLD exceeded\n",
                                                instr, debug_loc
                                            ));
                                        } else {
                                            ir_path_str.push_str(&format!(
                                                "{} | {}, {}, {}\n",
                                                instr,
                                                debug_loc,
                                                closest_disassem_line.1,
                                                closest_disassem_line.2
                                            ));
                                            // add asm instrs
                                            if !seen.contains_key(&closest_disassem_line.1) {
                                                total_assembly_instrs += closest_disassem_line.2;
                                                seen.insert(
                                                    closest_disassem_line.1,
                                                    closest_disassem_line.3,
                                                );
                                            };
                                        }
                                    }
                                    None => ir_path_str.push_str(&format!(
                                        "{} | {}, fn: {}, LOOKUP FAILED\n",
                                        instr, debug_loc, filename
                                    )),
                                }
                            }
                        }
                        None => ir_path_str
                            .push_str(&format!("{} | NO DEBUG LOC AVAILABLE!!!!\n", instr)),
                    }
                    match instr {
                        llvm_ir::instruction::Instruction::Call(_) => {
                            if reenter_set.contains(&(location.bb.name.clone(), idx)) {
                                broke_early = true;
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                // add terminator, but only if we did not leave bb early bc of function call.
                if !broke_early {
                    ir_path_str.push_str(&format!("{}\n", location.bb.term));
                }
            }
            BBInstrIndex::Terminator => {
                ir_path_str.push_str(&format!("{}\n", location.bb.term));
            }
        }
    }

    let mut seen_vec = seen
        .iter()
        .map(|(&line_num, &inst_str)| (line_num, inst_str))
        .collect::<Vec<(usize, &String)>>();
    seen_vec.sort();

    let asm_path_str = seen_vec.iter().fold("".to_owned(), |acc, e| acc + e.1);

    let res_str = "ASM Path:\n".to_owned() + &asm_path_str + "Annotated IR Path:\n" + &ir_path_str;

    Ok((res_str, total_assembly_instrs))
}
