/// Adds two numbers.
///
/// ```
/// assert_eq!(tiny::add(1, 2), 3);
/// ```
///
/// This example is wrong on purpose:
///
/// ```
/// assert_eq!(tiny::add(1, 2), 4);
/// ```
pub fn add(a: u32, b: u32) -> u32 {
    a + b
}

#[cfg(test)]
mod tests {
    #[test]
    fn passes() {
        assert_eq!(super::add(2, 2), 4);
    }

    #[test]
    fn fails() {
        println!("    indented output that is not a test name");
        assert_eq!(super::add(2, 2), 5);
    }

    #[test]
    #[ignore = "slow"]
    fn ignored() {}
}
