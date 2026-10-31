//! The fake ACP agent as a program. See `maestro_agent::fake` for its
//! command language.

fn main() {
    std::process::exit(maestro_agent::fake::serve());
}
