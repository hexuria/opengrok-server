impl crate::Vault {
    /// Build from a base64 KEK of exactly 32 bytes, with no retired keys. The server always
    /// builds its vault with the retired list (`from_base64_keys`); only tests want one key.
    pub fn from_base64_key(kek: &str) -> crate::VaultResult<Self> {
        Self::from_base64_keys(kek, &[])
    }
}
