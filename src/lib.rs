use cggmp24::{
    define_security_level, security_level::SecurityLevel128,
    supported_curves::Secp256k1,
};

pub mod manager;
pub mod protocol;

#[derive(Clone)]
pub struct Mid;
define_security_level!(Mid {
    kappa_bits: 192,
    rsa_prime_bitlen: 1024,
    rsa_pubkey_bitlen: (1024 * 2) - 1, // 2*1024 - 1
    epsilon: 192 * 2,                  // ≈ 2*kappa
    ell: 192,
    ell_prime: 256 * 5, // 5*ell
    m: 128,
});

pub type AuxMsg = cggmp24::key_refresh::msg::Msg<sha2::Sha256, Mid>;
pub type ThresholdMsg =
    cggmp24_keygen::ThresholdMsg<Secp256k1, SecurityLevel128, sha2::Sha256>;

pub fn error_chain_fmt(
    e: &impl std::error::Error,
    f: &mut std::fmt::Formatter<'_>,
) -> std::fmt::Result {
    writeln!(f, "{e}\n")?;
    let mut current = e.source();
    while let Some(cause) = current {
        writeln!(f, "Caused by:\n\t{cause}")?;
        current = cause.source();
    }
    Ok(())
}

#[macro_export]
macro_rules! impl_debug {
    ($type:ident) => {
        use $crate::error_chain_fmt;
        impl std::fmt::Debug for $type {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                error_chain_fmt(self, f)
            }
        }
    };
}
