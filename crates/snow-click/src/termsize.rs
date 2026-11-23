//! Terminal size of the first standard stream that is a terminal.

pub fn width() -> Option<usize> {
    use terminal_size::{Width, terminal_size_of};
    if let Some((Width(w), _)) = terminal_size_of(std::io::stdin()) {
        return Some(w as usize);
    }
    if let Some((Width(w), _)) = terminal_size_of(std::io::stdout()) {
        return Some(w as usize);
    }
    terminal_size_of(std::io::stderr()).map(|(Width(w), _)| w as usize)
}
