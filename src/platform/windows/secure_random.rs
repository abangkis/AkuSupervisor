#![allow(unsafe_code)]

use std::io;

use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
};

const TOKEN_BYTES: usize = 32;
const TOKEN_BYTES_U32: u32 = 32;

/// Generates a 256-bit token using the Windows system-preferred CNG provider.
///
/// # Errors
///
/// Returns an OS error if CNG cannot fill the token buffer.
pub fn generate_control_token() -> io::Result<String> {
    let mut bytes = [0_u8; TOKEN_BYTES];
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            bytes.as_mut_ptr(),
            TOKEN_BYTES_U32,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status != 0 {
        return Err(io::Error::other(format!(
            "BCryptGenRandom failed with NTSTATUS 0x{:08x}",
            status.cast_unsigned()
        )));
    }

    let mut token = String::with_capacity(TOKEN_BYTES * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut token, "{byte:02x}").expect("writing to String cannot fail");
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::generate_control_token;

    #[test]
    fn generated_token_has_256_bits_of_hex_encoded_material() {
        let first = generate_control_token().expect("generate first token");
        let second = generate_control_token().expect("generate second token");

        assert_eq!(first.len(), 64);
        assert!(first.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }
}
