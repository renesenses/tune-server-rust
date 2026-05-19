pub mod audio;
pub mod buffer;
pub mod discovery;
pub mod metadata;
pub mod scanner;

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_semver() {
        let v = version();
        assert!(v.split('.').count() == 3, "version must be semver: {v}");
    }
}
