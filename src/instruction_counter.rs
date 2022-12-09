use haybale::{State, backend::Backend};


pub fn count_instructions<'p, B: Backend>(
    disassembly: &str,
    state: &State<'p, B>,
) -> Result<usize, String> {
    Ok(1)
}
