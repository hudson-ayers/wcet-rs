use haybale::{backend::Backend, State};

pub fn count_instructions<'p, B: Backend>(
    disassembly: &Vec<String>,
    state: &State<'p, B>,
) -> Result<usize, String> {
    disassembly.iter().for_each(|s| println!("{}", s));
    Ok(1)
}
