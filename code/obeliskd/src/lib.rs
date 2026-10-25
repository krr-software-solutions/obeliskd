pub mod error;

pub use error::ObeliskdError;

/// Prints a greeting to stdout.
///
/// # Errors
///
/// [`ObeliskdError::Generic`] instead of printing, when `fail` is set.
pub fn print_hello(fail: bool) -> Result<(), ObeliskdError> {
    if fail {
        return Err(ObeliskdError::Generic {
            message: "asked to fail (--fail)".to_string(),
        });
    }
    println!("Hello from obeliskd!");
    Ok(())
}
